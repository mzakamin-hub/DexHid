// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! The virtual devices: "dexhid touchpad" (MT protocol B, INPUT_PROP_POINTER, no pressure, the M0 "B" variant that
//! libinput + KWin classified as a touchpad) and "dexhid keyboard" (EV_KEY only, no EV_REP: the desktop repeats).

use crate::sys::*;
use dexhid_core::keycodes;
use dexhid_core::touchpad::{PadFrame, SLOTS};

pub const PAD_NAME: &str = "dexhid touchpad";
pub const KBD_NAME: &str = "dexhid keyboard";

#[allow(dead_code)]
pub struct VirtualPad {
    pub u: Uinput,
    pub spec: (i32, i32, i32),
    cur: [Option<(i32, i32)>; SLOTS],
    ids: [i32; SLOTS],
    next_id: i32,
    tool: Option<u16>,
    touch: bool,
}

impl VirtualPad {
    /// spec = (xmax, ymax, resolution units/mm)
    pub fn create(spec: (i32, i32, i32)) -> Result<VirtualPad, String> {
        let (xmax, ymax, res) = spec;
        let keys = vec![
            BTN_LEFT,
            BTN_RIGHT,
            BTN_MIDDLE,
            BTN_TOUCH,
            BTN_TOOL_FINGER,
            BTN_TOOL_DOUBLETAP,
            BTN_TOOL_TRIPLETAP,
            BTN_TOOL_QUADTAP,
            BTN_TOOL_QUINTTAP,
        ];
        let abs = vec![
            (ABS_X, ai(0, xmax, res)),
            (ABS_Y, ai(0, ymax, res)),
            (ABS_MT_SLOT, ai(0, SLOTS as i32 - 1, 0)),
            (ABS_MT_TRACKING_ID, ai(0, 65535, 0)),
            (ABS_MT_POSITION_X, ai(0, xmax, res)),
            (ABS_MT_POSITION_Y, ai(0, ymax, res)),
        ];
        let u = Uinput::create(&UinputSpec { keys, abs, props: vec![INPUT_PROP_POINTER], ..UinputSpec::new(PAD_NAME) })?;
        Ok(VirtualPad { u, spec, cur: [None; SLOTS], ids: [-1; SLOTS], next_id: 1, tool: None, touch: false })
    }

    pub fn apply(&mut self, f: &PadFrame) {
        let mut any = false;
        for s in 0..SLOTS {
            match (self.cur[s], f.slots[s]) {
                (None, None) => {}
                (Some(_), None) => {
                    self.u.emit(EV_ABS, ABS_MT_SLOT, s as i32);
                    self.u.emit(EV_ABS, ABS_MT_TRACKING_ID, -1);
                    self.ids[s] = -1;
                    any = true;
                }
                (None, Some((x, y))) => {
                    self.ids[s] = self.next_id;
                    self.next_id = (self.next_id % 65535) + 1;
                    self.u.emit(EV_ABS, ABS_MT_SLOT, s as i32);
                    self.u.emit(EV_ABS, ABS_MT_TRACKING_ID, self.ids[s]);
                    self.u.emit(EV_ABS, ABS_MT_POSITION_X, x);
                    self.u.emit(EV_ABS, ABS_MT_POSITION_Y, y);
                    any = true;
                }
                (Some((ox, oy)), Some((x, y))) => {
                    if (ox, oy) != (x, y) {
                        self.u.emit(EV_ABS, ABS_MT_SLOT, s as i32);
                        if ox != x {
                            self.u.emit(EV_ABS, ABS_MT_POSITION_X, x);
                        }
                        if oy != y {
                            self.u.emit(EV_ABS, ABS_MT_POSITION_Y, y);
                        }
                        any = true;
                    }
                }
            }
            self.cur[s] = f.slots[s];
        }
        let n = self.cur.iter().filter(|c| c.is_some()).count();
        let tool = match n {
            0 => None,
            1 => Some(BTN_TOOL_FINGER),
            2 => Some(BTN_TOOL_DOUBLETAP),
            3 => Some(BTN_TOOL_TRIPLETAP),
            4 => Some(BTN_TOOL_QUADTAP),
            _ => Some(BTN_TOOL_QUINTTAP),
        };
        if tool != self.tool {
            if let Some(t) = self.tool {
                self.u.emit(EV_KEY, t, 0);
            }
            if let Some(t) = tool {
                self.u.emit(EV_KEY, t, 1);
            }
            self.tool = tool;
            any = true;
        }
        if (n > 0) != self.touch {
            self.touch = n > 0;
            self.u.emit(EV_KEY, BTN_TOUCH, self.touch as i32);
            any = true;
        }
        if let Some((x, y)) = self.cur.iter().flatten().next() {
            self.u.emit(EV_ABS, ABS_X, *x);
            self.u.emit(EV_ABS, ABS_Y, *y);
        }
        for &(b, p) in &f.buttons {
            if any {
                // contacts first in their own frame, then each button change in its own frame
                self.u.syn();
                any = false;
            }
            self.u.emit(EV_KEY, b, p as i32);
            self.u.syn();
        }
        if any {
            self.u.syn();
        }
    }
}

/// the virtual keyboard; since M2 it lives in the guardian process (guardian.rs), which owns the uinput fd
pub struct VirtualKbd {
    pub u: Uinput,
    pub held: std::collections::BTreeSet<u16>,
}

impl VirtualKbd {
    pub fn create() -> Result<VirtualKbd, String> {
        let u = Uinput::create(&UinputSpec { keys: keycodes::all_codes(), leds: vec![LED_NUML, LED_CAPSL, LED_SCROLLL], ..UinputSpec::new(KBD_NAME) })?;
        Ok(VirtualKbd { u, held: Default::default() })
    }
    pub fn key(&mut self, code: u16, pressed: bool) {
        if pressed {
            self.held.insert(code);
        } else {
            self.held.remove(&code);
        }
        self.u.emit(EV_KEY, code, pressed as i32);
        self.u.syn();
    }
    /// release anything still down (belt and braces: the router already released its keys)
    pub fn release_all(&mut self) -> usize {
        let v: Vec<u16> = self.held.iter().copied().collect();
        for c in &v {
            self.key(*c, false);
        }
        v.len()
    }
}

pub const MOUSE_NAME: &str = "dexhid mouse";

/// "dexhid mouse" (M2 relative-mouse fallback): REL_X/Y, wheels (hi-res), BTN_LEFT/RIGHT/MIDDLE
pub struct VirtualMouse {
    pub u: Uinput,
    buttons: std::collections::BTreeSet<u16>,
}

impl VirtualMouse {
    pub fn create() -> Result<VirtualMouse, String> {
        let u = Uinput::create(&UinputSpec {
            keys: vec![BTN_LEFT, BTN_RIGHT, BTN_MIDDLE],
            rels: vec![REL_X, REL_Y, REL_WHEEL, REL_HWHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES],
            props: vec![INPUT_PROP_POINTER],
            ..UinputSpec::new(MOUSE_NAME)
        })?;
        Ok(VirtualMouse { u, buttons: Default::default() })
    }
    pub fn apply(&mut self, outs: &[dexhid_core::mouse::MOut]) {
        use dexhid_core::mouse::MOut;
        for o in outs {
            match *o {
                MOut::Rel(x, y) => {
                    if x != 0 {
                        self.u.emit(EV_REL, REL_X, x);
                    }
                    if y != 0 {
                        self.u.emit(EV_REL, REL_Y, y);
                    }
                }
                MOut::Button(b, p) => {
                    if p {
                        self.buttons.insert(b);
                    } else {
                        self.buttons.remove(&b);
                    }
                    self.u.emit(EV_KEY, b, p as i32);
                }
                MOut::Wheel(v, h) => {
                    if v != 0 {
                        self.u.emit(EV_REL, REL_WHEEL_HI_RES, v);
                        if v % 120 == 0 {
                            self.u.emit(EV_REL, REL_WHEEL, v / 120);
                        }
                    }
                    if h != 0 {
                        self.u.emit(EV_REL, REL_HWHEEL_HI_RES, h);
                        if h % 120 == 0 {
                            self.u.emit(EV_REL, REL_HWHEEL, h / 120);
                        }
                    }
                }
            }
            self.u.syn();
        }
    }
    pub fn release_all(&mut self) {
        let b: Vec<u16> = self.buttons.iter().copied().collect();
        for c in b {
            self.u.emit(EV_KEY, c, 0);
            self.u.syn();
        }
        self.buttons.clear();
    }
}

/// destroy uinput devices off the critical path: UI_DEV_DESTROY waits for RCU grace periods and the device
/// consumers (tens of ms each on the tablet), so the release returns at once and this thread finishes the job
/// (the order stays: keys up + contacts lifted + ungrab happen before; destroy after)
pub fn destroy_later<T: Send + 'static>(what: &'static str, dev: T) {
    let t0 = crate::sys::mono_ns();
    let _ = std::thread::Builder::new().name("reaper".into()).spawn(move || {
        drop(dev);
        println!("DESTROYED t={} dev={} took_ms={:.1}", crate::sys::mono_ms(), what, (crate::sys::mono_ns() - t0) as f64 / 1e6);
    });
}

// ---------- replay touchscreen (test tool, `dexhid debug replay`) ----------
pub struct Ts {
    pub u: Uinput,
    pub xmax: i32,
    pub ymax: i32,
    pub slots: i32,
    next_id: i32,
}

/// clone the NVT caps when a node is given, else the documented NVT ranges
pub fn create_replay_ts(name: &str, clone: Option<&str>) -> Result<Ts, String> {
    let mut x = ai(0, 33919, 124);
    let mut y = ai(0, 23999, 124);
    let mut slot = ai(0, 9, 0);
    let mut press = ai(0, 255, 0);
    let mut major = ai(0, 255, 0);
    if let Some(path) = clone {
        let d = Dev::open(path, false).map_err(|e| format!("open {}: {}", path, strerr(e)))?;
        if let Some(a) = d.absinfo(ABS_MT_POSITION_X) {
            x = a;
        }
        if let Some(a) = d.absinfo(ABS_MT_POSITION_Y) {
            y = a;
        }
        if let Some(a) = d.absinfo(ABS_MT_SLOT) {
            slot = a;
        }
        if let Some(a) = d.absinfo(ABS_MT_PRESSURE) {
            press = a;
        }
        if let Some(a) = d.absinfo(ABS_MT_TOUCH_MAJOR) {
            major = a;
        }
        println!(
            "CLONE from={} name=\"{}\" x={}..{} res={} y={}..{} res={} slots={}",
            path,
            d.name(),
            x.minimum,
            x.maximum,
            x.resolution,
            y.minimum,
            y.maximum,
            y.resolution,
            slot.maximum + 1
        );
    }
    for a in [&mut x, &mut y, &mut slot, &mut press, &mut major] {
        a.value = 0;
    }
    let abs = vec![
        (ABS_X, x),
        (ABS_Y, y),
        (ABS_MT_SLOT, slot),
        (ABS_MT_TRACKING_ID, ai(0, 65535, 0)),
        (ABS_MT_POSITION_X, x),
        (ABS_MT_POSITION_Y, y),
        (ABS_MT_PRESSURE, press),
        (ABS_MT_TOUCH_MAJOR, major),
    ];
    let u = Uinput::create(&UinputSpec { keys: vec![BTN_TOUCH], abs, props: vec![INPUT_PROP_DIRECT], ..UinputSpec::new(name) })?;
    Ok(Ts { u, xmax: x.maximum, ymax: y.maximum, slots: slot.maximum + 1, next_id: 1 })
}

impl Ts {
    pub fn down(&mut self, slot: i32, x: i32, y: i32) {
        self.down_major(slot, x, y, 20);
    }
    /// a contact with a given touch major (palm replay)
    pub fn down_major(&mut self, slot: i32, x: i32, y: i32, major: i32) {
        let id = self.next_id;
        self.next_id = (self.next_id % 65535) + 1;
        self.u.emit(EV_ABS, ABS_MT_SLOT, slot);
        self.u.emit(EV_ABS, ABS_MT_TRACKING_ID, id);
        self.u.emit(EV_ABS, ABS_MT_POSITION_X, x);
        self.u.emit(EV_ABS, ABS_MT_POSITION_Y, y);
        self.u.emit(EV_ABS, ABS_MT_PRESSURE, 60);
        self.u.emit(EV_ABS, ABS_MT_TOUCH_MAJOR, major);
    }
    pub fn mv(&mut self, slot: i32, x: i32, y: i32) {
        self.u.emit(EV_ABS, ABS_MT_SLOT, slot);
        self.u.emit(EV_ABS, ABS_MT_POSITION_X, x);
        self.u.emit(EV_ABS, ABS_MT_POSITION_Y, y);
    }
    pub fn up(&mut self, slot: i32) {
        self.u.emit(EV_ABS, ABS_MT_SLOT, slot);
        self.u.emit(EV_ABS, ABS_MT_TRACKING_ID, -1);
    }
    pub fn frame(&mut self, n_active: usize, x: i32, y: i32) {
        if n_active > 0 {
            self.u.emit(EV_ABS, ABS_X, x);
            self.u.emit(EV_ABS, ABS_Y, y);
        }
        self.u.emit(EV_KEY, BTN_TOUCH, (n_active > 0) as i32);
        self.u.syn();
    }
}
