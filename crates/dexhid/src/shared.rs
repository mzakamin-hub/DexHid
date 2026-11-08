// SPDX-License-Identifier: GPL-3.0-or-later
//! Messages into the core thread and the state it shares with the UI and bus threads.

use dexhid_core::geom::Geometry;
use dexhid_core::router::UiModel;
use dexhid_core::state::{Choice, UiKind};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{Sender, SyncSender};
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub enum Cmd {
    // D-Bus / CLI
    Toggle(String),
    Release(String),
    Resume(String),
    SetLayout(String),
    DebugConnector(bool),
    // UI
    Prompt(Choice),
    PillUsePad,
    PillHide,
    Geometry(Geometry),
    /// the surface of this kind and epoch is on screen
    Mapped(UiKind, u64),
    Outputs(Vec<String>),
    UiLost(String),
    Labels(usize),
    // logind
    Locked(bool),
    SessionActive(bool),
    Sleep(bool, Option<SyncSender<()>>),
    Lid(bool),
    Note(String),
    // M2
    /// D-Bus SetSetting(key, value) -> reply
    SetSetting(String, String, SyncSender<Result<String, String>>),
    /// D-Bus GetSettings -> JSON
    GetSettings(SyncSender<String>),
    /// D-Bus RunQuick(id)
    RunQuick(String),
    /// KDE layout tracker: active group, layout names
    XkbGroup(u32, Vec<String>),
    /// theme "auto": the desktop's scheme
    ThemeAuto(String),
    /// M2.1 theme "desktop": the KDE look, a log line
    DeskLook(Option<dexhid_core::feedback::DeskLook>, String),
    // M3
    /// clipboard history (newest first), backend name
    Clips(Vec<dexhid_core::extras::ClipData>, String),
    /// the mirrored monitor's size
    MirrorSize(u32, u32),
    /// the focused window's class (KWin script -> D-Bus ActiveApp)
    ActiveApp(String),
    /// D-Bus Profile(command) -> reply
    Profile(String, SyncSender<Result<String, String>>),
    /// D-Bus Act(command) -> reply (gestures, emoji panel, clipboard, timer, select mode: same as the touch paths)
    Act(String, SyncSender<Result<String, String>>),
    /// D-Bus HidStatus -> JSON
    HidStatus(SyncSender<String>),
    // M4
    /// D-Bus Status -> JSON (built on request: no status work per input event)
    Status(SyncSender<String>),
    /// D-Bus OpenWindow(page) / OpenSettings, `dexhid toggle` with nothing to toggle
    OpenWindow(String),
    /// the app window was closed by the user / compositor
    WindowClosed,
    /// D-Bus CloseWindow / `dexhid window --close`
    CloseWindow,
    /// D-Bus SetMode / the window's mode switch -> reply (Err = the hint)
    SetMode(String, Option<SyncSender<Result<String, String>>>),
    /// the window: step a setting (key, +1 / -1)
    WinSetting(String, i32),
}

/// M4: the per-touch part of the UI model (pressed keys / button / quick action, touch dots), handed to the UI
/// without cloning the whole model; `clone_from` reuses the vectors' capacity (no allocation in steady state)
#[derive(Clone, Debug, Default)]
pub struct DynState {
    pub lit_keys: Vec<usize>,
    pub lit_button: Option<dexhid_core::router::Btn>,
    pub lit_quick: Option<usize>,
    pub dots: Vec<(f32, f32)>,
}

impl DynState {
    pub fn copy_from_model(&mut self, m: &UiModel) {
        self.lit_keys.clone_from(&m.lit_keys);
        self.lit_button = m.lit_button;
        self.lit_quick = m.lit_quick;
        self.dots.clone_from(&m.dots);
    }
    pub fn copy_into_model(&self, m: &mut UiModel) {
        m.lit_keys.clone_from(&self.lit_keys);
        m.lit_button = self.lit_button;
        m.lit_quick = self.lit_quick;
        m.dots.clone_from(&self.dots);
    }
}

/// UI-side configuration (fixed at start)
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct UiCfg {
    pub internal_output: String,
    pub touch_transform: String,
    pub size_mm: (f32, f32),
    pub tmax: (i32, i32),
    pub xkb_layout_index: u32,
    pub verbose: bool,
}

pub struct Shared {
    pub tx: Sender<Cmd>,
    /// write end of the core's wake pipe
    pub wake_core: i32,
    /// write end of the UI's wake pipe
    pub wake_ui: i32,
    /// what the UI shows + the model + the surface epoch (bumps on every kind change); `ui_ver` bumps on every change
    pub ui: Mutex<(UiKind, UiModel, u64)>,
    pub ui_ver: AtomicU64,
    /// last UI loop iteration (CLOCK_MONOTONIC ms), for the watchdog
    pub ui_heartbeat: AtomicI64,
    pub layout_names: Mutex<Vec<String>>,
    pub debug: bool,
    pub ui_cfg: UiCfg,
    /// M2: the active xkb group for the legends (KDE layout tracker), read by the UI thread
    pub xkb_group: std::sync::atomic::AtomicU32,
    // M3
    /// the latest mirror preview frame (w, h, 0xffRRGGBB pixels)
    pub mirror: Mutex<Option<(u32, u32, Vec<u32>)>>,
    /// bumps with every new mirror frame (the UI redraws the preview)
    pub mirror_gen: AtomicU64,
    /// legends from this xkb layout instead of the desktop's keymap (the keyboard language of a USB / BT target)
    pub legend_layout: Mutex<Option<String>>,
    /// the legends the UI shows now (code -> levels), for typing text / the word tracker in the daemon
    pub labels: Mutex<std::collections::HashMap<u16, [String; 4]>>,
    // M4
    /// the dynamic part of the model; `dyn_ver` bumps on every change
    pub dyn_ui: Mutex<DynState>,
    pub dyn_ver: AtomicU64,
    /// the app window's content (None = closed); `win_ver` bumps on every change
    pub win: Mutex<Option<crate::appwin::WinView>>,
    pub win_ver: AtomicU64,
    /// open the window at this page ("home", "settings", a module id)
    pub win_req: Mutex<Option<String>>,
    /// the daemon wants a UI heartbeat (watchdog ping without a UI timer)
    pub hb_req: AtomicBool,
    /// render at half resolution (setting render_scale)
    pub half_res: AtomicBool,
}

impl Shared {
    pub fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
        crate::sys::poke(self.wake_core);
    }
    /// a static change (layout, labels, modes, info text ...): the whole model
    pub fn publish_ui(&self, kind: UiKind, model: &UiModel, epoch: u64) {
        self.dyn_ui.lock().unwrap().copy_from_model(model);
        *self.ui.lock().unwrap() = (kind, model.clone(), epoch);
        self.ui_ver.fetch_add(1, Ordering::SeqCst);
        crate::sys::poke(self.wake_ui);
    }
    /// a per-touch change: only the dynamic part (no model clone, no allocation in steady state)
    pub fn publish_dyn(&self, model: &UiModel) {
        self.dyn_ui.lock().unwrap().copy_from_model(model);
        self.dyn_ver.fetch_add(1, Ordering::SeqCst);
        crate::sys::poke(self.wake_ui);
    }
    pub fn publish_win(&self, v: Option<crate::appwin::WinView>) {
        *self.win.lock().unwrap() = v;
        self.win_ver.fetch_add(1, Ordering::SeqCst);
        crate::sys::poke(self.wake_ui);
    }
}

pub type SharedRef = Arc<Shared>;
