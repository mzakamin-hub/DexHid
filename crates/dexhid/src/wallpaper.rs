// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! M2.1: the desktop wallpaper behind the deck (theme "desktop" + `wallpaper = true`). Plasma gives a file URL or a
//! wallpaper package directory (contents/images/<W>x<H>.<ext>); PNG and JPEG are decoded (pure Rust), other formats
//! (JPEG XL, WebP, SVG) are reported and the plain background stays. The image is scaled to cover the panel,
//! box-blurred when the desktop blurs its panels, and darkened so the legends stay readable. Loaded once per path /
//! size in the UI thread.

use std::path::{Path, PathBuf};

/// pick the image of a wallpaper package closest to the target size
fn from_package(dir: &Path, bw: i32, bh: i32) -> Option<PathBuf> {
    let mut best: Option<(i64, PathBuf)> = None;
    for sub in ["contents/images", "contents/images_dark"] {
        let Ok(rd) = std::fs::read_dir(dir.join(sub)) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("").to_ascii_lowercase();
            if !matches!(ext.as_str(), "png" | "jpg" | "jpeg") {
                continue;
            }
            let stem = p.file_stem().and_then(|x| x.to_str()).unwrap_or("");
            let (w, h) = stem.split_once('x').map(|(a, b)| (a.parse::<i64>().unwrap_or(0), b.parse::<i64>().unwrap_or(0))).unwrap_or((0, 0));
            // prefer the smallest image that covers the panel, else the biggest
            let score = if w >= bw as i64 && h >= bh as i64 { w * h } else { i64::MAX / 2 - w * h };
            if best.as_ref().is_none_or(|b| score < b.0) {
                best = Some((score, p));
            }
        }
        if best.is_some() {
            break;
        }
    }
    best.map(|b| b.1)
}

/// resolve a Plasma "Image" value (file:// URL, path or package directory) to an image file
pub fn resolve(v: &str, bw: i32, bh: i32) -> Result<PathBuf, String> {
    let p = PathBuf::from(v.strip_prefix("file://").unwrap_or(v));
    if p.is_dir() {
        return from_package(&p, bw, bh).ok_or_else(|| format!("{}: no PNG / JPEG in the wallpaper package", p.display()));
    }
    if p.is_file() {
        return Ok(p);
    }
    Err(format!("{}: not found", p.display()))
}

/// decode to (w, h, 0xffRRGGBB pixels)
pub fn decode(p: &Path) -> Result<(usize, usize, Vec<u32>), String> {
    let bytes = std::fs::read(p).map_err(|e| format!("{}: {}", p.display(), e))?;
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        let mut d = png::Decoder::new(std::io::Cursor::new(&bytes));
        d.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut r = d.read_info().map_err(|e| format!("png: {}", e))?;
        let mut buf = vec![0u8; r.output_buffer_size()];
        let info = r.next_frame(&mut buf).map_err(|e| format!("png: {}", e))?;
        let (w, h) = (info.width as usize, info.height as usize);
        let n = match info.color_type {
            png::ColorType::Rgba => 4,
            png::ColorType::Rgb => 3,
            png::ColorType::GrayscaleAlpha => 2,
            _ => 1,
        };
        let px = (0..w * h)
            .map(|i| {
                let s = &buf[i * n..i * n + n];
                let (r, g, b) = if n >= 3 { (s[0], s[1], s[2]) } else { (s[0], s[0], s[0]) };
                0xff00_0000 | (r as u32) << 16 | (g as u32) << 8 | b as u32
            })
            .collect();
        return Ok((w, h, px));
    }
    if bytes.starts_with(&[0xff, 0xd8]) {
        let mut d = zune_jpeg::JpegDecoder::new(&bytes);
        let buf = d.decode().map_err(|e| format!("jpeg: {:?}", e))?;
        let (w, h) = d.dimensions().ok_or("jpeg: no size")?;
        let n = (buf.len() / (w * h).max(1)).max(1);
        let px = (0..w * h)
            .map(|i| {
                let s = &buf[i * n..i * n + n];
                let (r, g, b) = if n >= 3 { (s[0], s[1], s[2]) } else { (s[0], s[0], s[0]) };
                0xff00_0000 | (r as u32) << 16 | (g as u32) << 8 | b as u32
            })
            .collect();
        return Ok((w, h, px));
    }
    Err(format!("{}: format not supported (PNG / JPEG only)", p.display()))
}

/// scale to cover bw x bh (area average when shrinking)
pub fn cover(w: usize, h: usize, src: &[u32], bw: usize, bh: usize) -> Vec<u32> {
    let s = (bw as f32 / w as f32).max(bh as f32 / h as f32);
    let ox = (w as f32 * s - bw as f32) / 2.0;
    let oy = (h as f32 * s - bh as f32) / 2.0;
    let k = (1.0 / s).ceil().max(1.0) as usize;
    let mut out = vec![0u32; bw * bh];
    for y in 0..bh {
        let sy0 = (((y as f32 + oy) / s) as usize).min(h - 1);
        for x in 0..bw {
            let sx0 = (((x as f32 + ox) / s) as usize).min(w - 1);
            let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
            for yy in sy0..(sy0 + k).min(h) {
                for xx in sx0..(sx0 + k).min(w) {
                    let p = src[yy * w + xx];
                    r += p >> 16 & 255;
                    g += p >> 8 & 255;
                    b += p & 255;
                    n += 1;
                }
            }
            out[y * bw + x] = 0xff00_0000 | (r / n) << 16 | (g / n) << 8 | (b / n);
        }
    }
    out
}

/// three passes of a box blur (approximates a gaussian), radius in px
pub fn blur(px: &mut [u32], w: usize, h: usize, rad: usize) {
    if rad == 0 || w == 0 || h == 0 {
        return;
    }
    let mut tmp = vec![0u32; px.len()];
    for _ in 0..3 {
        // rows (step 1), then columns (step w)
        for (step, n, lines) in [(1usize, w, h), (w, h, w)] {
            for l in 0..lines {
                let base = if step == 1 { l * w } else { l };
                let (mut r, mut g, mut b) = (0i64, 0i64, 0i64);
                let get = |i: isize, px: &[u32]| -> u32 { px[base + (i.clamp(0, n as isize - 1) as usize) * step] };
                for i in -(rad as isize)..=(rad as isize) {
                    let p = get(i, px);
                    r += (p >> 16 & 255) as i64;
                    g += (p >> 8 & 255) as i64;
                    b += (p & 255) as i64;
                }
                let d = (2 * rad + 1) as i64;
                for i in 0..n as isize {
                    tmp[base + i as usize * step] = 0xff00_0000 | ((r / d) as u32) << 16 | ((g / d) as u32) << 8 | (b / d) as u32;
                    let a = get(i + rad as isize + 1, px);
                    let o = get(i - rad as isize, px);
                    r += (a >> 16 & 255) as i64 - (o >> 16 & 255) as i64;
                    g += (a >> 8 & 255) as i64 - (o >> 8 & 255) as i64;
                    b += (a & 255) as i64 - (o & 255) as i64;
                }
            }
            px.copy_from_slice(&tmp);
        }
    }
}

/// the wallpaper buffer for a bw x bh canvas
pub fn load(v: &str, bw: i32, bh: i32, blurred: bool) -> Result<Vec<u32>, String> {
    let p = resolve(v, bw, bh)?;
    let (w, h, src) = decode(&p)?;
    if w == 0 || h == 0 {
        return Err("empty image".into());
    }
    let (bw, bh) = (bw.max(1) as usize, bh.max(1) as usize);
    let mut px = cover(w, h, &src, bw, bh);
    if blurred {
        blur(&mut px, bw, bh, (bw / 90).max(2));
    }
    for p in px.iter_mut() {
        *p = dexhid_core::feedback::mix(*p, 0xff00_0000, 0.35);
    }
    Ok(px)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_blur_and_png_roundtrip() {
        // 4 x 2 image: left half red, right half blue; cover into 2 x 2 crops the sides
        let src = vec![0xffff0000, 0xffff0000, 0xff0000ff, 0xff0000ff, 0xffff0000, 0xffff0000, 0xff0000ff, 0xff0000ff];
        let c = cover(4, 2, &src, 2, 2);
        assert_eq!(c.len(), 4);
        assert_eq!(c[0] & 0xffffff, 0xff0000);
        assert_eq!(c[1] & 0xffffff, 0x0000ff);
        let mut b = vec![0xff000000u32; 9];
        b[4] = 0xffffffff;
        blur(&mut b, 3, 3, 1);
        assert!(b[4] & 255 < 255 && b[0] & 255 > 0, "spread out");
        // a real PNG file through decode + load
        let d = std::env::temp_dir().join(format!("dexhid-wp-{}", std::process::id()));
        std::fs::create_dir_all(d.join("pkg/contents/images")).unwrap();
        let f = d.join("pkg/contents/images/8x4.png");
        {
            let file = std::fs::File::create(&f).unwrap();
            let mut e = png::Encoder::new(std::io::BufWriter::new(file), 8, 4);
            e.set_color(png::ColorType::Rgb);
            e.set_depth(png::BitDepth::Eight);
            let mut w = e.write_header().unwrap();
            w.write_image_data(&[200u8; 8 * 4 * 3]).unwrap();
        }
        assert_eq!(resolve(&format!("file://{}", d.join("pkg").display()), 4, 2).unwrap(), f);
        let px = load(f.to_str().unwrap(), 4, 2, true).unwrap();
        assert_eq!(px.len(), 8);
        assert_eq!(px[0] & 255, 130, "200 darkened by 35 %");
        std::fs::write(d.join("x.jxl"), b"\xff\x0a").unwrap();
        assert!(load(d.join("x.jxl").to_str().unwrap(), 4, 2, false).unwrap_err().contains("not supported"));
        std::fs::remove_dir_all(&d).ok();
    }
}
