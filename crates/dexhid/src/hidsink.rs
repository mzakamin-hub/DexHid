// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! M3: the HID sink of the USB / Bluetooth modes, in the daemon. Same router, different sink: key events become
//! keyboard / consumer / system reports, touchpad frames go through the relative-mouse recognizer (mouse.rs) and
//! become mouse reports, the laser / pen give absolute reports. The fds come from the root helper (hidhelper.rs);
//! the helper keeps duplicates and sends "everything released" if this process dies.
//! Writes are non-blocking: a report the host has not collected yet (EAGAIN) is queued and retried
//! (`flush`, the daemon polls every 8 ms while something is queued); with no host (USB: ESHUTDOWN, the
//! function is not configured) reports are dropped and the status says so.

use crate::ipc;
use dexhid_core::hid::{self, KeyState, MouseState, Report};
use dexhid_core::mouse::{MOut, Mouse, MouseCfg};
use dexhid_core::touchpad::PadFrame;
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Usb,
    Bt,
}

impl Kind {
    pub fn name(&self) -> &'static str {
        match self {
            Kind::Usb => "usb",
            Kind::Bt => "bluetooth",
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Event {
    Leds(bool, bool),
    Connected(String),
    Disconnected(String),
    HelperGone,
    Note(String),
}

pub struct Sink {
    pub kind: Kind,
    helper: i32,
    /// usb: kbd, mouse, extra; bt: ctrl, intr (empty until a host connects)
    fds: Vec<i32>,
    keys: KeyState,
    mouse: MouseState,
    rec: Mouse,
    abs: bool,
    queue: VecDeque<(usize, Vec<u8>)>,
    pub host: Option<String>,
    pub info: String,
    pub sent: u64,
    pub dropped_no_host: u64,
    warned: bool,
}

fn request(sock: i32, msg: &str) -> Result<(String, Vec<i32>), String> {
    ipc::send(sock, msg, &[])?;
    let mut fds = [libc::pollfd { fd: sock, events: libc::POLLIN, revents: 0 }];
    // RegisterProfile / gadget binding can take a moment
    if unsafe { libc::poll(fds.as_mut_ptr(), 1, 8000) } <= 0 {
        return Err("the HID helper did not answer".into());
    }
    let (r, f) = ipc::recv(sock).ok_or("the HID helper closed the connection")?;
    if let Some(e) = r.strip_prefix("ERR ") {
        for x in f {
            ipc::close(x);
        }
        return Err(e.to_string());
    }
    Ok((r, f))
}

/// one request to the helper on a fresh connection (status queries)
pub fn helper_query(path: &str, msg: &str) -> Result<String, String> {
    let s = ipc::connect(path)?;
    let r = request(s, msg).map(|x| x.0);
    ipc::close(s);
    r
}

impl Sink {
    /// connect to the helper and bring the mode up
    pub fn open(kind: Kind, path: &str, abs: bool, stop_other: bool, udc: &str, discoverable: u32, units_per_mm: f32) -> Result<Sink, String> {
        let s = ipc::connect(path).map_err(|e| format!("{} (is the DexHid HID helper running? dexhid-hid.socket / `sudo dexhid hid-helper`)", e))?;
        let hello = request(s, "HELLO");
        if let Err(e) = hello {
            ipc::close(s);
            return Err(e);
        }
        let cmd = match kind {
            Kind::Usb => format!("USB-UP abs={} stop_other={} udc={}", abs as u8, stop_other as u8, udc),
            Kind::Bt => format!("BT-UP abs={} discoverable={}", abs as u8, discoverable),
        };
        let (reply, fds) = match request(s, &cmd) {
            Ok(x) => x,
            Err(e) => {
                ipc::close(s);
                return Err(e);
            }
        };
        if kind == Kind::Usb && fds.len() != 3 {
            ipc::close(s);
            return Err(format!("helper sent {} fds, expected 3", fds.len()));
        }
        for f in &fds {
            ipc::set_nonblock(*f);
        }
        let rec = Mouse::new(MouseCfg { units_per_mm, px_per_mm: 9.0, ..Default::default() });
        Ok(Sink {
            kind,
            helper: s,
            fds,
            keys: KeyState::default(),
            mouse: MouseState::default(),
            rec,
            abs,
            queue: VecDeque::new(),
            host: if kind == Kind::Usb { Some("usb".into()) } else { None },
            info: reply.trim_start_matches("OK ").to_string(),
            sent: 0,
            dropped_no_host: 0,
            warned: false,
        })
    }

    pub fn connected(&self) -> bool {
        match self.kind {
            Kind::Usb => true,
            Kind::Bt => self.fds.len() == 2,
        }
    }

    /// the fds the daemon polls: the helper socket, then the data fds
    pub fn poll_fds(&self) -> Vec<i32> {
        let mut v = vec![self.helper];
        match self.kind {
            Kind::Usb => v.push(self.fds.first().copied().unwrap_or(-1)),
            Kind::Bt => v.extend(self.fds.iter().copied()),
        }
        v
    }

    pub fn queued(&self) -> bool {
        !self.queue.is_empty()
    }

    fn write(&mut self, idx: usize, b: Vec<u8>) {
        if !self.queue.is_empty() {
            self.queue.push_back((idx, b));
            return;
        }
        let Some(&fd) = self.fds.get(idx) else {
            self.dropped_no_host += 1;
            return;
        };
        match ipc::write_fd(fd, &b) {
            Ok(_) => self.sent += 1,
            Err(e) if e == libc::EAGAIN => self.queue.push_back((idx, b)),
            Err(e) if e == libc::ESHUTDOWN => {
                self.dropped_no_host += 1;
                if !self.warned {
                    self.warned = true;
                    println!("HIDSINK t={} {} write: no host has configured the gadget yet (ESHUTDOWN): reports dropped", crate::sys::mono_ms(), self.kind.name());
                }
            }
            Err(e) => println!("HIDSINK WARN write fd {}: {}", idx, crate::sys::strerr(e)),
        }
    }

    /// retry queued reports (keeps the order)
    pub fn flush(&mut self) {
        while let Some((idx, b)) = self.queue.front().cloned() {
            let Some(&fd) = self.fds.get(idx) else {
                self.queue.clear();
                return;
            };
            match ipc::write_fd(fd, &b) {
                Ok(_) => {
                    self.sent += 1;
                    self.queue.pop_front();
                }
                Err(e) if e == libc::EAGAIN => return,
                Err(_) => {
                    self.queue.pop_front();
                }
            }
            if self.queue.len() > 512 {
                // a host that stopped reading: keep only the newest state (release reports are re-sent anyway)
                self.queue.drain(..256);
            }
        }
    }

    fn report(&mut self, r: Report) {
        match self.kind {
            Kind::Usb => {
                if let Some((i, b)) = hid::encode_usb(&r, self.abs) {
                    self.write(i, b);
                }
            }
            Kind::Bt => {
                if let Some(b) = hid::encode_bt(&r, self.abs) {
                    self.write(1, b);
                }
            }
        }
    }

    pub fn key(&mut self, code: u16, pressed: bool) {
        for r in self.keys.key(code, pressed) {
            self.report(r);
        }
    }

    /// a touchpad frame (the relative-mouse recognizer: motion, taps, two-finger scroll)
    pub fn pad(&mut self, f: &PadFrame, now: i64) {
        let mut o = self.rec.feed(&f.slots, now);
        o.extend(f.buttons.iter().map(|&(b, p)| MOut::Button(b, p)));
        self.mouse_outs(&o);
    }

    pub fn mouse_outs(&mut self, o: &[MOut]) {
        let (mut dx, mut dy, mut wv, mut wh) = (0, 0, 0, 0);
        let mut btn = vec![];
        for m in o {
            match *m {
                MOut::Rel(x, y) => {
                    dx += x;
                    dy += y;
                }
                MOut::Wheel(v, h) => {
                    wv += v;
                    wh += h;
                }
                MOut::Button(b, p) => btn.push((b, p)),
            }
        }
        for r in self.mouse.feed(dx, dy, (wv, wh), &btn) {
            self.report(r);
        }
    }

    /// absolute pointer (laser, pen in tablet mode): 0..1, buttons bit 0 = tip / left
    pub fn abs(&mut self, x: f32, y: f32, buttons: u8) -> bool {
        if !self.abs {
            return false;
        }
        self.report(hid::abs_report(buttons, x, y));
        true
    }

    pub fn release_all(&mut self) -> usize {
        let mut v = self.keys.release_all();
        v.extend(self.mouse.release_all());
        // the recognizer's own state is reset (its button releases are covered by the mouse state above)
        let _ = self.rec.release_all();
        let n = v.len();
        for r in v {
            self.report(r);
        }
        self.flush();
        n
    }

    /// something readable on one of `poll_fds()`
    pub fn readable(&mut self, fd: i32) -> Vec<Event> {
        let mut ev = vec![];
        if fd == self.helper {
            match ipc::recv(self.helper) {
                Some((m, fds)) => {
                    if let Some(h) = m.strip_prefix("BT-CONNECTED host=") {
                        for f in &self.fds {
                            ipc::close(*f);
                        }
                        for f in &fds {
                            ipc::set_nonblock(*f);
                        }
                        self.fds = fds;
                        self.host = Some(format!("bt:{}", h.trim()));
                        self.keys = KeyState::default();
                        self.mouse = MouseState::default();
                        ev.push(Event::Connected(self.host.clone().unwrap()));
                    } else {
                        for f in fds {
                            ipc::close(f);
                        }
                        ev.push(Event::Note(format!("helper: {}", m)));
                    }
                }
                None => ev.push(Event::HelperGone),
            }
            return ev;
        }
        let mut b = [0u8; 64];
        let idx = self.fds.iter().position(|x| *x == fd);
        match ipc::read_fd(fd, &mut b) {
            Ok(0) | Err(_) if self.kind == Kind::Bt => {
                let h = self.host.take().unwrap_or_default();
                for f in std::mem::take(&mut self.fds) {
                    ipc::close(f);
                }
                self.queue.clear();
                ev.push(Event::Disconnected(h));
            }
            Ok(n) if n > 0 => match (self.kind, idx) {
                (Kind::Usb, Some(0)) => {
                    if let Some((c, nl)) = hid::leds_usb(&b[..n]) {
                        ev.push(Event::Leds(c, nl));
                    }
                }
                (Kind::Bt, Some(0)) => {
                    let r = hid::bt_control(&b[..n], &self.keys);
                    if let Some(rep) = r.reply {
                        let _ = ipc::write_fd(fd, &rep);
                    }
                    if let Some((c, nl)) = r.leds {
                        ev.push(Event::Leds(c, nl));
                    }
                    if r.unplug {
                        ev.push(Event::Note("host: virtual cable unplug".into()));
                    }
                }
                (Kind::Bt, Some(1)) => {
                    if let Some((c, nl)) = hid::leds_bt(&b[..n]) {
                        ev.push(Event::Leds(c, nl));
                    }
                }
                _ => {}
            },
            _ => {}
        }
        ev
    }

    /// orderly end: everything released, the helper takes the mode down, fds closed
    pub fn close(mut self) -> String {
        let n = self.release_all();
        let msg = match self.kind {
            Kind::Usb => "USB-DOWN",
            Kind::Bt => "BT-DOWN",
        };
        let r = request(self.helper, msg).map(|x| x.0).unwrap_or_else(|e| format!("ERR {}", e));
        for f in &self.fds {
            ipc::close(*f);
        }
        ipc::close(self.helper);
        format!("released_reports={} helper: {}", n, r)
    }
}
