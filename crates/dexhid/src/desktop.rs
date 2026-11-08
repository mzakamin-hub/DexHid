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
    /// M2.1: move the Plasma panels of the internal screen to the external one (`test_target` = debug: with fewer
    /// than two Plasma screens, "move" the panels onto their own screen so the script path runs without a monitor)
    PanelsMove { internal: String, state: PathBuf, test_target: Option<i32> },
    /// M2.1: put every panel back on the screen recorded in the state file (also after a crash, at start)
    PanelsRestore { state: PathBuf, why: String },
    /// M2.1: read the desktop look (KDE files)
    DeskLook,
}

pub struct Worker {
    tx: Sender<Job>,
    /// M2.1 fix: the KWin applier has its own thread. It may wait up to 3 s for KWin to register a new device; on
    /// the shared thread that wait delayed the dimming, its restore, the desktop look and the panel moves by
    /// seconds (T7e, M2.1-9 and M2.1-8 of the first M2.1 run).
    kwin_tx: Sender<Job>,
}

impl Worker {
    pub fn start(sh: SharedRef) -> Worker {
        let (tx, rx) = channel::<Job>();
        let (kwin_tx, kwin_rx) = channel::<Job>();
        let sh2 = sh.clone();
        std::thread::Builder::new()
            .name("kwin-apply".into())
            .spawn(move || {
                while let Ok(mut j) = kwin_rx.recv() {
                    // only the newest request counts (the touchpad is recreated often: older nodes are gone)
                    while let Ok(n) = kwin_rx.try_recv() {
                        j = n;
                    }
                    if let Job::KwinPad { node, natural_scroll, tap, accel } = j {
                        let r = kwin_apply(&node, &[("naturalScroll", Value::from(natural_scroll)), ("tapToClick", Value::from(tap)), ("pointerAcceleration", Value::from(accel))]);
                        sh2.send(Cmd::Note(r));
                    }
                }
            })
            .expect("kwin thread");
        std::thread::Builder::new()
            .name("desktop".into())
            .spawn(move || {
                let mut last_click = 0i64;
                let click = sound_file();
                while let Ok(j) = rx.recv() {
                    match j {
                        Job::KwinPad { .. } => {}
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
                        Job::PanelsMove { internal, state, test_target } => sh.send(Cmd::Note(panels_move(&internal, &state, test_target))),
                        Job::PanelsRestore { state, why } => {
                            if let Some(r) = panels_restore(&state, &why) {
                                sh.send(Cmd::Note(r));
                            }
                        }
                        Job::DeskLook => {
                            let (d, note) = desk_look();
                            sh.send(Cmd::DeskLook(d, note));
                        }
                    }
                }
            })
            .expect("desktop thread");
        Worker { tx, kwin_tx }
    }
    pub fn send(&self, j: Job) {
        let _ = if matches!(j, Job::KwinPad { .. }) { self.kwin_tx.send(j) } else { self.tx.send(j) };
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
        // M2.1 fix: a node that is gone (or reused by another device) is not waited for
        if !is_dexhid_node(node) {
            return format!("KWIN-SET skipped t={}: {} is no DexHid device any more (recreated meanwhile)", crate::sys::mono_ms(), node);
        }
        if kwin_get(&c, &path, "name").is_some() {
            ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !ok {
        return format!("KWIN-SET skipped t={}: {} not found (not KWin, or the device is not registered)", crate::sys::mono_ms(), path);
    }
    let mut parts = vec![];
    for (p, v) in props {
        // only real changes: KWin stores every Set in kcminputrc
        let same = match (v, kwin_get(&c, &path, p)) {
            (Value::Bool(b), Some(cur)) => bool::try_from(cur).ok() == Some(*b),
            (Value::F64(f), Some(cur)) => f64::try_from(cur).map(|x| (x - f).abs() < 1e-6).unwrap_or(false),
            _ => false,
        };
        if same {
            parts.push(format!("{}=unchanged", p));
            continue;
        }
        // a zvariant Value is sent as the variant Set expects (b for the switches, d for pointerAcceleration: the
        // run's read-back showed both types are accepted)
        let r = c.call_method(Some("org.kde.KWin"), path.as_str(), Some("org.freedesktop.DBus.Properties"), "Set", &("org.kde.KWin.InputDevice", *p, v));
        // M2.1 fix: the read-back as a plain value ("true", "0.5"); the Debug form ("Bool(true") broke the log checks
        let back = kwin_get(&c, &path, p).map(|v| plain(&v)).unwrap_or_else(|| "?".into());
        parts.push(format!("{}={}{}", p, back, if let Err(e) = &r { format!("(set failed: {})", e) } else { String::new() }));
    }
    format!("KWIN-SET t={} node={} {}", crate::sys::mono_ms(), node, parts.join(" "))
}

/// a D-Bus value as plain text: true / false / 0.5 / text
fn plain(v: &Value) -> String {
    match v {
        Value::Bool(b) => b.to_string(),
        Value::F64(f) => format!("{}", f),
        Value::I32(i) => i.to_string(),
        Value::U32(i) => i.to_string(),
        Value::I64(i) => i.to_string(),
        Value::U64(i) => i.to_string(),
        Value::Str(s) => s.to_string(),
        Value::Value(b) => plain(b),
        o => format!("{:?}", o),
    }
}

/// /dev/input/<node> is (still) one of DexHid's virtual devices
fn is_dexhid_node(node: &str) -> bool {
    std::fs::read_to_string(format!("/sys/class/input/{}/device/name", node)).map(|n| n.trim().starts_with("dexhid ")).unwrap_or(false)
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

/// the xkb layout codes of KDE's layout list: getLayoutsList gives (shortName, displayName, longName) per group;
/// the variant comes from kxkbrc [Layout] VariantList when its LayoutList matches the same position. Result per
/// group: "us", "de(nodeadkeys)". A missing short name falls back to the kxkbrc entry, then "us".
pub fn xkb_codes(list: &[(String, String, String)], kxkbrc: Option<&str>) -> Vec<String> {
    let (mut lays, mut vars): (Vec<String>, Vec<String>) = (vec![], vec![]);
    if let Some(t) = kxkbrc {
        let ini = dexhid_core::feedback::parse_ini(t);
        let get = |k: &str| ini.get(&("Layout".to_string(), k.to_string())).map(|v| v.split(',').map(|s| s.trim().to_string()).collect::<Vec<_>>()).unwrap_or_default();
        lays = get("LayoutList");
        vars = get("VariantList");
    }
    let n = if list.is_empty() { lays.len() } else { list.len() };
    (0..n)
        .map(|i| {
            let short = list.get(i).map(|x| x.0.trim().to_string()).filter(|s| !s.is_empty() && !s.contains(' ')).or_else(|| lays.get(i).cloned().filter(|s| !s.is_empty())).unwrap_or_else(|| "us".into());
            match (lays.get(i), vars.get(i)) {
                (Some(l), Some(v)) if *l == short && !v.is_empty() => format!("{}({})", short, v),
                _ => short,
            }
        })
        .collect()
}

/// follows org.kde.keyboard /Layouts; sends Cmd::XkbGroup on start and on every layoutChanged
pub fn kxkb_start(sh: SharedRef) {
    let _ = std::thread::Builder::new().name("kxkb".into()).spawn(move || {
        let c = match Connection::session() {
            Ok(c) => c,
            Err(_) => return,
        };
        let call = |m: &str| c.call_method(Some("org.kde.keyboard"), "/Layouts", Some("org.kde.KeyboardLayouts"), m, &());
        // M4 fix: xkb codes ("us", "de(nodeadkeys)"), never the display name ("English (US)"): the USB target's
        // legends compile a keymap from this
        let names = |c: &dyn Fn(&str) -> zbus::Result<zbus::message::Message>| -> Vec<String> {
            let list = c("getLayoutsList").ok().and_then(|m| m.body().deserialize::<Vec<(String, String, String)>>().ok()).unwrap_or_default();
            let kxkbrc = std::env::var("XDG_CONFIG_HOME")
                .map(std::path::PathBuf::from)
                .or_else(|_| std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
                .ok()
                .and_then(|d| std::fs::read_to_string(d.join("kxkbrc")).ok());
            xkb_codes(&list, kxkbrc.as_deref())
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

// ------------------------------------------------------------------ M2.1: Plasma panels follow the monitor

/// run a Plasma desktop script (org.kde.PlasmaShell.evaluateScript); returns what the script print()ed
fn plasma_eval(c: &Connection, script: &str) -> Result<String, String> {
    let m = c
        .call_method(Some("org.kde.plasmashell"), "/PlasmaShell", Some("org.kde.PlasmaShell"), "evaluateScript", &(script,))
        .map_err(|e| format!("evaluateScript: {}", e.to_string().chars().take(160).collect::<String>()))?;
    m.body().deserialize::<String>().map_err(|e| format!("evaluateScript reply: {}", e))
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct PlasmaLayout {
    /// (Plasma screen id, [x, y, w, h])
    pub screens: Vec<(i32, [i32; 4])>,
    /// (panel id, screen, location)
    pub panels: Vec<(i32, i32, String)>,
}

const LAYOUT_JS: &str = "var o={screens:[],panels:[]};for(var i=0;i<screenCount;i++){var g=screenGeometry(i);o.screens.push([i,g.x,g.y,g.width,g.height]);}var ps=panels();for(var j=0;j<ps.length;j++){o.panels.push([ps[j].id,ps[j].screen,ps[j].location]);}print(JSON.stringify(o));";

/// parse the layout script output
pub fn parse_layout(s: &str) -> Option<PlasmaLayout> {
    let v: serde_json::Value = serde_json::from_str(s.trim()).ok()?;
    let mut l = PlasmaLayout::default();
    for sc in v.get("screens")?.as_array()? {
        let a: Vec<i64> = sc.as_array()?.iter().filter_map(|x| x.as_i64()).collect();
        if a.len() == 5 {
            l.screens.push((a[0] as i32, [a[1] as i32, a[2] as i32, a[3] as i32, a[4] as i32]));
        }
    }
    for p in v.get("panels")?.as_array()? {
        let a = p.as_array()?;
        l.panels.push((a.first()?.as_i64()? as i32, a.get(1)?.as_i64()? as i32, a.get(2).and_then(|x| x.as_str()).unwrap_or("").to_string()));
    }
    Some(l)
}

/// output name -> logical geometry from `kscreen-doctor -o` (ANSI colours stripped; one- or multi-line format)
pub fn parse_kscreen(text: &str) -> Vec<(String, [i32; 4])> {
    let mut clean = String::new();
    let mut esc = false;
    for ch in text.chars() {
        if ch == '\u{1b}' {
            esc = true;
            continue;
        }
        if esc {
            if ch.is_ascii_alphabetic() {
                esc = false;
            }
            continue;
        }
        clean.push(ch);
    }
    let mut out = vec![];
    for chunk in clean.split("Output:").skip(1) {
        let name = chunk.split_whitespace().nth(1).unwrap_or("").to_string();
        let Some(g) = chunk.split("Geometry:").nth(1) else { continue };
        let mut it = g.split_whitespace();
        let (Some(pos), Some(size)) = (it.next(), it.next()) else { continue };
        let p: Vec<i32> = pos.split(',').filter_map(|x| x.parse().ok()).collect();
        let s: Vec<i32> = size.split('x').filter_map(|x| x.parse().ok()).collect();
        if p.len() == 2 && s.len() == 2 && !name.is_empty() {
            out.push((name, [p[0], p[1], s[0], s[1]]));
        }
    }
    out
}

fn is_internal(name: &str, internal: &str) -> bool {
    if internal != "auto" && !internal.is_empty() {
        return name == internal;
    }
    name.starts_with("DSI") || name.starts_with("eDP") || name.starts_with("LVDS")
}

/// (internal Plasma screen, external Plasma screen) from the layout + the kscreen names
pub fn pick_screens(l: &PlasmaLayout, names: &[(String, [i32; 4])], internal: &str) -> Result<(i32, i32), String> {
    if l.screens.len() < 2 {
        return Err(format!("Plasma has {} screen(s)", l.screens.len()));
    }
    let name_of = |g: &[i32; 4]| names.iter().find(|(_, ng)| ng == g).or_else(|| names.iter().find(|(_, ng)| ng[0] == g[0] && ng[1] == g[1])).map(|n| n.0.clone());
    let int = l.screens.iter().find(|(_, g)| name_of(g).is_some_and(|n| is_internal(&n, internal))).map(|s| s.0).ok_or("internal screen not found among the Plasma screens (kscreen-doctor missing?)")?;
    let ext = l.screens.iter().find(|(i, _)| *i != int).map(|s| s.0).ok_or("no external Plasma screen")?;
    Ok((int, ext))
}

fn set_screens_js(map: &[(i32, i32)]) -> String {
    let m = map.iter().map(|(id, s)| format!("\"{}\":{}", id, s)).collect::<Vec<_>>().join(",");
    format!("var M={{{}}};var r=[];var ps=panels();for(var i=0;i<ps.length;i++){{var p=ps[i];var t=M[String(p.id)];if(t!==undefined){{p.screen=t;r.push(p.id+\":\"+p.screen);}}}}print(r.join(\",\"));", m)
}

fn plasma_layout(c: &Connection) -> Result<PlasmaLayout, String> {
    let s = plasma_eval(c, LAYOUT_JS)?;
    parse_layout(&s).ok_or_else(|| format!("layout script output not understood: {}", s.chars().take(120).collect::<String>()))
}

fn kscreen_names() -> Vec<(String, [i32; 4])> {
    std::process::Command::new("kscreen-doctor")
        .arg("-o")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .map(|o| parse_kscreen(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// move the panels of the internal screen to the external one; their old screens go to `state` first
pub fn panels_move(internal: &str, state: &Path, test_target: Option<i32>) -> String {
    let c = match Connection::session() {
        Ok(c) => c,
        Err(e) => return format!("PANELS skip: no session bus ({})", e),
    };
    let l = match plasma_layout(&c) {
        Ok(l) => l,
        Err(e) => return format!("PANELS skip: not Plasma or scripting refused ({})", e),
    };
    if l.panels.is_empty() {
        return "PANELS none: the session has no panels".into();
    }
    // test mode (Some): with fewer than two Plasma screens, a same-screen "move" of the first panel's screen
    let test_same = test_target.is_some() && l.screens.len() < 2;
    let (int, ext) = if test_same {
        let s = l.panels[0].1;
        (s, s)
    } else {
        match pick_screens(&l, &kscreen_names(), internal) {
            Ok(x) => x,
            Err(e) => return format!("PANELS skip: {}", e),
        }
    };
    let moving: Vec<(i32, i32)> = l.panels.iter().filter(|p| p.1 == int).map(|p| (p.0, p.1)).collect();
    if moving.is_empty() {
        return format!("PANELS none on the internal screen {} (panels: {:?})", int, l.panels);
    }
    // the restore record first: a crash after this point is undone at the next start
    let rec = serde_json::json!({ "panels": moving.iter().map(|(i, s)| [*i, *s]).collect::<Vec<_>>() });
    if let Some(d) = state.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Err(e) = std::fs::write(state, rec.to_string()) {
        return format!("PANELS skip: state file {}: {}", state.display(), e);
    }
    let map: Vec<(i32, i32)> = moving.iter().map(|(i, _)| (*i, ext)).collect();
    match plasma_eval(&c, &set_screens_js(&map)) {
        Ok(r) => format!(
            "PANELS moved t={} from={} to={} panels={} result={}{}",
            crate::sys::mono_ms(),
            int,
            ext,
            moving.len(),
            r.trim(),
            if test_same { " (test: one Plasma screen, same-screen move)" } else { "" }
        ),
        Err(e) => format!("PANELS move failed: {} (state kept for the restore)", e),
    }
}

/// put the panels back from `state`; None = nothing recorded
pub fn panels_restore(state: &Path, why: &str) -> Option<String> {
    let s = std::fs::read_to_string(state).ok()?;
    let v: serde_json::Value = match serde_json::from_str(&s) {
        Ok(v) => v,
        Err(_) => {
            let _ = std::fs::remove_file(state);
            return Some("PANELS bad state file removed".into());
        }
    };
    let map: Vec<(i32, i32)> = v
        .get("panels")
        .and_then(|p| p.as_array())
        .map(|a| a.iter().filter_map(|x| Some((x.get(0)?.as_i64()? as i32, x.get(1)?.as_i64()? as i32))).collect())
        .unwrap_or_default();
    let c = match Connection::session() {
        Ok(c) => c,
        Err(e) => return Some(format!("PANELS restore failed: no session bus ({}); state kept", e)),
    };
    match plasma_eval(&c, &set_screens_js(&map)) {
        Ok(r) => {
            // read back: every recorded panel on its old screen again
            let ok = plasma_layout(&c).map(|l| map.iter().all(|(id, sc)| l.panels.iter().any(|p| p.0 == *id && p.1 == *sc))).unwrap_or(false);
            if ok {
                let _ = std::fs::remove_file(state);
            }
            Some(format!("PANELS restored t={} why={} panels={} result={} verified={}", crate::sys::mono_ms(), why, map.len(), r.trim(), ok))
        }
        Err(e) => Some(format!("PANELS restore failed: {} (state kept)", e)),
    }
}

// ------------------------------------------------------------------ M2.1: the desktop look

fn kde_cfg(name: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    std::fs::read_to_string(Path::new(&home).join(".config").join(name)).ok()
}

/// the KDE look; the second value is a log line
pub fn desk_look() -> (Option<dexhid_core::feedback::DeskLook>, String) {
    let Some(kg) = kde_cfg("kdeglobals") else { return (None, "THEME-DESKTOP no kdeglobals: built-in theme".into()) };
    let radius = ["klassyrc", "breezerc"].iter().find_map(|f| {
        let m = dexhid_core::feedback::parse_ini(&kde_cfg(f)?);
        m.iter().find(|((_, k), _)| k == "WindowCornerRadius" || k == "CornerRadius").and_then(|(_, v)| v.parse::<f32>().ok())
    });
    let d = dexhid_core::feedback::desk_look(&kg, kde_cfg("plasmashellrc").as_deref(), kde_cfg("kwinrc").as_deref(), radius);
    let note = match &d {
        Some(x) => format!(
            "THEME-DESKTOP scheme={} window=#{:06x} view=#{:06x} button=#{:06x} accent=#{:06x} translucent={} blur={} radius_mm={:.1}",
            if x.scheme.is_empty() { "-" } else { &x.scheme },
            x.window & 0xffffff,
            x.view & 0xffffff,
            x.button & 0xffffff,
            x.accent & 0xffffff,
            x.translucent,
            x.blur,
            x.radius_mm
        ),
        None => "THEME-DESKTOP kdeglobals has no colour scheme colours: built-in theme".into(),
    };
    (d, note)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M4 fix: the USB target's "desktop" layout is an xkb code (+ variant), never KDE's display name
    #[test]
    fn kde_layouts_resolve_to_xkb_codes() {
        let l = |a: &str, b: &str, c: &str| (a.to_string(), b.to_string(), c.to_string());
        let list = vec![l("us", "en", "English (US)"), l("de", "de", "German (no dead keys)"), l("fr", "", "French")];
        let rc = "[Layout]\nDisplayNames=,,\nLayoutList=us,de,fr\nUse=true\nVariantList=,nodeadkeys,\n";
        assert_eq!(xkb_codes(&list, Some(rc)), vec!["us", "de(nodeadkeys)", "fr"]);
        assert_eq!(xkb_codes(&list, None), vec!["us", "de", "fr"]);
        // no D-Bus list: kxkbrc alone; a broken short name never leaks a display name
        assert_eq!(xkb_codes(&[], Some(rc)), vec!["us", "de(nodeadkeys)", "fr"]);
        assert_eq!(xkb_codes(&[l("", "", "English (US)")], None), vec!["us"]);
        assert_eq!(crate::xkb::split_layout("de(nodeadkeys)"), ("de".to_string(), Some("nodeadkeys".to_string())));
        assert_eq!(crate::xkb::split_layout("us"), ("us".to_string(), None));
    }

    #[test]
    fn plasma_layout_kscreen_and_screen_pick() {
        let l = parse_layout("{\"screens\":[[0,0,0,1696,1200],[1,1696,0,2560,1440]],\"panels\":[[2,0,\"bottom\"],[40,1,\"top\"]]}\n").unwrap();
        assert_eq!(l.screens.len(), 2);
        assert_eq!(l.panels[0], (2, 0, "bottom".to_string()));
        let ks = "\u{1b}[01;32mOutput: \u{1b}[0;0m1 DSI-1 enabled connected priority 2 Panel\n\tGeometry: 0,0 1696x1200\n\tScale: 1.6\nOutput: 2 DP-1 enabled connected priority 1\n\tGeometry: 1696,0 2560x1440\n";
        let n = parse_kscreen(ks);
        assert_eq!(n, vec![("DSI-1".to_string(), [0, 0, 1696, 1200]), ("DP-1".to_string(), [1696, 0, 2560, 1440])]);
        assert_eq!(pick_screens(&l, &n, "auto").unwrap(), (0, 1));
        assert_eq!(pick_screens(&l, &n, "DP-1").unwrap(), (1, 0), "explicit internal name");
        let one = PlasmaLayout { screens: vec![(0, [0, 0, 1696, 1200])], panels: vec![] };
        assert!(pick_screens(&one, &n, "auto").unwrap_err().contains("1 screen"));
        let js = set_screens_js(&[(2, 1), (7, 1)]);
        assert!(js.contains("var M={\"2\":1,\"7\":1}") && js.contains("p.screen=t"));
    }

    #[test]
    fn restore_state_file_without_plasma() {
        let d = std::env::temp_dir().join(format!("dexhid-panels-{}", std::process::id()));
        let f = d.join("panels.json");
        assert!(panels_restore(&f, "start").is_none(), "nothing recorded: no-op");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(&f, "not json").unwrap();
        assert!(panels_restore(&f, "start").unwrap().contains("bad state"));
        assert!(!f.exists());
        std::fs::remove_dir_all(&d).ok();
    }
}
