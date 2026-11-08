// SPDX-License-Identifier: GPL-3.0-or-later
//! M3 clipboard history for the strip. Two backends:
//! - KDE Klipper (org.kde.klipper /klipper): the desktop's own history (getClipboardHistoryMenu, the
//!   clipboardHistoryUpdated signal, setClipboardContents) - text only;
//! - the wlroots data-control protocol (zwlr_data_control_manager_v1; KWin, sway, Hyprland, niri, COSMIC offer it):
//!   DexHid keeps its own history from the selection events, in memory only (never written to disk); text, and
//!   images (image/png) when the option is on. Setting an item offers it as the new selection.
//! Klipper is used when it runs and images are off; otherwise data-control. GNOME has neither (no history).
//! The daemon pastes with Ctrl+V after setting the selection (desktop target) or types the text (HID targets).

use crate::shared::{Cmd, SharedRef};
use dexhid_core::extras::{ClipData, ClipHistory, CLIP_MAX_BYTES};
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::mpsc::{channel, Receiver, Sender};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

pub struct ClipCtl {
    tx: Sender<ClipData>,
    pub backend: &'static str,
}

impl ClipCtl {
    /// make `d` the clipboard selection
    pub fn set(&self, d: ClipData) {
        let _ = self.tx.send(d);
    }
}

fn klipper_running() -> bool {
    zbus::blocking::Connection::session()
        .ok()
        .and_then(|c| c.call_method(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", Some("org.freedesktop.DBus"), "NameHasOwner", &("org.kde.klipper",)).ok())
        .and_then(|m| m.body().deserialize::<bool>().ok())
        .unwrap_or(false)
}

/// start the history (Cmd::Clips with the items, newest first)
pub fn start(sh: SharedRef, cap: usize, images: bool) -> ClipCtl {
    let (tx, rx) = channel::<ClipData>();
    if !images && klipper_running() {
        let sh2 = sh.clone();
        std::thread::Builder::new().name("clip-klipper".into()).spawn(move || klipper(sh2, cap)).ok();
        std::thread::Builder::new()
            .name("clip-klipper-set".into())
            .spawn(move || {
                while let Ok(d) = rx.recv() {
                    if let ClipData::Text(t) = d {
                        let r = zbus::blocking::Connection::session()
                            .map_err(|e| e.to_string())
                            .and_then(|c| c.call_method(Some("org.kde.klipper"), "/klipper", Some("org.kde.klipper.klipper"), "setClipboardContents", &(t.as_str(),)).map_err(|e| e.to_string()));
                        sh.send(Cmd::Note(format!("CLIP-SET backend=klipper ok={}", r.is_ok())));
                    }
                }
            })
            .ok();
        return ClipCtl { tx, backend: "klipper" };
    }
    std::thread::Builder::new().name("clip-dc".into()).spawn(move || data_control(sh, cap, images, rx)).ok();
    ClipCtl { tx, backend: "data-control" }
}

fn klipper_items(c: &zbus::blocking::Connection, cap: usize) -> Option<Vec<ClipData>> {
    let m = c.call_method(Some("org.kde.klipper"), "/klipper", Some("org.kde.klipper.klipper"), "getClipboardHistoryMenu", &()).ok()?;
    let v: Vec<String> = m.body().deserialize().ok()?;
    let mut h = ClipHistory::new(cap);
    for t in v.into_iter().rev() {
        h.push(ClipData::Text(t));
    }
    Some(h.items.into_iter().collect())
}

fn klipper(sh: SharedRef, cap: usize) {
    let Ok(c) = zbus::blocking::Connection::session() else { return };
    if let Some(v) = klipper_items(&c, cap) {
        sh.send(Cmd::Clips(v, "klipper".into()));
    }
    let Ok(p) = zbus::blocking::Proxy::new(&c, "org.kde.klipper", "/klipper", "org.kde.klipper.klipper") else { return };
    let Ok(sig) = p.receive_signal("clipboardHistoryUpdated") else { return };
    for _ in sig {
        if let Some(v) = klipper_items(&c, cap) {
            sh.send(Cmd::Clips(v, "klipper".into()));
        }
    }
}

// ------------------------------------------------------------------------------------------- data-control

struct Dc {
    seat: Option<wl_seat::WlSeat>,
    mgr: Option<ZwlrDataControlManagerV1>,
    mimes: HashMap<u32, Vec<String>>,
    selection: Option<ZwlrDataControlOfferV1>,
    new_selection: bool,
    /// what we offer as the selection now
    offering: Option<(ZwlrDataControlSourceV1, ClipData)>,
    finished: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Dc {
    fn event(st: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "wl_seat" if st.seat.is_none() => st.seat = Some(reg.bind(name, version.min(2), qh, ())),
                "zwlr_data_control_manager_v1" => st.mgr = Some(reg.bind(name, version.min(2), qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for Dc {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for Dc {
    fn event(_: &mut Self, _: &ZwlrDataControlManagerV1, _: <ZwlrDataControlManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for Dc {
    fn event(st: &mut Self, _: &ZwlrDataControlDeviceV1, ev: zwlr_data_control_device_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                st.mimes.insert(id.id().protocol_id(), vec![]);
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                if let Some(old) = st.selection.take() {
                    st.mimes.remove(&old.id().protocol_id());
                    old.destroy();
                }
                st.selection = id;
                st.new_selection = true;
            }
            zwlr_data_control_device_v1::Event::PrimarySelection { id: Some(o) } => {
                st.mimes.remove(&o.id().protocol_id());
                o.destroy();
            }
            zwlr_data_control_device_v1::Event::Finished => st.finished = true,
            _ => {}
        }
    }
    event_created_child!(Dc, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for Dc {
    fn event(st: &mut Self, o: &ZwlrDataControlOfferV1, ev: zwlr_data_control_offer_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = ev {
            st.mimes.entry(o.id().protocol_id()).or_default().push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for Dc {
    fn event(st: &mut Self, s: &ZwlrDataControlSourceV1, ev: zwlr_data_control_source_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                if let Some((_, d)) = st.offering.as_ref().filter(|(x, _)| x == s) {
                    let bytes: Vec<u8> = match d {
                        ClipData::Text(t) if mime_type.starts_with("text/") || mime_type == "UTF8_STRING" || mime_type == "STRING" || mime_type == "TEXT" => t.as_bytes().to_vec(),
                        ClipData::Image(m, b) if *m == mime_type => b.clone(),
                        _ => vec![],
                    };
                    // a small writer thread: the reader may be slow
                    std::thread::spawn(move || {
                        let mut f = std::fs::File::from(fd);
                        let _ = std::io::Write::write_all(&mut f, &bytes);
                    });
                }
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                if st.offering.as_ref().is_some_and(|(x, _)| x == s) {
                    st.offering = None;
                }
                s.destroy();
            }
            _ => {}
        }
    }
}

const TEXT_MIMES: &[&str] = &["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "STRING", "TEXT"];

/// read one selection (pipe, at most CLIP_MAX_BYTES, 1 s)
fn receive(conn: &Connection, o: &ZwlrDataControlOfferV1, mime: &str) -> Option<Vec<u8>> {
    let mut p = [0; 2];
    if unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return None;
    }
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(p[0]), OwnedFd::from_raw_fd(p[1])) };
    o.receive(mime.to_string(), w.as_fd());
    let _ = conn.flush();
    drop(w);
    let mut out = vec![];
    let mut buf = vec![0u8; 65536];
    let t0 = crate::sys::mono_ms();
    loop {
        if crate::sys::mono_ms() - t0 > 1000 {
            return None;
        }
        if !crate::sys::poll_in(&[r.as_raw_fd()], 200)[0] {
            continue;
        }
        match crate::ipc::read_fd(r.as_raw_fd(), &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > CLIP_MAX_BYTES {
                    return None;
                }
            }
            Err(e) if e == libc::EINTR || e == libc::EAGAIN => continue,
            Err(_) => return None,
        }
    }
    Some(out)
}

fn data_control(sh: SharedRef, cap: usize, images: bool, rx: Receiver<ClipData>) {
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            sh.send(Cmd::Note(format!("CLIP backend=data-control unavailable: no Wayland connection ({})", e)));
            return;
        }
    };
    let mut q = conn.new_event_queue::<Dc>();
    let qh = q.handle();
    let _reg = conn.display().get_registry(&qh, ());
    let mut st = Dc { seat: None, mgr: None, mimes: HashMap::new(), selection: None, new_selection: false, offering: None, finished: false };
    if q.roundtrip(&mut st).is_err() {
        return;
    }
    let (Some(mgr), Some(seat)) = (st.mgr.clone(), st.seat.clone()) else {
        sh.send(Cmd::Note("CLIP backend=data-control unavailable: the compositor offers no zwlr_data_control_manager_v1 (GNOME): no clipboard history".into()));
        return;
    };
    let dev = mgr.get_data_device(&seat, &qh, ());
    let _ = q.roundtrip(&mut st);
    sh.send(Cmd::Note(format!("CLIP backend=data-control ready images={}", images)));
    let (wake_rd, wake_wr) = crate::sys::wake_pipe();
    // the daemon's set requests wake this thread
    let (tx2, rx2) = channel::<ClipData>();
    std::thread::spawn(move || {
        while let Ok(d) = rx.recv() {
            if tx2.send(d).is_err() {
                break;
            }
            crate::sys::poke(wake_wr);
        }
    });
    let mut hist = ClipHistory::new(cap);
    loop {
        if st.finished {
            sh.send(Cmd::Note("CLIP data-control device finished".into()));
            return;
        }
        if st.new_selection {
            st.new_selection = false;
            if let Some(o) = st.selection.clone() {
                let m = st.mimes.get(&o.id().protocol_id()).cloned().unwrap_or_default();
                let text = TEXT_MIMES.iter().find(|t| m.iter().any(|x| x == **t));
                let got = if let Some(t) = text {
                    receive(&conn, &o, t).and_then(|b| String::from_utf8(b).ok()).map(ClipData::Text)
                } else if images && m.iter().any(|x| x == "image/png") {
                    receive(&conn, &o, "image/png").map(|b| ClipData::Image("image/png".into(), b))
                } else {
                    None
                };
                if let Some(d) = got {
                    if hist.push(d) {
                        sh.send(Cmd::Clips(hist.items.iter().cloned().collect(), "data-control".into()));
                    }
                }
            }
        }
        while let Ok(d) = rx2.try_recv() {
            let src = mgr.create_data_source(&qh, ());
            match &d {
                ClipData::Text(_) => {
                    for m in TEXT_MIMES {
                        src.offer(m.to_string());
                    }
                }
                ClipData::Image(m, _) => src.offer(m.clone()),
            }
            dev.set_selection(Some(&src));
            if let Some((old, _)) = st.offering.take() {
                old.destroy();
            }
            st.offering = Some((src, d));
            sh.send(Cmd::Note("CLIP-SET backend=data-control ok=true".into()));
        }
        let _ = q.dispatch_pending(&mut st);
        let _ = conn.flush();
        let Some(guard) = conn.prepare_read() else { continue };
        let fd = guard.connection_fd().as_raw_fd();
        let r = crate::sys::poll_in(&[fd, wake_rd], 60_000);
        if r[1] {
            crate::sys::drain(wake_rd);
        }
        if r[0] {
            if guard.read().is_err() {
                sh.send(Cmd::Note("CLIP data-control connection lost".into()));
                return;
            }
        } else {
            drop(guard);
        }
        if q.dispatch_pending(&mut st).is_err() {
            return;
        }
    }
}
