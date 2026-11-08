// SPDX-License-Identifier: GPL-3.0-or-later
//! DRM hotplug: kernel uevent parsing and the connector scan of /sys/class/drm.

use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct Uevent {
    pub action: String,
    pub devpath: String,
    pub props: HashMap<String, String>,
}

impl Uevent {
    pub fn get(&self, k: &str) -> Option<&str> {
        self.props.get(k).map(|s| s.as_str())
    }
    pub fn is_drm(&self) -> bool {
        self.get("SUBSYSTEM") == Some("drm")
    }
    pub fn is_hotplug(&self) -> bool {
        self.is_drm() && self.get("HOTPLUG") == Some("1")
    }
}

/// a NETLINK_KOBJECT_UEVENT message from the kernel group: "action@devpath\0KEY=VALUE\0..."
pub fn parse_uevent(buf: &[u8]) -> Option<Uevent> {
    if buf.starts_with(b"libudev") {
        return None; // udev's re-broadcast (group 2): we only listen to the kernel
    }
    let mut it = buf.split(|&b| b == 0).filter(|s| !s.is_empty());
    let head = std::str::from_utf8(it.next()?).ok()?;
    let (action, devpath) = head.split_once('@')?;
    let mut props = HashMap::new();
    for kv in it {
        if let Ok(s) = std::str::from_utf8(kv) {
            if let Some((k, v)) = s.split_once('=') {
                props.insert(k.to_string(), v.to_string());
            }
        }
    }
    Some(Uevent { action: action.into(), devpath: devpath.into(), props })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Connector {
    /// "DP-1" (without the "cardN-" prefix)
    pub name: String,
    pub connected: bool,
    pub enabled: bool,
    /// FNV-1a 64 of the EDID, hex; None without an EDID
    pub edid_hash: Option<String>,
}

pub fn fnv64(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// read every cardN-<connector> of a /sys/class/drm-like directory
pub fn scan(drm: &Path) -> Vec<Connector> {
    let mut v = vec![];
    let Ok(rd) = std::fs::read_dir(drm) else { return v };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        let Some(rest) = n.strip_prefix("card") else { continue };
        let Some((num, name)) = rest.split_once('-') else { continue };
        if num.parse::<u32>().is_err() || name.is_empty() {
            continue;
        }
        let p = e.path();
        let rd = |f: &str| std::fs::read_to_string(p.join(f)).map(|s| s.trim().to_string()).unwrap_or_default();
        let edid = std::fs::read(p.join("edid")).ok().filter(|b| !b.is_empty());
        v.push(Connector {
            name: name.to_string(),
            connected: rd("status") == "connected",
            enabled: rd("enabled") == "enabled",
            edid_hash: edid.map(|b| format!("{:016x}", fnv64(&b))),
        });
    }
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// '*' and '?' wildcards
pub fn glob(pat: &str, s: &str) -> bool {
    fn m(p: &[u8], s: &[u8]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(b'*'), _) => m(&p[1..], s) || (!s.is_empty() && m(p, &s[1..])),
            (Some(b'?'), Some(_)) => m(&p[1..], &s[1..]),
            (Some(a), Some(b)) if a == b => m(&p[1..], &s[1..]),
            _ => false,
        }
    }
    m(pat.as_bytes(), s.as_bytes())
}

/// the first connected external connector (matching one of `patterns`)
pub fn external<'a>(conns: &'a [Connector], patterns: &[String]) -> Option<&'a Connector> {
    conns.iter().find(|c| c.connected && patterns.iter().any(|p| glob(p, &c.name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uevent() {
        let b = b"change@/devices/platform/soc/ae00000.display-subsystem/drm/card0\0ACTION=change\0DEVPATH=/devices/x\0SUBSYSTEM=drm\0HOTPLUG=1\0CONNECTOR=36\0SEQNUM=5\0";
        let u = parse_uevent(b).unwrap();
        assert_eq!(u.action, "change");
        assert!(u.is_hotplug());
        assert_eq!(u.get("CONNECTOR"), Some("36"));
        assert!(parse_uevent(b"libudev\0xxxx").is_none());
        let u = parse_uevent(b"add@/devices/virtual/input/input9\0SUBSYSTEM=input\0").unwrap();
        assert!(!u.is_drm());
    }

    #[test]
    fn globbing() {
        assert!(glob("DP-*", "DP-1"));
        assert!(glob("HDMI-?-*", "HDMI-A-1"));
        assert!(!glob("DP-*", "DSI-1"));
        assert!(glob("*", ""));
    }

    #[test]
    fn scan_fake_sysfs() {
        let d = std::env::temp_dir().join(format!("dexhid-drm-{}", std::process::id()));
        for (n, st, en) in [("card0-DSI-1", "connected", "enabled"), ("card0-DP-1", "disconnected", "disabled"), ("card0", "", "")] {
            std::fs::create_dir_all(d.join(n)).unwrap();
            if !st.is_empty() {
                std::fs::write(d.join(n).join("status"), format!("{}\n", st)).unwrap();
                std::fs::write(d.join(n).join("enabled"), format!("{}\n", en)).unwrap();
            }
        }
        let pats = vec!["DP-*".to_string()];
        let c = scan(&d);
        assert_eq!(c.len(), 2);
        assert!(external(&c, &pats).is_none());
        std::fs::write(d.join("card0-DP-1/status"), "connected\n").unwrap();
        std::fs::write(d.join("card0-DP-1/edid"), [0u8, 255, 255, 1]).unwrap();
        let c = scan(&d);
        let e = external(&c, &pats).unwrap();
        assert_eq!(e.name, "DP-1");
        assert_eq!(e.edid_hash.as_deref().map(|s| s.len()), Some(16));
        std::fs::remove_dir_all(&d).ok();
    }
}
