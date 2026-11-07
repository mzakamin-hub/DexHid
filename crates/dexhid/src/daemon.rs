// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! `dexhid daemon`: the core thread. Owns the touchscreen grab, the virtual devices, the state machine and the
//! router; the UI, D-Bus and logind threads talk to it through `Cmd` + a wake pipe.
//!
//! Release order (every path): keys up + pad contacts lifted + SYN, ungrab, destroy the virtual devices, hide the UI.
//! The kernel does the same on a crash or SIGKILL (evdev_release ungrabs, uinput unregister sends key-ups: M0 S5).

use crate::dbus;
use crate::devices::{VirtualKbd, VirtualPad};
use crate::notify::{self, Notify};
use crate::shared::{Cmd, Shared, SharedRef, UiCfg};
use crate::sys::*;
use dexhid_core::arrangement::{self, Arrangement, Orientation};
use dexhid_core::chord::Chord;
use dexhid_core::config::{self, Config, Persist};
use dexhid_core::hotplug::{self, Connector};
use dexhid_core::keycodes;
use dexhid_core::layout::{self, Layout};
use dexhid_core::mt::Mt;
use dexhid_core::router::{Out, Router, RouterCfg};
use dexhid_core::state::{Autostart, Desired, Ev, Machine, Remember, UiKind};
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

fn find_touch(name: &str) -> Option<String> {
    event_nodes().into_iter().find(|p| Dev::open(p, false).map(|d| d.name() == name).unwrap_or(false))
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
    layouts: Vec<Layout>,
    machine: Machine,
    router: Router,
    mt: Mt,
    touch_path: Option<String>,
    nslots: usize,
    touch: Option<Dev>,
    grabbed: bool,
    pad: Option<VirtualPad>,
    kbd: Option<VirtualKbd>,
    chord: Chord,
    key_devs: Vec<Dev>,
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
    let (cfg, notes) = config::load(&paths);
    for n in notes {
        println!("CONFIG {}", n);
    }
    // touchscreen
    let touch_path = opts.touch_dev.clone().or(if cfg.touch_dev.is_empty() { None } else { Some(cfg.touch_dev.clone()) }).or_else(|| find_touch(&cfg.touchscreen));
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
    // layouts + persisted choices
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
    let autostart = persist.autostart.as_deref().and_then(Autostart::parse).or(Autostart::parse(&cfg.autostart)).unwrap_or(Autostart::Ask);
    let rcfg = RouterCfg {
        hold_ms: cfg.hold_ms,
        strip_mm: cfg.strip_mm,
        key_gap_mm: cfg.key_gap_mm,
        click_zone_mm: if cfg.click_zones { cfg.click_zone_mm } else { 0.0 },
        five_finger: cfg.five_finger_release,
        pad_units_per_mm: res as f32 / cfg.speed.clamp(0.2, 5.0),
        palm_major: cfg.palm_major,
        options: cfg.keyboard_options.iter().cloned().collect(),
        ..RouterCfg::default()
    };
    let mut router = Router::new(rcfg, layout);
    router.set_autostart_label(autostart.name());
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
    });
    // D-Bus first: a second instance stops here
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
    {
        let s = sh.clone();
        std::thread::Builder::new().name("ui".into()).spawn(move || crate::ui::run(s, ui_rd)).expect("ui thread");
        let s = sh.clone();
        std::thread::Builder::new().name("logind".into()).spawn(move || crate::logind::run(s)).expect("logind thread");
    }
    let netlink = match notify::uevent_socket() {
        Ok(fd) => fd,
        Err(e) => {
            println!("WARN hotplug: {} (no monitor detection)", e);
            -1
        }
    };
    // passive chord sources (never grabbed) + the debug test source (grabbed so the desktop does not react)
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

    let notify = Notify::from_env();
    let mut d = D {
        sh,
        rx,
        wake_rd,
        machine: Machine::new(autostart, cfg.display_pill),
        cfg_dir: cfg_home.join("dexhid"),
        state_file,
        persist,
        layouts,
        router,
        mt: Mt::new(nslots),
        touch_path,
        nslots,
        touch: None,
        grabbed: false,
        pad: None,
        kbd: None,
        chord,
        key_devs,
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
        cfg,
        opts,
    };
    println!("CONFIG autostart={} keyboard={} internal_output={} connectors={:?} watchdog_ms={:?}", d.machine.autostart.name(), d.router.layout().name, d.cfg.internal_output, d.cfg.external_connectors, d.notify.watchdog_ms);
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
    }

    fn connect(&mut self, id: String, label: &str) {
        let rem = self.persist.monitors.get(&id).map(|v| if v == "pad" { Remember::Pad } else { Remember::Display });
        println!("PLUG t={} connected={} remembered={:?}", mono_ms(), label, rem);
        self.monitor_id = Some(id);
        self.handle(Ev::Connected(rem), "plug");
    }

    /// re-read the DRM connectors; `startup` = no debounce
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
            // logind repeats values on every property change: act on real changes only (keeps the release reason)
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
            Cmd::Note(s) => println!("NOTE {}", s),
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
        let Some(l) = self.layouts.iter().find(|l| l.name == name).cloned() else {
            println!("ERROR unknown layout {:?} (have: {})", name, self.layouts.iter().map(|l| l.name.as_str()).collect::<Vec<_>>().join(", "));
            return;
        };
        let arr = self.router.orientation().and_then(|o| self.load_arr(&l.name, o));
        println!("LAYOUT t={} name={} title=\"{}\" source={}", mono_ms(), l.name, l.title, l.source);
        let o = self.router.set_layout(l, arr);
        self.apply(o);
        self.persist.keyboard = Some(name.to_string());
        if let Err(e) = self.persist.save(&self.state_file) {
            println!("WARN state file: {}", e);
        }
    }

    fn apply(&mut self, outs: Vec<Out>) {
        for o in outs {
            match o {
                Out::Key(c, p) => {
                    if let Some(k) = self.kbd.as_mut() {
                        k.key(c, p);
                    }
                    if self.opts.debug {
                        println!("KEY t={} {}={}", mono_ms(), keycodes::name(c), p as i32);
                    }
                }
                Out::Pad(f) => {
                    if let Some(p) = self.pad.as_mut() {
                        p.apply(&f);
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
                Out::AutostartNext => {
                    let a = self.machine.autostart.next();
                    self.machine.autostart = a;
                    self.persist.autostart = Some(a.name().into());
                    let _ = self.persist.save(&self.state_file);
                    println!("AUTOSTART {}", a.name());
                    let o = self.router.set_autostart_label(a.name());
                    self.apply(o);
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
                    if self.pad.is_some() {
                        self.pad = None;
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
        match VirtualPad::create(spec) {
            Ok(p) => {
                println!("UINPUT pad node={} xmax={} ymax={} res={}", p.u.node, spec.0, spec.1, spec.2);
                self.pad = Some(p);
            }
            Err(e) => {
                println!("ERROR uinput touchpad: {} (install 70-dexhid.rules for /dev/uinput)", e);
                self.handle(Ev::UiLost, "no-uinput");
            }
        }
    }

    fn release(&mut self) {
        if !self.grabbed && self.pad.is_none() && self.kbd.is_none() && self.touch.is_none() {
            return;
        }
        let was = self.grabbed;
        let o = self.router.reset_contacts();
        // only key / pad outs matter here
        let outs: Vec<Out> = o.into_iter().filter(|x| matches!(x, Out::Key(..) | Out::Pad(..))).collect();
        self.apply(outs);
        let extra = self.kbd.as_mut().map(|k| k.release_all()).unwrap_or(0);
        let mut slots = None;
        if let Some(t) = &self.touch {
            slots = t.active_slots();
            if self.grabbed {
                let _ = t.grab(false);
            }
        }
        self.grabbed = false;
        self.pad = None;
        self.kbd = None;
        self.touch = None;
        self.wait_logged = false;
        println!("RELEASED t={} reason={} grabbed={} active_slots={:?} extra_keys={} order=keys,contacts,ungrab,destroy", mono_ms(), self.reason, was, slots, extra);
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
            if self.kbd.is_none() {
                match VirtualKbd::create() {
                    Ok(k) => {
                        println!("UINPUT kbd node={}", k.u.node);
                        self.kbd = Some(k);
                    }
                    Err(err) => {
                        println!("ERROR uinput keyboard: {} (install 70-dexhid.rules for /dev/uinput)", err);
                        self.handle(Ev::UiLost, "no-uinput");
                        return self.reconcile();
                    }
                }
            }
            if self.pad.is_none() && self.router.ready() {
                self.create_pad();
                if self.pad.is_none() {
                    return self.reconcile();
                }
            }
            if !self.grabbed && self.mapped == UiKind::Overlay && self.router.ready() && self.pad.is_some() {
                let t = self.touch.as_ref().unwrap();
                let active = t.active_slots().unwrap_or(0);
                let tpath = t.path.clone();
                if active == 0 {
                    let g = t.grab(true);
                    let tids = t.mt_slots(ABS_MT_TRACKING_ID, self.nslots);
                    match g {
                        Ok(()) => {
                            self.grabbed = true;
                            self.mt = Mt::new(self.nslots);
                            if let Some(tids) = tids {
                                self.mt.sync_tids(&tids);
                            }
                            let o = self.router.reset_contacts();
                            self.apply(o);
                            println!("GRABBED t={} dev={} reason={}", mono_ms(), tpath, self.reason);
                            self.print_geom();
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
        // UI
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
            "touch_dev": self.touch_path,
            "pad_node": self.pad.as_ref().map(|p| p.u.node.clone()),
            "kbd_node": self.kbd.as_ref().map(|k| k.u.node.clone()),
            "held_keys": self.router.held_codes().iter().map(|&c| keycodes::name(c)).collect::<Vec<_>>(),
            "last_reason": self.reason,
            "outputs": self.outputs,
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
                return 0;
            }
            if let Some(code) = self.quit {
                self.reconcile();
                self.release();
                println!("EXIT t={} reason={} code={}", mono_ms(), self.reason, code);
                return code;
            }
            let now = mono_ms();
            // timers
            let mut dl: Vec<i64> = vec![];
            if let Some(p) = self.pending_plug {
                dl.push(p);
            }
            if self.grabbed {
                if let Some(x) = self.router.next_deadline() {
                    dl.push(x);
                }
            }
            if let Some(x) = self.chord.deadline() {
                dl.push(x);
            }
            if let Some(w) = self.notify.watchdog_ms {
                dl.push(self.last_ping + (w / 2).max(100));
            }
            if self.machine.effective().grab && !self.grabbed {
                dl.push(now + 50); // waiting for the overlay to map / fingers to lift
            }
            let timeout = dl.iter().map(|d| (d - now).clamp(0, 60_000)).min().unwrap_or(60_000) as i32;
            let mut fds = vec![self.wake_rd, self.netlink];
            let touch_fd = if self.grabbed { self.touch.as_ref().map(|t| t.fd).unwrap_or(-1) } else { -1 };
            fds.push(touch_fd);
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
            for i in 0..self.key_devs.len() {
                if !r[3 + i] {
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
            // timers due
            if self.pending_plug.is_some_and(|p| now >= p) {
                self.plug_due();
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
                        // no ping: systemd will act; look again in 250 ms (never a busy loop)
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
