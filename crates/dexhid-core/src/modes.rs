// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! M4: the app window's one mode switch (owner decision 8 Nov 2026). Three modes, one persisted setting
//! (`mode_select`, the user's last choice), mapped onto the existing machinery, no second state machine:
//!
//! - `usb`: the tablet is the keyboard + mouse of another computer (`features.usbhid.enabled`, `target = usb`; the
//!   HID target brings up pad mode without a monitor);
//! - `pad`: keyboard + touchpad for the external display (`target = desktop` + Resume); needs an external output;
//! - `screen`: DexHid releases everything (`target = desktop` + Exit = Hidden: nothing grabbed, nothing shown, no
//!   prompt until the monitor is plugged again, a toggle or another mode).
//!
//! `plan` returns the steps in order; every plan starts with `ReleaseAll` (keys, buttons and pad contacts lifted
//! before the target or the grab changes), the daemon runs them in one command and publishes once.

use crate::state::{Desired, Ev};

pub const NAMES: &[&str] = &["usb", "pad", "screen"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Usb,
    Pad,
    Screen,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "usb" => Some(Mode::Usb),
            "pad" | "touchpad" | "monitor" => Some(Mode::Pad),
            "screen" | "display" | "second-screen" => Some(Mode::Screen),
            _ => None,
        }
    }
    pub fn name(&self) -> &'static str {
        NAMES[self.index()]
    }
    pub fn index(&self) -> usize {
        match self {
            Mode::Usb => 0,
            Mode::Pad => 1,
            Mode::Screen => 2,
        }
    }
    pub fn from_index(i: usize) -> Option<Mode> {
        [Mode::Usb, Mode::Pad, Mode::Screen].get(i).copied()
    }
}

/// what the daemon knows when a mode is chosen
#[derive(Clone, Copy, Debug, Default)]
pub struct Ctx {
    /// an external output (real or simulated) is connected
    pub monitor: bool,
    pub usb_enabled: bool,
    /// the HID target is USB now
    pub target_usb: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// lift every key, button and pad contact on every sink (before anything changes)
    ReleaseAll,
    /// store a setting (key, value) through the settings registry
    Set(&'static str, &'static str),
    /// feed the state machine
    Machine(Ev),
}

pub const NO_MONITOR: &str = "no external display connected: connect a monitor over USB-C, then choose this mode";

/// the steps of a switch to `m`; Err = not possible now (with the hint the window / CLI shows)
pub fn plan(m: Mode, c: &Ctx) -> Result<Vec<Step>, String> {
    let mut v = vec![Step::ReleaseAll];
    match m {
        Mode::Usb => {
            if !c.usb_enabled {
                v.push(Step::Set("features.usbhid.enabled", "true"));
            }
            v.push(Step::Set("mode_select", "usb"));
            v.push(Step::Set("target", "usb"));
        }
        Mode::Pad => {
            if !c.monitor {
                return Err(NO_MONITOR.into());
            }
            v.push(Step::Set("mode_select", "pad"));
            if c.target_usb {
                v.push(Step::Set("target", "desktop"));
            }
            v.push(Step::Machine(Ev::Resume));
        }
        Mode::Screen => {
            v.push(Step::Set("mode_select", "screen"));
            if c.target_usb {
                v.push(Step::Set("target", "desktop"));
            }
            v.push(Step::Machine(Ev::Exit));
        }
    }
    Ok(v)
}

/// the mode that is in effect (the window's selected segment, `mode_effective` in the status)
pub fn effective(target_usb: bool, desired: Desired) -> Mode {
    if target_usb {
        Mode::Usb
    } else if desired == Desired::Pad {
        Mode::Pad
    } else {
        Mode::Screen
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Autostart, Machine};

    #[test]
    fn names_round_trip() {
        for (i, n) in NAMES.iter().enumerate() {
            let m = Mode::parse(n).unwrap();
            assert_eq!((m.name(), m.index(), Mode::from_index(i)), (*n, i, Some(m)));
        }
        assert_eq!(Mode::parse("Second-Screen"), Some(Mode::Screen));
        assert!(Mode::parse("bluetooth").is_none());
    }

    /// every plan releases first; pad needs a monitor; the target only changes when it must
    #[test]
    fn plan_table() {
        for monitor in [false, true] {
            for usb_enabled in [false, true] {
                for target_usb in [false, true] {
                    let c = Ctx { monitor, usb_enabled, target_usb };
                    for m in [Mode::Usb, Mode::Pad, Mode::Screen] {
                        match plan(m, &c) {
                            Ok(s) => {
                                assert_eq!(s[0], Step::ReleaseAll, "{:?} {:?}: release first", m, c);
                                assert_eq!(s.iter().filter(|x| **x == Step::ReleaseAll).count(), 1);
                                assert!(s.contains(&Step::Set("mode_select", m.name())), "{:?} stores the choice", m);
                                let to_desktop = s.contains(&Step::Set("target", "desktop"));
                                assert_eq!(to_desktop, m != Mode::Usb && target_usb, "{:?} {:?}", m, c);
                                assert_eq!(s.contains(&Step::Set("features.usbhid.enabled", "true")), m == Mode::Usb && !usb_enabled);
                            }
                            Err(e) => {
                                assert!(m == Mode::Pad && !monitor, "{:?} {:?} refused: {}", m, c, e);
                                assert_eq!(e, NO_MONITOR);
                            }
                        }
                    }
                }
            }
        }
    }

    /// the machine steps give the owner's semantics: pad grabs, screen hides with no prompt until a re-plug
    #[test]
    fn machine_effects() {
        let run = |m: Mode, mach: &mut Machine| {
            for s in plan(m, &Ctx { monitor: mach.connected, ..Default::default() }).unwrap() {
                if let Step::Machine(e) = s {
                    mach.handle(&e);
                }
            }
        };
        let mut mach = Machine::new(Autostart::Ask, true);
        mach.handle(&Ev::Connected(None));
        assert_eq!(mach.desired, Desired::Offer, "ask prompts on plug");
        run(Mode::Screen, &mut mach);
        assert_eq!(mach.desired, Desired::Hidden);
        assert!(!mach.effective().grab && mach.effective().ui == crate::state::UiKind::None, "second screen: nothing grabbed or shown");
        assert_eq!(effective(false, mach.desired), Mode::Screen);
        run(Mode::Pad, &mut mach);
        assert_eq!(mach.desired, Desired::Pad);
        assert!(mach.effective().grab);
        assert_eq!(effective(false, mach.desired), Mode::Pad);
        run(Mode::Screen, &mut mach);
        // re-plug: the prompt comes back
        mach.handle(&Ev::Disconnected);
        mach.handle(&Ev::Connected(None));
        assert_eq!(mach.desired, Desired::Offer, "the prompt returns after a re-plug");
        assert_eq!(effective(true, Desired::Idle), Mode::Usb);
    }
}
