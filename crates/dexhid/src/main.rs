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
//!   dexhid profile list|save NAME [monitor] [app=CLASS]|use NAME|auto|delete NAME   (M3 profiles)
//!   dexhid act CMD.. | hid-status                                   (M3 feature actions, USB HID state)
//!   dexhid hid-helper [--socket P] [--allow-uid N]                  (M3 root helper of the USB HID mode)
//!   dexhid --restore                                                (ExecStopPost: restores a dimmed panel)
//!   dexhid version

mod clipboard;
mod daemon;
mod dbus;
mod hidhelper;
mod hidsink;
mod ipc;
mod mirror;
mod debug;
mod desktop;
mod devices;
mod guardian;
mod logind;
mod notify;
mod render;
mod shared;
mod sys;
mod appwin;
mod look;
mod ui;
mod xkb;

/// M4 measurement: a counting allocator, only with `--features alloc-count` (the bench reads `allocs()`)
#[cfg(feature = "alloc-count")]
mod alloc_count {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicU64, Ordering};
    pub static N: AtomicU64 = AtomicU64::new(0);
    pub static BYTES: AtomicU64 = AtomicU64::new(0);
    pub struct Counting;
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            N.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(l.size() as u64, Ordering::Relaxed);
            unsafe { System.alloc(l) }
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            unsafe { System.dealloc(p, l) }
        }
        unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
            N.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(n as u64, Ordering::Relaxed);
            unsafe { System.realloc(p, l, n) }
        }
    }
    #[global_allocator]
    static GA: Counting = Counting;
}

/// heap allocations so far (count, bytes); (0, 0) without the `alloc-count` feature
pub fn allocs() -> (u64, u64) {
    #[cfg(feature = "alloc-count")]
    {
        use std::sync::atomic::Ordering;
        (alloc_count::N.load(Ordering::Relaxed), alloc_count::BYTES.load(Ordering::Relaxed))
    }
    #[cfg(not(feature = "alloc-count"))]
    {
        (0, 0)
    }
}

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
         dexhid profile list|save NAME [monitor] [app=CLASS]|use NAME|auto|delete NAME | act CMD.. | hid-status\n       \
         dexhid hid-helper [--socket P] [--allow-uid N] [--configfs DIR]\n       \
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
        // M3
        Some("profile") if argv.len() >= 2 => client("Profile", Some(&argv[1..].join(" "))),
        Some("act") if argv.len() >= 2 => client("Act", Some(&argv[1..].join(" "))),
        Some("hid-status") => client("HidStatus", None),
        Some("hid-helper") => hidhelper::run(&debug::Args(argv[1..].to_vec())),
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
            println!("dexhid {} (M3, GPL-3.0-or-later PROVISIONAL)", env!("CARGO_PKG_VERSION"));
            0
        }
        _ => usage(),
    };
    std::process::exit(rc);
}
