// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! Messages into the core thread and the state it shares with the UI and bus threads.

use dexhid_core::geom::Geometry;
use dexhid_core::router::UiModel;
use dexhid_core::state::{Choice, UiKind};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
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
    Mapped(UiKind),
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
    /// what the UI shows + the model; `ui_ver` bumps on every change
    pub ui: Mutex<(UiKind, UiModel)>,
    pub ui_ver: AtomicU64,
    /// last UI loop iteration (CLOCK_MONOTONIC ms), for the watchdog
    pub ui_heartbeat: AtomicI64,
    /// JSON status for `dexhid status`
    pub status: Mutex<String>,
    pub layout_names: Mutex<Vec<String>>,
    pub debug: bool,
    pub ui_cfg: UiCfg,
    /// M2: the active xkb group for the legends (KDE layout tracker), read by the UI thread
    pub xkb_group: std::sync::atomic::AtomicU32,
}

impl Shared {
    pub fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
        crate::sys::poke(self.wake_core);
    }
    pub fn publish_ui(&self, kind: UiKind, model: &UiModel) {
        *self.ui.lock().unwrap() = (kind, model.clone());
        self.ui_ver.fetch_add(1, Ordering::SeqCst);
        crate::sys::poke(self.wake_ui);
    }
}

pub type SharedRef = Arc<Shared>;
