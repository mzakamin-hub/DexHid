// SPDX-License-Identifier: GPL-3.0-or-later
//! logind watcher (system bus): the user's graphical session (User.Display) LockedHint / Active and its Lock /
//! Unlock signals, Manager.PrepareForSleep with a delay inhibitor (the grab is released before the sleep), LidClosed.

use crate::shared::{Cmd, SharedRef};
use std::time::Duration;
use zbus::blocking::Connection;
use zbus::zvariant::{OwnedFd, OwnedObjectPath, OwnedValue};

const LOGIN1: &str = "org.freedesktop.login1";
const MGR_PATH: &str = "/org/freedesktop/login1";
const MGR: &str = "org.freedesktop.login1.Manager";
const SESSION: &str = "org.freedesktop.login1.Session";

fn get_prop(c: &Connection, path: &str, iface: &str, prop: &str) -> Option<OwnedValue> {
    let m = c.call_method(Some(LOGIN1), path, Some("org.freedesktop.DBus.Properties"), "Get", &(iface, prop)).ok()?;
    m.body().deserialize::<OwnedValue>().ok()
}

fn get_bool(c: &Connection, path: &str, iface: &str, prop: &str) -> Option<bool> {
    get_prop(c, path, iface, prop).and_then(|v| bool::try_from(v).ok())
}

pub fn session_path(c: &Connection) -> Option<String> {
    let uid = unsafe { libc::getuid() };
    let m = c.call_method(Some(LOGIN1), MGR_PATH, Some(MGR), "GetUser", &(uid,)).ok()?;
    let user: OwnedObjectPath = m.body().deserialize().ok()?;
    let v = get_prop(c, user.as_str(), "org.freedesktop.login1.User", "Display")?;
    let (id, path): (String, OwnedObjectPath) = v.try_into().ok()?;
    if id.is_empty() {
        return None;
    }
    Some(path.as_str().to_string())
}

fn inhibit(c: &Connection) -> Option<OwnedFd> {
    let m = c
        .call_method(Some(LOGIN1), MGR_PATH, Some(MGR), "Inhibit", &("sleep", "dexhid", "Release the touchscreen before sleep", "delay"))
        .ok()?;
    m.body().deserialize::<OwnedFd>().ok()
}

/// runs forever in its own thread; sends Cmd::Locked / SessionActive / Sleep / Lid to the core
pub fn run(sh: SharedRef) {
    let c = match Connection::system() {
        Ok(c) => c,
        Err(e) => {
            sh.send(Cmd::Note(format!("WARN logind: no system bus ({}): lock / sleep pause disabled", e)));
            return;
        }
    };
    let sp = session_path(&c);
    match &sp {
        Some(p) => sh.send(Cmd::Note(format!("logind: graphical session {}", p))),
        None => sh.send(Cmd::Note("WARN logind: no graphical session for this user: lock pause disabled".into())),
    }
    let read_session = |c: &Connection| {
        if let Some(p) = &sp {
            if let Some(l) = get_bool(c, p, SESSION, "LockedHint") {
                sh.send(Cmd::Locked(l));
            }
            if let Some(a) = get_bool(c, p, SESSION, "Active") {
                sh.send(Cmd::SessionActive(a));
            }
        }
    };
    read_session(&c);
    if let Some(l) = get_bool(&c, MGR_PATH, MGR, "LidClosed") {
        sh.send(Cmd::Lid(l));
    }
    let mut inh = inhibit(&c);
    if inh.is_none() {
        sh.send(Cmd::Note("WARN logind: no sleep delay inhibitor (sleep still pauses, maybe late)".into()));
    }
    let rule = format!("type='signal',sender='{}'", LOGIN1);
    let it = match zbus::blocking::MessageIterator::for_match_rule(rule.as_str(), &c, Some(64)) {
        Ok(i) => i,
        Err(e) => {
            sh.send(Cmd::Note(format!("WARN logind: signal subscription failed: {}", e)));
            return;
        }
    };
    for m in it {
        let Ok(m) = m else { continue };
        let h = m.header();
        let member = h.member().map(|x| x.as_str().to_string()).unwrap_or_default();
        let path = h.path().map(|x| x.as_str().to_string()).unwrap_or_default();
        match member.as_str() {
            "PrepareForSleep" => {
                let Ok(going) = m.body().deserialize::<bool>() else { continue };
                if going {
                    let (tx, rx) = std::sync::mpsc::sync_channel(1);
                    sh.send(Cmd::Sleep(true, Some(tx)));
                    // the core releases the grab, then acks; never hold the sleep longer than 1 s
                    let _ = rx.recv_timeout(Duration::from_millis(1000));
                    inh = None;
                } else {
                    sh.send(Cmd::Sleep(false, None));
                    if inh.is_none() {
                        inh = inhibit(&c);
                    }
                }
            }
            "Lock" if Some(&path) == sp.as_ref() => sh.send(Cmd::Locked(true)),
            "Unlock" if Some(&path) == sp.as_ref() => {
                // the locker clears LockedHint once it is really gone; read it back shortly
                std::thread::sleep(Duration::from_millis(300));
                read_session(&c);
            }
            "PropertiesChanged" if Some(&path) == sp.as_ref() => read_session(&c),
            "PropertiesChanged" if path == MGR_PATH => {
                if let Some(l) = get_bool(&c, MGR_PATH, MGR, "LidClosed") {
                    sh.send(Cmd::Lid(l));
                }
            }
            _ => {}
        }
    }
    drop(inh);
}
