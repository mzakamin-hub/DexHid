// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! Minimal software renderer: rects, rounded rects, outlines, circles and text (ab_glyph with a system font) into a
//! premultiplied ARGB8888 / XRGB8888 buffer. Coordinates are logical px; `scale` maps them to buffer px.

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use dexhid_core::geom::Rect;
use std::collections::HashMap;

// prompt / pill colours (the pad-mode overlay uses the theme palette, dexhid_core::feedback::palette)
pub const BTN: u32 = 0xff2b3442;
pub const BTN_LIT: u32 = 0xff3f7fd0;
pub const TEXT: u32 = 0xffe8ecf2;
pub const TEXT_DIM: u32 = 0xff8b95a5;
pub const CARD: u32 = 0xf0202630;
pub const SHADE: u32 = 0x00000000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

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

    // ---------------------------------------------------------------- M4 anti-aliased primitives
    // Coverage comes from a signed distance (buffer px, < 0 inside). Interior spans are filled without any per-pixel
    // maths; only the 1-2 px boundary band and the corner arcs are evaluated. All clipped to the buffer: any rect,
    // also negative / huge / NaN-free zero-size ones, is safe.

    #[inline]
    fn put(&mut self, x: i32, y: i32, c: u32, cov: f32) {
        if cov <= 0.0 || x < 0 || y < 0 || x >= self.bw || y >= self.bh {
            return;
        }
        let a = (((c >> 24) as f32) * cov.min(1.0)).round() as u32;
        let i = (y * self.bw + x) as usize;
        self.px[i] = blend(self.px[i], (a << 24) | (c & 0x00ff_ffff));
    }

    #[inline]
    fn span(&mut self, y: i32, a: i32, b: i32, c: u32) {
        let (a, b) = (a.max(0), b.min(self.bw));
        if y < 0 || y >= self.bh || a >= b {
            return;
        }
        let row = (y * self.bw) as usize;
        let al = c >> 24;
        if al == 255 {
            self.px[row + a as usize..row + b as usize].fill(c);
        } else {
            // constant-colour src-over with the source terms hoisted and /255 as a multiply-shift
            let inv = 255 - al;
            let (sr, sg, sb, sa) = (((c >> 16) & 255) * al, ((c >> 8) & 255) * al, (c & 255) * al, al * 255);
            let div = |v: u32| ((v + 128) * 257) >> 16;
            for p in &mut self.px[row + a as usize..row + b as usize] {
                let d = *p;
                let r = div(sr + ((d >> 16) & 255) * inv);
                let g = div(sg + ((d >> 8) & 255) * inv);
                let bl = div(sb + (d & 255) * inv);
                let oa = div(sa + (d >> 24) * inv);
                *p = (oa << 24) | (r << 16) | (g << 8) | bl;
            }
        }
    }

    /// signed distance of (px, py) to a rounded box (centre, half size, radius), buffer px
    #[inline]
    fn sd_rrect(px: f32, py: f32, cx: f32, cy: f32, hx: f32, hy: f32, r: f32) -> f32 {
        let qx = (px - cx).abs() - (hx - r);
        let qy = (py - cy).abs() - (hy - r);
        let ox = qx.max(0.0);
        let oy = qy.max(0.0);
        (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
    }

    /// M4: anti-aliased filled rounded rect (logical coordinates)
    pub fn rrect(&mut self, r: &Rect, radius: f32, c: u32) {
        self.rrect_ring(r, radius, f32::INFINITY, c);
    }

    /// M4: anti-aliased rounded-rect outline of width `w` (logical px) inside `r`; `w = INFINITY` fills
    pub fn rrect_ring(&mut self, r: &Rect, radius: f32, w: f32, c: u32) {
        let s = self.scale;
        let (fx0, fy0, fx1, fy1) = (r.x * s, r.y * s, r.right() * s, r.bottom() * s);
        if !(fx1 - fx0 > 0.0 && fy1 - fy0 > 0.0) || c >> 24 == 0 {
            return;
        }
        let (hx, hy) = ((fx1 - fx0) / 2.0, (fy1 - fy0) / 2.0);
        let (cx, cy) = (fx0 + hx, fy0 + hy);
        let rad = (radius * s).clamp(0.0, hx.min(hy));
        let ww = (w * s).max(0.0);
        let x0 = (fx0.floor() as i32).max(0);
        let x1 = (fx1.ceil() as i32).min(self.bw);
        let y0 = (fy0.floor() as i32).max(0);
        let y1 = (fy1.ceil() as i32).min(self.bh);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let cov_at = |x: i32, y: i32| -> f32 {
            let d = Self::sd_rrect(x as f32 + 0.5, y as f32 + 0.5, cx, cy, hx, hy, rad);
            let outer = (0.5 - d).clamp(0.0, 1.0);
            if ww.is_finite() {
                outer.min((0.5 + d + ww).clamp(0.0, 1.0))
            } else {
                outer
            }
        };
        for y in y0..y1 {
            let py = y as f32 + 0.5;
            // the shape's exact horizontal extent on this row
            let dy = if py < fy0 + rad { fy0 + rad - py } else if py > fy1 - rad { py - (fy1 - rad) } else { 0.0 };
            let inset = if dy > 0.0 { rad - (rad * rad - dy * dy).max(0.0).sqrt() } else { 0.0 };
            let ring = ww.is_finite();
            let edge = if ring { ww + rad + 1.5 } else { 1.5 };
            if py - fy0 < edge || fy1 - py < edge {
                // boundary rows (the ring's top / bottom bands with the corner arcs): per pixel near the sides; in
                // the middle (farther from the sides than from the top / bottom edge) the distance only depends on
                // the row, so one coverage for the whole span
                let side = (if ring { ww } else { 0.0 } + rad + 2.0).ceil() as i32;
                let (ml, mr) = (((fx0.ceil() as i32) + side).max(x0), ((fx1.floor() as i32) - side).min(x1));
                if ml < mr {
                    for x in x0..ml {
                        let cv = cov_at(x, y);
                        self.put(x, y, c, cv);
                    }
                    let cv = cov_at((ml + mr) / 2, y);
                    if cv >= 1.0 {
                        self.span(y, ml, mr, c);
                    } else if cv > 0.0 {
                        let a = (((c >> 24) as f32) * cv).round() as u32;
                        self.span(y, ml, mr, (a << 24) | (c & 0x00ff_ffff));
                    }
                    for x in mr..x1 {
                        let cv = cov_at(x, y);
                        self.put(x, y, c, cv);
                    }
                } else {
                    for x in x0..x1 {
                        let cv = cov_at(x, y);
                        self.put(x, y, c, cv);
                    }
                }
                continue;
            }
            if ring {
                // straight sides only: the left and right bands
                let band = ww.ceil() as i32 + 2;
                let l_end = (x0 + band).min(x1);
                for x in x0..l_end {
                    let cv = cov_at(x, y);
                    self.put(x, y, c, cv);
                }
                for x in (x1 - band).max(l_end)..x1 {
                    let cv = cov_at(x, y);
                    self.put(x, y, c, cv);
                }
                continue;
            }
            // full interior between the boundary bands, per-pixel only within 1.5 px of the edges
            let l_full = ((fx0 + inset + 1.0).ceil() as i32).max(x0);
            let r_full = ((fx1 - inset - 1.0).floor() as i32).min(x1);
            for x in x0..l_full.min(x1) {
                let cv = cov_at(x, y);
                self.put(x, y, c, cv);
            }
            if l_full < r_full {
                self.span(y, l_full, r_full, c);
            }
            for x in r_full.max(l_full).max(x0)..x1 {
                let cv = cov_at(x, y);
                self.put(x, y, c, cv);
            }
        }
    }

    /// M4: a hairline / thin bar snapped to whole buffer px, never thinner than one px (no vanishing separators at
    /// fractional positions)
    pub fn bar(&mut self, r: &Rect, c: u32) {
        let s = self.scale;
        let x0 = (r.x * s).floor() as i32;
        let y0 = (r.y * s).floor() as i32;
        let x1 = ((r.right() * s).round() as i32).max(x0 + 1);
        let y1 = ((r.bottom() * s).round() as i32).max(y0 + 1);
        for y in y0.max(0)..y1.min(self.bh) {
            self.span(y, x0, x1, c);
        }
    }

    /// M4: anti-aliased disc
    pub fn disc(&mut self, cx: f32, cy: f32, rad: f32, c: u32) {
        self.rrect(&Rect::new(cx - rad, cy - rad, rad * 2.0, rad * 2.0), rad, c);
    }

    /// M4: a cheap soft shadow under an opaque rounded rect: three expanding low-alpha rings, offset down. Only the
    /// band around the shape is painted (the shape covers the rest), so the cost follows the perimeter, not the area.
    pub fn shadow(&mut self, r: &Rect, radius: f32, elev: f32, alpha: u32) {
        if elev <= 0.0 {
            return;
        }
        for (k, a) in [(1.0f32, alpha / 2), (0.6, alpha / 3), (0.3, alpha / 3)] {
            let e = elev * k;
            let rr = Rect::new(r.x - e * 0.5, r.y + elev * 0.35 - e * 0.25, r.w + e, r.h + e);
            self.rrect_ring(&rr, radius + e * 0.5, e + elev * 0.5 + 2.0 / self.scale + radius * 0.3, a.min(255) << 24);
        }
    }

    /// M4: evaluate an arbitrary signed distance (logical px in, logical px out) over a small box: icons only
    pub fn sdf(&mut self, bbox: &Rect, c: u32, f: impl Fn(f32, f32) -> f32) {
        let s = self.scale;
        let x0 = ((bbox.x * s).floor() as i32).max(0);
        let y0 = ((bbox.y * s).floor() as i32).max(0);
        let x1 = ((bbox.right() * s).ceil() as i32).min(self.bw);
        let y1 = ((bbox.bottom() * s).ceil() as i32).min(self.bh);
        for y in y0..y1 {
            for x in x0..x1 {
                let d = f((x as f32 + 0.5) / s, (y as f32 + 0.5) / s) * s;
                let cv = (0.5 - d).clamp(0.0, 1.0);
                if cv > 0.0 {
                    self.put(x, y, c, cv);
                }
            }
        }
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
    /// M3: a fallback font for emoji / symbols with outline glyphs (a colour-only bitmap / COLR font gives no
    /// outlines: such emoji show as their code point)
    emoji: Option<FontVec>,
    pub emoji_path: String,
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
                    let (emoji, emoji_path) = Self::emoji_font();
                    return Fonts { font: Some(f), emoji, emoji_path, path: p, cache: HashMap::new() };
                }
            }
        }
        Fonts { font: None, emoji: None, emoji_path: "none".into(), path: "none".into(), cache: HashMap::new() }
    }

    /// M3: an emoji font with outlines (Noto Emoji monochrome, Symbola, ...)
    fn emoji_font() -> (Option<FontVec>, String) {
        let mut cands: Vec<String> = vec![];
        if let Ok(p) = std::env::var("DEXHID_EMOJI_FONT") {
            cands.push(p);
        }
        for pat in ["Noto Emoji", "emoji", "Symbola"] {
            if let Ok(o) = std::process::Command::new("fc-match").args(["-f", "%{file}", pat]).output() {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !s.is_empty() && !cands.contains(&s) {
                    cands.push(s);
                }
            }
        }
        cands.extend(
            ["/usr/share/fonts/google-noto-emoji/NotoEmoji-Regular.ttf", "/usr/share/fonts/google-noto-emoji-fonts/NotoEmoji-Regular.ttf", "/usr/share/fonts/truetype/noto/NotoEmoji-Regular.ttf", "/usr/share/fonts/gdouros-symbola/Symbola.ttf"]
                .iter()
                .map(|s| s.to_string()),
        );
        for p in cands {
            if let Ok(b) = std::fs::read(&p) {
                if let Ok(f) = FontVec::try_from_vec(b) {
                    // outline glyphs needed (bitmap / COLR emoji fonts draw nothing here)
                    let g = f.glyph_id('\u{1f600}');
                    if g.0 != 0 && f.outline(g).is_some() {
                        return (Some(f), p);
                    }
                }
            }
        }
        (None, "none".into())
    }

    fn main_has(&self, c: char) -> bool {
        self.font.as_ref().is_some_and(|f| f.glyph_id(c).0 != 0)
    }

    fn has(&self, c: char) -> bool {
        self.main_has(c) || self.emoji.as_ref().is_some_and(|f| f.glyph_id(c).0 != 0)
    }

    /// the font that draws `c`
    fn font_for(&self, c: char) -> Option<&FontVec> {
        if !self.main_has(c) {
            if let Some(e) = self.emoji.as_ref().filter(|f| f.glyph_id(c).0 != 0) {
                return Some(e);
            }
        }
        self.font.as_ref()
    }

    /// replace symbols the font cannot draw
    pub fn printable(&self, s: &str) -> String {
        // M3: variation selectors / joiners are invisible
        let s: String = s.chars().filter(|c| !matches!(*c, '\u{fe0f}' | '\u{fe0e}' | '\u{200d}')).collect();
        if s.chars().all(|c| self.has(c) || c == ' ') {
            return s;
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
                        // M3: an emoji without a drawable glyph shows its code point
                        c if (c as u32) >= 0x2300 => format!("U+{:X}", c as u32),
                        _ => "?".into(),
                    }
                }
            })
            .collect()
    }

    /// width of `s` at `size` buffer px
    pub fn width(&self, s: &str, size: f32) -> f32 {
        if self.font.is_none() {
            return 0.0;
        }
        s.chars()
            .map(|c| {
                let f = self.font_for(c).unwrap();
                f.as_scaled(PxScale::from(size)).h_advance(f.glyph_id(c))
            })
            .sum()
    }

    fn glyph(&mut self, c: char, size: u32) -> Option<(Vec<u8>, i32, i32, i32, i32)> {
        if let Some(g) = self.cache.get(&(c, size)) {
            return g.clone();
        }
        let f = self.font_for(c)?;
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
                let f = self.font_for(ch).unwrap();
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

    /// M4 bench: glyph cache entries and coverage bytes
    pub fn cache_stats(&self) -> (usize, usize) {
        (self.cache.len(), self.cache.values().map(|g| g.as_ref().map(|g| g.0.len()).unwrap_or(0)).sum())
    }

    /// M4: text in `r` (logical), `size` logical px, aligned; never overflows: shrinks down to `min_frac` of the
    /// size, then ellipsizes with "…" (or "..." without that glyph). Returns the drawn width (logical px).
    pub fn text(&mut self, cv: &mut Canvas, r: &Rect, s: &str, size: f32, color: u32, align: Align, min_frac: f32) -> f32 {
        if self.font.is_none() || s.is_empty() || r.w <= 0.0 || r.h <= 0.0 {
            return 0.0;
        }
        let mut s = self.printable(s);
        let maxw = r.w * cv.scale;
        let mut px = (size.min(r.h) * cv.scale).max(6.0);
        let w = self.width(&s, px);
        if w > maxw && w > 0.0 {
            px = (px * maxw / w).max(px * min_frac.clamp(0.3, 1.0)).max(6.0);
        }
        let px = px.round();
        if self.width(&s, px) > maxw {
            let ell = if self.has('\u{2026}') { "\u{2026}" } else { "..." };
            let ew = self.width(ell, px);
            while !s.is_empty() && self.width(&s, px) + ew > maxw {
                s.pop();
            }
            let t = s.trim_end().len();
            s.truncate(t);
            s.push_str(ell);
        }
        let w = self.width(&s, px);
        let (asc, desc) = {
            let f = self.font.as_ref().unwrap();
            let sf = f.as_scaled(PxScale::from(px));
            (sf.ascent(), sf.descent())
        };
        let x0 = match align {
            Align::Left => r.x * cv.scale,
            Align::Center => r.x * cv.scale + (maxw - w) / 2.0,
            Align::Right => r.right() * cv.scale - w,
        };
        let top = r.y * cv.scale + (r.h * cv.scale - (asc - desc)) / 2.0;
        self.draw_at(cv, x0, top, &s, px as u32, color);
        w / cv.scale
    }

    /// M4: width in logical px of `s` at logical `size`
    pub fn measure(&self, s: &str, size: f32, scale: f32) -> f32 {
        self.width(&self.printable(s), (size * scale).round()) / scale
    }
}
