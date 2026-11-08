// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! M4 design prototype of the standalone app window (`dexhid debug render --window home|<module> --style m4`; not
//! wired to Wayland yet). A normal xdg_toplevel on the internal panel: works without an external monitor and
//! without grabbing the touchscreen (plain wl_touch / wl_pointer input). A navigation rail (Home + the settings
//! registry's modules) and a page: Home (mode status, Start touchpad mode, Start USB mode with the live power / role
//! line, the layout picker) or one settings module (rows from `settings::rows`). Laid out from constraints (tokens in
//! mm, content width capped, two columns only when they fit), every target >= 9 mm, text ellipsized.

use crate::look::{button, icon, Icon, Roles, Tokens, Tone};
use crate::render::{Align, Canvas, Fonts};
use dexhid_core::feedback::Palette;
use dexhid_core::geom::Rect;
use dexhid_core::settings::Kind;

/// what the window shows (built by the daemon from its state; the debug render fills it from defaults)
#[derive(Clone, Debug, Default)]
pub struct WinView {
    /// 0 = Home, i + 1 = module i
    pub page: usize,
    /// module titles (the settings registry order)
    pub modules: Vec<String>,
    /// the rows of the selected module: title, value, kind
    pub rows: Vec<(String, String, Option<Kind>)>,
    pub subtitle: String,
    /// "idle" | "pad" | "usb" and a one-line detail
    pub mode: String,
    pub mode_detail: String,
    /// an external monitor is connected (Start touchpad mode is possible)
    pub monitor: bool,
    /// the USB HID line (power in, data role, host state)
    pub usb_line: String,
    pub usb_ready: bool,
    /// (name, title) of the layouts, and the current one
    pub layouts: Vec<(String, String)>,
    pub layout: String,
    /// scroll offset of the page (logical px)
    pub scroll: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WinHit {
    Nav(u8),
    StartPad,
    StartUsb,
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
    if v.page == 0 {
        // two cards side by side when each gets >= 80 mm, else stacked
        let two = cw >= 2.0 * 80.0 * t.ppm + t.s4;
        let card_w = if two { (cw - t.s4) / 2.0 } else { cw };
        let card_h = t.s4 + t.t_label * 1.6 + t.t_caption * 1.5 * 2.0 + t.s3 + bh + t.s4;
        let a = Rect::new(cx, y, card_w, card_h);
        let b = if two { Rect::new(cx + card_w + t.s4, y, card_w, card_h) } else { Rect::new(cx, y + card_h + t.s4, card_w, card_h) };
        for (c, hit) in [(a, WinHit::StartPad), (b, WinHit::StartUsb)] {
            l.cards.push(c);
            let br = Rect::new(c.x + t.s4, c.bottom() - t.s4 - bh, (c.w - 2.0 * t.s4).min(70.0 * t.ppm), bh);
            l.hits.push((br, hit));
        }
        y = b.bottom() + t.s4;
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
    } else {
        let row_h = t.target.max(11.5 * t.ppm);
        for (i, (_, _, kind)) in v.rows.iter().enumerate() {
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
    let (title, sub) = if v.page == 0 { ("Home".to_string(), "Touchpad + keyboard for an external display, or a USB keyboard + mouse for another computer".to_string()) } else { (v.modules.get(v.page - 1).cloned().unwrap_or_default(), v.subtitle.clone()) };
    f.text(cv, &Rect::new(hd.x, hd.y, hd.w * 0.6, t.t_display * 1.5), &title, t.t_display, c.text, Align::Left, 0.7);
    if !sub.is_empty() {
        f.text(cv, &Rect::new(hd.x, hd.y + t.t_display * 1.5, hd.w, t.t_caption * 1.8), &sub, t.t_caption, c.text_dim, Align::Left, 0.85);
    }
    if v.page == 0 {
        // mode status pill (top right of the header)
        let (dot, label) = match v.mode.as_str() {
            "pad" => (c.accent, "Touchpad mode"),
            "usb" => (0xff3fb27f, "USB keyboard + mouse"),
            _ => (c.text_dim, "Idle"),
        };
        let text = format!("{}  \u{b7}  {}", label, v.mode_detail);
        let pw = (f.measure(&text, t.t_caption, cv.scale) + t.s8 + t.s3).min(hd.w * 0.5);
        let pr = Rect::new(hd.right() - pw, hd.y + t.s1, pw, t.ppm * 7.0);
        cv.rrect(&pr, pr.h / 2.0, c.raised | 0xff00_0000);
        cv.rrect_ring(&pr, pr.h / 2.0, t.hair, c.hairline);
        cv.disc(pr.x + t.s3 + t.ppm * 0.8, pr.y + pr.h / 2.0, t.ppm * 1.0, dot);
        f.text(cv, &Rect::new(pr.x + t.s3 + t.ppm * 2.6, pr.y, pr.w - t.s4 - t.ppm * 2.6, pr.h), &text, t.t_caption, c.text, Align::Left, 0.85);
        let cards: [(Icon, &str, String, String, WinHit, bool); 2] = [
            (
                Icon::Display,
                "Touchpad + keyboard",
                "The tablet screen becomes a touchpad and keyboard for the external display.".into(),
                if v.monitor { "Monitor connected: ready".into() } else { "Connect a monitor over USB-C to start".into() },
                WinHit::StartPad,
                v.monitor,
            ),
            (Icon::Usb, "USB keyboard + mouse", "The tablet types and points on another computer over USB-C.".into(), v.usb_line.clone(), WinHit::StartUsb, v.usb_ready),
        ];
        for (k, (ic, head, body, status, hit, enabled)) in cards.iter().enumerate() {
            let cr = l.cards[k];
            cv.shadow(&cr, t.r_lg, t.s1, c.shadow / 2);
            cv.rrect(&cr, t.r_lg, c.raised | 0xff00_0000);
            cv.rrect_ring(&cr, t.r_lg, t.hair, c.hairline);
            let ib = Rect::new(cr.x + t.s4, cr.y + t.s4, t.t_label * 1.6, t.t_label * 1.6);
            cv.rrect(&ib, ib.h / 2.0, c.accent_soft);
            icon(cv, &ib, *ic, ib.h * 0.55, c.accent);
            f.text(cv, &Rect::new(ib.right() + t.s2, ib.y, cr.w - ib.w - t.s8 - t.s2, ib.h), head, t.t_label, c.text, Align::Left, 0.8);
            let by = ib.bottom() + t.s1;
            f.text(cv, &Rect::new(cr.x + t.s4, by, cr.w - t.s8, t.t_caption * 1.5), body, t.t_caption, c.text_dim, Align::Left, 0.8);
            let scol = if *enabled { c.text } else { c.edit };
            let sr = Rect::new(cr.x + t.s4, by + t.t_caption * 1.5, cr.w - t.s8, t.t_caption * 1.5);
            if *ic == Icon::Usb {
                icon(cv, &Rect::new(sr.x, sr.y, t.icon * 0.8, sr.h), Icon::Bolt, t.icon * 0.7, c.text_dim);
                f.text(cv, &Rect::new(sr.x + t.icon, sr.y, sr.w - t.icon, sr.h), status, t.t_caption, scol, Align::Left, 0.8);
            } else {
                f.text(cv, &sr, status, t.t_caption, scol, Align::Left, 0.8);
            }
            if let Some(br) = hit_of(*hit) {
                let label = if *hit == WinHit::StartPad { "Start touchpad mode" } else { "Start USB mode" };
                if *enabled {
                    button(cv, f, &t, &c, &br, label, Some(Icon::Play), Tone::Filled, pressed == Some(*hit));
                } else {
                    button(cv, f, &t, &c, &br, label, Some(Icon::Play), Tone::Tonal, false);
                    cv.rrect(&br, t.r_md, (c.raised & 0x00ff_ffff) | 0x99 << 24);
                }
            }
        }
        if let Some(cr) = l.cards.get(2) {
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
            let (title, value, kind) = &v.rows[i];
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

/// the debug render's view: modules + rows from the default config, layouts from the layout dirs
pub fn debug_view(page: &str, layouts: Vec<(String, String)>, monitor: bool) -> WinView {
    let cfg = dexhid_core::config::Config::default();
    let val = toml::Value::try_from(&cfg).unwrap_or(toml::Value::Table(Default::default()));
    let mods = dexhid_core::settings::MODULES;
    let pi = if page == "home" { 0 } else { mods.iter().position(|m| m.id == page).map(|i| i + 1).unwrap_or(0) };
    let rows = if pi > 0 {
        dexhid_core::settings::rows(mods[pi - 1].id, &val, &Default::default()).into_iter().map(|r| (r.title, r.value, Some(r.kind))).collect()
    } else {
        vec![]
    };
    WinView {
        page: pi,
        modules: mods.iter().map(|m| m.title.to_string()).collect(),
        rows,
        subtitle: if pi > 0 { "Changes apply at once and are saved to settings.toml".into() } else { String::new() },
        mode: "idle".into(),
        mode_detail: if monitor { "DP-1 connected".into() } else { "no external display".into() },
        monitor,
        usb_line: "USB waiting for a host  |  no USB power in  |  data role device".into(),
        usb_ready: true,
        layout: "full".into(),
        layouts,
        scroll: 0.0,
    }
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
