// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! M3 mirror preview: a small, low-fps picture of the external monitor in a corner of the touchpad (off by default).
//! Capture goes through the standard xdg-desktop-portal ScreenCast interface (any desktop with a portal backend):
//! CreateSession -> SelectSources (one monitor, cursor embedded, persist_mode 2 with the saved restore token, so the
//! desktop asks only the first time) -> Start -> OpenPipeWireRemote. The PipeWire stream is decoded by GStreamer's
//! pipewiresrc in a child process (`gst-launch-1.0 ... ! fdsink`, raw BGRx frames of a fixed small size on its
//! stdout), so DexHid links no PipeWire / GStreamer library. Frames go to the UI through Shared.mirror.
//! Nothing is stored except the portal's restore token ($XDG_STATE_HOME/dexhid/mirror-token).

use crate::shared::{Cmd, SharedRef};
use std::collections::HashMap;
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use zbus::blocking::Connection;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

pub struct Mirror {
    stop: Arc<AtomicBool>,
    child: Arc<std::sync::Mutex<Option<u32>>>,
}

impl Mirror {
    pub fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(pid) = *self.child.lock().unwrap() {
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
    }
}

fn token_file() -> PathBuf {
    let base = match std::env::var("XDG_STATE_HOME") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".local/state"),
    };
    base.join("dexhid/mirror-token")
}

/// gst-launch-1.0 + the pipewiresrc element present?
pub fn gst_ok() -> Result<(), String> {
    if !crate::desktop::have_program("gst-launch-1.0") {
        return Err("gst-launch-1.0 missing (gstreamer1 tools)".into());
    }
    let ok = std::process::Command::new("gst-inspect-1.0").arg("pipewiresrc").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err("GStreamer element pipewiresrc missing (pipewire-gstreamer)".into())
    }
}

/// the gst-launch argv for a node read from fd 3, scaled to w x h, at fps, raw BGRx on stdout
pub fn gst_args(node: u32, w: u32, h: u32, fps: u32) -> Vec<String> {
    format!("-q pipewiresrc fd=3 path={} always-copy=true ! videorate drop-only=true ! video/x-raw,framerate={}/1 ! videoconvert ! videoscale ! video/x-raw,format=BGRx,width={},height={} ! fdsink fd=1 sync=false", node, fps.clamp(1, 10), w, h)
        .split_whitespace()
        .map(|s| s.to_string())
        .collect()
}

/// preview size for a source size (at most 480 wide, even numbers)
pub fn preview_size(sw: i32, sh: i32) -> (u32, u32) {
    let (sw, sh) = (sw.max(16) as f32, sh.max(16) as f32);
    let w = 480.0f32.min(sw);
    let h = (w * sh / sw).round();
    ((w as u32) & !1, (h as u32).max(2) & !1)
}

struct Portal<'a> {
    c: &'a Connection,
    sender: String,
    n: u32,
}

impl Portal<'_> {
    /// call a ScreenCast method that answers through a Request object; wait for its Response (up to `secs`)
    fn request(&mut self, method: &str, session: Option<&OwnedObjectPath>, mut opts: HashMap<&str, Value>, secs: u64) -> Result<HashMap<String, OwnedValue>, String> {
        self.n += 1;
        let tok = format!("dexhid{}_{}", std::process::id(), self.n);
        let path = format!("/org/freedesktop/portal/desktop/request/{}/{}", self.sender, tok);
        let proxy = zbus::blocking::Proxy::new(self.c, "org.freedesktop.portal.Desktop", path.clone(), "org.freedesktop.portal.Request").map_err(|e| e.to_string())?;
        let mut sig = proxy.receive_signal("Response").map_err(|e| e.to_string())?;
        opts.insert("handle_token", Value::from(tok.clone()));
        let dest = Some("org.freedesktop.portal.Desktop");
        let p = "/org/freedesktop/portal/desktop";
        let i = Some("org.freedesktop.portal.ScreenCast");
        let r = match (method, session) {
            ("CreateSession", _) => self.c.call_method(dest, p, i, method, &(opts,)),
            ("Start", Some(s)) => self.c.call_method(dest, p, i, method, &(s, "", opts)),
            (_, Some(s)) => self.c.call_method(dest, p, i, method, &(s, opts)),
            _ => return Err("no session".into()),
        };
        r.map_err(|e| format!("{}: {}", method, e))?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Some(m) = sig.next() {
                let _ = tx.send(m.body().deserialize::<(u32, HashMap<String, OwnedValue>)>().map_err(|e| e.to_string()));
            }
        });
        match rx.recv_timeout(std::time::Duration::from_secs(secs)) {
            Ok(Ok((0, res))) => Ok(res),
            Ok(Ok((code, _))) => Err(format!("{}: the portal answered {} ({})", method, code, if code == 1 { "cancelled by the user" } else { "failed" })),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(format!("{}: no answer within {} s (a desktop dialog waits for the user?)", method, secs)),
        }
    }
}

fn s(v: &OwnedValue) -> Option<String> {
    String::try_from(v.try_clone().ok()?).ok()
}

/// start the preview (portal session + gst child); frames go to the UI, notes to the daemon log
pub fn start(sh: SharedRef, fps: u32) -> Mirror {
    let stop = Arc::new(AtomicBool::new(false));
    let child = Arc::new(std::sync::Mutex::new(None));
    let (st2, ch2) = (stop.clone(), child.clone());
    std::thread::Builder::new()
        .name("mirror".into())
        .spawn(move || {
            let r = run(&sh, fps, &st2, &ch2);
            sh.send(Cmd::Note(match r {
                Ok(()) => "MIRROR stopped".into(),
                Err(e) => format!("MIRROR unavailable: {}", e),
            }));
            *sh.mirror.lock().unwrap() = None;
            crate::sys::poke(sh.wake_ui);
        })
        .ok();
    Mirror { stop, child }
}

fn run(sh: &SharedRef, fps: u32, stop: &AtomicBool, child: &std::sync::Mutex<Option<u32>>) -> Result<(), String> {
    gst_ok()?;
    let c = Connection::session().map_err(|e| format!("session bus: {}", e))?;
    let sender = c.unique_name().map(|n| n.trim_start_matches(':').replace('.', "_")).ok_or("no unique name")?;
    let mut p = Portal { c: &c, sender, n: 0 };
    let mut o = HashMap::new();
    o.insert("session_handle_token", Value::from(format!("dexhids{}", std::process::id())));
    let r = p.request("CreateSession", None, o, 10)?;
    let sess = r.get("session_handle").and_then(s).ok_or("no session_handle")?;
    let sess = OwnedObjectPath::from(ObjectPath::try_from(sess).map_err(|e| e.to_string())?);
    let mut o = HashMap::new();
    o.insert("types", Value::U32(1));
    o.insert("multiple", Value::Bool(false));
    o.insert("cursor_mode", Value::U32(2));
    o.insert("persist_mode", Value::U32(2));
    if let Ok(t) = std::fs::read_to_string(token_file()) {
        o.insert("restore_token", Value::from(t.trim().to_string()));
    }
    p.request("SelectSources", Some(&sess), o, 10)?;
    sh.send(Cmd::Note("MIRROR portal: Start (the desktop may ask which screen to share)".into()));
    let r = p.request("Start", Some(&sess), HashMap::new(), 120)?;
    if let Some(t) = r.get("restore_token").and_then(s) {
        let f = token_file();
        let _ = std::fs::create_dir_all(f.parent().unwrap());
        let _ = std::fs::write(&f, t);
    }
    let streams: Vec<(u32, HashMap<String, OwnedValue>)> = r.get("streams").and_then(|v| v.try_clone().ok()).and_then(|v| Vec::try_from(v).ok()).ok_or("no streams")?;
    let (node, props) = streams.into_iter().next().ok_or("no stream")?;
    let size: (i32, i32) = props.get("size").and_then(|v| v.try_clone().ok()).and_then(|v| <(i32, i32)>::try_from(v).ok()).unwrap_or((1920, 1080));
    let m = c
        .call_method(Some("org.freedesktop.portal.Desktop"), "/org/freedesktop/portal/desktop", Some("org.freedesktop.portal.ScreenCast"), "OpenPipeWireRemote", &(&sess, HashMap::<&str, Value>::new()))
        .map_err(|e| format!("OpenPipeWireRemote: {}", e))?;
    let fd: zbus::zvariant::OwnedFd = m.body().deserialize().map_err(|e| e.to_string())?;
    let fd: OwnedFd = fd.into();
    let (w, h) = preview_size(size.0, size.1);
    sh.send(Cmd::MirrorSize(size.0.max(1) as u32, size.1.max(1) as u32));
    sh.send(Cmd::Note(format!("MIRROR streaming node={} source={}x{} preview={}x{} fps={}", node, size.0, size.1, w, h, fps)));
    let raw = fd.as_raw_fd();
    let mut cmd = std::process::Command::new("gst-launch-1.0");
    cmd.args(gst_args(node, w, h, fps)).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            if libc::dup2(raw, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut ch = cmd.spawn().map_err(|e| format!("gst-launch-1.0: {}", e))?;
    *child.lock().unwrap() = Some(ch.id());
    drop(fd);
    let mut out = ch.stdout.take().unwrap();
    let mut frame = vec![0u8; (w * h * 4) as usize];
    let mut frames = 0u64;
    let res = loop {
        if stop.load(Ordering::SeqCst) {
            break Ok(());
        }
        if let Err(e) = out.read_exact(&mut frame) {
            break if stop.load(Ordering::SeqCst) { Ok(()) } else { Err(format!("gst stream ended: {}", e)) };
        }
        frames += 1;
        let px = dexhid_core::extras::downscale(&frame, w, h, w * 4, w, h);
        *sh.mirror.lock().unwrap() = Some((w, h, px));
        sh.mirror_gen.fetch_add(1, Ordering::SeqCst);
        crate::sys::poke(sh.wake_ui);
        if frames == 1 {
            sh.send(Cmd::Note(format!("MIRROR first frame {}x{}", w, h)));
        }
    };
    let _ = ch.kill();
    let _ = ch.wait();
    let _ = c.call_method(Some("org.freedesktop.portal.Desktop"), sess.as_str(), Some("org.freedesktop.portal.Session"), "Close", &());
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gst_argv_and_size() {
        let a = gst_args(42, 480, 270, 2);
        assert_eq!(a[0], "-q");
        assert!(a.contains(&"path=42".to_string()) && a.contains(&"fd=3".to_string()));
        assert!(a.iter().any(|x| x == "video/x-raw,format=BGRx,width=480,height=270"));
        assert!(a.iter().any(|x| x == "video/x-raw,framerate=2/1"));
        assert!(gst_args(1, 2, 2, 50).iter().any(|x| x == "video/x-raw,framerate=10/1"), "fps capped");
        assert_eq!(preview_size(1920, 1080), (480, 270));
        assert_eq!(preview_size(2560, 1440), (480, 270));
        assert_eq!(preview_size(1920, 1200), (480, 300));
        assert_eq!(preview_size(320, 200), (320, 200));
    }
}
