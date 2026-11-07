// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! Small pure helpers for M2 features that touch the system: the key-click sound (a generated WAV), the status
//! text of the bar (battery / charger / external display, from sysfs values), the brightness to dim to, the theme
//! palette, and the physical-keyboard test.

use std::path::Path;

/// a short soft click: 16-bit mono PCM WAV, 48 kHz, ~12 ms decaying 2 kHz tone
pub fn click_wav() -> Vec<u8> {
    let rate = 48_000u32;
    let n = (rate / 1000 * 12) as usize;
    let mut pcm = Vec::with_capacity(n * 2);
    for i in 0..n {
        let t = i as f32 / rate as f32;
        let env = (-t * 450.0).exp();
        let v = (t * 2000.0 * std::f32::consts::TAU).sin() * env * 0.35;
        pcm.extend_from_slice(&((v * i16::MAX as f32) as i16).to_le_bytes());
    }
    let mut w = Vec::with_capacity(44 + pcm.len());
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    w.extend_from_slice(&pcm);
    w
}

fn rd(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

/// Status text for the bar from a power_supply directory (normally /sys/class/power_supply) and the external
/// connector name: e.g. "84 % charging  |  DP-1". Battery = type Battery with a capacity; charger = any
/// Mains / USB supply online.
pub fn status_text(power_supply: &Path, external: Option<&str>) -> String {
    let mut cap = None;
    let mut status = String::new();
    let mut online = false;
    if let Ok(rd_) = std::fs::read_dir(power_supply) {
        let mut ents: Vec<_> = rd_.flatten().map(|e| e.path()).collect();
        ents.sort();
        for p in ents {
            let ty = rd(&p.join("type")).unwrap_or_default();
            if ty == "Battery" && cap.is_none() {
                if let Some(c) = rd(&p.join("capacity")).and_then(|s| s.parse::<u32>().ok()) {
                    cap = Some(c);
                    status = rd(&p.join("status")).unwrap_or_default();
                }
            } else if ty != "Battery" && rd(&p.join("online")).as_deref() == Some("1") {
                online = true;
            }
        }
    }
    let mut parts = vec![];
    if let Some(c) = cap {
        let s = match status.as_str() {
            "Charging" => " charging",
            "Full" => " full",
            _ if online => " plugged in",
            _ => "",
        };
        parts.push(format!("{} %{}", c, s));
    } else if online {
        parts.push("charger".into());
    }
    if let Some(e) = external {
        parts.push(e.to_string());
    }
    parts.join("  |  ")
}

/// the brightness to dim to: `percent` of max, at least 1, never above the current value
pub fn dim_value(cur: u32, max: u32, percent: u32) -> u32 {
    let v = ((max as u64 * percent.min(100) as u64) / 100) as u32;
    v.max(1).min(cur.max(1))
}

/// "#rrggbb" -> 0xffrrggbb
pub fn parse_rgb(s: &str) -> Option<u32> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    u32::from_str_radix(h, 16).ok().map(|v| 0xff00_0000 | v)
}

/// the colours the renderer uses
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub bg: u32,
    pub strip: u32,
    pub btn: u32,
    pub key: u32,
    pub key_mod: u32,
    pub pad: u32,
    pub pad_edge: u32,
    pub zone: u32,
    pub text: u32,
    pub text_dim: u32,
    pub accent: u32,
    pub edit: u32,
    pub card: u32,
}

fn with_alpha(c: u32, pct: u32) -> u32 {
    let a = (255 * pct.clamp(10, 100) / 100) & 255;
    (a << 24) | (c & 0x00ff_ffff)
}

/// theme name ("dark" | "light" | "amoled"; "auto" is resolved by the caller), accent, key opacity %
pub fn palette(theme: &str, accent: &str, key_alpha: u32) -> Palette {
    let accent = parse_rgb(accent).unwrap_or(0xff4a8be0);
    let mut p = match theme {
        "light" => Palette {
            bg: 0xffe9ecf1,
            strip: 0xffd5dae2,
            btn: 0xffc3cad5,
            key: 0xfffbfcfd,
            key_mod: 0xffdfe3ea,
            pad: 0xfff4f6f9,
            pad_edge: 0xffaab3c0,
            zone: 0xffe6e9ee,
            text: 0xff151a21,
            text_dim: 0xff5a6472,
            accent,
            edit: 0xffc07800,
            card: 0xf8f4f6f9,
        },
        "amoled" => Palette {
            bg: 0xff000000,
            strip: 0xff000000,
            btn: 0xff15181d,
            key: 0xff0c0e11,
            key_mod: 0xff060709,
            pad: 0xff000000,
            pad_edge: 0xff262c34,
            zone: 0xff07080a,
            text: 0xffc9ced6,
            text_dim: 0xff6b7380,
            accent,
            edit: 0xffe0a030,
            card: 0xf8090a0c,
        },
        _ => Palette {
            bg: 0xff101318,
            strip: 0xff1b2028,
            btn: 0xff2b3442,
            key: 0xff262c36,
            key_mod: 0xff1f242c,
            pad: 0xff181d24,
            pad_edge: 0xff3a4554,
            zone: 0xff1d232c,
            text: 0xffe8ecf2,
            text_dim: 0xff8b95a5,
            accent,
            edit: 0xffe0a030,
            card: 0xf8222a35,
        },
    };
    if key_alpha < 100 {
        p.key = with_alpha(p.key, key_alpha);
        p.key_mod = with_alpha(p.key_mod, key_alpha);
    }
    p
}

/// does an input device look like a physical keyboard (a BT / USB keyboard or the keyboard cover)?
/// `bus` = id bustype; virtual devices (BUS_VIRTUAL 0x06) and our own never count.
pub fn is_physical_keyboard(name: &str, bus: u16, has_letters: bool) -> bool {
    has_letters && bus != 0x06 && !name.starts_with("dexhid") && !name.contains("gpio-keys") && !name.contains("pmic")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_and_values() {
        let w = click_wav();
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(&w[8..12], b"WAVE");
        assert_eq!(u32::from_le_bytes([w[40], w[41], w[42], w[43]]) as usize, w.len() - 44);
        assert_eq!(dim_value(800, 1000, 20), 200);
        assert_eq!(dim_value(100, 1000, 20), 100, "never brighter than now");
        assert_eq!(dim_value(800, 1000, 0), 1);
        assert_eq!(parse_rgb("#4a8be0"), Some(0xff4a8be0));
        assert_eq!(parse_rgb("4a8be0"), None);
        assert_eq!(palette("amoled", "#ff0000", 100).bg, 0xff000000);
        assert_eq!(palette("dark", "#ff0000", 100).accent, 0xffff0000);
        assert_eq!(palette("light", "x", 50).key >> 24, 127);
        assert!(is_physical_keyboard("Keyboard Case BT", 0x05, true));
        assert!(!is_physical_keyboard("dexhid keyboard", 0x06, true));
        assert!(!is_physical_keyboard("Test BT Keyboard", 0x06, true), "virtual bus");
        assert!(!is_physical_keyboard("gpio-keys", 0x19, false));
    }

    #[test]
    fn status_from_fake_sysfs() {
        let d = std::env::temp_dir().join(format!("dexhid-ps-{}", std::process::id()));
        let b = d.join("battery");
        let u = d.join("usb");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::create_dir_all(&u).unwrap();
        std::fs::write(b.join("type"), "Battery\n").unwrap();
        std::fs::write(b.join("capacity"), "84\n").unwrap();
        std::fs::write(b.join("status"), "Charging\n").unwrap();
        std::fs::write(u.join("type"), "USB\n").unwrap();
        std::fs::write(u.join("online"), "1\n").unwrap();
        assert_eq!(status_text(&d, Some("DP-1")), "84 % charging  |  DP-1");
        std::fs::write(b.join("status"), "Discharging\n").unwrap();
        assert_eq!(status_text(&d, None), "84 % plugged in");
        std::fs::write(u.join("online"), "0\n").unwrap();
        assert_eq!(status_text(&d, None), "84 %");
        std::fs::remove_dir_all(&d).ok();
        assert_eq!(status_text(Path::new("/nonexistent-dexhid"), Some("DP-1")), "DP-1");
    }
}
