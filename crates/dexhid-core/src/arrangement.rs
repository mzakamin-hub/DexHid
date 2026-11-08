// SPDX-License-Identifier: GPL-3.0-or-later
//! Where the regions (touchpad, keyboard, keyboard halves, numpad) sit on the panel: one arrangement per keyboard
//! layout and orientation, stored as fractions of the work area (the panel below the control strip) in
//! `$XDG_CONFIG_HOME/dexhid/arrangement-<layout>-<landscape|portrait>.toml`. Edit operations (move / resize with
//! snapping, minimum sizes, no overlap) work in logical pixels.

use crate::geom::Rect;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const TOUCHPAD: &str = "touchpad";
/// the quick-actions bar (M2)
pub const QUICK: &str = "quick";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orientation {
    Landscape,
    Portrait,
}

impl Orientation {
    pub fn name(&self) -> &'static str {
        match self {
            Orientation::Landscape => "landscape",
            Orientation::Portrait => "portrait",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RegionPlace {
    pub id: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl RegionPlace {
    pub fn rect(&self) -> Rect {
        Rect::new(self.x, self.y, self.w, self.h)
    }
}

fn v1() -> u32 {
    1
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Arrangement {
    #[serde(default = "v1")]
    pub version: u32,
    #[serde(default)]
    pub layout: String,
    #[serde(default)]
    pub orientation: String,
    #[serde(rename = "region", default)]
    pub regions: Vec<RegionPlace>,
}

pub fn file_name(layout: &str, o: Orientation) -> String {
    format!("arrangement-{}-{}.toml", layout, o.name())
}

/// laptop key pitch (mm): 19.05 mm = the common laptop / desktop key spacing
pub const LAPTOP_PITCH_MM: f32 = 19.05;
/// the laptop grid is at least 15 key units wide
pub const LAPTOP_UNITS: f32 = 15.0;

/// M2.1 "laptop" preset (fractions of `area`): square keys of one pitch (at most LAPTOP_PITCH_MM, and so that a
/// grid of at least 15 units fits the width), the keyboard (+ numpad half a unit to its right) centred under the
/// strip instead of stretched, at most 62 % of the height; the touchpad below, centred, `pad_pct` % of the
/// keyboard width; the quick bar in the free column left of a narrower pad, else as a row above the pad.
/// `kb` / `np` = (width, height) in key units.
pub fn laptop(area: &Rect, ppm: f32, kb: (f32, f32), np: Option<(f32, f32)>, quick: bool, pad_pct: f32) -> Arrangement {
    let m = 2.0 * ppm;
    let total_u = kb.0 + np.map(|n| n.0 + 0.5).unwrap_or(0.0);
    let mut pitch = (LAPTOP_PITCH_MM * ppm).min((area.w - 2.0 * m) / total_u.max(LAPTOP_UNITS));
    let rows = kb.1.max(np.map(|n| n.1).unwrap_or(0.0)).max(1.0);
    if rows * pitch > area.h * 0.62 {
        pitch = area.h * 0.62 / rows;
    }
    let kb_w = kb.0 * pitch;
    let kb_h = kb.1 * pitch;
    let x0 = area.x + (area.w - total_u * pitch) / 2.0;
    let y0 = area.y + m;
    let mut v: Vec<(&str, Rect)> = vec![("keyboard", Rect::new(x0, y0, kb_w, kb_h))];
    if let Some(n) = np {
        v.push(("numpad", Rect::new(x0 + kb_w + 0.5 * pitch, y0, n.0 * pitch, n.1 * pitch)));
    }
    let py = y0 + rows * pitch + 2.0 * m;
    let pw = kb_w * pad_pct.clamp(20.0, 100.0) / 100.0;
    let mut pad = Rect::new(x0 + (kb_w - pw) / 2.0, py, pw, (area.bottom() - m - py).max(10.0 * ppm));
    if quick {
        let col = pad.x - x0 - m;
        if col >= 14.0 * ppm {
            v.push((QUICK, Rect::new(x0, pad.y, col, pad.h)));
        } else {
            let qh = 9.0 * ppm;
            v.push((QUICK, Rect::new(pad.x, pad.y, pad.w, qh)));
            pad.y += qh + m;
            pad.h -= qh + m;
        }
    }
    v.push((TOUCHPAD, pad));
    Arrangement {
        version: 1,
        layout: String::new(),
        orientation: String::new(),
        regions: v.into_iter().map(|(id, r)| {
            let f = r.to_frac(area);
            RegionPlace { id: id.into(), x: f.x, y: f.y, w: f.w, h: f.h }
        }).collect(),
    }
}

impl Arrangement {
    /// default placement for the given keyboard regions (the touchpad is always present)
    pub fn default_for(layout: &str, kb_regions: &[String], o: Orientation) -> Arrangement {
        let has = |r: &str| kb_regions.iter().any(|x| x == r);
        let land = o == Orientation::Landscape;
        let mut v: Vec<(&str, Rect)> = vec![];
        if has("keyboard_left") || has("keyboard_right") {
            if land {
                v.push(("keyboard_left", Rect::new(0.0, 0.3, 0.3, 0.7)));
                v.push(("keyboard_right", Rect::new(0.64, 0.3, 0.36, 0.7)));
                v.push((TOUCHPAD, Rect::new(0.31, 0.0, 0.32, 1.0)));
            } else {
                v.push(("keyboard_left", Rect::new(0.0, 0.58, 0.46, 0.42)));
                v.push(("keyboard_right", Rect::new(0.48, 0.58, 0.52, 0.42)));
                v.push((TOUCHPAD, Rect::new(0.0, 0.0, 1.0, 0.56)));
            }
        } else if has("keyboard") {
            let np = has("numpad");
            if land {
                v.push(("keyboard", Rect::new(0.0, 0.0, if np { 0.79 } else { 1.0 }, 0.56)));
                if np {
                    v.push(("numpad", Rect::new(0.8, 0.0, 0.2, 0.56)));
                }
                v.push((TOUCHPAD, Rect::new(0.18, 0.58, 0.64, 0.42)));
            } else {
                v.push((TOUCHPAD, Rect::new(0.0, 0.0, 1.0, if np { 0.4 } else { 0.56 })));
                v.push(("keyboard", Rect::new(0.0, if np { 0.42 } else { 0.58 }, 1.0, if np { 0.36 } else { 0.42 })));
                if np {
                    v.push(("numpad", Rect::new(0.55, 0.8, 0.45, 0.2)));
                }
            }
        } else {
            v.push((TOUCHPAD, Rect::new(0.0, 0.0, 1.0, 1.0)));
        }
        // the quick-actions bar (M2): carved out of free space next to / above the touchpad
        if has(QUICK) {
            let kb_any = has("keyboard");
            let split = has("keyboard_left") || has("keyboard_right");
            let q = if land && kb_any {
                // the touchpad row leaves 0.18 free on each side: the bar takes the left column
                Rect::new(0.0, 0.58, 0.16, 0.42)
            } else {
                // top strip of the touchpad: shrink the touchpad below it
                let tp = v.iter_mut().find(|(id, _)| *id == TOUCHPAD).map(|(_, r)| r).unwrap();
                let qh = if split || !kb_any { 0.12 } else { 0.08 };
                let q = Rect::new(tp.x, tp.y, tp.w, qh);
                tp.y += qh + 0.02;
                tp.h -= qh + 0.02;
                q
            };
            v.push((QUICK, q));
        }
        // regions a custom layout adds that the presets above do not know: stack them at the bottom
        let extra: Vec<&String> = kb_regions.iter().filter(|r| !v.iter().any(|(id, _)| id == r)).collect();
        if !extra.is_empty() {
            for (_, r) in v.iter_mut() {
                r.y *= 0.78;
                r.h *= 0.78;
            }
        }
        for (i, r) in extra.iter().enumerate() {
            let w = 1.0 / extra.len() as f32;
            v.push((r.as_str(), Rect::new(i as f32 * w, 0.8, w, 0.2)));
        }
        Arrangement {
            version: 1,
            layout: layout.into(),
            orientation: o.name().into(),
            regions: v.into_iter().map(|(id, r)| RegionPlace { id: id.into(), x: r.x, y: r.y, w: r.w, h: r.h }).collect(),
        }
    }

    pub fn get(&self, id: &str) -> Option<Rect> {
        self.regions.iter().find(|r| r.id == id).map(|r| r.rect())
    }

    pub fn set(&mut self, id: &str, f: Rect) {
        if let Some(r) = self.regions.iter_mut().find(|r| r.id == id) {
            r.x = f.x;
            r.y = f.y;
            r.w = f.w;
            r.h = f.h;
        } else {
            self.regions.push(RegionPlace { id: id.into(), x: f.x, y: f.y, w: f.w, h: f.h });
        }
    }

    /// Make a loaded arrangement usable for `required` regions in `area` (px) with minimum sizes `mins` (px):
    /// drop unknown regions, add missing ones from `defaults`, clamp into the area, grow to the minimum size.
    /// If regions overlap afterwards, fall back to `defaults` entirely. Returns notes for the log.
    pub fn fixup(&mut self, required: &[String], defaults: &Arrangement, area: &Rect, mins: &HashMap<String, (f32, f32)>) -> Vec<String> {
        let mut notes = vec![];
        self.regions.retain(|r| {
            let keep = required.contains(&r.id);
            if !keep {
                notes.push(format!("dropped unknown region {}", r.id));
            }
            keep
        });
        for id in required {
            if self.get(id).is_none() {
                if let Some(d) = defaults.get(id) {
                    self.set(id, d);
                    notes.push(format!("added missing region {}", id));
                }
            }
        }
        let ids: Vec<String> = self.regions.iter().map(|r| r.id.clone()).collect();
        for id in &ids {
            let f = self.get(id).unwrap();
            let px = Rect::from_frac(&f, area);
            let (mw, mh) = mins.get(id).copied().unwrap_or((1.0, 1.0));
            let fixed = clamp_into(&Rect::new(px.x, px.y, px.w.max(mw.min(area.w)), px.h.max(mh.min(area.h))), area);
            let moved = (fixed.x - px.x).abs() > 0.5 || (fixed.y - px.y).abs() > 0.5 || (fixed.w - px.w).abs() > 0.5 || (fixed.h - px.h).abs() > 0.5;
            if moved || !f.x.is_finite() {
                notes.push(format!("region {} clamped / grown to the minimum size", id));
            }
            self.set(id, fixed.to_frac(area));
        }
        let rects: Vec<Rect> = self.regions.iter().map(|r| r.rect()).collect();
        let overlap = (0..rects.len()).any(|i| (i + 1..rects.len()).any(|j| rects[i].overlaps(&rects[j])));
        if overlap {
            notes.push("regions overlap: using the default arrangement".into());
            let mut d = defaults.clone();
            d.regions.retain(|r| required.contains(&r.id));
            self.regions = d.regions;
        }
        notes
    }

    pub fn load(path: &std::path::Path) -> Result<Option<Arrangement>, String> {
        match std::fs::read_to_string(path) {
            Ok(t) => toml::from_str::<Arrangement>(&t).map(Some).map_err(|e| format!("{}: {}", path.display(), e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {}", path.display(), e)),
        }
    }

    /// atomic save (tmp file + rename)
    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d).map_err(|e| format!("{}: {}", d.display(), e))?;
        }
        let mut t = String::from("# DexHid region arrangement (fractions of the area below the control strip). Written by dexhid.\n");
        t.push_str(&toml::to_string(self).map_err(|e| e.to_string())?);
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, t).map_err(|e| format!("{}: {}", tmp.display(), e))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {}", path.display(), e))
    }
}

pub fn clamp_into(r: &Rect, area: &Rect) -> Rect {
    let w = r.w.min(area.w);
    let h = r.h.min(area.h);
    let x = r.x.clamp(area.x, area.right() - w);
    let y = r.y.clamp(area.y, area.bottom() - h);
    Rect::new(x, y, w, h)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    Move,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    /// edge handles (M2): resize one side only
    Left,
    Right,
    Top,
    Bottom,
}

impl Handle {
    /// (moves left, moves top, moves right, moves bottom)
    fn sides(&self) -> (bool, bool, bool, bool) {
        match self {
            Handle::Move => (true, true, true, true),
            Handle::TopLeft => (true, true, false, false),
            Handle::TopRight => (false, true, true, false),
            Handle::BottomLeft => (true, false, false, true),
            Handle::BottomRight => (false, false, true, true),
            Handle::Left => (true, false, false, false),
            Handle::Right => (false, false, true, false),
            Handle::Top => (false, true, false, false),
            Handle::Bottom => (false, false, false, true),
        }
    }
}

/// Which handle of region `r` (px) a contact at (x, y) grabs: corners within `hs` px resize two sides, a band of
/// `hs * 0.6` along each edge (away from the corners) resizes that side, the rest moves. The hit area reaches
/// `hs / 4` outside the region so a corner at the panel edge stays easy to hit.
pub fn hit_handle(r: &Rect, x: f32, y: f32, hs: f32) -> Option<Handle> {
    if !r.inset(-hs * 0.25).contains(x, y) {
        return None;
    }
    let hs = hs.min(r.w / 3.0).min(r.h / 3.0);
    let left = x < r.x + hs;
    let right = x >= r.right() - hs;
    let top = y < r.y + hs;
    let bottom = y >= r.bottom() - hs;
    let e = hs * 0.6;
    Some(match (left, right, top, bottom) {
        (true, _, true, _) => Handle::TopLeft,
        (_, true, true, _) => Handle::TopRight,
        (true, _, _, true) => Handle::BottomLeft,
        (_, true, _, true) => Handle::BottomRight,
        _ if x < r.x + e => Handle::Left,
        _ if x >= r.right() - e => Handle::Right,
        _ if y < r.y + e => Handle::Top,
        _ if y >= r.bottom() - e => Handle::Bottom,
        _ => Handle::Move,
    })
}

/// A snap guide to draw while dragging: a vertical (`vertical`) or horizontal line at `pos` from `from` to `to`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Guide {
    pub vertical: bool,
    pub pos: f32,
    pub from: f32,
    pub to: f32,
}

/// The guides for a dragged rect: every edge of `r` that lies on a work-area edge or on an edge of another region
/// (within half a pixel), drawn across both the rect and the target.
pub fn guides(r: &Rect, c: &EditCtx) -> Vec<Guide> {
    let mut v = vec![];
    let a = &c.area;
    let mut tx: Vec<(f32, f32, f32)> = vec![(a.x, a.y, a.bottom()), (a.right(), a.y, a.bottom())];
    let mut ty: Vec<(f32, f32, f32)> = vec![(a.y, a.x, a.right()), (a.bottom(), a.x, a.right())];
    for o in c.others {
        tx.push((o.x, o.y, o.bottom()));
        tx.push((o.right(), o.y, o.bottom()));
        ty.push((o.y, o.x, o.right()));
        ty.push((o.bottom(), o.x, o.right()));
    }
    for e in [r.x, r.right()] {
        if let Some(&(p, f, t)) = tx.iter().find(|(p, _, _)| (p - e).abs() < 0.5) {
            v.push(Guide { vertical: true, pos: p, from: f.min(r.y), to: t.max(r.bottom()) });
        }
    }
    for e in [r.y, r.bottom()] {
        if let Some(&(p, f, t)) = ty.iter().find(|(p, _, _)| (p - e).abs() < 0.5) {
            v.push(Guide { vertical: false, pos: p, from: f.min(r.x), to: t.max(r.right()) });
        }
    }
    v
}

pub struct EditCtx<'a> {
    /// work area (px)
    pub area: Rect,
    /// snap distance (px)
    pub snap: f32,
    /// minimum size of the dragged region (px)
    pub min: (f32, f32),
    /// the other regions (px)
    pub others: &'a [Rect],
}

fn snap_to(v: f32, targets: &[f32], thr: f32) -> Option<f32> {
    targets.iter().map(|t| t - v).filter(|d| d.abs() <= thr).min_by(|a, b| a.abs().total_cmp(&b.abs()))
}

/// The new rect (px) of a region dragged from `orig` by (dx, dy) with handle `h`.
/// Snaps the moving edges to the work-area edges and to the other regions' edges, keeps the minimum size,
/// stays inside the area and never overlaps another region (an axis that would overlap keeps its old value).
pub fn drag(orig: &Rect, h: Handle, dx: f32, dy: f32, c: &EditCtx) -> Rect {
    let a = &c.area;
    let tx: Vec<f32> = [a.x, a.right()].into_iter().chain(c.others.iter().flat_map(|o| [o.x, o.right()])).collect();
    let ty: Vec<f32> = [a.y, a.bottom()].into_iter().chain(c.others.iter().flat_map(|o| [o.y, o.bottom()])).collect();
    let (mw, mh) = (c.min.0.min(a.w), c.min.1.min(a.h));
    let cand = match h {
        Handle::Move => {
            let mut r = clamp_into(&Rect::new(orig.x + dx, orig.y + dy, orig.w, orig.h), a);
            let sx = [snap_to(r.x, &tx, c.snap), snap_to(r.right(), &tx, c.snap)]
                .into_iter()
                .flatten()
                .min_by(|p, q| p.abs().total_cmp(&q.abs()));
            let sy = [snap_to(r.y, &ty, c.snap), snap_to(r.bottom(), &ty, c.snap)]
                .into_iter()
                .flatten()
                .min_by(|p, q| p.abs().total_cmp(&q.abs()));
            r.x += sx.unwrap_or(0.0);
            r.y += sy.unwrap_or(0.0);
            clamp_into(&r, a)
        }
        _ => {
            let (mut l, mut t, mut rr, mut b) = (orig.x, orig.y, orig.right(), orig.bottom());
            let (movl, movt, movr, movb) = h.sides();
            if movl {
                l = (l + dx).clamp(a.x, rr - mw);
                l += snap_to(l, &tx, c.snap).unwrap_or(0.0);
                l = l.clamp(a.x, rr - mw);
            } else if movr {
                rr = (rr + dx).clamp(l + mw, a.right());
                rr += snap_to(rr, &tx, c.snap).unwrap_or(0.0);
                rr = rr.clamp(l + mw, a.right());
            }
            if movt {
                t = (t + dy).clamp(a.y, b - mh);
                t += snap_to(t, &ty, c.snap).unwrap_or(0.0);
                t = t.clamp(a.y, b - mh);
            } else if movb {
                b = (b + dy).clamp(t + mh, a.bottom());
                b += snap_to(b, &ty, c.snap).unwrap_or(0.0);
                b = b.clamp(t + mh, a.bottom());
            }
            Rect::new(l, t, rr - l, b - t)
        }
    };
    let free = |r: &Rect| !c.others.iter().any(|o| o.overlaps(r));
    if free(&cand) {
        return cand;
    }
    // keep the axis that collides
    let only_x = Rect::new(cand.x, orig.y, cand.w, orig.h);
    if free(&only_x) {
        return only_x;
    }
    let only_y = Rect::new(orig.x, cand.y, orig.w, cand.h);
    if free(&only_y) {
        return only_y;
    }
    *orig
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn defaults_do_not_overlap_and_fit() {
        for regs in [s(&[]), s(&["keyboard"]), s(&["keyboard", "numpad"]), s(&["keyboard_left", "keyboard_right"]), s(&["keyboard", "extra1"])] {
            for o in [Orientation::Landscape, Orientation::Portrait] {
                let a = Arrangement::default_for("x", &regs, o);
                assert!(a.get(TOUCHPAD).is_some());
                for r in &regs {
                    assert!(a.get(r).is_some(), "{:?} {:?}", r, o);
                }
                let rects: Vec<Rect> = a.regions.iter().map(|r| r.rect()).collect();
                for (i, r) in rects.iter().enumerate() {
                    assert!(r.x >= 0.0 && r.y >= 0.0 && r.right() <= 1.0001 && r.bottom() <= 1.0001, "{:?}", r);
                    for q in &rects[i + 1..] {
                        assert!(!r.overlaps(q), "{:?} {:?} {:?} {:?}", regs, o, r, q);
                    }
                }
            }
        }
    }

    #[test]
    fn move_snaps_and_clamps() {
        let area = Rect::new(0.0, 70.0, 1696.0, 1130.0);
        let orig = Rect::new(300.0, 700.0, 1000.0, 400.0);
        let c = EditCtx { area, snap: 20.0, min: (250.0, 180.0), others: &[] };
        // 15 px from the left edge after the move: snaps to it
        let r = drag(&orig, Handle::Move, -285.0, 0.0, &c);
        assert_eq!(r.x, 0.0);
        // far beyond the bottom: clamped
        let r = drag(&orig, Handle::Move, 0.0, 900.0, &c);
        assert_eq!(r.bottom(), area.bottom());
        // ordinary move, no snap
        let r = drag(&orig, Handle::Move, 100.0, -100.0, &c);
        assert_eq!((r.x, r.y), (400.0, 600.0));
    }

    #[test]
    fn resize_respects_min_and_snaps_to_neighbours() {
        let area = Rect::new(0.0, 70.0, 1696.0, 1130.0);
        let kb = Rect::new(0.0, 70.0, 1696.0, 600.0);
        let orig = Rect::new(300.0, 700.0, 1000.0, 400.0);
        let others = [kb];
        let c = EditCtx { area, snap: 20.0, min: (250.0, 180.0), others: &others };
        // shrink far past the minimum
        let r = drag(&orig, Handle::BottomRight, -2000.0, -2000.0, &c);
        assert_eq!((r.w, r.h), (250.0, 180.0));
        assert_eq!((r.x, r.y), (300.0, 700.0));
        // top edge dragged to 12 px below the keyboard: snaps onto the keyboard's bottom edge (touching, no overlap)
        let r = drag(&orig, Handle::TopLeft, 0.0, -18.0, &c);
        assert_eq!(r.y, 670.0);
        // dragged into the keyboard: the y axis keeps its old value
        let r = drag(&orig, Handle::TopLeft, -50.0, -300.0, &c);
        assert_eq!(r.y, 700.0);
        assert_eq!(r.x, 250.0);
    }

    #[test]
    fn move_never_overlaps() {
        let area = Rect::new(0.0, 0.0, 1000.0, 1000.0);
        let other = Rect::new(0.0, 0.0, 1000.0, 400.0);
        let orig = Rect::new(100.0, 500.0, 300.0, 300.0);
        let others = [other];
        let c = EditCtx { area, snap: 10.0, min: (100.0, 100.0), others: &others };
        let r = drag(&orig, Handle::Move, 50.0, -300.0, &c);
        assert!(!r.overlaps(&other));
        assert_eq!(r.x, 150.0);
        assert_eq!(r.y, 500.0);
    }

    #[test]
    fn handles() {
        let r = Rect::new(100.0, 100.0, 300.0, 300.0);
        assert_eq!(hit_handle(&r, 105.0, 105.0, 40.0), Some(Handle::TopLeft));
        assert_eq!(hit_handle(&r, 395.0, 395.0, 40.0), Some(Handle::BottomRight));
        assert_eq!(hit_handle(&r, 250.0, 250.0, 40.0), Some(Handle::Move));
        assert_eq!(hit_handle(&r, 50.0, 50.0, 40.0), None);
        // edge bands (24 px = 0.6 * 40) away from the corners
        assert_eq!(hit_handle(&r, 110.0, 250.0, 40.0), Some(Handle::Left));
        assert_eq!(hit_handle(&r, 390.0, 250.0, 40.0), Some(Handle::Right));
        assert_eq!(hit_handle(&r, 250.0, 105.0, 40.0), Some(Handle::Top));
        assert_eq!(hit_handle(&r, 250.0, 398.0, 40.0), Some(Handle::Bottom));
        assert_eq!(hit_handle(&r, 130.0, 250.0, 40.0), Some(Handle::Move));
    }

    #[test]
    fn edge_resize_moves_one_side_and_guides() {
        let area = Rect::new(0.0, 70.0, 1696.0, 1130.0);
        let kb = Rect::new(0.0, 70.0, 1696.0, 600.0);
        let orig = Rect::new(300.0, 700.0, 1000.0, 400.0);
        let others = [kb];
        let c = EditCtx { area, snap: 20.0, min: (250.0, 180.0), others: &others };
        let r = drag(&orig, Handle::Right, -200.0, 50.0, &c);
        assert_eq!(r, Rect::new(300.0, 700.0, 800.0, 400.0), "only the right side moves, dy ignored");
        let r = drag(&orig, Handle::Top, 40.0, -20.0, &c);
        assert_eq!(r, Rect::new(300.0, 670.0, 1000.0, 430.0), "top snaps onto the keyboard's bottom edge");
        let g = guides(&r, &c);
        assert!(g.iter().any(|g| !g.vertical && g.pos == 670.0), "{:?}", g);
        let r = drag(&orig, Handle::Bottom, 0.0, 500.0, &c);
        assert_eq!(r.bottom(), area.bottom());
        assert!(guides(&r, &c).iter().any(|g| !g.vertical && g.pos == area.bottom()));
    }

    #[test]
    fn quick_bar_defaults() {
        for regs in [s(&["keyboard", QUICK]), s(&["keyboard", "numpad", QUICK]), s(&["keyboard_left", "keyboard_right", QUICK]), s(&[QUICK])] {
            for o in [Orientation::Landscape, Orientation::Portrait] {
                let a = Arrangement::default_for("x", &regs, o);
                let q = a.get(QUICK).expect("quick placed");
                assert!(q.w > 0.05 && q.h > 0.05);
                let rects: Vec<Rect> = a.regions.iter().map(|r| r.rect()).collect();
                for (i, r) in rects.iter().enumerate() {
                    assert!(r.x >= 0.0 && r.y >= 0.0 && r.right() <= 1.0001 && r.bottom() <= 1.0001, "{:?}", r);
                    for q in &rects[i + 1..] {
                        assert!(!r.overlaps(q), "{:?} {:?} {:?} {:?}", regs, o, r, q);
                    }
                }
            }
        }
    }

    #[test]
    fn fixup_and_persist() {
        let area = Rect::new(0.0, 70.0, 1696.0, 1130.0);
        let req = s(&[TOUCHPAD, "keyboard"]);
        let d = Arrangement::default_for("full", &s(&["keyboard"]), Orientation::Landscape);
        let mut mins = HashMap::new();
        mins.insert(TOUCHPAD.to_string(), (250.0, 180.0));
        let mut a = d.clone();
        a.regions.push(RegionPlace { id: "bogus".into(), x: 0.0, y: 0.0, w: 0.1, h: 0.1 });
        a.set(TOUCHPAD, Rect::new(0.9, 0.9, 0.01, 0.01));
        let notes = a.fixup(&req, &d, &area, &mins);
        assert!(notes.iter().any(|n| n.contains("bogus")));
        let tp = Rect::from_frac(&a.get(TOUCHPAD).unwrap(), &area);
        assert!(tp.w >= 249.9 && tp.h >= 179.9 && tp.right() <= area.right() + 0.01 && tp.bottom() <= area.bottom() + 0.01);
        // overlapping -> defaults
        let mut b = d.clone();
        b.set(TOUCHPAD, Rect::new(0.0, 0.0, 0.5, 0.5));
        let notes = b.fixup(&req, &d, &area, &mins);
        assert!(notes.iter().any(|n| n.contains("overlap")));
        assert_eq!(b.get(TOUCHPAD), d.get(TOUCHPAD));
        // save / load round trip
        let p = std::env::temp_dir().join(format!("dexhid-arr-{}/{}", std::process::id(), file_name("full", Orientation::Landscape)));
        a.save(&p).unwrap();
        let back = Arrangement::load(&p).unwrap().unwrap();
        assert_eq!(back, a);
        assert!(Arrangement::load(&p.with_file_name("nope.toml")).unwrap().is_none());
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }
}
