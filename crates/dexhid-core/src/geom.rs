// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL: licence not decided by the owner yet)
//! Rectangles and the panel-native <-> logical (output) coordinate transforms.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn center(&self) -> (f32, f32) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }
    /// true when the two rects share area (touching edges do not count)
    pub fn overlaps(&self, o: &Rect) -> bool {
        const E: f32 = 1e-3;
        self.x < o.right() - E && o.x < self.right() - E && self.y < o.bottom() - E && o.y < self.bottom() - E
    }
    pub fn inset(&self, d: f32) -> Rect {
        Rect::new(self.x + d, self.y + d, (self.w - 2.0 * d).max(0.0), (self.h - 2.0 * d).max(0.0))
    }
    /// a rect given in fractions of `area` -> absolute
    pub fn from_frac(f: &Rect, area: &Rect) -> Rect {
        Rect::new(area.x + f.x * area.w, area.y + f.y * area.h, f.w * area.w, f.h * area.h)
    }
    /// absolute -> fractions of `area`
    pub fn to_frac(&self, area: &Rect) -> Rect {
        let w = area.w.max(1e-6);
        let h = area.h.max(1e-6);
        Rect::new((self.x - area.x) / w, (self.y - area.y) / h, self.w / w, self.h / h)
    }
    pub fn round(&self) -> (i32, i32, i32, i32) {
        let x0 = self.x.round() as i32;
        let y0 = self.y.round() as i32;
        let x1 = self.right().round() as i32;
        let y1 = self.bottom().round() as i32;
        (x0, y0, (x1 - x0).max(0), (y1 - y0).max(0))
    }
}

/// wl_output.transform values (0..7)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Transform {
    #[default]
    Normal,
    R90,
    R180,
    R270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

impl Transform {
    pub fn from_wl(v: u32) -> Transform {
        match v {
            1 => Transform::R90,
            2 => Transform::R180,
            3 => Transform::R270,
            4 => Transform::Flipped,
            5 => Transform::Flipped90,
            6 => Transform::Flipped180,
            7 => Transform::Flipped270,
            _ => Transform::Normal,
        }
    }
    pub fn from_name(s: &str) -> Option<Transform> {
        Some(match s {
            "normal" | "0" => Transform::Normal,
            "90" => Transform::R90,
            "180" => Transform::R180,
            "270" => Transform::R270,
            "flipped" => Transform::Flipped,
            "flipped-90" => Transform::Flipped90,
            "flipped-180" => Transform::Flipped180,
            "flipped-270" => Transform::Flipped270,
            _ => return None,
        })
    }
    pub fn name(&self) -> &'static str {
        match self {
            Transform::Normal => "normal",
            Transform::R90 => "90",
            Transform::R180 => "180",
            Transform::R270 => "270",
            Transform::Flipped => "flipped",
            Transform::Flipped90 => "flipped-90",
            Transform::Flipped180 => "flipped-180",
            Transform::Flipped270 => "flipped-270",
        }
    }
    /// logical width/height are the native height/width
    pub fn swaps_axes(&self) -> bool {
        matches!(self, Transform::R90 | Transform::R270 | Transform::Flipped90 | Transform::Flipped270)
    }
    fn rot(&self) -> u32 {
        match self {
            Transform::Normal | Transform::Flipped => 0,
            Transform::R90 | Transform::Flipped90 => 90,
            Transform::R180 | Transform::Flipped180 => 180,
            Transform::R270 | Transform::Flipped270 => 270,
        }
    }
    fn flipped(&self) -> bool {
        matches!(self, Transform::Flipped | Transform::Flipped90 | Transform::Flipped180 | Transform::Flipped270)
    }
}

/// Panel-native normalized touch point (u, v in 0..1, the touch controller's axes, which match the panel's native
/// scan-out mode) -> logical normalized point (x, y in 0..1 of the output as the user sees it).
/// Same convention as KWin's touch mapping (Rotate90: logical = (H - dy, dx) in mode pixels). The 90 / 270 direction is
/// still to be confirmed on a rotated tablet (I); `touch_transform` in the config overrides the output's transform.
pub fn native_to_logical(t: Transform, u: f32, v: f32) -> (f32, f32) {
    let (x, y) = match t.rot() {
        90 => (1.0 - v, u),
        180 => (1.0 - u, 1.0 - v),
        270 => (v, 1.0 - u),
        _ => (u, v),
    };
    if t.flipped() {
        (1.0 - x, y)
    } else {
        (x, y)
    }
}

/// inverse of [`native_to_logical`]
pub fn logical_to_native(t: Transform, x: f32, y: f32) -> (f32, f32) {
    let (x, y) = if t.flipped() { (1.0 - x, y) } else { (x, y) };
    match t.rot() {
        90 => (y, 1.0 - x),
        180 => (1.0 - x, 1.0 - y),
        270 => (1.0 - y, x),
        _ => (x, y),
    }
}

/// Everything needed to go from a raw touch sample to logical pixels and millimetres.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    /// logical surface size (px)
    pub w: f32,
    pub h: f32,
    pub transform: Transform,
    /// panel size in mm in its native orientation
    pub native_mm: (f32, f32),
    /// touch axis maxima (native units, inclusive)
    pub tmax: (i32, i32),
}

impl Geometry {
    pub fn logical_mm(&self) -> (f32, f32) {
        if self.transform.swaps_axes() {
            (self.native_mm.1, self.native_mm.0)
        } else {
            self.native_mm
        }
    }
    /// logical px per mm (average of both axes)
    pub fn px_per_mm(&self) -> f32 {
        let (wm, hm) = self.logical_mm();
        ((self.w / wm.max(1.0)) + (self.h / hm.max(1.0))) / 2.0
    }
    pub fn landscape(&self) -> bool {
        self.w >= self.h
    }
    pub fn touch_to_logical(&self, nx: i32, ny: i32) -> (f32, f32) {
        let u = nx as f32 / (self.tmax.0.max(1) as f32 + 1.0);
        let v = ny as f32 / (self.tmax.1.max(1) as f32 + 1.0);
        let (x, y) = native_to_logical(self.transform, u, v);
        (x * self.w, y * self.h)
    }
    pub fn logical_to_touch(&self, x: f32, y: f32) -> (i32, i32) {
        let (u, v) = logical_to_native(self.transform, x / self.w.max(1.0), y / self.h.max(1.0));
        ((u * (self.tmax.0 as f32 + 1.0)).round() as i32, (v * (self.tmax.1 as f32 + 1.0)).round() as i32)
    }
    /// a logical rect -> native touch rect as (x0, y0, x1, y1), for the test runner's GEOM lines
    pub fn rect_to_touch(&self, r: &Rect) -> (i32, i32, i32, i32) {
        let (ax, ay) = self.logical_to_touch(r.x, r.y);
        let (bx, by) = self.logical_to_touch(r.right(), r.bottom());
        (ax.min(bx), ay.min(by), ax.max(bx), ay.max(by))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Transform; 8] = [
        Transform::Normal,
        Transform::R90,
        Transform::R180,
        Transform::R270,
        Transform::Flipped,
        Transform::Flipped90,
        Transform::Flipped180,
        Transform::Flipped270,
    ];

    #[test]
    fn transforms_round_trip() {
        for t in ALL {
            for &(u, v) in &[(0.1f32, 0.2f32), (0.9, 0.3), (0.5, 0.5), (0.0, 1.0)] {
                let (x, y) = native_to_logical(t, u, v);
                let (u2, v2) = logical_to_native(t, x, y);
                assert!((u - u2).abs() < 1e-6 && (v - v2).abs() < 1e-6, "{:?}", t);
            }
            assert_eq!(Transform::from_name(t.name()), Some(t));
        }
    }

    #[test]
    fn rotation_directions_match_kwin() {
        // native top-left corner of the panel
        assert_eq!(native_to_logical(Transform::Normal, 0.0, 0.0), (0.0, 0.0));
        assert_eq!(native_to_logical(Transform::R90, 0.0, 0.0), (1.0, 0.0));
        assert_eq!(native_to_logical(Transform::R180, 0.0, 0.0), (1.0, 1.0));
        assert_eq!(native_to_logical(Transform::R270, 0.0, 0.0), (0.0, 1.0));
        assert_eq!(native_to_logical(Transform::Flipped, 0.0, 0.0), (1.0, 0.0));
    }

    #[test]
    fn geometry_mapping() {
        let g = Geometry { w: 1696.0, h: 1200.0, transform: Transform::Normal, native_mm: (274.0, 193.0), tmax: (33919, 23999) };
        let (x, y) = g.touch_to_logical(16960, 12000);
        assert!((x - 848.0).abs() < 0.1 && (y - 600.0).abs() < 0.1);
        assert_eq!(g.logical_to_touch(848.0, 600.0), (16960, 12000));
        assert!((g.px_per_mm() - 6.2).abs() < 0.1);
        let p = Geometry { w: 1200.0, h: 1696.0, transform: Transform::R90, ..g };
        assert_eq!(p.logical_mm(), (193.0, 274.0));
        assert!(!p.landscape());
        let (x, y) = p.touch_to_logical(0, 0);
        assert!((x - 1200.0).abs() < 0.1 && y.abs() < 0.1);
        let r = p.rect_to_touch(&Rect::new(0.0, 0.0, 600.0, 848.0));
        assert_eq!(r, (0, 12000, 16960, 24000));
    }

    #[test]
    fn rect_ops() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert!(a.overlaps(&Rect::new(5.0, 5.0, 10.0, 10.0)));
        assert!(!a.overlaps(&Rect::new(10.0, 0.0, 10.0, 10.0)));
        let area = Rect::new(0.0, 50.0, 200.0, 100.0);
        let f = Rect::new(0.25, 0.5, 0.5, 0.5);
        let abs = Rect::from_frac(&f, &area);
        assert_eq!(abs, Rect::new(50.0, 100.0, 100.0, 50.0));
        let back = abs.to_frac(&area);
        assert!((back.x - 0.25).abs() < 1e-6 && (back.h - 0.5).abs() < 1e-6);
    }
}
