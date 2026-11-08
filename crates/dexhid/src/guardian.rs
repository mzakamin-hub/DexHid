// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! The key guardian (M2): a tiny separate process (`dexhid guardian`, no threads, no UI, no bus) that owns the
//! "dexhid keyboard" uinput device. The daemon sends it key events over a SOCK_SEQPACKET socketpair; the guardian
//! writes them to uinput and tracks which keys are down.
//!
//! Why: when a uinput device is destroyed by the kernel (daemon crash, SIGKILL, watchdog), evdev clients such as the
//! compositor's libinput only see DEVICE_REMOVED; the kernel's own key-up events are dropped with the device (M1
//! T10: a held Shift was never released). The guardian notices the daemon's death (EOF on the socket, or
//! PDEATHSIG = SIGTERM) and FIRST sends key-ups for every held key (+ SYN), THEN destroys the device. It also
//! releases held keys (keeping the device) when the daemon stops talking for `hb_ms` (frozen daemon) and exits on
//! SIGTERM / SIGINT / SIGHUP after the same release. The daemon watches the guardian in turn (socket hang-up /
//! child exit): a dead guardian means an orderly release + a new guardian.
//! The systemd unit uses KillMode=mixed so a watchdog SIGKILL hits the daemon only and the guardian can clean up.
//!
//! Protocol (text, one message per packet). Daemon -> guardian: `C` create keyboard (reply `N <node>` / `E <err>`),
//! `K <code> <0|1>` key, `U` release all held keys (reply `U <n>`), `D` destroy the keyboard (keys up first), `P`
//! heartbeat, `X` exit. Guardian -> daemon: `L <caps> <num>` LED state from the desktop, `H <n>` keys released
//! after a lost heartbeat.

use crate::devices::VirtualKbd;
use crate::sys::*;
use std::collections::BTreeSet;
use std::os::unix::process::CommandExt;

fn send_raw(fd: i32, s: &str) -> bool {
    let r = unsafe { libc::send(fd, s.as_ptr() as *const libc::c_void, s.len(), libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT) };
    r == s.len() as isize
}

/// Ok(Some(msg)), Ok(None) = would block, Err = EOF / error
fn recv_raw(fd: i32) -> Result<Option<String>, ()> {
    let mut b = [0u8; 512];
    let r = unsafe { libc::recv(fd, b.as_mut_ptr() as *mut libc::c_void, b.len(), libc::MSG_DONTWAIT) };
    if r == 0 {
        return Err(());
    }
    if r < 0 {
        let e = errno();
        return if e == libc::EAGAIN || e == libc::EINTR { Ok(None) } else { Err(()) };
    }
    Ok(Some(String::from_utf8_lossy(&b[..r as usize]).into_owned()))
}

// ------------------------------------------------------------------ daemon side

pub struct Guardian {
    fd: i32,
    child: std::process::Child,
    pub node: Option<String>,
    pub held: BTreeSet<u16>,
    /// async messages received while waiting for a reply
    pending: Vec<String>,
    pub last_ping: i64,
}

impl Guardian {
    pub fn spawn(hb_ms: i64) -> Result<Guardian, String> {
        let mut sv = [-1i32; 2];
        if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, sv.as_mut_ptr()) } != 0 {
            return Err(format!("socketpair: {}", strerr(errno())));
        }
        let exe = std::env::current_exe().unwrap_or_else(|_| "/proc/self/exe".into());
        let child_fd = sv[1];
        let mut cmd = std::process::Command::new(exe);
        cmd.args(["guardian", "--fd", "3", "--hb-ms", &hb_ms.to_string()]);
        unsafe {
            cmd.pre_exec(move || {
                // fd 3 = our end (dup2 clears CLOEXEC); SIGTERM when the daemon dies (EOF is the main signal)
                if libc::dup2(child_fd, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let child = cmd.spawn().map_err(|e| format!("spawn guardian: {}", e));
        unsafe { libc::close(child_fd) };
        let child = child.inspect_err(|_| unsafe {
            libc::close(sv[0]);
        })?;
        Ok(Guardian { fd: sv[0], child, node: None, held: BTreeSet::new(), pending: vec![], last_ping: mono_ms() })
    }

    pub fn fd(&self) -> i32 {
        self.fd
    }
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    fn wait_reply(&mut self, prefix: &[&str], ms: i64) -> Option<String> {
        let end = mono_ms() + ms;
        loop {
            match recv_raw(self.fd) {
                Ok(Some(m)) => {
                    if prefix.iter().any(|p| m.starts_with(p)) {
                        return Some(m);
                    }
                    self.pending.push(m);
                    continue;
                }
                Ok(None) => {}
                Err(()) => return None,
            }
            let left = end - mono_ms();
            if left <= 0 {
                return None;
            }
            poll_in(&[self.fd], left.min(50) as i32);
        }
    }

    /// create the keyboard in the guardian; returns the event node
    pub fn create_kbd(&mut self) -> Result<String, String> {
        if let Some(n) = &self.node {
            return Ok(n.clone());
        }
        if !send_raw(self.fd, "C") {
            return Err("guardian not reachable".into());
        }
        match self.wait_reply(&["N ", "E "], 3000) {
            Some(m) if m.starts_with("N ") => {
                let n = m[2..].trim().to_string();
                self.node = Some(n.clone());
                self.held.clear();
                Ok(n)
            }
            Some(m) => Err(m[2..].to_string()),
            None => Err("no answer from the guardian".into()),
        }
    }

    pub fn key(&mut self, code: u16, pressed: bool) {
        if pressed {
            self.held.insert(code);
        } else {
            self.held.remove(&code);
        }
        send_raw(self.fd, &format!("K {} {}", code, pressed as i32));
    }

    /// keys up (waits for the guardian's confirmation, max 100 ms); returns the number the guardian released
    pub fn release_all(&mut self) -> usize {
        self.held.clear();
        if self.node.is_none() || !send_raw(self.fd, "U") {
            return 0;
        }
        self.wait_reply(&["U "], 100).and_then(|m| m[2..].trim().parse().ok()).unwrap_or(0)
    }

    /// destroy the keyboard (asynchronous in the guardian, keys up first)
    pub fn destroy_kbd(&mut self) {
        if self.node.take().is_some() {
            send_raw(self.fd, "D");
        }
        self.held.clear();
    }

    pub fn ping(&mut self) {
        self.last_ping = mono_ms();
        send_raw(self.fd, "P");
    }

    /// async messages (LED state, heartbeat releases); Err = the guardian is gone
    pub fn read_msgs(&mut self) -> Result<Vec<String>, ()> {
        let mut v = std::mem::take(&mut self.pending);
        loop {
            match recv_raw(self.fd) {
                Ok(Some(m)) => v.push(m),
                Ok(None) => return Ok(v),
                Err(()) => {
                    if v.is_empty() {
                        return Err(());
                    }
                    return Ok(v);
                }
            }
        }
    }

    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Guardian {
    fn drop(&mut self) {
        send_raw(self.fd, "X");
        unsafe { libc::close(self.fd) };
        // reap it (it exits at once after X / EOF); never block the daemon for long
        for _ in 0..20 {
            if !matches!(self.child.try_wait(), Ok(None)) {
                return;
            }
            sleep_ms(5);
        }
    }
}

// ------------------------------------------------------------------ the guardian process

/// where the guardian's key events go: the uinput keyboard, or (PC tests, DEXHID_GUARDIAN_FAKE=1) the log
enum Sink {
    Real(VirtualKbd),
    Fake,
}

struct G {
    dev: Option<Sink>,
    held: BTreeSet<u16>,
    leds: (bool, bool),
}

impl G {
    fn emit(&mut self, c: u16, v: i32) {
        match self.dev.as_mut() {
            Some(Sink::Real(k)) => {
                k.u.emit(EV_KEY, c, v);
                k.u.syn();
            }
            Some(Sink::Fake) => println!("GUARDIAN-FAKE t={} key={} value={}", mono_ms(), c, v),
            None => {}
        }
    }
    fn up_all(&mut self) -> usize {
        let v: Vec<u16> = self.held.iter().copied().collect();
        for &c in &v {
            self.emit(c, 0);
        }
        self.held.clear();
        v.len()
    }
    fn node(&self) -> String {
        match &self.dev {
            Some(Sink::Real(k)) => k.u.node.clone(),
            Some(Sink::Fake) => "fake".into(),
            None => String::new(),
        }
    }
    fn destroy(&mut self, why: &str) {
        let n = self.up_all();
        if self.dev.is_some() {
            let node = self.node();
            self.dev = None;
            println!("GUARDIAN t={} keys_up={} then destroyed {} ({})", mono_ms(), n, node, why);
        } else if why != "release" {
            println!("GUARDIAN t={} exit ({}), no keyboard", mono_ms(), why);
        }
    }
}

/// `dexhid guardian --fd 3 --hb-ms 3000`
pub fn run(fd: i32, hb_ms: i64) -> i32 {
    install_signals();
    // M4: a self-pipe so a signal always ends the (untimed) poll
    let (sig_rd, sig_wr) = wake_pipe();
    WAKE_FD.store(sig_wr, std::sync::atomic::Ordering::SeqCst);
    let fake = std::env::var("DEXHID_GUARDIAN_FAKE").is_ok();
    println!("GUARDIAN start pid={} hb_ms={}{}", std::process::id(), hb_ms, if fake { " (fake keyboard)" } else { "" });
    let mut g = G { dev: None, held: BTreeSet::new(), leds: (false, false) };
    let mut last = mono_ms();
    let mut hb_fired = false;
    loop {
        if stopped() {
            g.destroy(&format!("signal {}", stop_reason()));
            return 0;
        }
        let ufd = match &g.dev {
            Some(Sink::Real(k)) => k.u.fd,
            _ => -1,
        };
        // M4: no timer while no key is held (the daemon's death is a hang-up on the socket, a signal ends poll);
        // with keys held: wake when the heartbeat would be overdue
        let timeout = if hb_ms > 0 && !hb_fired && !g.held.is_empty() { (hb_ms - (mono_ms() - last)).clamp(0, hb_ms) as i32 + 1 } else { -1 };
        let r = poll_in(&[fd, ufd, sig_rd], timeout);
        if r[2] {
            drain(sig_rd);
        }
        let now = mono_ms();
        if r[0] {
            loop {
                match recv_raw(fd) {
                    Ok(Some(m)) => {
                        last = now;
                        hb_fired = false;
                        let w: Vec<&str> = m.split_whitespace().collect();
                        match w.first().copied() {
                            Some("K") => {
                                let (Some(c), Some(v)) = (w.get(1).and_then(|x| x.parse::<u16>().ok()), w.get(2)) else { continue };
                                let p = *v == "1";
                                // only transitions reach the device
                                if g.dev.is_some() && p != g.held.contains(&c) {
                                    if p {
                                        g.held.insert(c);
                                    } else {
                                        g.held.remove(&c);
                                    }
                                    g.emit(c, p as i32);
                                }
                            }
                            Some("C") => {
                                if g.dev.is_none() {
                                    if fake {
                                        g.dev = Some(Sink::Fake);
                                    } else {
                                        match VirtualKbd::create() {
                                            Ok(k) => g.dev = Some(Sink::Real(k)),
                                            Err(e) => {
                                                send_raw(fd, &format!("E {}", e));
                                                continue;
                                            }
                                        }
                                    }
                                    println!("GUARDIAN t={} keyboard {}", mono_ms(), g.node());
                                }
                                send_raw(fd, &format!("N {}", g.node()));
                            }
                            Some("U") => {
                                let n = g.up_all();
                                send_raw(fd, &format!("U {}", n));
                            }
                            Some("D") => g.destroy("release"),
                            Some("X") => {
                                g.destroy("exit");
                                return 0;
                            }
                            _ => {}
                        }
                    }
                    Ok(None) => break,
                    Err(()) => {
                        // the daemon is gone (crash, SIGKILL, watchdog): keys up BEFORE the device goes
                        println!("GUARDIAN t={} daemon gone: releasing {} held key(s) before destroying the keyboard", mono_ms(), g.held.len());
                        g.destroy("daemon gone");
                        return 0;
                    }
                }
            }
        }
        if ufd >= 0 && r.get(1).copied().unwrap_or(false) {
            let evs = match &g.dev {
                Some(Sink::Real(k)) => k.u.read_back(),
                _ => vec![],
            };
            let mut leds = g.leds;
            for e in evs.iter().filter(|e| e.type_ == EV_LED) {
                match e.code {
                    LED_CAPSL => leds.0 = e.value != 0,
                    LED_NUML => leds.1 = e.value != 0,
                    _ => {}
                }
            }
            if leds != g.leds {
                g.leds = leds;
                send_raw(fd, &format!("L {} {}", leds.0 as i32, leds.1 as i32));
            }
        }
        if !hb_fired && hb_ms > 0 && now - last > hb_ms && !g.held.is_empty() {
            hb_fired = true;
            let n = g.up_all();
            println!("GUARDIAN t={} no heartbeat for {} ms: released {} held key(s) (device kept)", now, now - last, n);
            send_raw(fd, &format!("H {}", n));
        }
    }
}