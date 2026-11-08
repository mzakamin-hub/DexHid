// SPDX-License-Identifier: GPL-3.0-or-later
//! M4: the standalone app window (`dexhid window`, `dexhid settings`, the app menu). A normal xdg_toplevel on the
//! internal panel, made by the UI thread on request and destroyed on close (nothing exists while it is closed):
//! works without an external monitor and without grabbing the touchscreen (plain wl_touch / wl_pointer input). A
//! navigation rail (Home + the settings registry's modules) and a page: Home (the 3-way mode switch with its live
//! lines and hints, the layout picker) or one settings module (rows from `settings::rows`, the same as the sheet).
//! Laid out from constraints (tokens in mm, content width capped, two columns only when they fit), every target
//! >= 9 mm, text ellipsized; long pages scroll by drag.

use crate::look::{button, icon, Icon, Roles, Tokens, Tone};
use crate::render::{Align, Canvas, Fonts};
use dexhid_core::feedback::Palette;
use dexhid_core::geom::Rect;
use dexhid_core::settings::Kind;

/// one settings row of the window
#[derive(Clone, Debug, PartialEq)]
pub struct WinRow {
    pub key: String,
    pub title: String,
    pub value: String,
    pub kind: Option<Kind>,
}

/// what the window shows (built by the daemon from its state, published when it changes)
#[derive(Clone, Debug, PartialEq)]
pub struct WinView {
    /// 0 = Home, i + 1 = module i (the UI's own selection, set before drawing)
    pub page: usize,
    /// module ids and titles (the settings registry order)
    pub module_ids: Vec<String>,
    pub modules: Vec<String>,
    /// the rows of every module
    pub module_rows: Vec<Vec<WinRow>>,
    /// the rows of the selected module (filled from `module_rows` by `select`)
    pub rows: Vec<WinRow>,
    pub subtitle: String,
    pub palette: Palette,
    /// the persisted `mode_select`: "usb" | "pad" | "screen" (the one mode switch on Home)
    pub mode: String,
    /// what runs now, one line ("Keyboard + touchpad on DP-1", "Waiting for a host", "Released")
    pub mode_detail: String,
    /// an external monitor is connected (the pad mode is possible), its connector name
    pub monitor: bool,
    pub monitor_name: String,
    /// the USB HID line (power in, data role, host state)
    pub usb_line: String,
    /// USB hint when the mode cannot work as is: "host" (port is in host role) | "none" (nothing attached) | ""
    pub usb_hint: String,
    /// (name, title) of the layouts, and the current one
    pub layouts: Vec<(String, String)>,
    pub layout: String,
    /// scroll offset of the page (logical px, the UI's own)
    pub scroll: f32,
}

impl Default for WinView {
    fn default() -> WinView {
        WinView {
            page: 0,
            module_ids: vec![],
            modules: vec![],
            module_rows: vec![],
            rows: vec![],
            subtitle: String::new(),
            palette: dexhid_core::feedback::palette("dark", "#4a8be0", 100),
            mode: "screen".into(),
            mode_detail: String::new(),
            monitor: false,
            monitor_name: String::new(),
            usb_line: String::new(),
            usb_hint: String::new(),
            layouts: vec![],
            layout: String::new(),
            scroll: 0.0,
        }
    }
}

impl WinView {
    /// select a page: 0 = Home, i + 1 = module i (clamped)
    pub fn select(&mut self, page: usize) {
        self.page = page.min(self.modules.len());
        self.rows = if self.page > 0 { self.module_rows.get(self.page - 1).cloned().unwrap_or_default() } else { vec![] };
        self.subtitle = if self.page > 0 { "Changes apply at once and are saved to settings.toml".into() } else { String::new() };
    }

    /// the page index of "home" | "settings" (= the first module) | a module id
    pub fn page_of(&self, name: &str) -> usize {
        match name {
            "home" | "" => 0,
            "settings" => 1.min(self.modules.len()),
            id => self.module_ids.iter().position(|m| m == id).map(|i| i + 1).unwrap_or(0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WinHit {
    Nav(u8),
    /// the mode switch: 0 = USB keyboard + mouse, 1 = keyboard + touchpad for the monitor, 2 = second screen
    Mode(u8),
    Layout(u8),
    RowToggle(u8),
    RowChoice(u8),
    RowMinus(u8),
    RowPlus(u8),
}

/// rects of everything on the page; drawing and hit tests use the same list
#[derive(Clone, Debug, Default)]
pub struct WinLayout {
    pub rail: Rect,
    pub nav: Vec<Rect>,
    pub content: Rect,
    pub header: Rect,
    pub cards: Vec<Rect>,
    pub rows: Vec<Rect>,
    pub hits: Vec<(Rect, WinHit)>,
    /// the page is taller than the window (scrolls)
    pub overflow: bool,
    /// the page's bottom edge in unscrolled coordinates (for the scroll limit)
    pub bottom: f32,
}

impl WinLayout {
    /// the largest useful scroll offset for a window of height `h`
    pub fn max_scroll(&self, h: f32, ppm: f32) -> f32 {
        (self.bottom + 4.8 * ppm - h).max(0.0)
    }
}

pub fn layout(w: f32, h: f32, ppm: f32, v: &WinView) -> WinLayout {
    let t = Tokens::new(ppm, 1.0);
    let (w, h) = (w.max(1.0), h.max(1.0));
    let mut l = WinLayout::default();
    // rail: 26 % of the width within 44..64 mm; below 120 mm window width it shrinks to icons-only width
    let rail_w = (w * 0.26).clamp(44.0 * t.ppm, 64.0 * t.ppm).min(w * 0.4);
    l.rail = Rect::new(0.0, 0.0, rail_w, h);
    let n = v.modules.len() + 1;
    let top = t.s4 + t.t_title * 1.6;
    let item_h = ((h - top - t.s4) / n as f32).clamp(t.target + t.ppm * 0.4, 11.0 * t.ppm);
    for i in 0..n {
        let r = Rect::new(t.s2, top + i as f32 * item_h, rail_w - 2.0 * t.s2, item_h - t.ppm * 0.4);
        l.overflow |= r.bottom() > h;
        l.nav.push(r);
        l.hits.push((r, WinHit::Nav(i as u8)));
    }
    let cx = rail_w + t.s6;
    let cw = (w - cx - t.s6).min(190.0 * t.ppm).max(1.0);
    l.content = Rect::new(cx, 0.0, cw, h);
    let mut y = t.s6 - v.scroll;
    l.header = Rect::new(cx, y, cw, t.t_display * 1.5 + t.t_caption * 1.8);
    y = l.header.bottom() + t.s4;
    let bh = t.target.max(11.0 * t.ppm);
    let _ = bh;
    if v.page == 0 {
        // the mode card: one segmented switch (3 segments side by side when each gets >= 46 mm, else stacked
        // full-width rows), then the selected mode's detail: live line + hint
        let inner = (cw - 2.0 * t.s4).max(1.0);
        let horiz = inner >= 3.0 * 46.0 * t.ppm;
        let seg_h = if horiz { 17.0 * t.ppm } else { t.target.max(12.0 * t.ppm) };
        let switch_h = if horiz { seg_h } else { 3.0 * seg_h };
        let detail_h = t.s3 + t.t_caption * 1.6 * 3.0;
        let ch = t.s4 + t.t_label * 1.6 + t.s2 + switch_h + 2.0 * t.ppm * 0.5 + detail_h + t.s4;
        let c = Rect::new(cx, y, cw, ch);
        l.cards.push(c);
        let sw = Rect::new(c.x + t.s4, c.y + t.s4 + t.t_label * 1.6 + t.s2, inner, switch_h + t.ppm);
        l.cards.push(sw);
        let p = t.ppm * 0.5;
        for i in 0..3u8 {
            let r = if horiz {
                let w = (sw.w - 2.0 * p) / 3.0;
                Rect::new(sw.x + p + i as f32 * w, sw.y + p, w, seg_h)
            } else {
                Rect::new(sw.x + p, sw.y + p + i as f32 * seg_h, sw.w - 2.0 * p, seg_h)
            };
            l.hits.push((r, WinHit::Mode(i)));
        }
        l.cards.push(Rect::new(c.x + t.s4, sw.bottom() + t.s3, inner, detail_h - t.s3));
        y = c.bottom() + t.s4;
        // layout picker: chips in a wrapping grid, >= 9 mm tall, columns from the width
        let cols = ((cw - 2.0 * t.s4) / (40.0 * t.ppm)).floor().clamp(1.0, 4.0) as usize;
        let chip_w = (cw - 2.0 * t.s4 - (cols - 1) as f32 * t.s2) / cols as f32;
        let chip_h = t.target.max(10.0 * t.ppm);
        let rows = v.layouts.len().div_ceil(cols).max(1);
        let ch = t.s4 + t.t_label * 1.6 + t.s2 + rows as f32 * (chip_h + t.s2) + t.s3;
        let c = Rect::new(cx, y, cw, ch);
        l.cards.push(c);
        for i in 0..v.layouts.len() {
            let r = Rect::new(c.x + t.s4 + (i % cols) as f32 * (chip_w + t.s2), c.y + t.s4 + t.t_label * 1.6 + t.s2 + (i / cols) as f32 * (chip_h + t.s2), chip_w, chip_h);
            l.hits.push((r, WinHit::Layout(i as u8)));
        }
        l.overflow |= c.bottom() > h;
        l.bottom = c.bottom() + v.scroll;
    } else {
        let row_h = t.target.max(11.5 * t.ppm);
        l.bottom = y + v.rows.len() as f32 * row_h + v.scroll;
        for (i, WinRow { kind, .. }) in v.rows.iter().enumerate() {
            let r = Rect::new(cx, y + i as f32 * row_h, cw, row_h);
            l.rows.push(r);
            let idx = i as u8;
            let ch = t.target;
            let cy = r.y + (r.h - ch) / 2.0;
            match kind {
                Some(Kind::Bool) => l.hits.push((Rect::new(r.right() - 16.0 * t.ppm, cy, 16.0 * t.ppm, ch), WinHit::RowToggle(idx))),
                Some(Kind::Num { .. }) => {
                    l.hits.push((Rect::new(r.right() - ch, cy, ch, ch), WinHit::RowPlus(idx)));
                    l.hits.push((Rect::new(r.right() - 3.0 * ch - t.s2, cy, ch, ch), WinHit::RowMinus(idx)));
                }
                _ => l.hits.push((Rect::new(r.right() - (44.0 * t.ppm).min(cw * 0.45), cy, (44.0 * t.ppm).min(cw * 0.45), ch), WinHit::RowChoice(idx))),
            }
            l.overflow |= r.bottom() > h;
        }
    }
    l
}

pub fn draw(cv: &mut Canvas, f: &mut Fonts, p: &Palette, w: f32, h: f32, ppm: f32, v: &WinView, pressed: Option<WinHit>) -> WinLayout {
    let t = Tokens::new(ppm, 1.0);
    let c = Roles::from(p);
    let l = layout(w, h, ppm, v);
    cv.clear(c.bg | 0xff00_0000);
    // rail
    cv.fill(&l.rail, c.surface | 0xff00_0000);
    cv.bar(&Rect::new(l.rail.right() - t.hair, 0.0, t.hair, h), c.hairline);
    let mark = t.ppm * 6.4;
    let mr = Rect::new(t.s4, t.s4, mark, mark);
    cv.rrect(&mr, mark * 0.28, c.accent);
    icon(cv, &mr, Icon::Keyboard, mark * 0.62, c.on_accent);
    f.text(cv, &Rect::new(mr.right() + t.s2, t.s4 - t.s1, l.rail.w - mark - t.s8, mark + t.s2), "DexHid", t.t_title, c.text, Align::Left, 0.8);
    for (i, r) in l.nav.iter().enumerate() {
        let sel = v.page == i;
        if sel {
            cv.rrect(r, t.r_md, c.accent_soft);
            cv.rrect(&Rect::new(r.x, r.y + r.h * 0.22, t.ppm * 0.7, r.h * 0.56), t.ppm * 0.35, c.accent);
        }
        let label = if i == 0 { "Home" } else { v.modules.get(i - 1).map(|s| s.as_str()).unwrap_or("") };
        let ic = if i == 0 { Some(Icon::Desktop) } else { None };
        let mut x = r.x + t.s3;
        if let Some(ic) = ic {
            icon(cv, &Rect::new(x, r.y, t.icon, r.h), ic, t.icon * 0.85, if sel { c.accent } else { c.text_dim });
            x += t.icon + t.s2;
        }
        f.text(cv, &Rect::new(x, r.y, r.right() - x - t.s2, r.h), label, t.t_body.min(r.h * 0.5), if sel { c.text } else { c.text_dim }, Align::Left, 0.8);
        if i == 0 {
            cv.bar(&Rect::new(r.x + t.s2, r.bottom() + t.ppm * 0.2, r.w - t.s4, t.hair), c.hairline);
        }
    }
    let hit_of = |want: WinHit| l.hits.iter().find(|(_, x)| *x == want).map(|(r, _)| *r);
    // header
    let hd = l.header;
    let (title, sub) = if v.page == 0 { ("Home".to_string(), "One switch: USB keyboard + mouse, keyboard + touchpad for the monitor, or a plain second screen".to_string()) } else { (v.modules.get(v.page - 1).cloned().unwrap_or_default(), v.subtitle.clone()) };
    f.text(cv, &Rect::new(hd.x, hd.y, hd.w * 0.6, t.t_display * 1.5), &title, t.t_display, c.text, Align::Left, 0.7);
    if !sub.is_empty() {
        f.text(cv, &Rect::new(hd.x, hd.y + t.t_display * 1.5, hd.w, t.t_caption * 1.8), &sub, t.t_caption, c.text_dim, Align::Left, 0.85);
    }
    if v.page == 0 {
        let sel_idx: u8 = match v.mode.as_str() {
            "usb" => 0,
            "pad" => 1,
            _ => 2,
        };
        // status pill (top right of the header): what runs now
        let (dot, label) = match sel_idx {
            0 => (if v.usb_hint.is_empty() { 0xff3fb27f } else { c.edit }, "USB keyboard + mouse"),
            1 => (if v.monitor { c.accent } else { c.edit }, "Keyboard + touchpad"),
            _ => (c.text_dim, "Second screen"),
        };
        let text = format!("{}  \u{b7}  {}", label, v.mode_detail);
        let pw = (f.measure(&text, t.t_caption, cv.scale) + t.s8 + t.s3).min(hd.w * 0.45);
        let pr = Rect::new(hd.right() - pw, hd.y + t.s1, pw, t.ppm * 7.0);
        cv.rrect(&pr, pr.h / 2.0, c.raised | 0xff00_0000);
        cv.rrect_ring(&pr, pr.h / 2.0, t.hair, c.hairline);
        cv.disc(pr.x + t.s3 + t.ppm * 0.8, pr.y + pr.h / 2.0, t.ppm * 1.0, dot);
        f.text(cv, &Rect::new(pr.x + t.s3 + t.ppm * 2.6, pr.y, pr.w - t.s4 - t.ppm * 2.6, pr.h), &text, t.t_caption, c.text, Align::Left, 0.85);
        // the mode card
        let cr = l.cards[0];
        cv.shadow(&cr, t.r_lg, t.s1, c.shadow / 2);
        cv.rrect(&cr, t.r_lg, c.raised | 0xff00_0000);
        cv.rrect_ring(&cr, t.r_lg, t.hair, c.hairline);
        f.text(cv, &Rect::new(cr.x + t.s4, cr.y + t.s4, cr.w - t.s8, t.t_label * 1.6), "What the tablet does", t.t_label, c.text, Align::Left, 0.8);
        let sw = l.cards[1];
        cv.rrect(&sw, t.r_md + t.ppm * 0.5, c.key_mod | 0xff00_0000);
        let usb_ok = v.usb_hint.is_empty();
        let segs: [(Icon, &str, String, bool); 3] = [
            (Icon::Usb, "USB keyboard + mouse", if usb_ok { "for another computer".into() } else if v.usb_hint == "host" { "port is in host role".into() } else { "no computer attached".into() }, true),
            (Icon::Display, "Keyboard + touchpad", if v.monitor { format!("for {}", if v.monitor_name.is_empty() { "the monitor" } else { &v.monitor_name }) } else { "no monitor connected".into() }, v.monitor),
            (Icon::Desktop, "Second screen", "tablet stays a normal screen".into(), true),
        ];
        let horiz = l.hits.iter().filter(|(_, x)| matches!(x, WinHit::Mode(_))).map(|(r, _)| r.y).fold(f32::NAN, |a, y| if a.is_nan() || (a - y).abs() < 0.5 { y } else { -1.0 }) >= 0.0;
        for (r, hit) in l.hits.iter().filter(|(_, x)| matches!(x, WinHit::Mode(_))) {
            let WinHit::Mode(i) = *hit else { continue };
            let (ic, name, sub, enabled) = &segs[i as usize];
            let sel = i == sel_idx;
            let down = pressed == Some(*hit) && *enabled;
            if sel || down {
                cv.shadow(r, t.r_md, t.ppm * 0.6, c.shadow / 2);
                cv.rrect(r, t.r_md, if down && !sel { c.accent_soft } else { c.accent });
            }
            let (fg, fg2) = if sel { (c.on_accent, (c.on_accent & 0x00ff_ffff) | 0xc0 << 24) } else if *enabled { (c.text, c.text_dim) } else { ((c.text_dim & 0x00ff_ffff) | 0x90 << 24, c.edit) };
            let isz = t.icon;
            if horiz {
                // icon on top, name, sub line
                let gy = r.y + (r.h - (isz + t.s1 + t.t_label * 1.25 + t.t_caption * 1.3)) / 2.0;
                icon(cv, &Rect::new(r.x, gy, r.w, isz), *ic, isz, fg);
                f.text(cv, &Rect::new(r.x + t.s2, gy + isz + t.s1, r.w - t.s4, t.t_label * 1.25), name, t.t_label, fg, Align::Center, 0.75);
                f.text(cv, &Rect::new(r.x + t.s2, gy + isz + t.s1 + t.t_label * 1.25, r.w - t.s4, t.t_caption * 1.3), sub, t.t_caption, fg2, Align::Center, 0.8);
            } else {
                // a row: icon, name, sub line right-aligned
                icon(cv, &Rect::new(r.x + t.s3, r.y, isz, r.h), *ic, isz, fg);
                let nx = r.x + t.s3 + isz + t.s3;
                let nw = f.measure(name, t.t_label, cv.scale).min(r.right() - nx - t.s3);
                f.text(cv, &Rect::new(nx, r.y, nw + 1.0, r.h), name, t.t_label, fg, Align::Left, 0.75);
                f.text(cv, &Rect::new(nx + nw + t.s3, r.y, (r.right() - nx - nw - t.s6).max(0.0), r.h), sub, t.t_caption, fg2, Align::Right, 0.8);
            }
            if sel {
                let d = t.ppm * 1.2;
                cv.disc(r.right() - t.s2 - d, r.y + t.s2 + d, d, c.on_accent);
                icon(cv, &Rect::new(r.right() - t.s2 - 2.0 * d, r.y + t.s2, 2.0 * d, 2.0 * d), Icon::Check, d * 1.5, c.accent);
            }
        }
        // the selected mode's detail: live line + hint
        let dr = l.cards[2];
        let lh = t.t_caption * 1.6;
        let line = |cv: &mut Canvas, f: &mut Fonts, k: usize, ic: Option<Icon>, s: &str, col: u32| {
            let y = dr.y + k as f32 * lh;
            let mut x = dr.x;
            if let Some(i) = ic {
                icon(cv, &Rect::new(x, y, t.icon * 0.8, lh), i, t.icon * 0.7, col);
                x += t.icon;
            }
            f.text(cv, &Rect::new(x, y, dr.right() - x, lh), s, t.t_caption, col, Align::Left, 0.85);
        };
        match sel_idx {
            0 => {
                line(cv, f, 0, None, "The tablet is the keyboard and mouse of another computer over USB-C (typing, pad, quick actions).", c.text_dim);
                line(cv, f, 1, Some(Icon::Bolt), &v.usb_line, c.text);
                match v.usb_hint.as_str() {
                    "host" => line(cv, f, 2, Some(Icon::Close), "The port is in host role (a hub or a device is attached). Connect the tablet directly to the computer.", c.edit),
                    "none" => line(cv, f, 2, Some(Icon::Usb), "Nothing attached: plug a USB-C cable into the computer. Keys go to it as soon as it enumerates.", c.edit),
                    _ => {}
                }
            }
            1 => {
                line(cv, f, 0, None, "The tablet screen becomes a keyboard and touchpad for the external display.", c.text_dim);
                if v.monitor {
                    line(cv, f, 1, Some(Icon::Display), &format!("{} connected: pad mode starts at once; Exit or a 5-finger hold returns here", if v.monitor_name.is_empty() { "Monitor" } else { &v.monitor_name }), c.text);
                } else {
                    line(cv, f, 1, Some(Icon::Display), "No external display connected. Connect a monitor over USB-C, then choose this mode.", c.edit);
                }
            }
            _ => {
                line(cv, f, 0, None, "DexHid releases the touchscreen and keys go to this desktop; the tablet stays a normal (extended) screen.", c.text_dim);
                if v.monitor {
                    line(cv, f, 1, Some(Icon::Check), "No prompt for this monitor until it is plugged in again or another mode is chosen.", c.text);
                }
            }
        }
        if let Some(cr) = l.cards.get(3) {
            cv.shadow(cr, t.r_lg, t.s1, c.shadow / 2);
            cv.rrect(cr, t.r_lg, c.raised | 0xff00_0000);
            cv.rrect_ring(cr, t.r_lg, t.hair, c.hairline);
            let ib = Rect::new(cr.x + t.s4, cr.y + t.s4, t.t_label * 1.6, t.t_label * 1.6);
            cv.rrect(&ib, ib.h / 2.0, c.accent_soft);
            icon(cv, &ib, Icon::Keyboard, ib.h * 0.55, c.accent);
            f.text(cv, &Rect::new(ib.right() + t.s2, ib.y, cr.w * 0.6, ib.h), "Keyboard layout", t.t_label, c.text, Align::Left, 0.8);
            for (r, hit) in l.hits.iter().filter(|(_, x)| matches!(x, WinHit::Layout(_))) {
                let WinHit::Layout(i) = hit else { continue };
                let (name, title) = &v.layouts[*i as usize];
                let sel = *name == v.layout;
                button(cv, f, &t, &c, r, title, if sel { Some(Icon::Check) } else { None }, if sel { Tone::Selected } else { Tone::Tonal }, pressed == Some(*hit));
            }
        }
    } else {
        let n = l.rows.len();
        for (i, r) in l.rows.iter().enumerate() {
            let WinRow { title, value, kind, .. } = &v.rows[i];
            if r.bottom() < 0.0 || r.y > h {
                continue;
            }
            if i + 1 < n {
                cv.bar(&Rect::new(r.x, r.bottom() - t.hair, r.w, t.hair), c.hairline);
            }
            let idx = i as u8;
            let ctl_x = l.hits.iter().filter(|(_, x)| matches!(x, WinHit::RowToggle(j) | WinHit::RowChoice(j) | WinHit::RowMinus(j) | WinHit::RowPlus(j) if *j == idx)).map(|(r, _)| r.x).fold(r.right(), f32::min);
            f.text(cv, &Rect::new(r.x, r.y, ctl_x - r.x - t.s4, r.h), title, t.t_body, c.text, Align::Left, 0.85);
            match kind {
                Some(Kind::Bool) => {
                    if let Some(sr) = hit_of(WinHit::RowToggle(idx)) {
                        crate::look::switch(cv, &t, &c, &sr, value.eq_ignore_ascii_case("on") || value == "true");
                    }
                }
                Some(Kind::Num { .. }) => {
                    for hh in [WinHit::RowMinus(idx), WinHit::RowPlus(idx)] {
                        if let Some(br) = hit_of(hh) {
                            let d = br.h * 0.8;
                            let cr = Rect::new(br.x + (br.w - d) / 2.0, br.y + (br.h - d) / 2.0, d, d);
                            cv.rrect(&cr, d / 2.0, if pressed == Some(hh) { c.accent } else { c.key_mod | 0xff00_0000 });
                            icon(cv, &cr, if matches!(hh, WinHit::RowMinus(_)) { Icon::Minus } else { Icon::Plus }, d * 0.42, c.text);
                        }
                    }
                    if let (Some(a), Some(b)) = (hit_of(WinHit::RowMinus(idx)), hit_of(WinHit::RowPlus(idx))) {
                        f.text(cv, &Rect::new(a.right(), a.y, b.x - a.right(), a.h), value, t.t_body, c.text, Align::Center, 0.8);
                    }
                }
                _ => {
                    if let Some(vr) = hit_of(WinHit::RowChoice(idx)) {
                        cv.rrect(&vr, t.r_md, if pressed == Some(WinHit::RowChoice(idx)) { c.accent_soft } else { c.key_mod | 0xff00_0000 });
                        let isz = t.icon * 0.7;
                        f.text(cv, &Rect::new(vr.x + t.s3, vr.y, vr.w - t.s3 - isz - t.s4, vr.h), value, t.t_body, c.text, Align::Left, 0.75);
                        icon(cv, &Rect::new(vr.right() - isz - t.s3, vr.y, isz, vr.h), Icon::Chevron(1), isz, c.text_dim);
                    }
                }
            }
        }
    }
    l
}

/// the rects to repaint when the daemon's view changes (`l` = the layout of `old`); None = repaint everything
pub fn diff(old: &WinView, new: &WinView, l: &WinLayout) -> Option<Vec<Rect>> {
    if old.page != new.page || old.scroll != new.scroll || old.palette != new.palette || old.modules != new.modules || old.layouts != new.layouts {
        return None;
    }
    let mut v = vec![];
    if new.page == 0 {
        if (&old.mode, &old.mode_detail, old.monitor, &old.monitor_name, &old.usb_line, &old.usb_hint) != (&new.mode, &new.mode_detail, new.monitor, &new.monitor_name, &new.usb_line, &new.usb_hint) {
            v.push(l.header);
            v.extend(l.cards.first().copied());
        }
        if old.layout != new.layout {
            v.extend(l.cards.get(3).copied());
        }
    } else {
        if old.rows.len() != new.rows.len() || old.rows.len() != l.rows.len() {
            return None;
        }
        for (i, (a, b)) in old.rows.iter().zip(&new.rows).enumerate() {
            if a != b {
                v.push(l.rows[i]);
            }
        }
    }
    Some(v)
}

/// the debug render's view: modules + rows from the default config, layouts from the layout dirs
pub fn debug_view(page: &str, layouts: Vec<(String, String)>, monitor: bool) -> WinView {
    let cfg = dexhid_core::config::Config::default();
    let val = toml::Value::try_from(&cfg).unwrap_or(toml::Value::Table(Default::default()));
    let mods = dexhid_core::settings::MODULES;
    let module_rows = mods.iter().map(|m| dexhid_core::settings::rows(m.id, &val, &Default::default()).into_iter().map(|r| WinRow { key: r.key, title: r.title, value: r.value, kind: Some(r.kind) }).collect()).collect();
    let mut v = WinView {
        module_ids: mods.iter().map(|m| m.id.to_string()).collect(),
        modules: mods.iter().map(|m| m.title.to_string()).collect(),
        module_rows,
        mode: "screen".into(),
        mode_detail: if monitor { "DP-1 connected".into() } else { "no external display".into() },
        monitor,
        monitor_name: if monitor { "DP-1".into() } else { String::new() },
        usb_line: "USB waiting for a host  |  no USB power in  |  data role device".into(),
        layout: "full".into(),
        layouts,
        ..Default::default()
    };
    let p = v.page_of(page);
    v.select(p);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// no overflow at window sizes from a phone-like portrait to the full panel; targets >= 9 mm; hits inside
    #[test]
    fn window_layout_fits_sizes() {
        let lays: Vec<(String, String)> = ["full", "laptop", "tablet", "split", "numpad", "textedit", "remote"].iter().map(|s| (s.to_string(), s.to_string())).collect();
        for page in ["home", "pointer", "keyboard", "usbhid"] {
            let v = debug_view(page, lays.clone(), false);
            for (w, h, ppm) in [(1696.0, 1128.0, 6.19), (1200.0, 1624.0, 6.19), (900.0, 700.0, 6.19), (2261.0, 1500.0, 8.25), (700.0, 1200.0, 4.0)] {
                let l = layout(w, h, ppm, &v);
                for (r, hit) in &l.hits {
                    assert!(r.x >= -0.01 && r.right() <= w + 0.01, "{:?} {:?} inside the window width at {}x{} ({})", hit, r, w, h, page);
                    assert!(r.h >= 9.0 * ppm - 0.01 && r.w >= 9.0 * ppm - 0.01, "{:?} >= 9 mm at {}x{} ({})", hit, w, h, page);
                    if !matches!(hit, WinHit::Nav(_)) {
                        assert!(r.x >= l.content.x - 0.01, "{:?} right of the rail", hit);
                    }
                }
                for c in &l.cards {
                    assert!(c.right() <= w + 0.01, "card inside at {}x{}", w, h);
                }
                for i in 0..l.hits.len() {
                    for j in i + 1..l.hits.len() {
                        assert!(!l.hits[i].0.overlaps(&l.hits[j].0), "{:?} / {:?} overlap at {}x{} ({})", l.hits[i].1, l.hits[j].1, w, h, page);
                    }
                }
            }
        }
    }
}
