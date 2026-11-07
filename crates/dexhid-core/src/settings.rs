// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! The settings registry (M2). Every user-visible option is a registered setting inside a module (a feature with an
//! on/off flag and its options). The settings sheet is built from this table, values are stored as overrides in
//! `$XDG_CONFIG_HOME/dexhid/settings.toml` (merged into the config after config.toml), and a new feature only adds
//! rows here plus its config field: no sheet / storage code changes.
//!
//! Keys are dotted paths into the config (`natural_scroll`, `quick_actions.enabled`,
//! `features.gestures.options.three_swipe`). Modules with `implemented = false` are designed and stored but not
//! acted on yet (planned for M3; the sheet marks them).

use std::collections::HashSet;
use toml::Value;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Bool,
    /// one of these strings
    Choice(&'static [&'static str]),
    /// a number: min, max, step (integers when `int`)
    Num { min: f64, max: f64, step: f64, int: bool },
    /// a button that triggers something (not stored): the key names the action
    Action,
}

#[derive(Clone, Copy, Debug)]
pub struct Module {
    pub id: &'static str,
    pub title: &'static str,
    /// acted on in this build (false: stored only, planned)
    pub implemented: bool,
    /// planned milestone + effort for the not-yet-implemented ones
    pub plan: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub struct Def {
    pub key: &'static str,
    pub module: &'static str,
    pub title: &'static str,
    pub kind: Kind,
}

pub const MODULES: &[Module] = &[
    Module { id: "general", title: "General", implemented: true, plan: "" },
    Module { id: "pointer", title: "Touchpad", implemented: true, plan: "" },
    Module { id: "keyboard", title: "Keyboard", implemented: true, plan: "" },
    Module { id: "quick", title: "Quick actions", implemented: true, plan: "" },
    Module { id: "look", title: "Display + theme", implemented: true, plan: "" },
    Module { id: "palm", title: "Palm + rest zones", implemented: true, plan: "" },
    Module { id: "feedback", title: "Sound + haptics", implemented: true, plan: "" },
    Module { id: "gestures", title: "Gesture shortcuts", implemented: true, plan: "" },
    Module { id: "clipboard", title: "Clipboard history", implemented: true, plan: "" },
    Module { id: "textedit", title: "Text editing mode", implemented: true, plan: "" },
    Module { id: "remote", title: "Media / presentation", implemented: true, plan: "" },
    Module { id: "pen", title: "Pen on the monitor", implemented: true, plan: "" },
    Module { id: "suggest", title: "Suggestions + emoji", implemented: true, plan: "" },
    Module { id: "profiles", title: "Profiles", implemented: true, plan: "" },
    Module { id: "mirror", title: "Mirror preview", implemented: true, plan: "" },
    Module { id: "usbhid", title: "USB keyboard / mouse", implemented: true, plan: "" },
];

const ON_OFF_ASK: &[&str] = &["ask", "on", "off"];
/// keyboard languages for another device (xkb layout or layout(variant)); "desktop" = this desktop's current one
pub const TARGET_LAYOUTS: &[&str] = &[
    "desktop", "us", "us(intl)", "us(dvorak)", "us(colemak)", "gb", "de", "de(nodeadkeys)", "ch", "fr", "fr(oss)", "be", "es", "it", "pt", "br", "nl", "se",
    "no", "dk", "fi", "pl", "cz", "hu", "ru", "ua", "tr", "jp",
];
const G: &[&str] = crate::gestures::ACTIONS;

pub const DEFS: &[Def] = &[
    // general
    Def { key: "act.layout", module: "general", title: "Keyboard layout", kind: Kind::Action },
    Def { key: "autostart", module: "general", title: "When a monitor is plugged in", kind: Kind::Choice(ON_OFF_ASK) },
    Def { key: "act.edit", module: "general", title: "Move / resize areas", kind: Kind::Action },
    Def { key: "status_info", module: "general", title: "Battery / display info in the bar", kind: Kind::Bool },
    Def { key: "pad_only_with_physical_keyboard", module: "general", title: "Touchpad only when a keyboard is attached", kind: Kind::Bool },
    Def { key: "track_layout", module: "general", title: "Legends follow the desktop layout", kind: Kind::Bool },
    Def { key: "arrangement", module: "general", title: "Arrangement", kind: Kind::Choice(&["free", "laptop"]) },
    Def { key: "laptop_pad_width", module: "general", title: "Laptop: touchpad width (% of keyboard)", kind: Kind::Num { min: 40.0, max: 100.0, step: 5.0, int: true } },
    Def { key: "panels_follow", module: "general", title: "Move desktop panels to the monitor (KDE)", kind: Kind::Bool },
    // pointer
    Def { key: "mode", module: "pointer", title: "Mode", kind: Kind::Choice(&["touchpad", "mouse"]) },
    Def { key: "speed", module: "pointer", title: "Pointer speed (DexHid)", kind: Kind::Num { min: 0.4, max: 3.0, step: 0.1, int: false } },
    Def { key: "pointer_accel", module: "pointer", title: "Desktop acceleration", kind: Kind::Num { min: -1.0, max: 1.0, step: 0.1, int: false } },
    Def { key: "natural_scroll", module: "pointer", title: "Natural scrolling", kind: Kind::Bool },
    Def { key: "tap_to_click", module: "pointer", title: "Tap to click", kind: Kind::Bool },
    Def { key: "right_click", module: "pointer", title: "Right click", kind: Kind::Choice(&["zones+tap", "tap", "zones"]) },
    Def { key: "zone_chord_middle", module: "pointer", title: "Both click zones = middle click", kind: Kind::Bool },
    Def { key: "swipe_gestures", module: "pointer", title: "3/4-finger swipes anywhere on the panel", kind: Kind::Bool },
    Def { key: "swipe_min_mm", module: "pointer", title: "Swipe: travel before it starts (mm)", kind: Kind::Num { min: 4.0, max: 20.0, step: 1.0, int: false } },
    Def { key: "scroll_inertia", module: "pointer", title: "Two-finger scroll inertia", kind: Kind::Bool },
    Def { key: "inertia_friction", module: "pointer", title: "Inertia friction", kind: Kind::Num { min: 1.0, max: 10.0, step: 0.5, int: false } },
    // keyboard
    Def { key: "sticky_modifiers", module: "keyboard", title: "Sticky modifiers", kind: Kind::Bool },
    Def { key: "long_press_ms", module: "keyboard", title: "Long press for alternates (ms)", kind: Kind::Num { min: 250.0, max: 1000.0, step: 50.0, int: true } },
    Def { key: "hold_ms", module: "keyboard", title: "Hold before a key repeats (ms)", kind: Kind::Num { min: 80.0, max: 600.0, step: 10.0, int: true } },
    Def { key: "key_slop_mm", module: "keyboard", title: "Finger wobble allowed on a key (mm)", kind: Kind::Num { min: 1.0, max: 8.0, step: 0.5, int: false } },
    Def { key: "modifier_mask", module: "keyboard", title: "Cancelled modifier: mask key", kind: Kind::Choice(&["UNKNOWN", "F24", "off"]) },
    Def { key: "opt.function", module: "keyboard", title: "Function key row", kind: Kind::Bool },
    Def { key: "opt.numpad", module: "keyboard", title: "Numeric keypad", kind: Kind::Bool },
    // quick actions (the per-action rows are generated from quick::BUILTIN)
    Def { key: "quick_actions.enabled", module: "quick", title: "Quick actions bar", kind: Kind::Bool },
    // look
    Def { key: "dim", module: "look", title: "Dim the tablet panel in pad mode", kind: Kind::Bool },
    Def { key: "dim_percent", module: "look", title: "Dim to (% of max)", kind: Kind::Num { min: 5.0, max: 80.0, step: 5.0, int: true } },
    Def { key: "theme", module: "look", title: "Theme", kind: Kind::Choice(&["desktop", "dark", "light", "amoled", "auto"]) },
    Def { key: "accent", module: "look", title: "Accent colour", kind: Kind::Choice(&["#4a8be0", "#e0a030", "#40b070", "#d05a8a", "#9a7ae0"]) },
    Def { key: "key_alpha", module: "look", title: "Key opacity (%)", kind: Kind::Num { min: 30.0, max: 100.0, step: 10.0, int: true } },
    // palm
    Def { key: "palm_major", module: "palm", title: "Palm: ignore touches larger than (0 = off)", kind: Kind::Num { min: 0.0, max: 255.0, step: 5.0, int: true } },
    Def { key: "palm_keyboard", module: "palm", title: "Palm check on the keys too", kind: Kind::Bool },
    Def { key: "rest_zone_mm", module: "palm", title: "Rest zone at the panel edges (mm)", kind: Kind::Num { min: 0.0, max: 20.0, step: 2.0, int: false } },
    Def { key: "dwt_ms", module: "palm", title: "Pad off after a key press (ms)", kind: Kind::Num { min: 0.0, max: 1000.0, step: 50.0, int: true } },
    Def { key: "pen_pause", module: "palm", title: "Ignore touches while the pen is near", kind: Kind::Bool },
    // feedback
    Def { key: "sound_feedback", module: "feedback", title: "Key click sound", kind: Kind::Bool },
    Def { key: "haptic_feedback", module: "feedback", title: "Vibrate on key press", kind: Kind::Bool },
    // M3 modules
    Def { key: "target", module: "general", title: "Keys + pointer go to", kind: Kind::Choice(&["desktop", "usb"]) },
    Def { key: "features.gestures.enabled", module: "gestures", title: "Gesture shortcuts (3 / 4 fingers)", kind: Kind::Bool },
    Def { key: "features.gestures.options.swipe3_left", module: "gestures", title: "3 fingers left", kind: Kind::Choice(G) },
    Def { key: "features.gestures.options.swipe3_right", module: "gestures", title: "3 fingers right", kind: Kind::Choice(G) },
    Def { key: "features.gestures.options.swipe3_up", module: "gestures", title: "3 fingers up", kind: Kind::Choice(G) },
    Def { key: "features.gestures.options.swipe3_down", module: "gestures", title: "3 fingers down", kind: Kind::Choice(G) },
    Def { key: "features.gestures.options.swipe4_left", module: "gestures", title: "4 fingers left", kind: Kind::Choice(G) },
    Def { key: "features.gestures.options.swipe4_right", module: "gestures", title: "4 fingers right", kind: Kind::Choice(G) },
    Def { key: "features.gestures.options.swipe4_up", module: "gestures", title: "4 fingers up", kind: Kind::Choice(G) },
    Def { key: "features.gestures.options.swipe4_down", module: "gestures", title: "4 fingers down", kind: Kind::Choice(G) },
    Def { key: "features.gestures.options.tap3", module: "gestures", title: "3-finger tap", kind: Kind::Choice(G) },
    Def { key: "features.gestures.options.tap4", module: "gestures", title: "4-finger tap", kind: Kind::Choice(G) },
    Def { key: "features.clipboard.enabled", module: "clipboard", title: "Clipboard history strip", kind: Kind::Bool },
    Def { key: "features.clipboard.options.items", module: "clipboard", title: "Items", kind: Kind::Num { min: 5.0, max: 10.0, step: 1.0, int: true } },
    Def { key: "features.clipboard.options.images", module: "clipboard", title: "Keep images too", kind: Kind::Bool },
    Def { key: "features.textedit.enabled", module: "textedit", title: "Text editing mode (select key, joystick)", kind: Kind::Bool },
    Def { key: "features.textedit.options.joystick", module: "textedit", title: "Cursor joystick", kind: Kind::Bool },
    Def { key: "features.textedit.options.joystick_dead_mm", module: "textedit", title: "Joystick dead zone (mm)", kind: Kind::Num { min: 1.0, max: 8.0, step: 0.5, int: false } },
    Def { key: "features.remote.enabled", module: "remote", title: "Presentation remote (laser, timer)", kind: Kind::Bool },
    Def { key: "features.remote.options.laser", module: "remote", title: "Laser pointer: pad = absolute pointer", kind: Kind::Bool },
    Def { key: "features.remote.options.laser_highlight", module: "remote", title: "Laser: highlight the pointer (KWin)", kind: Kind::Bool },
    Def { key: "features.remote.options.timer_min", module: "remote", title: "Talk timer (min, 0 = stopwatch)", kind: Kind::Num { min: 0.0, max: 120.0, step: 5.0, int: true } },
    Def { key: "features.pen.enabled", module: "pen", title: "Pen on the external monitor", kind: Kind::Bool },
    Def { key: "features.pen.options.mode", module: "pen", title: "Pen mode", kind: Kind::Choice(&["pointer", "tablet"]) },
    Def { key: "features.pen.options.speed", module: "pen", title: "Pointer mode speed", kind: Kind::Num { min: 0.3, max: 3.0, step: 0.1, int: false } },
    Def { key: "features.suggest.enabled", module: "suggest", title: "Keyboard extras (suggestions, emoji)", kind: Kind::Bool },
    Def { key: "features.suggest.options.words", module: "suggest", title: "Word suggestions (local word list)", kind: Kind::Bool },
    Def { key: "features.suggest.options.count", module: "suggest", title: "Suggestions shown", kind: Kind::Num { min: 2.0, max: 5.0, step: 1.0, int: true } },
    Def { key: "features.suggest.options.emoji_panel", module: "suggest", title: "Emoji panel", kind: Kind::Bool },
    Def { key: "features.profiles.enabled", module: "profiles", title: "Profiles (per monitor)", kind: Kind::Bool },
    Def { key: "features.profiles.options.per_app", module: "profiles", title: "Switch per focused app (KWin)", kind: Kind::Bool },
    Def { key: "features.mirror.enabled", module: "mirror", title: "Mirror preview of the monitor", kind: Kind::Bool },
    Def { key: "features.mirror.options.fps", module: "mirror", title: "Preview fps", kind: Kind::Num { min: 1.0, max: 10.0, step: 1.0, int: true } },
    Def { key: "features.mirror.options.size_pct", module: "mirror", title: "Preview size (% of the pad)", kind: Kind::Num { min: 20.0, max: 60.0, step: 5.0, int: true } },
    Def { key: "features.usbhid.enabled", module: "usbhid", title: "USB keyboard / mouse for another device", kind: Kind::Bool },
    Def { key: "features.usbhid.options.layout", module: "usbhid", title: "Keyboard language of that device", kind: Kind::Choice(TARGET_LAYOUTS) },
    Def { key: "features.usbhid.options.abs_pointer", module: "usbhid", title: "Absolute pointer (laser / pen)", kind: Kind::Bool },
    Def { key: "features.usbhid.options.stop_other", module: "usbhid", title: "Stop another gadget that holds the port", kind: Kind::Bool },
];

pub fn def(key: &str) -> Option<&'static Def> {
    DEFS.iter().find(|d| d.key == key)
}

pub fn module(id: &str) -> Option<&'static Module> {
    MODULES.iter().find(|m| m.id == id)
}

/// value at a dotted path
pub fn get<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut v = root;
    for p in path.split('.') {
        v = v.as_table()?.get(p)?;
    }
    Some(v)
}

/// set a value at a dotted path, creating tables on the way
pub fn set(root: &mut toml::Table, path: &str, val: Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut t = root;
    for p in &parts[..parts.len() - 1] {
        let e = t.entry(p.to_string()).or_insert_with(|| Value::Table(toml::Table::new()));
        if !e.is_table() {
            *e = Value::Table(toml::Table::new());
        }
        t = e.as_table_mut().unwrap();
    }
    t.insert(parts[parts.len() - 1].to_string(), val);
}

/// parse a value given as text (CLI / D-Bus) for a setting
pub fn parse(d: &Def, s: &str) -> Result<Value, String> {
    match d.kind {
        Kind::Bool => match s {
            "true" | "on" | "1" | "yes" => Ok(Value::Boolean(true)),
            "false" | "off" | "0" | "no" => Ok(Value::Boolean(false)),
            _ => Err(format!("{}: expected true / false", d.key)),
        },
        Kind::Choice(c) => {
            // M3: a gesture may run any quick action, custom ones included ("quick:<id>", checked by the daemon)
            if c.contains(&s) || (d.key.starts_with("features.gestures.options.") && s.strip_prefix("quick:").is_some_and(|i| !i.is_empty())) {
                Ok(Value::String(s.into()))
            } else {
                Err(format!("{}: one of {:?}", d.key, c))
            }
        }
        Kind::Num { min, max, int, .. } => {
            let v: f64 = s.parse().map_err(|_| format!("{}: not a number", d.key))?;
            if !(min..=max).contains(&v) {
                return Err(format!("{}: {} outside {}..{}", d.key, v, min, max));
            }
            Ok(if int { Value::Integer(v.round() as i64) } else { Value::Float((v * 1000.0).round() / 1000.0) })
        }
        Kind::Action => Err(format!("{} is an action, not a value", d.key)),
    }
}

fn num(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Integer(i)) => *i as f64,
        Some(Value::Float(f)) => *f,
        _ => 0.0,
    }
}

/// the value after one tap (`dir` = +1 / -1): bools toggle, choices cycle, numbers step and clamp
pub fn step(d: &Def, cur: Option<&Value>, dir: i32) -> Option<Value> {
    match d.kind {
        Kind::Bool => Some(Value::Boolean(!cur.and_then(|v| v.as_bool()).unwrap_or(false))),
        Kind::Choice(c) => {
            let i = cur.and_then(|v| v.as_str()).and_then(|s| c.iter().position(|x| *x == s)).unwrap_or(0) as i32;
            let n = c.len() as i32;
            Some(Value::String(c[((i + dir).rem_euclid(n)) as usize].into()))
        }
        Kind::Num { min, max, step, int } => {
            let v = (num(cur) + step * dir as f64).clamp(min, max);
            Some(if int { Value::Integer(v.round() as i64) } else { Value::Float((v * 1000.0).round() / 1000.0) })
        }
        Kind::Action => None,
    }
}

/// short text of a value for the sheet
pub fn display(d: &Def, v: Option<&Value>) -> String {
    match (d.kind, v) {
        (Kind::Action, _) => "\u{203a}".into(),
        (Kind::Bool, Some(Value::Boolean(b))) => if *b { "On".into() } else { "Off".into() },
        (Kind::Num { int: true, .. }, v) => format!("{}", num(v) as i64),
        (Kind::Num { .. }, v) => format!("{:.1}", num(v)),
        (_, Some(Value::String(s))) => s.clone(),
        (_, Some(x)) => x.to_string(),
        (_, None) => "-".into(),
    }
}

/// one row of the sheet
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub key: String,
    pub title: String,
    pub value: String,
    pub kind: Kind,
    /// the module is acted on in this build
    pub live: bool,
}

/// the rows of a module from the effective config (as a toml value); `hidden` = keys not available on this device
pub fn rows(module_id: &str, cfg: &Value, hidden: &HashSet<String>) -> Vec<Row> {
    let live = module(module_id).map(|m| m.implemented).unwrap_or(false);
    DEFS.iter()
        .filter(|d| d.module == module_id && !hidden.contains(d.key))
        .map(|d| Row { key: d.key.into(), title: d.title.into(), value: display(d, value_of(cfg, d.key).as_ref()), kind: d.kind, live })
        .collect()
}

/// the current value of a setting; `opt.<name>` = membership of <name> in `keyboard_options`
pub fn value_of(cfg: &Value, key: &str) -> Option<Value> {
    if let Some(o) = key.strip_prefix("opt.") {
        let on = get(cfg, "keyboard_options").and_then(|v| v.as_array()).is_some_and(|a| a.iter().any(|x| x.as_str() == Some(o)));
        return Some(Value::Boolean(on));
    }
    get(cfg, key).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn cfgv() -> Value {
        Value::try_from(Config::default()).unwrap()
    }

    #[test]
    fn every_def_maps_to_a_config_key_and_steps() {
        let v = cfgv();
        let mut seen = HashSet::new();
        for d in DEFS {
            assert!(seen.insert(d.key), "duplicate {}", d.key);
            assert!(module(d.module).is_some(), "{} has unknown module {}", d.key, d.module);
            if d.kind == Kind::Action || d.key.starts_with("opt.") {
                continue;
            }
            let cur = get(&v, d.key).unwrap_or_else(|| panic!("{} not in Config", d.key));
            let next = step(d, Some(cur), 1).unwrap();
            // the stepped value must still deserialize into a Config
            let mut t = toml::Table::new();
            set(&mut t, d.key, next.clone());
            let mut base = v.as_table().unwrap().clone();
            crate::config::merge(&mut base, t);
            let c: Result<Config, _> = Value::Table(base).try_into();
            assert!(c.is_ok(), "{} = {:?}: {:?}", d.key, next, c.err());
        }
        for m in MODULES {
            assert!(DEFS.iter().any(|d| d.module == m.id), "module {} has no settings", m.id);
            assert!(m.implemented || !m.plan.is_empty(), "{} needs a plan", m.id);
        }
    }

    #[test]
    fn step_parse_display() {
        let b = def("natural_scroll").unwrap();
        assert_eq!(step(b, Some(&Value::Boolean(false)), 1), Some(Value::Boolean(true)));
        let c = def("theme").unwrap();
        assert_eq!(step(c, Some(&Value::String("auto".into())), 1), Some(Value::String("desktop".into())), "cycles");
        assert_eq!(step(c, Some(&Value::String("desktop".into())), -1), Some(Value::String("auto".into())));
        let n = def("speed").unwrap();
        assert_eq!(step(n, Some(&Value::Float(2.95)), 1), Some(Value::Float(3.0)), "clamped");
        assert_eq!(parse(n, "1.25").unwrap(), Value::Float(1.25));
        assert!(parse(n, "9").is_err());
        assert!(parse(c, "pink").is_err());
        assert_eq!(parse(def("dim_percent").unwrap(), "30").unwrap(), Value::Integer(30));
        assert_eq!(display(b, Some(&Value::Boolean(true))), "On");
        let mut t = toml::Table::new();
        set(&mut t, "features.gestures.options.three_swipe", Value::String("off".into()));
        assert_eq!(get(&Value::Table(t), "features.gestures.options.three_swipe").and_then(|v| v.as_str()), Some("off"));
        let r = rows("feedback", &cfgv(), &["haptic_feedback".to_string()].into_iter().collect());
        assert_eq!(r.len(), 1, "hidden setting (no vibration device) is not shown");
        assert!(rows("gestures", &cfgv(), &HashSet::new()).iter().all(|r| r.live), "M3: every module is live");
        assert!(MODULES.iter().all(|m| m.implemented));
        let g = def("features.gestures.options.tap3").unwrap();
        assert_eq!(parse(g, "quick:term").unwrap(), Value::String("quick:term".into()));
        assert!(parse(g, "bogus").is_err());
        assert!(parse(def("features.usbhid.options.layout").unwrap(), "de(nodeadkeys)").is_ok());
        assert!(parse(def("target").unwrap(), "usb").is_ok());
        // Bluetooth HID mode removed (owner decision 7 Nov 2026): no module, no target choice
        assert!(parse(def("target").unwrap(), "bluetooth").is_err());
        assert!(module("bthid").is_none() && DEFS.iter().all(|d| !d.key.contains("bthid")));
    }
}
