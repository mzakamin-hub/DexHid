// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! Desktop integration (M2), all off the input thread:
//! - a worker thread for slow jobs: the KWin InputDevice applier (natural scroll, tap to click, pointer
//!   acceleration of the virtual touchpad over org.kde.KWin.InputDevice), the panel dimming + restore (logind
//!   Session.SetBrightness, original value kept in $XDG_RUNTIME_DIR/dexhid/brightness for `dexhid --restore`),
//!   the desktop colour scheme for theme "auto" (xdg-desktop-portal Settings), key-click sounds;
//! - the KDE keyboard-layout tracker (org.kde.keyboard /Layouts: layoutChanged) so the legends follow the active
//!   layout. Other desktops: see M2.md (the active group is not visible to an unfocused Wayland client).

use crate::shared::{Cmd, SharedRef};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::time::Duration;
use zbus::blocking::Connection;
use zbus::zvariant::{OwnedValue, Value};

pub enum Job {
    /// apply the desktop's touchpad settings to the virtual touchpad (event node name, e.g. "event11")
    KwinPad { node: String, natural_scroll: bool, tap: bool, accel: f64 },
    Dim { backlight: String, percent: u32 },
    Restore,
    ThemeAuto,
    Click,
}

pub struct Worker {
    tx: Sender<Job>,
}

impl Worker {
    pub fn start(sh: SharedRef) -> Worker {
        let (tx, rx) = channel::<Job>();
        std::thread::Builder::new()
            .name("desktop".into())
            .spawn(move || {
                let mut last_click = 0i64;
                let click = sound_file();
                while let Ok(j) = rx.recv() {
                    match j {
                        Job::KwinPad { node, natural_scroll, tap, accel } => {
                            let r = kwin_apply(&node, &[("naturalScroll", Value::from(natural_scroll)), ("tapToClick", Value::from(tap)), ("pointerAcceleration", Value::from(accel))]);
                            sh.send(Cmd::Note(r));
                        }
                        Job::Dim { backlight, percent } => sh.send(Cmd::Note(dim(&backlight, percent))),
                        Job::Restore => sh.send(Cmd::Note(restore())),
                        Job::ThemeAuto => {
                            let t = color_scheme().unwrap_or("dark");
                            sh.send(Cmd::ThemeAuto(t.to_string()));
                        }
                        Job::Click => {
                            let now = crate::sys::mono_ms();
                            if now - last_click < 40 {
                                continue;
                            }
                            last_click = now;
                            play_click(&click);
                        }
                    }
                }
            })
            .expect("desktop thread");
        Worker { tx }
    }
    pub fn send(&self, j: Job) {
        let _ = self.tx.send(j);
    }
}

// ------------------------------------------------------------------ KWin InputDevice

fn kwin_get(c: &Connection, path: &str, prop: &str) -> Option<OwnedValue> {
    let m = c.call_method(Some("org.kde.KWin"), path, Some("org.freedesktop.DBus.Properties"), "Get", &("org.kde.KWin.InputDevice", prop)).ok()?;
    m.body().deserialize::<OwnedValue>().ok()
}

/// set properties of /org/kde/KWin/InputDevice/<node>; waits up to 3 s for KWin to register the device
pub fn kwin_apply(node: &str, props: &[(&str, Value)]) -> String {
    let c = match Connection::session() {
        Ok(c) => c,
        Err(e) => return format!("KWIN-SET skipped: no session bus ({})", e),
    };
    let path = format!("/org/kde/KWin/InputDevice/{}", node);
    let mut ok = false;
    for _ in 0..30 {
        if kwin_get(&c, &path, "name").is_some() {
            ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !ok {
        return format!("KWIN-SET skipped: {} not found (not KWin, or the device is not registered)", path);
    }
    let mut parts = vec![];
    for (p, v) in props {
        let r = c.call_method(Some("org.kde.KWin"), path.as_str(), Some("org.freedesktop.DBus.Properties"), "Set", &("org.kde.KWin.InputDevice", *p, v));
        let back = kwin_get(&c, &path, p).map(|v| format!("{:?}", v)).unwrap_or_else(|| "?".into());
        parts.push(format!("{}={}{}", p, back.replace("OwnedValue(", "").trim_end_matches(')'), if r.is_err() { "(set failed)" } else { "" }));
    }
    format!("KWIN-SET node={} {}", node, parts.join(" "))
}

// ------------------------------------------------------------------ backlight

fn runtime_dir() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/tmp")).join("dexhid")
}

fn brightness_file() -> PathBuf {
    runtime_dir().join("brightness")
}

fn rd_u32(p: &Path) -> Option<u32> {
    std::fs::read_to_string(p).ok()?.trim().parse().ok()
}

/// (name, current, max) of the configured backlight ("auto" = the first one)
pub fn backlight(name: &str) -> Option<(String, u32, u32)> {
    let base = Path::new("/sys/class/backlight");
    let n = if name == "auto" || name.is_empty() {
        let mut v: Vec<String> = std::fs::read_dir(base).ok()?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v.into_iter().next()?
    } else {
        name.to_string()
    };
    let d = base.join(&n);
    Some((n, rd_u32(&d.join("brightness"))?, rd_u32(&d.join("max_brightness"))?))
}

fn session_path(c: &Connection) -> Option<String> {
    crate::logind::session_path(c)
}

fn set_brightness(name: &str, v: u32) -> Result<(), String> {
    let c = Connection::system().map_err(|e| format!("system bus: {}", e))?;
    let sp = session_path(&c).ok_or("no graphical session")?;
    c.call_method(Some("org.freedesktop.login1"), sp.as_str(), Some("org.freedesktop.login1.Session"), "SetBrightness", &("backlight", name, v))
        .map(|_| ())
        .map_err(|e| format!("SetBrightness: {}", e))
}

/// dim to `percent`; the original value is kept in the runtime file (a stale file from a crash keeps its original)
pub fn dim(cfg_name: &str, percent: u32) -> String {
    let Some((name, cur, max)) = backlight(cfg_name) else { return "DIM skipped: no backlight device".into() };
    let f = brightness_file();
    let orig = std::fs::read_to_string(&f).ok().and_then(|s| s.split_whitespace().nth(1).and_then(|v| v.parse::<u32>().ok())).unwrap_or(cur);
    let target = dexhid_core::feedback::dim_value(orig, max, percent);
    let _ = std::fs::create_dir_all(runtime_dir());
    if let Err(e) = std::fs::write(&f, format!("{} {} {}\n", name, orig, target)) {
        return format!("DIM skipped: {}: {}", f.display(), e);
    }
    match set_brightness(&name, target) {
        Ok(()) => format!("DIM t={} backlight={} from={} to={} max={}", crate::sys::mono_ms(), name, orig, target, max),
        Err(e) => {
            let _ = std::fs::remove_file(&f);
            format!("DIM failed: {}", e)
        }
    }
}

/// restore the value saved by `dim` (skipped when the user changed the brightness meanwhile)
pub fn restore() -> String {
    let f = brightness_file();
    let Ok(s) = std::fs::read_to_string(&f) else { return "RESTORE nothing to do".into() };
    let w: Vec<&str> = s.split_whitespace().collect();
    let (Some(name), Some(orig), Some(dimmed)) = (w.first(), w.get(1).and_then(|v| v.parse::<u32>().ok()), w.get(2).and_then(|v| v.parse::<u32>().ok())) else {
        let _ = std::fs::remove_file(&f);
        return "RESTORE bad state file removed".into();
    };
    let cur = backlight(name).map(|b| b.1);
    let msg = if cur.is_some() && cur != Some(dimmed) {
        format!("RESTORE t={} backlight={} kept {} (changed by the user, was dimmed to {})", crate::sys::mono_ms(), name, cur.unwrap(), dimmed)
    } else {
        match set_brightness(name, orig) {
            Ok(()) => format!("RESTORE t={} backlight={} to={}", crate::sys::mono_ms(), name, orig),
            Err(e) => return format!("RESTORE failed: {} (state kept for `dexhid --restore`)", e),
        }
    };
    let _ = std::fs::remove_file(&f);
    msg
}

// ------------------------------------------------------------------ theme "auto"

/// the desktop's colour scheme from the portal: "dark" | "light"
pub fn color_scheme() -> Option<&'static str> {
    let c = Connection::session().ok()?;
    let m = c
        .call_method(Some("org.freedesktop.portal.Desktop"), "/org/freedesktop/portal/desktop", Some("org.freedesktop.portal.Settings"), "ReadOne", &("org.freedesktop.appearance", "color-scheme"))
        .ok()?;
    let v: OwnedValue = m.body().deserialize().ok()?;
    let n: u32 = match &*v {
        Value::U32(n) => *n,
        Value::Value(b) => match &**b {
            Value::U32(n) => *n,
            _ => return None,
        },
        _ => return None,
    };
    Some(if n == 2 { "light" } else { "dark" })
}

// ------------------------------------------------------------------ key click

fn sound_file() -> PathBuf {
    let p = runtime_dir().join("click.wav");
    let _ = std::fs::create_dir_all(runtime_dir());
    if !p.exists() {
        let _ = std::fs::write(&p, dexhid_core::feedback::click_wav());
    }
    p
}

pub fn have_program(p: &str) -> bool {
    if p.contains('/') {
        return Path::new(p).exists();
    }
    std::env::var("PATH").unwrap_or_default().split(':').any(|d| !d.is_empty() && Path::new(d).join(p).is_file())
}

fn play_click(f: &Path) {
    if std::env::var("DEXHID_FEEDBACK_DRY").is_ok() {
        println!("FEEDBACK t={} sound (dry run, nothing played)", crate::sys::mono_ms());
        return;
    }
    let player = ["pw-play", "paplay", "aplay"].into_iter().find(|p| have_program(p));
    let Some(pl) = player else { return };
    let mut c = std::process::Command::new(pl);
    if pl == "pw-play" {
        c.args(["--volume", "0.4"]);
    }
    if let Ok(mut ch) = c.arg(f).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
        std::thread::spawn(move || {
            let _ = ch.wait();
        });
    }
}

/// a vibration device (FF_RUMBLE evdev or a vibrator LED); None = haptics hidden in the settings
pub fn vibrator() -> Option<String> {
    for p in ["/sys/class/leds/vibrator", "/sys/class/timed_output/vibrator"] {
        if Path::new(p).exists() {
            return Some(p.into());
        }
    }
    for n in crate::sys::event_nodes() {
        if let Ok(d) = crate::sys::Dev::open(&n, false) {
            // EV_FF = 0x15, FF_RUMBLE = 0x50
            if crate::sys::test_bit(&d.bits(0x15, 0x80), 0x50) {
                return Some(n);
            }
        }
    }
    None
}

// ------------------------------------------------------------------ KDE layout tracker

/// follows org.kde.keyboard /Layouts; sends Cmd::XkbGroup on start and on every layoutChanged
pub fn kxkb_start(sh: SharedRef) {
    let _ = std::thread::Builder::new().name("kxkb".into()).spawn(move || {
        let c = match Connection::session() {
            Ok(c) => c,
            Err(_) => return,
        };
        let call = |m: &str| c.call_method(Some("org.kde.keyboard"), "/Layouts", Some("org.kde.KeyboardLayouts"), m, &());
        let names = |c: &dyn Fn(&str) -> zbus::Result<zbus::message::Message>| -> Vec<String> {
            c("getLayoutsList")
                .ok()
                .and_then(|m| m.body().deserialize::<Vec<(String, String, String)>>().ok())
                .map(|v| v.into_iter().map(|(short, _variant, long)| if long.is_empty() { short } else { long }).collect())
                .unwrap_or_default()
        };
        let idx = match call("getLayout").and_then(|m| m.body().deserialize::<u32>().map_err(Into::into)) {
            Ok(i) => i,
            Err(e) => {
                sh.send(Cmd::Note(format!("KXKB not available ({}): legends use xkb_layout_index", e.to_string().chars().take(80).collect::<String>())));
                return;
            }
        };
        let mut list = names(&call);
        sh.send(Cmd::XkbGroup(idx, list.clone()));
        let rule = "type='signal',interface='org.kde.KeyboardLayouts'";
        let Ok(it) = zbus::blocking::MessageIterator::for_match_rule(rule, &c, Some(32)) else { return };
        for m in it.flatten() {
            let member = m.header().member().map(|x| x.as_str().to_string()).unwrap_or_default();
            match member.as_str() {
                "layoutChanged" => {
                    if let Ok(i) = m.body().deserialize::<u32>() {
                        sh.send(Cmd::XkbGroup(i, list.clone()));
                    }
                }
                "layoutListChanged" => {
                    list = names(&call);
                    if let Ok(i) = call("getLayout").and_then(|m| m.body().deserialize::<u32>().map_err(Into::into)) {
                        sh.send(Cmd::XkbGroup(i, list.clone()));
                    }
                }
                _ => {}
            }
        }
    });
}
