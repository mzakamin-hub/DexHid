// SPDX-License-Identifier: GPL-3.0-or-later
//! The DexHid state machine (plan §1 / §2). `desired` is what the user / monitor asked for; pause flags (lock,
//! inactive session, sleep, cover closed) override it without forgetting it. `effective()` is what the daemon applies:
//! grab or not, which UI.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Desired {
    /// no external display
    Idle,
    /// "ask": the prompt is shown, nothing grabbed
    Offer,
    /// touchpad + keyboard: the touchscreen is grabbed
    Pad,
    /// "Use as display": nothing grabbed, a small "Use as pad" pill is shown (if enabled)
    Display,
    /// "Exit" / "Never": nothing grabbed, nothing shown until the next plug, toggle or resume
    Hidden,
}

impl Desired {
    pub fn name(&self) -> &'static str {
        match self {
            Desired::Idle => "idle",
            Desired::Offer => "offer",
            Desired::Pad => "pad",
            Desired::Display => "display",
            Desired::Hidden => "hidden",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Autostart {
    On,
    Off,
    Ask,
}

impl Autostart {
    pub fn parse(s: &str) -> Option<Autostart> {
        match s {
            "on" => Some(Autostart::On),
            "off" => Some(Autostart::Off),
            "ask" => Some(Autostart::Ask),
            _ => None,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Autostart::On => "on",
            Autostart::Off => "off",
            Autostart::Ask => "ask",
        }
    }
    pub fn next(&self) -> Autostart {
        match self {
            Autostart::Ask => Autostart::On,
            Autostart::On => Autostart::Off,
            Autostart::Off => Autostart::Ask,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    UsePad,
    KeepDisplay,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Remember {
    Pad,
    Display,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Ev {
    /// an external display is connected (with the remembered choice for this monitor, if any)
    Connected(Option<Remember>),
    Disconnected,
    Prompt(Choice),
    Toggle,
    Release,
    Resume,
    UseAsDisplay,
    Exit,
    FiveFinger,
    Chord,
    Locked(bool),
    SessionActive(bool),
    Sleep(bool),
    LidClosed(bool),
    /// the UI cannot show the overlay (Wayland lost / no surface): never a blind pad
    UiLost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiKind {
    None,
    Prompt,
    Overlay,
    Pill,
}

impl UiKind {
    pub fn name(&self) -> &'static str {
        match self {
            UiKind::None => "none",
            UiKind::Prompt => "prompt",
            UiKind::Overlay => "overlay",
            UiKind::Pill => "pill",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Effective {
    pub grab: bool,
    pub ui: UiKind,
}

#[derive(Clone, Debug)]
pub struct Machine {
    pub desired: Desired,
    pub connected: bool,
    pub locked: bool,
    pub inactive: bool,
    pub sleeping: bool,
    pub lid_closed: bool,
    pub autostart: Autostart,
    pub pill: bool,
}

impl Machine {
    pub fn new(autostart: Autostart, pill: bool) -> Machine {
        Machine { desired: Desired::Idle, connected: false, locked: false, inactive: false, sleeping: false, lid_closed: false, autostart, pill }
    }

    pub fn paused(&self) -> bool {
        self.locked || self.inactive || self.sleeping || self.lid_closed
    }

    pub fn pause_reasons(&self) -> Vec<&'static str> {
        let mut v = vec![];
        if self.locked {
            v.push("locked");
        }
        if self.inactive {
            v.push("inactive");
        }
        if self.sleeping {
            v.push("sleep");
        }
        if self.lid_closed {
            v.push("lid");
        }
        v
    }

    pub fn effective(&self) -> Effective {
        if self.paused() {
            return Effective { grab: false, ui: UiKind::None };
        }
        match self.desired {
            Desired::Idle | Desired::Hidden => Effective { grab: false, ui: UiKind::None },
            Desired::Offer => Effective { grab: false, ui: UiKind::Prompt },
            Desired::Pad => Effective { grab: true, ui: UiKind::Overlay },
            Desired::Display => Effective { grab: false, ui: if self.pill { UiKind::Pill } else { UiKind::None } },
        }
    }

    /// apply an event; returns a choice to remember for the current monitor ("Always" / "Never")
    pub fn handle(&mut self, ev: &Ev) -> Option<Remember> {
        use Desired::*;
        match ev {
            Ev::Connected(rem) => {
                let was = self.connected;
                self.connected = true;
                if !was || self.desired == Idle {
                    self.desired = match rem {
                        Some(Remember::Pad) => Pad,
                        Some(Remember::Display) => Display,
                        None => match self.autostart {
                            Autostart::On => Pad,
                            Autostart::Off => Display,
                            Autostart::Ask => Offer,
                        },
                    };
                }
            }
            Ev::Disconnected => {
                self.connected = false;
                self.desired = Idle;
            }
            Ev::Prompt(c) if self.desired == Offer => {
                let (d, r) = match c {
                    Choice::UsePad => (Pad, None),
                    Choice::KeepDisplay => (Display, None),
                    Choice::Always => (Pad, Some(Remember::Pad)),
                    Choice::Never => (Display, Some(Remember::Display)),
                };
                self.desired = d;
                return r;
            }
            Ev::Prompt(_) => {}
            Ev::Toggle if self.connected => {
                self.desired = if self.desired == Pad { Display } else { Pad };
            }
            Ev::Toggle => {}
            Ev::Release if matches!(self.desired, Pad | Offer) => self.desired = Display,
            Ev::Release => {}
            Ev::Resume if self.connected && matches!(self.desired, Offer | Display | Hidden) => self.desired = Pad,
            Ev::Resume => {}
            Ev::UseAsDisplay | Ev::FiveFinger | Ev::Chord | Ev::UiLost if self.desired == Pad => self.desired = Display,
            Ev::UseAsDisplay | Ev::FiveFinger | Ev::Chord | Ev::UiLost => {}
            Ev::Exit if matches!(self.desired, Pad | Offer | Display) => self.desired = Hidden,
            Ev::Exit => {}
            Ev::Locked(b) => self.locked = *b,
            Ev::SessionActive(b) => self.inactive = !*b,
            Ev::Sleep(b) => self.sleeping = *b,
            Ev::LidClosed(b) => self.lid_closed = *b,
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Desired::*;

    fn eff(grab: bool, ui: UiKind) -> Effective {
        Effective { grab, ui }
    }

    /// (start desired, connected, event, expected desired, expected effective)
    #[test]
    fn transition_table() {
        let rows: Vec<(Desired, bool, Ev, Desired, Effective)> = vec![
            (Idle, false, Ev::Connected(None), Offer, eff(false, UiKind::Prompt)),
            (Idle, false, Ev::Connected(Some(Remember::Pad)), Pad, eff(true, UiKind::Overlay)),
            (Idle, false, Ev::Connected(Some(Remember::Display)), Display, eff(false, UiKind::Pill)),
            (Offer, true, Ev::Prompt(Choice::UsePad), Pad, eff(true, UiKind::Overlay)),
            (Offer, true, Ev::Prompt(Choice::KeepDisplay), Display, eff(false, UiKind::Pill)),
            (Offer, true, Ev::Prompt(Choice::Always), Pad, eff(true, UiKind::Overlay)),
            (Offer, true, Ev::Prompt(Choice::Never), Display, eff(false, UiKind::Pill)),
            (Pad, true, Ev::Prompt(Choice::Never), Pad, eff(true, UiKind::Overlay)),
            (Pad, true, Ev::Disconnected, Idle, eff(false, UiKind::None)),
            (Offer, true, Ev::Disconnected, Idle, eff(false, UiKind::None)),
            (Pad, true, Ev::Toggle, Display, eff(false, UiKind::Pill)),
            (Display, true, Ev::Toggle, Pad, eff(true, UiKind::Overlay)),
            (Hidden, true, Ev::Toggle, Pad, eff(true, UiKind::Overlay)),
            (Idle, false, Ev::Toggle, Idle, eff(false, UiKind::None)),
            (Pad, true, Ev::Release, Display, eff(false, UiKind::Pill)),
            (Display, true, Ev::Resume, Pad, eff(true, UiKind::Overlay)),
            (Idle, false, Ev::Resume, Idle, eff(false, UiKind::None)),
            (Pad, true, Ev::UseAsDisplay, Display, eff(false, UiKind::Pill)),
            (Pad, true, Ev::FiveFinger, Display, eff(false, UiKind::Pill)),
            (Pad, true, Ev::Chord, Display, eff(false, UiKind::Pill)),
            (Pad, true, Ev::UiLost, Display, eff(false, UiKind::Pill)),
            (Pad, true, Ev::Exit, Hidden, eff(false, UiKind::None)),
            (Display, true, Ev::Chord, Display, eff(false, UiKind::Pill)),
            (Pad, true, Ev::Connected(None), Pad, eff(true, UiKind::Overlay)),
        ];
        for (i, (d0, c0, ev, d1, e1)) in rows.into_iter().enumerate() {
            let mut m = Machine::new(Autostart::Ask, true);
            m.desired = d0;
            m.connected = c0;
            m.handle(&ev);
            assert_eq!(m.desired, d1, "row {} {:?}", i, ev);
            assert_eq!(m.effective(), e1, "row {} {:?}", i, ev);
        }
    }

    #[test]
    fn autostart_modes() {
        for (a, d) in [(Autostart::On, Pad), (Autostart::Off, Display), (Autostart::Ask, Offer)] {
            let mut m = Machine::new(a, false);
            m.handle(&Ev::Connected(None));
            assert_eq!(m.desired, d);
        }
        let mut m = Machine::new(Autostart::Off, false);
        m.handle(&Ev::Connected(None));
        assert_eq!(m.effective(), eff(false, UiKind::None), "no pill when disabled");
    }

    #[test]
    fn remember_choices() {
        let mut m = Machine::new(Autostart::Ask, true);
        m.handle(&Ev::Connected(None));
        assert_eq!(m.handle(&Ev::Prompt(Choice::Always)), Some(Remember::Pad));
        m.handle(&Ev::Disconnected);
        m.handle(&Ev::Connected(None));
        assert_eq!(m.handle(&Ev::Prompt(Choice::Never)), Some(Remember::Display));
    }

    #[test]
    fn pauses_keep_the_wish() {
        for p in [Ev::Locked(true), Ev::SessionActive(false), Ev::Sleep(true), Ev::LidClosed(true)] {
            let mut m = Machine::new(Autostart::On, true);
            m.handle(&Ev::Connected(None));
            assert!(m.effective().grab);
            m.handle(&p);
            assert_eq!(m.effective(), eff(false, UiKind::None), "{:?}", p);
            assert_eq!(m.desired, Pad);
            let undo = match p {
                Ev::Locked(_) => Ev::Locked(false),
                Ev::SessionActive(_) => Ev::SessionActive(true),
                Ev::Sleep(_) => Ev::Sleep(false),
                _ => Ev::LidClosed(false),
            };
            m.handle(&undo);
            assert!(m.effective().grab, "resumes after {:?}", p);
        }
        // unplug while locked: idle after unlock
        let mut m = Machine::new(Autostart::On, true);
        m.handle(&Ev::Connected(None));
        m.handle(&Ev::Locked(true));
        m.handle(&Ev::Disconnected);
        m.handle(&Ev::Locked(false));
        assert_eq!(m.effective(), eff(false, UiKind::None));
    }
}
