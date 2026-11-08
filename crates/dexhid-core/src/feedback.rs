// SPDX-License-Identifier: GPL-3.0-or-later
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

/// M2.1: the desktop's look (KDE: kdeglobals colours, the panel opacity / blur preference, a corner radius)
#[derive(Clone, Debug, PartialEq, Default)]
pub struct DeskLook {
    /// colour scheme name ([General] ColorScheme)
    pub scheme: String,
    pub window: u32,
    pub view: u32,
    pub button: u32,
    pub text: u32,
    pub text_dim: u32,
    pub accent: u32,
    /// panels translucent (plasmashellrc panelOpacity 0 = adaptive or 2 = translucent)
    pub translucent: bool,
    /// KWin blur effect on
    pub blur: bool,
    /// corner radius (mm)
    pub radius_mm: f32,
}

/// INI-style config (KConfig): (group, key) -> value; nested groups are joined as "A][B"
pub fn parse_ini(text: &str) -> std::collections::HashMap<(String, String), String> {
    let mut m = std::collections::HashMap::new();
    let mut g = String::new();
    for l in text.lines() {
        let l = l.trim();
        if l.starts_with('[') && l.ends_with(']') {
            g = l[1..l.len() - 1].to_string();
        } else if let Some((k, v)) = l.split_once('=') {
            if !l.starts_with('#') {
                // drop KConfig flags like Key[$e]
                let k = k.trim().split('[').next().unwrap_or("").to_string();
                m.insert((g.clone(), k), v.trim().to_string());
            }
        }
    }
    m
}

fn kcolor(v: &str) -> Option<u32> {
    if v.starts_with('#') {
        return parse_rgb(v);
    }
    let p: Vec<u32> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
    if p.len() < 3 {
        return None;
    }
    Some(0xff00_0000 | (p[0].min(255) << 16) | (p[1].min(255) << 8) | p[2].min(255))
}

/// mix two colours: `t` = share of `b` (0..1), opaque result
pub fn mix(a: u32, b: u32, t: f32) -> u32 {
    let ch = |s: u32| -> u32 {
        let x = ((a >> s) & 255) as f32;
        let y = ((b >> s) & 255) as f32;
        ((x + (y - x) * t).round() as u32).min(255) << s
    };
    0xff00_0000 | ch(16) | ch(8) | ch(0)
}

/// the desktop look from KDE's files (kdeglobals text, plasmashellrc / kwinrc text if present, a decoration corner
/// radius in px if one is configured). None = no colour scheme colours in kdeglobals.
pub fn desk_look(kdeglobals: &str, plasmashellrc: Option<&str>, kwinrc: Option<&str>, radius_px: Option<f32>) -> Option<DeskLook> {
    let k = parse_ini(kdeglobals);
    let c = |g: &str, key: &str| k.get(&(g.to_string(), key.to_string())).and_then(|v| kcolor(v));
    let window = c("Colors:Window", "BackgroundNormal")?;
    let text = c("Colors:Window", "ForegroundNormal").unwrap_or(0xffe8ecf2);
    let accent = c("General", "AccentColor").or_else(|| c("Colors:Selection", "BackgroundNormal")).unwrap_or(0xff3daee9);
    let translucent = plasmashellrc.map(parse_ini).map(|p| p.iter().any(|((g, key), v)| g.starts_with("PlasmaViews][Panel") && key == "panelOpacity" && (v == "0" || v == "2"))).unwrap_or(true);
    let blur = kwinrc.map(parse_ini).and_then(|p| p.get(&("Plugins".to_string(), "blurEnabled".to_string())).cloned()).map(|v| v != "false").unwrap_or(true);
    Some(DeskLook {
        scheme: k.get(&("General".to_string(), "ColorScheme".to_string())).cloned().unwrap_or_default(),
        window,
        view: c("Colors:View", "BackgroundNormal").unwrap_or(window),
        button: c("Colors:Button", "BackgroundNormal").unwrap_or(window),
        text,
        text_dim: c("Colors:Window", "ForegroundInactive").unwrap_or_else(|| mix(text, window, 0.45)),
        accent,
        translucent,
        blur,
        // px at 96 dpi -> mm; Breeze's default corner is about 5 px
        radius_mm: radius_px.unwrap_or(5.0) * 25.4 / 96.0,
    })
}

/// a palette from the desktop look (theme "desktop")
pub fn palette_desk(d: &DeskLook, key_alpha: u32) -> Palette {
    let mut p = Palette {
        bg: mix(d.window, 0xff00_0000, 0.12),
        strip: d.window,
        btn: d.button,
        key: d.button,
        key_mod: mix(d.button, d.window, 0.5),
        pad: d.view,
        pad_edge: mix(d.view, d.text, 0.22),
        zone: mix(d.view, d.button, 0.5),
        text: d.text,
        text_dim: d.text_dim,
        accent: d.accent,
        edit: 0xffe0a030,
        card: (d.window & 0x00ff_ffff) | 0xf800_0000,
    };
    if d.translucent {
        // the panel material: translucent surfaces over the panel background
        p.strip = (p.strip & 0x00ff_ffff) | 0xd800_0000;
        p.pad = (p.pad & 0x00ff_ffff) | 0xd000_0000;
        p.zone = (p.zone & 0x00ff_ffff) | 0xd000_0000;
    }
    if key_alpha < 100 {
        p.key = with_alpha(p.key, key_alpha);
        p.key_mod = with_alpha(p.key_mod, key_alpha);
    } else if d.translucent {
        p.key = (p.key & 0x00ff_ffff) | 0xe800_0000;
        p.key_mod = (p.key_mod & 0x00ff_ffff) | 0xe000_0000;
    }
    p
}

/// does an input device look like a physical keyboard (a BT / USB keyboard or the keyboard cover)?
/// `bus` = id bustype; virtual devices (BUS_VIRTUAL 0x06) and our own never count.
pub fn is_physical_keyboard(name: &str, bus: u16, has_letters: bool) -> bool {
    has_letters && bus != 0x06 && !name.starts_with("dexhid") && !name.contains("gpio-keys") && !name.contains("pmic")
}

/// a bit of a sysfs capability bitmap (`/sys/class/input/inputN/capabilities/key`: hex words, highest first,
/// each word one `unsigned long` = 64 bits on arm64 / x86_64)
pub fn caps_has(hex: &str, bit: usize) -> bool {
    let words: Vec<&str> = hex.split_whitespace().collect();
    let (w, b) = (bit / 64, bit % 64);
    if w >= words.len() {
        return false;
    }
    let s = words[words.len() - 1 - w];
    u64::from_str_radix(s, 16).map(|v| v >> b & 1 == 1).unwrap_or(false)
}

/// physical keyboards from sysfs (readable without access to the event nodes): (name, bustype)
pub fn sysfs_keyboards(class_input: &Path) -> Vec<(String, u16)> {
    let mut v = vec![];
    let Ok(rd_) = std::fs::read_dir(class_input) else { return v };
    let mut ents: Vec<_> = rd_.flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("input"))).collect();
    ents.sort();
    for p in ents {
        let name = rd(&p.join("name")).unwrap_or_default();
        let bus = rd(&p.join("id/bustype")).and_then(|s| u16::from_str_radix(&s, 16).ok()).unwrap_or(0);
        let caps = rd(&p.join("capabilities/key")).unwrap_or_default();
        let letters = caps_has(&caps, 30) && caps_has(&caps, 44) && caps_has(&caps, 57);
        if is_physical_keyboard(&name, bus, letters) {
            v.push((name, bus));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M2.1 item 9: the KDE look from kdeglobals / plasmashellrc / kwinrc
    #[test]
    fn desk_look_from_kde_files() {
        let kg = "[General]\nColorScheme=BreezeDark\n\n[Colors:Window]\nBackgroundNormal=32,35,38\nForegroundNormal=252,252,252\n\n[Colors:View]\nBackgroundNormal=20,22,24\n\n[Colors:Button]\nBackgroundNormal=41,44,48\n\n[Colors:Selection]\nBackgroundNormal=61,174,233\n";
        let ps = "[PlasmaViews][Panel 2]\nfloating=1\n\n[PlasmaViews][Panel 2][Defaults]\nthickness=44\n\n[PlasmaViews][Panel 2]\npanelOpacity=1\n";
        let kw = "[Plugins]\nblurEnabled=false\n";
        let d = desk_look(kg, Some(ps), Some(kw), Some(10.0)).unwrap();
        assert_eq!(d.scheme, "BreezeDark");
        assert_eq!(d.window, 0xff202326);
        assert_eq!(d.view, 0xff141618);
        assert_eq!(d.button, 0xff292c30);
        assert_eq!(d.accent, 0xff3daee9, "selection colour without an AccentColor");
        assert!(!d.translucent && !d.blur, "opaque panel, blur off");
        assert!((d.radius_mm - 2.646).abs() < 0.01);
        let p = palette_desk(&d, 100);
        assert_eq!(p.strip, 0xff202326);
        assert_eq!(p.text, 0xfffcfcfc);
        // AccentColor wins, translucent panels make the surfaces translucent
        let kg2 = format!("{}[General]\nAccentColor=#e93d58\n", kg);
        let d2 = desk_look(&kg2, Some("[PlasmaViews][Panel 7]\npanelOpacity=2\n"), None, None).unwrap();
        assert_eq!(d2.accent, 0xffe93d58);
        assert!(d2.translucent && d2.blur);
        assert_eq!(palette_desk(&d2, 100).strip >> 24, 0xd8);
        assert!(desk_look("[General]\nfoo=1\n", None, None, None).is_none(), "no colours: built-in theme");
    }

    #[test]
    fn sysfs_keyboard_scan() {
        // KEY_A (30), KEY_Z (44), KEY_SPACE (57) set in the lowest word
        let low = (1u64 << 30) | (1u64 << 44) | (1u64 << 57);
        assert!(caps_has(&format!("10000 0 {:x}", low), 57));
        assert!(!caps_has(&format!("{:x}", 1u64 << 30), 44));
        assert!(caps_has("1 0", 64));
        let d = std::env::temp_dir().join(format!("dexhid-in-{}", std::process::id()));
        for (n, name, bus, caps) in [("input3", "Keyboard Case", "0005", format!("0 {:x}", low)), ("input4", "dexhid keyboard", "0006", format!("{:x}", low)), ("input5", "gpio-keys", "0019", "e000000000000 0".to_string())] {
            let p = d.join(n);
            std::fs::create_dir_all(p.join("id")).unwrap();
            std::fs::create_dir_all(p.join("capabilities")).unwrap();
            std::fs::write(p.join("name"), format!("{}\n", name)).unwrap();
            std::fs::write(p.join("id/bustype"), format!("{}\n", bus)).unwrap();
            std::fs::write(p.join("capabilities/key"), format!("{}\n", caps)).unwrap();
        }
        assert_eq!(sysfs_keyboards(&d), vec![("Keyboard Case".to_string(), 5)]);
        std::fs::remove_dir_all(&d).ok();
    }

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
