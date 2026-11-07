// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! `dexhid daemon`: the core thread. Owns the touchscreen grab, the virtual touchpad / mouse, the state machine and
//! the router; the virtual keyboard lives in the key guardian process (guardian.rs). The UI, D-Bus, logind, desktop
//! and layout-tracker threads talk to it through `Cmd` + a wake pipe.
//!
//! Release order (every path): keys up (guardian confirms) + pad contacts lifted + SYN, ungrab, then the devices are
//! destroyed off the critical path (a reaper thread / the guardian), the brightness restored, the UI hidden.
//! On a crash / SIGKILL the kernel ungrabs and removes the touchpad; the guardian sends the key-ups before it
//! destroys the keyboard.

use crate::dbus;
use crate::desktop::{self, Job, Worker};
use crate::devices::{destroy_later, VirtualKbd, VirtualMouse, VirtualPad};
use crate::guardian::Guardian;
use crate::notify::{self, Notify};
use crate::shared::{Cmd, Shared, SharedRef, UiCfg};
use crate::sys::*;
use dexhid_core::arrangement::{self, Arrangement, Orientation};
use dexhid_core::chord::Chord;
use dexhid_core::config::{self, Config, Persist};
use dexhid_core::feedback;
use dexhid_core::geom::Rect;
use dexhid_core::hotplug::{self, Connector};
use dexhid_core::keycodes;
use dexhid_core::layout::{self, Layout};
use dexhid_core::mouse::{Mouse, MouseCfg};
use dexhid_core::mt::Mt;
use dexhid_core::quick::{self, Act};
use dexhid_core::router::{Out, Router, RouterCfg, SheetModule};
use dexhid_core::settings::{self, Kind};
use dexhid_core::state::{Autostart, Desired, Ev, Machine, Remember, UiKind};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::Arc;

#[derive(Default, Debug)]
pub struct Opts {
    pub config: Option<PathBuf>,
    pub touch_dev: Option<String>,
    pub debug: bool,
    pub chord_test_dev: Option<String>,
    pub simulate_connect: bool,
}

fn env_dir(var: &str, home_rel: &str) -> PathBuf {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(home_rel),
    }
}

fn find_dev(name: &str) -> Option<String> {
    event_nodes().into_iter().find(|p| Dev::open(p, false).map(|d| d.name() == name).unwrap_or(false))
}

/// the RouterCfg for a config (`res` = touchscreen units / mm)
pub fn router_cfg(cfg: &Config, res: i32) -> RouterCfg {
    let zones = cfg.click_zones && cfg.right_click != "tap";
    RouterCfg {
        hold_ms: cfg.hold_ms,
        strip_mm: cfg.strip_mm,
        key_gap_mm: cfg.key_gap_mm,
        click_zone_mm: if zones { cfg.click_zone_mm } else { 0.0 },
        five_finger: cfg.five_finger_release,
        pad_units_per_mm: res as f32 / cfg.speed.clamp(0.2, 5.0),
        palm_major: cfg.palm_major,
        options: cfg.keyboard_options.iter().cloned().collect(),
        sticky: cfg.sticky_modifiers,
        long_ms: cfg.long_press_ms,
        palm_keyboard: cfg.palm_keyboard,
        rest_zone_mm: cfg.rest_zone_mm,
        rest_zones: cfg.rest_zones.iter().map(|r| Rect::new(r[0], r[1], r[2], r[3])).collect(),
        dwt_ms: cfg.dwt_ms,
        pen_pause: cfg.pen_pause,
        quick: cfg.quick_actions.enabled,
        ..RouterCfg::default()
    }
}

struct D {
    sh: SharedRef,
    rx: Receiver<Cmd>,
    wake_rd: i32,
    cfg: Config,
    opts: Opts,
    cfg_dir: PathBuf,
    state_file: PathBuf,
    persist: Persist,
    paths: Vec<PathBuf>,
    settings_path: PathBuf,
    settings: toml::Table,
    layouts: Vec<Layout>,
    machine: Machine,
    router: Router,
    mt: Mt,
    touch_path: Option<String>,
    nslots: usize,
    res: i32,
    touch: Option<Dev>,
    grabbed: bool,
    pad: Option<VirtualPad>,
    mouse: Option<(VirtualMouse, Mouse)>,
    guard: Option<Guardian>,
    kbd_local: Option<VirtualKbd>,
    kbd_on: bool,
    chord: Chord,
    key_devs: Vec<Dev>,
    pen_dev: Option<Dev>,
    netlink: i32,
    real_conn: Option<Connector>,
    sim: bool,
    pending_plug: Option<i64>,
    await_output: bool,
    monitor_id: Option<String>,
    outputs: Vec<String>,
    mapped: UiKind,
    published: (UiKind, u64),
    model_dirty: bool,
    reason: String,
    notify: Notify,
    last_ping: i64,
    last_state: String,
    status: String,
    dbus_conn: Option<zbus::blocking::Connection>,
    wait_logged: bool,
    quit: Option<i32>,
    // M2
    worker: Worker,
    quick: Vec<quick::Item>,
    hidden: HashSet<String>,
    phys_kbd: Option<String>,
    pad_only_active: bool,
    chosen_layout: String,
    input_rescan_at: Option<i64>,
    info_next: i64,
    dimmed: bool,
    xkb: (u32, Vec<String>),
    leds: (bool, bool),
    theme_auto: String,
    guard_respawn_at: Option<i64>,
}

pub fn run(opts: Opts) -> i32 {
    install_signals();
    let (wake_rd, wake_wr) = wake_pipe();
    WAKE_FD.store(wake_wr, Ordering::SeqCst);
    let (ui_rd, ui_wr) = wake_pipe();
    println!("DEXHID daemon {} pid={} t={} debug={} (GPL-3.0-or-later PROVISIONAL)", env!("CARGO_PKG_VERSION"), std::process::id(), mono_ms(), opts.debug);

    let cfg_home = env_dir("XDG_CONFIG_HOME", ".config");
    let state_home = env_dir("XDG_STATE_HOME", ".local/state");
    let data_home = env_dir("XDG_DATA_HOME", ".local/share");
    let compat: Vec<String> = std::fs::read("/proc/device-tree/compatible")
        .map(|b| b.split(|&c| c == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect())
        .unwrap_or_default();
    let mut paths = config::default_paths(&cfg_home, &compat);
    if let Some(c) = &opts.config {
        paths.push(c.clone());
    }
    let settings_path = config::settings_path(&cfg_home);
    let settings = match config::load_settings(&settings_path) {
        Ok(t) => t,
        Err(e) => {
            println!("WARN settings {} (ignored)", e);
            toml::Table::new()
        }
    };
    let (cfg, notes) = config::load_with(&paths, &settings, opts.config.is_some());
    for n in notes {
        println!("CONFIG {}", n);
    }
    // touchscreen
    let touch_path = opts.touch_dev.clone().or(if cfg.touch_dev.is_empty() { None } else { Some(cfg.touch_dev.clone()) }).or_else(|| find_dev(&cfg.touchscreen));
    let (mut tmax, mut nslots, mut res) = ((33919, 23999), 10usize, 124);
    match &touch_path {
        Some(p) => match Dev::open(p, false) {
            Ok(d) => {
                if let (Some(x), Some(y)) = (d.absinfo(ABS_MT_POSITION_X), d.absinfo(ABS_MT_POSITION_Y)) {
                    tmax = (x.maximum, y.maximum);
                    if x.resolution > 0 {
                        res = x.resolution;
                    }
                }
                if let Some(s) = d.absinfo(ABS_MT_SLOT) {
                    nslots = (s.maximum + 1).max(1) as usize;
                }
                println!("TOUCH dev={} name=\"{}\" x=0..{} y=0..{} res={} slots={}", p, d.name(), tmax.0, tmax.1, res, nslots);
            }
            Err(e) => println!("ERROR touchscreen {}: {} (install 70-dexhid.rules for uaccess)", p, strerr(e)),
        },
        None => println!("WARN touchscreen \"{}\" not found; pad mode cannot start", cfg.touchscreen),
    }
    let mut ldirs = vec![PathBuf::from("/usr/share/dexhid/layouts")];
    ldirs.push(data_home.join("dexhid/layouts"));
    let (layouts, lerrs) = layout::load_all(&ldirs);
    for e in lerrs {
        println!("WARN layout {}", e);
    }
    println!("LAYOUTS {}", layouts.iter().map(|l| format!("{}({})", l.name, l.source)).collect::<Vec<_>>().join(" "));
    let state_file = state_home.join("dexhid/state.toml");
    let persist = Persist::load(&state_file);
    let kb = persist.keyboard.clone().unwrap_or(cfg.keyboard.clone());
    let layout = layouts.iter().find(|l| l.name == kb).or_else(|| layouts.iter().find(|l| l.name == "full")).cloned().unwrap_or_else(Layout::none);
    let chosen_layout = layout.name.clone();
    let autostart = persist.autostart.as_deref().and_then(Autostart::parse).or(Autostart::parse(&cfg.autostart)).unwrap_or(Autostart::Ask);
    let router = Router::new(router_cfg(&cfg, res), layout);
    let chord_keys: Vec<u16> = cfg.release_chord.iter().filter_map(|k| keycodes::code(k)).collect();
    let chord = Chord::new(chord_keys.clone(), cfg.release_hold_ms);

    let (tx, rx) = std::sync::mpsc::channel();
    let sh: SharedRef = Arc::new(Shared {
        tx,
        wake_core: wake_wr,
        wake_ui: ui_wr,
        ui: std::sync::Mutex::new((UiKind::None, Default::default())),
        ui_ver: Default::default(),
        ui_heartbeat: std::sync::atomic::AtomicI64::new(mono_ms()),
        status: std::sync::Mutex::new("{}".into()),
        layout_names: std::sync::Mutex::new(layouts.iter().map(|l| l.name.clone()).collect()),
        debug: opts.debug,
        ui_cfg: UiCfg {
            internal_output: cfg.internal_output.clone(),
            touch_transform: cfg.touch_transform.clone(),
            size_mm: (cfg.size_mm[0], cfg.size_mm[1]),
            tmax,
            xkb_layout_index: cfg.xkb_layout_index,
            verbose: opts.debug,
        },
        xkb_group: std::sync::atomic::AtomicU32::new(cfg.xkb_layout_index),
    });
    let dbus_conn = match dbus::serve(sh.clone()) {
        Ok(c) => {
            println!("DBUS name={} path={}", dbus::NAME, dbus::PATH);
            Some(c)
        }
        Err(e) if e.starts_with("TAKEN") => {
            println!("ERROR {}: another dexhid daemon is running", e);
            return 2;
        }
        Err(e) => {
            println!("WARN no D-Bus API ({}): CLI unavailable", e);
            None
        }
    };
    // a brightness left dimmed by a crashed run comes back first
    if desktop::backlight(&cfg.backlight).is_some() {
        let r = desktop::restore();
        if !r.contains("nothing") {
            println!("{} (left over)", r);
        }
    }
    {
        let s = sh.clone();
        std::thread::Builder::new().name("ui".into()).spawn(move || crate::ui::run(s, ui_rd)).expect("ui thread");
        let s = sh.clone();
        std::thread::Builder::new().name("logind".into()).spawn(move || crate::logind::run(s)).expect("logind thread");
    }
    if cfg.track_layout {
        desktop::kxkb_start(sh.clone());
    }
    let worker = Worker::start(sh.clone());
    let netlink = match notify::uevent_socket() {
        Ok(fd) => fd,
        Err(e) => {
            println!("WARN hotplug: {} (no monitor detection)", e);
            -1
        }
    };
    let mut key_devs = vec![];
    for p in event_nodes() {
        if Some(&p) == touch_path.as_ref() || Some(&p) == opts.chord_test_dev.as_ref() {
            continue;
        }
        let Ok(d) = Dev::open(&p, false) else { continue };
        if d.name().starts_with("dexhid") {
            continue;
        }
        if chord_keys.iter().any(|&k| d.has_key(k)) {
            println!("CHORD-WATCH node={} name=\"{}\" grab=no", p, d.name());
            key_devs.push(d);
        }
    }
    if let Some(p) = &opts.chord_test_dev {
        if !opts.debug {
            println!("ERROR --chord-test-dev needs --debug");
            return 2;
        }
        match Dev::open(p, false) {
            Ok(d) => {
                let g = d.grab(true).is_ok();
                println!("CHORD-WATCH node={} name=\"{}\" grab={} (test source)", p, d.name(), g);
                key_devs.push(d);
            }
            Err(e) => println!("WARN chord test dev {}: {}", p, strerr(e)),
        }
    }
    println!("CHORD keys={:?} hold_ms={} sources={}", chord_keys.iter().map(|&k| keycodes::name(k)).collect::<Vec<_>>(), cfg.release_hold_ms, key_devs.len());
    let guard = match Guardian::spawn(3000) {
        Ok(g) => {
            println!("GUARDIAN-SPAWNED pid={}", g.pid());
            Some(g)
        }
        Err(e) => {
            println!("WARN key guardian: {} (the keyboard stays in the daemon: a crash may leave keys down)", e);
            None
        }
    };
    let mut hidden = HashSet::new();
    match desktop::vibrator() {
        Some(v) => println!("HAPTICS device={} (playback planned M3)", v),
        None => {
            hidden.insert("haptic_feedback".to_string());
            println!("HAPTICS none: the vibration setting is hidden");
        }
    }

    let notify = Notify::from_env();
    let mut d = D {
        sh,
        rx,
        wake_rd,
        machine: Machine::new(autostart, cfg.display_pill),
        cfg_dir: cfg_home.join("dexhid"),
        state_file,
        persist,
        paths,
        settings_path,
        settings,
        layouts,
        router,
        mt: Mt::new(nslots),
        touch_path,
        nslots,
        res,
        touch: None,
        grabbed: false,
        pad: None,
        mouse: None,
        guard,
        kbd_local: None,
        kbd_on: false,
        chord,
        key_devs,
        pen_dev: None,
        netlink,
        real_conn: None,
        sim: false,
        pending_plug: None,
        await_output: false,
        monitor_id: None,
        outputs: vec![],
        mapped: UiKind::None,
        published: (UiKind::None, 0),
        model_dirty: true,
        reason: "start".into(),
        notify,
        last_ping: 0,
        last_state: String::new(),
        status: String::new(),
        dbus_conn,
        wait_logged: false,
        quit: None,
        worker,
        quick: vec![],
        hidden,
        phys_kbd: None,
        pad_only_active: false,
        chosen_layout,
        input_rescan_at: None,
        info_next: 0,
        dimmed: false,
        xkb: (0, vec![]),
        leds: (false, false),
        theme_auto: "dark".into(),
        guard_respawn_at: None,
        cfg,
        opts,
    };
    println!("CONFIG autostart={} keyboard={} internal_output={} connectors={:?} watchdog_ms={:?} mode={} quick={}", d.machine.autostart.name(), d.router.layout().name, d.cfg.internal_output, d.cfg.external_connectors, d.notify.watchdog_ms, d.cfg.mode, d.cfg.quick_actions.enabled);
    d.apply_config(None);
    d.scan_inputs();
    d.rescan(true);
    if d.opts.simulate_connect && d.opts.debug {
        d.cmd(Cmd::DebugConnector(true));
    }
    d.notify.send("READY=1");
    d.run_loop()
}

impl D {
    fn has_external_output(&self) -> bool {
        self.outputs.iter().any(|n| n != &self.cfg.internal_output && !n.starts_with("DSI") && !n.starts_with("eDP") && !n.starts_with("LVDS"))
    }

    fn handle(&mut self, ev: Ev, reason: &str) {
        self.reason = reason.to_string();
        if let Some(r) = self.machine.handle(&ev) {
            if let Some(id) = &self.monitor_id {
                self.persist.monitors.insert(id.clone(), if r == Remember::Pad { "pad".into() } else { "display".into() });
                match self.persist.save(&self.state_file) {
                    Ok(()) => println!("REMEMBER monitor={}.. choice={:?}", &id[..id.len().min(6)], r),
                    Err(e) => println!("WARN state file: {}", e),
                }
            }
        }
        // release / grab at once (unplug, release, chord ...): no detour through the rest of the loop
        if !self.machine.effective().grab {
            self.release();
        }
    }

    fn connect(&mut self, id: String, label: &str) {
        let rem = self.persist.monitors.get(&id).map(|v| if v == "pad" { Remember::Pad } else { Remember::Display });
        println!("PLUG t={} connected={} remembered={:?}", mono_ms(), label, rem);
        self.monitor_id = Some(id);
        self.handle(Ev::Connected(rem), "plug");
        self.info_next = 0;
    }

    fn rescan(&mut self, startup: bool) {
        let conns = hotplug::scan(std::path::Path::new("/sys/class/drm"));
        let ext = hotplug::external(&conns, &self.cfg.external_connectors).cloned();
        match (ext, self.real_conn.is_some()) {
            (Some(c), false) => {
                if self.pending_plug.is_none() && !self.await_output {
                    let at = if startup { mono_ms() } else { mono_ms() + self.cfg.plug_debounce_ms as i64 };
                    println!("PLUG-SEEN t={} connector={} enabled={} (debounce until {})", mono_ms(), c.name, c.enabled, at);
                    self.pending_plug = Some(at);
                }
            }
            (None, true) => {
                let n = self.real_conn.take().map(|c| c.name).unwrap_or_default();
                println!("UNPLUG t={} connector={}", mono_ms(), n);
                if !self.sim {
                    self.monitor_id = None;
                    self.handle(Ev::Disconnected, "unplug");
                }
            }
            (None, false) => {
                if self.pending_plug.take().is_some() || self.await_output {
                    println!("PLUG-FLAP t={} (gone during the debounce)", mono_ms());
                }
                self.await_output = false;
            }
            (Some(_), true) => {}
        }
    }

    fn plug_due(&mut self) {
        self.pending_plug = None;
        let conns = hotplug::scan(std::path::Path::new("/sys/class/drm"));
        let Some(c) = hotplug::external(&conns, &self.cfg.external_connectors).cloned() else { return };
        if self.cfg.require_compositor_output && !self.has_external_output() {
            if !self.await_output {
                println!("PLUG-WAIT connector={} : no non-internal compositor output yet (mirror / disabled monitor?)", c.name);
            }
            self.await_output = true;
            return;
        }
        self.await_output = false;
        let id = c.edid_hash.clone().unwrap_or_else(|| c.name.clone());
        let name = c.name.clone();
        self.real_conn = Some(c);
        if !self.sim {
            self.connect(id, &name);
        }
    }

    // ---------------------------------------------------------------- settings

    fn cfg_value(&self) -> toml::Value {
        toml::Value::try_from(&self.cfg).unwrap_or(toml::Value::Table(toml::Table::new()))
    }

    fn build_sheet(&self) -> Vec<SheetModule> {
        let v = self.cfg_value();
        settings::MODULES
            .iter()
            .map(|m| {
                let mut rows = settings::rows(m.id, &v, &self.hidden);
                for r in rows.iter_mut() {
                    if r.key == "act.layout" {
                        r.value = self.router.layout().title.clone();
                    }
                    if r.key == "autostart" {
                        r.value = self.machine.autostart.name().into();
                    }
                }
                if m.id == "quick" {
                    for (id, label) in quick::all_ids(&self.cfg.quick_actions) {
                        let s = quick::size_of(&self.cfg.quick_actions, &id);
                        rows.push(settings::Row {
                            key: format!("quick:{}", id),
                            title: label,
                            value: if s == 0 { "off".into() } else if s == 2 { "large".into() } else { "on".into() },
                            kind: Kind::Choice(&["off", "on", "large"]),
                            live: true,
                        });
                    }
                }
                SheetModule { id: m.id.into(), title: m.title.into(), live: m.implemented, plan: m.plan.into(), rows }
            })
            .collect()
    }

    /// change one setting (sheet step or D-Bus); returns the new display value
    fn set_setting(&mut self, key: &str, value: Option<&str>, dir: i32) -> Result<String, String> {
        let mut t = self.settings.clone();
        let cur = self.cfg_value();
        let shown: String;
        if let Some(id) = key.strip_prefix("quick.up:") {
            let a = quick::move_up(&self.cfg.quick_actions.actions, id);
            settings::set(&mut t, "quick_actions.actions", toml::Value::Array(a.into_iter().map(toml::Value::String).collect()));
            shown = "moved".into();
        } else if let Some(id) = key.strip_prefix("quick:") {
            if !quick::all_ids(&self.cfg.quick_actions).iter().any(|(i, _)| i == id) {
                return Err(format!("unknown quick action {}", id));
            }
            let a = match value {
                Some("off") => self.cfg.quick_actions.actions.iter().filter(|e| e.split(':').next() != Some(id)).cloned().collect(),
                Some(_) | None => quick::cycle(&self.cfg.quick_actions.actions, id),
            };
            shown = format!("{}", quick::size_of(&dexhid_core::config::QuickCfg { actions: a.clone(), ..Default::default() }, id));
            settings::set(&mut t, "quick_actions.actions", toml::Value::Array(a.into_iter().map(toml::Value::String).collect()));
        } else {
            let d = settings::def(key).ok_or_else(|| format!("unknown setting {}", key))?;
            let nv = match value {
                Some(s) => settings::parse(d, s)?,
                None => settings::step(d, settings::value_of(&cur, key).as_ref(), dir).ok_or("not a value setting")?,
            };
            if let Some(o) = key.strip_prefix("opt.") {
                let mut opts: Vec<String> = self.cfg.keyboard_options.iter().filter(|x| x.as_str() != o).cloned().collect();
                if nv.as_bool() == Some(true) {
                    opts.push(o.to_string());
                }
                settings::set(&mut t, "keyboard_options", toml::Value::Array(opts.into_iter().map(toml::Value::String).collect()));
            } else {
                settings::set(&mut t, key, nv.clone());
            }
            if key == "autostart" {
                if let Some(a) = nv.as_str().and_then(Autostart::parse) {
                    self.machine.autostart = a;
                    self.persist.autostart = Some(a.name().into());
                    let _ = self.persist.save(&self.state_file);
                }
            }
            shown = settings::display(d, Some(&nv));
        }
        let (c, notes) = config::load_with(&self.paths, &t, self.opts.config.is_some());
        if let Some(e) = notes.iter().find(|n| n.starts_with("ERROR")) {
            return Err(e.clone());
        }
        config::save_settings(&self.settings_path, &t)?;
        self.settings = t;
        let old = std::mem::replace(&mut self.cfg, c);
        println!("SETTING t={} key={} value={}", mono_ms(), key, shown);
        self.apply_config(Some(old));
        Ok(shown)
    }

    /// push the (changed) config into the router, the devices and the desktop
    fn apply_config(&mut self, old: Option<Config>) {
        let o = self.router.set_cfg(router_cfg(&self.cfg, self.res));
        self.apply(o);
        let (items, notes) = quick::resolve(&self.cfg.quick_actions, &desktop::have_program);
        if old.as_ref().map(|o| o.quick_actions != self.cfg.quick_actions).unwrap_or(true) {
            for n in notes {
                println!("NOTE {}", n);
            }
            println!("QUICK-ITEMS {}", items.iter().map(|i| if i.size == 2 { format!("{}:2", i.id) } else { i.id.clone() }).collect::<Vec<_>>().join(","));
        }
        let o = self.router.set_quick(items.iter().map(|i| (i.id.clone(), i.label.clone(), i.size)).collect());
        self.quick = items;
        self.apply(o);
        let theme = if self.cfg.theme == "auto" { self.theme_auto.clone() } else { self.cfg.theme.clone() };
        if self.cfg.theme == "auto" && old.as_ref().is_none_or(|o| o.theme != "auto") {
            self.worker.send(Job::ThemeAuto);
        }
        let o = self.router.set_theme(&theme, &self.cfg.accent, self.cfg.key_alpha);
        self.apply(o);
        if !self.cfg.status_info {
            let o = self.router.set_info("");
            self.apply(o);
        }
        self.info_next = 0;
        let sheet = self.build_sheet();
        let o = self.router.set_sheet(sheet);
        self.apply(o);
        if let Some(old) = old {
            if old.mode != self.cfg.mode && (self.pad.is_some() || self.mouse.is_some()) {
                self.pad = None;
                self.mouse = None;
                self.create_pad();
            }
            if (old.natural_scroll, old.tap_to_click, old.pointer_accel) != (self.cfg.natural_scroll, self.cfg.tap_to_click, self.cfg.pointer_accel) {
                self.kwin_pad();
            }
            if self.grabbed && (old.dim, old.dim_percent) != (self.cfg.dim, self.cfg.dim_percent) {
                self.set_dim(self.cfg.dim);
            }
            if old.pad_only_with_physical_keyboard != self.cfg.pad_only_with_physical_keyboard {
                self.update_pad_only();
            }
        }
    }

    fn kwin_pad(&self) {
        if let Some(p) = &self.pad {
            let node = p.u.node.rsplit('/').next().unwrap_or_default().to_string();
            self.worker.send(Job::KwinPad { node, natural_scroll: self.cfg.natural_scroll, tap: self.cfg.tap_to_click, accel: self.cfg.pointer_accel as f64 });
        }
    }

    fn set_dim(&mut self, on: bool) {
        if on && self.cfg.dim {
            self.worker.send(Job::Dim { backlight: self.cfg.backlight.clone(), percent: self.cfg.dim_percent });
            self.dimmed = true;
        } else if self.dimmed {
            self.worker.send(Job::Restore);
            self.dimmed = false;
        }
    }

    // ---------------------------------------------------------------- inputs: physical keyboards, pen

    fn scan_inputs(&mut self) {
        // keyboards from sysfs (no access to their event nodes needed); the pen through its uaccess node
        let phys = feedback::sysfs_keyboards(std::path::Path::new("/sys/class/input")).into_iter().next().map(|k| k.0);
        let mut pen = None;
        if !self.cfg.pen.is_empty() && self.pen_dev.is_none() {
            pen = find_dev(&self.cfg.pen).and_then(|p| Dev::open(&p, false).ok());
        }
        if phys != self.phys_kbd {
            println!("PHYS-KEYBOARD t={} {}", mono_ms(), phys.as_deref().map(|n| format!("attached name=\"{}\"", n)).unwrap_or_else(|| "none".into()));
            self.phys_kbd = phys;
            self.update_pad_only();
        }
        if let Some(d) = pen {
            println!("PEN-WATCH node={} name=\"{}\" grab=no", d.path, d.name());
            self.pen_dev = Some(d);
        }
    }

    fn update_pad_only(&mut self) {
        let want = self.cfg.pad_only_with_physical_keyboard && self.phys_kbd.is_some();
        if want && !self.pad_only_active {
            self.pad_only_active = true;
            self.chosen_layout = self.router.layout().name.clone();
            println!("PAD-ONLY t={} on (keyboard \"{}\" attached)", mono_ms(), self.phys_kbd.clone().unwrap_or_default());
            self.set_layout_inner("none", false);
            let o = self.router.set_notice("touchpad only: a keyboard is attached");
            self.apply(o);
        } else if !want && self.pad_only_active {
            self.pad_only_active = false;
            println!("PAD-ONLY t={} off -> {}", mono_ms(), self.chosen_layout);
            let l = self.chosen_layout.clone();
            self.set_layout_inner(&l, false);
            let o = self.router.set_notice("");
            self.apply(o);
        }
    }

    // ---------------------------------------------------------------- keyboard (guardian)

    fn kb_create(&mut self) -> Result<String, String> {
        if let Some(g) = self.guard.as_mut() {
            let n = g.create_kbd()?;
            self.kbd_on = true;
            return Ok(n);
        }
        let k = VirtualKbd::create()?;
        let n = k.u.node.clone();
        self.kbd_local = Some(k);
        self.kbd_on = true;
        Ok(n)
    }
    fn kb_key(&mut self, c: u16, p: bool) {
        if !self.kbd_on {
            return;
        }
        if let Some(g) = self.guard.as_mut() {
            g.key(c, p);
        } else if let Some(k) = self.kbd_local.as_mut() {
            k.key(c, p);
        }
    }
    fn kb_release_all(&mut self) -> usize {
        if !self.kbd_on {
            return 0;
        }
        if let Some(g) = self.guard.as_mut() {
            g.release_all()
        } else {
            self.kbd_local.as_mut().map(|k| k.release_all()).unwrap_or(0)
        }
    }
    fn kb_destroy(&mut self) {
        if !self.kbd_on {
            return;
        }
        self.kbd_on = false;
        if let Some(g) = self.guard.as_mut() {
            g.destroy_kbd();
        }
        if let Some(k) = self.kbd_local.take() {
            destroy_later("keyboard", k);
        }
    }
    fn kb_node(&self) -> Option<String> {
        if !self.kbd_on {
            return None;
        }
        self.guard.as_ref().and_then(|g| g.node.clone()).or_else(|| self.kbd_local.as_ref().map(|k| k.u.node.clone()))
    }

    fn guard_msgs(&mut self) {
        let Some(g) = self.guard.as_mut() else { return };
        match g.read_msgs() {
            Ok(v) => {
                for m in v {
                    let w: Vec<&str> = m.split_whitespace().collect();
                    match w.first().copied() {
                        Some("L") => {
                            let leds = (w.get(1) == Some(&"1"), w.get(2) == Some(&"1"));
                            println!("LEDS t={} caps={} num={}", mono_ms(), leds.0, leds.1);
                            self.leds = leds;
                            let o = self.router.set_leds(leds.0, leds.1);
                            self.apply(o);
                        }
                        Some("H") => println!("WARN guardian released {} key(s) after a lost heartbeat", w.get(1).unwrap_or(&"?")),
                        _ => {}
                    }
                }
            }
            Err(()) => self.guardian_lost(),
        }
    }

    fn guardian_lost(&mut self) {
        let pid = self.guard.as_ref().map(|g| g.pid()).unwrap_or(0);
        println!("ERROR key guardian {} gone -> release; a new guardian starts", pid);
        drop(self.guard.take());
        self.kbd_on = false;
        if self.machine.effective().grab {
            self.handle(Ev::UiLost, "guardian-lost");
        }
        self.guard_respawn_at = Some(mono_ms() + 500);
    }

    // ---------------------------------------------------------------- commands

    fn cmd(&mut self, c: Cmd) {
        if matches!(c, Cmd::Toggle(_) | Cmd::Release(_) | Cmd::Resume(_) | Cmd::SetLayout(_)) {
            println!("CMD-RX t={} {:?}", mono_ms(), c);
        }
        match c {
            Cmd::Toggle(s) => self.handle(Ev::Toggle, &format!("toggle:{}", s)),
            Cmd::Release(s) => self.handle(Ev::Release, &format!("release:{}", s)),
            Cmd::Resume(s) => self.handle(Ev::Resume, &format!("resume:{}", s)),
            Cmd::SetLayout(n) => self.set_layout(&n),
            Cmd::DebugConnector(on) => {
                println!("DEBUG-CONNECTOR t={} connected={}", mono_ms(), on);
                if on && !self.sim {
                    self.sim = true;
                    self.connect("simulated".into(), "simulated");
                } else if !on && self.sim {
                    self.sim = false;
                    if self.real_conn.is_none() {
                        self.monitor_id = None;
                        self.handle(Ev::Disconnected, "unplug:simulated");
                    }
                }
            }
            Cmd::Prompt(c) => {
                println!("PROMPT-CHOICE t={} {:?}", mono_ms(), c);
                self.handle(Ev::Prompt(c), "prompt");
            }
            Cmd::PillUsePad => self.handle(Ev::Resume, "pill"),
            Cmd::PillHide => self.handle(Ev::Exit, "pill:hide"),
            Cmd::Geometry(g) => {
                let o = self.router.set_geometry(g);
                self.apply(o);
            }
            Cmd::Mapped(k) => {
                if k == self.published.0 {
                    self.mapped = k;
                }
            }
            Cmd::Outputs(v) => {
                println!("OUTPUTS {:?}", v);
                self.outputs = v;
                if self.await_output && self.has_external_output() {
                    self.plug_due();
                }
            }
            Cmd::UiLost(r) => {
                println!("ERROR UI lost: {} -> release, exit 1 (Restart=on-failure brings dexhid back)", r);
                self.handle(Ev::UiLost, "ui-lost");
                self.quit = Some(1);
            }
            Cmd::Labels(n) => println!("LABELS xkb keys={}", n),
            Cmd::Locked(b) if b != self.machine.locked => self.handle(Ev::Locked(b), if b { "lock" } else { "unlock" }),
            Cmd::SessionActive(b) if b == self.machine.inactive => self.handle(Ev::SessionActive(b), if b { "session-active" } else { "session-inactive" }),
            Cmd::Lid(b) if b != self.machine.lid_closed => self.handle(Ev::LidClosed(b), if b { "lid-closed" } else { "lid-open" }),
            Cmd::Locked(_) | Cmd::SessionActive(_) | Cmd::Lid(_) => {}
            Cmd::Sleep(b, ack) => {
                self.handle(Ev::Sleep(b), if b { "sleep" } else { "wake" });
                self.reconcile();
                if let Some(a) = ack {
                    let _ = a.send(());
                }
                if !b {
                    self.rescan(false);
                }
            }
            Cmd::Note(s) => println!("{}", if s.starts_with(char::is_uppercase) && s.split(' ').next().is_some_and(|w| w.chars().all(|c| c.is_ascii_uppercase() || c == '-')) { s } else { format!("NOTE {}", s) }),
            Cmd::SetSetting(k, v, reply) => {
                let r = self.set_setting(&k, if v.is_empty() { None } else { Some(&v) }, 1);
                if let Err(e) = &r {
                    println!("SETTING-REJECTED key={} value={} ({})", k, v, e);
                }
                let _ = reply.send(r);
            }
            Cmd::GetSettings(reply) => {
                let v = self.cfg_value();
                let mut m = serde_json::Map::new();
                for d in settings::DEFS.iter().filter(|d| d.kind != Kind::Action) {
                    m.insert(d.key.into(), serde_json::Value::String(settings::display(d, settings::value_of(&v, d.key).as_ref())));
                }
                m.insert("quick_actions.actions".into(), serde_json::json!(self.cfg.quick_actions.actions));
                let _ = reply.send(serde_json::Value::Object(m).to_string());
            }
            Cmd::RunQuick(id) => self.run_quick(&id),
            Cmd::XkbGroup(i, names) => {
                println!("XKB-GROUP t={} group={} name={:?} layouts={:?}", mono_ms(), i, names.get(i as usize).cloned().unwrap_or_default(), names);
                self.xkb = (i, names);
                self.sh.xkb_group.store(i, Ordering::SeqCst);
                poke(self.sh.wake_ui);
            }
            Cmd::ThemeAuto(t) => {
                println!("THEME auto -> {}", t);
                self.theme_auto = t;
                if self.cfg.theme == "auto" {
                    let o = self.router.set_theme(&self.theme_auto.clone(), &self.cfg.accent.clone(), self.cfg.key_alpha);
                    self.apply(o);
                }
            }
        }
    }

    fn run_quick(&mut self, id: &str) {
        let Some(it) = self.quick.iter().find(|q| q.id == id).cloned() else {
            println!("WARN quick action {} not shown / unknown", id);
            return;
        };
        match &it.act {
            Act::Chord(codes) => {
                println!("QUICK t={} id={} chord={}", mono_ms(), id, codes.iter().map(|c| keycodes::name(*c)).collect::<Vec<_>>().join("+"));
                for (c, p) in quick::chord_events(codes) {
                    self.kb_key(c, p);
                }
            }
            Act::Command(argv) => {
                println!("QUICK t={} id={} command={}", mono_ms(), id, argv[0]);
                match std::process::Command::new(&argv[0]).args(&argv[1..]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
                    Ok(mut ch) => {
                        std::thread::spawn(move || {
                            let _ = ch.wait();
                        });
                    }
                    Err(e) => println!("WARN quick action {}: {}", id, e),
                }
            }
        }
    }

    fn arr_path(&self, layout: &str, o: Orientation) -> PathBuf {
        self.cfg_dir.join(arrangement::file_name(layout, o))
    }

    fn load_arr(&self, layout: &str, o: Orientation) -> Option<Arrangement> {
        let p = self.arr_path(layout, o);
        match Arrangement::load(&p) {
            Ok(a) => {
                if a.is_some() {
                    println!("ARRANGEMENT loaded path={}", p.display());
                }
                a
            }
            Err(e) => {
                println!("WARN arrangement {} (defaults used)", e);
                None
            }
        }
    }

    fn set_layout(&mut self, name: &str) {
        if self.pad_only_active {
            // remembered for when the keyboard goes away
            if self.layouts.iter().any(|l| l.name == name) {
                self.chosen_layout = name.to_string();
                println!("LAYOUT-DEFERRED name={} (touchpad only while a keyboard is attached)", name);
            }
            return;
        }
        self.set_layout_inner(name, true);
    }

    fn set_layout_inner(&mut self, name: &str, persist: bool) {
        let Some(l) = self.layouts.iter().find(|l| l.name == name).cloned() else {
            println!("ERROR unknown layout {:?} (have: {})", name, self.layouts.iter().map(|l| l.name.as_str()).collect::<Vec<_>>().join(", "));
            return;
        };
        let arr = self.router.orientation().and_then(|o| self.load_arr(&l.name, o));
        println!("LAYOUT t={} name={} title=\"{}\" source={}", mono_ms(), l.name, l.title, l.source);
        let o = self.router.set_layout(l, arr);
        self.apply(o);
        if persist {
            self.chosen_layout = name.to_string();
            self.persist.keyboard = Some(name.to_string());
            if let Err(e) = self.persist.save(&self.state_file) {
                println!("WARN state file: {}", e);
            }
        }
        let s = self.build_sheet();
        let o = self.router.set_sheet(s);
        self.apply(o);
    }

    fn apply(&mut self, outs: Vec<Out>) {
        for o in outs {
            match o {
                Out::Key(c, p) => {
                    self.kb_key(c, p);
                    if self.opts.debug {
                        println!("KEY t={} {}={}", mono_ms(), keycodes::name(c), p as i32);
                    }
                }
                Out::KeyFeedback => {
                    if self.cfg.sound_feedback {
                        self.worker.send(Job::Click);
                    }
                }
                Out::Pad(f) => {
                    if let Some(p) = self.pad.as_mut() {
                        p.apply(&f);
                    } else if let Some((dev, m)) = self.mouse.as_mut() {
                        let mut o = m.feed(&f.slots, mono_ms());
                        o.extend(f.buttons.iter().map(|&(b, p)| dexhid_core::mouse::MOut::Button(b, p)));
                        dev.apply(&o);
                    }
                }
                Out::UseAsDisplay => self.handle(Ev::UseAsDisplay, "button:display"),
                Out::Exit => self.handle(Ev::Exit, "button:exit"),
                Out::FiveFinger => {
                    println!("FIVE-FINGER t={} hold -> release", mono_ms());
                    self.handle(Ev::FiveFinger, "five-finger");
                }
                Out::LayoutNext => {
                    let cur = self.router.layout().name.clone();
                    let i = self.layouts.iter().position(|l| l.name == cur).unwrap_or(0);
                    let next = self.layouts[(i + 1) % self.layouts.len()].name.clone();
                    self.set_layout(&next);
                }
                Out::SetLayout(n) => self.set_layout(&n),
                Out::Quick(id) => self.run_quick(&id),
                Out::SettingStep(k, dir) => {
                    if let Err(e) = self.set_setting(&k, None, dir) {
                        println!("SETTING-REJECTED key={} ({})", k, e);
                    }
                }
                Out::SaveArrangement => {
                    let a = self.router.arrangement();
                    let o = self.router.orientation().unwrap_or(Orientation::Landscape);
                    let p = self.arr_path(&a.layout, o);
                    match a.save(&p) {
                        Ok(()) => {
                            let tp = a.get(arrangement::TOUCHPAD).unwrap_or_default();
                            println!("ARRANGEMENT saved path={} touchpad={:.4},{:.4},{:.4},{:.4}", p.display(), tp.x, tp.y, tp.w, tp.h);
                        }
                        Err(e) => println!("ERROR arrangement save: {}", e),
                    }
                }
                Out::ResetArrangement => {
                    let layout = self.router.layout().name.clone();
                    let o = self.router.orientation().unwrap_or(Orientation::Landscape);
                    let p = self.arr_path(&layout, o);
                    let _ = std::fs::remove_file(&p);
                    println!("ARRANGEMENT reset path={}", p.display());
                }
                Out::PadChanged => {
                    if self.pad.is_some() || self.mouse.is_some() {
                        if let Some(p) = self.pad.take() {
                            destroy_later("touchpad", p);
                        }
                        if let Some((m, _)) = self.mouse.take() {
                            destroy_later("mouse", m);
                        }
                        self.create_pad();
                    }
                }
                Out::NeedArrangement(o) => {
                    let name = self.router.layout().name.clone();
                    let a = self.load_arr(&name, o);
                    println!("ORIENTATION {}", o.name());
                    let outs = self.router.set_arrangement(a);
                    self.apply(outs);
                }
                Out::EditMode(b) => println!("EDIT t={} on={}", mono_ms(), b),
                Out::PanelOpen(b) => println!("PANEL t={} open={}", mono_ms(), b),
                Out::Static | Out::Dynamic => self.model_dirty = true,
                Out::Note(s) => println!("NOTE {}", s),
            }
        }
    }

    fn create_pad(&mut self) {
        let Some(spec) = self.router.pad_spec() else { return };
        if self.cfg.mode == "mouse" {
            match VirtualMouse::create() {
                Ok(m) => {
                    println!("UINPUT mouse node={} (mode=mouse)", m.u.node);
                    let mc = MouseCfg { units_per_mm: spec.2 as f32, px_per_mm: 9.0, natural_scroll: self.cfg.natural_scroll, ..Default::default() };
                    self.mouse = Some((m, Mouse::new(mc)));
                }
                Err(e) => {
                    println!("ERROR uinput mouse: {}", e);
                    self.handle(Ev::UiLost, "no-uinput");
                }
            }
            return;
        }
        match VirtualPad::create(spec) {
            Ok(p) => {
                println!("UINPUT pad node={} xmax={} ymax={} res={}", p.u.node, spec.0, spec.1, spec.2);
                self.pad = Some(p);
                self.kwin_pad();
            }
            Err(e) => {
                println!("ERROR uinput touchpad: {} (install 70-dexhid.rules for /dev/uinput)", e);
                self.handle(Ev::UiLost, "no-uinput");
            }
        }
    }

    /// The release. Critical path (timed, logged in RELEASED): keys up through the guardian (confirmed), pad
    /// contacts lifted, ungrab. Then, off the critical path: the devices are destroyed by a reaper thread and the
    /// guardian, the brightness restored by the desktop worker.
    fn release(&mut self) {
        if !self.grabbed && self.pad.is_none() && self.mouse.is_none() && !self.kbd_on && self.touch.is_none() {
            return;
        }
        let t0 = mono_ns();
        let was = self.grabbed;
        let o = self.router.reset_contacts();
        let outs: Vec<Out> = o.into_iter().filter(|x| matches!(x, Out::Key(..) | Out::Pad(..))).collect();
        self.apply(outs);
        if let Some((dev, m)) = self.mouse.as_mut() {
            let o = m.release_all();
            dev.apply(&o);
            dev.release_all();
        }
        let extra = self.kb_release_all();
        let t_keys = mono_ns();
        let mut slots = None;
        if let Some(t) = &self.touch {
            slots = t.active_slots();
            if self.grabbed {
                let _ = t.grab(false);
            }
        }
        let t_ungrab = mono_ns();
        self.grabbed = false;
        self.touch = None;
        self.wait_logged = false;
        println!(
            "RELEASED t={} reason={} grabbed={} active_slots={:?} extra_keys={} order=keys,contacts,ungrab,destroy keys_ms={:.1} ungrab_ms={:.1}",
            mono_ms(),
            self.reason,
            was,
            slots,
            extra,
            (t_keys - t0) as f64 / 1e6,
            (t_ungrab - t_keys) as f64 / 1e6
        );
        if let Some(p) = self.pad.take() {
            destroy_later("touchpad", p);
        }
        if let Some((m, _)) = self.mouse.take() {
            destroy_later("mouse", m);
        }
        self.kb_destroy();
        self.set_dim(false);
    }

    /// make the devices / grab / UI match the state machine
    fn reconcile(&mut self) {
        let e = self.machine.effective();
        if e.grab {
            if self.touch.is_none() {
                let Some(p) = self.touch_path.clone() else {
                    println!("ERROR no touchscreen: pad mode impossible");
                    self.handle(Ev::UiLost, "no-touchscreen");
                    return self.reconcile();
                };
                match Dev::open(&p, false) {
                    Ok(d) => {
                        d.set_clock(libc::CLOCK_MONOTONIC);
                        self.touch = Some(d);
                    }
                    Err(err) => {
                        println!("ERROR no access to the touchscreen {}: {} (install 70-dexhid.rules)", p, strerr(err));
                        self.handle(Ev::UiLost, "no-access");
                        return self.reconcile();
                    }
                }
            }
            if !self.kbd_on {
                match self.kb_create() {
                    Ok(n) => println!("UINPUT kbd node={} owner={}", n, if self.guard.is_some() { "guardian" } else { "daemon" }),
                    Err(err) => {
                        println!("ERROR uinput keyboard: {} (install 70-dexhid.rules for /dev/uinput)", err);
                        self.handle(Ev::UiLost, "no-uinput");
                        return self.reconcile();
                    }
                }
            }
            if self.pad.is_none() && self.mouse.is_none() && self.router.ready() {
                self.create_pad();
                if self.pad.is_none() && self.mouse.is_none() {
                    return self.reconcile();
                }
            }
            if !self.grabbed && self.mapped == UiKind::Overlay && self.router.ready() && (self.pad.is_some() || self.mouse.is_some()) {
                let t = self.touch.as_ref().unwrap();
                let active = t.active_slots().unwrap_or(0);
                let tpath = t.path.clone();
                if active == 0 {
                    let g = t.grab(true);
                    let n = self.nslots;
                    let tids = t.mt_slots(ABS_MT_TRACKING_ID, n);
                    let xs = t.mt_slots(ABS_MT_POSITION_X, n).unwrap_or_default();
                    let ys = t.mt_slots(ABS_MT_POSITION_Y, n).unwrap_or_default();
                    let mj = t.mt_slots(ABS_MT_TOUCH_MAJOR, n).unwrap_or_default();
                    let cur = t.absinfo(ABS_MT_SLOT).map(|a| a.value).unwrap_or(0);
                    match g {
                        Ok(()) => {
                            self.grabbed = true;
                            self.mt = Mt::new(self.nslots);
                            if let Some(tids) = tids {
                                // M2: positions + current slot too (the kernel drops repeated ABS values)
                                self.mt.sync_full(&tids, &xs, &ys, &mj, cur);
                            }
                            let o = self.router.reset_contacts();
                            self.apply(o);
                            println!("GRABBED t={} dev={} reason={} slot={}", mono_ms(), tpath, self.reason, cur);
                            self.print_geom();
                            self.set_dim(true);
                        }
                        Err(err) => {
                            println!("ERROR EVIOCGRAB {}: {}", tpath, strerr(err));
                            self.handle(Ev::UiLost, "grab-failed");
                            return self.reconcile();
                        }
                    }
                } else if !self.wait_logged {
                    self.wait_logged = true;
                    println!("WAIT-LIFT t={} active_contacts={} (grab only with no finger down)", mono_ms(), active);
                }
            }
        } else {
            self.release();
        }
        let model_ver = self.router.model().version;
        if self.published.0 != e.ui || self.model_dirty {
            if self.published.0 != e.ui {
                self.mapped = UiKind::None;
            }
            if self.model_dirty && model_ver != self.published.1 && e.ui == UiKind::Overlay {
                self.print_geom();
            }
            self.published = (e.ui, model_ver);
            self.model_dirty = false;
            self.sh.publish_ui(e.ui, self.router.model());
        }
    }

    fn print_geom(&self) {
        let v = self.router.model().version;
        println!("GEOM-BEGIN v={} layout={} edit={}", v, self.router.layout().name, self.router.editing());
        for l in self.router.geom_lines() {
            println!("{}", l);
        }
        println!("GEOM-END v={}", v);
    }

    fn update_status(&mut self) {
        let e = self.machine.effective();
        let st = format!(
            "desired={} grab={} grabbed={} ui={} mapped={} paused={:?}",
            self.machine.desired.name(),
            e.grab,
            self.grabbed,
            e.ui.name(),
            self.mapped.name(),
            self.machine.pause_reasons()
        );
        if st != self.last_state {
            println!("STATE t={} {} reason={}", mono_ms(), st, self.reason);
            self.last_state = st;
            if let Some(c) = &self.dbus_conn {
                dbus::emit_state(c, self.machine.desired.name());
            }
            self.notify.send(&format!("STATUS={}", self.machine.desired.name()));
        }
        let o = self.router.orientation().map(|o| o.name()).unwrap_or("unknown");
        let s = serde_json::json!({
            "state": self.machine.desired.name(),
            "grab": e.grab,
            "grabbed": self.grabbed,
            "ui": e.ui.name(),
            "mapped": self.mapped.name(),
            "connected": self.machine.connected,
            "simulated": self.sim,
            "connector": self.real_conn.as_ref().map(|c| c.name.clone()),
            "paused": self.machine.pause_reasons(),
            "autostart": self.machine.autostart.name(),
            "layout": self.router.layout().name,
            "layouts": self.layouts.iter().map(|l| l.name.clone()).collect::<Vec<_>>(),
            "orientation": o,
            "edit": self.router.editing(),
            "settings_open": self.router.sheet_open(),
            "touch_dev": self.touch_path,
            "pad_node": self.pad.as_ref().map(|p| p.u.node.clone()),
            "mouse_node": self.mouse.as_ref().map(|m| m.0.u.node.clone()),
            "kbd_node": self.kb_node(),
            "guardian_pid": self.guard.as_ref().map(|g| g.pid()),
            "held_keys": self.router.held_codes().iter().map(|&c| keycodes::name(c)).collect::<Vec<_>>(),
            "last_reason": self.reason,
            "outputs": self.outputs,
            "mode": self.cfg.mode,
            "leds": {"caps": self.leds.0, "num": self.leds.1},
            "xkb_group": self.xkb.0,
            "xkb_layout": self.xkb.1.get(self.xkb.0 as usize),
            "phys_keyboard": self.phys_kbd,
            "pad_only": self.pad_only_active,
            "quick": self.quick.iter().map(|q| q.id.clone()).collect::<Vec<_>>(),
            "dimmed": self.dimmed,
            "theme": self.router.model().theme.0,
        })
        .to_string();
        if s != self.status {
            self.status = s.clone();
            *self.sh.status.lock().unwrap() = s;
        }
    }

    fn run_loop(&mut self) -> i32 {
        let mut evs: Vec<InputEvent> = Vec::with_capacity(256);
        loop {
            if stopped() {
                self.reason = stop_reason();
                self.release();
                self.notify.send("STOPPING=1");
                println!("EXIT t={} reason={}", mono_ms(), self.reason);
                // the restore runs in the worker: give it a moment (ExecStopPost=dexhid --restore covers the rest)
                if self.cfg.dim {
                    sleep_ms(150);
                }
                return 0;
            }
            if let Some(code) = self.quit {
                self.reconcile();
                self.release();
                println!("EXIT t={} reason={} code={}", mono_ms(), self.reason, code);
                return code;
            }
            let now = mono_ms();
            let mut dl: Vec<i64> = vec![];
            if let Some(p) = self.pending_plug {
                dl.push(p);
            }
            if let Some(p) = self.input_rescan_at {
                dl.push(p);
            }
            if let Some(p) = self.guard_respawn_at {
                dl.push(p);
            }
            if self.grabbed {
                if let Some(x) = self.router.next_deadline() {
                    dl.push(x);
                }
                if self.cfg.status_info {
                    dl.push(self.info_next);
                }
            }
            if let Some(x) = self.chord.deadline() {
                dl.push(x);
            }
            if let Some(w) = self.notify.watchdog_ms {
                dl.push(self.last_ping + (w / 2).max(100));
            }
            if let Some(g) = &self.guard {
                dl.push(g.last_ping + 1000);
            }
            if self.machine.effective().grab && !self.grabbed {
                dl.push(now + 50);
            }
            let timeout = dl.iter().map(|d| (d - now).clamp(0, 60_000)).min().unwrap_or(60_000) as i32;
            let mut fds = vec![self.wake_rd, self.netlink];
            let touch_fd = if self.grabbed { self.touch.as_ref().map(|t| t.fd).unwrap_or(-1) } else { -1 };
            fds.push(touch_fd);
            fds.push(self.guard.as_ref().map(|g| g.fd()).unwrap_or(-1));
            fds.push(self.pen_dev.as_ref().map(|d| d.fd).unwrap_or(-1));
            let kbase = fds.len();
            fds.extend(self.key_devs.iter().map(|k| k.fd));
            let r = poll_in(&fds, timeout.max(0));
            let now = mono_ms();
            if r[0] {
                drain(self.wake_rd);
            }
            while let Ok(c) = self.rx.try_recv() {
                self.cmd(c);
            }
            if r[1] {
                let ev = notify::read_uevents(self.netlink);
                if ev.iter().any(|u| u.is_drm()) {
                    if self.opts.debug {
                        for u in ev.iter().filter(|u| u.is_drm()) {
                            println!("UEVENT action={} hotplug={} connector={:?}", u.action, u.is_hotplug(), u.get("CONNECTOR"));
                        }
                    }
                    self.rescan(false);
                }
                if ev.iter().any(|u| u.get("SUBSYSTEM") == Some("input") && matches!(u.action.as_str(), "add" | "remove")) && self.input_rescan_at.is_none() {
                    self.input_rescan_at = Some(now + 300);
                }
            }
            if r[2] {
                evs.clear();
                let res = self.touch.as_ref().map(|t| t.read_all(&mut evs));
                if let Some(Err(e)) = res {
                    println!("ERROR touchscreen read: {}", strerr(e));
                    self.handle(Ev::UiLost, "touchscreen-gone");
                } else {
                    let mut outs = vec![];
                    for ev in &evs {
                        if ev.type_ == EV_SYN && ev.code == SYN_DROPPED {
                            println!("WARN SYN_DROPPED: lifting all contacts");
                        }
                        for c in self.mt.feed(ev.type_, ev.code, ev.value) {
                            outs.extend(self.router.contact(c, now));
                        }
                    }
                    self.apply(outs);
                }
            }
            if r[3] {
                self.guard_msgs();
            }
            if r[4] {
                evs.clear();
                let ok = self.pen_dev.as_ref().map(|d| d.read_all(&mut evs).is_ok()).unwrap_or(false);
                if !ok {
                    println!("PEN-WATCH gone");
                    self.pen_dev = None;
                    let o = self.router.set_pen(false);
                    self.apply(o);
                } else {
                    for ev in evs.clone() {
                        if ev.type_ == EV_KEY && ev.code == BTN_TOOL_PEN {
                            if self.opts.debug {
                                println!("PEN t={} near={}", now, ev.value != 0);
                            }
                            let o = self.router.set_pen(ev.value != 0);
                            self.apply(o);
                            self.model_dirty = true;
                        }
                    }
                }
            }
            for i in 0..self.key_devs.len() {
                if !r[kbase + i] {
                    continue;
                }
                evs.clear();
                if self.key_devs[i].read_all(&mut evs).is_err() {
                    self.chord.forget_dev(i);
                    continue;
                }
                for ev in &evs {
                    if ev.type_ == EV_KEY {
                        self.chord.key(i, ev.code, ev.value, now);
                        if self.opts.debug && self.chord.keys.contains(&ev.code) {
                            println!("CHORD-KEY t={} {}={}", now, keycodes::name(ev.code), ev.value);
                        }
                    }
                }
            }
            if self.pending_plug.is_some_and(|p| now >= p) {
                self.plug_due();
            }
            if self.input_rescan_at.is_some_and(|p| now >= p) {
                self.input_rescan_at = None;
                self.scan_inputs();
            }
            if self.guard_respawn_at.is_some_and(|p| now >= p) {
                self.guard_respawn_at = None;
                match Guardian::spawn(3000) {
                    Ok(g) => {
                        println!("GUARDIAN-SPAWNED pid={} (respawn)", g.pid());
                        self.guard = Some(g);
                    }
                    Err(e) => println!("WARN key guardian respawn: {}", e),
                }
            }
            if let Some(g) = self.guard.as_mut() {
                if now - g.last_ping >= 1000 {
                    g.ping();
                    if !g.alive() {
                        self.guardian_lost();
                    }
                }
            }
            if self.grabbed && self.cfg.status_info && now >= self.info_next {
                self.info_next = now + 30_000;
                let ext = self.real_conn.as_ref().map(|c| c.name.clone()).or(if self.sim { Some("simulated display".into()) } else { None });
                let t = feedback::status_text(std::path::Path::new("/sys/class/power_supply"), ext.as_deref());
                let o = self.router.set_info(&t);
                self.apply(o);
            }
            if self.grabbed {
                let o = self.router.tick(now);
                self.apply(o);
            }
            if self.chord.poll(now) {
                let held = self.chord.held_since().map(|s| now - s).unwrap_or(-1);
                println!("CHORD t={} held_ms={} desired={}", now, held, self.machine.desired.name());
                if self.machine.desired == Desired::Pad {
                    self.handle(Ev::Chord, "chord");
                }
            }
            self.reconcile();
            self.update_status();
            if let Some(w) = self.notify.watchdog_ms {
                if now - self.last_ping >= (w / 2).max(100) {
                    let ui_age = now - self.sh.ui_heartbeat.load(Ordering::SeqCst);
                    if ui_age < 2000 {
                        self.notify.send("WATCHDOG=1");
                        self.last_ping = now;
                    } else {
                        self.last_ping = now - (w / 2).max(100) + 250;
                        if self.opts.debug {
                            println!("WARN UI heartbeat {} ms old: no watchdog ping", ui_age);
                        }
                    }
                }
            }
        }
    }
}
