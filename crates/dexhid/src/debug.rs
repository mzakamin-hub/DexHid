// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! `dexhid debug ...`: test tools for the tablet runner (root). Not used by the daemon.
//!   list                                   input devices and their roles
//!   grabtest --dev P                       FREE / BUSY (+ active slots)
//!   replay --fifo F [--clone P] [--name N] [--secs S]
//!                                          replay touchscreen driven by commands written to the FIFO
//!   volsim --fifo F [--keys K1+K2] [--secs S]
//!                                          virtual volume keys (the daemon grabs it via --chord-test-dev)
//! FIFO commands (one per line, several per line with ';'), coordinates in native touch units:
//!   tap X Y [MS] | hold X Y MS | drag X0 Y0 X1 Y1 MS | press S X Y | pressm S X Y MAJOR | moveto S X Y | release S
//!   multi N X Y DX MS (N fingers DX apart, held MS) | swipe MS DX DY X1 Y1 [X2 Y2 ..] (M2.1: fingers moved together)
//!   down CODE.. | up CODE.. (volsim) | sleep MS | mark ID | quit

use crate::devices::{create_replay_ts, Ts};
use crate::sys::*;

pub struct Args(pub Vec<String>);
impl Args {
    pub fn val(&self, k: &str) -> Option<String> {
        self.0.iter().position(|a| a == k).and_then(|i| self.0.get(i + 1)).cloned()
    }
    pub fn flag(&self, k: &str) -> bool {
        self.0.iter().any(|a| a == k)
    }
    pub fn f64(&self, k: &str, d: f64) -> f64 {
        self.val(k).and_then(|v| v.parse().ok()).unwrap_or(d)
    }
}

pub fn run(a: &Args) -> i32 {
    match a.0.first().map(|s| s.as_str()) {
        Some("list") => list(),
        Some("grabtest") => grabtest(a),
        Some("replay") => replay(a),
        Some("volsim") => volsim(a),
        Some("render") => crate::ui::render_debug(a),
        // M3 test tools
        Some("hid-desc") => hid_desc(a),
        Some("hid-client") => hid_client(a),
        Some("gadget-remove") => {
            let root = std::path::PathBuf::from(a.val("--configfs").unwrap_or_else(|| "/sys/kernel/config/usb_gadget".into()));
            match dexhid_core::gadget::remove(&root, root.starts_with("/sys/kernel/config")) {
                Ok(l) => {
                    println!("GADGET-REMOVE steps={} {}", l.len(), if l.is_empty() { "(nothing there)" } else { "" });
                    0
                }
                Err(e) => {
                    println!("GADGET-REMOVE failed: {}", e);
                    1
                }
            }
        }
        _ => {
            eprintln!("usage: dexhid debug list | grabtest --dev P | replay --fifo F [--clone P] [--name N] [--secs S] | volsim --fifo F [--keys K1+K2] [--secs S]");
            2
        }
    }
}

/// `debug hid-desc kbd|mouse|extra|extra-abs`: a report descriptor as hex (runner: compare with configfs)
fn hid_desc(a: &Args) -> i32 {
    use dexhid_core::hid;
    let w = a.0.get(1).map(|s| s.as_str()).unwrap_or("kbd");
    let d = match w {
        "kbd" => hid::usb_functions(false)[0].desc.clone(),
        "mouse" => hid::usb_functions(false)[1].desc.clone(),
        "extra" => hid::usb_functions(false)[2].desc.clone(),
        "extra-abs" => hid::usb_functions(true)[2].desc.clone(),
        _ => {
            println!("FAIL hid-desc: kbd | mouse | extra | extra-abs");
            return 2;
        }
    };
    println!("{}", hid::hex(&d));
    0
}

/// `debug hid-client --socket S [--hold MS] [--write] CMD..`: a test client of the HID helper. Sends each command,
/// prints the replies; with --write writes one keyboard report (all keys up) to the first fd it got and prints the
/// result (no host: ESHUTDOWN); then holds the connection MS ms (the runner kills it to test the crash teardown).
fn hid_client(a: &Args) -> i32 {
    let sock = a.val("--socket").unwrap_or_else(|| "/run/dexhid/hid.sock".into());
    let hold = a.val("--hold").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let s = match crate::ipc::connect(&sock) {
        Ok(s) => s,
        Err(e) => {
            println!("FAIL {}", e);
            return 1;
        }
    };
    let mut got: Vec<i32> = vec![];
    let skip = ["--socket", "--hold"];
    let mut i = 1;
    while i < a.0.len() {
        let w = &a.0[i];
        if skip.contains(&w.as_str()) {
            i += 2;
            continue;
        }
        if w == "--write" {
            i += 1;
            continue;
        }
        let _ = crate::ipc::send(s, w, &[]);
        match crate::ipc::recv(s) {
            Some((r, fds)) => {
                println!("REPLY {} fds={}", r, fds.len());
                got.extend(fds);
            }
            None => {
                println!("REPLY <closed>");
                break;
            }
        }
        i += 1;
    }
    if a.flag("--write") {
        if let Some(&fd) = got.first() {
            let r = crate::ipc::write_fd(fd, &[0u8; 8]);
            println!("WRITE kbd result={}", match r {
                Ok(n) => format!("ok {} bytes", n),
                Err(e) if e == libc::ESHUTDOWN => "ESHUTDOWN (no host configured the gadget)".into(),
                Err(e) => strerr(e),
            });
        }
    }
    if hold > 0 {
        println!("HOLD {} ms pid={}", hold, std::process::id());
        sleep_ms(hold);
    }
    0
}

fn list() -> i32 {
    for p in event_nodes() {
        let Ok(d) = Dev::open(&p, false) else {
            println!("DEV path={} open=failed", p);
            continue;
        };
        let props = d.props();
        let mt = d.has_abs(ABS_MT_POSITION_X);
        println!(
            "DEV path={} name=\"{}\" direct={} pointer={} mt={} volup={} voldown={} active={}",
            p,
            d.name(),
            test_bit(&props, INPUT_PROP_DIRECT as usize),
            test_bit(&props, INPUT_PROP_POINTER as usize),
            mt,
            d.has_key(KEY_VOLUMEUP),
            d.has_key(KEY_VOLUMEDOWN),
            d.active_slots().map(|n| n.to_string()).unwrap_or_else(|| "-".into())
        );
    }
    0
}

fn grabtest(a: &Args) -> i32 {
    let Some(p) = a.val("--dev") else {
        println!("GRABTEST result=ERR no --dev");
        return 1;
    };
    let d = match Dev::open(&p, false) {
        Ok(d) => d,
        Err(e) => {
            println!("GRABTEST dev={} result=ERR open: {}", p, strerr(e));
            return 1;
        }
    };
    let slots = d.active_slots().map(|n| n.to_string()).unwrap_or_else(|| "n/a".into());
    match d.grab(true) {
        Ok(()) => {
            let _ = d.grab(false);
            println!("GRABTEST dev={} result=FREE active_slots={}", p, slots);
            0
        }
        Err(e) if e == libc::EBUSY => {
            println!("GRABTEST dev={} result=BUSY active_slots={}", p, slots);
            2
        }
        Err(e) => {
            println!("GRABTEST dev={} result=ERR {}", p, strerr(e));
            1
        }
    }
}

/// open a FIFO for reading without ever seeing EOF (we hold a write end too)
fn open_fifo(path: &str) -> Result<i32, String> {
    let c = std::ffi::CString::new(path).unwrap();
    if !std::path::Path::new(path).exists() && unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
        return Err(format!("mkfifo {}: {}", path, strerr(errno())));
    }
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC) };
    if fd < 0 {
        Err(format!("open {}: {}", path, strerr(errno())))
    } else {
        Ok(fd)
    }
}

/// read command lines from the FIFO until the deadline; calls `f` for each command, which returns false to quit
fn command_loop(fd: i32, secs: f64, mut f: impl FnMut(&[&str]) -> bool) -> &'static str {
    let end = mono_ms() + (secs * 1000.0) as i64;
    let mut buf = String::new();
    loop {
        if stopped() {
            return "signal";
        }
        if mono_ms() >= end {
            return "timeout";
        }
        let r = poll_in(&[fd], 100);
        if !r[0] {
            continue;
        }
        let mut b = [0u8; 4096];
        let n = unsafe { libc::read(fd, b.as_mut_ptr() as *mut libc::c_void, b.len()) };
        if n <= 0 {
            continue;
        }
        buf.push_str(&String::from_utf8_lossy(&b[..n as usize]));
        while let Some(i) = buf.find('\n') {
            let line: String = buf.drain(..=i).collect();
            for cmd in line.split(';') {
                let w: Vec<&str> = cmd.split_whitespace().collect();
                if w.is_empty() {
                    continue;
                }
                if w[0] == "quit" {
                    return "quit";
                }
                if w[0] == "mark" {
                    println!("MARK {} t={}", w.get(1).unwrap_or(&"-"), mono_ms());
                    continue;
                }
                if w[0] == "sleep" {
                    sleep_ms(w.get(1).and_then(|v| v.parse().ok()).unwrap_or(0));
                    continue;
                }
                if !f(&w) {
                    return "quit";
                }
            }
        }
    }
}

struct Replay {
    ts: Ts,
    down: Vec<Option<(i32, i32)>>,
}

impl Replay {
    fn frame(&mut self) {
        let n = self.down.iter().filter(|d| d.is_some()).count();
        let (x, y) = self.down.iter().flatten().next().copied().unwrap_or((0, 0));
        self.ts.frame(n, x, y);
    }
    fn press(&mut self, s: usize, x: i32, y: i32) {
        if s < self.down.len() {
            self.ts.down(s as i32, x, y);
            self.down[s] = Some((x, y));
            self.frame();
        }
    }
    fn moveto(&mut self, s: usize, x: i32, y: i32) {
        if s < self.down.len() && self.down[s].is_some() {
            self.ts.mv(s as i32, x, y);
            self.down[s] = Some((x, y));
            self.frame();
        }
    }
    fn release(&mut self, s: usize) {
        if s < self.down.len() && self.down[s].is_some() {
            self.ts.up(s as i32);
            self.down[s] = None;
            self.frame();
        }
    }
    fn release_all(&mut self) {
        for s in 0..self.down.len() {
            if self.down[s].is_some() {
                self.ts.up(s as i32);
                self.down[s] = None;
            }
        }
        self.frame();
    }
}

fn replay(a: &Args) -> i32 {
    install_signals();
    let Some(fifo) = a.val("--fifo") else {
        println!("FAIL replay: no --fifo");
        return 1;
    };
    let name = a.val("--name").unwrap_or_else(|| "dexhid-replay".into());
    let ts = match create_replay_ts(&name, a.val("--clone").as_deref()) {
        Ok(t) => t,
        Err(e) => {
            println!("FAIL replay: {}", e);
            return 1;
        }
    };
    let n = ts.slots.max(1) as usize;
    println!("NODE {} name=\"{}\" slots={} xmax={} ymax={} t={}", ts.u.node, name, ts.slots, ts.xmax, ts.ymax, mono_ms());
    let fd = match open_fifo(&fifo) {
        Ok(fd) => fd,
        Err(e) => {
            println!("FAIL replay: {}", e);
            return 1;
        }
    };
    let mut r = Replay { ts, down: vec![None; n] };
    let p = |w: &[&str], i: usize| -> i32 { w.get(i).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0) as i32 };
    let why = command_loop(fd, a.f64("--secs", 170.0).min(600.0), |w| {
        println!("CMD t={} {}", mono_ms(), w.join(" "));
        match w[0] {
            "tap" => {
                r.press(0, p(w, 1), p(w, 2));
                sleep_ms(w.get(3).and_then(|v| v.parse().ok()).unwrap_or(60));
                r.release(0);
            }
            "hold" => {
                r.press(0, p(w, 1), p(w, 2));
                sleep_ms(p(w, 3).max(0) as u64);
                r.release(0);
            }
            "drag" => {
                let (x0, y0, x1, y1, ms) = (p(w, 1), p(w, 2), p(w, 3), p(w, 4), p(w, 5).max(20));
                let steps = (ms / 10).max(2);
                r.press(0, x0, y0);
                sleep_ms(30);
                for i in 1..=steps {
                    let f = i as f64 / steps as f64;
                    r.moveto(0, x0 + ((x1 - x0) as f64 * f) as i32, y0 + ((y1 - y0) as f64 * f) as i32);
                    sleep_ms(10);
                }
                sleep_ms(40);
                r.release(0);
            }
            "press" => r.press(p(w, 1).max(0) as usize, p(w, 2), p(w, 3)),
            // pressm S X Y MAJOR: a contact with this touch major (palm replay)
            "pressm" => {
                let s = p(w, 1).max(0) as usize;
                if s < r.down.len() {
                    r.ts.down_major(s as i32, p(w, 2), p(w, 3), p(w, 4));
                    r.down[s] = Some((p(w, 2), p(w, 3)));
                    r.frame();
                }
            }
            "moveto" => r.moveto(p(w, 1).max(0) as usize, p(w, 2), p(w, 3)),
            "release" => r.release(p(w, 1).max(0) as usize),
            // M2.1: swipe MS DX DY X1 Y1 [X2 Y2 ...]: fingers at the points (slots 0..), all moved by DX DY over MS
            // in 8 ms steps, then lifted (whole-screen swipes, two-finger flicks)
            "swipe" => {
                let ms = p(w, 1).max(16);
                let (dx, dy) = (p(w, 2), p(w, 3));
                let pts: Vec<(i32, i32)> = (4..w.len()).step_by(2).map(|i| (p(w, i), p(w, i + 1))).collect();
                for (i, &(x, y)) in pts.iter().enumerate() {
                    r.press(i, x, y);
                    sleep_ms(4);
                }
                sleep_ms(30);
                let steps = (ms / 8).max(2);
                for s in 1..=steps {
                    for (i, &(x, y)) in pts.iter().enumerate() {
                        if i < r.down.len() && r.down[i].is_some() {
                            r.ts.mv(i as i32, x + dx * s / steps, y + dy * s / steps);
                            r.down[i] = Some((x + dx * s / steps, y + dy * s / steps));
                        }
                    }
                    r.frame();
                    sleep_ms(8);
                }
                r.release_all();
            }
            "multi" => {
                let (k, x, y, dx, ms) = (p(w, 1).clamp(1, 10) as usize, p(w, 2), p(w, 3), p(w, 4), p(w, 5).max(0));
                for i in 0..k {
                    r.press(i, x + dx * i as i32, y);
                    sleep_ms(15);
                }
                sleep_ms(ms as u64);
                r.release_all();
            }
            other => println!("WARN unknown command {}", other),
        }
        true
    });
    r.release_all();
    println!("REPLAY done t={} reason={}", mono_ms(), why);
    0
}

fn volsim(a: &Args) -> i32 {
    install_signals();
    let Some(fifo) = a.val("--fifo") else {
        println!("FAIL volsim: no --fifo");
        return 1;
    };
    let keys: Vec<u16> = a.val("--keys").unwrap_or_else(|| "KEY_VOLUMEUP+KEY_VOLUMEDOWN".into()).split('+').filter_map(dexhid_core::keycodes::code).collect();
    // M2: --name / --bus bluetooth make it a stand-in for a pen (keys 320 = BTN_TOOL_PEN) or a BT keyboard
    let name = a.val("--name").unwrap_or_else(|| "dexhid-volsim".into());
    let bus = if a.val("--bus").as_deref() == Some("bluetooth") { BUS_BLUETOOTH } else { 0 };
    // M3: --pen adds pen axes (X / Y / pressure / tilt, ranges cloned from --clone P when given) + BTN_TOUCH /
    // BTN_STYLUS; command `abs X Y [PRESSURE]`
    let mut keys = keys;
    let mut abs = vec![];
    if a.flag("--pen") {
        let src = a.val("--clone").and_then(|p| Dev::open(&p, false).ok());
        let g = |code: u16, d: AbsInfo| src.as_ref().and_then(|s| s.absinfo(code)).unwrap_or(d);
        abs.push((ABS_X, g(ABS_X, ai(0, 33919, 124))));
        abs.push((0x01, g(0x01, ai(0, 23999, 124))));
        abs.push((0x18, g(0x18, ai(0, 4095, 0))));
        abs.push((0x1a, g(0x1a, ai(-90, 90, 0))));
        abs.push((0x1b, g(0x1b, ai(-90, 90, 0))));
        for k in [BTN_TOOL_PEN, BTN_TOUCH, 0x14b] {
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
        keys.sort_unstable();
    }
    let u = match Uinput::create(&UinputSpec { keys: keys.clone(), bus, abs, ..UinputSpec::new(&name) }) {
        Ok(u) => u,
        Err(e) => {
            println!("FAIL volsim: {}", e);
            return 1;
        }
    };
    println!("NODE {} name=\"{}\" t={}", u.node, name, mono_ms());
    let fd = match open_fifo(&fifo) {
        Ok(fd) => fd,
        Err(e) => {
            println!("FAIL volsim: {}", e);
            return 1;
        }
    };
    let why = command_loop(fd, a.f64("--secs", 170.0).min(600.0), |w| {
        if w[0] == "abs" {
            let n = |i: usize| w.get(i).and_then(|v| v.parse::<i32>().ok());
            if let (Some(x), Some(y)) = (n(1), n(2)) {
                u.emit(EV_ABS, ABS_X, x);
                u.emit(EV_ABS, 0x01, y);
                if let Some(p) = n(3) {
                    u.emit(EV_ABS, 0x18, p);
                }
                u.syn();
                println!("SIM t={} abs {} {}", mono_ms(), x, y);
            }
            return true;
        }
        let v = match w[0] {
            "down" => 1,
            "up" => 0,
            other => {
                println!("WARN unknown command {}", other);
                return true;
            }
        };
        for k in &w[1..] {
            if let Some(c) = dexhid_core::keycodes::code(k) {
                u.emit(EV_KEY, c, v);
            }
        }
        u.syn();
        println!("SIM t={} {} {}", mono_ms(), w[0], w[1..].join(" "));
        true
    });
    for k in keys {
        u.emit(EV_KEY, k, 0);
    }
    u.syn();
    println!("VOLSIM done t={} reason={}", mono_ms(), why);
    0
}
