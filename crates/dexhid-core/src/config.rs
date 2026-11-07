// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! Configuration (TOML), merged in order: built-in defaults, /usr/share/dexhid/devices/*.toml (device presets whose
//! `match_compatible` matches /proc/device-tree/compatible), /etc/dexhid/config.toml,
//! $XDG_CONFIG_HOME/dexhid/config.toml, then `--config <file>`. Later files override single keys.
//! The app's own choices (current keyboard, autostart from the settings sheet, remembered monitors) live in
//! $XDG_STATE_HOME/dexhid/state.toml, never in config.toml.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// device preset fields (ignored at run time)
    pub match_compatible: Vec<String>,
    /// evdev name of the internal touchscreen
    pub touchscreen: String,
    /// explicit event node (overrides `touchscreen`; tests use the replay device)
    pub touch_dev: String,
    pub pen: String,
    /// wl_output name of the internal panel, or "auto" (first DSI / eDP / LVDS output)
    pub internal_output: String,
    /// override of the touch -> output transform ("" = follow the output; normal, 90, 180, 270, flipped, ...)
    pub touch_transform: String,
    /// panel size in mm in its native orientation
    pub size_mm: [f32; 2],
    /// DRM connectors that count as an external display (glob)
    pub external_connectors: Vec<String>,
    /// "ask" | "on" | "off"
    pub autostart: String,
    /// debounce of a plug (ms); unplug acts at once
    pub plug_debounce_ms: u64,
    /// also require a non-internal wl_output (rules out a plugged but disabled monitor)
    pub require_compositor_output: bool,
    /// default keyboard layout: full | tablet | split | none | a user layout name
    pub keyboard: String,
    /// optional rows / parts shown: "function", "numpad"
    pub keyboard_options: Vec<String>,
    /// planned (M2): pad only while a physical keyboard is attached
    pub pad_only_with_physical_keyboard: bool,
    pub release_chord: Vec<String>,
    pub release_hold_ms: i64,
    pub five_finger_release: bool,
    pub click_zones: bool,
    pub click_zone_mm: f32,
    /// pointer speed factor (scales the touchpad's declared resolution)
    pub speed: f32,
    /// key hold delay (ms) before a held key presses
    pub hold_ms: i64,
    /// pad contacts with ABS_MT_TOUCH_MAJOR above this never reach the touchpad (0 = off)
    pub palm_major: i32,
    /// show the small "Use as pad" pill while the monitor is used as a display
    pub display_pill: bool,
    pub strip_mm: f32,
    pub key_gap_mm: f32,
    /// xkb layout group used for the legends (the active group is not visible to an unfocused client)
    pub xkb_layout_index: u32,
    /// "dark" | "light" | "amoled" | "auto" (follow the desktop's colour scheme)
    pub theme: String,
    // ---- M2 settings (all in the settings sheet, see settings.rs)
    /// "touchpad" (libinput touchpad) | "mouse" (own recognizer, relative mouse)
    pub mode: String,
    /// applied through the desktop's input settings (KWin InputDevice) for the virtual touchpad
    pub natural_scroll: bool,
    pub tap_to_click: bool,
    /// desktop pointer acceleration -1..1 (KWin pointerAcceleration)
    pub pointer_accel: f32,
    /// "zones+tap" | "tap" | "zones": DexHid click zones and / or the desktop's two-finger tap
    pub right_click: String,
    /// dim the internal panel while in pad mode (logind SetBrightness), restored on release / `dexhid --restore`
    pub dim: bool,
    pub dim_percent: u32,
    /// backlight device name in /sys/class/backlight, or "auto" (the first one)
    pub backlight: String,
    pub sticky_modifiers: bool,
    /// long press on a key with an alternate (`long = ...` in the layout) sends the alternate
    pub long_press_ms: i64,
    /// palm check (palm_major) also on the keyboard regions
    pub palm_keyboard: bool,
    /// touches starting this close to the panel's left / right / bottom edge are ignored (0 = off)
    pub rest_zone_mm: f32,
    /// extra rest zones: [x, y, w, h] fractions of the area below the bar
    pub rest_zones: Vec<[f32; 4]>,
    /// pad touches starting this soon after a key press are ignored (own disable-while-typing, 0 = off)
    pub dwt_ms: i64,
    /// ignore new touches while the pen is in proximity
    pub pen_pause: bool,
    /// accent colour "#rrggbb"
    pub accent: String,
    /// key opacity in %
    pub key_alpha: u32,
    /// battery / charger / external display info in the bar
    pub status_info: bool,
    pub sound_feedback: bool,
    pub haptic_feedback: bool,
    /// follow the desktop's active keyboard layout for the legends (KDE: org.kde.keyboard)
    pub track_layout: bool,
    pub quick_actions: QuickCfg,
    /// planned feature modules (stored and shown in the settings sheet; acted on from M3)
    pub features: Features,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct QuickCfg {
    pub enabled: bool,
    /// shown actions in order; "id" or "id:2" (double width)
    pub actions: Vec<String>,
    pub custom: Vec<CustomAction>,
}

impl Default for QuickCfg {
    fn default() -> QuickCfg {
        QuickCfg {
            enabled: true,
            actions: ["copy", "paste", "undo", "redo", "alt_tab", "show_desktop", "screenshot", "vol_down", "vol_up", "play_pause"].iter().map(|s| s.to_string()).collect(),
            custom: vec![],
        }
    }
}

/// a user action: a key chord ("LEFTCTRL+LEFTSHIFT+T") or a command line (argv list, no shell)
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct CustomAction {
    pub id: String,
    pub label: String,
    pub keys: String,
    pub command: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Feature {
    pub enabled: bool,
    pub options: BTreeMap<String, toml::Value>,
}

fn feat(opts: &[(&str, toml::Value)]) -> Feature {
    Feature { enabled: false, options: opts.iter().map(|(k, v)| (k.to_string(), v.clone())).collect() }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Features {
    pub gestures: Feature,
    pub clipboard: Feature,
    pub textedit: Feature,
    pub remote: Feature,
    pub pen: Feature,
    pub suggest: Feature,
    pub profiles: Feature,
    pub mirror: Feature,
    pub wireless: Feature,
}

impl Default for Features {
    fn default() -> Features {
        use toml::Value::{Boolean as B, Integer as I, String as S};
        Features {
            gestures: feat(&[("three_swipe", S("desktop".into())), ("four_swipe", S("overview".into())), ("three_tap", S("middle-click".into()))]),
            clipboard: feat(&[("items", I(8))]),
            textedit: feat(&[("joystick", B(false))]),
            remote: feat(&[("laser", B(false)), ("timer_min", I(0))]),
            pen: feat(&[("mode", S("pointer".into()))]),
            suggest: feat(&[]),
            profiles: feat(&[("per_app", B(false))]),
            mirror: feat(&[("fps", I(5))]),
            wireless: feat(&[]),
        }
    }
}

impl Default for Config {
    fn default() -> Config {
        Config {
            match_compatible: vec![],
            touchscreen: "NVTCapacitiveTouchScreen".into(),
            touch_dev: String::new(),
            pen: "NVTCapacitivePen".into(),
            internal_output: "auto".into(),
            touch_transform: String::new(),
            size_mm: [274.0, 193.0],
            external_connectors: vec!["DP-*".into(), "HDMI-*".into(), "USB-C-*".into()],
            autostart: "ask".into(),
            plug_debounce_ms: 1500,
            require_compositor_output: true,
            keyboard: "full".into(),
            keyboard_options: vec!["function".into()],
            pad_only_with_physical_keyboard: false,
            release_chord: vec!["KEY_VOLUMEUP".into(), "KEY_VOLUMEDOWN".into()],
            release_hold_ms: 2000,
            five_finger_release: true,
            click_zones: true,
            click_zone_mm: 14.0,
            speed: 1.0,
            hold_ms: 150,
            palm_major: 0,
            display_pill: true,
            strip_mm: 11.0,
            key_gap_mm: 1.2,
            xkb_layout_index: 0,
            theme: "dark".into(),
            mode: "touchpad".into(),
            natural_scroll: false,
            tap_to_click: true,
            pointer_accel: 0.0,
            right_click: "zones+tap".into(),
            dim: true,
            dim_percent: 20,
            backlight: "auto".into(),
            sticky_modifiers: false,
            long_press_ms: 450,
            palm_keyboard: true,
            rest_zone_mm: 0.0,
            rest_zones: vec![],
            dwt_ms: 300,
            pen_pause: true,
            accent: "#4a8be0".into(),
            key_alpha: 100,
            status_info: true,
            sound_feedback: false,
            haptic_feedback: false,
            track_layout: true,
            quick_actions: QuickCfg::default(),
            features: Features::default(),
        }
    }
}

/// deep merge: tables merge key by key, everything else is replaced
pub fn merge(base: &mut toml::Table, top: toml::Table) {
    for (k, v) in top {
        match (base.get_mut(&k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(t)) => merge(b, t),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}

/// the app-written settings overrides (the settings sheet), merged after config.toml
pub fn settings_path(xdg_config: &Path) -> PathBuf {
    xdg_config.join("dexhid/settings.toml")
}

/// read the settings overrides (missing = empty)
pub fn load_settings(p: &Path) -> Result<toml::Table, String> {
    match std::fs::read_to_string(p) {
        Ok(s) => toml::from_str::<toml::Table>(&s).map_err(|e| format!("{}: {}", p.display(), e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(toml::Table::new()),
        Err(e) => Err(format!("{}: {}", p.display(), e)),
    }
}

/// atomic write of the settings overrides
pub fn save_settings(p: &Path, t: &toml::Table) -> Result<(), String> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {}", d.display(), e))?;
    }
    let mut s = String::from("# DexHid settings written by the settings sheet / `dexhid set` (overrides config.toml)\n");
    s.push_str(&toml::to_string(t).map_err(|e| e.to_string())?);
    let tmp = p.with_extension("toml.tmp");
    std::fs::write(&tmp, s).map_err(|e| format!("{}: {}", tmp.display(), e))?;
    std::fs::rename(&tmp, p).map_err(|e| format!("{}: {}", p.display(), e))
}

/// merge the config files plus the settings overrides into a Config (the settings come before `--config`, which is
/// last in `paths`, so a test config still wins)
pub fn load_with(paths: &[PathBuf], settings: &toml::Table, settings_before_last: bool) -> (Config, Vec<String>) {
    let mut t = toml::Table::new();
    let mut notes = vec![];
    let n = paths.len();
    for (i, p) in paths.iter().enumerate() {
        if settings_before_last && i + 1 == n {
            merge(&mut t, settings.clone());
        }
        match std::fs::read_to_string(p) {
            Ok(s) => match toml::from_str::<toml::Table>(&s) {
                Ok(x) => {
                    notes.push(format!("config file {}", p.display()));
                    merge(&mut t, x)
                }
                Err(e) => notes.push(format!("ERROR config {}: {}", p.display(), e)),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => notes.push(format!("ERROR config {}: {}", p.display(), e)),
        }
    }
    if !settings_before_last {
        merge(&mut t, settings.clone());
    }
    match Config::deserialize(toml::Value::Table(t)) {
        Ok(c) => (c, notes),
        Err(e) => {
            notes.push(format!("ERROR config (merged): {}; using the defaults", e));
            (Config::default(), notes)
        }
    }
}

/// the files to merge, in order (device presets filtered by compatible)
pub fn default_paths(xdg_config: &Path, compatible: &[String]) -> Vec<PathBuf> {
    let mut v = vec![];
    if let Ok(rd) = std::fs::read_dir("/usr/share/dexhid/devices") {
        let mut f: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "toml")).collect();
        f.sort();
        for p in f {
            let ok = std::fs::read_to_string(&p)
                .ok()
                .and_then(|t| toml::from_str::<toml::Table>(&t).ok())
                .and_then(|t| t.get("match_compatible").and_then(|m| m.as_array()).cloned())
                .map(|m| m.iter().filter_map(|x| x.as_str()).any(|c| compatible.iter().any(|d| d == c)))
                .unwrap_or(false);
            if ok {
                v.push(p);
            }
        }
    }
    v.push(PathBuf::from("/etc/dexhid/config.toml"));
    v.push(xdg_config.join("dexhid/config.toml"));
    v
}

/// merge the given files (missing ones are skipped); returns the config and notes (files used, errors)
pub fn load(paths: &[PathBuf]) -> (Config, Vec<String>) {
    let mut t = toml::Table::new();
    let mut notes = vec![];
    for p in paths {
        match std::fs::read_to_string(p) {
            Ok(s) => match toml::from_str::<toml::Table>(&s) {
                Ok(x) => {
                    notes.push(format!("config file {}", p.display()));
                    merge(&mut t, x)
                }
                Err(e) => notes.push(format!("ERROR config {}: {}", p.display(), e)),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => notes.push(format!("ERROR config {}: {}", p.display(), e)),
        }
    }
    match Config::deserialize(toml::Value::Table(t.clone())) {
        Ok(c) => (c, notes),
        Err(e) => {
            notes.push(format!("ERROR config (merged): {}; using the defaults", e));
            (Config::default(), notes)
        }
    }
}

/// $XDG_STATE_HOME/dexhid/state.toml: the app's own remembered choices
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Persist {
    pub keyboard: Option<String>,
    pub autostart: Option<String>,
    /// EDID hash (hex) -> "pad" | "display"; stays local, never logged in full
    pub monitors: BTreeMap<String, String>,
}

impl Persist {
    pub fn load(p: &Path) -> Persist {
        std::fs::read_to_string(p).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
    }
    pub fn save(&self, p: &Path) -> Result<(), String> {
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).map_err(|e| format!("{}: {}", d.display(), e))?;
        }
        let tmp = p.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string(self).map_err(|e| e.to_string())?).map_err(|e| format!("{}: {}", tmp.display(), e))?;
        std::fs::rename(&tmp, p).map_err(|e| format!("{}: {}", p.display(), e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_order_and_defaults() {
        let d = std::env::temp_dir().join(format!("dexhid-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let a = d.join("a.toml");
        let b = d.join("b.toml");
        std::fs::write(&a, "autostart = \"on\"\nkeyboard = \"split\"\n").unwrap();
        std::fs::write(&b, "autostart = \"off\"\nrelease_hold_ms = 3000\n").unwrap();
        let (c, notes) = load(&[a.clone(), d.join("missing.toml"), b.clone()]);
        assert_eq!(c.autostart, "off");
        assert_eq!(c.keyboard, "split");
        assert_eq!(c.release_hold_ms, 3000);
        assert_eq!(c.touchscreen, "NVTCapacitiveTouchScreen");
        assert_eq!(notes.len(), 2);
        std::fs::write(&b, "no_such_key = 1\n").unwrap();
        let (c, notes) = load(&[a, b]);
        assert_eq!(c, Config::default());
        assert!(notes.iter().any(|n| n.starts_with("ERROR")));
        let p = d.join("st/state.toml");
        let mut s = Persist::default();
        s.keyboard = Some("tablet".into());
        s.monitors.insert("abc".into(), "pad".into());
        s.save(&p).unwrap();
        assert_eq!(Persist::load(&p), s);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn settings_overrides_deep_merge_and_order() {
        let d = std::env::temp_dir().join(format!("dexhid-set-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let user = d.join("config.toml");
        let test = d.join("test.toml");
        std::fs::write(&user, "speed = 1.5\n[quick_actions]\nactions = [\"copy\"]\n").unwrap();
        std::fs::write(&test, "autostart = \"on\"\n").unwrap();
        let sp = settings_path(&d);
        let mut s = toml::Table::new();
        crate::settings::set(&mut s, "quick_actions.enabled", toml::Value::Boolean(false));
        crate::settings::set(&mut s, "speed", toml::Value::Float(2.0));
        crate::settings::set(&mut s, "autostart", toml::Value::String("off".into()));
        save_settings(&sp, &s).unwrap();
        let back = load_settings(&sp).unwrap();
        assert_eq!(back, s);
        let (c, notes) = load_with(&[user, test], &back, true);
        assert!(notes.iter().all(|n| !n.starts_with("ERROR")), "{:?}", notes);
        assert_eq!(c.speed, 2.0, "settings override config.toml");
        assert!(!c.quick_actions.enabled);
        assert_eq!(c.quick_actions.actions, vec!["copy"], "deep merge keeps the sibling key");
        assert_eq!(c.autostart, "on", "--config (last) still wins over the settings");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn example_config_parses() {
        let t = include_str!("../../../data/config.example.toml");
        let c: Config = toml::from_str(t).expect("config.example.toml must parse");
        assert_eq!(c.autostart, "ask");
        let p = include_str!("../../../data/devices/oneplus-iceland.toml");
        let c: Config = toml::from_str(p).expect("device preset must parse");
        assert_eq!(c.touchscreen, "NVTCapacitiveTouchScreen");
    }
}
