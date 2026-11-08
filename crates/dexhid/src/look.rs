// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! M4 design prototype (`dexhid debug render --style m4`, not used by the daemon yet): design tokens (spacing,
//! radii, type scale, all in mm so they hold at any DPI / scale), colour roles derived from the theme palette (so
//! dark / light / AMOLED / the desktop's own KDE colours all keep working), a small vector icon set drawn from
//! signed distances, and the restyled overlay / prompt / pill. Draws inside the router's rects (hit tests unchanged).

use crate::render::{Align, Canvas, Fonts};
use crate::ui::{code_label, key_label, Hit};
use dexhid_core::feedback::{mix, Palette};
use dexhid_core::geom::Rect;
use dexhid_core::keycodes;
use dexhid_core::layout::{Action, KeyKind, PlacedKey};
use dexhid_core::router::{Btn, ButtonView, UiModel};

// ------------------------------------------------------------------ tokens

/// sizes in logical px, computed once per frame from px/mm (all from mm: the same physical size on any panel)
#[derive(Clone, Copy, Debug)]
pub struct Tokens {
    pub ppm: f32,
    /// spacing scale 0.8 / 1.6 / 2.4 / 3.2 / 4.8 / 6.4 mm
    pub s1: f32,
    pub s2: f32,
    pub s3: f32,
    pub s4: f32,
    pub s6: f32,
    pub s8: f32,
    /// radii: small (chips, small buttons) 1.2 mm, medium (keys, buttons) 1.8 mm, large (pad, cards) 3.2 mm
    pub r_sm: f32,
    pub r_md: f32,
    pub r_lg: f32,
    /// type scale: caption 2.6, body 3.2, label 3.6, title 4.6, display 6.0 mm (glyph em size)
    pub t_caption: f32,
    pub t_body: f32,
    pub t_label: f32,
    pub t_title: f32,
    pub t_display: f32,
    /// hairline 0.18 mm (>= 1 buffer px), key lip 0.4 mm, minimum touch target 9 mm
    pub hair: f32,
    pub lip: f32,
    pub target: f32,
    pub icon: f32,
}

impl Tokens {
    pub fn new(ppm: f32, corner: f32) -> Tokens {
        let ppm = if ppm.is_finite() && ppm > 0.5 { ppm } else { 6.0 };
        let mm = |v: f32| v * ppm;
        let c = corner.clamp(0.0, 3.0);
        Tokens {
            ppm,
            s1: mm(0.8),
            s2: mm(1.6),
            s3: mm(2.4),
            s4: mm(3.2),
            s6: mm(4.8),
            s8: mm(6.4),
            r_sm: mm(1.2) * c,
            r_md: mm(1.8) * c,
            r_lg: mm(3.2) * c,
            t_caption: mm(2.6),
            t_body: mm(3.2),
            t_label: mm(3.6),
            t_title: mm(4.6),
            t_display: mm(6.0),
            hair: mm(0.18).max(1.0),
            lip: mm(0.4),
            target: mm(9.0),
            icon: mm(4.6),
        }
    }
}

/// colour roles (ARGB, straight alpha)
#[derive(Clone, Copy, Debug)]
pub struct Roles {
    /// a dark scheme (for the implementation: icon weights / shadow strength)
    #[allow(dead_code)]
    pub dark: bool,
    pub bg: u32,
    /// strip, bands, touchpad
    pub surface: u32,
    /// cards, sheet, buttons
    pub raised: u32,
    pub key: u32,
    pub key_mod: u32,
    pub key_lip: u32,
    pub outline: u32,
    pub hairline: u32,
    pub text: u32,
    pub text_dim: u32,
    pub accent: u32,
    pub on_accent: u32,
    /// the toggled / selected container (accent mixed into the surface)
    pub accent_soft: u32,
    pub danger: u32,
    pub edit: u32,
    pub scrim: u32,
    pub shadow: u32,
}

fn lum(c: u32) -> f32 {
    let (r, g, b) = (((c >> 16) & 255) as f32, ((c >> 8) & 255) as f32, (c & 255) as f32);
    (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255.0
}

fn alpha(c: u32, a: u32) -> u32 {
    (a.min(255) << 24) | (c & 0x00ff_ffff)
}

/// keep the palette's alpha (translucent keys / panels) on a mixed colour
fn mix_keep(a: u32, b: u32, t: f32) -> u32 {
    (a & 0xff00_0000) | (mix(a, b, t) & 0x00ff_ffff)
}

impl Roles {
    pub fn from(p: &Palette) -> Roles {
        let dark = lum(p.bg) < 0.5;
        let white = 0xffffffff;
        let black = 0xff000000;
        let raised = if dark { mix_keep(p.strip, white, 0.035) } else { mix_keep(p.pad, white, 0.6) };
        Roles {
            dark,
            bg: p.bg,
            surface: p.strip,
            raised: (p.card & 0xff00_0000) | (raised & 0x00ff_ffff),
            key: p.key,
            key_mod: p.key_mod,
            key_lip: if dark { alpha(mix(p.key, black, 0.55), 0xff) } else { alpha(mix(p.key, 0xff8c96a5, 0.55), 0xff) },
            outline: alpha(mix(p.strip, p.text, if dark { 0.16 } else { 0.22 }), 0xff),
            hairline: alpha(mix(p.strip, p.text, if dark { 0.09 } else { 0.13 }), 0xff),
            text: p.text,
            text_dim: p.text_dim,
            accent: p.accent,
            on_accent: if lum(p.accent) > 0.62 { 0xff10141a } else { white },
            accent_soft: alpha(mix(p.strip, p.accent, if dark { 0.26 } else { 0.18 }), 0xff),
            danger: if dark { 0xffff6b63 } else { 0xffc8322b },
            edit: p.edit,
            scrim: if dark { 0x99000000 } else { 0x66101418 },
            shadow: if dark { 0x60 } else { 0x30 },
        }
    }
}

// ------------------------------------------------------------------ icons

/// a primitive in unit coordinates (-1..1, y down), widths in units
#[derive(Clone, Copy, Debug)]
enum Sh {
    Seg(f32, f32, f32, f32, f32),
    Ring(f32, f32, f32, f32),
    Disc(f32, f32, f32),
    /// filled rounded box: centre, half size, radius
    Box(f32, f32, f32, f32, f32),
    /// rounded box outline: centre, half size, radius, width
    BoxRing(f32, f32, f32, f32, f32, f32),
    /// circle arc: centre, radius, width, start angle, sweep (rad, y down: 0 = right, pi/2 = down)
    Arc(f32, f32, f32, f32, f32, f32),
}

const W: f32 = 0.15;
const PI: f32 = std::f32::consts::PI;

fn d_seg(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32) -> f32 {
    let (dx, dy) = (x1 - x0, y1 - y0);
    let l = dx * dx + dy * dy;
    let t = if l > 0.0 { (((px - x0) * dx + (py - y0) * dy) / l).clamp(0.0, 1.0) } else { 0.0 };
    let (qx, qy) = (px - x0 - t * dx, py - y0 - t * dy);
    (qx * qx + qy * qy).sqrt()
}

fn d_box(px: f32, py: f32, cx: f32, cy: f32, hx: f32, hy: f32, r: f32) -> f32 {
    let qx = (px - cx).abs() - (hx - r);
    let qy = (py - cy).abs() - (hy - r);
    let (ox, oy) = (qx.max(0.0), qy.max(0.0));
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}

impl Sh {
    fn dist(&self, px: f32, py: f32) -> f32 {
        match *self {
            Sh::Seg(x0, y0, x1, y1, w) => d_seg(px, py, x0, y0, x1, y1) - w / 2.0,
            Sh::Ring(cx, cy, r, w) => (((px - cx).powi(2) + (py - cy).powi(2)).sqrt() - r).abs() - w / 2.0,
            Sh::Disc(cx, cy, r) => ((px - cx).powi(2) + (py - cy).powi(2)).sqrt() - r,
            Sh::Box(cx, cy, hx, hy, r) => d_box(px, py, cx, cy, hx, hy, r),
            Sh::BoxRing(cx, cy, hx, hy, r, w) => d_box(px, py, cx, cy, hx, hy, r).abs() - w / 2.0,
            Sh::Arc(cx, cy, r, w, a0, sweep) => {
                let (dx, dy) = (px - cx, py - cy);
                let mut a = dy.atan2(dx) - a0;
                while a < 0.0 {
                    a += 2.0 * PI;
                }
                while a >= 2.0 * PI {
                    a -= 2.0 * PI;
                }
                if a <= sweep {
                    ((dx * dx + dy * dy).sqrt() - r).abs() - w / 2.0
                } else {
                    let e0 = (cx + r * a0.cos(), cy + r * a0.sin());
                    let e1 = (cx + r * (a0 + sweep).cos(), cy + r * (a0 + sweep).sin());
                    let d0 = ((px - e0.0).powi(2) + (py - e0.1).powi(2)).sqrt();
                    let d1 = ((px - e1.0).powi(2) + (py - e1.1).powi(2)).sqrt();
                    d0.min(d1) - w / 2.0
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Icon {
    Gear,
    Close,
    Keyboard,
    Display,
    Power,
    Usb,
    Copy,
    Paste,
    Cut,
    Undo,
    Redo,
    SelectAll,
    Find,
    Save,
    Desktop,
    Windows,
    Switch,
    Camera,
    VolDown,
    VolUp,
    Mute,
    Play,
    Next,
    Prev,
    Emoji,
    /// 0 = right, 1 = down, 2 = left, 3 = up
    Chevron(u8),
    Arrow(u8),
    Plus,
    Minus,
    Check,
    Laser,
    Timer,
    Backspace,
    Enter,
    Shift,
    Battery(u8),
    Bolt,
}

const GEAR: &[Sh] = &[
    Sh::Ring(0.0, 0.0, 0.45, 0.24),
    Sh::Seg(0.58, 0.0, 0.84, 0.0, 0.28),
    Sh::Seg(-0.58, 0.0, -0.84, 0.0, 0.28),
    Sh::Seg(0.0, 0.58, 0.0, 0.84, 0.28),
    Sh::Seg(0.0, -0.58, 0.0, -0.84, 0.28),
    Sh::Seg(0.41, 0.41, 0.59, 0.59, 0.28),
    Sh::Seg(-0.41, 0.41, -0.59, 0.59, 0.28),
    Sh::Seg(0.41, -0.41, 0.59, -0.59, 0.28),
    Sh::Seg(-0.41, -0.41, -0.59, -0.59, 0.28),
];
const CLOSE: &[Sh] = &[Sh::Seg(-0.55, -0.55, 0.55, 0.55, 0.17), Sh::Seg(-0.55, 0.55, 0.55, -0.55, 0.17)];
const KEYBOARD: &[Sh] = &[
    Sh::BoxRing(0.0, 0.0, 0.9, 0.62, 0.16, W),
    Sh::Disc(-0.5, -0.24, 0.09),
    Sh::Disc(-0.17, -0.24, 0.09),
    Sh::Disc(0.17, -0.24, 0.09),
    Sh::Disc(0.5, -0.24, 0.09),
    Sh::Disc(-0.34, 0.04, 0.09),
    Sh::Disc(0.0, 0.04, 0.09),
    Sh::Disc(0.34, 0.04, 0.09),
    Sh::Seg(-0.36, 0.32, 0.36, 0.32, W),
];
const DISPLAY: &[Sh] = &[Sh::BoxRing(0.0, -0.18, 0.9, 0.58, 0.12, W), Sh::Seg(0.0, 0.42, 0.0, 0.72, W), Sh::Seg(-0.42, 0.78, 0.42, 0.78, W)];
const POWER: &[Sh] = &[Sh::Arc(0.0, 0.08, 0.66, 0.17, -PI / 2.0 + 0.75, 2.0 * PI - 1.5), Sh::Seg(0.0, -0.86, 0.0, -0.12, 0.17)];
const USB: &[Sh] = &[
    Sh::Seg(0.0, 0.62, 0.0, -0.62, W),
    Sh::Seg(0.0, -0.88, -0.2, -0.58, W),
    Sh::Seg(0.0, -0.88, 0.2, -0.58, W),
    Sh::Disc(0.0, 0.7, 0.17),
    Sh::Seg(0.0, 0.3, -0.46, 0.02, W),
    Sh::Seg(-0.46, 0.02, -0.46, -0.24, W),
    Sh::Box(-0.46, -0.34, 0.11, 0.11, 0.02),
    Sh::Seg(0.0, 0.42, 0.46, 0.14, W),
    Sh::Seg(0.46, 0.14, 0.46, -0.16, W),
    Sh::Disc(0.46, -0.27, 0.12),
];
const COPY: &[Sh] = &[Sh::BoxRing(0.17, 0.17, 0.55, 0.62, 0.12, W), Sh::Seg(-0.7, 0.36, -0.7, -0.62, W), Sh::Arc(-0.55, -0.62, 0.15, W, PI, PI / 2.0), Sh::Seg(-0.55, -0.77, 0.25, -0.77, W)];
const PASTE: &[Sh] = &[Sh::BoxRing(0.0, 0.1, 0.62, 0.78, 0.12, W), Sh::Box(0.0, -0.68, 0.3, 0.14, 0.06), Sh::Seg(-0.3, 0.05, 0.3, 0.05, W), Sh::Seg(-0.3, 0.38, 0.15, 0.38, W)];
const CUT: &[Sh] = &[Sh::Ring(-0.42, 0.56, 0.22, W), Sh::Ring(0.42, 0.56, 0.22, W), Sh::Seg(-0.28, 0.38, 0.42, -0.82, W), Sh::Seg(0.28, 0.38, -0.42, -0.82, W)];
const UNDO: &[Sh] = &[Sh::Arc(0.05, 0.2, 0.55, W, PI, PI * 1.25), Sh::Seg(-0.5, 0.2, -0.8, -0.1, W), Sh::Seg(-0.5, 0.2, -0.18, -0.06, W)];
const REDO: &[Sh] = &[Sh::Arc(-0.05, 0.2, 0.55, W, -PI * 0.25, PI * 1.25), Sh::Seg(0.5, 0.2, 0.8, -0.1, W), Sh::Seg(0.5, 0.2, 0.18, -0.06, W)];
const SELECT_ALL: &[Sh] = &[
    Sh::Seg(-0.8, -0.8, -0.35, -0.8, W),
    Sh::Seg(-0.8, -0.8, -0.8, -0.35, W),
    Sh::Seg(0.8, -0.8, 0.35, -0.8, W),
    Sh::Seg(0.8, -0.8, 0.8, -0.35, W),
    Sh::Seg(-0.8, 0.8, -0.35, 0.8, W),
    Sh::Seg(-0.8, 0.8, -0.8, 0.35, W),
    Sh::Seg(0.8, 0.8, 0.35, 0.8, W),
    Sh::Seg(0.8, 0.8, 0.8, 0.35, W),
    Sh::Box(0.0, 0.0, 0.32, 0.32, 0.06),
];
const FIND: &[Sh] = &[Sh::Ring(-0.16, -0.16, 0.5, 0.16), Sh::Seg(0.22, 0.22, 0.75, 0.75, 0.2)];
const SAVE: &[Sh] = &[
    Sh::Seg(0.0, -0.82, 0.0, 0.3, W),
    Sh::Seg(0.0, 0.34, -0.36, -0.02, W),
    Sh::Seg(0.0, 0.34, 0.36, -0.02, W),
    Sh::Seg(-0.78, 0.42, -0.78, 0.78, W),
    Sh::Seg(-0.78, 0.78, 0.78, 0.78, W),
    Sh::Seg(0.78, 0.78, 0.78, 0.42, W),
];
const DESKTOP: &[Sh] = &[Sh::BoxRing(0.0, 0.0, 0.88, 0.66, 0.14, W), Sh::Seg(-0.88, 0.36, 0.88, 0.36, W), Sh::Disc(-0.55, 0.52, 0.06)];
const WINDOWS: &[Sh] = &[Sh::Box(-0.4, -0.4, 0.32, 0.32, 0.08), Sh::Box(0.4, -0.4, 0.32, 0.32, 0.08), Sh::Box(-0.4, 0.4, 0.32, 0.32, 0.08), Sh::Box(0.4, 0.4, 0.32, 0.32, 0.08)];
const SWITCH: &[Sh] = &[
    Sh::Seg(-0.75, -0.3, 0.7, -0.3, W),
    Sh::Seg(0.75, -0.3, 0.42, -0.62, W),
    Sh::Seg(0.75, -0.3, 0.42, 0.02, W),
    Sh::Seg(0.75, 0.32, -0.7, 0.32, W),
    Sh::Seg(-0.75, 0.32, -0.42, 0.0, W),
    Sh::Seg(-0.75, 0.32, -0.42, 0.64, W),
];
const CAMERA: &[Sh] = &[Sh::BoxRing(0.0, 0.12, 0.86, 0.6, 0.16, W), Sh::Ring(0.0, 0.14, 0.27, W), Sh::Box(-0.28, -0.58, 0.24, 0.1, 0.04)];
const SPEAKER: [Sh; 4] = [Sh::Box(-0.58, 0.0, 0.16, 0.22, 0.04), Sh::Seg(-0.42, -0.2, -0.08, -0.58, W), Sh::Seg(-0.42, 0.2, -0.08, 0.58, W), Sh::Seg(-0.08, -0.58, -0.08, 0.58, W)];
const PLAY: &[Sh] = &[Sh::Seg(-0.42, -0.62, -0.42, 0.62, 0.16), Sh::Seg(-0.42, -0.62, 0.62, 0.0, 0.16), Sh::Seg(-0.42, 0.62, 0.62, 0.0, 0.16), Sh::Seg(-0.3, -0.28, -0.3, 0.28, 0.32), Sh::Seg(-0.25, 0.0, 0.3, 0.0, 0.3)];
const NEXT: &[Sh] = &[Sh::Seg(-0.6, -0.55, -0.6, 0.55, W), Sh::Seg(-0.6, -0.55, 0.3, 0.0, W), Sh::Seg(-0.6, 0.55, 0.3, 0.0, W), Sh::Seg(-0.45, 0.0, 0.0, 0.0, 0.3), Sh::Seg(0.62, -0.58, 0.62, 0.58, 0.17)];
const PREV: &[Sh] = &[Sh::Seg(0.6, -0.55, 0.6, 0.55, W), Sh::Seg(0.6, -0.55, -0.3, 0.0, W), Sh::Seg(0.6, 0.55, -0.3, 0.0, W), Sh::Seg(0.45, 0.0, 0.0, 0.0, 0.3), Sh::Seg(-0.62, -0.58, -0.62, 0.58, 0.17)];
const EMOJI: &[Sh] = &[Sh::Ring(0.0, 0.0, 0.8, W), Sh::Disc(-0.28, -0.2, 0.1), Sh::Disc(0.28, -0.2, 0.1), Sh::Arc(0.0, 0.02, 0.42, W, 0.45, PI - 0.9)];
const CHEVRON: &[Sh] = &[Sh::Seg(-0.2, -0.5, 0.25, 0.0, 0.17), Sh::Seg(0.25, 0.0, -0.2, 0.5, 0.17)];
const ARROW: &[Sh] = &[Sh::Seg(0.0, 0.72, 0.0, -0.62, W), Sh::Seg(0.0, -0.72, -0.4, -0.3, W), Sh::Seg(0.0, -0.72, 0.4, -0.3, W)];
const PLUS: &[Sh] = &[Sh::Seg(-0.55, 0.0, 0.55, 0.0, 0.17), Sh::Seg(0.0, -0.55, 0.0, 0.55, 0.17)];
const MINUS: &[Sh] = &[Sh::Seg(-0.55, 0.0, 0.55, 0.0, 0.17)];
const CHECK: &[Sh] = &[Sh::Seg(-0.6, 0.02, -0.18, 0.44, 0.18), Sh::Seg(-0.18, 0.44, 0.62, -0.42, 0.18)];
const LASER: &[Sh] = &[
    Sh::Disc(0.0, 0.0, 0.26),
    Sh::Seg(0.48, 0.0, 0.8, 0.0, W),
    Sh::Seg(-0.48, 0.0, -0.8, 0.0, W),
    Sh::Seg(0.0, 0.48, 0.0, 0.8, W),
    Sh::Seg(0.0, -0.48, 0.0, -0.8, W),
    Sh::Seg(0.34, 0.34, 0.56, 0.56, W),
    Sh::Seg(-0.34, 0.34, -0.56, 0.56, W),
    Sh::Seg(0.34, -0.34, 0.56, -0.56, W),
    Sh::Seg(-0.34, -0.34, -0.56, -0.56, W),
];
const TIMER: &[Sh] = &[Sh::Ring(0.0, 0.12, 0.68, W), Sh::Seg(0.0, 0.12, 0.0, -0.28, W), Sh::Seg(0.0, 0.12, 0.28, 0.3, W), Sh::Seg(-0.22, -0.86, 0.22, -0.86, W)];
const BACKSPACE: &[Sh] = &[
    Sh::Seg(-0.88, 0.0, -0.42, -0.56, W),
    Sh::Seg(-0.42, -0.56, 0.86, -0.56, W),
    Sh::Seg(0.86, -0.56, 0.86, 0.56, W),
    Sh::Seg(0.86, 0.56, -0.42, 0.56, W),
    Sh::Seg(-0.42, 0.56, -0.88, 0.0, W),
    Sh::Seg(-0.02, -0.22, 0.42, 0.22, W),
    Sh::Seg(-0.02, 0.22, 0.42, -0.22, W),
];
const ENTER: &[Sh] = &[Sh::Seg(0.72, -0.62, 0.72, 0.18, W), Sh::Seg(0.72, 0.18, -0.62, 0.18, W), Sh::Seg(-0.66, 0.18, -0.32, -0.16, W), Sh::Seg(-0.66, 0.18, -0.32, 0.52, W)];
const SHIFT: &[Sh] = &[
    Sh::Seg(0.0, -0.8, 0.74, 0.02, W),
    Sh::Seg(0.74, 0.02, 0.32, 0.02, W),
    Sh::Seg(0.32, 0.02, 0.32, 0.72, W),
    Sh::Seg(0.32, 0.72, -0.32, 0.72, W),
    Sh::Seg(-0.32, 0.72, -0.32, 0.02, W),
    Sh::Seg(-0.32, 0.02, -0.74, 0.02, W),
    Sh::Seg(-0.74, 0.02, 0.0, -0.8, W),
];
const BOLT: &[Sh] = &[Sh::Seg(0.18, -0.85, -0.32, 0.08, 0.2), Sh::Seg(-0.32, 0.08, 0.3, -0.08, 0.2), Sh::Seg(0.3, -0.08, -0.18, 0.85, 0.2)];

/// draw `icon` centred in `r` (logical px), `size` = the icon box edge
pub fn icon(cv: &mut Canvas, r: &Rect, ic: Icon, size: f32, c: u32) {
    let size = size.min(r.w).min(r.h);
    if size < 2.0 {
        return;
    }
    let (cx, cy) = r.center();
    let half = size / 2.0;
    let bbox = Rect::new(cx - half - 1.0, cy - half - 1.0, size + 2.0, size + 2.0);
    let mut extra: [Sh; 6] = [Sh::Disc(9.0, 9.0, 0.0); 6];
    let mut n_extra = 0usize;
    let (base, rot): (&[Sh], u8) = match ic {
        Icon::Gear => (GEAR, 0),
        Icon::Close => (CLOSE, 0),
        Icon::Keyboard => (KEYBOARD, 0),
        Icon::Display => (DISPLAY, 0),
        Icon::Power => (POWER, 0),
        Icon::Usb => (USB, 0),
        Icon::Copy => (COPY, 0),
        Icon::Paste => (PASTE, 0),
        Icon::Cut => (CUT, 0),
        Icon::Undo => (UNDO, 0),
        Icon::Redo => (REDO, 0),
        Icon::SelectAll => (SELECT_ALL, 0),
        Icon::Find => (FIND, 0),
        Icon::Save => (SAVE, 0),
        Icon::Desktop => (DESKTOP, 0),
        Icon::Windows => (WINDOWS, 0),
        Icon::Switch => (SWITCH, 0),
        Icon::Camera => (CAMERA, 0),
        Icon::VolDown | Icon::VolUp | Icon::Mute => {
            extra[..4].copy_from_slice(&SPEAKER);
            match ic {
                Icon::VolUp => {
                    extra[4] = Sh::Seg(0.3, 0.0, 0.82, 0.0, W);
                    extra[5] = Sh::Seg(0.56, -0.26, 0.56, 0.26, W);
                    n_extra = 6;
                }
                Icon::VolDown => {
                    extra[4] = Sh::Seg(0.3, 0.0, 0.82, 0.0, W);
                    n_extra = 5;
                }
                _ => {
                    extra[4] = Sh::Seg(0.3, -0.24, 0.8, 0.26, W);
                    extra[5] = Sh::Seg(0.3, 0.26, 0.8, -0.24, W);
                    n_extra = 6;
                }
            }
            (&[], 0)
        }
        Icon::Play => (PLAY, 0),
        Icon::Next => (NEXT, 0),
        Icon::Prev => (PREV, 0),
        Icon::Emoji => (EMOJI, 0),
        Icon::Chevron(d) => (CHEVRON, d),
        Icon::Arrow(d) => (ARROW, (d + 1) % 4),
        Icon::Plus => (PLUS, 0),
        Icon::Minus => (MINUS, 0),
        Icon::Check => (CHECK, 0),
        Icon::Laser => (LASER, 0),
        Icon::Timer => (TIMER, 0),
        Icon::Backspace => (BACKSPACE, 0),
        Icon::Enter => (ENTER, 0),
        Icon::Shift => (SHIFT, 0),
        Icon::Bolt => (BOLT, 0),
        Icon::Battery(pct) => {
            extra[0] = Sh::BoxRing(-0.08, 0.0, 0.78, 0.44, 0.12, 0.13);
            extra[1] = Sh::Box(0.81, 0.0, 0.07, 0.17, 0.04);
            let fw = 0.58 * (pct.min(100) as f32 / 100.0).max(0.08);
            extra[2] = Sh::Box(-0.66 + fw, 0.0, fw, 0.24, 0.05);
            n_extra = 3;
            (&[], 0)
        }
    };
    let ex = &extra[..n_extra];
    cv.sdf(&bbox, c, |x, y| {
        // to unit coordinates, rotated by `rot` quarter turns (the shapes point right / up)
        let (mut u, mut v) = ((x - cx) / half, (y - cy) / half);
        for _ in 0..rot {
            let t = u;
            u = v;
            v = -t;
        }
        let mut d = f32::INFINITY;
        for s in base.iter().chain(ex.iter()) {
            d = d.min(s.dist(u, v));
        }
        d * half
    });
}

pub fn icon_of_btn(b: &ButtonView) -> Option<Icon> {
    Some(match b.btn {
        Btn::Exit => Icon::Power,
        Btn::Settings => Icon::Gear,
        Btn::Layout => Icon::Keyboard,
        Btn::Display => Icon::Display,
        Btn::Target => {
            if b.label.contains("USB") {
                Icon::Usb
            } else {
                Icon::Desktop
            }
        }
        Btn::Done => Icon::Check,
        Btn::Reset => Icon::Undo,
        Btn::Cancel | Btn::PanelClose => Icon::Close,
        Btn::SheetPrev => Icon::Chevron(2),
        Btn::SheetNext => Icon::Chevron(0),
        Btn::SheetPlus(_) => Icon::Plus,
        Btn::SheetMinus(_) => Icon::Minus,
        Btn::SheetUp(_) => Icon::Arrow(3),
        _ => return None,
    })
}

pub fn icon_of_quick(id: &str) -> Option<Icon> {
    Some(match id {
        "copy" => Icon::Copy,
        "paste" => Icon::Paste,
        "cut" => Icon::Cut,
        "undo" => Icon::Undo,
        "redo" => Icon::Redo,
        "select_all" => Icon::SelectAll,
        "find" => Icon::Find,
        "save" => Icon::Save,
        "alt_tab" => Icon::Switch,
        "show_desktop" => Icon::Desktop,
        "overview" => Icon::Windows,
        "close_window" => Icon::Close,
        "other_screen" => Icon::Display,
        "fullscreen" => Icon::SelectAll,
        "screenshot" => Icon::Camera,
        "vol_down" | "mon_bright_down" => Icon::VolDown,
        "vol_up" | "mon_bright_up" => Icon::VolUp,
        "mute" => Icon::Mute,
        "play_pause" => Icon::Play,
        "next" => Icon::Next,
        "prev" => Icon::Prev,
        "emoji" => Icon::Emoji,
        _ => return None,
    })
}

fn icon_of_key(k: &PlacedKey) -> Option<Icon> {
    if k.label.is_some() {
        return None;
    }
    Some(match &k.kind {
        KeyKind::Code(14) => Icon::Backspace,
        KeyKind::Code(28 | 96) => Icon::Enter,
        KeyKind::Code(42 | 54) => Icon::Shift,
        KeyKind::Code(103) => Icon::Arrow(3),
        KeyKind::Code(108) => Icon::Arrow(1),
        KeyKind::Code(105) => Icon::Arrow(2),
        KeyKind::Code(106) => Icon::Arrow(0),
        KeyKind::Action(Action::EmojiPanel) => Icon::Emoji,
        KeyKind::Action(Action::Laser) => Icon::Laser,
        KeyKind::Action(Action::KeyboardHide) => Icon::Keyboard,
        _ => return None,
    })
}

// ------------------------------------------------------------------ components

/// a button: tonal / filled / quiet, icon + label (label ellipsized; icon only when the label does not fit)
#[derive(Clone, Copy, PartialEq)]
pub enum Tone {
    Tonal,
    Filled,
    Danger,
    Quiet,
    Selected,
}

#[allow(clippy::too_many_arguments)]
pub fn button(cv: &mut Canvas, f: &mut Fonts, t: &Tokens, c: &Roles, r: &Rect, label: &str, ic: Option<Icon>, tone: Tone, pressed: bool) {
    let rad = t.r_md.min(r.h / 2.0);
    let (bg, fg) = match (tone, pressed) {
        (_, true) => (c.accent, c.on_accent),
        (Tone::Filled, _) => (c.accent, c.on_accent),
        (Tone::Danger, _) => (c.raised, c.danger),
        (Tone::Selected, _) => (c.accent_soft, c.text),
        (Tone::Quiet, _) => (0, c.text),
        (Tone::Tonal, _) => (c.raised, c.text),
    };
    if bg >> 24 != 0 {
        cv.rrect(r, rad, bg);
    }
    if tone == Tone::Tonal && !pressed {
        cv.rrect_ring(r, rad, t.hair, c.hairline);
    }
    let pad = t.s3.min(r.w * 0.12);
    let inner = Rect::new(r.x + pad, r.y, (r.w - 2.0 * pad).max(0.0), r.h);
    let isz = t.icon.min(r.h * 0.5);
    let ts = t.t_label.min(r.h * 0.42);
    let lw = if label.is_empty() { 0.0 } else { f.measure(label, ts, cv.scale) };
    match ic {
        Some(i) if !label.is_empty() && isz + t.s2 + lw.min(t.s8 * 2.0) <= inner.w => {
            // icon + label, centred as a group
            let total = (isz + t.s2 + lw).min(inner.w);
            let x = inner.x + (inner.w - total) / 2.0;
            icon(cv, &Rect::new(x, r.y, isz, r.h), i, isz, fg);
            let lr = Rect::new(x + isz + t.s2, r.y, (inner.right() - x - isz - t.s2).max(0.0), r.h);
            f.text(cv, &lr, label, ts, fg, Align::Left, 0.85);
        }
        Some(i) => icon(cv, r, i, isz, fg),
        None => {
            f.text(cv, &inner, label, ts, fg, Align::Center, 0.7);
        }
    }
}

/// an on / off switch (track + knob), right-aligned in `r`
pub fn switch(cv: &mut Canvas, t: &Tokens, c: &Roles, r: &Rect, on: bool) {
    let h = (t.ppm * 5.4).min(r.h);
    let w = h * 1.75;
    let tr = Rect::new(r.right() - w, r.y + (r.h - h) / 2.0, w, h);
    if on {
        cv.rrect(&tr, h / 2.0, c.accent);
    } else {
        cv.rrect(&tr, h / 2.0, c.key_mod | 0xff00_0000);
        cv.rrect_ring(&tr, h / 2.0, t.hair, c.outline);
    }
    let kr = h / 2.0 - t.ppm * 0.6;
    let kx = if on { tr.right() - h / 2.0 } else { tr.x + h / 2.0 };
    cv.disc(kx, tr.y + h / 2.0 + t.ppm * 0.1, kr, 0x40000000);
    cv.disc(kx, tr.y + h / 2.0, kr, if on { c.on_accent } else { c.text_dim });
}

// ------------------------------------------------------------------ overlay

fn draw_key(cv: &mut Canvas, f: &mut Fonts, t: &Tokens, c: &Roles, m: &UiModel, k: &PlacedKey, lit: bool, labels: Option<&crate::xkb::Labels>) {
    let sym = matches!(k.kind, KeyKind::Code(code) if keycodes::is_symbol_key(code) || code == 57) || matches!(k.kind, KeyKind::Combo(_));
    let on = match &k.kind {
        KeyKind::Code(code) => m.latched.iter().any(|(l, _)| l == code) || (*code == 58 && m.leds.0) || (*code == 69 && m.leds.1),
        KeyKind::Action(Action::Fn) => m.fn_on,
        KeyKind::Action(Action::SelectMode) => m.select,
        KeyKind::Action(Action::Laser) => m.laser,
        _ => false,
    };
    let rad = t.r_md.min(k.rect.h * 0.25);
    let lip = t.lip.min(k.rect.h * 0.08);
    let body = Rect::new(k.rect.x, k.rect.y, k.rect.w, (k.rect.h - lip).max(1.0));
    let (fill, fg) = if lit {
        (c.accent, c.on_accent)
    } else if on {
        (c.accent_soft, c.accent)
    } else if sym {
        (c.key, c.text)
    } else {
        (c.key_mod, c.text_dim)
    };
    if lit {
        // pressed: sits down onto its lip
        cv.rrect(&Rect::new(body.x, body.y + lip, body.w, body.h), rad, fill);
    } else {
        cv.rrect(&Rect::new(body.x, body.y + lip, body.w, body.h), rad, c.key_lip & if c.key >> 24 == 255 { 0xffff_ffff } else { 0x80ff_ffff });
        cv.rrect(&body, rad, fill);
        if on {
            cv.rrect_ring(&body, rad, t.hair * 1.5, c.accent);
        }
    }
    let b = if lit { Rect::new(body.x, body.y + lip, body.w, body.h) } else { body };
    if on && !lit {
        let locked = matches!(&k.kind, KeyKind::Code(code) if m.latched.iter().any(|(l, lk)| l == code && *lk)) || matches!(&k.kind, KeyKind::Code(58 | 69));
        if locked {
            cv.disc(b.right() - t.s2 - t.ppm * 0.3, b.y + t.s2 + t.ppm * 0.3, t.ppm * 0.7, c.accent);
        }
    }
    let inner = b.inset(t.s1);
    if let Some(ic) = icon_of_key(k) {
        icon(cv, &inner, ic, (t.icon * 1.1).min(b.h * 0.42), fg);
        return;
    }
    let l = key_label(k, m, labels);
    let n = l.chars().count();
    // one glyph (letters, digits, symbols): large; every named key (F1, Esc, Ctrl, PgUp): one label size
    let size = if n <= 1 { (b.h * 0.40).min(t.ppm * 5.4) } else { (b.h * 0.27).min(t.t_label) };
    let fg = if sym || lit || on { fg } else { c.text_dim };
    f.text(cv, &inner, &l, size, fg, Align::Center, 0.7);
    if let Some(alt) = &k.long {
        let al = match alt {
            KeyKind::Code(cd) => code_label(*cd, false, labels),
            KeyKind::Combo(v) => code_label(*v.last().unwrap_or(&0), true, labels),
            _ => String::new(),
        };
        let r = Rect::new(b.right() - b.w * 0.42, b.y + t.ppm * 0.3, b.w * 0.38, b.h * 0.32);
        f.text(cv, &r, &al, t.t_caption.min(b.h * 0.24), if lit { c.on_accent } else { c.text_dim }, Align::Right, 0.7);
    }
}

/// strip info: "84 % charging  |  DP-1" -> chips with icons
fn info_chips(cv: &mut Canvas, f: &mut Fonts, t: &Tokens, c: &Roles, area: &Rect, info: &str) -> f32 {
    let mut x = area.x;
    let h = (area.h * 0.62).min(t.ppm * 7.0);
    let y = area.y + (area.h - h) / 2.0;
    for seg in info.split('|').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let ic = if seg.contains('%') {
            let pct: u8 = seg.split(|ch: char| !ch.is_ascii_digit()).find(|s| !s.is_empty()).and_then(|s| s.parse().ok()).unwrap_or(50);
            Some(Icon::Battery(pct))
        } else if seg.starts_with("USB") || seg.contains(" USB") || seg.contains("usb") {
            Some(Icon::Usb)
        } else if seg.starts_with("Timer") {
            Some(Icon::Timer)
        } else if seg.contains("DP-") || seg.contains("HDMI") || seg.contains("display") {
            Some(Icon::Display)
        } else if seg.contains(" V ") || seg.contains("power") {
            Some(Icon::Bolt)
        } else {
            None
        };
        let ts = t.t_caption;
        let isz = t.icon * 0.82;
        let tw = f.measure(seg, ts, cv.scale);
        let w = t.s2 + if ic.is_some() { isz + t.s1 } else { 0.0 } + tw + t.s2;
        let room = area.right() - x;
        if room < t.ppm * 14.0 {
            break;
        }
        let w = w.min(room);
        let r = Rect::new(x, y, w, h);
        cv.rrect(&r, h / 2.0, c.raised);
        let mut tx = x + t.s2;
        if let Some(i) = ic {
            icon(cv, &Rect::new(tx, y, isz, h), i, isz, c.text_dim);
            tx += isz + t.s1;
        }
        f.text(cv, &Rect::new(tx, y, (r.right() - t.s2 - tx).max(0.0), h), seg, ts, c.text, Align::Left, 0.9);
        x += w + t.s1;
    }
    x
}

pub fn draw_static(cv: &mut Canvas, f: &mut Fonts, m: &UiModel, p: &Palette, corner: f32, labels: Option<&crate::xkb::Labels>) {
    let t = Tokens::new(m.px_per_mm, corner);
    let c = Roles::from(p);
    cv.clear(c.bg);
    // strip
    cv.fill(&m.strip, c.surface);
    cv.bar(&Rect::new(m.strip.x, m.strip.bottom() - t.hair, m.strip.w, t.hair), c.hairline);
    let first_btn = m.buttons.iter().filter(|b| m.strip.contains(b.rect.x, b.rect.y)).map(|b| b.rect.x).fold(m.strip.right(), f32::min);
    let mark = (m.strip.h * 0.5).min(t.ppm * 6.4);
    let mr = Rect::new(m.strip.x + t.s4, m.strip.y + (m.strip.h - mark) / 2.0, mark, mark);
    cv.rrect(&mr, t.r_sm.max(mark * 0.28), c.accent);
    icon(cv, &mr, Icon::Keyboard, mark * 0.62, c.on_accent);
    let tw = f.text(cv, &Rect::new(mr.right() + t.s2, m.strip.y, t.ppm * 22.0, m.strip.h), &m.title, t.t_title.min(m.strip.h * 0.42), c.text, Align::Left, 0.8);
    let info_area = Rect::new(mr.right() + t.s2 + tw + t.s4, m.strip.y, (first_btn - (mr.right() + t.s2 + tw + t.s4) - t.s4).max(0.0), m.strip.h);
    let mut x = info_area.x;
    if !m.edit && !m.info.is_empty() {
        x = info_chips(cv, f, &t, &c, &info_area, &m.info);
    }
    let (line, col) = if m.edit { (&m.hint, c.edit) } else if !m.notice.is_empty() { (&m.notice, c.accent) } else { (&m.hint, c.text_dim) };
    let hr = Rect::new(x + t.s2, m.strip.y, (info_area.right() - x - t.s2).max(0.0), m.strip.h);
    if hr.w > t.ppm * 20.0 {
        f.text(cv, &hr, line, t.t_caption, col, Align::Left, 1.0);
    }
    // areas
    for r in &m.regions {
        if r.id == dexhid_core::arrangement::TOUCHPAD {
            cv.rrect(&r.rect, t.r_lg, c.surface);
            cv.rrect_ring(&r.rect, t.r_lg, t.hair, c.hairline);
        }
    }
    if m.laser {
        if let Some(r) = m.regions.iter().find(|r| r.id == dexhid_core::arrangement::TOUCHPAD) {
            cv.rrect_ring(&r.rect.inset(t.s1), t.r_lg, t.ppm * 0.5, 0xffe5484d);
            let cr = Rect::new(r.rect.x + (r.rect.w - t.ppm * 80.0).max(0.0) / 2.0, r.rect.bottom() - t.s3 - t.ppm * 7.0, (t.ppm * 80.0).min(r.rect.w), t.ppm * 7.0);
            cv.rrect(&cr, cr.h / 2.0, c.raised);
            icon(cv, &Rect::new(cr.x + t.s2, cr.y, t.icon, cr.h), Icon::Laser, t.icon * 0.8, 0xffe5484d);
            f.text(cv, &Rect::new(cr.x + t.s2 + t.icon + t.s1, cr.y, cr.w - t.icon - t.s4 - t.s1, cr.h), "Laser pointer: the pad is the monitor", t.t_caption, c.text_dim, Align::Left, 0.8);
        }
    }
    for b in &m.bands {
        cv.fill(&b.rect, c.surface);
        cv.bar(&Rect::new(b.rect.x, b.rect.bottom() - t.hair, b.rect.w, t.hair), c.hairline);
        if b.chips.is_empty() && !b.empty.is_empty() {
            f.text(cv, &Rect::new(b.rect.x + t.s4, b.rect.y, b.rect.w - t.s8, b.rect.h), &b.empty, t.t_caption, c.text_dim, Align::Left, 1.0);
        }
        for (l, r) in &b.chips {
            let r = r.inset(t.ppm * 0.3);
            cv.rrect(&r, r.h / 2.0, c.raised);
            cv.rrect_ring(&r, r.h / 2.0, t.hair, c.hairline);
            let (ic, text) = match l.strip_prefix("\u{263a} ") {
                Some(rest) => (Some(Icon::Emoji), rest),
                None => (None, l.as_str()),
            };
            let inner = r.inset(t.s2);
            match ic {
                Some(i) => {
                    let lw = f.measure(text, t.t_body, cv.scale).min(inner.w - t.icon - t.s1);
                    let x = inner.x + (inner.w - (t.icon + t.s1 + lw)) / 2.0;
                    icon(cv, &Rect::new(x, r.y, t.icon, r.h), i, t.icon * 0.85, c.text);
                    f.text(cv, &Rect::new(x + t.icon + t.s1, r.y, lw + 1.0, r.h), text, t.t_body, c.text, Align::Left, 0.8);
                }
                None => {
                    let align = if b.id == "clip" { Align::Left } else { Align::Center };
                    f.text(cv, &Rect::new(inner.x, r.y, inner.w, r.h), text, t.t_body.min(r.h * 0.5), c.text, align, 0.85);
                }
            }
        }
    }
    if let Some((l, r)) = &m.zones {
        let sep = Rect::new(r.x - t.hair / 2.0, r.y + t.s3, t.hair, (r.h - 2.0 * t.s3).max(0.0));
        cv.bar(&Rect::new(l.x + t.s3, l.y, (r.right() - l.x - 2.0 * t.s3).max(0.0), t.hair), c.hairline);
        cv.bar(&sep, c.hairline);
        f.text(cv, l, "Left click", t.t_caption, alpha(c.text_dim, 0xb0), Align::Center, 0.8);
        f.text(cv, r, "Right click", t.t_caption, alpha(c.text_dim, 0xb0), Align::Center, 0.8);
    }
    for q in &m.quick {
        quick_button(cv, f, &t, &c, q, false);
    }
    for k in &m.keys {
        draw_key(cv, f, &t, &c, m, k, false, labels);
    }
    if m.edit {
        for r in &m.regions {
            cv.rrect_ring(&r.rect, t.r_lg, t.ppm * 0.5, c.edit);
            let hs = t.ppm * 2.2;
            for (x, y) in [(r.rect.x, r.rect.y), (r.rect.right(), r.rect.y), (r.rect.x, r.rect.bottom()), (r.rect.right(), r.rect.bottom())] {
                cv.disc(x.clamp(hs, m.w - hs), y.clamp(hs, m.h - hs), hs, c.edit);
            }
            let (bl, bt) = ((r.rect.w * 0.18).min(t.ppm * 16.0), t.ppm * 1.6);
            let (cx, cy) = r.rect.center();
            cv.rrect(&Rect::new(cx - bl / 2.0, r.rect.y, bl, bt), bt / 2.0, c.edit);
            cv.rrect(&Rect::new(cx - bl / 2.0, r.rect.bottom() - bt, bl, bt), bt / 2.0, c.edit);
            let bl2 = (r.rect.h * 0.18).min(t.ppm * 16.0);
            cv.rrect(&Rect::new(r.rect.x, cy - bl2 / 2.0, bt, bl2), bt / 2.0, c.edit);
            cv.rrect(&Rect::new(r.rect.right() - bt, cy - bl2 / 2.0, bt, bl2), bt / 2.0, c.edit);
            let name = r.id.replace('_', " ");
            let w = (f.measure(&name, t.t_label, cv.scale) + t.s8).min(r.rect.w);
            let chip = Rect::new(cx - w / 2.0, cy - t.ppm * 4.5, w, t.ppm * 9.0);
            cv.rrect(&chip, chip.h / 2.0, alpha(c.raised, 0xf0));
            f.text(cv, &chip.inset(t.s2), &name, t.t_label, c.edit, Align::Center, 0.7);
        }
        for g in &m.guides {
            let w = t.ppm * 0.35;
            let r = if g.vertical { Rect::new(g.pos - w / 2.0, g.from, w, g.to - g.from) } else { Rect::new(g.from, g.pos - w / 2.0, g.to - g.from, w) };
            cv.bar(&r, c.accent);
        }
    }
    if let Some(s) = &m.sheet {
        sheet(cv, f, &t, &c, m, s);
    }
    if let Some(e) = &m.emoji {
        cv.bar(&Rect::new(0.0, m.strip.bottom(), m.w, m.h - m.strip.bottom()), c.scrim);
        cv.shadow(&e.rect, t.r_lg, t.s3, c.shadow);
        cv.rrect(&e.rect, t.r_lg, c.raised | 0xff00_0000);
    }
    for b in &m.buttons {
        if matches!(b.btn, Btn::SheetTab(_) | Btn::SheetRow(_) | Btn::SheetMinus(_) | Btn::SheetPlus(_) | Btn::SheetUp(_)) {
            continue; // drawn by the sheet
        }
        strip_button(cv, f, &t, &c, m, b, false);
    }
}

fn quick_button(cv: &mut Canvas, f: &mut Fonts, t: &Tokens, c: &Roles, q: &dexhid_core::router::QuickView, pressed: bool) {
    let r = q.rect;
    let rad = t.r_md.min(r.h / 2.0);
    cv.rrect(&r, rad, if pressed { c.accent } else { c.key_mod | 0xff00_0000 });
    let fg = if pressed { c.on_accent } else { c.text };
    let ic = icon_of_quick(&q.id);
    let isz = t.icon.min(r.h * 0.4);
    match ic {
        // tall enough: icon above a caption; else icon + label in a row
        Some(i) if r.h >= isz + t.t_caption + t.s3 * 1.5 => {
            let gh = isz + t.s1 + t.t_caption;
            let y = r.y + (r.h - gh) / 2.0;
            icon(cv, &Rect::new(r.x, y, r.w, isz), i, isz, fg);
            f.text(cv, &Rect::new(r.x + t.s1, y + isz + t.s1, r.w - t.s2, t.t_caption * 1.2), &q.label, t.t_caption, if pressed { fg } else { c.text_dim }, Align::Center, 0.75);
        }
        _ => button(cv, f, t, c, &r, &q.label, ic, Tone::Quiet, pressed),
    }
}

fn strip_button(cv: &mut Canvas, f: &mut Fonts, t: &Tokens, c: &Roles, m: &UiModel, b: &ButtonView, pressed: bool) {
    let sel = matches!(b.btn, Btn::EmojiTab(i) if m.emoji.as_ref().is_some_and(|e| e.page == i as usize));
    match b.btn {
        Btn::Emoji(_) => {
            if pressed {
                cv.rrect(&b.rect, t.r_md, c.accent_soft);
            }
            f.text(cv, &b.rect, &b.label, b.rect.h * 0.6, c.text, Align::Center, 0.6);
        }
        Btn::EmojiTab(_) => button(cv, f, t, c, &b.rect, &b.label, None, if sel { Tone::Selected } else { Tone::Quiet }, pressed),
        Btn::Exit => button(cv, f, t, c, &b.rect, &b.label, icon_of_btn(b), Tone::Danger, pressed),
        Btn::Done => button(cv, f, t, c, &b.rect, &b.label, icon_of_btn(b), Tone::Filled, pressed),
        Btn::PanelClose if m.sheet.is_some() => button(cv, f, t, c, &b.rect, "Done", Some(Icon::Check), Tone::Filled, pressed),
        Btn::SheetPrev | Btn::SheetNext => button(cv, f, t, c, &b.rect, &b.label.replace(['\u{2039}', '\u{203a}'], "").trim().to_string(), icon_of_btn(b), Tone::Quiet, pressed),
        Btn::Target if b.label.contains("USB") => button(cv, f, t, c, &b.rect, &b.label, icon_of_btn(b), Tone::Selected, pressed),
        _ => button(cv, f, t, c, &b.rect, &b.label, icon_of_btn(b), Tone::Tonal, pressed),
    }
}

fn sheet(cv: &mut Canvas, f: &mut Fonts, t: &Tokens, c: &Roles, m: &UiModel, s: &dexhid_core::router::SheetView) {
    // scrim over the areas (static layer: once per open), then the card
    cv.bar(&Rect::new(0.0, m.strip.bottom(), m.w, m.h - m.strip.bottom()), c.scrim);
    cv.shadow(&s.rect, t.r_lg, t.s4, c.shadow);
    cv.rrect(&s.rect, t.r_lg, c.raised | 0xff00_0000);
    // navigation rail
    let tabs: Vec<&ButtonView> = m.buttons.iter().filter(|b| matches!(b.btn, Btn::SheetTab(_))).collect();
    if let Some(b0) = tabs.first() {
        let rail = Rect::new(s.rect.x, s.rect.y, b0.rect.right() + t.s2 - s.rect.x, s.rect.h);
        cv.rrect(&rail, t.r_lg, c.surface | 0xff00_0000);
        cv.fill(&Rect::new(rail.right() - t.r_lg, rail.y, t.r_lg, rail.h), c.surface | 0xff00_0000);
        cv.bar(&Rect::new(rail.right(), rail.y, t.hair, rail.h), c.hairline);
    }
    for b in &tabs {
        let i = if let Btn::SheetTab(i) = b.btn { i as usize } else { 0 };
        let sel = s.tab == i;
        let planned = b.label.ends_with('\u{b7}');
        let label = b.label.trim_end_matches('\u{b7}').trim_end();
        let r = b.rect.inset(t.ppm * 0.2);
        if sel {
            cv.rrect(&r, t.r_md.min(r.h / 2.0), c.accent_soft);
            cv.rrect(&Rect::new(r.x, r.y + r.h * 0.22, t.ppm * 0.7, r.h * 0.56), t.ppm * 0.35, c.accent);
        }
        let col = if sel { c.text } else if planned { alpha(c.text_dim, 0xb0) } else { c.text_dim };
        f.text(cv, &Rect::new(r.x + t.s3, r.y, r.w - t.s3 - t.s4, r.h), label, t.t_body.min(r.h * 0.5), col, Align::Left, 0.8);
        if planned {
            cv.disc(r.right() - t.s2, r.y + r.h / 2.0, t.ppm * 0.6, c.edit);
        }
    }
    // header
    let x0 = s.rows.first().map(|r| r.rect.x).unwrap_or(s.rect.x + t.ppm * 60.0);
    let hw = s.rect.right() - x0 - t.s4;
    f.text(cv, &Rect::new(x0 + t.s1, s.rect.y + t.s3, hw, t.t_title * 1.4), &s.title, t.t_title, c.text, Align::Left, 0.8);
    if !s.subtitle.is_empty() {
        let planned = s.subtitle.starts_with("planned");
        f.text(cv, &Rect::new(x0 + t.s1, s.rect.y + t.s3 + t.t_title * 1.4, hw, t.t_caption * 1.4), &s.subtitle, t.t_caption, if planned { c.edit } else { c.text_dim }, Align::Left, 0.85);
    }
    // rows
    let n = s.rows.len();
    for (i, r) in s.rows.iter().enumerate() {
        let idx = i as u8;
        let row_btn = m.buttons.iter().find(|b| b.btn == Btn::SheetRow(idx));
        let minus = m.buttons.iter().find(|b| b.btn == Btn::SheetMinus(idx));
        let plus = m.buttons.iter().find(|b| b.btn == Btn::SheetPlus(idx));
        let up = m.buttons.iter().find(|b| b.btn == Btn::SheetUp(idx));
        let ctl_x = [row_btn, minus, plus, up].iter().flatten().map(|b| b.rect.x).fold(r.rect.right(), f32::min);
        if i + 1 < n {
            cv.bar(&Rect::new(r.rect.x + t.s1, r.rect.bottom() + t.ppm * 0.4, r.rect.w - t.s1, t.hair), c.hairline);
        }
        let lr = Rect::new(r.rect.x + t.s1, r.rect.y, (ctl_x - r.rect.x - t.s3).max(0.0), r.rect.h);
        f.text(cv, &lr, &r.title, t.t_body, if r.live { c.text } else { c.text_dim }, Align::Left, 0.85);
        let Some(vb) = row_btn else { continue };
        let v = vb.label.as_str();
        if minus.is_some() || plus.is_some() {
            // stepper: round − / + with the value between
            for b in [minus, plus].into_iter().flatten() {
                let d = b.rect.h.min(b.rect.w);
                let cr = Rect::new(b.rect.x + (b.rect.w - d) / 2.0, b.rect.y + (b.rect.h - d) / 2.0, d, d);
                cv.rrect(&cr, d / 2.0, c.key_mod | 0xff00_0000);
                icon(cv, &cr, icon_of_btn(b).unwrap_or(Icon::Plus), d * 0.42, c.text);
            }
            f.text(cv, &vb.rect, v, t.t_body, c.text, Align::Center, 0.8);
        } else if v == "On" || v == "Off" || v == "on" || v == "off" {
            switch(cv, t, c, &vb.rect, v.eq_ignore_ascii_case("on"));
        } else {
            // a choice: value chip with a chevron
            let vr = vb.rect;
            cv.rrect(&vr, t.r_md.min(vr.h / 2.0), c.key_mod | 0xff00_0000);
            let isz = t.icon * 0.7;
            f.text(cv, &Rect::new(vr.x + t.s2, vr.y, vr.w - t.s2 - isz - t.s3, vr.h), v, t.t_body, c.text, Align::Left, 0.75);
            icon(cv, &Rect::new(vr.right() - isz - t.s2, vr.y, isz, vr.h), Icon::Chevron(1), isz, c.text_dim);
        }
        if let Some(b) = up {
            let d = b.rect.h.min(b.rect.w);
            let cr = Rect::new(b.rect.x + (b.rect.w - d) / 2.0, b.rect.y + (b.rect.h - d) / 2.0, d, d);
            icon(cv, &cr, Icon::Arrow(3), d * 0.45, c.text_dim);
        }
    }
}

/// dynamic layer: pressed keys / buttons / quick actions + touch dots; returns the logical rects touched
pub fn draw_dynamic(cv: &mut Canvas, f: &mut Fonts, m: &UiModel, p: &Palette, corner: f32, labels: Option<&crate::xkb::Labels>, out: &mut Vec<Rect>) {
    let t = Tokens::new(m.px_per_mm, corner);
    let c = Roles::from(p);
    for &i in &m.lit_keys {
        if let Some(k) = m.keys.get(i) {
            draw_key(cv, f, &t, &c, m, k, true, labels);
            out.push(k.rect);
        }
    }
    if let Some(b) = m.lit_button.and_then(|b| m.buttons.iter().find(|v| v.btn == b)) {
        strip_button(cv, f, &t, &c, m, b, true);
        out.push(b.rect);
    }
    if let Some(q) = m.lit_quick.and_then(|i| m.quick.get(i)) {
        quick_button(cv, f, &t, &c, q, true);
        out.push(q.rect);
    }
    let r = t.ppm * 3.6;
    for &(x, y) in &m.dots {
        cv.disc(x, y, r, alpha(c.accent, 0x50));
        cv.disc(x, y, r * 0.45, alpha(c.accent, 0xd0));
        out.push(Rect::new(x - r, y - r, 2.0 * r, 2.0 * r));
    }
}

// ------------------------------------------------------------------ prompt / pill

/// the "ask" card, laid out from its content (no fixed card height): 2 columns of buttons when they fit at
/// >= 9 mm targets, else one column; the card never leaves the screen (content shrinks to fit)
pub fn prompt_layout(w: f32, h: f32, ppm: f32) -> (Rect, Rect, Rect, Rect, Vec<(Rect, Hit, &'static str)>) {
    use Hit as H;
    use dexhid_core::state::Choice;
    let t = Tokens::new(ppm, 1.0);
    let margin = t.s4.min(w * 0.04);
    let cw = (150.0 * t.ppm).min(w - 2.0 * margin).max(1.0);
    let pad = t.s6.min(cw * 0.06);
    let inner = (cw - 2.0 * pad).max(1.0);
    let two_cols = inner >= 2.0 * 50.0 * t.ppm + t.s3;
    let bh = t.target.max(12.0 * t.ppm).min(h * 0.12);
    let bh2 = t.target.max(10.0 * t.ppm).min(h * 0.1);
    let gap = t.s3;
    let icon_h = 12.0 * t.ppm;
    let title_h = t.t_display * 1.4;
    let body_h = t.t_body * 1.6;
    let buttons_h = if two_cols { bh + gap + bh2 } else { 2.0 * bh + 2.0 * bh2 + 3.0 * gap };
    let mut ch = pad + icon_h + t.s3 + title_h + body_h + t.s6 + buttons_h + pad;
    let avail = (h - 2.0 * margin).max(1.0).min(h);
    let k = if ch > avail { avail / ch } else { 1.0 };
    ch *= k;
    let card = Rect::new((w - cw) / 2.0, ((h - ch) / 2.0).max(0.0), cw, ch);
    let mut y = card.y + pad * k;
    let icon_r = Rect::new(card.x, y, cw, icon_h * k);
    y += (icon_h + t.s3) * k;
    let title_r = Rect::new(card.x + pad, y, inner, title_h * k);
    y += title_h * k;
    let body_r = Rect::new(card.x + pad, y, inner, body_h * k);
    y += (body_h + t.s6) * k;
    let x = card.x + pad;
    let (bh, bh2, gap) = (bh * k, bh2 * k, gap * k);
    let b = if two_cols {
        let bw = (inner - gap) / 2.0;
        vec![
            (Rect::new(x, y, bw, bh), H::Choice(Choice::UsePad), "Use as touchpad + keyboard"),
            (Rect::new(x + bw + gap, y, bw, bh), H::Choice(Choice::KeepDisplay), "Keep as display"),
            (Rect::new(x, y + bh + gap, bw, bh2), H::Choice(Choice::Always), "Always for this monitor"),
            (Rect::new(x + bw + gap, y + bh + gap, bw, bh2), H::Choice(Choice::Never), "Never for this monitor"),
        ]
    } else {
        vec![
            (Rect::new(x, y, inner, bh), H::Choice(Choice::UsePad), "Use as touchpad + keyboard"),
            (Rect::new(x, y + bh + gap, inner, bh), H::Choice(Choice::KeepDisplay), "Keep as display"),
            (Rect::new(x, y + 2.0 * (bh + gap), inner, bh2), H::Choice(Choice::Always), "Always for this monitor"),
            (Rect::new(x, y + 2.0 * (bh + gap) + bh2 + gap, inner, bh2), H::Choice(Choice::Never), "Never for this monitor"),
        ]
    };
    (card, icon_r, title_r, body_r, b)
}

pub fn draw_prompt(cv: &mut Canvas, f: &mut Fonts, p: &Palette, w: f32, h: f32, ppm: f32, lit: Option<Hit>) -> (Rect, Vec<(Rect, Hit)>) {
    let t = Tokens::new(ppm, 1.0);
    let c = Roles::from(p);
    let (card, icon_r, title_r, body_r, hits) = prompt_layout(w, h, ppm);
    cv.shadow(&card, t.r_lg * 1.5, t.s4, c.shadow + 0x20);
    cv.rrect(&card, t.r_lg * 1.5, c.raised | 0xff00_0000);
    cv.rrect_ring(&card, t.r_lg * 1.5, t.hair, c.hairline);
    let d = icon_r.h;
    let ib = Rect::new(icon_r.x + (icon_r.w - d) / 2.0, icon_r.y, d, d);
    cv.rrect(&ib, d / 2.0, c.accent_soft);
    icon(cv, &ib, Icon::Display, d * 0.5, c.accent);
    f.text(cv, &title_r, "External display connected", t.t_display, c.text, Align::Center, 0.6);
    f.text(cv, &body_r, "Use this tablet as a touchpad and keyboard?", t.t_body, c.text_dim, Align::Center, 0.6);
    for (i, (r, hit, label)) in hits.iter().enumerate() {
        let tone = match i {
            0 => Tone::Filled,
            1 => Tone::Tonal,
            _ => Tone::Quiet,
        };
        if tone == Tone::Quiet && lit != Some(*hit) {
            cv.rrect_ring(r, t.r_md, t.hair, c.outline);
        }
        let ic = match i {
            0 => Some(Icon::Keyboard),
            1 => Some(Icon::Display),
            _ => None,
        };
        button(cv, f, &t, &c, r, label, ic, tone, lit == Some(*hit));
    }
    (card, hits.into_iter().map(|(r, h, _)| (r, h)).collect())
}

pub fn draw_pill(cv: &mut Canvas, f: &mut Fonts, p: &Palette, w: f32, ppm: f32, lit: Option<Hit>) -> (Rect, Vec<(Rect, Hit)>) {
    use Hit as H;
    let t = Tokens::new(ppm, 1.0);
    let c = Roles::from(p);
    let ph = t.target.max(10.0 * ppm);
    let pw = (62.0 * ppm).min(w - 2.0 * t.s2);
    let pill = Rect::new((w - pw) / 2.0, t.s2, pw, ph);
    let hw = ph;
    let a = Rect::new(pill.x, pill.y, pw - hw, ph);
    let b = Rect::new(pill.right() - hw, pill.y, hw, ph);
    cv.shadow(&pill, ph / 2.0, t.s2, c.shadow);
    cv.rrect(&pill, ph / 2.0, c.raised | 0xff00_0000);
    if lit == Some(H::PillPad) {
        cv.rrect(&a, ph / 2.0, c.accent_soft);
    }
    if lit == Some(H::PillHide) {
        cv.rrect(&b, ph / 2.0, c.accent_soft);
    }
    let isz = t.icon;
    let lw = f.measure("Use as pad", t.t_label, cv.scale);
    let gx = a.x + (a.w - isz - t.s2 - lw) / 2.0;
    icon(cv, &Rect::new(gx, a.y, isz, a.h), Icon::Keyboard, isz, c.accent);
    f.text(cv, &Rect::new(gx + isz + t.s2, a.y, lw + 2.0, a.h), "Use as pad", t.t_label, c.text, Align::Left, 0.8);
    cv.bar(&Rect::new(b.x, b.y + t.s2, t.hair, b.h - 2.0 * t.s2), c.hairline);
    icon(cv, &b, Icon::Close, isz * 0.75, c.text_dim);
    (pill, vec![(a, H::PillPad), (b, H::PillHide)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// the prompt fits on every size / orientation and keeps >= 9 mm targets where the screen allows
    #[test]
    fn prompt_layout_fits_every_size() {
        for (w, h, ppm) in [(1696.0, 1200.0, 6.19), (1200.0, 1696.0, 6.19), (2261.0, 1600.0, 8.25), (800.0, 600.0, 3.0), (360.0, 640.0, 6.0), (1696.0, 300.0, 6.19), (40.0, 40.0, 6.0)] {
            let (card, icon_r, title_r, body_r, b) = prompt_layout(w, h, ppm);
            assert!(card.x >= -0.01 && card.y >= -0.01 && card.right() <= w + 0.01 && card.bottom() <= h + 0.01, "card inside the screen at {}x{}", w, h);
            for r in [icon_r, title_r, body_r].iter().chain(b.iter().map(|x| &x.0)) {
                assert!(r.x >= card.x - 0.01 && r.right() <= card.right() + 0.01 && r.y >= card.y - 0.01 && r.bottom() <= card.bottom() + 0.01, "{:?} inside the card at {}x{}", r, w, h);
            }
            for i in 0..b.len() {
                for j in i + 1..b.len() {
                    assert!(!b[i].0.overlaps(&b[j].0), "buttons do not overlap at {}x{}", w, h);
                }
            }
            if h >= 120.0 * ppm && w >= 80.0 * ppm {
                assert!(b.iter().all(|x| x.0.h >= 9.0 * ppm - 0.01), "9 mm targets at {}x{}", w, h);
            }
        }
    }

    /// the AA primitives clip to the buffer: off-screen, zero, inverted, huge rects and radii never panic
    #[test]
    fn primitives_clip_odd_rects() {
        let (bw, bh) = (64, 48);
        let mut px = vec![0u32; (bw * bh) as usize];
        for scale in [1.0f32, 1.5, 2.0, 2.25] {
            let mut cv = Canvas { px: &mut px, bw, bh, scale };
            for r in [Rect::new(-50.0, -50.0, 40.0, 40.0), Rect::new(10.0, 10.0, 0.0, 5.0), Rect::new(10.0, 10.0, -5.0, 5.0), Rect::new(-1e6, -1e6, 2e6, 2e6), Rect::new(30.0, 20.0, 0.3, 0.3), Rect::new(60.0, 40.0, 50.0, 50.0)] {
                cv.rrect(&r, 8.0, 0xff336699);
                cv.rrect(&r, 1e9, 0x80336699);
                cv.rrect_ring(&r, 4.0, 1.5, 0xffffffff);
                cv.shadow(&r, 4.0, 3.0, 0x60);
                cv.bar(&r, 0x40ffffff);
                icon(&mut cv, &r, Icon::Gear, 20.0, 0xffffffff);
            }
        }
    }

    /// text never paints outside its rect (shrinks, then ellipsizes); skipped when the PC has no font
    #[test]
    fn text_stays_inside_its_rect() {
        let mut f = Fonts::load();
        if !f.ok() {
            return;
        }
        for scale in [1.0f32, 2.0] {
            let (bw, bh) = ((200.0 * scale) as i32, (60.0 * scale) as i32);
            let mut px = vec![0u32; (bw * bh) as usize];
            let r = Rect::new(20.0, 10.0, 90.0, 30.0);
            {
                let mut cv = Canvas { px: &mut px, bw, bh, scale };
                for align in [Align::Left, Align::Center, Align::Right] {
                    f.text(&mut cv, &r, "Keyboard: Full PC keyboard with numpad and more", 20.0, 0xffffffff, align, 0.8);
                }
            }
            for y in 0..bh {
                for x in 0..bw {
                    let (lx, ly) = (x as f32 / scale, y as f32 / scale);
                    if px[(y * bw + x) as usize] != 0 {
                        assert!(lx >= r.x - 1.0 && lx <= r.right() + 1.0 && ly >= r.y - 1.0 && ly <= r.bottom() + 1.0, "ink at {},{} outside {:?} (scale {})", lx, ly, r, scale);
                    }
                }
            }
        }
    }

    #[test]
    fn tokens_survive_odd_densities() {
        for ppm in [0.0, -1.0, f32::NAN, 0.3, 3.0, 6.19, 12.4, 40.0] {
            let t = Tokens::new(ppm, f32::NAN.max(1.0));
            assert!(t.ppm.is_finite() && t.ppm > 0.0 && t.hair >= 1.0 && t.target > t.s8);
        }
    }

    #[test]
    fn roles_pick_readable_text_on_accent() {
        let p = dexhid_core::feedback::palette("dark", "#f0e060", 100);
        assert_eq!(Roles::from(&p).on_accent, 0xff10141a, "dark text on a light accent");
        let p = dexhid_core::feedback::palette("light", "#3060c0", 100);
        let r = Roles::from(&p);
        assert!(!r.dark && r.on_accent == 0xffffffff);
    }
}
