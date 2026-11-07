// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! M3: `dexhid hid-helper` - the small privileged helper of the USB / Bluetooth HID modes (root: configfs, the UDC,
//! privileged L2CAP PSMs, BlueZ profile registration). The user daemon connects over a unix seqpacket socket
//! (`/run/dexhid/hid.sock`, systemd socket unit or started by hand); the helper checks the peer (root, --allow-uid,
//! or the active seat0 user), does the setup and hands back open file descriptors (SCM_RIGHTS): the /dev/hidgN
//! nodes for USB, the control + interrupt L2CAP channels for Bluetooth. The daemon writes the reports itself.
//! The helper keeps duplicates: when the daemon goes away (EOF: exit, crash, SIGKILL) it sends "everything
//! released" reports, unbinds and removes the gadget, unregisters the profile, restores the adapter and closes the
//! channels. One client at a time.
//!
//! Protocol (one packet per message):
//!   HELLO | STATUS | USB-UP abs=0|1 stop_other=0|1 udc=auto | USB-DOWN | BT-UP abs=0|1 discoverable=S | BT-DOWN
//!   -> "OK ..." / "ERR ..."; async: "BT-CONNECTED host=<tag>" (+ control, interrupt fds), "BT-WAITING"

use crate::ipc;
use crate::sys::*;
use dexhid_core::{gadget, hid};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};

struct Usb {
    fds: Vec<i32>,
    abs: bool,
    stopped: Vec<(String, String)>,
}

struct Bt {
    conn: zbus::blocking::Connection,
    intr_listen: i32,
    abs: bool,
    /// device tag -> control fd (from BlueZ NewConnection), waiting for its interrupt channel
    ctrl: HashMap<String, i32>,
    /// connected pairs (ctrl, intr) the daemon got (duplicates kept for the release reports)
    pairs: Vec<(i32, i32)>,
    adapter: String,
    restore: Vec<(&'static str, zbus::zvariant::OwnedValue)>,
    rx: Receiver<(String, i32)>,
    wake: i32,
}

struct H {
    root: PathBuf,
    udc_dir: PathBuf,
    role_dir: PathBuf,
    allow_uid: Option<u32>,
    usb: Option<Usb>,
    bt: Option<Bt>,
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
    let sock = arg(a, "--socket", "/run/dexhid/hid.sock");
    let mut h = H {
        root: PathBuf::from(arg(a, "--configfs", "/sys/kernel/config/usb_gadget")),
        udc_dir: PathBuf::from(arg(a, "--udc-dir", "/sys/class/udc")),
        role_dir: PathBuf::from(arg(a, "--role-dir", "/sys/class/usb_role")),
        allow_uid: a.val("--allow-uid").and_then(|v| v.parse().ok()),
        usb: None,
        bt: None,
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
    while !stopped() {
        let bt_fds: Vec<i32> = h.bt.as_ref().map(|b| vec![b.intr_listen, b.wake]).unwrap_or_default();
        let mut fds = vec![lfd, client.unwrap_or(-1)];
        fds.extend(&bt_fds);
        let r = poll_in(&fds, 1000);
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
        if !bt_fds.is_empty() && (r[2] || r[3]) {
            if let Some(m) = h.bt_events() {
                if let Some(c) = client {
                    let _ = ipc::send(c, &m.0, &m.1);
                }
            }
        }
    }
    h.teardown(&stop_reason());
    if let Some(c) = client {
        ipc::close(c);
    }
    let _ = std::fs::remove_file(&sock);
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
            "BT-UP" => match self.bt_up(kv(m, "abs").as_deref() == Some("1"), kv(m, "discoverable").and_then(|v| v.parse().ok()).unwrap_or(120)) {
                Ok(t) => (format!("OK bt {}", t), vec![]),
                Err(e) => {
                    self.bt_down();
                    (format!("ERR {}", e), vec![])
                }
            },
            "BT-DOWN" => {
                self.bt_down();
                ("OK bt down".into(), vec![])
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
            "bt_up": self.bt.is_some(),
            "bt_connected": self.bt.as_ref().map(|b| b.pairs.len()).unwrap_or(0),
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

    // ------------------------------------------------------------------------------------------------ Bluetooth

    fn bt_up(&mut self, abs: bool, discoverable: u32) -> Result<String, String> {
        if self.bt.is_some() {
            return Err("Bluetooth HID is up already".into());
        }
        // the interrupt channel is ours; BlueZ hands the control channel over (Profile1.NewConnection)
        let intr = l2cap_listen(hid::PSM_INTERRUPT).map_err(|e| {
            if e.contains("in use") {
                format!("PSM 19 is in use: BlueZ's input plugin listens on the HID PSMs (17 / 19) as a host. Bluetooth HID device mode needs bluetoothd started without it (--noplugin=input, an owner step). ({})", e)
            } else {
                e
            }
        })?;
        let conn = zbus::blocking::Connection::system().map_err(|e| format!("system bus: {}", e))?;
        let (tx, rx) = channel();
        let (wake_rd, wake_wr) = wake_pipe();
        let prof = BtProfile { tx, wake: wake_wr };
        conn.object_server().at(PROFILE_PATH, prof).map_err(|e| format!("profile object: {}", e))?;
        let desc = hid::bt_descriptor(abs);
        let mut opts: HashMap<&str, zbus::zvariant::Value> = HashMap::new();
        opts.insert("Name", "DexHid keyboard + mouse".into());
        opts.insert("Role", "server".into());
        opts.insert("PSM", zbus::zvariant::Value::U16(hid::PSM_CONTROL));
        opts.insert("RequireAuthentication", true.into());
        opts.insert("RequireAuthorization", false.into());
        opts.insert("AutoConnect", false.into());
        opts.insert("ServiceRecord", hid::sdp_record(&desc).into());
        let path = zbus::zvariant::ObjectPath::try_from(PROFILE_PATH).unwrap();
        conn.call_method(Some("org.bluez"), "/org/bluez", Some("org.bluez.ProfileManager1"), "RegisterProfile", &(path, hid::BT_HID_UUID, opts))
            .map_err(|e| format!("RegisterProfile: {}", e))?;
        println!("HIDHELPER bt profile registered uuid={} record_bytes={} descriptor_bytes={}", hid::BT_HID_UUID, hid::sdp_record(&desc).len(), desc.len());
        let adapter = "/org/bluez/hci0".to_string();
        let mut restore = vec![];
        for (p, v) in [("Pairable", zbus::zvariant::Value::Bool(true)), ("DiscoverableTimeout", zbus::zvariant::Value::U32(discoverable)), ("Discoverable", zbus::zvariant::Value::Bool(true))] {
            if let Some(old) = prop_get(&conn, &adapter, p) {
                restore.push((p, old));
            }
            if let Err(e) = prop_set(&conn, &adapter, p, v) {
                println!("HIDHELPER WARN adapter {}: {}", p, e);
            }
        }
        self.bt = Some(Bt { conn, intr_listen: intr, abs, ctrl: HashMap::new(), pairs: vec![], adapter, restore, rx, wake: wake_rd });
        Ok(format!("listening psm_control=17(bluez) psm_interrupt=19 discoverable_s={} abs={}", discoverable, abs as u8))
    }

    /// new channels: the control fd from BlueZ, the interrupt connection on our socket; a complete pair goes to
    /// the daemon
    fn bt_events(&mut self) -> Option<(String, Vec<i32>)> {
        let b = self.bt.as_mut()?;
        drain(b.wake);
        while let Ok((tag, fd)) = b.rx.try_recv() {
            println!("HIDHELPER bt control channel host={}", tag);
            b.ctrl.insert(tag, fd);
        }
        let mut sa = [0u8; 16];
        let mut l = sa.len() as libc::socklen_t;
        let c = unsafe { libc::accept4(b.intr_listen, sa.as_mut_ptr() as *mut libc::sockaddr, &mut l, libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK) };
        if c >= 0 {
            // sockaddr_l2: family(2) psm(2) bdaddr(6, little endian)
            let a: Vec<String> = sa[4..10].iter().rev().map(|x| format!("{:02X}", x)).collect();
            let tag = ipc::host_tag(&format!("dev_{}", a.join("_")));
            println!("HIDHELPER bt interrupt channel host={}", tag);
            let ctrl = b.ctrl.remove(&tag).or_else(|| {
                let k = b.ctrl.keys().next().cloned()?;
                b.ctrl.remove(&k)
            });
            match ctrl {
                Some(cf) => {
                    b.pairs.push((unsafe { libc::dup(cf) }, unsafe { libc::dup(c) }));
                    return Some((format!("BT-CONNECTED host={}", tag), vec![cf, c]));
                }
                None => {
                    println!("HIDHELPER WARN interrupt channel without a control channel: closed");
                    ipc::close(c);
                }
            }
        }
        None
    }

    fn bt_down(&mut self) {
        let Some(b) = self.bt.take() else { return };
        for (cf, inf) in &b.pairs {
            for r in hid::all_released(b.abs) {
                if let Some(f) = hid::encode_bt(&r, b.abs) {
                    let _ = ipc::write_fd(*inf, &f);
                }
            }
            ipc::close(*inf);
            ipc::close(*cf);
        }
        for (_, f) in b.ctrl {
            ipc::close(f);
        }
        ipc::close(b.intr_listen);
        let path = zbus::zvariant::ObjectPath::try_from(PROFILE_PATH).unwrap();
        match b.conn.call_method(Some("org.bluez"), "/org/bluez", Some("org.bluez.ProfileManager1"), "UnregisterProfile", &(path,)) {
            Ok(_) => println!("HIDHELPER bt profile unregistered"),
            Err(e) => println!("HIDHELPER WARN UnregisterProfile: {}", e),
        }
        for (p, v) in b.restore.into_iter().rev() {
            let v = zbus::zvariant::Value::from(v);
            match prop_set(&b.conn, &b.adapter, p, v) {
                Ok(()) => println!("HIDHELPER adapter {} restored", p),
                Err(e) => println!("HIDHELPER WARN adapter {} not restored: {}", p, e),
            }
        }
        let _ = b.conn.object_server().remove::<BtProfile, _>(PROFILE_PATH);
        println!("HIDHELPER bt down: released reports sent, channels closed, adapter restored");
    }

    fn teardown(&mut self, why: &str) {
        if self.usb.is_some() || self.bt.is_some() {
            println!("HIDHELPER teardown ({})", why);
        }
        self.usb_down();
        self.bt_down();
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

const PROFILE_PATH: &str = "/org/dexhid/hidprofile";

struct BtProfile {
    tx: Sender<(String, i32)>,
    wake: i32,
}

#[zbus::interface(name = "org.bluez.Profile1")]
impl BtProfile {
    fn release(&self) {
        println!("HIDHELPER bt profile released by BlueZ");
    }
    fn new_connection(&self, device: zbus::zvariant::OwnedObjectPath, fd: zbus::zvariant::OwnedFd, _props: HashMap<String, zbus::zvariant::OwnedValue>) {
        // the device path carries the address: only a tag of it is used (and never logged)
        let tag = ipc::host_tag(device.as_str().rsplit('/').next().unwrap_or(""));
        let raw = std::os::fd::IntoRawFd::into_raw_fd(std::os::fd::OwnedFd::from(fd));
        let _ = self.tx.send((tag, raw));
        poke(self.wake);
    }
    fn request_disconnection(&self, _device: zbus::zvariant::OwnedObjectPath) {
        println!("HIDHELPER bt disconnection requested");
    }
}

fn prop_get(c: &zbus::blocking::Connection, path: &str, p: &str) -> Option<zbus::zvariant::OwnedValue> {
    let m = c.call_method(Some("org.bluez"), path, Some("org.freedesktop.DBus.Properties"), "Get", &("org.bluez.Adapter1", p)).ok()?;
    m.body().deserialize::<zbus::zvariant::OwnedValue>().ok()
}

fn prop_set(c: &zbus::blocking::Connection, path: &str, p: &str, v: zbus::zvariant::Value) -> Result<(), String> {
    c.call_method(Some("org.bluez"), path, Some("org.freedesktop.DBus.Properties"), "Set", &("org.bluez.Adapter1", p, v)).map(|_| ()).map_err(|e| e.to_string())
}

/// a listening L2CAP seqpacket socket on a PSM (any adapter), encryption required (BT_SECURITY medium)
fn l2cap_listen(psm: u16) -> Result<i32, String> {
    const AF_BLUETOOTH: i32 = 31;
    const BTPROTO_L2CAP: i32 = 0;
    const SOL_BLUETOOTH: i32 = 274;
    const BT_SECURITY: i32 = 4;
    let fd = unsafe { libc::socket(AF_BLUETOOTH, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, BTPROTO_L2CAP) };
    if fd < 0 {
        return Err(format!("L2CAP socket: {} (no Bluetooth in the kernel?)", ipc::errstr()));
    }
    // struct sockaddr_l2 { sa_family_t l2_family; __le16 l2_psm; bdaddr_t l2_bdaddr; __le16 l2_cid; __u8 l2_bdaddr_type; }
    let mut sa = [0u8; 14];
    sa[0..2].copy_from_slice(&(AF_BLUETOOTH as u16).to_ne_bytes());
    sa[2..4].copy_from_slice(&psm.to_le_bytes());
    if unsafe { libc::bind(fd, sa.as_ptr() as *const libc::sockaddr, sa.len() as u32) } < 0 {
        let e = crate::sys::errno();
        ipc::close(fd);
        return Err(format!("bind PSM {}: {}{}", psm, crate::sys::strerr(e), if e == libc::EADDRINUSE { " (in use)" } else { "" }));
    }
    let sec: [u8; 2] = [2, 0];
    unsafe { libc::setsockopt(fd, SOL_BLUETOOTH, BT_SECURITY, sec.as_ptr() as *const libc::c_void, 2) };
    if unsafe { libc::listen(fd, 2) } < 0 {
        let e = ipc::errstr();
        ipc::close(fd);
        return Err(format!("listen PSM {}: {}", psm, e));
    }
    Ok(fd)
}
