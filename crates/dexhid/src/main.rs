// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL: the owner has not decided the licence yet; change
// every header before the repository goes public)
//! dexhid: the tablet screen as touchpad + keyboard while an external display is connected.
//! Written from scratch; no Denial code (behaviour numbers cited from Denial's INPUT_DECK.md).
//!
//!   dexhid daemon [--config F] [--touch-dev P] [--debug [--chord-test-dev P] [--simulate-connect]]
//!   dexhid toggle | release | resume | status | layout <name>       (D-Bus client of org.dexhid.DexHid1)
//!   dexhid debug-connect | debug-disconnect                         (needs a daemon started with --debug)
//!   dexhid debug list | grabtest | replay | volsim                  (test tools, see debug.rs)
//!   dexhid set KEY [VALUE] | settings | quick ID                    (M2: settings registry, quick actions)
//!   dexhid guardian --fd N [--hb-ms MS]                             (the key guardian; started by the daemon)
//!   dexhid --restore                                                (ExecStopPost: restores a dimmed panel)
//!   dexhid version

mod daemon;
mod dbus;
mod debug;
mod desktop;
mod devices;
mod guardian;
mod logind;
mod notify;
mod render;
mod shared;
mod sys;
mod ui;
mod xkb;

/// M2.1: where the panel restore record lives ($XDG_STATE_HOME/dexhid/panels.json)
pub fn panels_state() -> PathBuf {
    let base = match std::env::var("XDG_STATE_HOME") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".local/state"),
    };
    base.join("dexhid/panels.json")
}

use std::path::PathBuf;

fn usage() -> i32 {
    eprintln!(
        "usage: dexhid daemon [--config FILE] [--touch-dev /dev/input/eventN] [--debug [--chord-test-dev P] [--simulate-connect]]\n       \
         dexhid toggle | release | resume | status | layout <name> | debug-connect | debug-disconnect\n       \
         dexhid set KEY [VALUE] | settings | quick ID\n       \
         dexhid debug list | grabtest --dev P | replay --fifo F ... | volsim --fifo F ...\n       \
         dexhid --restore | version"
    );
    2
}

fn client(method: &str, arg: Option<&str>) -> i32 {
    match dbus::call(method, arg) {
        Ok(s) => {
            println!("{}", s);
            0
        }
        Err(e) => {
            eprintln!("dexhid: {}", e);
            1
        }
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let a = debug::Args(argv.clone());
    let rc = match argv.first().map(|s| s.as_str()) {
        Some("daemon") => daemon::run(daemon::Opts {
            config: a.val("--config").map(PathBuf::from),
            touch_dev: a.val("--touch-dev"),
            debug: a.flag("--debug"),
            chord_test_dev: a.val("--chord-test-dev"),
            simulate_connect: a.flag("--simulate-connect"),
        }),
        Some("toggle") => client("Toggle", None),
        Some("release") => client("Release", None),
        Some("resume") => client("Resume", None),
        Some("status") => client("Status", None),
        Some("layout") => match argv.get(1) {
            Some(n) => client("SetLayout", Some(n)),
            None => usage(),
        },
        Some("set") => match (argv.get(1), argv.get(2)) {
            (Some(k), v) => client("SetSetting", Some(&format!("{}\u{1f}{}", k, v.map(|s| s.as_str()).unwrap_or("")))),
            _ => usage(),
        },
        Some("settings") => client("GetSettings", None),
        Some("quick") => match argv.get(1) {
            Some(id) => client("RunQuick", Some(id)),
            None => usage(),
        },
        Some("guardian") => guardian::run(a.val("--fd").and_then(|v| v.parse().ok()).unwrap_or(3), a.val("--hb-ms").and_then(|v| v.parse().ok()).unwrap_or(3000)),
        Some("debug-connect") => client("DebugConnector", Some("connect")),
        Some("debug-disconnect") => client("DebugConnector", Some("disconnect")),
        Some("debug") => debug::run(&debug::Args(argv[1..].to_vec())),
        Some("--restore") => {
            // ExecStopPost: put back a brightness the daemon dimmed (the state is in $XDG_RUNTIME_DIR/dexhid) and
            // (M2.1) the Plasma panels it moved to the monitor ($XDG_STATE_HOME/dexhid/panels.json)
            println!("{}", desktop::restore());
            if let Some(r) = desktop::panels_restore(&panels_state(), "restore") {
                println!("{}", r);
            }
            0
        }
        Some("version") | Some("--version") => {
            println!("dexhid {} (M2.1, GPL-3.0-or-later PROVISIONAL)", env!("CARGO_PKG_VERSION"));
            0
        }
        _ => usage(),
    };
    std::process::exit(rc);
}
