// SPDX-License-Identifier: GPL-3.0-or-later
//! M3 gesture shortcuts: 3 / 4-finger swipes (left / right / up / down) and taps on the panel mapped to actions.
//! With the module on, DexHid recognises these gestures itself (on the touchpad too) instead of handing them to the
//! desktop: the router keeps at most two fingers on the virtual touchpad, the third one freezes them (lifted only
//! after libinput's tap timeout, so no two-finger tap = right click is produced) and the gesture runs its action.
//! Actions are key chords through the virtual keyboard (desktop shortcuts; the defaults follow KDE Plasma), a middle
//! click on the virtual touchpad, or any quick action (built-in id or a custom one from config.toml).

use crate::config::QuickCfg;
use crate::quick;
use std::collections::BTreeMap;

/// the gestures that can carry an action
pub const GESTURES: &[&str] = &[
    "swipe3_left", "swipe3_right", "swipe3_up", "swipe3_down", "swipe4_left", "swipe4_right", "swipe4_up", "swipe4_down", "tap3", "tap4",
];

/// the actions offered in the settings sheet (a quick-action id, including custom ones, is accepted from
/// config.toml / `dexhid set` as "quick:<id>")
pub const ACTIONS: &[&str] = &[
    "off", "back", "forward", "desktop-left", "desktop-right", "overview", "show-desktop", "app-switch", "middle-click", "copy", "paste", "cut",
    "undo", "redo", "select-all", "close-window", "fullscreen", "screenshot", "play-pause", "next", "prev", "vol-up", "vol-down", "mute",
    "to-screen",
];

/// defaults: swipes move like the content (left = the desktop to the right), up = overview, 4 fingers = history
pub fn default_map() -> BTreeMap<String, String> {
    [
        ("swipe3_left", "desktop-right"),
        ("swipe3_right", "desktop-left"),
        ("swipe3_up", "overview"),
        ("swipe3_down", "show-desktop"),
        ("swipe4_left", "forward"),
        ("swipe4_right", "back"),
        ("swipe4_up", "app-switch"),
        ("swipe4_down", "off"),
        ("tap3", "middle-click"),
        ("tap4", "off"),
    ]
    .iter()
    .map(|(a, b)| (a.to_string(), b.to_string()))
    .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub enum GAct {
    None,
    Chord(Vec<u16>),
    MiddleClick,
    Quick(String),
}

/// what an action name does
pub fn action(name: &str, q: &QuickCfg) -> Result<GAct, String> {
    let chord = |s: &str| quick::parse_chord(s).map(GAct::Chord).ok_or_else(|| format!("bad chord {}", s));
    match name {
        "off" | "" => Ok(GAct::None),
        "middle-click" => Ok(GAct::MiddleClick),
        "back" => chord("LEFTALT+LEFT"),
        "forward" => chord("LEFTALT+RIGHT"),
        // KDE Plasma: Meta+Ctrl+Left / Right switch one desktop
        "desktop-left" => chord("LEFTCTRL+LEFTMETA+LEFT"),
        "desktop-right" => chord("LEFTCTRL+LEFTMETA+RIGHT"),
        "app-switch" => chord("LEFTALT+TAB"),
        "to-screen" => Ok(GAct::Quick("other_screen".into())),
        "select-all" => Ok(GAct::Quick("select_all".into())),
        "close-window" => Ok(GAct::Quick("close_window".into())),
        "show-desktop" => Ok(GAct::Quick("show_desktop".into())),
        "play-pause" => Ok(GAct::Quick("play_pause".into())),
        "vol-up" => Ok(GAct::Quick("vol_up".into())),
        "vol-down" => Ok(GAct::Quick("vol_down".into())),
        n => {
            let id = n.strip_prefix("quick:").unwrap_or(n);
            if quick::BUILTIN.iter().any(|b| b.0 == id) || q.custom.iter().any(|c| c.id == id) {
                Ok(GAct::Quick(id.to_string()))
            } else {
                Err(format!("unknown gesture action {:?}", n))
            }
        }
    }
}

/// the gesture name of a recognised swipe (finger count, mean unit direction; y grows downwards)
pub fn swipe_name(fingers: usize, dir: (f32, f32)) -> String {
    let d = if dir.0.abs() >= dir.1.abs() {
        if dir.0 < 0.0 { "left" } else { "right" }
    } else if dir.1 < 0.0 {
        "up"
    } else {
        "down"
    };
    format!("swipe{}_{}", fingers.clamp(3, 4), d)
}

/// the configured action of a gesture: the feature's options (strings) over the defaults
pub fn mapped(options: &BTreeMap<String, toml::Value>, gesture: &str) -> String {
    options.get(gesture).and_then(|v| v.as_str()).map(|s| s.to_string()).or_else(|| default_map().get(gesture).cloned()).unwrap_or_else(|| "off".into())
}

/// multi-finger tap recognition: 3-4 contacts that all land within `land_ms`, never move more than `slop_px` and
/// are all lifted within `max_ms` of the first touch
#[derive(Clone, Debug)]
pub struct TapCfg {
    pub land_ms: i64,
    pub max_ms: i64,
    pub slop_px: f32,
}

impl Default for TapCfg {
    fn default() -> TapCfg {
        TapCfg { land_ms: 150, max_ms: 350, slop_px: 20.0 }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Taps {
    pub cfg: TapCfg,
    first: Option<i64>,
    downs: Vec<(u32, f32, f32)>,
    live: usize,
    max: usize,
    spoiled: bool,
}

impl Taps {
    pub fn new(cfg: TapCfg) -> Taps {
        Taps { cfg, ..Default::default() }
    }
    pub fn down(&mut self, id: u32, x: f32, y: f32, t: i64) {
        if self.live == 0 {
            *self = Taps { cfg: self.cfg.clone(), first: Some(t), ..Default::default() };
        }
        if t - self.first.unwrap_or(t) > self.cfg.land_ms {
            self.spoiled = true;
        }
        self.downs.push((id, x, y));
        self.live += 1;
        self.max = self.max.max(self.live);
    }
    pub fn moved(&mut self, id: u32, x: f32, y: f32) {
        if let Some(d) = self.downs.iter().find(|d| d.0 == id) {
            if ((x - d.1).powi(2) + (y - d.2).powi(2)).sqrt() > self.cfg.slop_px {
                self.spoiled = true;
            }
        }
    }
    /// 3 or 4 = a multi-finger tap ended with this lift
    pub fn up(&mut self, id: u32, t: i64) -> Option<usize> {
        if !self.downs.iter().any(|d| d.0 == id) {
            return None;
        }
        self.live = self.live.saturating_sub(1);
        if self.live > 0 {
            return None;
        }
        let ok = !self.spoiled && (3..=4).contains(&self.max) && self.first.is_some_and(|f| t - f <= self.cfg.max_ms);
        self.first = None;
        ok.then_some(self.max)
    }
    /// the contacts down now landed together (a gesture may be starting)
    pub fn landing(&self, t: i64) -> bool {
        self.live >= 3 && !self.spoiled && self.first.is_some_and(|f| t - f <= self.cfg.land_ms)
    }
    pub fn spoil(&mut self) {
        self.spoiled = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swipe_names_and_mapping() {
        assert_eq!(swipe_name(3, (-0.9, 0.1)), "swipe3_left");
        assert_eq!(swipe_name(3, (0.95, -0.2)), "swipe3_right");
        assert_eq!(swipe_name(4, (0.1, -1.0)), "swipe4_up");
        assert_eq!(swipe_name(4, (0.0, 1.0)), "swipe4_down");
        assert_eq!(swipe_name(5, (0.0, 1.0)), "swipe4_down", "clamped");
        let q = QuickCfg::default();
        for g in GESTURES {
            let a = mapped(&BTreeMap::new(), g);
            assert!(ACTIONS.contains(&a.as_str()), "{} -> {}", g, a);
        }
        for a in ACTIONS {
            assert!(action(a, &q).is_ok(), "{}", a);
        }
        let mut o = BTreeMap::new();
        o.insert("swipe3_up".to_string(), toml::Value::String("back".into()));
        assert_eq!(mapped(&o, "swipe3_up"), "back");
        assert_eq!(mapped(&o, "swipe3_down"), "show-desktop", "default");
        assert_eq!(action("back", &q).unwrap(), GAct::Chord(vec![56, 105]));
        assert_eq!(action("desktop-right", &q).unwrap(), GAct::Chord(vec![29, 125, 106]));
        assert_eq!(action("middle-click", &q).unwrap(), GAct::MiddleClick);
        assert_eq!(action("copy", &q).unwrap(), GAct::Quick("copy".into()));
        assert_eq!(action("quick:mute", &q).unwrap(), GAct::Quick("mute".into()));
        assert_eq!(action("off", &q).unwrap(), GAct::None);
        assert!(action("quick:nope", &q).is_err());
        let mut q2 = QuickCfg::default();
        q2.custom.push(crate::config::CustomAction { id: "term".into(), command: vec!["konsole".into()], ..Default::default() });
        assert_eq!(action("quick:term", &q2).unwrap(), GAct::Quick("term".into()), "custom quick actions too");
    }

    #[test]
    fn taps() {
        let mut t = Taps::new(TapCfg::default());
        t.down(1, 0.0, 0.0, 0);
        t.down(2, 50.0, 0.0, 20);
        t.down(3, 100.0, 0.0, 40);
        assert!(t.landing(45));
        t.moved(2, 55.0, 2.0);
        assert_eq!(t.up(1, 120), None);
        assert_eq!(t.up(2, 125), None);
        assert_eq!(t.up(3, 130), Some(3), "3-finger tap");
        // moved too far
        t.down(1, 0.0, 0.0, 1000);
        t.down(2, 50.0, 0.0, 1010);
        t.down(3, 100.0, 0.0, 1020);
        t.moved(3, 150.0, 0.0);
        t.up(1, 1100);
        t.up(2, 1100);
        assert_eq!(t.up(3, 1100), None);
        // landed too far apart / held too long / two fingers
        for (dt, hold, n) in [(200, 100, 3), (10, 600, 4), (10, 100, 2)] {
            let base = 5000;
            for i in 0..n {
                t.down(10 + i as u32, i as f32 * 40.0, 0.0, base + i as i64 * dt);
            }
            let mut r = None;
            for i in 0..n {
                r = t.up(10 + i as u32, base + (n as i64 - 1) * dt + hold);
            }
            assert_eq!(r, None, "dt={} hold={} n={}", dt, hold, n);
        }
        // 4 fingers
        for i in 0..4 {
            t.down(20 + i, i as f32 * 40.0, 0.0, 9000 + i as i64 * 10);
        }
        let mut r = None;
        for i in 0..4 {
            r = t.up(20 + i, 9200);
        }
        assert_eq!(r, Some(4));
    }
}
