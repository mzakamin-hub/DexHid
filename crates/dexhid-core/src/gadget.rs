// SPDX-License-Identifier: GPL-3.0-or-later
//! M3 USB HID mode: the configfs gadget (keyboard + mouse + extra), its teardown, conflict and role checks, and the
//! port's power state. Pure file operations on a root directory: the privileged helper passes
//! /sys/kernel/config/usb_gadget, the unit tests a temporary directory (configfs creates attribute files on mkdir
//! and removes them with the directory; on a plain directory the files are written / deleted explicitly).
//!
//! Layout (Documentation/usb/gadget_configfs.rst):
//!   <root>/dexhid/{idVendor,idProduct,bcdDevice,bcdUSB,strings/0x409/*,configs/c.1/{MaxPower,bmAttributes,strings/0x409/configuration},
//!                  functions/hid.{kbd,mouse,extra}/{protocol,subclass,report_length,report_desc}, UDC}
//!   configs/c.1/hid.<f> -> functions/hid.<f> (symlinks)
//! The serial number is a fixed text, never the tablet's serial.

use crate::hid::UsbFunction;
use std::path::{Path, PathBuf};

pub const GADGET: &str = "dexhid";
/// Linux Foundation / Multifunction Composite Gadget (the IDs the kernel docs use for composite test gadgets)
pub const VENDOR: u16 = 0x1d6b;
pub const PRODUCT: u16 = 0x0104;

#[derive(Clone, Debug)]
pub struct Spec {
    pub functions: Vec<UsbFunction>,
    pub manufacturer: String,
    pub product: String,
    pub serial: String,
    /// the tablet has its own battery: self powered, asks the host for little current
    pub max_power_ma: u32,
}

pub fn spec(abs: bool) -> Spec {
    Spec {
        functions: crate::hid::usb_functions(abs),
        manufacturer: "DexHid".into(),
        product: "DexHid keyboard + mouse".into(),
        serial: "dexhid-0001".into(),
        max_power_ma: 100,
    }
}

fn w(p: &Path, data: &[u8], log: &mut Vec<String>) -> Result<(), String> {
    std::fs::write(p, data).map_err(|e| format!("write {}: {}", p.display(), e))?;
    log.push(format!("write {} ({} bytes)", p.display(), data.len()));
    Ok(())
}

fn mk(p: &Path, log: &mut Vec<String>) -> Result<(), String> {
    match std::fs::create_dir(p) {
        Ok(()) => {
            log.push(format!("mkdir {}", p.display()));
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(format!("mkdir {}: {}", p.display(), e)),
    }
}

/// create the gadget (not bound). Fails if it already exists (a left-over is removed first by the caller).
pub fn create(root: &Path, s: &Spec) -> Result<Vec<String>, String> {
    let g = root.join(GADGET);
    if g.exists() {
        return Err(format!("{} exists already (left over: remove it first)", g.display()));
    }
    let mut log = vec![];
    mk(&g, &mut log)?;
    w(&g.join("idVendor"), format!("{:#06x}\n", VENDOR).as_bytes(), &mut log)?;
    w(&g.join("idProduct"), format!("{:#06x}\n", PRODUCT).as_bytes(), &mut log)?;
    w(&g.join("bcdDevice"), b"0x0100\n", &mut log)?;
    w(&g.join("bcdUSB"), b"0x0200\n", &mut log)?;
    let st = g.join("strings");
    mk(&st, &mut log)?;
    let s409 = st.join("0x409");
    mk(&s409, &mut log)?;
    w(&s409.join("manufacturer"), s.manufacturer.as_bytes(), &mut log)?;
    w(&s409.join("product"), s.product.as_bytes(), &mut log)?;
    w(&s409.join("serialnumber"), s.serial.as_bytes(), &mut log)?;
    let cs = g.join("configs");
    mk(&cs, &mut log)?;
    let c = cs.join("c.1");
    mk(&c, &mut log)?;
    w(&c.join("MaxPower"), format!("{}\n", s.max_power_ma).as_bytes(), &mut log)?;
    // bit 7 must be set, bit 6 = self powered
    w(&c.join("bmAttributes"), b"0xc0\n", &mut log)?;
    let cst = c.join("strings");
    mk(&cst, &mut log)?;
    mk(&cst.join("0x409"), &mut log)?;
    w(&cst.join("0x409/configuration"), b"DexHid keyboard + mouse", &mut log)?;
    let fs = g.join("functions");
    mk(&fs, &mut log)?;
    for f in &s.functions {
        let d = fs.join(format!("hid.{}", f.name));
        mk(&d, &mut log)?;
        w(&d.join("protocol"), format!("{}\n", f.protocol).as_bytes(), &mut log)?;
        w(&d.join("subclass"), format!("{}\n", f.subclass).as_bytes(), &mut log)?;
        w(&d.join("report_length"), format!("{}\n", f.report_length).as_bytes(), &mut log)?;
        w(&d.join("report_desc"), &f.desc, &mut log)?;
    }
    for f in &s.functions {
        let link = c.join(format!("hid.{}", f.name));
        std::os::unix::fs::symlink(fs.join(format!("hid.{}", f.name)), &link).map_err(|e| format!("symlink {}: {}", link.display(), e))?;
        log.push(format!("link {}", link.display()));
    }
    Ok(log)
}

pub fn bind(root: &Path, udc: &str) -> Result<(), String> {
    let p = root.join(GADGET).join("UDC");
    std::fs::write(&p, format!("{}\n", udc)).map_err(|e| format!("bind {} to {}: {}", GADGET, udc, e))
}

pub fn unbind(root: &Path) -> Result<(), String> {
    let p = root.join(GADGET).join("UDC");
    if !p.exists() {
        return Ok(());
    }
    let cur = std::fs::read_to_string(&p).unwrap_or_default();
    if cur.trim().is_empty() {
        return Ok(());
    }
    std::fs::write(&p, "\n").map_err(|e| format!("unbind: {}", e))
}

fn rmdir(p: &Path, configfs: bool, log: &mut Vec<String>) -> Result<(), String> {
    if !p.exists() {
        return Ok(());
    }
    if !configfs {
        // a plain directory (tests): attribute files go first
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                let ft = e.file_type().map_err(|e| e.to_string())?;
                if ft.is_file() {
                    std::fs::remove_file(e.path()).map_err(|e| e.to_string())?;
                }
            }
        }
    }
    std::fs::remove_dir(p).map_err(|e| format!("rmdir {}: {}", p.display(), e))?;
    log.push(format!("rmdir {}", p.display()));
    Ok(())
}

/// unbind and remove everything `create` made, in the reverse order configfs needs (links, config strings,
/// configs, functions, strings, the gadget). Works on a half-made gadget too.
pub fn remove(root: &Path, configfs: bool) -> Result<Vec<String>, String> {
    let g = root.join(GADGET);
    let mut log = vec![];
    if !g.exists() {
        return Ok(log);
    }
    unbind(root)?;
    log.push("unbind".into());
    let c = g.join("configs/c.1");
    if let Ok(rd) = std::fs::read_dir(&c) {
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
                std::fs::remove_file(e.path()).map_err(|er| format!("unlink {}: {}", e.path().display(), er))?;
                log.push(format!("unlink {}", e.path().display()));
            }
        }
    }
    rmdir(&c.join("strings/0x409"), configfs, &mut log)?;
    if !configfs {
        rmdir(&c.join("strings"), configfs, &mut log)?;
    }
    rmdir(&c, configfs, &mut log)?;
    if let Ok(rd) = std::fs::read_dir(g.join("functions")) {
        let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        v.sort();
        for f in v {
            rmdir(&f, configfs, &mut log)?;
        }
    }
    rmdir(&g.join("strings/0x409"), configfs, &mut log)?;
    if !configfs {
        for d in ["configs", "functions", "strings"] {
            rmdir(&g.join(d), configfs, &mut log)?;
        }
    }
    rmdir(&g, configfs, &mut log)?;
    Ok(log)
}

/// other gadgets in `root` that hold a UDC: (gadget name, UDC)
pub fn others_bound(root: &Path) -> Vec<(String, String)> {
    let mut v = vec![];
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n == GADGET {
                continue;
            }
            let u = std::fs::read_to_string(e.path().join("UDC")).unwrap_or_default().trim().to_string();
            if !u.is_empty() {
                v.push((n, u));
            }
        }
    }
    v.sort();
    v
}

/// the UDCs (/sys/class/udc) and the gadget driver bound to each ("" = free); a legacy g_* gadget shows here too
pub fn udcs(class_dir: &Path) -> Vec<(String, String, String)> {
    let mut v = vec![];
    if let Ok(rd) = std::fs::read_dir(class_dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            let f = std::fs::read_to_string(e.path().join("function")).unwrap_or_default().trim().to_string();
            let st = std::fs::read_to_string(e.path().join("state")).unwrap_or_default().trim().to_string();
            v.push((n, f, st));
        }
    }
    v.sort();
    v
}

/// pick the UDC: the configured one, else the only one
pub fn pick_udc(list: &[(String, String, String)], want: &str) -> Result<String, String> {
    if !want.is_empty() && want != "auto" {
        return list.iter().find(|u| u.0 == want).map(|u| u.0.clone()).ok_or_else(|| format!("UDC {} not found (have: {:?})", want, list.iter().map(|u| &u.0).collect::<Vec<_>>()));
    }
    match list.len() {
        0 => Err("no USB device controller (UDC): the kernel has no gadget support or the port is host-only".into()),
        1 => Ok(list[0].0.clone()),
        _ => Err(format!("several UDCs, set usb_udc: {:?}", list.iter().map(|u| &u.0).collect::<Vec<_>>())),
    }
}

/// the USB role switches (/sys/class/usb_role/*/role): name, role ("device" | "host" | "none")
pub fn roles(dir: &Path) -> Vec<(String, String)> {
    let mut v = vec![];
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Ok(r) = std::fs::read_to_string(e.path().join("role")) {
                v.push((e.file_name().to_string_lossy().into_owned(), r.trim().to_string()));
            }
        }
    }
    v.sort();
    v
}

/// the text shown for the data role (owner, 7 Nov: the tablet must stay the USB device; a PD source on a pass-through
/// splitter is fine, a hub / dock makes the tablet a host and cannot work)
pub fn role_text(roles: &[(String, String)]) -> (bool, String) {
    if roles.is_empty() {
        return (true, "data role unknown (no usb_role switch): binding is tried".into());
    }
    if roles.iter().any(|r| r.1 == "host") {
        return (
            false,
            "the tablet is USB host now (a hub / dock / OTG adapter is attached): gadget mode needs the tablet as the device. Use a cable or a PD pass-through splitter (charger + data) straight to the other device".into(),
        );
    }
    if roles.iter().all(|r| r.1 == "none") {
        return (true, "no data partner yet (role none): plug the other device in; the role becomes device when it is a host".into());
    }
    (true, "data role: device (the other device is the USB host)".into())
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Power {
    /// e.g. "qcom-battmgr-usb online 9.0 V 2.20 A [PD]"
    pub sources: Vec<String>,
    pub battery: Option<String>,
    /// typec port: power role / data role ("sink" / "device"), partner present
    pub typec: Vec<String>,
}

fn rd(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn bracket(s: &str) -> String {
    s.split_whitespace().find(|w| w.starts_with('[')).map(|w| w.trim_matches(|c| c == '[' || c == ']').to_string()).unwrap_or_else(|| s.to_string())
}

/// the live power state from /sys/class/power_supply (battmgr usb / ucsi source supplies, the battery) and
/// /sys/class/typec (roles). Values: current in uA, voltage in uV.
pub fn power(supply_dir: &Path, typec_dir: &Path) -> Power {
    let mut p = Power::default();
    let mut names: Vec<PathBuf> = std::fs::read_dir(supply_dir).map(|r| r.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    names.sort();
    for d in names {
        let n = d.file_name().unwrap().to_string_lossy().into_owned();
        let ty = rd(&d.join("type")).unwrap_or_default();
        if ty == "Battery" {
            let cap = rd(&d.join("capacity")).unwrap_or_else(|| "?".into());
            let st = rd(&d.join("status")).unwrap_or_default();
            let cur = rd(&d.join("current_now")).and_then(|v| v.parse::<i64>().ok());
            p.battery = Some(format!("{} % {}{}", cap, st.to_lowercase(), cur.map(|c| format!(" {:+.2} A", c as f64 / 1e6)).unwrap_or_default()));
            continue;
        }
        if !(n.contains("usb") || n.starts_with("ucsi") || ty.starts_with("USB")) {
            continue;
        }
        let online = rd(&d.join("online")).map(|v| v == "1").unwrap_or(false);
        let v = rd(&d.join("voltage_now")).and_then(|v| v.parse::<i64>().ok());
        let c = rd(&d.join("current_now")).or_else(|| rd(&d.join("current_max"))).and_then(|v| v.parse::<i64>().ok());
        let t = rd(&d.join("usb_type")).map(|s| bracket(&s));
        let mut s = format!("{} {}", n, if online { "online" } else { "offline" });
        if online {
            if let Some(v) = v {
                s.push_str(&format!(" {:.1} V", v as f64 / 1e6));
            }
            if let Some(c) = c {
                s.push_str(&format!(" {:.2} A", c as f64 / 1e6));
            }
        }
        if let Some(t) = t {
            s.push_str(&format!(" [{}]", t));
        }
        p.sources.push(s);
    }
    let mut ports: Vec<PathBuf> = std::fs::read_dir(typec_dir).map(|r| r.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    ports.sort();
    for d in ports {
        let n = d.file_name().unwrap().to_string_lossy().into_owned();
        if n.contains('-') {
            continue;
        }
        let pr = rd(&d.join("power_role")).map(|s| bracket(&s)).unwrap_or_default();
        let dr = rd(&d.join("data_role")).map(|s| bracket(&s)).unwrap_or_default();
        let partner = typec_dir.join(format!("{}-partner", n)).exists();
        p.typec.push(format!("{} power {} data {}{}", n, pr, dr, if partner { " partner" } else { "" }));
    }
    p
}

impl Power {
    /// one line for the USB HID panel / the strip
    pub fn line(&self) -> String {
        let mut v = vec![];
        let on: Vec<&String> = self.sources.iter().filter(|s| s.contains(" online")).collect();
        if on.is_empty() {
            v.push("no USB power in".to_string());
        } else {
            v.extend(on.iter().map(|s| s.to_string()));
        }
        if let Some(b) = &self.battery {
            v.push(format!("battery {}", b));
        }
        v.extend(self.typec.iter().cloned());
        v.join("  |  ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(n: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dexhid-gadget-{}-{}", n, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn create_bind_remove_on_a_plain_dir() {
        let root = tmp("cfs");
        let s = spec(false);
        let log = create(&root, &s).unwrap();
        let g = root.join(GADGET);
        assert_eq!(std::fs::read_to_string(g.join("idVendor")).unwrap(), "0x1d6b\n");
        assert_eq!(std::fs::read_to_string(g.join("idProduct")).unwrap(), "0x0104\n");
        assert_eq!(std::fs::read(g.join("functions/hid.kbd/report_desc")).unwrap(), crate::hid::keyboard(None), "the descriptor is written binary, byte for byte");
        assert_eq!(std::fs::read_to_string(g.join("functions/hid.kbd/protocol")).unwrap(), "1\n");
        assert_eq!(std::fs::read_to_string(g.join("functions/hid.mouse/protocol")).unwrap(), "2\n");
        assert_eq!(std::fs::read_to_string(g.join("functions/hid.extra/report_length")).unwrap(), "3\n");
        assert_eq!(std::fs::read_to_string(g.join("strings/0x409/serialnumber")).unwrap(), "dexhid-0001", "fixed serial, never the tablet's");
        assert!(std::fs::read_link(g.join("configs/c.1/hid.kbd")).is_ok());
        assert!(log.iter().any(|l| l.starts_with("link ")));
        assert!(create(&root, &s).is_err(), "a second create refuses (left-over)");
        bind(&root, "a600000.usb").unwrap();
        assert_eq!(std::fs::read_to_string(g.join("UDC")).unwrap(), "a600000.usb\n");
        // another gadget holding the UDC is reported
        std::fs::create_dir_all(root.join("ncm")).unwrap();
        std::fs::write(root.join("ncm/UDC"), "a600000.usb\n").unwrap();
        assert_eq!(others_bound(&root), vec![("ncm".to_string(), "a600000.usb".to_string())]);
        let rl = remove(&root, false).unwrap();
        assert!(!g.exists(), "{:?}", rl);
        assert_eq!(rl.first().map(|s| s.as_str()), Some("unbind"));
        let ui = rl.iter().position(|l| l.starts_with("unlink")).unwrap();
        let fi = rl.iter().position(|l| l.contains("functions/hid.")).unwrap();
        assert!(ui < fi, "links before functions");
        assert!(remove(&root, false).unwrap().is_empty(), "nothing left: no-op");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn udc_role_and_power() {
        let d = tmp("sys");
        let udc = d.join("udc/a600000.usb");
        std::fs::create_dir_all(&udc).unwrap();
        std::fs::write(udc.join("function"), "\n").unwrap();
        std::fs::write(udc.join("state"), "not attached\n").unwrap();
        let l = udcs(&d.join("udc"));
        assert_eq!(l, vec![("a600000.usb".into(), "".into(), "not attached".into())]);
        assert_eq!(pick_udc(&l, "auto").unwrap(), "a600000.usb");
        assert!(pick_udc(&l, "x.usb").is_err());
        assert!(pick_udc(&[], "auto").is_err());
        let r = d.join("role/a600000.usb-role-switch");
        std::fs::create_dir_all(&r).unwrap();
        std::fs::write(r.join("role"), "device\n").unwrap();
        let rs = roles(&d.join("role"));
        assert!(role_text(&rs).0);
        std::fs::write(r.join("role"), "host\n").unwrap();
        let (ok, t) = role_text(&roles(&d.join("role")));
        assert!(!ok && t.contains("host"), "{}", t);
        assert!(role_text(&[("x".into(), "none".into())]).1.contains("no data partner"));
        // power: battmgr usb + ucsi source + battery + a typec port with a partner
        let ps = d.join("ps");
        for (n, files) in [
            ("qcom-battmgr-usb", vec![("type", "USB"), ("online", "1"), ("voltage_now", "9000000"), ("current_now", "2200000"), ("usb_type", "Unknown SDP DCP CDP [PD] PD_PPS")]),
            ("ucsi-source-psy-pmic_glink.ucsi.01", vec![("type", "USB"), ("online", "0")]),
            ("qcom-battmgr-bat", vec![("type", "Battery"), ("capacity", "84"), ("status", "Charging"), ("current_now", "1100000")]),
        ] {
            std::fs::create_dir_all(ps.join(n)).unwrap();
            for (f, v) in files {
                std::fs::write(ps.join(n).join(f), format!("{}\n", v)).unwrap();
            }
        }
        let tc = d.join("typec");
        std::fs::create_dir_all(tc.join("port0")).unwrap();
        std::fs::create_dir_all(tc.join("port0-partner")).unwrap();
        std::fs::write(tc.join("port0/power_role"), "source [sink]\n").unwrap();
        std::fs::write(tc.join("port0/data_role"), "host [device]\n").unwrap();
        let p = power(&ps, &tc);
        assert_eq!(p.sources[0], "qcom-battmgr-usb online 9.0 V 2.20 A [PD]");
        assert_eq!(p.battery.as_deref(), Some("84 % charging +1.10 A"));
        assert_eq!(p.typec, vec!["port0 power sink data device partner".to_string()]);
        let line = p.line();
        assert!(line.contains("9.0 V") && line.contains("battery 84 %") && !line.contains("ucsi"), "{}", line);
        std::fs::remove_dir_all(&d).ok();
    }
}
