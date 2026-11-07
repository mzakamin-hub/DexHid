// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! Pad-mode touch routing. Every contact belongs to the area where it STARTED until it lifts: the control strip,
//! a settings-panel button, the touchpad, a keyboard key, or (edit mode) a region handle. The router also owns the
//! on-screen model the UI draws (strip buttons, regions, keys, pressed state, touch dots) and the 5-finger release.
//! Pure logic: no devices, no clock (times are passed in, ms).

use crate::arrangement::{self, Arrangement, EditCtx, Handle, Orientation, TOUCHPAD};
use crate::geom::{Geometry, Rect};
use crate::keycodes;
use crate::keys::{ContactId, KeyMachine, KeyOut};
use crate::layout::{Action, KeyKind, Layout, PlacedKey};
use crate::mt::Contact;
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
    PanelLayout,
    PanelAutostart,
    PanelEdit,
    PanelClose,
}

impl Btn {
    pub fn name(&self) -> &'static str {
        match self {
            Btn::Display => "strip.display",
            Btn::Layout => "strip.layout",
            Btn::Settings => "strip.settings",
            Btn::Exit => "strip.exit",
            Btn::Done => "strip.done",
            Btn::Reset => "strip.reset",
            Btn::Cancel => "strip.cancel",
            Btn::PanelLayout => "panel.layout",
            Btn::PanelAutostart => "panel.autostart",
            Btn::PanelEdit => "panel.edit",
            Btn::PanelClose => "panel.close",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ButtonView {
    pub btn: Btn,
    pub rect: Rect,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RegionView {
    pub id: String,
    pub rect: Rect,
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
    pub panel: Option<Rect>,
    pub panel_title: String,
    // dynamic part
    pub lit_keys: Vec<usize>,
    pub lit_button: Option<Btn>,
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
    AutostartNext,
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
    panel_open: bool,
    autostart_label: String,
    keys: KeyMachine,
    pad: Option<PadMapper>,
    contacts: HashMap<ContactId, Track>,
    five: Option<(i64, bool)>,
    five_blocked: bool,
    model: UiModel,
    orient: Option<Orientation>,
}

impl Router {
    pub fn new(cfg: RouterCfg, layout: Layout) -> Router {
        let page = layout.first_page();
        let hold = cfg.hold_ms;
        Router {
            cfg,
            geo: None,
            layout,
            page,
            arr: None,
            arr_backup: None,
            edit: false,
            panel_open: false,
            autostart_label: "ask".into(),
            keys: KeyMachine::new(hold),
            pad: None,
            contacts: HashMap::new(),
            five: None,
            five_blocked: false,
            model: UiModel::default(),
            orient: None,
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

    pub fn set_autostart_label(&mut self, s: &str) -> Vec<Out> {
        self.autostart_label = s.to_string();
        if self.panel_open {
            self.relayout()
        } else {
            vec![]
        }
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

    fn kb_regions(&self) -> Vec<String> {
        self.layout.regions(&self.cfg.options)
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
        // touchpad
        let pad_rect = regions.iter().find(|r| r.id == TOUCHPAD).map(|r| r.rect).unwrap_or(area);
        let zone = if self.cfg.click_zone_mm > 0.0 { self.cfg.click_zone_mm * ppm } else { 0.0 };
        let upp = self.cfg.pad_units_per_mm / ppm;
        let changed = match &self.pad {
            Some(p) => p.rect != pad_rect || (p.units_per_px - upp).abs() > 1e-4 || (p.zone_h - zone.min(pad_rect.h / 3.0)).abs() > 1e-3,
            None => true,
        };
        if changed {
            if let Some(p) = self.pad.as_mut() {
                if let Some(f) = p.release_all() {
                    out.push(Out::Pad(f));
                }
            }
            self.pad = Some(PadMapper::new(pad_rect, upp, zone));
            out.push(Out::PadChanged);
        }
        // buttons
        let m = (strip.h * 0.14).round();
        let bh = strip.h - 2.0 * m;
        let mut buttons = vec![];
        let specs: Vec<(Btn, f32, String)> = if self.edit {
            vec![(Btn::Cancel, 24.0, "Cancel".into()), (Btn::Reset, 24.0, "Reset".into()), (Btn::Done, 24.0, "Done".into())]
        } else {
            vec![
                (Btn::Exit, 18.0, "Exit".into()),
                (Btn::Settings, 26.0, "Settings".into()),
                (Btn::Layout, 40.0, format!("Keyboard: {}", self.layout.title)),
                (Btn::Display, 36.0, "Use as display".into()),
            ]
        };
        let mut x = strip.right() - m;
        for (b, wmm, label) in specs {
            let w = (wmm * ppm).min(strip.w / 5.0);
            x -= w;
            buttons.push(ButtonView { btn: b, rect: Rect::new(x, strip.y + m, w, bh), label });
            x -= m;
        }
        let mut panel = None;
        if self.panel_open && !self.edit {
            let pw = (130.0 * ppm).min(area.w * 0.9);
            let rowh = 14.0 * ppm;
            let ph = rowh * 5.6;
            let p = Rect::new(area.x + (area.w - pw) / 2.0, area.y + (area.h - ph).max(0.0) / 2.0, pw, ph.min(area.h));
            let items = [
                (Btn::PanelLayout, format!("Keyboard: {}", self.layout.title)),
                (Btn::PanelAutostart, format!("When a monitor is plugged in: {}", self.autostart_label)),
                (Btn::PanelEdit, "Move / resize areas".to_string()),
                (Btn::PanelClose, "Close".to_string()),
            ];
            for (i, (b, l)) in items.into_iter().enumerate() {
                let r = Rect::new(p.x + m * 2.0, p.y + rowh * (1.2 + i as f32 * 1.05), p.w - m * 4.0, rowh * 0.9);
                buttons.push(ButtonView { btn: b, rect: r, label: l });
            }
            panel = Some(p);
        }
        let zones = self.pad.as_ref().and_then(|p| p.zones());
        let version = self.model.version + 1;
        self.model = UiModel {
            version,
            w: g.w,
            h: g.h,
            px_per_mm: ppm,
            strip,
            title: "DexHid".into(),
            hint: if self.edit { "drag an area to move it, a corner to resize it".into() } else { "hold the bar to move / resize the areas".into() },
            buttons,
            regions,
            zones,
            keys,
            shift: self.keys.shift_held(),
            edit: self.edit,
            panel,
            panel_title: "Settings (more in M2)".into(),
            lit_keys: self.keys.lit_keys(),
            lit_button: None,
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
        if lit != self.model.lit_keys || dots != self.model.dots || btn != self.model.lit_button {
            self.model.lit_keys = lit;
            self.model.dots = dots;
            self.model.lit_button = btn;
            out.push(Out::Dynamic);
        }
        if self.keys.shift_held() != self.model.shift {
            self.model.shift = self.keys.shift_held();
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

    fn key_outs(&mut self, v: Vec<KeyOut>, out: &mut Vec<Out>) {
        for k in v {
            match k {
                KeyOut::Key(c, p) => out.push(Out::Key(c, p)),
                KeyOut::Action(Action::Page(id)) => {
                    self.page = id;
                    out.extend(self.relayout());
                }
                KeyOut::Action(Action::LayoutNext) => out.push(Out::LayoutNext),
                KeyOut::Action(Action::KeyboardHide) => out.push(Out::SetLayout("none".into())),
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
                        self.key_outs(v, &mut out);
                    }
                    Owner::Edit { handle, ref region, orig, x0, y0 } => {
                        let (_, area) = self.work_area();
                        let others: Vec<Rect> = self.model.regions.iter().filter(|r| &r.id != region).map(|r| r.rect).collect();
                        let min = self.mins().get(region).copied().unwrap_or((10.0, 10.0));
                        let ctx = EditCtx { area, snap: self.cfg.snap_mm * ppm, min, others: &others };
                        let nr = arrangement::drag(&orig, handle, lx - x0, ly - y0, &ctx);
                        let cur = self.model.regions.iter().find(|r| &r.id == region).map(|r| r.rect);
                        if cur != Some(nr) {
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
                        self.key_outs(v, &mut out);
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

    fn owner_for_down(&mut self, id: ContactId, x: f32, y: f32, major: i32, t: i64, out: &mut Vec<Out>) -> Owner {
        let strip = self.model.strip;
        if self.panel_open && !self.edit {
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
        for r in &self.model.regions {
            if !r.rect.contains(x, y) {
                continue;
            }
            if r.id == TOUCHPAD {
                if self.cfg.palm_major > 0 && major > self.cfg.palm_major {
                    out.push(Out::Note(format!("palm: contact {} ignored (major {})", id, major)));
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
            if let Some(k) = self.hit_key(x, y) {
                let kind = self.model.keys[k].kind.clone();
                let v = self.keys.down(id, k, &kind, t);
                self.key_outs(v, out);
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
        self.panel_open = false;
        out.push(Out::EditMode(true));
        out.extend(self.relayout());
    }

    fn fire(&mut self, b: Btn, out: &mut Vec<Out>) {
        out.push(Out::Note(format!("button {}", b.name())));
        match b {
            Btn::Display => out.push(Out::UseAsDisplay),
            Btn::Exit => out.push(Out::Exit),
            Btn::Layout | Btn::PanelLayout => out.push(Out::LayoutNext),
            Btn::PanelAutostart => out.push(Out::AutostartNext),
            Btn::Settings => {
                self.panel_open = !self.panel_open;
                out.push(Out::PanelOpen(self.panel_open));
                out.extend(self.relayout());
            }
            Btn::PanelClose => {
                self.panel_open = false;
                out.push(Out::PanelOpen(false));
                out.extend(self.relayout());
            }
            Btn::PanelEdit => self.enter_edit(out),
            Btn::Done => {
                self.edit = false;
                self.arr_backup = None;
                out.push(Out::EditMode(false));
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
                self.edit = false;
                out.push(Out::EditMode(false));
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
        self.key_outs(v, &mut out);
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
            if matches!(tr.owner, Owner::Pad | Owner::Key) {
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

    /// GEOM lines for logs and the tablet test runner: native touch coordinates of every button, region, zone and key
    pub fn geom_lines(&self) -> Vec<String> {
        let Some(g) = self.geo else { return vec![] };
        let mut v = vec![];
        let mut line = |name: &str, r: &Rect| {
            let (x0, y0, x1, y1) = g.rect_to_touch(r);
            v.push(format!("GEOM name={} native={},{},{},{} logical={:.0},{:.0},{:.0},{:.0}", name, x0, y0, x1, y1, r.x, r.y, r.w, r.h));
        };
        line("strip", &self.model.strip);
        for b in &self.model.buttons {
            line(b.btn.name(), &b.rect);
        }
        for r in &self.model.regions {
            line(&format!("region.{}", r.id), &r.rect);
        }
        if let Some((l, r)) = &self.model.zones {
            line("zone.left", l);
            line("zone.right", r);
        }
        let mut seen = HashSet::new();
        for k in &self.model.keys {
            let n = match &k.kind {
                KeyKind::Code(c) => format!("key.{}", keycodes::name(*c)),
                KeyKind::Action(Action::Page(p)) => format!("key.page:{}", p),
                KeyKind::Action(Action::LayoutNext) => "key.layout:next".into(),
                KeyKind::Action(Action::KeyboardHide) => "key.keyboard:hide".into(),
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
        let l = r.geom_lines().into_iter().find(|l| l.contains(&format!("name={} ", name))).unwrap_or_else(|| panic!("no {}", name));
        let n: Vec<i32> = l.split("native=").nth(1).unwrap().split(' ').next().unwrap().split(',').map(|x| x.parse().unwrap()).collect();
        ((n[0] + n[2]) / 2, (n[1] + n[3]) / 2)
    }

    fn down(r: &mut Router, id: u32, p: (i32, i32), t: i64) -> Vec<Out> {
        r.contact(Contact::Down { id, x: p.0, y: p.1, major: 10 }, t)
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
        // starts on the pad, slides over the keyboard: still a pad finger, no key
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
        // long press on the strip enters edit mode
        dn(&mut r, 1, "strip", 0);
        assert_eq!(r.next_deadline(), Some(700));
        let o = r.tick(700);
        assert!(o.contains(&Out::EditMode(true)) && r.editing());
        let o = up(&mut r, 1, 800);
        assert!(!o.iter().any(|x| matches!(x, Out::UseAsDisplay | Out::Exit | Out::LayoutNext)), "no button after a long press");
        // move the touchpad 100 px left
        let c = at(&r, "region.touchpad");
        let g = geo();
        down(&mut r, 2, c, 1000);
        let (lx, ly) = g.touch_to_logical(c.0, c.1);
        let to = g.logical_to_touch(lx - 100.0, ly);
        let o = mv(&mut r, 2, to, 1100);
        assert!(o.contains(&Out::Static));
        up(&mut r, 2, 1200);
        let moved = r.arrangement().get(TOUCHPAD).unwrap();
        assert!((moved.x - (before.x - 100.0 / 1696.0)).abs() < 0.002, "{:?} {:?}", before, moved);
        // Done persists
        dn(&mut r, 3, "strip.done", 1300);
        let o = up(&mut r, 3, 1350);
        assert!(o.contains(&Out::SaveArrangement) && !r.editing());
        // edit again, resize the bottom-right corner, then Cancel restores
        dn(&mut r, 4, "strip", 2000);
        r.tick(2700);
        up(&mut r, 4, 2800);
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
        // Reset -> defaults
        dn(&mut r, 7, "strip", 4000);
        r.tick(4700);
        up(&mut r, 7, 4800);
        dn(&mut r, 8, "strip.reset", 5000);
        let o = up(&mut r, 8, 5050);
        assert!(o.contains(&Out::ResetArrangement));
        assert_eq!(r.arrangement().get(TOUCHPAD).unwrap(), before);
    }

    #[test]
    fn edit_never_overlaps_strip_or_regions() {
        let mut r = router("full");
        dn(&mut r, 1, "strip", 0);
        r.tick(700);
        up(&mut r, 1, 800);
        let g = geo();
        let c = at(&r, "region.touchpad");
        down(&mut r, 2, c, 1000);
        // drag far up into the keyboard and the strip
        mv(&mut r, 2, (c.0, 0), 1100);
        up(&mut r, 2, 1200);
        let m = r.model();
        let tp = m.regions.iter().find(|x| x.id == TOUCHPAD).unwrap().rect;
        let kb = m.regions.iter().find(|x| x.id == "keyboard").unwrap().rect;
        assert!(!tp.overlaps(&kb) && tp.y >= m.strip.bottom() - 0.01, "{:?} {:?}", tp, kb);
        let _ = g;
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
        // moving fingers do not trigger it
        let mut r = router("full");
        for i in 0..5 {
            down(&mut r, i + 1, (c.0 + i as i32 * 1500, c.1), 0);
        }
        mv(&mut r, 1, (c.0 + 2000, c.1 + 2000), 100);
        assert!(!r.tick(900).contains(&Out::FiveFinger));
    }

    #[test]
    fn settings_panel() {
        let mut r = router("full");
        dn(&mut r, 1, "strip.settings", 0);
        let o = up(&mut r, 1, 30);
        assert!(o.contains(&Out::PanelOpen(true)));
        dn(&mut r, 2, "panel.autostart", 100);
        assert!(up(&mut r, 2, 130).contains(&Out::AutostartNext));
        // keys under the panel do not type while it is open
        let o = dn(&mut r, 3, "region.touchpad", 200);
        assert!(!o.iter().any(|x| matches!(x, Out::Pad(_))));
        up(&mut r, 3, 230);
        dn(&mut r, 4, "panel.close", 300);
        assert!(up(&mut r, 4, 330).contains(&Out::PanelOpen(false)));
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
        // a touch at the pad centre (in rotated native coordinates) reaches the pad
        let c = at(&r, "region.touchpad");
        let o = down(&mut r, 1, c, 0);
        assert!(o.iter().any(|x| matches!(x, Out::Pad(_))));
    }
}
