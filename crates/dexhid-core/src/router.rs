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

use crate::arrangement::{self, Arrangement, EditCtx, Guide, Handle, Orientation, QUICK, TOUCHPAD};
use crate::geom::{Geometry, Rect};
use crate::keycodes;
use crate::keys::{ContactId, KeyMachine, KeyOut};
use crate::layout::{Action, KeyKind, Layout, PlacedKey};
use crate::mt::Contact;
use crate::settings::{Kind, Row};
use crate::touchpad::{PadFrame, PadMapper};
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
        }
    }
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
    /// latched sticky modifiers (code, locked)
    pub latched: Vec<(u16, bool)>,
    pub fn_on: bool,
    /// a short notice in the strip (pen pause, pad only, ...)
    pub notice: String,
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
    Ignored,
}

#[derive(Clone, Debug)]
struct Track {
    x: f32,
    y: f32,
    x0: f32,
    y0: f32,
    owner: Owner,
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
}

impl Router {
    pub fn new(cfg: RouterCfg, layout: Layout) -> Router {
        let page = layout.first_page();
        let mut keys = KeyMachine::new(cfg.hold_ms);
        keys.long_ms = cfg.long_ms;
        keys.sticky = cfg.sticky;
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
        }
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
        (Rect::new(0.0, 0.0, g.w, sh), Rect::new(0.0, sh, g.w, g.h - sh))
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
            view.subtitle = if md.live { String::new() } else { format!("planned ({}): settings are stored now", md.plan) };
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

    /// recompute every rect from layout + arrangement + geometry
    fn relayout(&mut self) -> Vec<Out> {
        let mut out = vec![];
        let Some(g) = self.geo else { return out };
        let ppm = g.px_per_mm();
        let (strip, area) = self.work_area();
        let o = self.orient.unwrap_or(Orientation::Landscape);
        let kb = self.kb_regions();
        let mut required = vec![TOUCHPAD.to_string()];
        required.extend(kb.iter().cloned());
        let defaults = Arrangement::default_for(&self.layout.name, &kb, o);
        let mut arr = self.arr.clone().unwrap_or_else(|| defaults.clone());
        arr.layout = self.layout.name.clone();
        arr.orientation = o.name().into();
        for n in arr.fixup(&required, &defaults, &area, &self.mins()) {
            out.push(Out::Note(format!("arrangement: {}", n)));
        }
        self.arr = Some(arr.clone());
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
        // buttons
        let m = (strip.h * 0.14).round();
        let bh = strip.h - 2.0 * m;
        let mut buttons = vec![];
        let specs: Vec<(Btn, f32, String, &str)> = if self.edit {
            vec![(Btn::Cancel, 24.0, "Cancel".into(), "strip.cancel"), (Btn::Reset, 24.0, "Reset".into(), "strip.reset"), (Btn::Done, 24.0, "Done".into(), "strip.done")]
        } else {
            vec![
                (Btn::Exit, 18.0, "Exit".into(), "strip.exit"),
                (Btn::Settings, 26.0, "Settings".into(), "strip.settings"),
                (Btn::Layout, 40.0, format!("Keyboard: {}", self.layout.title), "strip.layout"),
                (Btn::Display, 36.0, "Use as display".into(), "strip.display"),
            ]
        };
        let mut x = strip.right() - m;
        for (b, wmm, label, name) in specs {
            let w = (wmm * ppm).min(strip.w / 5.0);
            x -= w;
            buttons.push(Self::btn(b, Rect::new(x, strip.y + m, w, bh), &label, name.into()));
            x -= m;
        }
        let sheet = if self.sheet_open && !self.edit { Some(self.place_sheet(&area, ppm, &mut buttons)) } else { None };
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
            latched: self.keys.latched(),
            fn_on: self.keys.fn_active(),
            notice,
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
        if self.keys.shift_held() != self.model.shift || latched != self.model.latched || self.keys.fn_active() != self.model.fn_on {
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
                    if p {
                        self.last_key_t = t;
                        out.push(Out::KeyFeedback);
                    }
                    out.push(Out::Key(c, p))
                }
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

    pub fn contact(&mut self, c: Contact, t: i64) -> Vec<Out> {
        let mut out = vec![];
        let Some(g) = self.geo else { return out };
        match c {
            Contact::Down { id, x, y, major } => {
                let (lx, ly) = g.touch_to_logical(x, y);
                let owner = self.owner_for_down(id, lx, ly, major, t, &mut out);
                self.contacts.insert(id, Track { x: lx, y: ly, x0: lx, y0: ly, owner });
                if self.cfg.five_finger && self.contacts.len() >= 5 && self.five.is_none() && !self.five_blocked {
                    self.five = Some((t, false));
                }
            }
            Contact::Move { id, x, y, .. } => {
                let (lx, ly) = g.touch_to_logical(x, y);
                let Some(tr) = self.contacts.get_mut(&id) else { return out };
                tr.x = lx;
                tr.y = ly;
                let tr = tr.clone();
                let ppm = g.px_per_mm();
                let moved = ((tr.x - tr.x0).powi(2) + (tr.y - tr.y0).powi(2)).sqrt();
                if self.five.is_some() && moved > self.cfg.five_slop_mm * ppm {
                    self.five = None;
                    self.five_blocked = true;
                }
                match tr.owner {
                    Owner::Pad => {
                        if let Some(p) = self.pad.as_mut() {
                            p.motion(id, lx, ly);
                        }
                        self.pad_frame(&mut out);
                    }
                    Owner::Key => {
                        let k = self.hit_key(lx, ly);
                        let v = self.keys.motion(id, k);
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
            }
            Contact::Up { id } => {
                let Some(tr) = self.contacts.remove(&id) else { return out };
                match tr.owner {
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
                            p.up(id);
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
        if self.sheet_open && !self.edit {
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
        for r in &self.model.regions {
            if !r.rect.contains(x, y) {
                continue;
            }
            if r.id == TOUCHPAD {
                if palm {
                    out.push(Out::Note(format!("palm: contact {} ignored (major {})", id, major)));
                    return Owner::Ignored;
                }
                if self.cfg.dwt_ms > 0 && t - self.last_key_t < self.cfg.dwt_ms {
                    out.push(Out::Note(format!("dwt: pad contact {} ignored ({} ms after a key)", id, t - self.last_key_t)));
                    return Owner::Ignored;
                }
                if let Some(p) = self.pad.as_mut() {
                    if p.down(id, x, y) {
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
                let long = self.model.keys[k].long.clone();
                let v = self.keys.down(id, k, &kind, long.as_ref(), t);
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
                out.push(Out::PanelOpen(false));
                out.extend(self.relayout());
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
        for tr in self.contacts.values_mut() {
            if matches!(tr.owner, Owner::Pad | Owner::Key | Owner::Quick(_)) {
                tr.owner = Owner::Ignored;
            }
        }
        out
    }

    /// forget all contacts (after an ungrab: the next grab starts clean)
    pub fn reset_contacts(&mut self) -> Vec<Out> {
        let mut out = self.release_all();
        self.contacts.clear();
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
        let mut seen = HashSet::new();
        for k in &self.model.keys {
            let n = match &k.kind {
                KeyKind::Code(c) => format!("key.{}", keycodes::name(*c)),
                KeyKind::Combo(v) => format!("key.{}", v.iter().map(|c| keycodes::name(*c)).collect::<Vec<_>>().join("+")),
                KeyKind::Action(Action::Page(p)) => format!("key.page:{}", p),
                KeyKind::Action(Action::LayoutNext) => "key.layout:next".into(),
                KeyKind::Action(Action::KeyboardHide) => "key.keyboard:hide".into(),
                KeyKind::Action(Action::Fn) => "key.fn".into(),
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
        // a planned module is marked
        tap(&mut r, 7, "sheet.tab.gestures", 600);
        assert!(r.model().sheet.as_ref().unwrap().subtitle.contains("planned"));
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
}
