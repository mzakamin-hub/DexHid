// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! Software renderer (M4): anti-aliased rects / rounded rects / rings / discs / soft shadows, a small signed-distance
//! painter for icons, and text (ab_glyph with a system font) into a premultiplied ARGB8888 / XRGB8888 buffer.
//! Coordinates are logical px; `scale` maps them to buffer px. Every primitive clips to the canvas clip rect, so a
//! frame can repaint only its damaged rects by drawing the whole scene with a clip (elements outside are culled
//! with `visible`). No primitive allocates; glyphs are rasterised once into a bounded cache and blitted by reference.

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use dexhid_core::geom::Rect;
use std::borrow::Cow;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// a buffer rect (x, y, w, h)
pub type BRect = (i32, i32, i32, i32);

pub struct Canvas<'a> {
    pub px: &'a mut [u32],
    /// buffer size (px)
    pub bw: i32,
    pub bh: i32,
    pub scale: f32,
    /// paint only inside (x0, y0, x1, y1), buffer px
    clip: (i32, i32, i32, i32),
}

#[inline]
fn div255(v: u32) -> u32 {
    ((v + 128) * 257) >> 16
}

/// straight-alpha `src` over premultiplied `dst`
#[inline]
fn blend(dst: u32, src: u32) -> u32 {
    let a = src >> 24;
    if a == 255 {
        return src;
    }
    if a == 0 {
        return dst;
    }
    let inv = 255 - a;
    let r = div255(((src >> 16) & 255) * a + ((dst >> 16) & 255) * inv);
    let g = div255(((src >> 8) & 255) * a + ((dst >> 8) & 255) * inv);
    let b = div255((src & 255) * a + (dst & 255) * inv);
    let oa = div255(a * 255 + (dst >> 24) * inv);
    (oa << 24) | (r << 16) | (g << 8) | b
}

impl<'a> Canvas<'a> {
    pub fn new(px: &'a mut [u32], bw: i32, bh: i32, scale: f32) -> Canvas<'a> {
        let (bw, bh) = (bw.max(0), bh.max(0));
        debug_assert!(px.len() >= (bw * bh) as usize);
        let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
        Canvas { px, bw, bh, scale, clip: (0, 0, bw, bh) }
    }

    /// limit painting to a buffer rect (intersected with the buffer); `None` = the whole buffer
    pub fn set_clip(&mut self, c: Option<BRect>) {
        self.clip = match c {
            Some((x, y, w, h)) => (x.clamp(0, self.bw), y.clamp(0, self.bh), (x + w).clamp(0, self.bw), (y + h).clamp(0, self.bh)),
            None => (0, 0, self.bw, self.bh),
        };
    }

    /// can anything drawn inside logical rect `r` (+ `margin` logical px) land in the clip?
    #[inline]
    pub fn visible_m(&self, r: &Rect, margin: f32) -> bool {
        let s = self.scale;
        let (x0, y0, x1, y1) = self.clip;
        x0 < x1 && y0 < y1 && (r.x - margin) * s < x1 as f32 && (r.right() + margin) * s > x0 as f32 && (r.y - margin) * s < y1 as f32 && (r.bottom() + margin) * s > y0 as f32
    }

    #[inline]
    pub fn visible(&self, r: &Rect) -> bool {
        self.visible_m(r, 2.0)
    }

    /// fill the clip region
    pub fn clear(&mut self, c: u32) {
        let (x0, y0, x1, y1) = self.clip;
        for y in y0..y1 {
            let row = (y * self.bw) as usize;
            self.px[row + x0 as usize..row + x1 as usize].fill(c);
        }
    }

    #[inline]
    fn put(&mut self, x: i32, y: i32, c: u32, cov: f32) {
        let (x0, y0, x1, y1) = self.clip;
        if cov <= 0.0 || x < x0 || y < y0 || x >= x1 || y >= y1 {
            return;
        }
        let a = (((c >> 24) as f32) * cov.min(1.0)).round() as u32;
        let i = (y * self.bw + x) as usize;
        self.px[i] = blend(self.px[i], (a << 24) | (c & 0x00ff_ffff));
    }

    #[inline]
    fn span(&mut self, y: i32, a: i32, b: i32, c: u32) {
        let (cx0, cy0, cx1, cy1) = self.clip;
        let (a, b) = (a.max(cx0), b.min(cx1));
        if y < cy0 || y >= cy1 || a >= b {
            return;
        }
        let row = (y * self.bw) as usize;
        let al = c >> 24;
        if al == 255 {
            self.px[row + a as usize..row + b as usize].fill(c);
        } else if al > 0 {
            // constant-colour src-over with the source terms hoisted
            let inv = 255 - al;
            let (sr, sg, sb, sa) = (((c >> 16) & 255) * al, ((c >> 8) & 255) * al, (c & 255) * al, al * 255);
            for p in &mut self.px[row + a as usize..row + b as usize] {
                let d = *p;
                let r = div255(sr + ((d >> 16) & 255) * inv);
                let g = div255(sg + ((d >> 8) & 255) * inv);
                let bl = div255(sb + (d & 255) * inv);
                let oa = div255(sa + (d >> 24) * inv);
                *p = (oa << 24) | (r << 16) | (g << 8) | bl;
            }
        }
    }

    /// a plain rect, edges rounded to whole buffer px
    pub fn fill(&mut self, r: &Rect, c: u32) {
        let s = self.scale;
        let x0 = (r.x * s).round() as i32;
        let y0 = (r.y * s).round() as i32;
        let x1 = (r.right() * s).round() as i32;
        let y1 = (r.bottom() * s).round() as i32;
        for y in y0.max(self.clip.1)..y1.min(self.clip.3) {
            self.span(y, x0, x1, c);
        }
    }

    /// a hairline / thin bar snapped to whole buffer px, never thinner than one px
    pub fn bar(&mut self, r: &Rect, c: u32) {
        let s = self.scale;
        let x0 = (r.x * s).floor() as i32;
        let y0 = (r.y * s).floor() as i32;
        let x1 = ((r.right() * s).round() as i32).max(x0 + 1);
        let y1 = ((r.bottom() * s).round() as i32).max(y0 + 1);
        for y in y0.max(self.clip.1)..y1.min(self.clip.3) {
            self.span(y, x0, x1, c);
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

    /// anti-aliased filled rounded rect
    pub fn rrect(&mut self, r: &Rect, radius: f32, c: u32) {
        self.rrect_ring(r, radius, f32::INFINITY, c);
    }

    /// anti-aliased rounded-rect outline of width `w` (logical px) inside `r`; `w = INFINITY` fills. Interior spans
    /// are filled directly; only the edge band and the corner arcs are evaluated per pixel.
    pub fn rrect_ring(&mut self, r: &Rect, radius: f32, w: f32, c: u32) {
        let s = self.scale;
        let (fx0, fy0, fx1, fy1) = (r.x * s, r.y * s, r.right() * s, r.bottom() * s);
        if !(fx1 - fx0 > 0.0 && fy1 - fy0 > 0.0) || c >> 24 == 0 || !self.visible_m(r, 1.0) {
            return;
        }
        let (hx, hy) = ((fx1 - fx0) / 2.0, (fy1 - fy0) / 2.0);
        let (cx, cy) = (fx0 + hx, fy0 + hy);
        let rad = (radius * s).clamp(0.0, hx.min(hy));
        let ww = (w * s).max(0.0);
        let ring = ww.is_finite();
        let (clx0, cly0, clx1, cly1) = self.clip;
        let x0 = (fx0.floor() as i32).max(clx0);
        let x1 = (fx1.ceil() as i32).min(clx1);
        let y0 = (fy0.floor() as i32).max(cly0);
        let y1 = (fy1.ceil() as i32).min(cly1);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let cov_at = |x: i32, y: i32| -> f32 {
            let d = Self::sd_rrect(x as f32 + 0.5, y as f32 + 0.5, cx, cy, hx, hy, rad);
            let outer = (0.5 - d).clamp(0.0, 1.0);
            if ring {
                outer.min((0.5 + d + ww).clamp(0.0, 1.0))
            } else {
                outer
            }
        };
        let edge = if ring { ww + rad + 1.5 } else { 1.5 };
        for y in y0..y1 {
            let py = y as f32 + 0.5;
            if py - fy0 < edge || fy1 - py < edge {
                // boundary rows: per pixel near the sides; in the middle (farther from the sides than from the
                // top / bottom edge) the distance only depends on the row: one coverage for the whole span
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
                let l_end = ((fx0.floor() as i32) + band).min(x1);
                for x in x0..l_end {
                    let cv = cov_at(x, y);
                    self.put(x, y, c, cv);
                }
                for x in ((fx1.ceil() as i32) - band).max(l_end).max(x0)..x1 {
                    let cv = cov_at(x, y);
                    self.put(x, y, c, cv);
                }
                continue;
            }
            let dy = if py < fy0 + rad { fy0 + rad - py } else if py > fy1 - rad { py - (fy1 - rad) } else { 0.0 };
            let inset = if dy > 0.0 { rad - (rad * rad - dy * dy).max(0.0).sqrt() } else { 0.0 };
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

    /// anti-aliased disc
    pub fn disc(&mut self, cx: f32, cy: f32, rad: f32, c: u32) {
        self.rrect(&Rect::new(cx - rad, cy - rad, rad * 2.0, rad * 2.0), rad, c);
    }

    /// a cheap soft shadow under an opaque rounded rect: three expanding low-alpha rings, offset down; only the band
    /// around the shape is painted (the shape covers the rest), so the cost follows the perimeter
    pub fn shadow(&mut self, r: &Rect, radius: f32, elev: f32, alpha: u32) {
        if elev <= 0.0 || !self.visible_m(r, elev * 2.0) {
            return;
        }
        for (k, a) in [(1.0f32, alpha / 2), (0.6, alpha / 3), (0.3, alpha / 3)] {
            let e = elev * k;
            let rr = Rect::new(r.x - e * 0.5, r.y + elev * 0.35 - e * 0.25, r.w + e, r.h + e);
            self.rrect_ring(&rr, radius + e * 0.5, e + elev * 0.5 + 2.0 / self.scale + radius * 0.3, a.min(255) << 24);
        }
    }

    /// evaluate a signed distance (logical px in, logical px out) over a small box: icons only
    pub fn sdf(&mut self, bbox: &Rect, c: u32, f: impl Fn(f32, f32) -> f32) {
        if !self.visible_m(bbox, 0.0) {
            return;
        }
        let s = self.scale;
        let (clx0, cly0, clx1, cly1) = self.clip;
        let x0 = ((bbox.x * s).floor() as i32).max(clx0);
        let y0 = ((bbox.y * s).floor() as i32).max(cly0);
        let x1 = ((bbox.right() * s).ceil() as i32).min(clx1);
        let y1 = ((bbox.bottom() * s).ceil() as i32).min(cly1);
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

    /// nearest-neighbour blit of an opaque `w` x `h` image into logical rect `r` (the mirror preview)
    pub fn blit(&mut self, r: &Rect, w: u32, h: u32, src: &[u32]) {
        if w == 0 || h == 0 || src.len() < (w * h) as usize || !self.visible_m(r, 0.0) {
            return;
        }
        let s = self.scale;
        let (bx0, by0) = ((r.x * s).round() as i32, (r.y * s).round() as i32);
        let (bx1, by1) = ((r.right() * s).round() as i32, (r.bottom() * s).round() as i32);
        let (dw, dh) = ((bx1 - bx0).max(1) as u32, (by1 - by0).max(1) as u32);
        let (clx0, cly0, clx1, cly1) = self.clip;
        for y in by0.max(cly0)..by1.min(cly1) {
            let sy = (((y - by0) as u32) * h / dh).min(h - 1);
            for x in bx0.max(clx0)..bx1.min(clx1) {
                let sx = (((x - bx0) as u32) * w / dw).min(w - 1);
                self.px[(y * self.bw + x) as usize] = src[(sy * w + sx) as usize] | 0xff00_0000;
            }
        }
    }
}

// ------------------------------------------------------------------ fonts + glyph cache

struct Glyph {
    cov: Vec<u8>,
    w: i32,
    h: i32,
    ox: i32,
    oy: i32,
}

pub struct Fonts {
    font: Option<FontVec>,
    /// a fallback font for emoji / symbols with outline glyphs (a colour-only bitmap / COLR font gives no
    /// outlines: such emoji show as their code point)
    emoji: Option<FontVec>,
    pub emoji_path: String,
    pub path: String,
    cache: HashMap<(char, u32), Glyph>,
    cache_bytes: usize,
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

/// the glyph cache is dropped when it grows past this (a full keyboard at 2 sizes is ~0.4 MB)
const CACHE_MAX: usize = 6 << 20;

fn main_has(font: &Option<FontVec>, c: char) -> bool {
    font.as_ref().is_some_and(|f| f.glyph_id(c).0 != 0)
}

fn font_for<'f>(font: &'f Option<FontVec>, emoji: &'f Option<FontVec>, c: char) -> Option<&'f FontVec> {
    if !main_has(font, c) {
        if let Some(e) = emoji.as_ref().filter(|f| f.glyph_id(c).0 != 0) {
            return Some(e);
        }
    }
    font.as_ref()
}

fn rasterize(f: &FontVec, c: char, size: u32) -> Glyph {
    let sf = f.as_scaled(PxScale::from(size as f32));
    let gl = sf.scaled_glyph(c);
    match f.outline_glyph(gl) {
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
            Glyph { cov, w, h, ox: bb.min.x.round() as i32, oy: bb.min.y.round() as i32 + (sf.ascent().round() as i32) }
        }
        None => Glyph { cov: vec![], w: 0, h: 0, ox: 0, oy: 0 },
    }
}

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
                    return Fonts { font: Some(f), emoji, emoji_path, path: p, cache: HashMap::new(), cache_bytes: 0 };
                }
            }
        }
        Fonts { font: None, emoji: None, emoji_path: "none".into(), path: "none".into(), cache: HashMap::new(), cache_bytes: 0 }
    }

    /// an emoji font with outlines (Noto Emoji monochrome, Symbola, ...)
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

    fn has(&self, c: char) -> bool {
        main_has(&self.font, c) || self.emoji.as_ref().is_some_and(|f| f.glyph_id(c).0 != 0)
    }

    /// replace symbols the font cannot draw (borrowed when nothing needs replacing: no allocation)
    pub fn printable<'s>(&self, s: &'s str) -> Cow<'s, str> {
        let invisible = |c: char| matches!(c, '\u{fe0f}' | '\u{fe0e}' | '\u{200d}');
        if s.chars().all(|c| !invisible(c) && (c == ' ' || self.has(c))) {
            return Cow::Borrowed(s);
        }
        let mut out = String::with_capacity(s.len());
        for c in s.chars().filter(|c| !invisible(*c)) {
            if c == ' ' || self.has(c) {
                out.push(c);
            } else {
                match c {
                    '\u{232b}' => out.push_str("Bksp"),
                    '\u{23ce}' => out.push_str("Enter"),
                    '\u{21e7}' => out.push_str("Shift"),
                    '\u{2190}' => out.push('<'),
                    '\u{2192}' => out.push('>'),
                    '\u{2191}' => out.push('^'),
                    '\u{2193}' => out.push('v'),
                    // an emoji without a drawable glyph shows its code point
                    c if (c as u32) >= 0x2300 => out.push_str(&format!("U+{:X}", c as u32)),
                    _ => out.push('?'),
                }
            }
        }
        Cow::Owned(out)
    }

    /// width of `s` at `size` buffer px
    fn width(&self, s: &str, size: f32) -> f32 {
        if self.font.is_none() {
            return 0.0;
        }
        s.chars()
            .map(|c| {
                let f = font_for(&self.font, &self.emoji, c).unwrap();
                f.as_scaled(PxScale::from(size)).h_advance(f.glyph_id(c))
            })
            .sum()
    }

    pub fn ok(&self) -> bool {
        self.font.is_some()
    }

    /// glyph cache entries and coverage bytes
    pub fn cache_stats(&self) -> (usize, usize) {
        (self.cache.len(), self.cache_bytes)
    }

    /// text in `r` (logical), `size` logical px, aligned; never overflows: shrinks down to `min_frac` of the size,
    /// then ellipsizes with "…" (or "..." without that glyph). Returns the drawn width (logical px). Culled (no
    /// work at all) when `r` is outside the canvas clip.
    #[allow(clippy::too_many_arguments)]
    pub fn text(&mut self, cv: &mut Canvas, r: &Rect, s: &str, size: f32, color: u32, align: Align, min_frac: f32) -> f32 {
        if self.font.is_none() || s.is_empty() || r.w <= 0.0 || r.h <= 0.0 || !cv.visible(r) {
            return 0.0;
        }
        let ps = self.printable(s);
        let maxw = r.w * cv.scale;
        let mut px = (size.min(r.h) * cv.scale).max(6.0);
        let w = self.width(&ps, px);
        if w > maxw && w > 0.0 {
            px = (px * maxw / w).max(px * min_frac.clamp(0.3, 1.0)).max(6.0);
        }
        let px = px.round();
        let fits = self.width(&ps, px) <= maxw;
        let owned;
        let s: &str = if fits {
            &ps
        } else {
            let ell = if self.has('\u{2026}') { "\u{2026}" } else { "..." };
            let ew = self.width(ell, px);
            let mut t = ps.to_string();
            while !t.is_empty() && self.width(&t, px) + ew > maxw {
                t.pop();
            }
            let n = t.trim_end().len();
            t.truncate(n);
            t.push_str(ell);
            owned = t;
            &owned
        };
        let w = self.width(s, px);
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
        self.draw_at(cv, x0, top, s, px as u32, color);
        w / cv.scale
    }

    /// width in logical px of `s` at logical `size`
    pub fn measure(&self, s: &str, size: f32, scale: f32) -> f32 {
        self.width(&self.printable(s), (size * scale).round()) / scale
    }

    fn draw_at(&mut self, cv: &mut Canvas, x0: f32, top: f32, s: &str, size: u32, color: u32) {
        if self.cache_bytes > CACHE_MAX {
            self.cache.clear();
            self.cache_bytes = 0;
        }
        let mut x = x0;
        let (cr, cg, cb) = ((color >> 16) & 255, (color >> 8) & 255, color & 255);
        let ca = color >> 24;
        let (clx0, cly0, clx1, cly1) = cv.clip;
        for ch in s.chars() {
            let Some(f) = font_for(&self.font, &self.emoji, ch) else { return };
            let adv = f.as_scaled(PxScale::from(size as f32)).h_advance(f.glyph_id(ch));
            if !self.cache.contains_key(&(ch, size)) {
                let g = rasterize(f, ch, size);
                self.cache_bytes += g.cov.len() + 32;
                self.cache.insert((ch, size), g);
            }
            let g = &self.cache[&(ch, size)];
            let ox = x.round() as i32 + g.ox;
            let oy = top.round() as i32 + g.oy;
            for yy in (cly0 - oy).max(0)..g.h.min(cly1 - oy) {
                let py = oy + yy;
                for xx in (clx0 - ox).max(0)..g.w.min(clx1 - ox) {
                    let a = g.cov[(yy * g.w + xx) as usize] as u32 * ca / 255;
                    if a == 0 {
                        continue;
                    }
                    let i = (py * cv.bw + ox + xx) as usize;
                    cv.px[i] = blend(cv.px[i], (a << 24) | (cr << 16) | (cg << 8) | cb);
                }
            }
            x += adv;
        }
    }
}
