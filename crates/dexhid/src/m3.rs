// SPDX-License-Identifier: GPL-3.0-or-later
//! M3 features in the daemon (a child module of daemon.rs, so it extends `D`):
//! gesture actions, clipboard strip, word suggestions + emoji, presentation timer + laser, pen on the monitor,
//! profiles (per monitor / per app with a KWin script), the mirror preview, and the USB HID target
//! (the sink in hidsink.rs, the root helper in hidhelper.rs). Every feature is a settings module (settings.rs).

use super::D;
use crate::clipboard::{self, ClipCtl};
use crate::desktop;
use crate::hidsink::{self, Kind, Sink};
use crate::mirror::{self, Mirror};
use crate::shared::Cmd;
use crate::sys::*;
use dexhid_core::config::Config;
use dexhid_core::extras::{self, ClipData, Timer, WordList, WordTracker};
use dexhid_core::gestures::{self, GAct};
use dexhid_core::keycodes;
use dexhid_core::pen::{self as penl, Mode as PenMode, Pen, PenCfg, PenOut};
use dexhid_core::profiles::{self, Profile};
use dexhid_core::quick;
use dexhid_core::router::{Out, RouterCfg};
use dexhid_core::state::{Ev, Remember};
use dexhid_core::touchpad::PadFrame;
use std::collections::HashMap;
use std::path::PathBuf;

/// an absolute uinput tablet: the presentation laser (hover only) or the pen in tablet mode
pub struct Tablet {
    u: Uinput,
    max: (i32, i32),
    near: bool,
}

impl Tablet {
    fn create(name: &str, max: (i32, i32), res: (i32, i32), pressure: Option<i32>, tilt: Option<(i32, i32)>) -> Result<Tablet, String> {
        let mut keys = vec![BTN_TOOL_PEN, BTN_TOUCH, penl::BTN_STYLUS, penl::BTN_STYLUS2];
        keys.sort_unstable();
        let mut abs = vec![(ABS_X, ai(0, max.0, res.0)), (penl::ABS_Y, ai(0, max.1, res.1))];
        if let Some(p) = pressure {
            abs.push((penl::ABS_PRESSURE, ai(0, p, 0)));
        }
        if let Some((a, b)) = tilt {
            abs.push((penl::ABS_TILT_X, ai(a, b, 0)));
            abs.push((penl::ABS_TILT_Y, ai(a, b, 0)));
        }
        let u = Uinput::create(&UinputSpec { keys, abs, ..UinputSpec::new(name) })?;
        Ok(Tablet { u, max, near: false })
    }
    fn hover(&mut self, x: f32, y: f32) {
        if !self.near {
            self.u.emit(EV_KEY, BTN_TOOL_PEN, 1);
            self.near = true;
        }
        self.u.emit(EV_ABS, ABS_X, (x.clamp(0.0, 1.0) * self.max.0 as f32) as i32);
        self.u.emit(EV_ABS, penl::ABS_Y, (y.clamp(0.0, 1.0) * self.max.1 as f32) as i32);
        self.u.syn();
    }
    fn away(&mut self) {
        if self.near {
            self.u.emit(EV_KEY, BTN_TOUCH, 0);
            self.u.emit(EV_KEY, BTN_TOOL_PEN, 0);
            self.u.syn();
            self.near = false;
        }
    }
    fn pen(&mut self, s: &penl::PenState) {
        self.u.emit(EV_KEY, BTN_TOOL_PEN, s.near as i32);
        self.u.emit(EV_ABS, ABS_X, s.x);
        self.u.emit(EV_ABS, penl::ABS_Y, s.y);
        self.u.emit(EV_ABS, penl::ABS_PRESSURE, s.pressure);
        self.u.emit(EV_ABS, penl::ABS_TILT_X, s.tilt.0);
        self.u.emit(EV_ABS, penl::ABS_TILT_Y, s.tilt.1);
        self.u.emit(EV_KEY, BTN_TOUCH, s.touch as i32);
        self.u.emit(EV_KEY, penl::BTN_STYLUS, s.stylus as i32);
        self.u.emit(EV_KEY, penl::BTN_STYLUS2, s.stylus2 as i32);
        self.u.syn();
        self.near = s.near;
    }
}

pub enum PenDev {
    Pointer(crate::devices::VirtualMouse),
    Tablet(Tablet),
    /// HID target: the pen goes to the sink
    Sink,
}

#[derive(Default)]
pub struct M3State {
    pub sink: Option<Sink>,
    /// the target the sink serves now ("desktop" = none)
    pub target: String,
    pub sink_note: String,
    pub hid_connected: bool,
    pub info_next: i64,
    pub clip: Option<ClipCtl>,
    pub clip_cfg: (bool, usize, bool),
    pub clips: Vec<ClipData>,
    pub clip_backend: String,
    pub paste_at: Option<i64>,
    pub words: Option<WordList>,
    pub tracker: WordTracker,
    pub timer: Timer,
    pub mirror: Option<Mirror>,
    pub profiles: Vec<Profile>,
    pub profile: Option<String>,
    pub app: Option<String>,
    pub kwin_script: bool,
    pub pen: Option<(Pen, PenDev)>,
    pub laser: Option<Tablet>,
    pub highlight: bool,
    pub target_labels: Option<(String, HashMap<u16, [String; 4]>)>,
    pub gestures_run: u32,
    pub last_gesture: String,
    pub warned_abs: bool,
}

fn now() -> i64 {
    mono_ms()
}

impl D {
    // ---------------------------------------------------------------- configuration

    /// the router settings of the M3 modules (depends on the current layout and target)
    pub(super) fn m3_router(&self, rc: &mut RouterCfg) {
        let f = &self.cfg.features;
        rc.gestures = f.gestures.enabled;
        rc.textedit = f.textedit.enabled;
        rc.joy_dead_mm = f.textedit.n("joystick_dead_mm", 3.0) as f32;
        if f.textedit.enabled && !f.textedit.b("joystick", true) {
            rc.joy_dead_mm = 1e6;
        }
        rc.laser = f.remote.enabled && f.remote.b("laser", false) && self.router.layout().name == "remote";
        rc.clip_band = f.clipboard.enabled;
        rc.suggest_band = f.suggest.enabled && f.suggest.b("words", true);
        rc.emoji = f.suggest.enabled && f.suggest.b("emoji_panel", true);
        rc.mirror_pct = if f.mirror.enabled { f.mirror.n("size_pct", 35.0) as f32 } else { 0.0 };
        rc.target_label = if f.usbhid.enabled {
            Some(match self.m3.target.as_str() {
                "usb" => "To: USB device".into(),
                _ => "To: this desktop".into(),
            })
        } else {
            None
        };
    }

    /// apply M3 setting changes (start / stop the parts that run in the background)
    pub(super) fn m3_apply(&mut self, old: Option<&Config>) {
        let f = self.cfg.features.clone();
        // clipboard history
        let cc = (f.clipboard.enabled, f.clipboard.n("items", 8.0) as usize, f.clipboard.b("images", false));
        if cc != self.m3.clip_cfg {
            self.m3.clip_cfg = cc;
            self.m3.clip = None;
            self.m3.clips.clear();
            let o = self.router.set_clips(vec![]);
            self.apply(o);
            if cc.0 {
                let c = clipboard::start(self.sh.clone(), cc.1, cc.2);
                println!("CLIP t={} backend={} items={} images={}", now(), c.backend, cc.1, cc.2);
                self.m3.clip_backend = c.backend.into();
                self.m3.clip = Some(c);
            }
        }
        // word list
        if f.suggest.enabled && f.suggest.b("words", true) {
            if self.m3.words.is_none() {
                let user = std::fs::read_to_string(self.cfg_dir.join("words.txt")).unwrap_or_default();
                let w = WordList::builtin().with_user(&user);
                println!("SUGGEST words={} (local list; desktop input method: {})", w.len(), im_note());
                self.m3.words = Some(w);
            }
        } else if self.m3.words.take().is_some() {
            self.m3.tracker.reset();
            let o = self.router.set_suggestions(vec![]);
            self.apply(o);
        }
        // timer
        let tm = f.remote.n("timer_min", 0.0) as i64;
        if self.m3.timer.total_ms != tm * 60_000 && !self.m3.timer.running() {
            self.m3.timer = Timer::new(tm);
        }
        // profiles
        if f.profiles.enabled {
            self.m3_profiles_reload();
        }
        let want_script = f.profiles.enabled && f.profiles.b("per_app", false);
        if want_script != self.m3.kwin_script {
            self.m3.kwin_script = want_script;
            kwin_app_script(want_script);
            println!("PROFILE-APPS t={} kwin_script={}", now(), want_script);
        }
        // mirror only while in pad mode
        if self.grabbed {
            self.m3_mirror(f.mirror.enabled);
        }
        self.m3_laser_sync();
        if old.is_some_and(|o| o.features.pen != f.pen) && self.grabbed {
            self.m3_pen_off();
            self.m3_pen_on();
        }
        // the HID target
        let want = match self.cfg.target.as_str() {
            "usb" if f.usbhid.enabled => "usb",
            _ => "desktop",
        };
        let opts_changed = old.is_some_and(|o| o.features.usbhid != f.usbhid);
        if want != self.m3.target || (opts_changed && want != "desktop" && old.is_some_and(|o| o.features.usbhid.options.get("abs_pointer") != f.usbhid.options.get("abs_pointer"))) {
            self.m3_set_target(want);
        } else if opts_changed {
            self.m3_legends();
        }
    }

    // ---------------------------------------------------------------- the HID targets

    fn target_layout(&self) -> String {
        let f = &self.cfg.features;
        let l = match self.m3.target.as_str() {
            "usb" => f.usbhid.s("layout", "desktop"),
            _ => "desktop".into(),
        };
        if l == "desktop" {
            self.xkb.1.get(self.xkb.0 as usize).cloned().filter(|s| !s.is_empty()).unwrap_or_else(|| "us".into())
        } else {
            l
        }
    }

    /// legends: the target's keyboard language while a HID target is active, the desktop's keymap otherwise
    fn m3_legends(&mut self) {
        let l = if self.m3.target == "desktop" { None } else { Some(self.target_layout()) };
        let cur = self.sh.legend_layout.lock().unwrap().clone();
        if cur != l {
            println!("LEGENDS-LAYOUT t={} {}", now(), l.clone().unwrap_or_else(|| "desktop keymap".into()));
            *self.sh.legend_layout.lock().unwrap() = l;
            self.m3.target_labels = None;
            poke(self.sh.wake_ui);
        }
    }

    fn m3_set_target(&mut self, want: &str) {
        if let Some(s) = self.m3.sink.take() {
            let r = s.close();
            println!("HID-TARGET t={} {} closed: {}", now(), self.m3.target, r);
        }
        self.m3.target = "desktop".into();
        self.m3.sink_note.clear();
        if want != "desktop" {
            let f = &self.cfg.features;
            let abs = f.usbhid.b("abs_pointer", false);
            let upm = self.router.pad_spec().map(|s| s.2 as f32).unwrap_or(124.0);
            match Sink::open(Kind::Usb, &self.cfg.hid_helper_socket, abs, f.usbhid.b("stop_other", false), &f.usbhid.s("udc", "auto"), upm) {
                Ok(s) => {
                    println!("HID-TARGET t={} {} up: {}", now(), want, s.info);
                    self.m3.sink = Some(s);
                    self.m3.target = want.into();
                }
                Err(e) => {
                    println!("HID-TARGET t={} {} failed: {}", now(), want, e);
                    self.m3.sink_note = format!("{}: {}", want, e);
                    let o = self.router.set_notice(&format!("{} mode failed: {}", want, e.chars().take(80).collect::<String>()));
                    self.apply(o);
                    if self.cfg.target != "desktop" {
                        let _ = self.set_setting("target", Some("desktop"), 0);
                    }
                }
            }
        }
        self.m3_legends();
        let rc = self.router_cfg_full();
        let o = self.router.set_cfg(rc);
        self.apply(o);
        // pad mode without a monitor: the other device is the "display"
        if self.m3.target != "desktop" {
            if !self.machine.connected {
                self.m3.hid_connected = true;
                self.handle(Ev::Connected(Some(Remember::Pad)), "hid-target");
            }
            self.handle(Ev::Resume, "hid-target");
            self.m3.info_next = 0;
        } else {
            let o = self.router.set_notice("");
            self.apply(o);
            if self.m3.hid_connected {
                self.m3.hid_connected = false;
                if self.real_conn.is_none() && !self.sim {
                    self.handle(Ev::Disconnected, "hid-target-off");
                }
            }
        }
    }

    /// a key event: to the HID sink or the virtual keyboard; feeds the word tracker
    pub(super) fn out_key(&mut self, c: u16, p: bool) {
        if let Some(s) = self.m3.sink.as_mut() {
            s.key(c, p);
        } else {
            self.kb_key(c, p);
        }
        if p && self.m3.words.is_some() && !keycodes::is_modifier(c) {
            self.m3_track(c);
        }
    }

    /// a touchpad frame: true when the HID sink took it
    pub(super) fn m3_pad(&mut self, f: &PadFrame) -> bool {
        if f.slots.iter().any(|s| s.is_some()) && !self.m3.tracker.word.is_empty() {
            // a touch may move the caret: the word ends
            self.m3.tracker.reset();
            let o = self.router.set_suggestions(vec![]);
            self.apply(o);
        }
        match self.m3.sink.as_mut() {
            Some(s) => {
                s.pad(f, now());
                true
            }
            None => false,
        }
    }

    // ---------------------------------------------------------------- outputs of the router

    pub(super) fn m3_out(&mut self, o: Out) {
        match o {
            Out::Gesture(g) => self.m3_gesture(&g),
            Out::Chip(band, i) => self.m3_chip(&band, i),
            Out::Emoji(e) => self.m3_emoji(&e),
            Out::Laser(p) => self.m3_laser(p),
            Out::Timer(s) => {
                let t = now();
                if s == "reset" {
                    self.m3.timer.reset();
                } else {
                    self.m3.timer.toggle(t);
                }
                println!("TIMER t={} {} running={} text=\"{}\"", t, s, self.m3.timer.running(), self.m3.timer.text(t));
                self.info_next = 0;
            }
            Out::TargetNext => {
                let f = &self.cfg.features;
                let order: Vec<&str> = ["desktop", "usb"].into_iter().filter(|t| *t == "desktop" || (*t == "usb" && f.usbhid.enabled)).collect();
                let i = order.iter().position(|t| *t == self.cfg.target).unwrap_or(0);
                let next = order[(i + 1) % order.len()].to_string();
                if let Err(e) = self.set_setting("target", Some(&next), 0) {
                    println!("SETTING-REJECTED key=target ({})", e);
                }
            }
            _ => {}
        }
    }

    fn m3_gesture(&mut self, g: &str) {
        let name = gestures::mapped(&self.cfg.features.gestures.options, g);
        let a = gestures::action(&name, &self.cfg.quick_actions);
        println!("GESTURE t={} gesture={} action={} ok={}", now(), g, name, a.is_ok());
        self.m3.gestures_run += 1;
        self.m3.last_gesture = format!("{}={}", g, name);
        match a {
            Ok(GAct::Chord(codes)) => {
                for (c, p) in quick::chord_events(&codes) {
                    self.out_key(c, p);
                }
            }
            Ok(GAct::MiddleClick) => {
                let o = self.router.pad_click(dexhid_core::touchpad::BTN_MIDDLE);
                self.apply(o);
            }
            Ok(GAct::Quick(id)) => self.run_quick_any(&id),
            Ok(GAct::None) => {}
            Err(e) => println!("WARN gesture {}: {}", g, e),
        }
    }

    /// a quick action by id, shown in the bar or not (gestures may run any of them)
    pub(super) fn run_quick_any(&mut self, id: &str) {
        if self.quick.iter().any(|q| q.id == id) {
            return self.run_quick(id);
        }
        let q = dexhid_core::config::QuickCfg { enabled: true, actions: vec![id.to_string()], custom: self.cfg.quick_actions.custom.clone() };
        let (items, _) = quick::resolve(&q, &desktop::have_program);
        let Some(it) = items.into_iter().next() else {
            println!("WARN quick action {} unknown / not installed", id);
            return;
        };
        self.quick.push(it);
        self.run_quick(id);
        self.quick.retain(|x| x.id != id || self.cfg.quick_actions.actions.iter().any(|a| a.split(':').next() == Some(id)));
    }

    // ---------------------------------------------------------------- typing text

    fn labels_now(&mut self) -> HashMap<u16, [String; 4]> {
        if self.m3.target == "desktop" {
            let l = self.sh.labels.lock().unwrap().clone();
            if !l.is_empty() {
                return l;
            }
        } else {
            let lay = self.target_layout();
            if self.m3.target_labels.as_ref().is_none_or(|x| x.0 != lay) {
                let m = crate::xkb::keymap_from_layout(&lay).and_then(|k| crate::xkb::labels(&k, 0, &keycodes::all_codes())).map(|l| l.map).unwrap_or_default();
                println!("TYPE-LAYOUT t={} layout={} keys={}", now(), lay, m.len());
                self.m3.target_labels = Some((lay, m));
            }
            if let Some((_, m)) = &self.m3.target_labels {
                if !m.is_empty() {
                    return m.clone();
                }
            }
        }
        // fallback: US legends
        keycodes::all_codes()
            .into_iter()
            .filter_map(|c| {
                let (a, b) = keycodes::us_legend(c);
                (a.chars().count() == 1).then(|| (c, [a.to_lowercase(), b.to_string(), String::new(), String::new()]))
            })
            .collect()
    }

    /// type `text` through the current target (keymap of the target language); returns typed / missing chars
    pub(super) fn type_text(&mut self, text: &str) -> (usize, Vec<char>) {
        let rev = extras::reverse_map(&self.labels_now());
        let (ev, miss) = extras::type_text(text, &rev);
        let n = ev.len() / 2;
        let words = self.m3.words.take();
        for (c, p) in ev {
            self.out_key(c, p);
        }
        self.m3.words = words;
        if let Some(s) = self.m3.sink.as_mut() {
            s.flush();
        }
        (n, miss)
    }

    fn m3_track(&mut self, c: u16) {
        match c {
            14 => self.m3.tracker.backspace(),
            _ => {
                let l = self.labels_now();
                let shift = self.router.model().shift;
                let caps = self.leds.0;
                let ch = l.get(&c).map(|v| {
                    let s = &v[0];
                    let up = shift ^ (caps && s.chars().all(|x| x.is_alphabetic()));
                    if up && !v[1].is_empty() { v[1].clone() } else { s.clone() }
                });
                let ch = ch.and_then(|s| {
                    let mut it = s.chars();
                    match (it.next(), it.next()) {
                        (Some(x), None) => Some(x),
                        _ => None,
                    }
                });
                self.m3.tracker.typed(ch);
            }
        }
        let n = self.cfg.features.suggest.n("count", 3.0) as usize;
        let s = self.m3.words.as_ref().map(|w| w.suggest(&self.m3.tracker.word, n)).unwrap_or_default();
        let o = self.router.set_suggestions(s);
        self.apply(o);
    }

    // ---------------------------------------------------------------- chips, emoji

    fn m3_chip(&mut self, band: &str, i: usize) {
        match band {
            "suggest" => {
                let s = self.router.model().bands.iter().find(|b| b.id == "suggest").and_then(|b| b.chips.get(i + self.cfg.features.suggest.enabled as usize * self.cfg.features.suggest.b("emoji_panel", true) as usize)).map(|c| c.0.clone());
                let Some(w) = s else { return };
                let (del, text) = self.m3.tracker.accept(&w);
                println!("SUGGEST-ACCEPT t={} word={} delete={}", now(), w, del);
                for _ in 0..del {
                    self.out_key(14, true);
                    self.out_key(14, false);
                }
                let (n, miss) = self.type_text(&text);
                if !miss.is_empty() {
                    println!("NOTE suggestion: {} chars not in the keymap", miss.len());
                }
                let _ = n;
                let o = self.router.set_suggestions(vec![]);
                self.apply(o);
            }
            _ => {
                let Some(d) = self.m3.clips.get(i).cloned() else { return };
                if self.m3.target != "desktop" {
                    match d {
                        ClipData::Text(t) => {
                            let (n, miss) = self.type_text(&t);
                            println!("CLIP-TYPE t={} chars={} keys={} missing={} target={}", now(), t.chars().count(), n, miss.len(), self.m3.target);
                        }
                        ClipData::Image(..) => println!("NOTE clipboard image: a keyboard cannot paste an image on another device"),
                    }
                    return;
                }
                if let Some(c) = &self.m3.clip {
                    c.set(d);
                    self.m3.paste_at = Some(now() + 120);
                    println!("CLIP-PASTE t={} item={} (selection set, Ctrl+V follows)", now(), i);
                }
            }
        }
    }

    fn m3_emoji(&mut self, e: &str) {
        if self.m3.target != "desktop" {
            // a keyboard has no emoji keys: open the other device's emoji picker (Windows: Win+.)
            println!("EMOJI t={} target={} -> the target's picker (Meta+.)", now(), self.m3.target);
            for (c, p) in quick::chord_events(&[keycodes::KEY_LEFTMETA, 52]) {
                self.out_key(c, p);
            }
            return;
        }
        match &self.m3.clip {
            Some(c) => {
                c.set(ClipData::Text(e.to_string()));
                self.m3.paste_at = Some(now() + 120);
                println!("EMOJI t={} codepoints={} via the clipboard + Ctrl+V", now(), e.chars().map(|c| format!("U+{:X}", c as u32)).collect::<Vec<_>>().join(","));
            }
            None => {
                // without a clipboard backend: start one for this (data-control / Klipper)
                let c = clipboard::start(self.sh.clone(), 8, false);
                c.set(ClipData::Text(e.to_string()));
                self.m3.clip = Some(c);
                self.m3.paste_at = Some(now() + 250);
                println!("EMOJI t={} via a clipboard backend started for it", now());
            }
        }
    }

    // ---------------------------------------------------------------- laser + pen

    fn ext_output(&self) -> Option<String> {
        self.outputs.iter().find(|n| *n != &self.cfg.internal_output && !n.starts_with("DSI") && !n.starts_with("eDP") && !n.starts_with("LVDS")).cloned()
    }

    fn map_to_monitor(&self, node: &str) {
        let Some(out) = self.ext_output() else {
            println!("NOTE {} not mapped: no external output", node);
            return;
        };
        let n = node.rsplit('/').next().unwrap_or_default().to_string();
        let sh = self.sh.clone();
        std::thread::spawn(move || {
            let r = desktop::kwin_apply(&n, &[("outputName", zbus::zvariant::Value::from(out.as_str()))]);
            sh.send(Cmd::Note(r));
        });
    }

    fn m3_laser(&mut self, p: Option<(f32, f32)>) {
        if let Some(s) = self.m3.sink.as_mut() {
            match p {
                Some((x, y)) => {
                    if !s.abs(x, y, 0) && !self.m3.warned_abs {
                        self.m3.warned_abs = true;
                        println!("NOTE laser on another device needs the absolute pointer option");
                    }
                }
                None => {}
            }
            return;
        }
        match p {
            Some((x, y)) => {
                if self.m3.laser.is_none() {
                    match Tablet::create("dexhid laser", (32767, 32767), (100, 100), None, None) {
                        Ok(t) => {
                            println!("UINPUT laser node={}", t.u.node);
                            self.map_to_monitor(&t.u.node.clone());
                            self.m3.laser = Some(t);
                        }
                        Err(e) => {
                            println!("ERROR laser device: {}", e);
                            return;
                        }
                    }
                }
                if let Some(t) = self.m3.laser.as_mut() {
                    let first = !t.near;
                    t.hover(x, y);
                    if first {
                        println!("LASER t={} on x={:.3} y={:.3}", now(), x, y);
                        if self.cfg.features.remote.b("laser_highlight", true) && !self.m3.highlight {
                            self.m3.highlight = true;
                            track_mouse();
                        }
                    }
                }
            }
            None => {
                if let Some(t) = self.m3.laser.as_mut() {
                    t.away();
                    println!("LASER t={} off", now());
                }
                if self.m3.highlight {
                    self.m3.highlight = false;
                    track_mouse();
                }
            }
        }
    }

    /// the laser device exists (silent: no event until a touch) while pad mode + laser + layout remote are on, so a
    /// test client can attach first; else it goes
    pub(super) fn m3_laser_sync(&mut self) {
        let f = &self.cfg.features.remote;
        let want = self.grabbed && f.enabled && f.b("laser", false) && self.router.layout().name == "remote" && self.m3.sink.is_none();
        if want && self.m3.laser.is_none() {
            match Tablet::create("dexhid laser", (32767, 32767), (100, 100), None, None) {
                Ok(t) => {
                    println!("UINPUT laser node={}", t.u.node);
                    self.map_to_monitor(&t.u.node.clone());
                    self.m3.laser = Some(t);
                }
                Err(e) => println!("ERROR laser device: {}", e),
            }
        } else if !want && self.m3.laser.is_some() {
            self.m3_laser_off();
        }
    }

    fn m3_laser_off(&mut self) {
        if let Some(mut t) = self.m3.laser.take() {
            t.away();
            crate::devices::destroy_later("laser", t.u);
        }
        if self.m3.highlight {
            self.m3.highlight = false;
            track_mouse();
        }
    }

    pub(super) fn m3_pen_on(&mut self) {
        let f = &self.cfg.features.pen;
        if !f.enabled || self.m3.pen.is_some() {
            return;
        }
        let Some(d) = self.pen_dev.as_ref() else { return };
        let (Some(ax), Some(ay)) = (d.absinfo(ABS_X), d.absinfo(penl::ABS_Y)) else {
            println!("WARN pen: no ABS_X / ABS_Y");
            return;
        };
        let rot = match self.router.geometry().map(|g| g.transform) {
            Some(dexhid_core::geom::Transform::R90) => 90,
            Some(dexhid_core::geom::Transform::R180) => 180,
            Some(dexhid_core::geom::Transform::R270) => 270,
            _ => 0,
        };
        let mode = if f.s("mode", "pointer") == "tablet" { PenMode::Tablet } else { PenMode::Pointer };
        let max = (ax.maximum, ay.maximum);
        // pointer: the full panel width = about 1920 px at speed 1
        let gain = f.n("speed", 1.0) as f32 * 1920.0 / max.0.max(1) as f32;
        let dev = if self.m3.sink.is_some() {
            PenDev::Sink
        } else if mode == PenMode::Tablet {
            let (_, _, rmax) = penl::rotate(0, 0, max, rot);
            let res = if rot % 180 == 90 { (ay.resolution, ax.resolution) } else { (ax.resolution, ay.resolution) };
            let pr = d.absinfo(penl::ABS_PRESSURE).map(|a| a.maximum);
            let tl = d.absinfo(penl::ABS_TILT_X).map(|a| (a.minimum, a.maximum));
            match Tablet::create("dexhid pen", rmax, res, pr.or(Some(4095)), tl.or(Some((-90, 90)))) {
                Ok(t) => {
                    println!("UINPUT pen node={} mode=tablet max={:?} rot={}", t.u.node, rmax, rot);
                    self.map_to_monitor(&t.u.node.clone());
                    PenDev::Tablet(t)
                }
                Err(e) => {
                    println!("ERROR pen tablet device: {}", e);
                    return;
                }
            }
        } else {
            match crate::devices::VirtualMouse::create_named("dexhid pen pointer") {
                Ok(m) => {
                    println!("UINPUT pen node={} mode=pointer gain={:.3}", m.u.node, gain);
                    PenDev::Pointer(m)
                }
                Err(e) => {
                    println!("ERROR pen pointer device: {}", e);
                    return;
                }
            }
        };
        let g = d.grab(true).is_ok();
        println!("PEN-GRAB t={} grabbed={} mode={:?} target={}", now(), g, mode, self.m3.target);
        self.m3.pen = Some((Pen::new(PenCfg { mode, rot, max, gain }), dev));
    }

    pub(super) fn m3_pen_off(&mut self) {
        let Some((mut p, dev)) = self.m3.pen.take() else { return };
        let o = p.release();
        match dev {
            PenDev::Pointer(mut m) => {
                let mo: Vec<_> = o.iter().filter_map(|x| if let PenOut::Button(b, v) = x { Some(dexhid_core::mouse::MOut::Button(*b, *v)) } else { None }).collect();
                m.apply(&mo);
                crate::devices::destroy_later("pen", m);
            }
            PenDev::Tablet(mut t) => {
                t.away();
                crate::devices::destroy_later("pen", t.u);
            }
            PenDev::Sink => {}
        }
        if let Some(d) = self.pen_dev.as_ref() {
            let _ = d.grab(false);
        }
        println!("PEN-GRAB t={} released", now());
    }

    /// events of the real pen; true = consumed (pad mode with the pen module)
    pub(super) fn m3_pen_events(&mut self, evs: &[InputEvent]) -> bool {
        let Some((p, _)) = self.m3.pen.as_mut() else { return false };
        let mut outs = vec![];
        for e in evs {
            outs.extend(p.feed(e.type_, e.code, e.value));
        }
        let max = p.cfg.max;
        let rot = p.cfg.rot;
        for o in outs {
            match (&mut self.m3.pen, o) {
                (Some((_, PenDev::Pointer(m))), PenOut::Rel(x, y)) => m.apply(&[dexhid_core::mouse::MOut::Rel(x, y)]),
                (Some((_, PenDev::Pointer(m))), PenOut::Button(b, v)) => m.apply(&[dexhid_core::mouse::MOut::Button(b, v)]),
                (Some((_, PenDev::Tablet(t))), PenOut::Abs(s)) => t.pen(&s),
                (Some((_, PenDev::Sink)), PenOut::Rel(x, y)) => {
                    if let Some(s) = self.m3.sink.as_mut() {
                        s.mouse_outs(&[dexhid_core::mouse::MOut::Rel(x, y)]);
                    }
                }
                (Some((_, PenDev::Sink)), PenOut::Button(b, v)) => {
                    if let Some(s) = self.m3.sink.as_mut() {
                        s.mouse_outs(&[dexhid_core::mouse::MOut::Button(b, v)]);
                    }
                }
                (Some((_, PenDev::Sink)), PenOut::Abs(st)) => {
                    let (_, _, rm) = penl::rotate(0, 0, max, rot);
                    if let Some(s) = self.m3.sink.as_mut() {
                        if st.near {
                            s.abs(st.x as f32 / rm.0.max(1) as f32, st.y as f32 / rm.1.max(1) as f32, st.touch as u8);
                        }
                    }
                }
                _ => {}
            }
        }
        true
    }

    // ---------------------------------------------------------------- mirror

    fn m3_mirror(&mut self, on: bool) {
        match (on, self.m3.mirror.is_some()) {
            (true, false) => {
                let fps = self.cfg.features.mirror.n("fps", 2.0) as u32;
                println!("MIRROR t={} start fps={}", now(), fps);
                self.m3.mirror = Some(mirror::start(self.sh.clone(), fps));
            }
            (false, true) => {
                if let Some(m) = self.m3.mirror.take() {
                    m.stop();
                    println!("MIRROR t={} stop", now());
                }
            }
            _ => {}
        }
    }

    // ---------------------------------------------------------------- grab / release / exit

    pub(super) fn m3_on_grab(&mut self) {
        self.m3_pen_on();
        self.m3_laser_sync();
        self.m3_mirror(self.cfg.features.mirror.enabled);
        if self.cfg.features.profiles.enabled {
            self.m3_select_profile("grab");
        }
    }

    pub(super) fn m3_on_release(&mut self) -> usize {
        let n = self.m3.sink.as_mut().map(|s| s.release_all()).unwrap_or(0);
        self.m3_pen_off();
        self.m3_laser_off();
        self.m3_mirror(false);
        self.m3.tracker.reset();
        n
    }

    pub(super) fn m3_exit(&mut self) {
        self.m3_on_release();
        if let Some(s) = self.m3.sink.take() {
            println!("HID-TARGET t={} closed at exit: {}", now(), s.close());
        }
        if self.m3.kwin_script {
            kwin_app_script(false);
        }
    }

    // ---------------------------------------------------------------- loop integration

    pub(super) fn m3_poll_fds(&self) -> Vec<i32> {
        self.m3.sink.as_ref().map(|s| s.poll_fds()).unwrap_or_default()
    }

    pub(super) fn m3_readable(&mut self, fd: i32) {
        let Some(s) = self.m3.sink.as_mut() else { return };
        for e in s.readable(fd) {
            match e {
                hidsink::Event::Leds(c, n) => {
                    println!("LEDS t={} caps={} num={} (target {})", now(), c, n, self.m3.target);
                    self.leds = (c, n);
                    let o = self.router.set_leds(c, n);
                    self.apply(o);
                }
                hidsink::Event::Note(n) => println!("NOTE {}", n),
                hidsink::Event::HelperGone => {
                    println!("ERROR HID helper gone: back to this desktop");
                    if let Some(s) = self.m3.sink.take() {
                        drop(s);
                    }
                    self.m3.target = "desktop".into();
                    let _ = self.set_setting("target", Some("desktop"), 0);
                    self.m3_set_target("desktop");
                }
            }
        }
    }

    pub(super) fn m3_deadlines(&self, dl: &mut Vec<i64>) {
        if self.m3.sink.as_ref().is_some_and(|s| s.queued()) {
            dl.push(now() + 8);
        }
        if let Some(p) = self.m3.paste_at {
            dl.push(p);
        }
        if self.grabbed && (self.m3.timer.running() || self.m3.target != "desktop") {
            dl.push(self.m3.info_next);
        }
    }

    pub(super) fn m3_tick(&mut self, t: i64) {
        if let Some(s) = self.m3.sink.as_mut() {
            if s.queued() {
                s.flush();
            }
        }
        if self.m3.paste_at.is_some_and(|p| t >= p) {
            self.m3.paste_at = None;
            for (c, p) in quick::chord_events(&[keycodes::KEY_LEFTCTRL, 47]) {
                self.out_key(c, p);
            }
            println!("CLIP-PASTE t={} Ctrl+V sent", t);
        }
        if self.grabbed && (self.m3.timer.running() || self.m3.target != "desktop") && t >= self.m3.info_next {
            self.m3.info_next = t + if self.m3.timer.running() { 1000 } else { 5000 };
            self.info_next = 0;
            if self.router.sheet_open() && self.m3.target != "desktop" {
                let s = self.build_sheet();
                let o = self.router.set_sheet(s);
                self.apply(o);
            }
        }
    }

    /// the strip text: timer + HID target state (power, role) before the M2 battery / display info
    pub(super) fn m3_info(&self, base: String) -> String {
        let mut v = vec![];
        let t = self.m3.timer.text(now());
        if !t.is_empty() {
            v.push(format!("Timer {}", t));
        }
        if self.m3.target != "desktop" {
            if let Some(s) = &self.m3.sink {
                v.push(format!("{}{}", self.m3.target.to_uppercase(), if s.connected() { " connected" } else { " waiting for a host" }));
            }
            if self.m3.target == "usb" {
                let pw = dexhid_core::gadget::power(std::path::Path::new("/sys/class/power_supply"), std::path::Path::new("/sys/class/typec"));
                let roles = dexhid_core::gadget::roles(std::path::Path::new("/sys/class/usb_role"));
                let (ok, rt) = dexhid_core::gadget::role_text(&roles);
                v.push(pw.line());
                if !ok {
                    v.push(rt);
                }
            }
            return v.join("  |  ");
        }
        if !base.is_empty() {
            v.push(base);
        }
        v.join("  |  ")
    }

    /// the live status line of the USB tab of the settings sheet
    pub(super) fn m3_sheet_note(&self, id: &str) -> Option<String> {
        match id {
            "usbhid" => {
                let pw = dexhid_core::gadget::power(std::path::Path::new("/sys/class/power_supply"), std::path::Path::new("/sys/class/typec"));
                let roles = dexhid_core::gadget::roles(std::path::Path::new("/sys/class/usb_role"));
                let st = match (&self.m3.sink, self.m3.target.as_str()) {
                    (Some(s), "usb") => format!("active, {} reports sent", s.sent),
                    _ => "off".into(),
                };
                Some(format!("{}  |  {}  |  {}", st, pw.line(), dexhid_core::gadget::role_text(&roles).1))
            }
            _ => None,
        }
    }

    // ---------------------------------------------------------------- profiles

    fn profiles_dir(&self) -> PathBuf {
        self.cfg_dir.join("profiles")
    }

    fn m3_profiles_reload(&mut self) {
        let (ps, notes) = profiles::load_all(&self.profiles_dir());
        for n in notes {
            println!("WARN {}", n);
        }
        if ps.iter().map(|p| &p.name).collect::<Vec<_>>() != self.m3.profiles.iter().map(|p| &p.name).collect::<Vec<_>>() {
            println!("PROFILES {}", ps.iter().map(|p| p.name.clone()).collect::<Vec<_>>().join(","));
        }
        self.m3.profiles = ps;
    }

    /// the user's settings with the active profile on top
    pub(super) fn layered(&self, user: &toml::Table) -> toml::Table {
        let p = self.m3.profile.as_ref().and_then(|n| self.m3.profiles.iter().find(|p| &p.name == n));
        profiles::layered(user, p)
    }

    /// the arrangement directory: the active profile's, else the config dir
    pub(super) fn arr_dir(&self) -> PathBuf {
        match &self.m3.profile {
            Some(n) => self.profiles_dir().join(n),
            None => self.cfg_dir.clone(),
        }
    }

    pub(super) fn m3_select_profile(&mut self, why: &str) {
        if !self.cfg.features.profiles.enabled && self.m3.profile.is_none() {
            return;
        }
        let per_app = self.cfg.features.profiles.b("per_app", false);
        let pick = if self.cfg.features.profiles.enabled {
            profiles::select(&self.m3.profiles, self.monitor_id.as_deref().or(self.real_conn.as_ref().map(|c| c.name.as_str())), self.m3.app.as_deref(), per_app, self.persist.profile.as_deref()).map(|p| p.name.clone())
        } else {
            None
        };
        if pick == self.m3.profile {
            return;
        }
        println!("PROFILE t={} active={} was={} why={}", now(), pick.clone().unwrap_or_else(|| "none".into()), self.m3.profile.clone().unwrap_or_else(|| "none".into()), why);
        self.m3.profile = pick.clone();
        let t = self.layered(&self.settings.clone());
        let (c, notes) = dexhid_core::config::load_with(&self.paths, &t, self.opts.config.is_some());
        for n in notes.iter().filter(|n| n.starts_with("ERROR")) {
            println!("CONFIG {}", n);
        }
        let old = std::mem::replace(&mut self.cfg, c);
        self.apply_config(Some(old));
        let layout = pick.and_then(|n| self.m3.profiles.iter().find(|p| p.name == n)).and_then(|p| p.layout.clone());
        let cur = self.router.layout().name.clone();
        let l = layout.unwrap_or_else(|| self.persist.keyboard.clone().unwrap_or(cur.clone()));
        // the arrangement of the profile's directory comes with the layout (re)load
        self.set_layout_inner(&l, false);
    }

    fn m3_profile_cmd(&mut self, cmd: &str) -> Result<String, String> {
        let w: Vec<&str> = cmd.split_whitespace().collect();
        self.m3_profiles_reload();
        match w.as_slice() {
            ["list"] => Ok(serde_json::json!({
                "profiles": self.m3.profiles.iter().map(|p| serde_json::json!({"name": p.name, "monitors": p.monitors.len(), "apps": p.apps, "layout": p.layout})).collect::<Vec<_>>(),
                "active": self.m3.profile,
                "forced": self.persist.profile,
                "enabled": self.cfg.features.profiles.enabled,
            })
            .to_string()),
            ["save", name, rest @ ..] => {
                let mut p = Profile { name: name.to_string(), layout: Some(self.router.layout().name.clone()), settings: self.settings.clone(), ..Default::default() };
                for r in rest {
                    if *r == "monitor" {
                        p.monitors.push(self.monitor_id.clone().map(|m| m.chars().take(12).collect()).or_else(|| self.real_conn.as_ref().map(|c| c.name.clone())).unwrap_or_else(|| "*".into()));
                    } else if let Some(a) = r.strip_prefix("app=") {
                        p.apps.push(a.to_string());
                    } else {
                        return Err(format!("unknown save option {:?} (monitor, app=CLASS)", r));
                    }
                }
                let f = profiles::save(&self.profiles_dir(), &p)?;
                // the current arrangements go with it
                let mut copied = 0;
                if let Ok(rd) = std::fs::read_dir(self.arr_dir()) {
                    for e in rd.flatten() {
                        let n = e.file_name().to_string_lossy().into_owned();
                        if n.starts_with("arrangement-") && n.ends_with(".toml") && std::fs::copy(e.path(), self.profiles_dir().join(name).join(&n)).is_ok() {
                            copied += 1;
                        }
                    }
                }
                self.m3_profiles_reload();
                println!("PROFILE-SAVED name={} monitors={} apps={} arrangements={}", name, p.monitors.len(), p.apps.len(), copied);
                Ok(format!("saved {} ({} arrangements)", f.display(), copied))
            }
            ["use", name] => {
                if !self.m3.profiles.iter().any(|p| p.name == *name) {
                    return Err(format!("no profile {}", name));
                }
                self.persist.profile = Some(name.to_string());
                let _ = self.persist.save(&self.state_file);
                self.m3_select_profile("use");
                Ok(format!("active {}", name))
            }
            ["auto"] => {
                self.persist.profile = None;
                let _ = self.persist.save(&self.state_file);
                self.m3_select_profile("auto");
                Ok(format!("automatic ({})", self.m3.profile.clone().unwrap_or_else(|| "none".into())))
            }
            ["delete", name] => {
                if self.m3.profile.as_deref() == Some(*name) {
                    self.persist.profile = None;
                    self.m3.profile = None;
                }
                profiles::delete(&self.profiles_dir(), name)?;
                self.m3_profiles_reload();
                self.m3_select_profile("delete");
                Ok(format!("deleted {}", name))
            }
            _ => Err("profile list | save NAME [monitor] [app=CLASS ..] | use NAME | auto | delete NAME".into()),
        }
    }

    // ---------------------------------------------------------------- commands

    pub(super) fn m3_cmd(&mut self, c: Cmd) {
        match c {
            Cmd::Clips(v, backend) => {
                self.m3.clip_backend = backend;
                let labels: Vec<String> = v.iter().map(|d| extras::label(d, 28)).collect();
                println!("CLIPS t={} n={} backend={}", now(), v.len(), self.m3.clip_backend);
                self.m3.clips = v;
                let o = self.router.set_clips(labels);
                self.apply(o);
            }
            Cmd::MirrorSize(w, h) => {
                let o = self.router.set_mirror_src(w, h);
                self.apply(o);
            }
            Cmd::ActiveApp(a) => {
                if self.m3.app.as_deref() != Some(a.as_str()) {
                    if self.opts.debug {
                        println!("ACTIVE-APP t={} class={}", now(), a);
                    }
                    self.m3.app = Some(a);
                    if self.cfg.features.profiles.b("per_app", false) {
                        self.m3_select_profile("app");
                    }
                }
            }
            Cmd::Profile(cmd, reply) => {
                let r = self.m3_profile_cmd(&cmd);
                let _ = reply.send(r);
            }
            Cmd::Act(cmd, reply) => {
                let r = self.m3_act(&cmd);
                let _ = reply.send(r);
            }
            Cmd::HidStatus(reply) => {
                let _ = reply.send(self.m3_hid_status());
            }
            _ => {}
        }
    }

    fn m3_hid_status(&self) -> String {
        let pw = dexhid_core::gadget::power(std::path::Path::new("/sys/class/power_supply"), std::path::Path::new("/sys/class/typec"));
        let roles = dexhid_core::gadget::roles(std::path::Path::new("/sys/class/usb_role"));
        let helper = if self.m3.sink.is_some() { "connected (in use)".to_string() } else { hidsink::helper_query(&self.cfg.hid_helper_socket, "STATUS").unwrap_or_else(|e| format!("unavailable: {}", e)) };
        serde_json::json!({
            "target": self.m3.target,
            "setting": self.cfg.target,
            "usb_enabled": self.cfg.features.usbhid.enabled,
            "sink": self.m3.sink.as_ref().map(|s| serde_json::json!({"kind": s.kind.name(), "info": s.info, "host": s.host, "connected": s.connected(), "sent": s.sent, "dropped_no_host": s.dropped_no_host})),
            "note": self.m3.sink_note,
            "legend_layout": self.sh.legend_layout.lock().unwrap().clone(),
            "helper": helper,
            "power": pw.line(),
            "roles": roles,
            "role_text": dexhid_core::gadget::role_text(&roles).1,
        })
        .to_string()
    }

    fn m3_act(&mut self, cmd: &str) -> Result<String, String> {
        let (verb, arg) = cmd.split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((cmd, ""));
        match verb {
            "gesture" => {
                if !gestures::GESTURES.contains(&arg) {
                    return Err(format!("gesture one of {:?}", gestures::GESTURES));
                }
                self.m3_gesture(arg);
                Ok(self.m3.last_gesture.clone())
            }
            "emoji-panel" => {
                let o = self.router.toggle_emoji();
                self.apply(o);
                Ok(format!("open={}", self.router.emoji_open()))
            }
            "emoji" => {
                self.m3_emoji(arg);
                Ok("ok".into())
            }
            "clip" => {
                let i: usize = arg.parse().map_err(|_| "clip N")?;
                if i >= self.m3.clips.len() {
                    return Err(format!("{} items", self.m3.clips.len()));
                }
                self.m3_chip("clip", i);
                Ok("ok".into())
            }
            "clips" => Ok(serde_json::json!({"backend": self.m3.clip_backend, "items": self.m3.clips.iter().map(|d| extras::label(d, 40)).collect::<Vec<_>>()}).to_string()),
            "suggest" => {
                let i: usize = arg.parse().map_err(|_| "suggest N")?;
                self.m3_chip("suggest", i);
                Ok("ok".into())
            }
            "words" => Ok(self.m3.words.as_ref().map(|w| w.suggest(arg, 5).join(",")).ok_or("word suggestions are off")?),
            "select-mode" => {
                let o = self.router.toggle_select();
                self.apply(o);
                Ok(format!("select={}", self.router.select_mode()))
            }
            "timer" => {
                self.m3_out(Out::Timer(if arg == "reset" { "reset".into() } else { "toggle".into() }));
                Ok(self.m3.timer.text(now()))
            }
            "laser" => {
                if arg == "up" {
                    self.m3_laser(None);
                } else {
                    let v: Vec<f32> = arg.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                    if v.len() != 2 {
                        return Err("laser X Y | laser up".into());
                    }
                    self.m3_laser(Some((v[0], v[1])));
                }
                Ok(format!("device={}", self.m3.laser.as_ref().map(|t| t.u.node.clone()).unwrap_or_else(|| "none".into())))
            }
            "type" => {
                let (n, miss) = self.type_text(arg);
                Ok(format!("keys={} missing={}", n, miss.iter().collect::<String>()))
            }
            _ => Err("act gesture NAME | emoji-panel | emoji E | clip N | clips | suggest N | words PREFIX | select-mode | timer toggle|reset | laser X Y|up | type TEXT".into()),
        }
    }

    pub(super) fn m3_status(&self, m: &mut serde_json::Map<String, serde_json::Value>) {
        m.insert("target".into(), serde_json::json!(self.m3.target));
        m.insert("hid_connected".into(), serde_json::json!(self.m3.sink.as_ref().map(|s| s.connected())));
        m.insert("profile".into(), serde_json::json!(self.m3.profile));
        m.insert("clip_backend".into(), serde_json::json!(self.m3.clip.as_ref().map(|c| c.backend)));
        m.insert("clips".into(), serde_json::json!(self.m3.clips.len()));
        m.insert("mirror".into(), serde_json::json!(self.m3.mirror.is_some()));
        m.insert("timer".into(), serde_json::json!(self.m3.timer.text(now())));
        m.insert("select_mode".into(), serde_json::json!(self.router.select_mode()));
        m.insert("pen_mode".into(), serde_json::json!(self.m3.pen.as_ref().map(|p| format!("{:?}", p.0.cfg.mode).to_lowercase())));
        m.insert("gestures_run".into(), serde_json::json!(self.m3.gestures_run));
        m.insert("word".into(), serde_json::json!(self.m3.tracker.word));
        m.insert("emoji_open".into(), serde_json::json!(self.router.emoji_open()));
    }
}

/// the desktop input method (KDE: the virtual keyboard setting in kwinrc; fcitx5 / ibus on the bus)
fn im_note() -> String {
    let c = zbus::blocking::Connection::session().ok();
    let has = |n: &str| {
        c.as_ref()
            .and_then(|c| c.call_method(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", Some("org.freedesktop.DBus"), "NameHasOwner", &(n,)).ok())
            .and_then(|m| m.body().deserialize::<bool>().ok())
            .unwrap_or(false)
    };
    if has("org.fcitx.Fcitx5") {
        "fcitx5 runs: its own word hints (keyboard engine 'Enable word hint') apply to DexHid's keys too".into()
    } else if has("org.freedesktop.IBus") || has("org.freedesktop.portal.IBus") {
        "ibus runs".into()
    } else {
        "none found".into()
    }
}

/// KWin "Track Mouse" effect (a ring around the pointer) through its global shortcut (I: the action name of KWin 6)
fn track_mouse() {
    std::thread::spawn(|| {
        if let Ok(c) = zbus::blocking::Connection::session() {
            let r = c.call_method(Some("org.kde.kglobalaccel"), "/component/kwin", Some("org.kde.kglobalaccel.Component"), "invokeShortcut", &("TrackMouse",));
            println!("LASER-HIGHLIGHT invokeShortcut TrackMouse ok={}", r.is_ok());
        }
    });
}

const APP_SCRIPT: &str = "dexhid-apps";

/// load / unload the KWin script that reports the active window's class (per-app profiles)
fn kwin_app_script(on: bool) {
    std::thread::spawn(move || {
        let Ok(c) = zbus::blocking::Connection::session() else { return };
        let _ = c.call_method(Some("org.kde.KWin"), "/Scripting", Some("org.kde.kwin.Scripting"), "unloadScript", &(APP_SCRIPT,));
        if !on {
            println!("PROFILE-APPS kwin script unloaded");
            return;
        }
        let dir = std::env::var("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/tmp")).join("dexhid");
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("dexhid-apps.js");
        let js = "function dexhidReport(w) { callDBus(\"org.dexhid.DexHid1\", \"/org/dexhid/DexHid1\", \"org.dexhid.DexHid1\", \"ActiveApp\", w ? String(w.resourceClass) : \"\"); }\nworkspace.windowActivated.connect(dexhidReport);\ndexhidReport(workspace.activeWindow);\n";
        if std::fs::write(&f, js).is_err() {
            return;
        }
        let id = c
            .call_method(Some("org.kde.KWin"), "/Scripting", Some("org.kde.kwin.Scripting"), "loadScript", &(f.to_string_lossy().as_ref(), APP_SCRIPT))
            .ok()
            .and_then(|m| m.body().deserialize::<i32>().ok());
        let ran = id.is_some_and(|i| c.call_method(Some("org.kde.KWin"), format!("/Scripting/Script{}", i).as_str(), Some("org.kde.kwin.Script"), "run", &()).is_ok());
        println!("PROFILE-APPS kwin script id={:?} running={}", id, ran);
    });
}
