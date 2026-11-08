// SPDX-License-Identifier: GPL-3.0-or-later
//! Session-bus API `org.dexhid.DexHid1` at /org/dexhid/DexHid1 (see data/org.dexhid.DexHid1.xml) and the CLI client.
//! Methods return at once with a short text; the effect shows in Status() and the StateChanged signal.

use crate::shared::{Cmd, SharedRef};

pub const NAME: &str = "org.dexhid.DexHid1";
pub const PATH: &str = "/org/dexhid/DexHid1";

pub struct Api {
    pub sh: SharedRef,
}

#[zbus::interface(name = "org.dexhid.DexHid1")]
impl Api {
    /// pad <-> display (needs a connected external display)
    fn toggle(&self) -> String {
        self.sh.send(Cmd::Toggle("dbus".into()));
        "ok".into()
    }
    /// leave pad mode (the monitor stays a display)
    fn release(&self) -> String {
        self.sh.send(Cmd::Release("dbus".into()));
        "ok".into()
    }
    /// enter pad mode again
    fn resume(&self) -> String {
        self.sh.send(Cmd::Resume("dbus".into()));
        "ok".into()
    }
    /// JSON (built by the daemon on request)
    fn status(&self) -> zbus::fdo::Result<String> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.sh.send(Cmd::Status(tx));
        rx.recv_timeout(std::time::Duration::from_secs(3)).map_err(|_| zbus::fdo::Error::Failed("daemon did not answer".into()))
    }
    /// M4: open the app window at a page: "home" | "settings" | a settings module id ("pointer", "usbhid", ...)
    fn open_window(&self, page: &str) -> String {
        let p: String = page.chars().take(40).filter(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        self.sh.send(Cmd::OpenWindow(if p.is_empty() { "home".into() } else { p }));
        "ok".into()
    }
    /// M4: close the app window (nothing of it stays: no surface, no buffers)
    fn close_window(&self) -> String {
        self.sh.send(Cmd::CloseWindow);
        "ok".into()
    }
    /// M4: open the app window at the settings
    fn open_settings(&self) -> String {
        self.sh.send(Cmd::OpenWindow("settings".into()));
        "ok".into()
    }
    /// M4: the one mode switch: "usb" | "pad" | "screen"; returns the mode, or an error with the hint (e.g. pad
    /// without an external display)
    fn set_mode(&self, mode: &str) -> zbus::fdo::Result<String> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.sh.send(Cmd::SetMode(mode.to_string(), Some(tx)));
        match rx.recv_timeout(std::time::Duration::from_secs(8)) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(zbus::fdo::Error::Failed(e)),
            Err(_) => Err(zbus::fdo::Error::Failed("daemon did not answer".into())),
        }
    }
    fn set_layout(&self, name: &str) -> zbus::fdo::Result<String> {
        let names = self.sh.layout_names.lock().unwrap().clone();
        if !names.iter().any(|n| n == name) {
            return Err(zbus::fdo::Error::InvalidArgs(format!("unknown layout {:?} (have: {})", name, names.join(", "))));
        }
        self.sh.send(Cmd::SetLayout(name.to_string()));
        Ok("ok".into())
    }
    /// M2: change a setting (key from the settings registry, e.g. "natural_scroll", "speed", "quick:copy");
    /// an empty value steps it like a tap in the settings sheet. Returns the new value.
    fn set_setting(&self, key: &str, value: &str) -> zbus::fdo::Result<String> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.sh.send(Cmd::SetSetting(key.to_string(), value.to_string(), tx));
        match rx.recv_timeout(std::time::Duration::from_secs(3)) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(zbus::fdo::Error::InvalidArgs(e)),
            Err(_) => Err(zbus::fdo::Error::Failed("daemon did not answer".into())),
        }
    }
    /// M2: all settings as JSON {key: value}
    fn get_settings(&self) -> zbus::fdo::Result<String> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.sh.send(Cmd::GetSettings(tx));
        rx.recv_timeout(std::time::Duration::from_secs(3)).map_err(|_| zbus::fdo::Error::Failed("daemon did not answer".into()))
    }
    /// M2: run a quick action by id (as if its button was tapped)
    fn run_quick(&self, id: &str) -> String {
        self.sh.send(Cmd::RunQuick(id.to_string()));
        "ok".into()
    }
    /// M3: the focused window's class (sent by DexHid's KWin script for per-app profiles)
    fn active_app(&self, class: &str) -> String {
        let c: String = class.chars().take(200).filter(|c| !c.is_control()).collect();
        self.sh.send(Cmd::ActiveApp(c));
        "ok".into()
    }
    /// M3: profiles: "list" | "save NAME [monitor] [app=CLASS ..]" | "use NAME" | "auto" | "delete NAME"
    fn profile(&self, cmd: &str) -> zbus::fdo::Result<String> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.sh.send(Cmd::Profile(cmd.to_string(), tx));
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(zbus::fdo::Error::InvalidArgs(e)),
            Err(_) => Err(zbus::fdo::Error::Failed("daemon did not answer".into())),
        }
    }
    /// M3: run a feature action as its touch path would: "gesture NAME" | "emoji-panel" | "emoji E" | "clip N" |
    /// "suggest N" | "select-mode" | "timer toggle|reset" | "laser X Y|up" | "target NAME" | "type TEXT" | "clips"
    fn act(&self, cmd: &str) -> zbus::fdo::Result<String> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.sh.send(Cmd::Act(cmd.to_string(), tx));
        match rx.recv_timeout(std::time::Duration::from_secs(12)) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(zbus::fdo::Error::InvalidArgs(e)),
            Err(_) => Err(zbus::fdo::Error::Failed("daemon did not answer".into())),
        }
    }
    /// M3: USB HID state as JSON (target, helper, gadget, power, role)
    fn hid_status(&self) -> zbus::fdo::Result<String> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.sh.send(Cmd::HidStatus(tx));
        rx.recv_timeout(std::time::Duration::from_secs(5)).map_err(|_| zbus::fdo::Error::Failed("daemon did not answer".into()))
    }
    /// debug builds / `--debug` only: act as if an external display was (dis)connected (tests without a monitor)
    fn debug_connector(&self, connected: bool) -> zbus::fdo::Result<String> {
        if !self.sh.debug {
            return Err(zbus::fdo::Error::AccessDenied("DebugConnector needs `dexhid daemon --debug`".into()));
        }
        self.sh.send(Cmd::DebugConnector(connected));
        Ok("ok".into())
    }
    /// `--debug` only (M2.1 test aid): log one line `DEBUG-NOTE <text>`; KWin test scripts report through it
    /// (KWin 6.8 sends a script's print() to a debug category that is off by default, so it never reaches the journal)
    fn debug_note(&self, text: &str) -> zbus::fdo::Result<String> {
        if !self.sh.debug {
            return Err(zbus::fdo::Error::AccessDenied("DebugNote needs `dexhid daemon --debug`".into()));
        }
        let t: String = text.chars().take(400).map(|c| if c.is_control() { ' ' } else { c }).collect();
        println!("DEBUG-NOTE t={} {}", crate::sys::mono_ms(), t);
        Ok("ok".into())
    }
}

/// serve the API; returns the connection (keep it alive) or an error (e.g. the name is taken: another instance)
/// Err starting with "TAKEN" = another instance owns the name (no queueing, no replacement either way)
pub fn serve(sh: SharedRef) -> Result<zbus::blocking::Connection, String> {
    use zbus::fdo::{RequestNameFlags, RequestNameReply};
    let api = Api { sh };
    let c = zbus::blocking::connection::Builder::session()
        .and_then(|b| b.serve_at(PATH, api))
        .and_then(|b| b.build())
        .map_err(|e| format!("session bus: {}", e))?;
    match c.request_name_with_flags(NAME, RequestNameFlags::DoNotQueue.into()) {
        Ok(RequestNameReply::PrimaryOwner) | Ok(RequestNameReply::AlreadyOwner) => Ok(c),
        Ok(r) => Err(format!("TAKEN {} ({:?})", NAME, r)),
        Err(zbus::Error::NameTaken) => Err(format!("TAKEN {}", NAME)),
        Err(e) => Err(format!("request name {}: {}", NAME, e)),
    }
}

pub fn emit_state(conn: &zbus::blocking::Connection, state: &str) {
    let _ = conn.emit_signal(None::<&str>, PATH, NAME, "StateChanged", &(state,));
}

/// CLI: call a method of the running daemon
pub fn call(method: &str, arg: Option<&str>) -> Result<String, String> {
    let c = zbus::blocking::Connection::session().map_err(|e| format!("session bus: {}", e))?;
    let r = match (method, arg) {
        ("DebugConnector", Some(a)) => c.call_method(Some(NAME), PATH, Some(NAME), method, &(a == "connect",)),
        ("SetSetting", Some(a)) => {
            let (k, v) = a.split_once('\u{1f}').unwrap_or((a, ""));
            c.call_method(Some(NAME), PATH, Some(NAME), method, &(k, v))
        }
        (_, Some(a)) => c.call_method(Some(NAME), PATH, Some(NAME), method, &(a,)),
        (_, None) => c.call_method(Some(NAME), PATH, Some(NAME), method, &()),
    }
    .map_err(|e| {
        let s = e.to_string();
        let hint = if s.contains("ServiceUnknown") || s.contains("NameHasNoOwner") || s.contains("was not provided") { " (is `dexhid daemon` running?)" } else { "" };
        format!("{}: {}{}", method, s, hint)
    })?;
    r.body().deserialize::<String>().map_err(|e| e.to_string())
}
