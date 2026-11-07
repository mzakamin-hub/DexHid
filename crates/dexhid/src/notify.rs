// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! sd_notify (READY / WATCHDOG / STOPPING / STATUS) without libsystemd, and the kernel uevent socket.

use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};

pub struct Notify {
    sock: Option<(UnixDatagram, SocketAddr)>,
    /// watchdog interval from WATCHDOG_USEC (only when WATCHDOG_PID is ours or unset)
    pub watchdog_ms: Option<i64>,
}

impl Notify {
    pub fn from_env() -> Notify {
        let mut sock = None;
        if let Ok(p) = std::env::var("NOTIFY_SOCKET") {
            let addr = if let Some(a) = p.strip_prefix('@') {
                SocketAddr::from_abstract_name(a.as_bytes()).ok()
            } else {
                SocketAddr::from_pathname(&p).ok()
            };
            if let (Some(addr), Ok(s)) = (addr, UnixDatagram::unbound()) {
                sock = Some((s, addr));
            }
        }
        let pid_ok = std::env::var("WATCHDOG_PID").map(|p| p.parse::<u32>().ok() == Some(std::process::id())).unwrap_or(true);
        let watchdog_ms = std::env::var("WATCHDOG_USEC").ok().and_then(|v| v.parse::<i64>().ok()).filter(|_| pid_ok).map(|us| us / 1000);
        Notify { sock, watchdog_ms }
    }
    pub fn send(&self, msg: &str) {
        if let Some((s, a)) = &self.sock {
            let _ = s.send_to_addr(msg.as_bytes(), a);
        }
    }
}

/// NETLINK_KOBJECT_UEVENT socket on the kernel group (no libudev): non-blocking fd
pub fn uevent_socket() -> Result<i32, String> {
    unsafe {
        let fd = libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, libc::NETLINK_KOBJECT_UEVENT);
        if fd < 0 {
            return Err(format!("netlink socket: {}", std::io::Error::last_os_error()));
        }
        let mut sa: libc::sockaddr_nl = std::mem::zeroed();
        sa.nl_family = libc::AF_NETLINK as u16;
        sa.nl_groups = 1;
        let r = libc::bind(fd, &sa as *const libc::sockaddr_nl as *const libc::sockaddr, std::mem::size_of::<libc::sockaddr_nl>() as u32);
        if r < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(format!("netlink bind: {}", e));
        }
        Ok(fd)
    }
}

/// read all pending uevents
pub fn read_uevents(fd: i32) -> Vec<dexhid_core::hotplug::Uevent> {
    let mut out = vec![];
    let mut buf = vec![0u8; 16384];
    loop {
        let r = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if r <= 0 {
            break;
        }
        if let Some(u) = dexhid_core::hotplug::parse_uevent(&buf[..r as usize]) {
            out.push(u);
        }
    }
    out
}
