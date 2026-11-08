// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! M3: `dexhid hid-helper` - the small privileged helper of the USB HID mode (root: configfs, the UDC). The user
//! daemon connects over a unix seqpacket socket (`/run/dexhid/hid.sock`, systemd socket unit or started by hand);
//! the helper checks the peer (root, --allow-uid, or the active seat0 user), does the setup and hands back open file
//! descriptors (SCM_RIGHTS): the /dev/hidgN nodes. The daemon writes the reports itself.
//! The helper keeps duplicates: when the daemon goes away (EOF: exit, crash, SIGKILL) it sends "everything
//! released" reports, unbinds and removes the gadget. One client at a time.
//! (The Bluetooth HID mode was removed by the owner's decision of 7 Nov 2026: it conflicts with the BlueZ input
//! plugin the Bluetooth keyboard cover needs.)
//!
//! Protocol (one packet per message):
//!   HELLO | STATUS | USB-UP abs=0|1 stop_other=0|1 udc=auto | USB-DOWN
//!   -> "OK ..." / "ERR ..."

use crate::ipc;
use crate::sys::*;
use dexhid_core::{gadget, hid};
use std::path::{Path, PathBuf};

struct Usb {
    fds: Vec<i32>,
    abs: bool,
    stopped: Vec<(String, String)>,
}

struct H {
    root: PathBuf,
    udc_dir: PathBuf,
    role_dir: PathBuf,
    allow_uid: Option<u32>,
    usb: Option<Usb>,
}

fn arg(a: &crate::debug::Args, k: &str, d: &str) -> String {
    a.val(k).unwrap_or_else(|| d.to_string())
}

fn kv(line: &str, k: &str) -> Option<String> {
    line.split_whitespace().find_map(|w| w.strip_prefix(&format!("{}=", k)).map(|s| s.to_string()))
}

/// the uid of the active seat0 session (systemd's runtime file)
fn seat_uid() -> Option<u32> {
    std::fs::read_to_string("/run/systemd/seats/seat0").ok()?.lines().find_map(|l| l.strip_prefix("ACTIVE_UID=")).and_then(|v| v.trim().parse().ok())
}

pub fn run(a: &crate::debug::Args) -> i32 {
    install_signals();
    let (sig_rd, sig_wr) = wake_pipe();
    WAKE_FD.store(sig_wr, std::sync::atomic::Ordering::SeqCst);
    let sock = arg(a, "--socket", "/run/dexhid/hid.sock");
    let mut h = H {
        root: PathBuf::from(arg(a, "--configfs", "/sys/kernel/config/usb_gadget")),
        udc_dir: PathBuf::from(arg(a, "--udc-dir", "/sys/class/udc")),
        role_dir: PathBuf::from(arg(a, "--role-dir", "/sys/class/usb_role")),
        allow_uid: a.val("--allow-uid").and_then(|v| v.parse().ok()),
        usb: None,
    };
    // systemd socket activation (LISTEN_FDS=1 -> fd 3) or our own socket
    let lfd = if std::env::var("LISTEN_FDS").as_deref() == Ok("1") { 3 } else {
        match ipc::listen(&sock) {
            Ok(f) => f,
            Err(e) => {
                println!("HIDHELPER ERROR {}", e);
                return 1;
            }
        }
    };
    println!("HIDHELPER listening socket={} configfs={} pid={}", sock, h.root.display(), std::process::id());
    // a gadget left by a crashed helper goes first
    if h.root.join(gadget::GADGET).exists() {
        let r = gadget::remove(&h.root, h.configfs());
        println!("HIDHELPER left-over gadget removed: {:?}", r.map(|v| v.len()));
    }
    let mut client: Option<i32> = None;
    // M4: socket-activated, the helper leaves after this long without a client (systemd starts it again on the next
    // connect; the listening socket stays with systemd); its own socket (tests): it stays
    let activated = std::env::var("LISTEN_FDS").as_deref() == Ok("1");
    let idle_exit_ms: i32 = a.val("--idle-exit-ms").and_then(|v| v.parse().ok()).unwrap_or(if activated { 60_000 } else { -1 });
    while !stopped() {
        let fds = vec![lfd, client.unwrap_or(-1), sig_rd];
        // M4: no timer while a client is connected or the helper owns its socket (the self-pipe ends poll on a
        // signal; the client's hang-up is an event); one timeout to leave when idle
        let timeout = if client.is_none() { idle_exit_ms } else { -1 };
        let r = poll_in(&fds, timeout);
        if r[2] {
            drain(sig_rd);
        }
        if timeout >= 0 && client.is_none() && !r.iter().any(|x| *x) && !stopped() {
            println!("HIDHELPER idle {} ms without a client: exit (socket activation brings it back)", timeout);
            break;
        }
        if r[0] {
            if let Some(c) = ipc::accept(lfd) {
                let p = ipc::peer(c);
                let ok = p.is_some_and(|(_, uid)| uid == 0 || Some(uid) == h.allow_uid || Some(uid) == seat_uid());
                if !ok || client.is_some() {
                    let why = if ok { "busy: another client is connected" } else { "not allowed (root, --allow-uid or the active seat0 user)" };
                    let _ = ipc::send(c, &format!("ERR {}", why), &[]);
                    println!("HIDHELPER refused pid={:?} uid={:?}: {}", p.map(|x| x.0), p.map(|x| x.1), why);
                    ipc::close(c);
                } else {
                    println!("HIDHELPER client pid={:?} uid={:?}", p.map(|x| x.0), p.map(|x| x.1));
                    client = Some(c);
                }
            }
        }
        if let Some(c) = client {
            if r[1] {
                match ipc::recv(c) {
                    Some((m, fds)) => {
                        for f in fds {
                            ipc::close(f);
                        }
                        let (reply, out_fds) = h.command(m.trim());
                        println!("HIDHELPER {} -> {}", m.trim(), reply.split_whitespace().take(6).collect::<Vec<_>>().join(" "));
                        let _ = ipc::send(c, &reply, &out_fds);
                    }
                    None => {
                        println!("HIDHELPER client gone: teardown");
                        h.teardown("client gone");
                        ipc::close(c);
                        client = None;
                    }
                }
            }
        }
    }
    h.teardown(&if stopped() { stop_reason() } else { "idle exit".into() });
    if let Some(c) = client {
        ipc::close(c);
    }
    // the socket path belongs to systemd when it was handed over (removing it would break the next activation)
    if !activated {
        let _ = std::fs::remove_file(&sock);
    }
    println!("HIDHELPER exit");
    0
}

impl H {
    fn configfs(&self) -> bool {
        self.root.starts_with("/sys/kernel/config")
    }

    fn command(&mut self, m: &str) -> (String, Vec<i32>) {
        let cmd = m.split_whitespace().next().unwrap_or("");
        match cmd {
            "HELLO" => (format!("OK dexhid-hid-helper {}", env!("CARGO_PKG_VERSION")), vec![]),
            "STATUS" => (format!("OK {}", self.status()), vec![]),
            "USB-UP" => match self.usb_up(kv(m, "abs").as_deref() == Some("1"), kv(m, "stop_other").as_deref() == Some("1"), &kv(m, "udc").unwrap_or_else(|| "auto".into())) {
                Ok((t, f)) => (format!("OK usb {}", t), f),
                Err(e) => {
                    self.usb_down();
                    (format!("ERR {}", e), vec![])
                }
            },
            "USB-DOWN" => {
                let n = self.usb_down();
                (format!("OK usb down ({} steps)", n), vec![])
            }
            _ => (format!("ERR unknown command {:?}", cmd), vec![]),
        }
    }

    fn status(&self) -> String {
        let udcs = gadget::udcs(&self.udc_dir);
        let roles = gadget::roles(&self.role_dir);
        let pw = gadget::power(Path::new("/sys/class/power_supply"), Path::new("/sys/class/typec"));
        serde_json::json!({
            "configfs": self.root.exists(),
            "udcs": udcs.iter().map(|u| serde_json::json!({"name": u.0, "function": u.1, "state": u.2})).collect::<Vec<_>>(),
            "others_bound": gadget::others_bound(&self.root),
            "roles": roles,
            "role_text": gadget::role_text(&roles).1,
            "power": pw.line(),
            "usb_up": self.usb.is_some(),
        })
        .to_string()
    }

    // ------------------------------------------------------------------------------------------------ USB

    fn usb_up(&mut self, abs: bool, stop_other: bool, udc: &str) -> Result<(String, Vec<i32>), String> {
        if self.usb.is_some() {
            return Err("USB HID is up already".into());
        }
        if !self.root.exists() {
            return Err(format!("{} missing: configfs not mounted or CONFIG_USB_CONFIGFS off", self.root.display()));
        }
        let roles = gadget::roles(&self.role_dir);
        let (role_ok, rt) = gadget::role_text(&roles);
        if !role_ok {
            return Err(rt);
        }
        let list = gadget::udcs(&self.udc_dir);
        let u = gadget::pick_udc(&list, udc)?;
        let mut stopped = vec![];
        let others: Vec<(String, String)> = gadget::others_bound(&self.root).into_iter().filter(|o| o.1 == u).collect();
        if !others.is_empty() {
            if !stop_other {
                return Err(format!("the port is used by gadget {:?}: refused (setting \"Stop another gadget\" lets DexHid stop it and bring it back afterwards)", others.iter().map(|o| &o.0).collect::<Vec<_>>()));
            }
            for (g, udc) in &others {
                std::fs::write(self.root.join(g).join("UDC"), "\n").map_err(|e| format!("stop gadget {}: {}", g, e))?;
                println!("HIDHELPER stopped gadget {} (rebound at teardown)", g);
                stopped.push((g.clone(), udc.clone()));
            }
        } else if let Some(x) = list.iter().find(|x| x.0 == u && !x.1.is_empty()) {
            return Err(format!("UDC {} is bound to {:?} (not a configfs gadget DexHid can stop)", u, x.1));
        }
        self.usb = Some(Usb { fds: vec![], abs, stopped });
        let spec = gadget::spec(abs);
        let log = gadget::create(&self.root, &spec)?;
        println!("HIDHELPER gadget created ({} steps), descriptors kbd={} mouse={} extra={} bytes", log.len(), spec.functions[0].desc.len(), spec.functions[1].desc.len(), spec.functions[2].desc.len());
        gadget::bind(&self.root, &u)?;
        let mut nodes = vec![];
        let mut fds = vec![];
        for f in &spec.functions {
            let n = hidg_node(&self.root.join(gadget::GADGET).join(format!("functions/hid.{}", f.name)))?;
            let c = std::ffi::CString::new(n.clone()).unwrap();
            let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC) };
            if fd < 0 {
                for x in &fds {
                    ipc::close(*x);
                }
                return Err(format!("open {}: {}", n, ipc::errstr()));
            }
            nodes.push(n);
            fds.push(fd);
        }
        let state = std::fs::read_to_string(self.udc_dir.join(&u).join("state")).unwrap_or_default().trim().to_string();
        if let Some(us) = self.usb.as_mut() {
            us.fds = fds.iter().map(|f| unsafe { libc::dup(*f) }).collect();
        }
        Ok((format!("udc={} state={} kbd={} mouse={} extra={} abs={} role={:?}", u, state.replace(' ', "_"), nodes[0], nodes[1], nodes[2], abs as u8, roles.iter().map(|r| r.1.clone()).collect::<Vec<_>>()), fds))
    }

    /// release reports, unbind, remove, rebind stopped gadgets; returns the number of teardown steps
    fn usb_down(&mut self) -> usize {
        let Some(u) = self.usb.take() else {
            return 0;
        };
        for r in hid::all_released(u.abs) {
            if let Some((i, b)) = hid::encode_usb(&r, u.abs) {
                if let Some(&fd) = u.fds.get(i) {
                    let _ = ipc::write_fd(fd, &b);
                }
            }
        }
        for f in &u.fds {
            ipc::close(*f);
        }
        let n = match gadget::remove(&self.root, self.configfs()) {
            Ok(l) => l.len(),
            Err(e) => {
                println!("HIDHELPER WARN gadget removal: {}", e);
                0
            }
        };
        for (g, udc) in u.stopped {
            match std::fs::write(self.root.join(&g).join("UDC"), format!("{}\n", udc)) {
                Ok(()) => println!("HIDHELPER gadget {} bound to {} again", g, udc),
                Err(e) => println!("HIDHELPER WARN rebind {}: {}", g, e),
            }
        }
        println!("HIDHELPER usb down: released reports sent, gadget removed ({} steps)", n);
        n
    }

    fn teardown(&mut self, why: &str) {
        if self.usb.is_some() {
            println!("HIDHELPER teardown ({})", why);
        }
        self.usb_down();
    }
}

/// /dev/hidgN of a function (its `dev` attribute M:m -> /sys/dev/char/M:m/uevent DEVNAME); waits for udev
fn hidg_node(func: &Path) -> Result<String, String> {
    let dev = std::fs::read_to_string(func.join("dev")).map_err(|e| format!("{}/dev: {} (no hidg node: the function is not bound)", func.display(), e))?;
    let dev = dev.trim();
    let ue = std::fs::read_to_string(format!("/sys/dev/char/{}/uevent", dev)).map_err(|e| format!("/sys/dev/char/{}: {}", dev, e))?;
    let name = ue.lines().find_map(|l| l.strip_prefix("DEVNAME=")).ok_or("no DEVNAME")?;
    let p = format!("/dev/{}", name);
    for _ in 0..100 {
        if Path::new(&p).exists() {
            return Ok(p);
        }
        sleep_ms(20);
    }
    Err(format!("{} did not appear", p))
}
