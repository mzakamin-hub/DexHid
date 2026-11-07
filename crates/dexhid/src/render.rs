// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! Minimal software renderer: rects, rounded rects, outlines, circles and text (ab_glyph with a system font) into a
//! premultiplied ARGB8888 / XRGB8888 buffer. Coordinates are logical px; `scale` maps them to buffer px.

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use dexhid_core::geom::Rect;
use std::collections::HashMap;

pub const BG: u32 = 0xff101318;
pub const STRIP: u32 = 0xff1b2028;
pub const BTN: u32 = 0xff2b3442;
pub const BTN_LIT: u32 = 0xff3f7fd0;
pub const KEY: u32 = 0xff262c36;
pub const KEY_MOD: u32 = 0xff1f242c;
pub const KEY_LIT: u32 = 0xff4a8be0;
pub const PAD: u32 = 0xff181d24;
pub const PAD_EDGE: u32 = 0xff3a4554;
pub const ZONE: u32 = 0xff1d232c;
pub const TEXT: u32 = 0xffe8ecf2;
pub const TEXT_DIM: u32 = 0xff8b95a5;
pub const EDIT: u32 = 0xffe0a030;
pub const DOT: u32 = 0xc04a8be0;
pub const CARD: u32 = 0xf0202630;
pub const SHADE: u32 = 0x00000000;

pub struct Canvas<'a> {
    pub px: &'a mut [u32],
    /// buffer size (px)
    pub bw: i32,
    pub bh: i32,
    pub scale: f32,
}

fn blend(dst: u32, src: u32) -> u32 {
    let a = src >> 24;
    if a == 255 {
        return src;
    }
    if a == 0 {
        return dst;
    }
    // premultiply src, then src over dst (dst premultiplied)
    let inv = 255 - a;
    let ch = |s: u32, d: u32| -> u32 { ((s * a) / 255 + (d * inv) / 255).min(255) };
    let r = ch((src >> 16) & 255, (dst >> 16) & 255);
    let g = ch((src >> 8) & 255, (dst >> 8) & 255);
    let b = ch(src & 255, dst & 255);
    let da = (dst >> 24) & 255;
    let oa = (a + (da * inv) / 255).min(255);
    (oa << 24) | (r << 16) | (g << 8) | b
}

impl<'a> Canvas<'a> {
    fn brect(&self, r: &Rect) -> (i32, i32, i32, i32) {
        let x0 = ((r.x * self.scale).round() as i32).clamp(0, self.bw);
        let y0 = ((r.y * self.scale).round() as i32).clamp(0, self.bh);
        let x1 = ((r.right() * self.scale).round() as i32).clamp(0, self.bw);
        let y1 = ((r.bottom() * self.scale).round() as i32).clamp(0, self.bh);
        (x0, y0, x1, y1)
    }

    /// the buffer-pixel rect covering a logical rect (for damage / restore)
    pub fn damage_of(&self, r: &Rect) -> (i32, i32, i32, i32) {
        let (x0, y0, x1, y1) = self.brect(&r.inset(-2.0));
        (x0, y0, (x1 - x0).max(0), (y1 - y0).max(0))
    }

    pub fn clear(&mut self, c: u32) {
        self.px.fill(c);
    }

    pub fn fill(&mut self, r: &Rect, c: u32) {
        self.round_rect(r, 0.0, c);
    }

    pub fn round_rect(&mut self, r: &Rect, radius: f32, c: u32) {
        let (x0, y0, x1, y1) = self.brect(r);
        let rad = (radius * self.scale).min((x1 - x0) as f32 / 2.0).min((y1 - y0) as f32 / 2.0).max(0.0);
        let opaque = c >> 24 == 255;
        for y in y0..y1 {
            // horizontal inset at this row for the rounded corners
            let dy = if (y as f32) < y0 as f32 + rad {
                y0 as f32 + rad - y as f32 - 0.5
            } else if (y as f32) > y1 as f32 - rad - 1.0 {
                y as f32 + 0.5 - (y1 as f32 - rad)
            } else {
                0.0
            };
            let inset = if dy > 0.0 { (rad - (rad * rad - dy * dy).max(0.0).sqrt()).round() as i32 } else { 0 };
            let row = (y * self.bw) as usize;
            let (a, b) = ((x0 + inset).min(x1) as usize, (x1 - inset).max(x0) as usize);
            if a >= b {
                continue;
            }
            if opaque {
                self.px[row + a..row + b].fill(c);
            } else {
                for p in &mut self.px[row + a..row + b] {
                    *p = blend(*p, c);
                }
            }
        }
    }

    pub fn outline(&mut self, r: &Rect, w: f32, c: u32) {
        self.fill(&Rect::new(r.x, r.y, r.w, w), c);
        self.fill(&Rect::new(r.x, r.bottom() - w, r.w, w), c);
        self.fill(&Rect::new(r.x, r.y, w, r.h), c);
        self.fill(&Rect::new(r.right() - w, r.y, w, r.h), c);
    }

    pub fn circle(&mut self, cx: f32, cy: f32, rad: f32, c: u32) {
        self.round_rect(&Rect::new(cx - rad, cy - rad, rad * 2.0, rad * 2.0), rad, c);
    }

    /// copy a rect (buffer px) from another full-size buffer
    pub fn copy_from(&mut self, src: &[u32], d: (i32, i32, i32, i32)) {
        let (x, y, w, h) = d;
        for yy in y.max(0)..(y + h).min(self.bh) {
            let r = (yy * self.bw) as usize;
            let a = r + x.max(0) as usize;
            let b = r + (x + w).min(self.bw) as usize;
            if a < b {
                self.px[a..b].copy_from_slice(&src[a..b]);
            }
        }
    }
}

pub struct Fonts {
    font: Option<FontVec>,
    pub path: String,
    cache: HashMap<(char, u32), Option<(Vec<u8>, i32, i32, i32, i32)>>,
}

const FALLBACK_FONTS: &[&str] = &[
    "/usr/share/fonts/google-noto-vf/NotoSans[wght].ttf",
    "/usr/share/fonts/google-noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
];

impl Fonts {
    pub fn load() -> Fonts {
        let mut cands: Vec<String> = vec![];
        if let Ok(p) = std::env::var("DEXHID_FONT") {
            cands.push(p);
        }
        if let Ok(o) = std::process::Command::new("fc-match").args(["-f", "%{file}", "sans-serif:weight=medium"]).output() {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !s.is_empty() {
                cands.push(s);
            }
        }
        cands.extend(FALLBACK_FONTS.iter().map(|s| s.to_string()));
        for p in cands {
            if let Ok(b) = std::fs::read(&p) {
                if let Ok(f) = FontVec::try_from_vec(b) {
                    return Fonts { font: Some(f), path: p, cache: HashMap::new() };
                }
            }
        }
        Fonts { font: None, path: "none".into(), cache: HashMap::new() }
    }

    fn has(&self, c: char) -> bool {
        self.font.as_ref().is_some_and(|f| f.glyph_id(c).0 != 0)
    }

    /// replace symbols the font cannot draw
    pub fn printable(&self, s: &str) -> String {
        if s.chars().all(|c| self.has(c) || c == ' ') {
            return s.to_string();
        }
        s.chars()
            .map(|c| {
                if self.has(c) {
                    c.to_string()
                } else {
                    match c {
                        '\u{232b}' => "Bksp".into(),
                        '\u{23ce}' => "Enter".into(),
                        '\u{21e7}' => "Shift".into(),
                        '\u{2190}' => "<".into(),
                        '\u{2192}' => ">".into(),
                        '\u{2191}' => "^".into(),
                        '\u{2193}' => "v".into(),
                        _ => "?".into(),
                    }
                }
            })
            .collect()
    }

    /// width of `s` at `size` buffer px
    pub fn width(&self, s: &str, size: f32) -> f32 {
        let Some(f) = &self.font else { return 0.0 };
        let sf = f.as_scaled(PxScale::from(size));
        s.chars().map(|c| sf.h_advance(f.glyph_id(c))).sum()
    }

    fn glyph(&mut self, c: char, size: u32) -> Option<(Vec<u8>, i32, i32, i32, i32)> {
        if let Some(g) = self.cache.get(&(c, size)) {
            return g.clone();
        }
        let f = self.font.as_ref()?;
        let sf = f.as_scaled(PxScale::from(size as f32));
        let gl = sf.scaled_glyph(c);
        let r = match f.outline_glyph(gl) {
            Some(og) => {
                let bb = og.px_bounds();
                let (w, h) = (bb.width().ceil() as i32, bb.height().ceil() as i32);
                let mut cov = vec![0u8; (w.max(0) * h.max(0)) as usize];
                og.draw(|x, y, v| {
                    let i = (y as i32 * w + x as i32) as usize;
                    if i < cov.len() {
                        cov[i] = (v.clamp(0.0, 1.0) * 255.0) as u8;
                    }
                });
                Some((cov, w, h, bb.min.x.round() as i32, bb.min.y.round() as i32 + (sf.ascent().round() as i32)))
            }
            None => Some((vec![], 0, 0, 0, 0)),
        };
        self.cache.insert((c, size), r.clone());
        r
    }

    /// draw `s` centred in `r` (logical px), `size` in logical px; shrinks to fit the width
    pub fn draw_centered(&mut self, cv: &mut Canvas, r: &Rect, s: &str, size: f32, color: u32) {
        if self.font.is_none() || s.is_empty() {
            return;
        }
        let s = self.printable(s);
        let mut px = (size * cv.scale).max(6.0);
        let maxw = r.w * cv.scale * 0.92;
        let w = self.width(&s, px);
        if w > maxw && w > 0.0 {
            px = (px * maxw / w).max(6.0);
        }
        let px = px.round();
        let w = self.width(&s, px);
        let (asc, desc) = {
            let f = self.font.as_ref().unwrap();
            let sf = f.as_scaled(PxScale::from(px));
            (sf.ascent(), sf.descent())
        };
        let x0 = r.x * cv.scale + (r.w * cv.scale - w) / 2.0;
        let top = r.y * cv.scale + (r.h * cv.scale - (asc - desc)) / 2.0;
        self.draw_at(cv, x0, top, &s, px as u32, color);
    }

    /// draw left-aligned, vertically centred
    pub fn draw_left(&mut self, cv: &mut Canvas, r: &Rect, s: &str, size: f32, color: u32) {
        if self.font.is_none() || s.is_empty() {
            return;
        }
        let s = self.printable(s);
        let px = (size * cv.scale).max(6.0).round();
        let (asc, desc) = {
            let f = self.font.as_ref().unwrap();
            let sf = f.as_scaled(PxScale::from(px));
            (sf.ascent(), sf.descent())
        };
        let top = r.y * cv.scale + (r.h * cv.scale - (asc - desc)) / 2.0;
        self.draw_at(cv, r.x * cv.scale, top, &s, px as u32, color);
    }

    fn draw_at(&mut self, cv: &mut Canvas, x0: f32, top: f32, s: &str, size: u32, color: u32) {
        let mut x = x0;
        let (cr, cg, cb) = ((color >> 16) & 255, (color >> 8) & 255, color & 255);
        let ca = color >> 24;
        for ch in s.chars() {
            let adv = {
                let f = self.font.as_ref().unwrap();
                f.as_scaled(PxScale::from(size as f32)).h_advance(f.glyph_id(ch))
            };
            if let Some((cov, w, h, gx, gy)) = self.glyph(ch, size) {
                let ox = x.round() as i32 + gx;
                let oy = top.round() as i32 + gy;
                for yy in 0..h {
                    let py = oy + yy;
                    if py < 0 || py >= cv.bh {
                        continue;
                    }
                    for xx in 0..w {
                        let pxx = ox + xx;
                        if pxx < 0 || pxx >= cv.bw {
                            continue;
                        }
                        let a = cov[(yy * w + xx) as usize] as u32 * ca / 255;
                        if a == 0 {
                            continue;
                        }
                        let i = (py * cv.bw + pxx) as usize;
                        cv.px[i] = blend(cv.px[i], (a << 24) | (cr << 16) | (cg << 8) | cb);
                    }
                }
            }
            x += adv;
        }
    }

    pub fn ok(&self) -> bool {
        self.font.is_some()
    }
}
