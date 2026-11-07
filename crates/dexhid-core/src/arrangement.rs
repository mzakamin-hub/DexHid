// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! Where the regions (touchpad, keyboard, keyboard halves, numpad) sit on the panel: one arrangement per keyboard
//! layout and orientation, stored as fractions of the work area (the panel below the control strip) in
//! `$XDG_CONFIG_HOME/dexhid/arrangement-<layout>-<landscape|portrait>.toml`. Edit operations (move / resize with
//! snapping, minimum sizes, no overlap) work in logical pixels.

use crate::geom::Rect;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const TOUCHPAD: &str = "touchpad";

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
}

/// which handle of region `r` (px) a contact at (x, y) grabs: corners within `hs` px resize, the rest moves
pub fn hit_handle(r: &Rect, x: f32, y: f32, hs: f32) -> Option<Handle> {
    if !r.inset(-hs * 0.25).contains(x, y) {
        return None;
    }
    let hs = hs.min(r.w / 3.0).min(r.h / 3.0);
    let left = x < r.x + hs;
    let right = x >= r.right() - hs;
    let top = y < r.y + hs;
    let bottom = y >= r.bottom() - hs;
    Some(match (left, right, top, bottom) {
        (true, _, true, _) => Handle::TopLeft,
        (_, true, true, _) => Handle::TopRight,
        (true, _, _, true) => Handle::BottomLeft,
        (_, true, _, true) => Handle::BottomRight,
        _ => Handle::Move,
    })
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
            let movl = matches!(h, Handle::TopLeft | Handle::BottomLeft);
            let movt = matches!(h, Handle::TopLeft | Handle::TopRight);
            if movl {
                l = (l + dx).clamp(a.x, rr - mw);
                l += snap_to(l, &tx, c.snap).unwrap_or(0.0);
                l = l.clamp(a.x, rr - mw);
            } else {
                rr = (rr + dx).clamp(l + mw, a.right());
                rr += snap_to(rr, &tx, c.snap).unwrap_or(0.0);
                rr = rr.clamp(l + mw, a.right());
            }
            if movt {
                t = (t + dy).clamp(a.y, b - mh);
                t += snap_to(t, &ty, c.snap).unwrap_or(0.0);
                t = t.clamp(a.y, b - mh);
            } else {
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
