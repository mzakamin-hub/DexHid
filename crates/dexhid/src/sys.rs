// SPDX-License-Identifier: GPL-3.0-or-later
//! Raw evdev / uinput access through libc ioctls (no evdev crate: fewer dependencies, exact control).
//! Taken over from the M0 spike (proven on the tablet 7 Nov 2026: grab, EVIOCGMTSLOTS, uinput MT devices).
#![allow(dead_code)]

use std::ffi::CString;
use std::os::raw::{c_int, c_ulong};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_ABS: u16 = 0x03;
pub const EV_REL: u16 = 0x02;
pub const EV_SW: u16 = 0x05;
pub const EV_LED: u16 = 0x11;
pub const LED_NUML: u16 = 0x00;
pub const LED_CAPSL: u16 = 0x01;
pub const LED_SCROLLL: u16 = 0x02;
pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_HWHEEL: u16 = 0x06;
pub const REL_WHEEL: u16 = 0x08;
pub const REL_WHEEL_HI_RES: u16 = 0x0b;
pub const REL_HWHEEL_HI_RES: u16 = 0x0c;
pub const BUS_BLUETOOTH: u16 = 0x05;
pub const SYN_REPORT: u16 = 0;
pub const SYN_DROPPED: u16 = 3;

pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_MT_SLOT: u16 = 0x2f;
pub const ABS_MT_TOUCH_MAJOR: u16 = 0x30;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;
pub const ABS_MT_PRESSURE: u16 = 0x3a;

pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_VOLUMEDOWN: u16 = 114;
pub const KEY_VOLUMEUP: u16 = 115;
pub const KEY_POWER: u16 = 116;
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;
pub const BTN_TOOL_PEN: u16 = 0x140;
pub const BTN_TOOL_FINGER: u16 = 0x145;
pub const BTN_TOOL_QUINTTAP: u16 = 0x148;
pub const BTN_TOUCH: u16 = 0x14a;
pub const BTN_TOOL_DOUBLETAP: u16 = 0x14d;
pub const BTN_TOOL_TRIPLETAP: u16 = 0x14e;
pub const BTN_TOOL_QUADTAP: u16 = 0x14f;
pub const SW_LID: u16 = 0x00;

pub const INPUT_PROP_POINTER: u16 = 0;
pub const INPUT_PROP_DIRECT: u16 = 1;
pub const INPUT_PROP_BUTTONPAD: u16 = 2;
pub const BUS_VIRTUAL: u16 = 0x06;

pub const KEY_CNT: usize = 0x300;
pub const ABS_CNT: usize = 0x40;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct InputEvent {
    pub sec: i64,
    pub usec: i64,
    pub type_: u16,
    pub code: u16,
    pub value: i32,
}
impl InputEvent {
    pub fn ns(&self) -> i64 {
        self.sec * 1_000_000_000 + self.usec * 1000
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct AbsInfo {
    pub value: i32,
    pub minimum: i32,
    pub maximum: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    absinfo: AbsInfo,
}

// generic (arm64 / x86) _IOC encoding
const IOC_WRITE: c_ulong = 1;
const IOC_READ: c_ulong = 2;
const fn ioc(dir: c_ulong, ty: u8, nr: c_ulong, size: usize) -> c_ulong {
    (dir << 30) | ((size as c_ulong) << 16) | ((ty as c_ulong) << 8) | nr
}
fn eviocgname(len: usize) -> c_ulong { ioc(IOC_READ, b'E', 0x06, len) }
fn eviocgprop(len: usize) -> c_ulong { ioc(IOC_READ, b'E', 0x09, len) }
fn eviocgmtslots(len: usize) -> c_ulong { ioc(IOC_READ, b'E', 0x0a, len) }
fn eviocgkey(len: usize) -> c_ulong { ioc(IOC_READ, b'E', 0x18, len) }
fn eviocgbit(ev: u16, len: usize) -> c_ulong { ioc(IOC_READ, b'E', 0x20 + ev as c_ulong, len) }
fn eviocgabs(abs: u16) -> c_ulong { ioc(IOC_READ, b'E', 0x40 + abs as c_ulong, std::mem::size_of::<AbsInfo>()) }
const EVIOCGID: c_ulong = ioc(IOC_READ, b'E', 0x02, 8);
const EVIOCGRAB: c_ulong = ioc(IOC_WRITE, b'E', 0x90, 4);
const EVIOCSCLOCKID: c_ulong = ioc(IOC_WRITE, b'E', 0xa0, 4);

const UI_DEV_CREATE: c_ulong = ioc(0, b'U', 1, 0);
const UI_DEV_DESTROY: c_ulong = ioc(0, b'U', 2, 0);
const UI_DEV_SETUP: c_ulong = ioc(IOC_WRITE, b'U', 3, std::mem::size_of::<UinputSetup>());
const UI_ABS_SETUP: c_ulong = ioc(IOC_WRITE, b'U', 4, std::mem::size_of::<UinputAbsSetup>());
const UI_SET_EVBIT: c_ulong = ioc(IOC_WRITE, b'U', 100, 4);
const UI_SET_KEYBIT: c_ulong = ioc(IOC_WRITE, b'U', 101, 4);
const UI_SET_RELBIT: c_ulong = ioc(IOC_WRITE, b'U', 102, 4);
const UI_SET_ABSBIT: c_ulong = ioc(IOC_WRITE, b'U', 103, 4);
const UI_SET_LEDBIT: c_ulong = ioc(IOC_WRITE, b'U', 105, 4);
const UI_SET_PROPBIT: c_ulong = ioc(IOC_WRITE, b'U', 110, 4);
fn ui_get_sysname(len: usize) -> c_ulong { ioc(IOC_READ, b'U', 44, len) }

pub fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}
pub fn strerr(e: i32) -> String {
    std::io::Error::from_raw_os_error(e).to_string()
}

// ---------- time / signals ----------
pub fn now_clk(clk: i32) -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(clk, &mut ts) };
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}
pub fn mono_ns() -> i64 { now_clk(libc::CLOCK_MONOTONIC) }
pub fn mono_ms() -> i64 { mono_ns() / 1_000_000 }
pub fn sleep_ms(ms: u64) { std::thread::sleep(Duration::from_millis(ms)) }

pub static STOP: AtomicBool = AtomicBool::new(false);
pub static STOP_SIG: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
/// write end of the daemon's wake pipe (-1 = none); the signal handler pokes it (write is async-signal-safe)
pub static WAKE_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
extern "C" fn on_sig(s: c_int) {
    STOP_SIG.store(s, Ordering::SeqCst);
    STOP.store(true, Ordering::SeqCst);
    let fd = WAKE_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        let b = [b'S'];
        unsafe { libc::write(fd, b.as_ptr() as *const libc::c_void, 1) };
    }
}

/// a non-blocking pipe: (read end, write end)
pub fn wake_pipe() -> (c_int, c_int) {
    let mut fds = [-1 as c_int; 2];
    unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
    (fds[0], fds[1])
}
pub fn poke(fd: c_int) {
    let b = [1u8];
    unsafe { libc::write(fd, b.as_ptr() as *const libc::c_void, 1) };
}
pub fn drain(fd: c_int) {
    let mut b = [0u8; 256];
    while unsafe { libc::read(fd, b.as_mut_ptr() as *mut libc::c_void, b.len()) } > 0 {}
}
pub fn install_signals() {
    unsafe {
        let h = on_sig as extern "C" fn(c_int) as libc::sighandler_t;
        libc::signal(libc::SIGTERM, h);
        libc::signal(libc::SIGINT, h);
        libc::signal(libc::SIGHUP, h);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}
pub fn stopped() -> bool { STOP.load(Ordering::SeqCst) }
pub fn stop_reason() -> String {
    match STOP_SIG.load(Ordering::SeqCst) {
        libc::SIGTERM => "sigterm".into(),
        libc::SIGINT => "sigint".into(),
        libc::SIGHUP => "sighup".into(),
        0 => "none".into(),
        s => format!("signal{}", s),
    }
}

pub fn drop_privs(uid: u32, gid: u32) -> Result<(), String> {
    unsafe {
        if libc::setgroups(0, std::ptr::null()) != 0 { return Err(format!("setgroups: {}", strerr(errno()))); }
        if libc::setresgid(gid, gid, gid) != 0 { return Err(format!("setresgid: {}", strerr(errno()))); }
        if libc::setresuid(uid, uid, uid) != 0 { return Err(format!("setresuid: {}", strerr(errno()))); }
        if libc::getuid() != uid || libc::geteuid() != uid { return Err("uid check failed".into()); }
    }
    Ok(())
}

/// alive and not a zombie (an exited but unreaped probe must count as gone)
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 { return false; }
    match std::fs::read_to_string(format!("/proc/{}/stat", pid)) {
        Ok(s) => match s.rfind(')') {
            Some(i) => !matches!(s[i + 1..].trim_start().chars().next(), Some('Z') | Some('X') | None),
            None => false,
        },
        Err(_) => false,
    }
}

// ---------- evdev ----------
pub struct Dev {
    pub fd: c_int,
    pub path: String,
}
impl Drop for Dev {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

pub fn test_bit(bits: &[u8], n: usize) -> bool {
    n / 8 < bits.len() && bits[n / 8] & (1 << (n % 8)) != 0
}

impl Dev {
    pub fn open(path: &str, write: bool) -> Result<Dev, i32> {
        let c = CString::new(path).unwrap();
        let fl = (if write { libc::O_RDWR } else { libc::O_RDONLY }) | libc::O_NONBLOCK | libc::O_CLOEXEC;
        let fd = unsafe { libc::open(c.as_ptr(), fl) };
        if fd < 0 { return Err(errno()); }
        Ok(Dev { fd, path: path.to_string() })
    }
    pub fn name(&self) -> String {
        let mut b = [0u8; 256];
        let r = unsafe { libc::ioctl(self.fd, eviocgname(b.len()), b.as_mut_ptr()) };
        if r < 0 { return String::new(); }
        let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        String::from_utf8_lossy(&b[..n]).into_owned()
    }
    pub fn bustype(&self) -> u16 {
        let mut id = InputId::default();
        unsafe { libc::ioctl(self.fd, EVIOCGID, &mut id as *mut InputId) };
        id.bustype
    }
    pub fn bits(&self, ev: u16, nbits: usize) -> Vec<u8> {
        let mut b = vec![0u8; nbits.div_ceil(8)];
        unsafe { libc::ioctl(self.fd, eviocgbit(ev, b.len()), b.as_mut_ptr()) };
        b
    }
    pub fn props(&self) -> Vec<u8> {
        let mut b = vec![0u8; 4];
        unsafe { libc::ioctl(self.fd, eviocgprop(b.len()), b.as_mut_ptr()) };
        b
    }
    pub fn has_key(&self, code: u16) -> bool { test_bit(&self.bits(EV_KEY, KEY_CNT), code as usize) }
    pub fn has_abs(&self, code: u16) -> bool { test_bit(&self.bits(EV_ABS, ABS_CNT), code as usize) }
    pub fn absinfo(&self, code: u16) -> Option<AbsInfo> {
        let mut a = AbsInfo::default();
        let r = unsafe { libc::ioctl(self.fd, eviocgabs(code), &mut a as *mut AbsInfo) };
        if r < 0 { None } else { Some(a) }
    }
    /// EVIOCGRAB: Ok or errno (EBUSY = somebody else holds the grab)
    pub fn grab(&self, on: bool) -> Result<(), i32> {
        let v: c_int = if on { 1 } else { 0 };
        // the kernel takes the int by value for EVIOCGRAB
        let r = unsafe { libc::ioctl(self.fd, EVIOCGRAB, v as libc::c_ulong) };
        if r < 0 { Err(errno()) } else { Ok(()) }
    }
    pub fn set_clock(&self, clk: i32) -> bool {
        let v: c_int = clk;
        unsafe { libc::ioctl(self.fd, EVIOCSCLOCKID, &v as *const c_int) == 0 }
    }
    /// live MT slot values for one ABS_MT code (EVIOCGMTSLOTS)
    pub fn mt_slots(&self, code: u16, nslots: usize) -> Option<Vec<i32>> {
        let mut b = vec![0i32; nslots + 1];
        b[0] = code as i32;
        let r = unsafe { libc::ioctl(self.fd, eviocgmtslots(b.len() * 4), b.as_mut_ptr()) };
        if r < 0 { None } else { Some(b[1..].to_vec()) }
    }
    /// number of active contacts (tracking id != -1), None if not MT
    pub fn active_slots(&self) -> Option<usize> {
        let a = self.absinfo(ABS_MT_SLOT)?;
        if !self.has_abs(ABS_MT_TRACKING_ID) { return None; }
        let n = (a.maximum + 1).max(1) as usize;
        Some(self.mt_slots(ABS_MT_TRACKING_ID, n)?.iter().filter(|&&v| v != -1).count())
    }
    pub fn keys_down(&self) -> Vec<u16> {
        let mut b = vec![0u8; KEY_CNT / 8];
        unsafe { libc::ioctl(self.fd, eviocgkey(b.len()), b.as_mut_ptr()) };
        (0..KEY_CNT).filter(|&k| test_bit(&b, k)).map(|k| k as u16).collect()
    }
    /// read everything available (non-blocking). Err(errno) on ENODEV etc.
    pub fn read_all(&self, out: &mut Vec<InputEvent>) -> Result<(), i32> {
        let mut buf = [InputEvent::default(); 64];
        loop {
            let r = unsafe {
                libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, std::mem::size_of_val(&buf))
            };
            if r < 0 {
                let e = errno();
                if e == libc::EAGAIN || e == libc::EINTR { return Ok(()); }
                return Err(e);
            }
            if r == 0 { return Ok(()); }
            let n = r as usize / std::mem::size_of::<InputEvent>();
            out.extend_from_slice(&buf[..n]);
            if n < buf.len() { return Ok(()); }
        }
    }
}

pub fn event_nodes() -> Vec<String> {
    let mut v: Vec<(u32, String)> = std::fs::read_dir("/dev/input")
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    n.strip_prefix("event").and_then(|s| s.parse::<u32>().ok()).map(|i| (i, format!("/dev/input/{}", n)))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v.into_iter().map(|x| x.1).collect()
}

pub fn key_code(name: &str) -> Option<u16> {
    match name.trim() {
        "KEY_VOLUMEUP" | "VOLUMEUP" | "volup" => Some(KEY_VOLUMEUP),
        "KEY_VOLUMEDOWN" | "VOLUMEDOWN" | "voldown" => Some(KEY_VOLUMEDOWN),
        "KEY_POWER" | "POWER" => Some(KEY_POWER),
        s => s.parse().ok(),
    }
}
pub fn key_name(c: u16) -> String {
    match c {
        KEY_VOLUMEUP => "KEY_VOLUMEUP".into(),
        KEY_VOLUMEDOWN => "KEY_VOLUMEDOWN".into(),
        KEY_POWER => "KEY_POWER".into(),
        KEY_LEFTSHIFT => "KEY_LEFTSHIFT".into(),
        BTN_LEFT => "BTN_LEFT".into(),
        BTN_RIGHT => "BTN_RIGHT".into(),
        BTN_MIDDLE => "BTN_MIDDLE".into(),
        x => format!("{}", x),
    }
}

// ---------- uinput ----------
pub struct Uinput {
    pub fd: c_int,
    pub name: String,
    pub node: String,
    destroyed: bool,
}

pub struct UinputSpec<'a> {
    pub name: &'a str,
    pub keys: Vec<u16>,
    pub abs: Vec<(u16, AbsInfo)>,
    pub props: Vec<u16>,
    /// M2: relative axes, LEDs (written back by the desktop, read from the uinput fd), bus type (0 = virtual)
    pub rels: Vec<u16>,
    pub leds: Vec<u16>,
    pub bus: u16,
}

impl<'a> UinputSpec<'a> {
    pub fn new(name: &'a str) -> UinputSpec<'a> {
        UinputSpec { name, keys: vec![], abs: vec![], props: vec![], rels: vec![], leds: vec![], bus: 0 }
    }
}

fn ioctl_int(fd: c_int, req: c_ulong, v: u16) -> Result<(), String> {
    let r = unsafe { libc::ioctl(fd, req, v as c_ulong) };
    if r < 0 { Err(format!("ioctl {:#x} {}: {}", req, v, strerr(errno()))) } else { Ok(()) }
}

impl Uinput {
    pub fn create(spec: &UinputSpec) -> Result<Uinput, String> {
        let c = CString::new("/dev/uinput").unwrap();
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if fd < 0 { return Err(format!("open /dev/uinput: {}", strerr(errno()))); }
        let mut u = Uinput { fd, name: spec.name.to_string(), node: String::new(), destroyed: false };
        ioctl_int(fd, UI_SET_EVBIT, EV_SYN)?;
        if !spec.keys.is_empty() {
            ioctl_int(fd, UI_SET_EVBIT, EV_KEY)?;
            for &k in &spec.keys { ioctl_int(fd, UI_SET_KEYBIT, k)?; }
        }
        if !spec.abs.is_empty() {
            ioctl_int(fd, UI_SET_EVBIT, EV_ABS)?;
            for (code, ai) in &spec.abs {
                ioctl_int(fd, UI_SET_ABSBIT, *code)?;
                let s = UinputAbsSetup { code: *code, absinfo: *ai };
                let r = unsafe { libc::ioctl(fd, UI_ABS_SETUP, &s as *const UinputAbsSetup) };
                if r < 0 { return Err(format!("UI_ABS_SETUP {}: {}", code, strerr(errno()))); }
            }
        }
        if !spec.rels.is_empty() {
            ioctl_int(fd, UI_SET_EVBIT, EV_REL)?;
            for &r in &spec.rels { ioctl_int(fd, UI_SET_RELBIT, r)?; }
        }
        if !spec.leds.is_empty() {
            ioctl_int(fd, UI_SET_EVBIT, EV_LED)?;
            for &l in &spec.leds { ioctl_int(fd, UI_SET_LEDBIT, l)?; }
        }
        for &p in &spec.props { ioctl_int(fd, UI_SET_PROPBIT, p)?; }
        let mut setup = UinputSetup {
            id: InputId { bustype: if spec.bus == 0 { BUS_VIRTUAL } else { spec.bus }, vendor: 0x1d6b, product: 0xde01, version: 1 },
            name: [0u8; 80],
            ff_effects_max: 0,
        };
        let nb = spec.name.as_bytes();
        setup.name[..nb.len().min(79)].copy_from_slice(&nb[..nb.len().min(79)]);
        let r = unsafe { libc::ioctl(fd, UI_DEV_SETUP, &setup as *const UinputSetup) };
        if r < 0 { return Err(format!("UI_DEV_SETUP: {}", strerr(errno()))); }
        let r = unsafe { libc::ioctl(fd, UI_DEV_CREATE) };
        if r < 0 { return Err(format!("UI_DEV_CREATE: {}", strerr(errno()))); }
        // find the event node
        let mut sb = [0u8; 64];
        let r = unsafe { libc::ioctl(fd, ui_get_sysname(sb.len()), sb.as_mut_ptr()) };
        if r >= 0 {
            let n = sb.iter().position(|&c| c == 0).unwrap_or(0);
            let sys = String::from_utf8_lossy(&sb[..n]).into_owned();
            for _ in 0..100 {
                if let Ok(rd) = std::fs::read_dir(format!("/sys/class/input/{}", sys)) {
                    for e in rd.flatten() {
                        let f = e.file_name().to_string_lossy().into_owned();
                        if f.starts_with("event") {
                            let p = format!("/dev/input/{}", f);
                            if std::path::Path::new(&p).exists() { u.node = p; }
                        }
                    }
                }
                if !u.node.is_empty() { break; }
                sleep_ms(20);
            }
        }
        if u.node.is_empty() { return Err("uinput: event node not found".into()); }
        Ok(u)
    }
    pub fn emit(&self, type_: u16, code: u16, value: i32) {
        let ev = InputEvent { sec: 0, usec: 0, type_, code, value };
        unsafe {
            libc::write(self.fd, &ev as *const InputEvent as *const libc::c_void, std::mem::size_of::<InputEvent>());
        }
    }
    pub fn syn(&self) { self.emit(EV_SYN, SYN_REPORT, 0) }
    /// events the kernel writes back to a uinput device (EV_LED from the desktop), non-blocking
    pub fn read_back(&self) -> Vec<InputEvent> {
        let mut out = vec![];
        let mut buf = [InputEvent::default(); 16];
        loop {
            let r = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, std::mem::size_of_val(&buf)) };
            if r <= 0 { break; }
            let n = r as usize / std::mem::size_of::<InputEvent>();
            out.extend_from_slice(&buf[..n]);
            if n < buf.len() { break; }
        }
        out
    }
    pub fn destroy(&mut self) {
        if !self.destroyed {
            unsafe {
                libc::ioctl(self.fd, UI_DEV_DESTROY);
                libc::close(self.fd);
            }
            self.destroyed = true;
        }
    }
}
impl Drop for Uinput {
    fn drop(&mut self) { self.destroy() }
}

pub fn ai(min: i32, max: i32, res: i32) -> AbsInfo {
    AbsInfo { value: 0, minimum: min, maximum: max, fuzz: 0, flat: 0, resolution: res }
}

// ---------- poll ----------
pub fn poll_in(fds: &[c_int], timeout_ms: i32) -> Vec<bool> {
    let mut p: Vec<libc::pollfd> = fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    let r = unsafe { libc::poll(p.as_mut_ptr(), p.len() as libc::nfds_t, timeout_ms) };
    if r <= 0 { return vec![false; fds.len()]; }
    p.iter().map(|x| x.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0).collect()
}

pub fn pct(v: &mut [i64], p: f64) -> i64 {
    if v.is_empty() { return -1; }
    v.sort_unstable();
    let i = ((v.len() as f64 - 1.0) * p).round() as usize;
    v[i.min(v.len() - 1)]
}
pub fn lat_line(name: &str, v: &mut Vec<i64>) -> String {
    let n = v.len();
    if n == 0 { return format!("LAT name={} n=0", name); }
    let us = |x: i64| x / 1000;
    format!(
        "LAT name={} n={} p50_us={} p95_us={} p99_us={} max_us={} min_us={}",
        name, n, us(pct(v, 0.50)), us(pct(v, 0.95)), us(pct(v, 0.99)), us(pct(v, 1.0)), us(pct(v, 0.0))
    )
}
