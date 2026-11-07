// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! Pad-mode touch routing. Every contact belongs to the area where it STARTED until it lifts: the control strip,
//! a settings-sheet button, the touchpad, a keyboard key, a quick action, or (edit mode) a region handle. The router
//! also owns the on-screen model the UI draws (strip buttons, regions, keys, pressed state, touch dots, the settings
//! sheet, quick actions, snap guides) and the 5-finger release. Pure logic: no devices, no clock (times are ms).
//!
//! M2: edge handles + snap guides in edit mode, the virtual touchpad is recreated only when edit mode ends (M1
//! recreated it on every drag step, which made the daemon lag behind the finger), the settings sheet (built from the
//! settings registry), quick actions, palm / rest-zone / disable-while-typing / pen-proximity filters, sticky
//! modifiers, the Fn layer, combo keys and long-press alternates (keys.rs), Caps / Num lock LEDs, theme + status text.
//!
//! M2.1: key slop (a stationary hold, slide-off with hysteresis), the mask key for cancelled modifiers, whole-screen
//! 3/4-finger swipes (swipe.rs) delivered as synthetic fingers on the virtual touchpad (so the desktop's own
//! touchpad gestures run), independent click zones with an optional left+right = middle chord, two-finger scroll
//! inertia (touchpad.rs), AltGr state for the legends, the "laptop" arrangement preset.

use crate::arrangement::{self, Arrangement, EditCtx, Guide, Handle, Orientation, QUICK, TOUCHPAD};
use crate::geom::{Geometry, Rect};
use crate::keycodes;
use crate::keys::{ContactId, KeyMachine, KeyOut};
use crate::layout::{Action, KeyKind, Layout, PlacedKey};
use crate::mt::Contact;
use crate::settings::{Kind, Row};
use crate::swipe::{Phase, Swipe, SwipeCfg};
use crate::touchpad::{Inertia, PadFrame, PadMapper, SLOTS};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug)]
pub struct RouterCfg {
    pub hold_ms: i64,
    pub strip_mm: f32,
    pub key_gap_mm: f32,
    /// 0 = no click zones
    pub click_zone_mm: f32,
    pub five_finger: bool,
    pub five_hold_ms: i64,
    pub five_slop_mm: f32,
    pub longpress_ms: i64,
    pub slop_mm: f32,
    pub snap_mm: f32,
    pub handle_mm: f32,
    /// touchpad device units per mm (the speed factor is folded in: units / speed)
    pub pad_units_per_mm: f32,
    pub palm_major: i32,
    pub options: HashSet<String>,
    // M2
    pub sticky: bool,
    pub long_ms: i64,
    pub palm_keyboard: bool,
    pub rest_zone_mm: f32,
    /// fractions of the work area
    pub rest_zones: Vec<Rect>,
    pub dwt_ms: i64,
    pub pen_pause: bool,
    /// show the quick-actions region
    pub quick: bool,
    // M2.1
    /// movement that restarts a pending key hold, and the hysteresis for sliding off a key (mm)
    pub key_slop_mm: f32,
    /// neutral key between a cancelled lone modifier's press and release (None = off)
    pub mask_key: Option<u16>,
    /// whole-screen 3/4-finger swipes
    pub swipe: bool,
    pub swipe_min_mm: f32,
    /// both click zones within 50 ms = middle button
    pub chord_middle: bool,
    pub inertia: Inertia,
    /// "laptop" arrangement preset (else free placement)
    pub laptop: bool,
    /// laptop preset: touchpad width in % of the keyboard width
    pub laptop_pad_pct: f32,
    // M3
    /// gesture shortcuts: 3 / 4-finger swipes and taps are recognised here (also on the touchpad) -> Out::Gesture
    pub gestures: bool,
    /// text editing mode: the select key and the cursor joystick act
    pub textedit: bool,
    pub joy_dead_mm: f32,
    /// presentation laser: touchpad contacts -> Out::Laser (absolute position in the pad)
    pub laser: bool,
    /// the clipboard band / the suggestion band under the strip
    pub clip_band: bool,
    pub suggest_band: bool,
    /// an "Emoji" chip that opens the emoji panel
    pub emoji: bool,
    /// mirror preview size in % of the pad width (0 = off)
    pub mirror_pct: f32,
    /// the target button in the strip ("This desktop" / "USB"), None = hidden
    pub target_label: Option<String>,
}

impl Default for RouterCfg {
    fn default() -> RouterCfg {
        RouterCfg {
            hold_ms: 150,
            strip_mm: 11.0,
            key_gap_mm: 1.2,
            click_zone_mm: 14.0,
            five_finger: true,
            five_hold_ms: 800,
            five_slop_mm: 5.0,
            longpress_ms: 700,
            slop_mm: 4.0,
            snap_mm: 6.0,
            handle_mm: 10.0,
            pad_units_per_mm: 124.0,
            palm_major: 0,
            options: ["function".to_string()].into_iter().collect(),
            sticky: false,
            long_ms: 450,
            palm_keyboard: true,
            rest_zone_mm: 0.0,
            rest_zones: vec![],
            dwt_ms: 0,
            pen_pause: true,
            quick: false,
            key_slop_mm: 3.0,
            mask_key: Some(240),
            swipe: true,
            swipe_min_mm: 8.0,
            chord_middle: false,
            inertia: Inertia::default(),
            laptop: false,
            laptop_pad_pct: 100.0,
            gestures: false,
            textedit: false,
            joy_dead_mm: 3.0,
            laser: false,
            clip_band: false,
            suggest_band: false,
            emoji: false,
            mirror_pct: 0.0,
            target_label: None,
        }
    }
}

/// M3: libinput's tap timeout is 180 ms; frozen touchpad fingers are lifted no earlier than this after they landed
/// (a quicker lift of two stationary fingers would be a two-finger tap = right click)
pub const TAP_GUARD_MS: i64 = 220;
/// M3: band height under the strip (mm)
pub const BAND_MM: f32 = 9.0;

/// M3: a band of chips under the strip (clipboard history, word suggestions)
#[derive(Clone, Debug, PartialEq)]
pub struct BandView {
    pub id: String,
    pub rect: Rect,
    pub chips: Vec<(String, Rect)>,
    /// text shown when the band has no chips
    pub empty: String,
}

/// M3: the emoji panel
#[derive(Clone, Debug, PartialEq, Default)]
pub struct EmojiView {
    pub rect: Rect,
    pub page: usize,
    pub items: Vec<(String, Rect)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Btn {
    Display,
    Layout,
    Settings,
    Exit,
    Done,
    Reset,
    Cancel,
    PanelClose,
    /// settings sheet: module tab i, row i (tap = step +1), its minus / plus / move-up buttons, pages
    SheetTab(u8),
    SheetRow(u8),
    SheetMinus(u8),
    SheetPlus(u8),
    SheetUp(u8),
    SheetPrev,
    SheetNext,
    // M3
    /// the strip's target button (desktop -> usb)
    Target,
    /// emoji panel: category tab i, emoji i
    EmojiTab(u8),
    Emoji(u8),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ButtonView {
    pub btn: Btn,
    pub rect: Rect,
    pub label: String,
    /// GEOM name (strip.display, sheet.row.<key>, ...)
    pub name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RegionView {
    pub id: String,
    pub rect: Rect,
}

#[derive(Clone, Debug, PartialEq)]
pub struct QuickView {
    pub id: String,
    pub label: String,
    pub rect: Rect,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SheetRowView {
    pub rect: Rect,
    pub title: String,
    pub live: bool,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct SheetView {
    pub rect: Rect,
    pub title: String,
    pub subtitle: String,
    pub rows: Vec<SheetRowView>,
    /// selected tab index
    pub tab: usize,
}

/// one module of the settings sheet (built by the daemon from settings::rows + the quick-action rows)
#[derive(Clone, Debug, PartialEq)]
pub struct SheetModule {
    pub id: String,
    pub title: String,
    pub live: bool,
    pub plan: String,
    pub rows: Vec<Row>,
}

/// what the UI draws. `version` changes when the static part changes (layout, arrangement, labels, edit mode).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UiModel {
    pub version: u64,
    pub w: f32,
    pub h: f32,
    pub px_per_mm: f32,
    pub strip: Rect,
    pub title: String,
    pub hint: String,
    pub buttons: Vec<ButtonView>,
    pub regions: Vec<RegionView>,
    pub zones: Option<(Rect, Rect)>,
    pub keys: Vec<PlacedKey>,
    pub shift: bool,
    pub edit: bool,
    pub sheet: Option<SheetView>,
    pub quick: Vec<QuickView>,
    pub guides: Vec<Guide>,
    /// battery / display text in the strip
    pub info: String,
    /// theme name, accent "#rrggbb", key opacity %
    pub theme: (String, String, u32),
    /// Caps Lock, Num Lock LEDs (from the desktop, through the virtual keyboard)
    pub leds: (bool, bool),
    /// M2.1: AltGr (level 3) held or latched on the panel
    pub altgr: bool,
    /// M2.1: the arrangement comes from the laptop preset
    pub laptop: bool,
    /// M2.1: the desktop's look (theme "desktop"), None = built-in palette
    pub desk: Option<crate::feedback::DeskLook>,
    /// latched sticky modifiers (code, locked)
    pub latched: Vec<(u16, bool)>,
    pub fn_on: bool,
    /// a short notice in the strip (pen pause, pad only, ...)
    pub notice: String,
    // M3
    pub bands: Vec<BandView>,
    pub emoji: Option<EmojiView>,
    /// mirror preview rect (the UI draws the latest frame into it)
    pub mirror: Option<Rect>,
    /// text editing: select-while-moving is on
    pub select: bool,
    /// the touchpad is the laser pointer area
    pub laser: bool,
    // dynamic part
    pub lit_keys: Vec<usize>,
    pub lit_button: Option<Btn>,
    pub lit_quick: Option<usize>,
    pub dots: Vec<(f32, f32)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    Key(u16, bool),
    Pad(PadFrame),
    UseAsDisplay,
    Exit,
    FiveFinger,
    LayoutNext,
    SetLayout(String),
    /// edit mode left with "Done": persist `arrangement()`
    SaveArrangement,
    /// the arrangement was reset to the defaults (also persisted by removing the file)
    ResetArrangement,
    /// the touchpad rect changed: recreate the virtual touchpad (no pad contact is down at this point)
    PadChanged,
    /// the orientation changed: load the arrangement of the new orientation (`set_arrangement`)
    NeedArrangement(Orientation),
    EditMode(bool),
    PanelOpen(bool),
    /// a quick action button was tapped
    Quick(String),
    /// settings sheet: change a setting by one step (+1 / -1); `quick:<id>` / `quick.up:<id>` for quick rows
    SettingStep(String, i32),
    /// a key press reached the virtual keyboard (feedback: sound / haptics)
    KeyFeedback,
    /// M2.1: a whole-screen swipe took over / ended (log line for the runner)
    Swipe(String),
    /// M2.1: set a setting to a value (edit mode from the laptop preset switches to free placement)
    SetSetting(String, String),
    /// M3: a gesture shortcut was recognised ("swipe3_left", "tap3", ...)
    Gesture(String),
    /// M3: a chip of a band was tapped (band id, index)
    Chip(String, usize),
    /// M3: an emoji of the panel was tapped
    Emoji(String),
    /// M3: laser pointer position in the pad (0..1), None = lifted
    Laser(Option<(f32, f32)>),
    /// M3: talk timer "toggle" / "reset"
    Timer(String),
    /// M3: the strip's target button
    TargetNext,
    /// static model changed (full redraw), dynamic changed (pressed keys / dots)
    Static,
    Dynamic,
    Note(String),
}

#[derive(Clone, Debug)]
enum Owner {
    Strip { btn: Option<Btn>, t0: i64, long: bool },
    Button(Btn),
    Pad,
    Key,
    Quick(usize),
    Edit { handle: Handle, region: String, orig: Rect, x0: f32, y0: f32 },
    /// M2.1: a participant of a recognised whole-screen swipe (synthetic finger index)
    Swipe(usize),
    Ignored,
    // M3
    /// a finger of a (possible) gesture shortcut: captured, nothing goes out until it lifts
    Gesture,
    /// the text-editing joystick: the arrow repeating now and when the next one is due
    Joystick { key: Option<u16>, iv: i64, next: i64 },
    /// a chip of a band (band index, chip index)
    Chip(usize, usize),
    /// the presentation laser on the touchpad
    Laser,
}

/// M2.1: the synthetic touchpad fingers of a recognised whole-screen swipe
#[derive(Clone, Debug)]
struct Synth {
    /// device positions of the fingers
    fingers: Vec<(f32, f32)>,
    lifted: bool,
    /// travel still to deliver in small steps (device units)
    catch: (f32, f32),
    next: i64,
}

#[derive(Clone, Debug)]
struct Track {
    x: f32,
    y: f32,
    x0: f32,
    y0: f32,
    owner: Owner,
    /// M3: when it landed
    t0: i64,
}

pub struct Router {
    pub cfg: RouterCfg,
    geo: Option<Geometry>,
    layout: Layout,
    page: String,
    arr: Option<Arrangement>,
    arr_backup: Option<Arrangement>,
    edit: bool,
    sheet_open: bool,
    sheet_tab: usize,
    sheet_page: usize,
    sheet: Vec<SheetModule>,
    keys: KeyMachine,
    pad: Option<PadMapper>,
    contacts: HashMap<ContactId, Track>,
    five: Option<(i64, bool)>,
    five_blocked: bool,
    model: UiModel,
    orient: Option<Orientation>,
    quick_items: Vec<(String, String, u32)>,
    guides: Vec<Guide>,
    info: String,
    theme: (String, String, u32),
    leds: (bool, bool),
    pen: bool,
    notice: String,
    last_key_t: i64,
    /// sheet rows on screen: (row index in the module, key)
    sheet_keys: Vec<String>,
    swipe: Swipe,
    synth: Option<Synth>,
    desk: Option<crate::feedback::DeskLook>,
    // M3
    taps: crate::gestures::Taps,
    /// frozen touchpad fingers are lifted at this time (tap guard)
    pad_lift_at: Option<i64>,
    select: bool,
    clips: Vec<String>,
    suggestions: Vec<String>,
    emoji_page: Option<usize>,
    mirror_src: (u32, u32),
}

impl Router {
    pub fn new(cfg: RouterCfg, layout: Layout) -> Router {
        let page = layout.first_page();
        let mut keys = KeyMachine::new(cfg.hold_ms);
        keys.long_ms = cfg.long_ms;
        keys.sticky = cfg.sticky;
        keys.mask = cfg.mask_key;
        let swipe = Swipe::new(SwipeCfg { enabled: cfg.swipe || cfg.gestures, ..Default::default() });
        Router {
            cfg,
            geo: None,
            layout,
            page,
            arr: None,
            arr_backup: None,
            edit: false,
            sheet_open: false,
            sheet_tab: 0,
            sheet_page: 0,
            sheet: vec![],
            keys,
            pad: None,
            contacts: HashMap::new(),
            five: None,
            five_blocked: false,
            model: UiModel::default(),
            orient: None,
            quick_items: vec![],
            guides: vec![],
            info: String::new(),
            theme: ("dark".into(), "#4a8be0".into(), 100),
            leds: (false, false),
            pen: false,
            notice: String::new(),
            last_key_t: i64::MIN / 2,
            sheet_keys: vec![],
            swipe,
            synth: None,
            desk: None,
            taps: crate::gestures::Taps::new(Default::default()),
            pad_lift_at: None,
            select: false,
            clips: vec![],
            suggestions: vec![],
            emoji_page: None,
            mirror_src: (1920, 1080),
        }
    }

    // ---------------------------------------------------------------- M3 setters

    /// the clipboard band's labels (newest first)
    pub fn set_clips(&mut self, v: Vec<String>) -> Vec<Out> {
        if self.clips == v {
            return vec![];
        }
        self.clips = v;
        self.relayout()
    }

    /// the current word suggestions
    pub fn set_suggestions(&mut self, v: Vec<String>) -> Vec<Out> {
        if self.suggestions == v {
            return vec![];
        }
        self.suggestions = v;
        self.relayout()
    }

    /// the mirrored monitor's size (the preview keeps its aspect)
    pub fn set_mirror_src(&mut self, w: u32, h: u32) -> Vec<Out> {
        if self.mirror_src == (w, h) {
            return vec![];
        }
        self.mirror_src = (w, h);
        self.relayout()
    }

    pub fn select_mode(&self) -> bool {
        self.select
    }

    /// select-while-moving on / off (the select key; D-Bus Act)
    pub fn toggle_select(&mut self) -> Vec<Out> {
        let mut out = vec![];
        self.key_outs(vec![KeyOut::Action(Action::SelectMode)], 0, &mut out);
        out
    }

    pub fn emoji_open(&self) -> bool {
        self.emoji_page.is_some()
    }

    /// open / close the emoji panel (also the quick action / D-Bus)
    pub fn toggle_emoji(&mut self) -> Vec<Out> {
        let mut out = vec![];
        self.emoji_page = if self.emoji_page.is_some() { None } else { Some(0) };
        if self.emoji_page.is_some() {
            self.sheet_open = false;
        }
        out.push(Out::PanelOpen(self.emoji_page.is_some()));
        out.extend(self.relayout());
        out
    }

    /// a button click on the touchpad (gesture action "middle-click"), fingers kept where they are
    pub fn pad_click(&mut self, btn: u16) -> Vec<Out> {
        self.pad.as_mut().map(|p| p.click(btn).into_iter().map(Out::Pad).collect()).unwrap_or_default()
    }

    /// a key tap that respects select-while-moving (Shift around caret keys)
    fn nav_tap(&mut self, code: u16, out: &mut Vec<Out>) {
        let shift = self.select && crate::extras::is_nav(code);
        if shift {
            out.push(Out::Key(keycodes::KEY_LEFTSHIFT, true));
        }
        out.push(Out::Key(code, true));
        out.push(Out::Key(code, false));
        if shift {
            out.push(Out::Key(keycodes::KEY_LEFTSHIFT, false));
        }
        out.push(Out::KeyFeedback);
    }

    /// M3 gesture mode: three or more fingers landed together (or a third finger on the touchpad): capture them
    fn capture_gesture(&mut self, t: i64, out: &mut Vec<Out>, min: usize) {
        let ids: Vec<ContactId> = self.contacts.iter().filter(|(_, c)| matches!(c.owner, Owner::Pad | Owner::Key | Owner::Quick(_))).map(|(i, _)| *i).collect();
        if ids.len() < min.max(1) {
            return;
        }
        // keys: pending taps are cancelled, pressed keys released (with the modifier mask)
        let v = self.keys.release_all();
        self.key_outs(v, t, out);
        // touchpad fingers freeze; they lift once libinput can no longer take them for a tap
        let first_pad = self.contacts.values().filter(|c| matches!(c.owner, Owner::Pad)).map(|c| c.t0).min();
        if let Some(p) = self.pad.as_mut() {
            if p.coasting() {
                p.stop_coast();
            }
        }
        match first_pad {
            Some(t0) if t < t0 + TAP_GUARD_MS => self.pad_lift_at = Some(t0 + TAP_GUARD_MS),
            Some(_) => {
                if let Some(f) = self.pad.as_mut().and_then(|p| p.release_all()) {
                    out.push(Out::Pad(f));
                }
            }
            None => {}
        }
        for id in &ids {
            if let Some(c) = self.contacts.get_mut(id) {
                c.owner = Owner::Gesture;
            }
        }
        out.push(Out::Note(format!("gesture: {} fingers captured", ids.len())));
    }

    /// M2.1: the desktop's look (theme "desktop")
    pub fn set_look(&mut self, desk: Option<crate::feedback::DeskLook>) -> Vec<Out> {
        if self.desk == desk {
            return vec![];
        }
        self.desk = desk;
        self.relayout()
    }

    pub fn swipe_phase(&self) -> Phase {
        self.swipe.phase()
    }

    pub fn ready(&self) -> bool {
        self.geo.is_some()
    }
    pub fn geometry(&self) -> Option<Geometry> {
        self.geo
    }
    pub fn orientation(&self) -> Option<Orientation> {
        self.orient
    }
    pub fn layout(&self) -> &Layout {
        &self.layout
    }
    pub fn editing(&self) -> bool {
        self.edit
    }
    pub fn sheet_open(&self) -> bool {
        self.sheet_open
    }
    pub fn arrangement(&self) -> Arrangement {
        self.arr.clone().unwrap_or_default()
    }
    pub fn model(&self) -> &UiModel {
        &self.model
    }
    pub fn held_codes(&self) -> Vec<u16> {
        self.keys.held_codes()
    }
    pub fn contacts_down(&self) -> usize {
        self.contacts.len()
    }

    fn ppm(&self) -> f32 {
        self.geo.map(|g| g.px_per_mm()).unwrap_or(6.0)
    }

    /// (xmax, ymax, resolution units/mm) of the virtual touchpad for the current pad rect
    pub fn pad_spec(&self) -> Option<(i32, i32, i32)> {
        let p = self.pad.as_ref()?;
        let (x, y) = p.dims();
        Some((x, y, self.cfg.pad_units_per_mm.round().max(1.0) as i32))
    }

    /// apply a changed configuration (settings sheet); returns the outs of the relayout
    pub fn set_cfg(&mut self, cfg: RouterCfg) -> Vec<Out> {
        let mut out = vec![];
        if cfg.sticky != self.cfg.sticky || cfg.options != self.cfg.options {
            out.extend(self.release_all());
        }
        self.keys.hold_ms = cfg.hold_ms;
        self.keys.long_ms = cfg.long_ms;
        self.keys.sticky = cfg.sticky;
        self.keys.mask = cfg.mask_key;
        self.swipe.cfg.enabled = cfg.swipe || cfg.gestures;
        if !cfg.textedit {
            self.select = false;
        }
        if !cfg.emoji {
            self.emoji_page = None;
        }
        self.cfg = cfg;
        out.extend(self.relayout());
        out
    }

    pub fn set_autostart_label(&mut self, _s: &str) -> Vec<Out> {
        vec![]
    }

    /// the settings sheet content (call again after every change)
    pub fn set_sheet(&mut self, modules: Vec<SheetModule>) -> Vec<Out> {
        if self.sheet == modules {
            return vec![];
        }
        self.sheet = modules;
        if self.sheet_open {
            self.relayout()
        } else {
            vec![]
        }
    }

    pub fn set_quick(&mut self, items: Vec<(String, String, u32)>) -> Vec<Out> {
        if self.quick_items == items {
            return vec![];
        }
        self.quick_items = items;
        self.relayout()
    }

    pub fn set_info(&mut self, s: &str) -> Vec<Out> {
        if self.info == s {
            return vec![];
        }
        self.info = s.to_string();
        self.relayout()
    }

    pub fn set_notice(&mut self, s: &str) -> Vec<Out> {
        if self.notice == s {
            return vec![];
        }
        self.notice = s.to_string();
        self.relayout()
    }

    pub fn set_theme(&mut self, name: &str, accent: &str, key_alpha: u32) -> Vec<Out> {
        let t = (name.to_string(), accent.to_string(), key_alpha);
        if self.theme == t {
            return vec![];
        }
        self.theme = t;
        self.relayout()
    }

    pub fn set_leds(&mut self, caps: bool, num: bool) -> Vec<Out> {
        if self.leds == (caps, num) {
            return vec![];
        }
        self.leds = (caps, num);
        self.relayout()
    }

    /// pen in proximity (passive read of the pen device): new touches are ignored while `pen_pause` is on
    pub fn set_pen(&mut self, near: bool) -> Vec<Out> {
        if self.pen == near {
            return vec![];
        }
        self.pen = near;
        vec![Out::Note(format!("pen {}", if near { "near: touches paused" } else { "away" }))]
    }

    pub fn set_geometry(&mut self, g: Geometry) -> Vec<Out> {
        if self.geo == Some(g) {
            return vec![];
        }
        let mut out = self.release_all();
        self.geo = Some(g);
        let o = if g.landscape() { Orientation::Landscape } else { Orientation::Portrait };
        if self.orient != Some(o) {
            self.orient = Some(o);
            self.arr = None;
            self.edit = false;
            out.push(Out::NeedArrangement(o));
        }
        out.extend(self.relayout());
        out
    }

    /// switch the keyboard layout; `arr` = the saved arrangement of this layout for the current orientation
    pub fn set_layout(&mut self, layout: Layout, arr: Option<Arrangement>) -> Vec<Out> {
        let mut out = self.release_all();
        self.layout = layout;
        self.page = self.layout.first_page();
        self.arr = arr;
        self.edit = false;
        out.extend(self.relayout());
        out
    }

    pub fn set_arrangement(&mut self, arr: Option<Arrangement>) -> Vec<Out> {
        self.arr = arr;
        self.relayout()
    }

    /// the regions besides the touchpad: keyboard parts + the quick bar
    fn kb_regions(&self) -> Vec<String> {
        let mut v = self.layout.regions(&self.cfg.options);
        if self.cfg.quick && !self.quick_items.is_empty() {
            v.push(QUICK.to_string());
        }
        v
    }

    fn work_area(&self) -> (Rect, Rect) {
        let g = self.geo.unwrap();
        let sh = (self.cfg.strip_mm * g.px_per_mm()).round().min(g.h / 4.0);
        // M3: the bands (suggestions, clipboard) sit between the strip and the areas
        let bh = self.band_ids().len() as f32 * (BAND_MM * g.px_per_mm()).round();
        (Rect::new(0.0, 0.0, g.w, sh), Rect::new(0.0, sh + bh, g.w, g.h - sh - bh))
    }

    /// M3: the bands shown (in order from the top)
    fn band_ids(&self) -> Vec<&'static str> {
        let mut v = vec![];
        if self.cfg.suggest_band || self.cfg.emoji {
            v.push("suggest");
        }
        if self.cfg.clip_band {
            v.push("clip");
        }
        v
    }

    fn place_bands(&self, strip: &Rect, ppm: f32) -> Vec<BandView> {
        let bh = (BAND_MM * ppm).round();
        let gap = 1.0 * ppm;
        let mut out = vec![];
        for (i, id) in self.band_ids().into_iter().enumerate() {
            let r = Rect::new(0.0, strip.bottom() + i as f32 * bh, strip.w, bh);
            let mut labels: Vec<String> = vec![];
            let mut empty = String::new();
            match id {
                "suggest" => {
                    if self.cfg.emoji {
                        labels.push("\u{263a} Emoji".into());
                    }
                    labels.extend(self.suggestions.iter().cloned());
                    if self.suggestions.is_empty() && self.cfg.suggest_band {
                        empty = "suggestions appear while you type".into();
                    }
                }
                _ => {
                    labels.extend(self.clips.iter().cloned());
                    if labels.is_empty() {
                        empty = "clipboard history: copy something".into();
                    }
                }
            }
            let n = labels.len().max(1) as f32;
            let cw = ((r.w - gap) / n.max(if id == "clip" { 5.0 } else { 4.0 })).min(70.0 * ppm);
            let chips = labels.into_iter().enumerate().map(|(j, l)| (l, Rect::new(r.x + gap + j as f32 * cw, r.y + gap / 2.0, cw - gap, r.h - gap))).collect();
            out.push(BandView { id: id.into(), rect: r, chips, empty });
        }
        out
    }

    fn place_emoji(&self, area: &Rect, ppm: f32, page: usize, buttons: &mut Vec<ButtonView>) -> EmojiView {
        let pw = (220.0 * ppm).min(area.w - 4.0 * ppm);
        let ph = (area.h - 4.0 * ppm).max(40.0 * ppm);
        let p = Rect::new(area.x + (area.w - pw) / 2.0, area.y + 2.0 * ppm, pw, ph);
        let m = 2.0 * ppm;
        let th = 9.0 * ppm;
        let cats = crate::extras::EMOJI;
        let tw = (pw - 2.0 * m - 30.0 * ppm) / cats.len() as f32;
        for (i, c) in cats.iter().enumerate() {
            buttons.push(Self::btn(Btn::EmojiTab(i as u8), Rect::new(p.x + m + i as f32 * tw, p.y + m, tw - ppm, th), c.0, format!("emoji.tab.{}", i)));
        }
        buttons.push(Self::btn(Btn::PanelClose, Rect::new(p.right() - m - 28.0 * ppm, p.y + m, 28.0 * ppm, th), "Close", "panel.close".into()));
        let items = crate::extras::emoji_page(page);
        let gy = p.y + m + th + m;
        let cols = ((pw - 2.0 * m) / (14.0 * ppm)).floor().max(1.0) as usize;
        let cell = (pw - 2.0 * m) / cols as f32;
        let rows = items.len().div_ceil(cols).max(1);
        let ch = ((p.bottom() - m - gy) / rows as f32).min(cell);
        let mut v = vec![];
        for (i, e) in items.into_iter().enumerate() {
            let r = Rect::new(p.x + m + (i % cols) as f32 * cell, gy + (i / cols) as f32 * ch, cell - ppm, ch - ppm);
            buttons.push(Self::btn(Btn::Emoji(i as u8), r, &e, format!("emoji.item.{}", i)));
            v.push((e, r));
        }
        EmojiView { rect: p, page, items: v }
    }

    fn mins(&self) -> HashMap<String, (f32, f32)> {
        let ppm = self.ppm();
        let mut m = HashMap::new();
        m.insert(TOUCHPAD.to_string(), (40.0 * ppm, 30.0 * ppm));
        m.insert(QUICK.to_string(), (12.0 * ppm, 10.0 * ppm));
        for p in self.layout.visible_parts(&self.cfg.options) {
            let (mut uw, mut uh) = (0.0f32, 0.0f32);
            for g in &p.pages {
                let (w, h) = Layout::units(g, &self.cfg.options);
                uw = uw.max(w);
                uh = uh.max(h);
            }
            m.insert(p.region.clone(), (uw * 7.0 * ppm, uh * 6.0 * ppm));
        }
        m
    }

    fn place_quick(&self, r: &Rect, ppm: f32) -> Vec<QuickView> {
        let items = &self.quick_items;
        if items.is_empty() {
            return vec![];
        }
        let horiz = r.w >= r.h;
        let (axis, cross) = if horiz { (r.w, r.h) } else { (r.h, r.w) };
        let total: u32 = items.iter().map(|i| i.2.max(1)).sum();
        let min_len = 14.0 * ppm;
        let lines = ((total as f32 * min_len / axis).ceil() as usize).clamp(1, items.len()).min(((cross / (8.0 * ppm)).floor() as usize).max(1));
        let per_line = (total as f32 / lines as f32).ceil() as u32;
        let gap = 1.2 * ppm;
        let line_w = cross / lines as f32;
        let mut out = vec![];
        let mut chunks: Vec<Vec<&(String, String, u32)>> = vec![vec![]];
        let mut acc = 0;
        for it in items {
            if acc + it.2.max(1) > per_line && !chunks.last().unwrap().is_empty() && chunks.len() < lines {
                chunks.push(vec![]);
                acc = 0;
            }
            acc += it.2.max(1);
            chunks.last_mut().unwrap().push(it);
        }
        for (li, ch) in chunks.iter().enumerate() {
            let w: u32 = ch.iter().map(|i| i.2.max(1)).sum();
            let unit = axis / w.max(1) as f32;
            let mut pos = 0.0;
            for it in ch {
                let len = unit * it.2.max(1) as f32;
                let rect = if horiz {
                    Rect::new(r.x + pos + gap / 2.0, r.y + li as f32 * line_w + gap / 2.0, len - gap, line_w - gap)
                } else {
                    Rect::new(r.x + li as f32 * line_w + gap / 2.0, r.y + pos + gap / 2.0, line_w - gap, len - gap)
                };
                out.push(QuickView { id: it.0.clone(), label: it.1.clone(), rect });
                pos += len;
            }
        }
        out
    }

    fn btn(b: Btn, rect: Rect, label: &str, name: String) -> ButtonView {
        ButtonView { btn: b, rect, label: label.to_string(), name }
    }

    /// the settings sheet: tabs on the left, the rows of the selected module on the right, paged
    fn place_sheet(&mut self, area: &Rect, ppm: f32, buttons: &mut Vec<ButtonView>) -> SheetView {
        let pw = (240.0 * ppm).min(area.w - 4.0 * ppm);
        let ph = (area.h - 4.0 * ppm).max(40.0 * ppm);
        let p = Rect::new(area.x + (area.w - pw) / 2.0, area.y + 2.0 * ppm, pw, ph);
        let m = 2.0 * ppm;
        let tab_w = (52.0 * ppm).min(pw * 0.32);
        let n = self.sheet.len().max(1);
        let tab_h = ((ph - 2.0 * m) / n as f32).min(10.0 * ppm);
        self.sheet_tab = self.sheet_tab.min(n - 1);
        for (i, md) in self.sheet.iter().enumerate() {
            let r = Rect::new(p.x + m, p.y + m + i as f32 * tab_h, tab_w, tab_h - 0.6 * ppm);
            let label = if md.live { md.title.clone() } else { format!("{} \u{b7}", md.title) };
            buttons.push(Self::btn(Btn::SheetTab(i as u8), r, &label, format!("sheet.tab.{}", md.id)));
        }
        let rx = p.x + tab_w + 2.0 * m;
        let rw = p.right() - m - rx;
        let head = 12.0 * ppm;
        let foot = 11.0 * ppm;
        let row_h = 10.5 * ppm;
        let per_page = (((ph - head - foot - 2.0 * m) / row_h).floor() as usize).max(1);
        let mut view = SheetView { rect: p, tab: self.sheet_tab, ..Default::default() };
        self.sheet_keys.clear();
        if let Some(md) = self.sheet.get(self.sheet_tab).cloned() {
            view.title = md.title.clone();
            // M3: a live module may carry a status line (USB HID: power, data role, host)
            view.subtitle = if md.live { md.plan.clone() } else { format!("planned ({}): settings are stored now", md.plan) };
            let pages = md.rows.len().div_ceil(per_page).max(1);
            self.sheet_page = self.sheet_page.min(pages - 1);
            let first = self.sheet_page * per_page;
            for (j, row) in md.rows.iter().enumerate().skip(first).take(per_page) {
                let i = j - first;
                let y = p.y + m + head + i as f32 * row_h;
                let rr = Rect::new(rx, y, rw, row_h - 1.0 * ppm);
                view.rows.push(SheetRowView { rect: rr, title: row.title.clone(), live: row.live });
                let vw = 30.0 * ppm;
                let bh = rr.h - 1.6 * ppm;
                let by = rr.y + 0.8 * ppm;
                let k = row.key.clone();
                let idx = i as u8;
                match row.kind {
                    Kind::Num { .. } => {
                        let bw = 10.0 * ppm;
                        buttons.push(Self::btn(Btn::SheetPlus(idx), Rect::new(rr.right() - bw, by, bw, bh), "+", format!("sheet.plus.{}", k)));
                        buttons.push(Self::btn(Btn::SheetRow(idx), Rect::new(rr.right() - bw - vw - ppm, by, vw, bh), &row.value, format!("sheet.row.{}", k)));
                        buttons.push(Self::btn(Btn::SheetMinus(idx), Rect::new(rr.right() - 2.0 * bw - vw - 2.0 * ppm, by, bw, bh), "\u{2212}", format!("sheet.minus.{}", k)));
                    }
                    _ => {
                        buttons.push(Self::btn(Btn::SheetRow(idx), Rect::new(rr.right() - vw, by, vw, bh), &row.value, format!("sheet.row.{}", k)));
                        if k.starts_with("quick:") && row.value != "off" {
                            let bw = 10.0 * ppm;
                            buttons.push(Self::btn(Btn::SheetUp(idx), Rect::new(rr.right() - vw - bw - ppm, by, bw, bh), "\u{2191}", format!("sheet.up.{}", k)));
                        }
                    }
                }
                self.sheet_keys.push(k);
            }
            let fy = p.bottom() - m - foot + 1.0 * ppm;
            if pages > 1 {
                let bw = 24.0 * ppm;
                buttons.push(Self::btn(Btn::SheetPrev, Rect::new(rx, fy, bw, foot - 2.0 * ppm), "\u{2039} Back", "sheet.prev".into()));
                buttons.push(Self::btn(Btn::SheetNext, Rect::new(rx + bw + m, fy, bw, foot - 2.0 * ppm), &format!("More {}/{} \u{203a}", self.sheet_page + 1, pages), "sheet.next".into()));
            }
            let cw = 26.0 * ppm;
            buttons.push(Self::btn(Btn::PanelClose, Rect::new(p.right() - m - cw, fy, cw, foot - 2.0 * ppm), "Close", "panel.close".into()));
        }
        view
    }

    /// the laptop preset applies (a layout with a main "keyboard" region, not while editing)
    fn laptop_active(&self) -> bool {
        self.cfg.laptop && !self.edit && self.layout.regions(&self.cfg.options).iter().any(|r| r == "keyboard")
    }

    /// M2.1 "laptop" preset: keys on a laptop grid (max 19.05 mm pitch, square keys, >= 15 units wide), the
    /// keyboard centred instead of stretched, the touchpad below with a width that follows the keyboard
    fn laptop_arrangement(&self, area: &Rect, ppm: f32) -> Arrangement {
        let units = |region: &str| {
            self.layout.visible_parts(&self.cfg.options).find(|p| p.region == region).map(|p| Layout::units(Layout::page_of(p, &self.page), &self.cfg.options))
        };
        let kb = units("keyboard").unwrap_or((15.0, 5.0));
        let quick = self.cfg.quick && !self.quick_items.is_empty();
        let mut a = arrangement::laptop(area, ppm, kb, units("numpad"), quick, self.cfg.laptop_pad_pct);
        a.layout = self.layout.name.clone();
        a
    }

    /// recompute every rect from layout + arrangement + geometry
    fn relayout(&mut self) -> Vec<Out> {
        let mut out = vec![];
        let Some(g) = self.geo else { return out };
        let ppm = g.px_per_mm();
        self.keys.slop = self.cfg.key_slop_mm * ppm;
        let (strip, area) = self.work_area();
        let o = self.orient.unwrap_or(Orientation::Landscape);
        let kb = self.kb_regions();
        let mut required = vec![TOUCHPAD.to_string()];
        required.extend(kb.iter().cloned());
        let defaults = Arrangement::default_for(&self.layout.name, &kb, o);
        let laptop = self.laptop_active();
        let mut arr = if laptop { self.laptop_arrangement(&area, ppm) } else { self.arr.clone().unwrap_or_else(|| defaults.clone()) };
        arr.layout = self.layout.name.clone();
        arr.orientation = o.name().into();
        if !laptop {
            for n in arr.fixup(&required, &defaults, &area, &self.mins()) {
                out.push(Out::Note(format!("arrangement: {}", n)));
            }
            self.arr = Some(arr.clone());
        }
        let regions: Vec<RegionView> = required
            .iter()
            .filter_map(|id| arr.get(id).map(|f| RegionView { id: id.clone(), rect: Rect::from_frac(&f, &area) }))
            .collect();
        // keys
        let mut keys = vec![];
        for p in self.layout.visible_parts(&self.cfg.options) {
            if let Some(r) = regions.iter().find(|r| r.id == p.region) {
                keys.extend(Layout::place(p, &self.page, &r.rect, &self.cfg.options, self.cfg.key_gap_mm * ppm));
            }
        }
        let quick = regions.iter().find(|r| r.id == QUICK).map(|r| self.place_quick(&r.rect, ppm)).unwrap_or_default();
        // touchpad: while editing, the virtual device keeps its old size (recreated once when edit mode ends)
        let pad_rect = regions.iter().find(|r| r.id == TOUCHPAD).map(|r| r.rect).unwrap_or(area);
        let zone = if self.cfg.click_zone_mm > 0.0 { self.cfg.click_zone_mm * ppm } else { 0.0 };
        let upp = self.cfg.pad_units_per_mm / ppm;
        let changed = match &self.pad {
            Some(p) => p.rect != pad_rect || (p.units_per_px - upp).abs() > 1e-4 || (p.zone_h - zone.min(pad_rect.h / 3.0)).abs() > 1e-3,
            None => true,
        };
        if changed && !(self.edit && self.pad.is_some()) {
            let old_dims = self.pad.as_ref().map(|p| p.dims());
            if let Some(p) = self.pad.as_mut() {
                if let Some(f) = p.release_all() {
                    out.push(Out::Pad(f));
                }
            }
            let np = PadMapper::new(pad_rect, upp, zone);
            // only a new size / resolution needs a new virtual device; a moved pad keeps it
            if old_dims != Some(np.dims()) || self.pad.as_ref().is_some_and(|p| (p.units_per_px - upp).abs() > 1e-4) {
                out.push(Out::PadChanged);
            }
            self.pad = Some(np);
        }
        if let Some(p) = self.pad.as_mut() {
            p.chord_middle = self.cfg.chord_middle;
            p.inertia = self.cfg.inertia;
            p.units_per_mm = self.cfg.pad_units_per_mm;
        }
        // buttons
        let m = (strip.h * 0.14).round();
        let bh = strip.h - 2.0 * m;
        let mut buttons = vec![];
        let specs: Vec<(Btn, f32, String, &str)> = if self.edit {
            vec![(Btn::Cancel, 24.0, "Cancel".into(), "strip.cancel"), (Btn::Reset, 24.0, "Reset".into(), "strip.reset"), (Btn::Done, 24.0, "Done".into(), "strip.done")]
        } else {
            let mut v = vec![
                (Btn::Exit, 18.0, "Exit".into(), "strip.exit"),
                (Btn::Settings, 26.0, "Settings".into(), "strip.settings"),
                (Btn::Layout, 40.0, format!("Keyboard: {}", self.layout.title), "strip.layout"),
                (Btn::Display, 36.0, "Use as display".into(), "strip.display"),
            ];
            if let Some(t) = &self.cfg.target_label {
                v.push((Btn::Target, 34.0, t.clone(), "strip.target"));
            }
            v
        };
        let mut x = strip.right() - m;
        for (b, wmm, label, name) in specs {
            let w = (wmm * ppm).min(strip.w / 5.0);
            x -= w;
            buttons.push(Self::btn(b, Rect::new(x, strip.y + m, w, bh), &label, name.into()));
            x -= m;
        }
        let sheet = if self.sheet_open && !self.edit { Some(self.place_sheet(&area, ppm, &mut buttons)) } else { None };
        // M3: bands, the emoji panel, the mirror preview
        let bands = if self.edit { vec![] } else { self.place_bands(&strip, ppm) };
        let emoji = match self.emoji_page {
            Some(pg) if !self.edit && sheet.is_none() => Some(self.place_emoji(&area, ppm, pg, &mut buttons)),
            _ => None,
        };
        let mirror = if self.cfg.mirror_pct > 0.0 && !self.edit { Some(crate::extras::mirror_rect(&pad_rect, self.mirror_src.0, self.mirror_src.1, self.cfg.mirror_pct, 2.0 * ppm)) } else { None };
        let zones = if self.edit { PadMapper::new(pad_rect, upp, zone).zones() } else { self.pad.as_ref().and_then(|p| p.zones()) };
        let version = self.model.version + 1;
        let notice = if self.pen && self.cfg.pen_pause { "pen near: touches paused".to_string() } else { self.notice.clone() };
        self.model = UiModel {
            version,
            w: g.w,
            h: g.h,
            px_per_mm: ppm,
            strip,
            title: "DexHid".into(),
            hint: if self.edit { "drag an area to move it, an edge or corner to resize it".into() } else { "hold the bar to move / resize the areas".into() },
            buttons,
            regions,
            zones,
            keys,
            shift: self.keys.shift_held(),
            edit: self.edit,
            sheet,
            quick,
            guides: self.guides.clone(),
            info: self.info.clone(),
            theme: self.theme.clone(),
            leds: self.leds,
            altgr: self.keys.held_codes().contains(&keycodes::KEY_RIGHTALT),
            laptop,
            desk: self.desk.clone(),
            latched: self.keys.latched(),
            fn_on: self.keys.fn_active(),
            notice,
            bands,
            emoji,
            mirror,
            select: self.select,
            laser: self.cfg.laser,
            lit_keys: self.keys.lit_keys(),
            lit_button: None,
            lit_quick: None,
            dots: self.contacts.values().map(|t| (t.x, t.y)).collect(),
        };
        out.push(Out::Static);
        out
    }

    fn update_dynamic(&mut self, out: &mut Vec<Out>) {
        let lit = self.keys.lit_keys();
        let dots: Vec<(f32, f32)> = self.contacts.values().filter(|t| !matches!(t.owner, Owner::Ignored)).map(|t| (t.x, t.y)).collect();
        let btn = self.contacts.values().find_map(|t| match t.owner {
            Owner::Strip { btn: Some(b), long: false, .. } => {
                let r = self.model.buttons.iter().find(|v| v.btn == b)?.rect;
                r.contains(t.x, t.y).then_some(b)
            }
            Owner::Button(b) => Some(b),
            _ => None,
        });
        let lq = self.contacts.values().find_map(|t| if let Owner::Quick(i) = t.owner { Some(i) } else { None });
        if lit != self.model.lit_keys || dots != self.model.dots || btn != self.model.lit_button || lq != self.model.lit_quick {
            self.model.lit_keys = lit;
            self.model.dots = dots;
            self.model.lit_button = btn;
            self.model.lit_quick = lq;
            out.push(Out::Dynamic);
        }
        let latched = self.keys.latched();
        let altgr = self.keys.held_codes().contains(&keycodes::KEY_RIGHTALT);
        if self.keys.shift_held() != self.model.shift || latched != self.model.latched || self.keys.fn_active() != self.model.fn_on || altgr != self.model.altgr {
            self.model.altgr = altgr;
            self.model.shift = self.keys.shift_held();
            self.model.latched = latched;
            self.model.fn_on = self.keys.fn_active();
            self.model.version += 1;
            out.push(Out::Static);
        }
    }

    fn hit_button(&self, x: f32, y: f32) -> Option<Btn> {
        self.model.buttons.iter().find(|b| b.rect.contains(x, y)).map(|b| b.btn)
    }

    fn hit_key(&self, x: f32, y: f32) -> Option<usize> {
        self.model.keys.iter().position(|k| k.rect.contains(x, y))
    }

    fn key_outs(&mut self, v: Vec<KeyOut>, t: i64, out: &mut Vec<Out>) {
        for k in v {
            match k {
                KeyOut::Key(c, p) => {
                    if p && Some(c) != self.cfg.mask_key {
                        // M2.1 fix: like libinput, a modifier does not start disable-while-typing (Ctrl / Shift /
                        // Super resting + a pad finger = Ctrl+click, Shift+drag, ...)
                        if !crate::keycodes::is_modifier(c) {
                            self.last_key_t = t;
                        }
                        out.push(Out::KeyFeedback);
                    }
                    // M3 select-while-moving: Shift around the caret keys
                    let shift = self.select && crate::extras::is_nav(c);
                    if shift && p {
                        out.push(Out::Key(keycodes::KEY_LEFTSHIFT, true));
                    }
                    out.push(Out::Key(c, p));
                    if shift && !p {
                        out.push(Out::Key(keycodes::KEY_LEFTSHIFT, false));
                    }
                }
                KeyOut::Action(Action::SelectMode) => {
                    if self.cfg.textedit {
                        self.select = !self.select;
                        out.push(Out::Note(format!("select mode {}", if self.select { "on" } else { "off" })));
                        out.extend(self.relayout());
                    }
                }
                KeyOut::Action(Action::Joystick) => {}
                KeyOut::Action(Action::EmojiPanel) => {
                    if self.cfg.emoji {
                        out.extend(self.toggle_emoji());
                    }
                }
                KeyOut::Action(Action::Timer(s)) => out.push(Out::Timer(s)),
                KeyOut::Action(Action::Laser) => out.push(Out::SetSetting("features.remote.options.laser".into(), (!self.cfg.laser).to_string())),
                KeyOut::Action(Action::Page(id)) => {
                    self.page = id;
                    out.extend(self.relayout());
                }
                KeyOut::Action(Action::LayoutNext) => out.push(Out::LayoutNext),
                KeyOut::Action(Action::KeyboardHide) => out.push(Out::SetLayout("none".into())),
                KeyOut::Action(Action::Fn) => {}
            }
        }
    }

    fn pad_frame(&mut self, out: &mut Vec<Out>) {
        if let Some(f) = self.pad.as_mut().and_then(|p| p.take_frame()) {
            out.push(Out::Pad(f));
        }
    }

    /// M2.1: contacts that can take part in a whole-screen swipe (not in edit mode / the settings sheet)
    fn swipe_update(&mut self) {
        let live: Vec<(ContactId, f32, f32)> = if self.edit || self.sheet_open {
            vec![]
        } else {
            let mut v: Vec<(ContactId, f32, f32)> = self
                .contacts
                .iter()
                .filter(|(_, t)| matches!(t.owner, Owner::Strip { .. } | Owner::Pad | Owner::Key | Owner::Quick(_) | Owner::Gesture))
                .map(|(id, t)| (*id, t.x, t.y))
                .collect();
            v.sort_by_key(|x| x.0);
            v
        };
        self.swipe.contacts_changed(&live);
    }

    fn synth_frame(&self) -> PadFrame {
        let mut f = PadFrame::default();
        if let Some(s) = &self.synth {
            if !s.lifted {
                for (i, p) in s.fingers.iter().enumerate().take(SLOTS) {
                    f.slots[i] = Some((p.0.round() as i32, p.1.round() as i32));
                }
            }
        }
        f
    }

    /// move the synthetic fingers by (dx, dy) device units (one finger or all), clamped to the device
    fn synth_move(&mut self, which: Option<usize>, d: (f32, f32)) {
        let Some((xm, ym)) = self.pad.as_ref().map(|p| p.dims()) else { return };
        if let Some(s) = self.synth.as_mut() {
            for (i, p) in s.fingers.iter_mut().enumerate() {
                if which.is_none_or(|w| w == i) {
                    p.0 = (p.0 + d.0).clamp(0.0, xm as f32);
                    p.1 = (p.1 + d.1).clamp(0.0, ym as f32);
                }
            }
        }
    }

    /// M2.1: a whole-screen swipe won: release panel-owned keys / buttons / scrolling, capture every contact, put
    /// the same number of synthetic fingers on the virtual touchpad so libinput recognises the desktop's gesture
    fn start_swipe(&mut self, r: crate::swipe::Recognised, t: i64, out: &mut Vec<Out>) {
        let n = r.ids.len();
        // M3 gesture shortcuts: the gesture is ours (on the touchpad too); nothing is handed to the desktop
        if self.cfg.gestures {
            self.capture_gesture(t, out, 1);
            for tr in self.contacts.values_mut() {
                tr.owner = Owner::Ignored;
            }
            self.taps.spoil();
            let name = crate::gestures::swipe_name(n, r.dir);
            out.push(Out::Swipe(format!("gesture {} fingers={} dir={:.2},{:.2}", name, n, r.dir.0, r.dir.1)));
            out.push(Out::Gesture(name));
            return;
        }
        if r.ids.iter().all(|id| matches!(self.contacts.get(id).map(|c| &c.owner), Some(Owner::Pad))) {
            out.push(Out::Swipe(format!("native fingers={} (all on the touchpad: libinput's own gesture)", n)));
            return;
        }
        let held = self.keys.held_codes().len();
        let v = self.keys.release_all();
        self.key_outs(v, t, out);
        let mut buttons = 0;
        if let Some(p) = self.pad.as_mut() {
            buttons = p.buttons_down().len();
            if let Some(f) = p.release_all() {
                out.push(Out::Pad(f));
            }
        }
        for (id, tr) in self.contacts.iter_mut() {
            tr.owner = match r.ids.iter().position(|x| x == id) {
                Some(i) => Owner::Swipe(i),
                None => Owner::Ignored,
            };
        }
        let Some(p) = self.pad.as_ref() else { return };
        let (xm, ym) = p.dims();
        let upm = self.cfg.pad_units_per_mm;
        let spacing = (15.0 * upm).min(xm as f32 / (n as f32 + 1.0));
        let cx = xm as f32 * (0.5 - 0.3 * r.dir.0);
        let cy = ym as f32 * (0.5 - 0.3 * r.dir.1);
        let fingers: Vec<(f32, f32)> = (0..n).map(|i| ((cx + (i as f32 - (n as f32 - 1.0) / 2.0) * spacing).clamp(0.0, xm as f32), cy.clamp(0.0, ym as f32))).collect();
        // the travel before recognition is delivered in small steps (no jump for libinput), at most 25 mm
        let mut catch = (r.travel.0 * p.units_per_px, r.travel.1 * p.units_per_px);
        let cl = (catch.0 * catch.0 + catch.1 * catch.1).sqrt();
        if cl > 25.0 * upm {
            catch = (catch.0 * 25.0 * upm / cl, catch.1 * 25.0 * upm / cl);
        }
        self.synth = Some(Synth { fingers, lifted: false, catch, next: t + crate::touchpad::STEP_MS });
        out.push(Out::Pad(self.synth_frame()));
        out.push(Out::Swipe(format!("start fingers={} dir={:.2},{:.2} released_keys={} released_buttons={}", n, r.dir.0, r.dir.1, held, buttons)));
    }

    fn end_synth(&mut self, out: &mut Vec<Out>, why: &str) {
        if let Some(s) = self.synth.as_mut() {
            if !s.lifted {
                s.lifted = true;
                out.push(Out::Pad(PadFrame::default()));
                out.push(Out::Swipe(format!("end ({})", why)));
            }
        }
    }

    pub fn contact(&mut self, c: Contact, t: i64) -> Vec<Out> {
        let mut out = vec![];
        let Some(g) = self.geo else { return out };
        match c {
            Contact::Down { id, x, y, major } => {
                let (lx, ly) = g.touch_to_logical(x, y);
                // any new touch stops a scroll inertia (its virtual fingers lift in their own frame)
                if let Some(p) = self.pad.as_mut() {
                    if p.coasting() {
                        p.stop_coast();
                        self.pad_frame(&mut out);
                        out.push(Out::Note("inertia stopped by a touch".into()));
                    }
                }
                let owner = if self.swipe.phase() == Phase::Active { Owner::Ignored } else { self.owner_for_down(id, lx, ly, major, t, &mut out) };
                let eligible = matches!(owner, Owner::Pad | Owner::Key | Owner::Quick(_));
                self.contacts.insert(id, Track { x: lx, y: ly, x0: lx, y0: ly, owner, t0: t });
                // M3 gesture shortcuts: 3+ fingers that land together, or a third finger on the touchpad
                if self.cfg.gestures && eligible && !self.edit && !self.sheet_open {
                    self.taps.cfg.slop_px = 3.0 * g.px_per_mm();
                    self.taps.down(id, lx, ly, t);
                    let on_pad = self.contacts.values().filter(|c| matches!(c.owner, Owner::Pad)).count();
                    let capturing = self.contacts.values().any(|c| matches!(c.owner, Owner::Gesture));
                    if self.taps.landing(t) || on_pad >= 3 || capturing {
                        self.capture_gesture(t, &mut out, 1);
                    }
                }
                if self.cfg.five_finger && self.contacts.len() >= 5 && self.five.is_none() && !self.five_blocked {
                    self.five = Some((t, false));
                }
                self.swipe_update();
            }
            Contact::Move { id, x, y, .. } => {
                let (lx, ly) = g.touch_to_logical(x, y);
                let Some(tr) = self.contacts.get_mut(&id) else { return out };
                let (ox, oy) = (tr.x, tr.y);
                tr.x = lx;
                tr.y = ly;
                let tr = tr.clone();
                if let Owner::Swipe(i) = tr.owner {
                    if self.synth.as_ref().is_some_and(|s| !s.lifted) {
                        let upp = self.pad.as_ref().map(|p| p.units_per_px).unwrap_or(1.0);
                        self.synth_move(Some(i), ((lx - ox) * upp, (ly - oy) * upp));
                        out.push(Out::Pad(self.synth_frame()));
                    }
                }
                let ppm = g.px_per_mm();
                let moved = ((tr.x - tr.x0).powi(2) + (tr.y - tr.y0).powi(2)).sqrt();
                if self.five.is_some() && moved > self.cfg.five_slop_mm * ppm {
                    self.five = None;
                    self.five_blocked = true;
                }
                if self.cfg.gestures {
                    self.taps.moved(id, lx, ly);
                }
                match tr.owner {
                    Owner::Pad => {
                        if let Some(p) = self.pad.as_mut() {
                            p.motion(id, lx, ly, t);
                        }
                        self.pad_frame(&mut out);
                    }
                    Owner::Laser => {
                        if let Some(p) = self.pad.as_ref() {
                            out.push(Out::Laser(Some(((lx - p.rect.x) / p.rect.w, (ly - p.rect.y) / p.rect.h))));
                        }
                    }
                    Owner::Joystick { key, .. } => {
                        let j = crate::extras::joystick((lx - tr.x0) / ppm, (ly - tr.y0) / ppm, self.cfg.joy_dead_mm);
                        let nk = j.map(|x| x.0);
                        if nk != key {
                            if let Some((k, iv)) = j {
                                self.nav_tap(k, &mut out);
                                if let Some(Track { owner: Owner::Joystick { key, iv: i, next }, .. }) = self.contacts.get_mut(&id) {
                                    *key = Some(k);
                                    *i = iv;
                                    *next = t + iv;
                                }
                            } else if let Some(Track { owner: Owner::Joystick { key, .. }, .. }) = self.contacts.get_mut(&id) {
                                *key = None;
                            }
                        } else if let (Some(_), Some((_, iv))) = (key, j) {
                            if let Some(Track { owner: Owner::Joystick { iv: i, .. }, .. }) = self.contacts.get_mut(&id) {
                                *i = iv;
                            }
                        }
                    }
                    Owner::Key => {
                        // off the key = outside its rect grown by the slop (hysteresis at the key edges)
                        let slop = self.cfg.key_slop_mm * ppm;
                        let on = self.keys.key_of(id).and_then(|k| self.model.keys.get(k)).is_some_and(|k| k.rect.inset(-slop).contains(lx, ly));
                        let v = self.keys.motion(id, on, (lx, ly), t);
                        self.key_outs(v, t, &mut out);
                    }
                    Owner::Edit { handle, ref region, orig, x0, y0 } => {
                        let (_, area) = self.work_area();
                        let others: Vec<Rect> = self.model.regions.iter().filter(|r| &r.id != region).map(|r| r.rect).collect();
                        let min = self.mins().get(region).copied().unwrap_or((10.0, 10.0));
                        let ctx = EditCtx { area, snap: self.cfg.snap_mm * ppm, min, others: &others };
                        let nr = arrangement::drag(&orig, handle, lx - x0, ly - y0, &ctx);
                        let cur = self.model.regions.iter().find(|r| &r.id == region).map(|r| r.rect);
                        let gs = arrangement::guides(&nr, &ctx);
                        if cur != Some(nr) || gs != self.guides {
                            self.guides = gs;
                            if let Some(a) = self.arr.as_mut() {
                                a.set(region, nr.to_frac(&area));
                            }
                            out.extend(self.relayout());
                        }
                    }
                    Owner::Strip { long: false, .. } if moved > self.cfg.slop_mm * ppm => {
                        // moved: no long press any more (the button still fires if released on it)
                        if let Some(Track { owner: Owner::Strip { t0, .. }, .. }) = self.contacts.get_mut(&id) {
                            *t0 = i64::MAX / 2;
                        }
                    }
                    _ => {}
                }
                if self.swipe.phase() == Phase::Candidate {
                    let pos: HashMap<ContactId, (f32, f32)> = self.contacts.iter().map(|(i, t)| (*i, (t.x, t.y))).collect();
                    self.swipe.cfg.min_px = self.cfg.swipe_min_mm * ppm;
                    if let Some(r) = self.swipe.check(&pos) {
                        self.start_swipe(r, t, &mut out);
                    }
                }
            }
            Contact::Up { id } => {
                let Some(tr) = self.contacts.remove(&id) else { return out };
                if self.cfg.gestures {
                    if let Some(n) = self.taps.up(id, t) {
                        out.push(Out::Swipe(format!("gesture tap{} fingers={}", n, n)));
                        out.push(Out::Gesture(format!("tap{}", n)));
                    }
                }
                match tr.owner {
                    Owner::Swipe(_) => self.end_synth(&mut out, "a finger lifted"),
                    Owner::Laser => out.push(Out::Laser(None)),
                    Owner::Chip(b, i) => {
                        if let Some(band) = self.model.bands.get(b).cloned() {
                            if band.chips.get(i).is_some_and(|c| c.1.contains(tr.x, tr.y)) {
                                if band.id == "suggest" && self.cfg.emoji {
                                    if i == 0 {
                                        out.extend(self.toggle_emoji());
                                    } else {
                                        out.push(Out::Chip(band.id.clone(), i - 1));
                                    }
                                } else {
                                    out.push(Out::Chip(band.id.clone(), i));
                                }
                            }
                        }
                    }
                    Owner::Strip { btn: Some(b), long: false, .. } => {
                        if self.hit_button(tr.x, tr.y) == Some(b) {
                            self.fire(b, &mut out);
                        }
                    }
                    Owner::Button(b) => {
                        if self.hit_button(tr.x, tr.y) == Some(b) {
                            self.fire(b, &mut out);
                        }
                    }
                    Owner::Pad => {
                        if let Some(p) = self.pad.as_mut() {
                            p.up(id, t);
                            if p.coasting() {
                                out.push(Out::Note("scroll inertia: virtual fingers kept".into()));
                            }
                        }
                        self.pad_frame(&mut out);
                    }
                    Owner::Key => {
                        let v = self.keys.up(id);
                        self.key_outs(v, t, &mut out);
                    }
                    Owner::Quick(i) => {
                        if let Some(q) = self.model.quick.get(i) {
                            if q.rect.contains(tr.x, tr.y) {
                                out.push(Out::Quick(q.id.clone()));
                            }
                        }
                    }
                    Owner::Edit { .. } => {
                        if !self.guides.is_empty() {
                            self.guides.clear();
                            out.extend(self.relayout());
                        }
                    }
                    _ => {}
                }
                if self.contacts.len() < 5 {
                    self.five = None;
                    if self.contacts.is_empty() {
                        self.five_blocked = false;
                    }
                }
                if self.contacts.is_empty() {
                    if self.swipe.phase() == Phase::Active {
                        out.push(Out::Swipe("all contacts lifted".into()));
                    }
                    self.swipe.reset();
                    self.synth = None;
                } else {
                    self.swipe_update();
                }
            }
        }
        self.update_dynamic(&mut out);
        out
    }

    fn in_rest_zone(&self, x: f32, y: f32) -> bool {
        let (_, area) = self.work_area();
        let rz = self.cfg.rest_zone_mm * self.ppm();
        if rz > 0.0 && (x < area.x + rz || x >= area.right() - rz || y >= area.bottom() - rz) {
            return true;
        }
        self.cfg.rest_zones.iter().any(|f| Rect::from_frac(f, &area).contains(x, y))
    }

    fn owner_for_down(&mut self, id: ContactId, x: f32, y: f32, major: i32, t: i64, out: &mut Vec<Out>) -> Owner {
        let strip = self.model.strip;
        if (self.sheet_open || self.emoji_page.is_some()) && !self.edit {
            if let Some(b) = self.hit_button(x, y) {
                return if strip.contains(x, y) { Owner::Strip { btn: Some(b), t0: t, long: false } } else { Owner::Button(b) };
            }
            return if strip.contains(x, y) { Owner::Strip { btn: None, t0: t, long: false } } else { Owner::Ignored };
        }
        if strip.contains(x, y) {
            return Owner::Strip { btn: self.hit_button(x, y), t0: t, long: false };
        }
        if self.edit {
            let hs = self.cfg.handle_mm * self.ppm();
            // smaller regions first, so a region inside a bigger one stays reachable
            let mut regs = self.model.regions.clone();
            regs.sort_by(|a, b| (a.rect.w * a.rect.h).total_cmp(&(b.rect.w * b.rect.h)));
            for r in regs {
                if let Some(h) = arrangement::hit_handle(&r.rect, x, y, hs) {
                    return Owner::Edit { handle: h, region: r.id.clone(), orig: r.rect, x0: x, y0: y };
                }
            }
            return Owner::Ignored;
        }
        // M2 filters: pen near, rest zones, palms
        if self.pen && self.cfg.pen_pause {
            out.push(Out::Note(format!("pen: contact {} ignored", id)));
            return Owner::Ignored;
        }
        if self.in_rest_zone(x, y) {
            out.push(Out::Note(format!("rest zone: contact {} ignored", id)));
            return Owner::Ignored;
        }
        let palm = self.cfg.palm_major > 0 && major > self.cfg.palm_major;
        // M3: band chips (suggestions, clipboard)
        for (bi, b) in self.model.bands.iter().enumerate() {
            if b.rect.contains(x, y) {
                return match b.chips.iter().position(|c| c.1.contains(x, y)) {
                    Some(ci) if !palm => Owner::Chip(bi, ci),
                    _ => Owner::Ignored,
                };
            }
        }
        for r in &self.model.regions {
            if !r.rect.contains(x, y) {
                continue;
            }
            if r.id == TOUCHPAD {
                if palm {
                    out.push(Out::Note(format!("palm: contact {} ignored (major {})", id, major)));
                    return Owner::Ignored;
                }
                // M3 presentation laser: the pad is an absolute pointer area (one finger)
                if self.cfg.laser {
                    if self.contacts.values().any(|c| matches!(c.owner, Owner::Laser)) {
                        return Owner::Ignored;
                    }
                    out.push(Out::Laser(Some(((x - r.rect.x) / r.rect.w, (y - r.rect.y) / r.rect.h))));
                    return Owner::Laser;
                }
                if self.cfg.dwt_ms > 0 && t - self.last_key_t < self.cfg.dwt_ms {
                    out.push(Out::Note(format!("dwt: pad contact {} ignored ({} ms after a key)", id, t - self.last_key_t)));
                    return Owner::Ignored;
                }
                if let Some(p) = self.pad.as_mut() {
                    if p.down(id, x, y, t) {
                        self.pad_frame(out);
                        return Owner::Pad;
                    }
                }
                return Owner::Ignored;
            }
            if palm && self.cfg.palm_keyboard {
                out.push(Out::Note(format!("palm: contact {} on {} ignored (major {})", id, r.id, major)));
                return Owner::Ignored;
            }
            if r.id == QUICK {
                return match self.model.quick.iter().position(|q| q.rect.contains(x, y)) {
                    Some(i) => Owner::Quick(i),
                    None => Owner::Ignored,
                };
            }
            if let Some(k) = self.hit_key(x, y) {
                let kind = self.model.keys[k].kind.clone();
                if kind == KeyKind::Action(Action::Joystick) {
                    return if self.cfg.textedit { Owner::Joystick { key: None, iv: 400, next: i64::MAX / 2 } } else { Owner::Ignored };
                }
                let long = self.model.keys[k].long.clone();
                let v = self.keys.down(id, k, &kind, long.as_ref(), t, (x, y));
                self.key_outs(v, t, out);
                return Owner::Key;
            }
            return Owner::Ignored;
        }
        Owner::Ignored
    }

    fn enter_edit(&mut self, out: &mut Vec<Out>) {
        out.extend(self.release_all());
        for tr in self.contacts.values_mut() {
            if !matches!(tr.owner, Owner::Strip { .. }) {
                tr.owner = Owner::Ignored;
            }
        }
        if self.laptop_active() {
            // editing starts from the laptop arrangement and switches to free placement
            if let Some(g) = self.geo {
                let (_, area) = self.work_area();
                self.arr = Some(self.laptop_arrangement(&area, g.px_per_mm()));
            }
            out.push(Out::SetSetting("arrangement".into(), "free".into()));
        }
        self.arr_backup = self.arr.clone();
        self.edit = true;
        self.sheet_open = false;
        out.push(Out::EditMode(true));
        out.extend(self.relayout());
    }

    fn leave_edit(&mut self, out: &mut Vec<Out>) {
        self.edit = false;
        self.guides.clear();
        out.push(Out::EditMode(false));
    }

    fn sheet_key(&self, i: u8) -> Option<String> {
        self.sheet_keys.get(i as usize).cloned()
    }

    fn fire(&mut self, b: Btn, out: &mut Vec<Out>) {
        let name = self.model.buttons.iter().find(|v| v.btn == b).map(|v| v.name.clone()).unwrap_or_default();
        out.push(Out::Note(format!("button {}", name)));
        match b {
            Btn::Display => out.push(Out::UseAsDisplay),
            Btn::Exit => out.push(Out::Exit),
            Btn::Layout => out.push(Out::LayoutNext),
            Btn::Settings => {
                self.sheet_open = !self.sheet_open;
                out.push(Out::PanelOpen(self.sheet_open));
                out.extend(self.relayout());
            }
            Btn::PanelClose => {
                self.sheet_open = false;
                self.emoji_page = None;
                out.push(Out::PanelOpen(false));
                out.extend(self.relayout());
            }
            Btn::Target => out.push(Out::TargetNext),
            Btn::EmojiTab(i) => {
                self.emoji_page = Some(i as usize);
                out.extend(self.relayout());
            }
            Btn::Emoji(i) => {
                if let Some(e) = self.model.emoji.as_ref().and_then(|v| v.items.get(i as usize)) {
                    out.push(Out::Emoji(e.0.clone()));
                }
            }
            Btn::SheetTab(i) => {
                self.sheet_tab = i as usize;
                self.sheet_page = 0;
                out.extend(self.relayout());
            }
            Btn::SheetPrev | Btn::SheetNext => {
                if b == Btn::SheetNext {
                    self.sheet_page += 1;
                    let n = self.sheet.get(self.sheet_tab).map(|m| m.rows.len()).unwrap_or(0);
                    if self.sheet_page * self.sheet_keys.len().max(1) >= n {
                        self.sheet_page = 0;
                    }
                } else {
                    self.sheet_page = self.sheet_page.saturating_sub(1);
                }
                out.extend(self.relayout());
            }
            Btn::SheetRow(i) | Btn::SheetPlus(i) | Btn::SheetMinus(i) | Btn::SheetUp(i) => {
                let Some(key) = self.sheet_key(i) else { return };
                match (b, key.as_str()) {
                    (_, "act.edit") => self.enter_edit(out),
                    (_, "act.layout") => out.push(Out::LayoutNext),
                    (Btn::SheetUp(_), _) => out.push(Out::SettingStep(key.replacen("quick:", "quick.up:", 1), -1)),
                    (Btn::SheetMinus(_), _) => out.push(Out::SettingStep(key, -1)),
                    _ => out.push(Out::SettingStep(key, 1)),
                }
            }
            Btn::Done => {
                self.leave_edit(out);
                self.arr_backup = None;
                out.push(Out::SaveArrangement);
                out.extend(self.relayout());
            }
            Btn::Reset => {
                self.arr = None;
                out.push(Out::ResetArrangement);
                out.extend(self.relayout());
            }
            Btn::Cancel => {
                self.arr = self.arr_backup.take();
                self.leave_edit(out);
                out.extend(self.relayout());
            }
        }
    }

    pub fn tick(&mut self, t: i64) -> Vec<Out> {
        let mut out = vec![];
        if self.geo.is_none() {
            return out;
        }
        let v = self.keys.tick(t);
        self.key_outs(v, t, &mut out);
        if let Some(p) = self.pad.as_mut() {
            p.tick(t);
        }
        self.pad_frame(&mut out);
        // M3: frozen gesture fingers leave the touchpad after the tap guard
        if self.pad_lift_at.is_some_and(|at| t >= at) {
            self.pad_lift_at = None;
            if let Some(f) = self.pad.as_mut().and_then(|p| p.release_all()) {
                out.push(Out::Pad(f));
                out.push(Out::Note("gesture: frozen touchpad fingers lifted".into()));
            }
        }
        // M3: joystick repeats
        let due: Vec<(ContactId, u16, i64)> = self
            .contacts
            .iter()
            .filter_map(|(id, c)| match c.owner {
                Owner::Joystick { key: Some(k), iv, next } if t >= next => Some((*id, k, iv)),
                _ => None,
            })
            .collect();
        for (id, k, iv) in due {
            self.nav_tap(k, &mut out);
            if let Some(Track { owner: Owner::Joystick { next, .. }, .. }) = self.contacts.get_mut(&id) {
                *next = t + iv;
            }
        }
        // the swipe's travel before recognition, in steps of at most 2.5 mm every 8 ms
        let step = 2.5 * self.cfg.pad_units_per_mm;
        if let Some((d, next)) = self.synth.as_ref().filter(|s| !s.lifted && (s.catch.0 != 0.0 || s.catch.1 != 0.0)).map(|s| (s.catch, s.next)) {
            if t >= next {
                let l = (d.0 * d.0 + d.1 * d.1).sqrt();
                let k = if l > step { step / l } else { 1.0 };
                let mv = (d.0 * k, d.1 * k);
                self.synth_move(None, mv);
                if let Some(s) = self.synth.as_mut() {
                    s.catch = if k < 1.0 { (d.0 - mv.0, d.1 - mv.1) } else { (0.0, 0.0) };
                    s.next = t + crate::touchpad::STEP_MS;
                }
                out.push(Out::Pad(self.synth_frame()));
            }
        }
        let mut long = false;
        for tr in self.contacts.values_mut() {
            if let Owner::Strip { t0, long: l, .. } = &mut tr.owner {
                if !*l && t - *t0 >= self.cfg.longpress_ms {
                    *l = true;
                    long = true;
                }
            }
        }
        if long && !self.edit {
            out.push(Out::Note("long press on the strip: edit mode".into()));
            self.enter_edit(&mut out);
        }
        if let Some((t0, false)) = self.five {
            if t - t0 >= self.cfg.five_hold_ms {
                self.five = Some((t0, true));
                out.push(Out::FiveFinger);
            }
        }
        self.update_dynamic(&mut out);
        out
    }

    pub fn next_deadline(&self) -> Option<i64> {
        let mut d = self.keys.next_deadline();
        let joy = self.contacts.values().filter_map(|c| if let Owner::Joystick { key: Some(_), next, .. } = c.owner { Some(next) } else { None }).min();
        let extra = [
            self.pad.as_ref().and_then(|p| p.next_deadline()),
            self.synth.as_ref().filter(|s| !s.lifted && (s.catch.0 != 0.0 || s.catch.1 != 0.0)).map(|s| s.next),
            self.pad_lift_at,
            joy,
        ];
        for x in extra.into_iter().flatten() {
            d = Some(d.map_or(x, |y| y.min(x)));
        }
        for tr in self.contacts.values() {
            if let Owner::Strip { t0, long: false, .. } = tr.owner {
                let x = t0.saturating_add(self.cfg.longpress_ms);
                d = Some(d.map_or(x, |y| y.min(x)));
            }
        }
        if let Some((t0, false)) = self.five {
            let x = t0 + self.cfg.five_hold_ms;
            d = Some(d.map_or(x, |y| y.min(x)));
        }
        d
    }

    /// lift every key and pad contact (before an ungrab, a device change, a layout change or edit mode);
    /// contacts still down stay tracked but are ignored until they lift
    pub fn release_all(&mut self) -> Vec<Out> {
        let mut out = vec![];
        for k in self.keys.release_all() {
            if let KeyOut::Key(c, p) = k {
                out.push(Out::Key(c, p));
            }
        }
        if let Some(f) = self.pad.as_mut().and_then(|p| p.release_all()) {
            out.push(Out::Pad(f));
        }
        self.end_synth(&mut out, "release");
        self.pad_lift_at = None;
        if self.contacts.values().any(|c| matches!(c.owner, Owner::Laser)) {
            out.push(Out::Laser(None));
        }
        for tr in self.contacts.values_mut() {
            if matches!(tr.owner, Owner::Pad | Owner::Key | Owner::Quick(_) | Owner::Swipe(_) | Owner::Gesture | Owner::Joystick { .. } | Owner::Chip(..) | Owner::Laser) {
                tr.owner = Owner::Ignored;
            }
        }
        self.taps.spoil();
        out
    }

    /// forget all contacts (after an ungrab: the next grab starts clean)
    pub fn reset_contacts(&mut self) -> Vec<Out> {
        let mut out = self.release_all();
        self.contacts.clear();
        self.taps = crate::gestures::Taps::new(self.taps.cfg.clone());
        self.swipe.reset();
        self.synth = None;
        self.five = None;
        self.five_blocked = false;
        self.update_dynamic(&mut out);
        out
    }

    /// GEOM lines for logs and the tablet test runner: native touch coordinates of every button, region, zone,
    /// quick action and key
    pub fn geom_lines(&self) -> Vec<String> {
        let Some(g) = self.geo else { return vec![] };
        let mut v = vec![];
        let mut line = |name: &str, r: &Rect| {
            let (x0, y0, x1, y1) = g.rect_to_touch(r);
            v.push(format!("GEOM name={} native={},{},{},{} logical={:.0},{:.0},{:.0},{:.0}", name, x0, y0, x1, y1, r.x, r.y, r.w, r.h));
        };
        line("strip", &self.model.strip);
        for b in &self.model.buttons {
            line(&b.name, &b.rect);
        }
        for r in &self.model.regions {
            line(&format!("region.{}", r.id), &r.rect);
        }
        if let Some((l, r)) = &self.model.zones {
            line("zone.left", l);
            line("zone.right", r);
        }
        for q in &self.model.quick {
            line(&format!("quick.{}", q.id), &q.rect);
        }
        for b in &self.model.bands {
            line(&format!("band.{}", b.id), &b.rect);
            for (i, (_, r)) in b.chips.iter().enumerate() {
                line(&format!("chip.{}.{}", b.id, i), r);
            }
        }
        if let Some(m) = &self.model.mirror {
            line("mirror", m);
        }
        let mut seen = HashSet::new();
        for k in &self.model.keys {
            let n = match &k.kind {
                KeyKind::Code(c) => format!("key.{}", keycodes::name(*c)),
                KeyKind::Combo(v) => format!("key.{}", v.iter().map(|c| keycodes::name(*c)).collect::<Vec<_>>().join("+")),
                KeyKind::Action(Action::Page(p)) => format!("key.page:{}", p),
                KeyKind::Action(Action::LayoutNext) => "key.layout:next".into(),
                KeyKind::Action(Action::KeyboardHide) => "key.keyboard:hide".into(),
                KeyKind::Action(Action::Fn) => "key.fn".into(),
                KeyKind::Action(Action::SelectMode) => "key.select:toggle".into(),
                KeyKind::Action(Action::Joystick) => "key.joystick".into(),
                KeyKind::Action(Action::EmojiPanel) => "key.emoji:panel".into(),
                KeyKind::Action(Action::Timer(s)) => format!("key.timer:{}", s),
                KeyKind::Action(Action::Laser) => "key.laser:toggle".into(),
                KeyKind::Gap => continue,
            };
            if seen.insert(n.clone()) {
                line(&n, &k.rect);
            }
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Transform;
    use crate::layout::load_all;

    fn geo() -> Geometry {
        Geometry { w: 1696.0, h: 1200.0, transform: Transform::Normal, native_mm: (274.0, 193.0), tmax: (33919, 23999) }
    }

    fn layout(n: &str) -> Layout {
        load_all(&[]).0.into_iter().find(|l| l.name == n).unwrap()
    }

    fn router(n: &str) -> Router {
        let mut r = Router::new(RouterCfg::default(), layout(n));
        let o = r.set_geometry(geo());
        assert!(o.contains(&Out::NeedArrangement(Orientation::Landscape)));
        assert!(o.contains(&Out::PadChanged));
        r
    }

    /// native touch coordinates of the centre of a GEOM name
    fn at(r: &Router, name: &str) -> (i32, i32) {
        let n = boxn(r, name);
        ((n[0] + n[2]) / 2, (n[1] + n[3]) / 2)
    }
    fn boxn(r: &Router, name: &str) -> Vec<i32> {
        let l = r.geom_lines().into_iter().find(|l| l.contains(&format!("name={} ", name))).unwrap_or_else(|| panic!("no {}", name));
        l.split("native=").nth(1).unwrap().split(' ').next().unwrap().split(',').map(|x| x.parse().unwrap()).collect()
    }

    fn down(r: &mut Router, id: u32, p: (i32, i32), t: i64) -> Vec<Out> {
        r.contact(Contact::Down { id, x: p.0, y: p.1, major: 10 }, t)
    }
    fn downm(r: &mut Router, id: u32, p: (i32, i32), major: i32, t: i64) -> Vec<Out> {
        r.contact(Contact::Down { id, x: p.0, y: p.1, major }, t)
    }
    fn mv(r: &mut Router, id: u32, p: (i32, i32), t: i64) -> Vec<Out> {
        r.contact(Contact::Move { id, x: p.0, y: p.1, major: 10 }, t)
    }
    fn dn(r: &mut Router, id: u32, name: &str, t: i64) -> Vec<Out> {
        let p = at(r, name);
        down(r, id, p, t)
    }
    fn mvn(r: &mut Router, id: u32, name: &str, t: i64) -> Vec<Out> {
        let p = at(r, name);
        mv(r, id, p, t)
    }
    fn up(r: &mut Router, id: u32, t: i64) -> Vec<Out> {
        r.contact(Contact::Up { id }, t)
    }
    fn keys(o: &[Out]) -> Vec<(u16, bool)> {
        o.iter().filter_map(|x| if let Out::Key(c, p) = x { Some((*c, *p)) } else { None }).collect()
    }
    fn tap(r: &mut Router, id: u32, name: &str, t: i64) -> Vec<Out> {
        dn(r, id, name, t);
        up(r, id, t + 30)
    }
    /// the replay tool's drag: press, 10 ms steps, release
    fn drag(r: &mut Router, id: u32, a: (i32, i32), b: (i32, i32), ms: i64, t: i64) -> Vec<Out> {
        let mut o = down(r, id, a, t);
        let steps = (ms / 10).max(2);
        for i in 1..=steps {
            let f = i as f64 / steps as f64;
            o.extend(mv(r, id, (a.0 + ((b.0 - a.0) as f64 * f) as i32, a.1 + ((b.1 - a.1) as f64 * f) as i32), t + 30 + i * 10));
        }
        o.extend(up(r, id, t + 80 + steps * 10));
        o
    }
    fn edit_on(r: &mut Router, id: u32, t: i64) {
        dn(r, id, "strip", t);
        r.tick(t + 700);
        up(r, id, t + 800);
        assert!(r.editing());
    }

    #[test]
    fn key_tap_and_hold() {
        let mut r = router("full");
        let a = at(&r, "key.KEY_A");
        down(&mut r, 1, a, 0);
        let o = up(&mut r, 1, 50);
        assert_eq!(keys(&o), vec![(30, true), (30, false)]);
        down(&mut r, 2, a, 1000);
        assert_eq!(r.next_deadline(), Some(1150));
        assert_eq!(keys(&r.tick(1150)), vec![(30, true)]);
        assert!(keys(&r.tick(1700)).is_empty());
        assert_eq!(keys(&up(&mut r, 2, 1700)), vec![(30, false)]);
    }

    #[test]
    fn shift_chord_and_labels() {
        let mut r = router("full");
        let v0 = r.model().version;
        let o = dn(&mut r, 1, "key.KEY_LEFTSHIFT", 0);
        assert_eq!(keys(&o), vec![(42, true)]);
        assert!(r.model().shift && r.model().version > v0, "shifted legends");
        let o = dn(&mut r, 2, "key.KEY_Z", 10);
        assert_eq!(keys(&o), vec![(44, true)]);
        assert_eq!(keys(&up(&mut r, 2, 20)), vec![(44, false)]);
        assert_eq!(keys(&up(&mut r, 1, 30)), vec![(42, false)]);
        assert!(!r.model().shift);
    }

    #[test]
    fn slide_off_cancels() {
        let mut r = router("full");
        dn(&mut r, 1, "key.KEY_C", 0);
        mvn(&mut r, 1, "key.KEY_P", 40);
        assert!(keys(&up(&mut r, 1, 60)).is_empty());
    }

    #[test]
    fn pad_and_click_zones() {
        let mut r = router("full");
        let (xmax, ymax, res) = r.pad_spec().unwrap();
        assert!(xmax > 1000 && ymax > 1000 && res == 124);
        let c = at(&r, "region.touchpad");
        let o = down(&mut r, 1, c, 0);
        let f = o.iter().find_map(|x| if let Out::Pad(f) = x { Some(f.clone()) } else { None }).unwrap();
        let (px, py) = f.slots[0].unwrap();
        assert!((px - xmax / 2).abs() < 30 && (py - ymax / 2).abs() < 30, "centre of the pad -> centre of the device");
        let o = up(&mut r, 1, 30);
        assert!(o.iter().any(|x| matches!(x, Out::Pad(f) if f.slots[0].is_none())));
        let o = dn(&mut r, 2, "zone.right", 100);
        assert!(o.iter().any(|x| matches!(x, Out::Pad(f) if f.buttons == vec![(crate::touchpad::BTN_RIGHT, true)])));
        let o = up(&mut r, 2, 150);
        assert!(o.iter().any(|x| matches!(x, Out::Pad(f) if f.buttons == vec![(crate::touchpad::BTN_RIGHT, false)])));
    }

    #[test]
    fn contact_stays_with_its_start_region() {
        let mut r = router("full");
        dn(&mut r, 1, "region.touchpad", 0);
        let o = mvn(&mut r, 1, "key.KEY_A", 20);
        assert!(keys(&o).is_empty());
        assert!(o.iter().any(|x| matches!(x, Out::Pad(_))));
        let o = up(&mut r, 1, 40);
        assert!(keys(&o).is_empty());
    }

    #[test]
    fn strip_buttons_fire_on_release_inside() {
        let mut r = router("full");
        dn(&mut r, 1, "strip.display", 0);
        let o = up(&mut r, 1, 50);
        assert!(o.contains(&Out::UseAsDisplay));
        dn(&mut r, 2, "strip.exit", 0);
        mvn(&mut r, 2, "region.touchpad", 20);
        let o = up(&mut r, 2, 40);
        assert!(!o.contains(&Out::Exit), "released outside the button");
        dn(&mut r, 3, "strip.layout", 0);
        assert!(up(&mut r, 3, 50).contains(&Out::LayoutNext));
    }

    #[test]
    fn layout_switch_and_pages() {
        let mut r = router("full");
        let o = r.set_layout(layout("tablet"), None);
        assert!(o.contains(&Out::Static));
        assert!(r.geom_lines().iter().any(|l| l.contains("key.page:sym")));
        let p = at(&r, "key.page:sym");
        down(&mut r, 1, p, 0);
        up(&mut r, 1, 30);
        assert!(r.geom_lines().iter().any(|l| l.contains("key.KEY_1 ")), "symbols page shown");
        assert!(r.geom_lines().iter().any(|l| l.contains("key.KEY_LEFTSHIFT+KEY_2 ")), "combo keys on the symbols page");
        r.set_layout(layout("split"), None);
        let names: Vec<String> = r.model().regions.iter().map(|x| x.id.clone()).collect();
        assert_eq!(names, vec!["touchpad", "keyboard_left", "keyboard_right"]);
        let o = r.set_layout(Layout::none(), None);
        assert!(o.contains(&Out::PadChanged), "touchpad grows to the whole area");
        assert_eq!(r.model().keys.len(), 0);
    }

    #[test]
    fn edit_mode_move_resize_done_cancel_reset() {
        let mut r = router("full");
        let before = r.arrangement().get(TOUCHPAD).unwrap();
        dn(&mut r, 1, "strip", 0);
        assert_eq!(r.next_deadline(), Some(700));
        let o = r.tick(700);
        assert!(o.contains(&Out::EditMode(true)) && r.editing());
        let o = up(&mut r, 1, 800);
        assert!(!o.iter().any(|x| matches!(x, Out::UseAsDisplay | Out::Exit | Out::LayoutNext)), "no button after a long press");
        let c = at(&r, "region.touchpad");
        let g = geo();
        down(&mut r, 2, c, 1000);
        let (lx, ly) = g.touch_to_logical(c.0, c.1);
        let to = g.logical_to_touch(lx - 100.0, ly);
        let o = mv(&mut r, 2, to, 1100);
        assert!(o.contains(&Out::Static));
        assert!(!o.contains(&Out::PadChanged), "no device recreation while dragging");
        up(&mut r, 2, 1200);
        let moved = r.arrangement().get(TOUCHPAD).unwrap();
        assert!((moved.x - (before.x - 100.0 / 1696.0)).abs() < 0.002, "{:?} {:?}", before, moved);
        dn(&mut r, 3, "strip.done", 1300);
        let o = up(&mut r, 3, 1350);
        assert!(o.contains(&Out::SaveArrangement) && !r.editing());
        assert!(!o.contains(&Out::PadChanged), "a move keeps the size: same device");
        edit_on(&mut r, 4, 2000);
        let pr = r.model().regions.iter().find(|x| x.id == TOUCHPAD).unwrap().rect;
        let corner = g.logical_to_touch(pr.right() - 5.0, pr.bottom() - 5.0);
        down(&mut r, 5, corner, 3000);
        mv(&mut r, 5, g.logical_to_touch(pr.right() - 150.0, pr.bottom() - 100.0), 3100);
        up(&mut r, 5, 3200);
        let small = r.arrangement().get(TOUCHPAD).unwrap();
        assert!(small.w < moved.w && small.h < moved.h);
        dn(&mut r, 6, "strip.cancel", 3300);
        up(&mut r, 6, 3350);
        assert_eq!(r.arrangement().get(TOUCHPAD).unwrap(), moved);
        edit_on(&mut r, 7, 4000);
        dn(&mut r, 8, "strip.reset", 5000);
        let o = up(&mut r, 8, 5050);
        assert!(o.contains(&Out::ResetArrangement));
        assert_eq!(r.arrangement().get(TOUCHPAD).unwrap(), before);
    }

    /// M1 T7 failure reproduced: the runner moved the touchpad 2500 native units left, then dragged its
    /// bottom-right corner (250 units inside, at the panel's bottom edge) by (-2250, -1250). M1 recreated the
    /// virtual touchpad on every drag step, so the daemon lagged and the runner read a stale corner; here the
    /// sequence runs as the runner does it, reading the corner from the GEOM lines after the move.
    #[test]
    fn runner_t7_drag_sequence_resizes() {
        let mut r = router("full");
        let b0 = boxn(&r, "region.touchpad");
        edit_on(&mut r, 1, 0);
        let (cx, cy) = ((b0[0] + b0[2]) / 2, (b0[1] + b0[3]) / 2);
        let o = drag(&mut r, 2, (cx, cy), (cx - 2500, cy), 500, 1000);
        assert!(!o.contains(&Out::PadChanged));
        let b1 = boxn(&r, "region.touchpad");
        assert!(b1[0] < b0[0]);
        let (a1, bb1) = (b1[2], b1[3]);
        assert_eq!(bb1, 24000, "the touchpad sits on the panel's bottom edge");
        let o = drag(&mut r, 3, (a1 - 250, bb1 - 250), (a1 - 2500, bb1 - 1500), 500, 3000);
        assert!(!o.contains(&Out::PadChanged));
        let b2 = boxn(&r, "region.touchpad");
        assert!(b2[2] - b2[0] < b1[2] - b1[0] && b2[3] - b2[1] < b1[3] - b1[1], "resized smaller: {:?} -> {:?}", b1, b2);
        assert_eq!((b2[0], b2[1]), (b1[0], b1[1]), "the top-left corner stays");
        // Done: now the virtual touchpad is recreated once with the new size
        let o = tap(&mut r, 4, "strip.done", 5000);
        assert!(o.contains(&Out::SaveArrangement) && o.contains(&Out::PadChanged));
        let (xm, ym, _) = r.pad_spec().unwrap();
        assert!((xm as f32 - (b2[2] - b2[0]) as f32).abs() < 200.0 && (ym as f32 - (b2[3] - b2[1]) as f32).abs() < 200.0, "{} {} {:?}", xm, ym, b2);
    }

    #[test]
    fn edge_handles_and_guides() {
        let mut r = router("full");
        edit_on(&mut r, 1, 0);
        let g = geo();
        let pr = r.model().regions.iter().find(|x| x.id == TOUCHPAD).unwrap().rect;
        // left edge, middle height: only the left side moves
        let p = g.logical_to_touch(pr.x + 3.0, pr.y + pr.h / 2.0);
        down(&mut r, 2, p, 1000);
        mv(&mut r, 2, g.logical_to_touch(pr.x + 80.0, pr.y + pr.h / 2.0 + 40.0), 1100);
        let nr = r.model().regions.iter().find(|x| x.id == TOUCHPAD).unwrap().rect;
        assert!((nr.x - (pr.x + 77.0)).abs() < 1.0 && (nr.right() - pr.right()).abs() < 0.5 && (nr.y - pr.y).abs() < 0.01 && (nr.h - pr.h).abs() < 0.01, "{:?} {:?}", pr, nr);
        // dragging the top edge up to the keyboard snaps and shows a guide while the finger is down
        up(&mut r, 2, 1200);
        let kb = r.model().regions.iter().find(|x| x.id == "keyboard").unwrap().rect;
        let nr = r.model().regions.iter().find(|x| x.id == TOUCHPAD).unwrap().rect;
        let p = g.logical_to_touch(nr.x + nr.w / 2.0, nr.y + 2.0);
        down(&mut r, 3, p, 2000);
        mv(&mut r, 3, g.logical_to_touch(nr.x + nr.w / 2.0, kb.bottom() + 6.0), 2100);
        let m = r.model();
        let tp = m.regions.iter().find(|x| x.id == TOUCHPAD).unwrap().rect;
        assert!((tp.y - kb.bottom()).abs() < 0.5, "snapped onto the keyboard edge: {} vs {}", tp.y, kb.bottom());
        assert!(m.guides.iter().any(|gd| !gd.vertical && (gd.pos - kb.bottom()).abs() < 0.5));
        up(&mut r, 3, 2200);
        assert!(r.model().guides.is_empty(), "guides vanish on release");
    }

    #[test]
    fn edit_never_overlaps_strip_or_regions() {
        let mut r = router("full");
        edit_on(&mut r, 1, 0);
        let c = at(&r, "region.touchpad");
        down(&mut r, 2, c, 1000);
        mv(&mut r, 2, (c.0, 0), 1100);
        up(&mut r, 2, 1200);
        let m = r.model();
        let tp = m.regions.iter().find(|x| x.id == TOUCHPAD).unwrap().rect;
        let kb = m.regions.iter().find(|x| x.id == "keyboard").unwrap().rect;
        assert!(!tp.overlaps(&kb) && tp.y >= m.strip.bottom() - 0.01, "{:?} {:?}", tp, kb);
    }

    #[test]
    fn five_finger_hold_releases() {
        let mut r = router("full");
        let c = at(&r, "region.touchpad");
        for i in 0..5 {
            down(&mut r, i + 1, (c.0 + i as i32 * 1500, c.1), 0);
        }
        assert_eq!(r.next_deadline().map(|d| d <= 800), Some(true));
        let o = r.tick(800);
        assert!(o.contains(&Out::FiveFinger));
        assert!(!r.tick(2000).contains(&Out::FiveFinger), "fires once");
        let o = r.release_all();
        assert!(o.iter().any(|x| matches!(x, Out::Pad(f) if f.slots.iter().all(|s| s.is_none()))));
        let mut r = router("full");
        for i in 0..5 {
            down(&mut r, i + 1, (c.0 + i as i32 * 1500, c.1), 0);
        }
        mv(&mut r, 1, (c.0 + 2000, c.1 + 2000), 100);
        assert!(!r.tick(900).contains(&Out::FiveFinger));
    }

    fn sheet_data() -> Vec<SheetModule> {
        let cfg = toml::Value::try_from(crate::config::Config::default()).unwrap();
        crate::settings::MODULES
            .iter()
            .map(|m| SheetModule { id: m.id.into(), title: m.title.into(), live: m.implemented, plan: m.plan.into(), rows: crate::settings::rows(m.id, &cfg, &HashSet::new()) })
            .collect()
    }

    #[test]
    fn settings_sheet_tabs_rows_steps() {
        let mut r = router("full");
        r.set_sheet(sheet_data());
        let o = tap(&mut r, 1, "strip.settings", 0);
        assert!(o.contains(&Out::PanelOpen(true)));
        assert!(r.model().sheet.is_some());
        // general tab: autostart row steps
        let o = tap(&mut r, 2, "sheet.row.autostart", 100);
        assert!(o.contains(&Out::SettingStep("autostart".into(), 1)));
        // pointer tab: natural scroll toggles, speed has minus / plus
        tap(&mut r, 3, "sheet.tab.pointer", 200);
        assert_eq!(r.model().sheet.as_ref().unwrap().title, "Touchpad");
        assert!(tap(&mut r, 4, "sheet.row.natural_scroll", 300).contains(&Out::SettingStep("natural_scroll".into(), 1)));
        assert!(tap(&mut r, 5, "sheet.minus.speed", 400).contains(&Out::SettingStep("speed".into(), -1)));
        assert!(tap(&mut r, 6, "sheet.plus.speed", 500).contains(&Out::SettingStep("speed".into(), 1)));
        // M3: the gestures module is live (no "planned" note), its rows are paged
        tap(&mut r, 7, "sheet.tab.gestures", 600);
        assert!(r.model().sheet.as_ref().unwrap().subtitle.is_empty());
        assert!(r.model().buttons.iter().any(|b| b.name == "sheet.row.features.gestures.options.swipe3_left"));
        // touches on the pad under the sheet do nothing
        let o = dn(&mut r, 8, "region.touchpad", 700);
        assert!(!o.iter().any(|x| matches!(x, Out::Pad(_))));
        up(&mut r, 8, 730);
        // "Move / resize areas" from the sheet enters edit mode
        tap(&mut r, 9, "sheet.tab.general", 800);
        tap(&mut r, 10, "sheet.row.act.edit", 900);
        assert!(r.editing() && !r.sheet_open());
        tap(&mut r, 11, "strip.cancel", 1000);
        tap(&mut r, 12, "strip.settings", 1100);
        assert!(tap(&mut r, 13, "panel.close", 1200).contains(&Out::PanelOpen(false)));
    }

    #[test]
    fn quick_actions_region_and_taps() {
        let mut cfg = RouterCfg::default();
        cfg.quick = true;
        let mut r = Router::new(cfg, layout("full"));
        r.set_quick(vec![("copy".into(), "Copy".into(), 1), ("paste".into(), "Paste".into(), 2), ("vol_up".into(), "Vol +".into(), 1)]);
        r.set_geometry(geo());
        assert!(r.model().regions.iter().any(|x| x.id == QUICK));
        let p = boxn(&r, "quick.paste");
        let c = boxn(&r, "quick.copy");
        assert!((p[3] - p[1]) > (c[3] - c[1]) || (p[2] - p[0]) > (c[2] - c[0]), "size 2 is bigger");
        let o = tap(&mut r, 1, "quick.copy", 0);
        assert!(o.contains(&Out::Quick("copy".into())));
        // slide off: nothing
        dn(&mut r, 2, "quick.vol_up", 100);
        mvn(&mut r, 2, "region.touchpad", 120);
        assert!(!up(&mut r, 2, 140).iter().any(|x| matches!(x, Out::Quick(_))));
        // the quick bar is a region of edit mode too
        edit_on(&mut r, 3, 1000);
        assert!(r.model().regions.iter().any(|x| x.id == QUICK));
    }

    #[test]
    fn palm_rest_zone_dwt_and_pen() {
        let mut cfg = RouterCfg::default();
        cfg.palm_major = 60;
        cfg.rest_zone_mm = 8.0;
        cfg.dwt_ms = 300;
        let mut r = Router::new(cfg, layout("full"));
        r.set_geometry(geo());
        // a large contact on the pad and on a key: ignored
        let c = at(&r, "region.touchpad");
        let o = downm(&mut r, 1, c, 120, 0);
        assert!(!o.iter().any(|x| matches!(x, Out::Pad(_))) && o.iter().any(|x| matches!(x, Out::Note(n) if n.starts_with("palm"))));
        up(&mut r, 1, 20);
        let a = at(&r, "key.KEY_A");
        downm(&mut r, 2, a, 120, 100);
        assert!(keys(&up(&mut r, 2, 130)).is_empty(), "palm on the keys");
        // a normal finger works
        down(&mut r, 3, a, 200);
        assert_eq!(keys(&up(&mut r, 3, 230)).len(), 2);
        // pad touch 100 ms after a key: held back (dwt); 400 ms after: fine
        let o = down(&mut r, 4, c, 330);
        assert!(!o.iter().any(|x| matches!(x, Out::Pad(_))));
        up(&mut r, 4, 340);
        let o = down(&mut r, 5, c, 700);
        assert!(o.iter().any(|x| matches!(x, Out::Pad(_))));
        up(&mut r, 5, 720);
        // rest zone at the bottom edge of the panel (the click zones end there)
        let g = geo();
        let o = down(&mut r, 6, g.logical_to_touch(c.0 as f32 * 0.05, 1198.0), 2000);
        assert!(o.iter().any(|x| matches!(x, Out::Note(n) if n.starts_with("rest zone"))));
        up(&mut r, 6, 2010);
        // pen near: everything paused
        r.set_pen(true);
        let o = down(&mut r, 7, c, 3000);
        assert!(!o.iter().any(|x| matches!(x, Out::Pad(_))));
        up(&mut r, 7, 3010);
        r.set_pen(false);
        assert!(down(&mut r, 8, c, 4000).iter().any(|x| matches!(x, Out::Pad(_))));
        up(&mut r, 8, 4010);
        // M2.1 fix: a resting modifier does not start dwt (Ctrl held + a pad finger 80 ms later moves the pointer)
        let ctrl = at(&r, "key.KEY_LEFTCTRL");
        let o = down(&mut r, 9, ctrl, 5000);
        assert!(keys(&o).contains(&(29, true)), "Ctrl pressed at once");
        let o = down(&mut r, 10, c, 5080);
        assert!(o.iter().any(|x| matches!(x, Out::Pad(_))), "pad finger with Ctrl held: {:?}", o);
        up(&mut r, 10, 5200);
        up(&mut r, 9, 5250);
    }

    #[test]
    fn sticky_fn_and_long_press_through_the_router() {
        let mut cfg = RouterCfg::default();
        cfg.sticky = true;
        let mut r = Router::new(cfg, layout("full"));
        r.set_geometry(geo());
        tap(&mut r, 1, "key.KEY_LEFTSHIFT", 0);
        assert_eq!(r.model().latched, vec![(42, false)], "latched Shift drawn");
        let mut o = dn(&mut r, 2, "key.KEY_A", 100);
        o.extend(up(&mut r, 2, 130));
        assert_eq!(keys(&o), vec![(30, true), (30, false), (42, false)]);
        // Fn + 1 = F1
        dn(&mut r, 3, "key.fn", 200);
        assert!(r.model().fn_on);
        let o = tap(&mut r, 4, "key.KEY_1", 210);
        assert_eq!(keys(&o), vec![(59, true), (59, false)]);
        up(&mut r, 3, 300);
        // tablet: long press Q = 1
        r.set_layout(layout("tablet"), None);
        dn(&mut r, 5, "key.KEY_Q", 1000);
        let o = r.tick(1450);
        assert_eq!(keys(&o), vec![(2, true), (2, false)]);
        assert!(keys(&up(&mut r, 5, 1500)).is_empty());
    }

    // ------------------------------------------------------------------ M2.1

    fn pads(o: &[Out]) -> Vec<PadFrame> {
        o.iter().filter_map(|x| if let Out::Pad(f) = x { Some(f.clone()) } else { None }).collect()
    }
    fn swipes(o: &[Out]) -> Vec<String> {
        o.iter().filter_map(|x| if let Out::Swipe(s) = x { Some(s.clone()) } else { None }).collect()
    }
    /// fingers at native points, moved together by (dx, dy) native units in 10 ms steps, then lifted
    fn multi_swipe(r: &mut Router, pts: &[(i32, i32)], d: (i32, i32), steps: i32, t: i64) -> Vec<Out> {
        let mut o = vec![];
        for (i, p) in pts.iter().enumerate() {
            o.extend(down(r, 100 + i as u32, *p, t + i as i64 * 5));
        }
        let mut tt = t + 20;
        for s in 1..=steps {
            tt = t + 20 + s as i64 * 10;
            for (i, p) in pts.iter().enumerate() {
                o.extend(mv(r, 100 + i as u32, (p.0 + d.0 * s / steps, p.1 + d.1 * s / steps), tt));
            }
            o.extend(r.tick(tt));
        }
        for i in 0..pts.len() {
            o.extend(up(r, 100 + i as u32, tt + 10 + i as i64));
        }
        o
    }

    #[test]
    fn three_finger_swipe_over_the_keyboard_becomes_a_touchpad_gesture() {
        let mut r = router("full");
        let pts = [at(&r, "key.KEY_Q"), at(&r, "key.KEY_W"), at(&r, "key.KEY_E")];
        // 20 mm up (124 native units / mm)
        let o = multi_swipe(&mut r, &pts, (0, -2480), 12, 0);
        assert!(keys(&o).iter().all(|k| !k.1), "no key typed: {:?}", keys(&o));
        assert!(keys(&o).is_empty(), "no key events at all: {:?}", keys(&o));
        let sw = swipes(&o);
        assert!(sw.iter().any(|s| s.starts_with("start fingers=3 dir=0.00,-1.00")), "{:?}", sw);
        let f = pads(&o);
        let three: Vec<&PadFrame> = f.iter().filter(|f| f.slots.iter().filter(|s| s.is_some()).count() == 3).collect();
        assert!(three.len() >= 3, "synthetic fingers down and moving ({} frames)", three.len());
        let y0 = three.first().unwrap().slots[0].unwrap().1;
        let y1 = three.last().unwrap().slots[0].unwrap().1;
        assert!(y1 < y0, "they move up: {} -> {}", y0, y1);
        assert!(f.last().unwrap().slots.iter().all(|s| s.is_none()), "lifted at the end");
        assert_eq!(r.swipe_phase(), Phase::Idle);
        assert!(r.held_codes().is_empty());
    }

    #[test]
    fn swipe_cancels_held_super_with_the_mask_and_stops_pad_input() {
        let mut r = router("full");
        // Super held by the first finger, two more fingers on the touchpad (keys would form a Super chord at
        // once, which stays delivered): all three swipe right together
        let c = at(&r, "region.touchpad");
        let pts = [at(&r, "key.KEY_LEFTMETA"), (c.0 - 1500, c.1), (c.0 + 1500, c.1)];
        let o = multi_swipe(&mut r, &pts, (2480, 0), 12, 0);
        let k = keys(&o);
        assert_eq!(k.first(), Some(&(125, true)), "Super pressed at once (modifier)");
        let mi = k.iter().position(|x| *x == (240, true)).expect("mask pressed");
        let ri = k.iter().position(|x| *x == (125, false)).expect("Super released");
        assert!(mi < ri && k[mi + 1] == (240, false), "mask tap before the Super release: {:?}", k);
        assert_eq!(k.len(), 4, "Super, mask, Super up only: {:?}", k);
        assert!(swipes(&o).iter().any(|s| s.starts_with("start fingers=3")));
        // a swipe with a pad finger in it: the pad's two-finger input stops (fingers lifted) when it wins
        let mut r = router("full");
        let c = at(&r, "region.touchpad");
        let pts = [(c.0 - 1500, c.1), (c.0 + 1500, c.1), at(&r, "key.KEY_B")];
        let o = multi_swipe(&mut r, &pts, (3000, 0), 12, 0);
        let sw = swipes(&o);
        assert!(sw.iter().any(|s| s.starts_with("start fingers=3")), "{:?}", sw);
        let f = pads(&o);
        let si = f.iter().position(|f| f.slots.iter().filter(|s| s.is_some()).count() == 3).unwrap();
        assert!(f[si - 1].slots.iter().all(|s| s.is_none()), "the real pad fingers lifted before the synthetic ones");
    }

    #[test]
    fn resting_chords_and_one_moving_pad_finger_stay_normal() {
        let mut r = router("full");
        // Ctrl + Alt resting, T tapped: a chord, no swipe
        dn(&mut r, 1, "key.KEY_LEFTCTRL", 0);
        dn(&mut r, 2, "key.KEY_LEFTALT", 10);
        let o = dn(&mut r, 3, "key.KEY_T", 50);
        assert_eq!(keys(&o), vec![(20, true)], "pressed at once inside the chord");
        // Ctrl resting + two pad fingers scrolling: the modifier is stationary -> not a swipe
        let c = at(&r, "region.touchpad");
        down(&mut r, 4, c, 100);
        down(&mut r, 5, (c.0 + 1500, c.1), 100);
        let mut o = vec![];
        for s in 1..=10 {
            o.extend(mv(&mut r, 4, (c.0, c.1 - 200 * s), 100 + s as i64 * 10));
            o.extend(mv(&mut r, 5, (c.0 + 1500, c.1 - 200 * s), 100 + s as i64 * 10));
        }
        assert!(swipes(&o).is_empty() && r.swipe_phase() != Phase::Active);
        assert!(pads(&o).len() >= 10, "the pad keeps scrolling");
        up(&mut r, 4, 300);
        up(&mut r, 5, 300);
        up(&mut r, 3, 300);
        let o = up(&mut r, 2, 310);
        assert_eq!(keys(&o), vec![(56, false)], "a used modifier: no mask");
        // still resting Ctrl + three moving pad fingers: four contacts, one stationary -> no swipe either
        let o = multi_swipe(&mut r, &[(c.0 - 1500, c.1), c, (c.0 + 1500, c.1)], (0, -2480), 12, 400);
        assert!(swipes(&o).is_empty());
        up(&mut r, 1, 900);
        // three fingers all on the touchpad: libinput's own gesture, DexHid does not take over
        let o = multi_swipe(&mut r, &[(c.0 - 1500, c.1), c, (c.0 + 1500, c.1)], (0, -2480), 12, 1000);
        assert!(swipes(&o).iter().any(|s| s.starts_with("native")));
    }

    #[test]
    fn swipe_disabled_keeps_keys() {
        let mut cfg = RouterCfg::default();
        cfg.swipe = false;
        let mut r = Router::new(cfg, layout("full"));
        r.set_geometry(geo());
        let pts = [at(&r, "key.KEY_Q"), at(&r, "key.KEY_W"), at(&r, "key.KEY_E")];
        let o = multi_swipe(&mut r, &pts, (0, -2480), 12, 0);
        assert!(swipes(&o).is_empty());
    }

    #[test]
    fn slide_off_needs_the_slop_beyond_the_key_edge() {
        let mut r = router("full");
        let b = boxn(&r, "key.KEY_G");
        let cy = (b[1] + b[3]) / 2;
        down(&mut r, 1, (b[2] - 50, cy), 0);
        // 1 mm past the right edge: still on the key (3 mm slop)
        mv(&mut r, 1, (b[2] + 124, cy), 20);
        assert_eq!(keys(&up(&mut r, 1, 40)), vec![(34, true), (34, false)], "small wobble over the edge still types");
        down(&mut r, 2, (b[2] - 50, cy), 100);
        mv(&mut r, 2, (b[2] + 124 * 6, cy), 120);
        assert!(keys(&up(&mut r, 2, 140)).is_empty(), "6 mm off: cancelled");
    }

    #[test]
    fn click_zone_thumb_drag_and_middle_chord_through_the_router() {
        let mut cfg = RouterCfg::default();
        cfg.chord_middle = true;
        let mut r = Router::new(cfg, layout("full"));
        r.set_geometry(geo());
        let l = at(&r, "zone.left");
        let rr = at(&r, "zone.right");
        let mut o = down(&mut r, 1, l, 0);
        o.extend(down(&mut r, 2, rr, 20));
        let b: Vec<(u16, bool)> = pads(&o).into_iter().flat_map(|f| f.buttons).collect();
        assert_eq!(b, vec![(crate::touchpad::BTN_MIDDLE, true)]);
        up(&mut r, 1, 100);
        let b: Vec<(u16, bool)> = pads(&up(&mut r, 2, 110)).into_iter().flat_map(|f| f.buttons).collect();
        assert_eq!(b, vec![(crate::touchpad::BTN_MIDDLE, false)]);
        // thumb on the left zone + a finger moving on the pad = drag (button first, then motion, button held)
        let c = at(&r, "region.touchpad");
        down(&mut r, 3, l, 1000);
        let o = r.tick(1050);
        assert!(pads(&o).iter().any(|f| f.buttons == vec![(crate::touchpad::BTN_LEFT, true)]));
        down(&mut r, 4, c, 1100);
        let o = mv(&mut r, 4, (c.0 + 2000, c.1), 1120);
        assert!(pads(&o).iter().any(|f| f.slots[0].is_some()));
        up(&mut r, 4, 1200);
        let b: Vec<(u16, bool)> = pads(&up(&mut r, 3, 1300)).into_iter().flat_map(|f| f.buttons).collect();
        assert_eq!(b, vec![(crate::touchpad::BTN_LEFT, false)]);
    }

    #[test]
    fn scroll_inertia_after_lift_and_cancel_on_touch() {
        let mut cfg = RouterCfg::default();
        cfg.inertia.enabled = true;
        let mut r = Router::new(cfg, layout("full"));
        r.set_geometry(geo());
        let c = at(&r, "region.touchpad");
        let (a, b) = ((c.0 - 1000, c.1 + 2000), (c.0 + 1000, c.1 + 2000));
        down(&mut r, 1, a, 0);
        down(&mut r, 2, b, 0);
        for s in 1..=8 {
            // 200 native units (1.6 mm) per 8 ms = 200 mm/s upwards
            mv(&mut r, 1, (a.0, a.1 - 200 * s), s as i64 * 8);
            mv(&mut r, 2, (b.0, b.1 - 200 * s), s as i64 * 8);
        }
        up(&mut r, 1, 66);
        let o = up(&mut r, 2, 68);
        assert!(o.iter().any(|x| matches!(x, Out::Note(n) if n.contains("inertia"))));
        let mut frames = vec![];
        let mut t = 68;
        for _ in 0..12 {
            let Some(d) = r.next_deadline() else { break };
            t = d;
            frames.extend(pads(&r.tick(t)));
        }
        let two = frames.iter().filter(|f| f.slots.iter().filter(|s| s.is_some()).count() == 2).count();
        assert!(two >= 10, "virtual fingers keep scrolling after the lift ({} frames)", two);
        // a touch on a key stops it at once
        let o = dn(&mut r, 3, "key.KEY_K", t + 1);
        let f = pads(&o);
        assert!(f.last().unwrap().slots.iter().all(|s| s.is_none()), "coast lifted");
        assert!(o.iter().any(|x| matches!(x, Out::Note(n) if n.contains("inertia stopped"))));
    }

    #[test]
    fn laptop_preset_geometry() {
        let mut cfg = RouterCfg::default();
        cfg.laptop = true;
        let mut r = Router::new(cfg, layout("laptop"));
        r.set_geometry(geo());
        let m = r.model().clone();
        assert!(m.laptop);
        let ppm = m.px_per_mm;
        let kb = m.regions.iter().find(|x| x.id == "keyboard").unwrap().rect;
        let tp = m.regions.iter().find(|x| x.id == TOUCHPAD).unwrap().rect;
        let pitch = kb.w / 15.0;
        assert!(pitch <= 19.05 * ppm + 0.01, "pitch {} mm", pitch / ppm);
        assert!(((kb.x + kb.w / 2.0) - m.w / 2.0).abs() < 1.0, "centred");
        assert!((tp.w - kb.w).abs() < 1.0 && (tp.x - kb.x).abs() < 1.0, "pad width follows the keyboard");
        assert!(tp.y > kb.bottom(), "pad below the keyboard");
        // keys are square on the grid: a letter key is pitch wide and tall (minus the gap)
        let q = m.keys.iter().find(|k| k.kind == KeyKind::Code(16)).unwrap().rect;
        assert!((q.w - q.h).abs() < 1.0, "square keys {:?}", q);
        // a very wide screen: not stretched (19.05 mm pitch), centred
        let wide = Geometry { w: 3392.0, native_mm: (548.0, 193.0), ..geo() };
        r.set_geometry(wide);
        let m = r.model().clone();
        let kb = m.regions.iter().find(|x| x.id == "keyboard").unwrap().rect;
        assert!((kb.w / 15.0 - 19.05 * m.px_per_mm).abs() < 0.5, "max laptop pitch on a wide screen");
        assert!(((kb.x + kb.w / 2.0) - m.w / 2.0).abs() < 1.0);
        // edit mode from the preset starts from these rects and switches to free placement
        let o = {
            dn(&mut r, 1, "strip", 0);
            r.tick(700)
        };
        assert!(o.contains(&Out::SetSetting("arrangement".into(), "free".into())));
        assert!(r.editing());
    }

    #[test]
    fn altgr_state_for_the_legends() {
        let mut r = router("full");
        dn(&mut r, 1, "key.KEY_RIGHTALT", 0);
        assert!(r.model().altgr);
        up(&mut r, 1, 50);
        assert!(!r.model().altgr);
    }

    #[test]
    fn rotation_reloads_arrangement() {
        let mut r = router("full");
        let p = Geometry { w: 1200.0, h: 1696.0, transform: Transform::R90, ..geo() };
        let o = r.set_geometry(p);
        assert!(o.contains(&Out::NeedArrangement(Orientation::Portrait)));
        let m = r.model();
        let tp = m.regions.iter().find(|x| x.id == TOUCHPAD).unwrap().rect;
        assert!(tp.y < 400.0, "portrait default puts the pad on top: {:?}", tp);
        let c = at(&r, "region.touchpad");
        let o = down(&mut r, 1, c, 0);
        assert!(o.iter().any(|x| matches!(x, Out::Pad(_))));
    }

    // ------------------------------------------------------------------------------------------------ M3

    fn gestures_of(o: &[Out]) -> Vec<String> {
        o.iter().filter_map(|x| if let Out::Gesture(s) = x { Some(s.clone()) } else { None }).collect()
    }

    fn router_cfg(n: &str, f: impl Fn(&mut RouterCfg)) -> Router {
        let mut c = RouterCfg::default();
        f(&mut c);
        let mut r = Router::new(c, layout(n));
        r.set_geometry(geo());
        r
    }

    #[test]
    fn m3_gesture_swipe_over_keys_and_on_the_pad() {
        let mut r = router_cfg("full", |c| c.gestures = true);
        let pts = [at(&r, "key.KEY_Q"), at(&r, "key.KEY_W"), at(&r, "key.KEY_E")];
        let o = multi_swipe(&mut r, &pts, (-2480, 0), 12, 0);
        assert_eq!(gestures_of(&o), vec!["swipe3_left"]);
        assert!(keys(&o).is_empty(), "no key events: {:?}", keys(&o));
        assert!(pads(&o).iter().all(|f| f.slots.iter().all(|s| s.is_none())), "no synthetic fingers in gesture mode");
        // all three on the touchpad: the third finger freezes the first two; they lift after the tap guard
        let mut r = router_cfg("full", |c| c.gestures = true);
        let b = boxn(&r, "region.touchpad");
        let y = b[1] + (b[3] - b[1]) / 3;
        let pts = [(b[0] + 2000, y), (b[0] + 4000, y), (b[0] + 6000, y)];
        let mut o = vec![];
        for (i, p) in pts.iter().enumerate() {
            o.extend(down(&mut r, 100 + i as u32, *p, i as i64 * 5));
        }
        assert!(o.iter().any(|x| matches!(x, Out::Note(n) if n.contains("captured"))), "{:?}", o);
        let before = pads(&o).len();
        for s in 1..=12 {
            for (i, p) in pts.iter().enumerate() {
                o.extend(mv(&mut r, 100 + i as u32, (p.0, p.1 - 2480 * s / 12), 20 + s as i64 * 10));
            }
            o.extend(r.tick(20 + s as i64 * 10));
        }
        assert_eq!(gestures_of(&o), vec!["swipe3_up"]);
        assert_eq!(r.next_deadline(), Some(TAP_GUARD_MS), "the frozen fingers wait for the tap guard");
        o.extend(r.tick(TAP_GUARD_MS));
        let f = pads(&o);
        assert!(f[before..].iter().all(|x| x.slots.iter().filter(|s| s.is_some()).count() <= 3), "no new fingers");
        assert!(f.last().unwrap().slots.iter().all(|s| s.is_none()), "frozen fingers lifted");
        // the lift came at or after the tap guard
        let lift_note = o.iter().position(|x| matches!(x, Out::Note(n) if n.contains("frozen touchpad fingers lifted")));
        assert!(lift_note.is_some() || f.last().unwrap().slots.iter().all(|s| s.is_none()));
        for i in 0..3 {
            up(&mut r, 100 + i, 300);
        }
        assert_eq!(r.contacts_down(), 0);
    }

    #[test]
    fn m3_gesture_tap_and_no_two_finger_tap_on_the_pad() {
        let mut r = router_cfg("full", |c| c.gestures = true);
        // 3 fingers on keys, quick tap: tap3, nothing typed
        let pts = [at(&r, "key.KEY_A"), at(&r, "key.KEY_S"), at(&r, "key.KEY_D")];
        let mut o = vec![];
        for (i, p) in pts.iter().enumerate() {
            o.extend(down(&mut r, 10 + i as u32, *p, i as i64 * 20));
        }
        for i in 0..3 {
            o.extend(up(&mut r, 10 + i, 150 + i as i64));
        }
        assert_eq!(gestures_of(&o), vec!["tap3"]);
        assert!(keys(&o).is_empty(), "{:?}", keys(&o));
        // 3 fingers on the pad, quick tap: the two frozen fingers stay down until the guard (no 2-finger tap)
        let b = boxn(&r, "region.touchpad");
        let y = b[1] + (b[3] - b[1]) / 3;
        let mut o = vec![];
        for i in 0..3 {
            o.extend(down(&mut r, 20 + i, (b[0] + 2000 + i as i32 * 2000, y), 1000 + i as i64 * 10));
        }
        for i in 0..3 {
            o.extend(up(&mut r, 20 + i, 1100));
        }
        assert_eq!(gestures_of(&o), vec!["tap3"]);
        let f = pads(&o);
        assert!(f.last().unwrap().slots.iter().any(|s| s.is_some()), "still down at 100 ms: {:?}", f.last());
        let o = r.tick(1000 + TAP_GUARD_MS);
        assert!(pads(&o).last().unwrap().slots.iter().all(|s| s.is_none()), "lifted at the guard");
        // middle click with the fingers where they are
        let o = r.pad_click(BTN_MIDDLE_T);
        assert_eq!(pads(&o).iter().flat_map(|f| f.buttons.clone()).collect::<Vec<_>>(), vec![(BTN_MIDDLE_T, true), (BTN_MIDDLE_T, false)]);
        // gestures off: three keys pressed slowly type
        let mut r = router("full");
        let o1 = tap(&mut r, 1, "key.KEY_A", 0);
        assert!(keys(&o1).contains(&(30, true)));
    }

    const BTN_MIDDLE_T: u16 = crate::touchpad::BTN_MIDDLE;

    #[test]
    fn m3_textedit_select_mode_and_joystick() {
        let mut r = router_cfg("textedit", |c| c.textedit = true);
        let o = tap(&mut r, 1, "key.KEY_LEFT", 0);
        assert_eq!(keys(&o), vec![(105, true), (105, false)]);
        tap(&mut r, 2, "key.select:toggle", 100);
        assert!(r.select_mode() && r.model().select);
        let o = tap(&mut r, 3, "key.KEY_LEFT", 200);
        assert_eq!(keys(&o), vec![(42, true), (105, true), (105, false), (42, false)], "Shift added");
        let o = tap(&mut r, 4, "key.KEY_LEFTCTRL+KEY_RIGHT", 400);
        assert_eq!(keys(&o), vec![(29, true), (42, true), (106, true), (106, false), (42, false), (29, false)], "Ctrl+Shift+Right selects a word");
        tap(&mut r, 5, "key.select:toggle", 600);
        assert!(!r.select_mode());
        // joystick: 6 mm right -> one RIGHT at once, repeats on tick, faster when further
        let c = at(&r, "key.joystick");
        let ppm_native = 124;
        let mut o = down(&mut r, 6, c, 1000);
        o.extend(mv(&mut r, 6, (c.0 + 6 * ppm_native, c.1), 1010));
        assert_eq!(keys(&o), vec![(106, true), (106, false)]);
        let d = r.next_deadline().unwrap();
        assert!(d > 1010 && d <= 1010 + 400, "{}", d);
        let o = r.tick(d);
        assert_eq!(keys(&o), vec![(106, true), (106, false)], "repeat");
        let o = mv(&mut r, 6, (c.0, c.1 - 8 * ppm_native), d + 5);
        assert_eq!(keys(&o), vec![(103, true), (103, false)], "direction change: UP at once");
        up(&mut r, 6, d + 20);
        assert!(r.tick(d + 2000).iter().all(|x| !matches!(x, Out::Key(..))), "nothing after the lift");
        // module off: the joystick does nothing, the select key does nothing
        let mut r = router("textedit");
        let c = at(&r, "key.joystick");
        let mut o = down(&mut r, 1, c, 0);
        o.extend(mv(&mut r, 1, (c.0 + 2000, c.1), 10));
        o.extend(up(&mut r, 1, 20));
        o.extend(tap(&mut r, 2, "key.select:toggle", 100));
        assert!(keys(&o).is_empty() && !r.select_mode());
    }

    #[test]
    fn m3_laser_timer_target_and_mirror() {
        let mut r = router_cfg("remote", |c| {
            c.laser = true;
            c.target_label = Some("Target: this desktop".into());
            c.mirror_pct = 30.0;
        });
        let b = boxn(&r, "region.touchpad");
        let (cx, cy) = ((b[0] + b[2]) / 2, (b[1] + b[3]) / 2);
        let o = down(&mut r, 1, (cx, cy), 0);
        let l: Vec<&Out> = o.iter().filter(|x| matches!(x, Out::Laser(_))).collect();
        assert!(matches!(l[0], Out::Laser(Some((x, y))) if (x - 0.5).abs() < 0.02 && (y - 0.5).abs() < 0.02), "{:?}", l);
        assert!(pads(&o).is_empty(), "the laser is not a touchpad");
        assert!(down(&mut r, 2, (cx + 500, cy), 5).iter().all(|x| !matches!(x, Out::Laser(_))), "one laser finger");
        let o = mv(&mut r, 1, (b[2], cy), 10);
        assert!(o.iter().any(|x| matches!(x, Out::Laser(Some((x, _))) if *x > 0.98)));
        assert!(up(&mut r, 1, 20).contains(&Out::Laser(None)));
        up(&mut r, 2, 25);
        assert!(tap(&mut r, 3, "key.timer:toggle", 100).contains(&Out::Timer("toggle".into())));
        assert!(tap(&mut r, 4, "key.laser:toggle", 200).contains(&Out::SetSetting("features.remote.options.laser".into(), "false".into())));
        assert!(tap(&mut r, 5, "strip.target", 300).contains(&Out::TargetNext));
        let m = r.model().mirror.unwrap();
        let pad = r.model().regions.iter().find(|x| x.id == TOUCHPAD).unwrap().rect;
        assert!(pad.contains(m.x, m.y) && pad.contains(m.right() - 1.0, m.bottom() - 1.0), "preview inside the pad");
        assert!(((m.w / pad.w) - 0.3).abs() < 0.02);
    }

    #[test]
    fn m3_bands_and_emoji_panel() {
        let mut r = router_cfg("full", |c| {
            c.clip_band = true;
            c.suggest_band = true;
            c.emoji = true;
        });
        let strip = r.model().strip;
        let area_top = r.model().regions.iter().map(|x| x.rect.y).fold(f32::MAX, f32::min);
        assert!(area_top >= strip.bottom() + 2.0 * BAND_MM * r.model().px_per_mm - 1.0, "two bands below the strip");
        r.set_clips(vec!["one".into(), "two".into()]);
        r.set_suggestions(vec!["the".into(), "that".into()]);
        assert!(tap(&mut r, 1, "chip.clip.1", 0).contains(&Out::Chip("clip".into(), 1)));
        assert!(tap(&mut r, 2, "chip.suggest.2", 100).contains(&Out::Chip("suggest".into(), 1)), "chip 0 is the emoji chip");
        let o = tap(&mut r, 3, "chip.suggest.0", 200);
        assert!(o.contains(&Out::PanelOpen(true)) && r.emoji_open());
        let o = tap(&mut r, 4, "emoji.item.1", 300);
        assert_eq!(o.iter().filter(|x| matches!(x, Out::Emoji(_))).count(), 1);
        tap(&mut r, 5, "emoji.tab.2", 400);
        assert_eq!(r.model().emoji.as_ref().unwrap().page, 2);
        // touches outside the panel do nothing while it is open
        let o = dn(&mut r, 6, "region.touchpad", 500);
        assert!(!o.iter().any(|x| matches!(x, Out::Pad(_))));
        up(&mut r, 6, 520);
        assert!(tap(&mut r, 7, "panel.close", 600).contains(&Out::PanelOpen(false)));
        assert!(!r.emoji_open());
        // no bands: the areas start right below the strip
        let r2 = router("full");
        let top2 = r2.model().regions.iter().map(|x| x.rect.y).fold(f32::MAX, f32::min);
        assert!(top2 < area_top);
    }
}
