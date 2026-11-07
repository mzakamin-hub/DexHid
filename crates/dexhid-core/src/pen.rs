// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! M3 pen on the external monitor. While pad mode is active and the module is on, the daemon grabs the pen
//! (NVTCapacitivePen) and re-emits it:
//! - "pointer": a precise relative pointer ("dexhid pen pointer"): hover moves the pointer (no jump when the pen
//!   comes back into range), the tip is the left button, the barrel buttons right / middle;
//! - "tablet": an absolute drawing tablet ("dexhid pen", pressure + tilt copied, no INPUT_PROP_DIRECT = an external
//!   tablet for libinput) that the desktop maps to the monitor (KWin InputDevice outputName = the external output).
//! The panel orientation is applied, so "up" on the tablet is up on the monitor. On release the grab ends and the
//! virtual device goes away (the kernel lifts the tip / buttons).

pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_PRESSURE: u16 = 0x18;
pub const ABS_TILT_X: u16 = 0x1a;
pub const ABS_TILT_Y: u16 = 0x1b;
pub const BTN_TOOL_PEN: u16 = 0x140;
pub const BTN_TOOL_RUBBER: u16 = 0x141;
pub const BTN_TOUCH: u16 = 0x14a;
pub const BTN_STYLUS: u16 = 0x14b;
pub const BTN_STYLUS2: u16 = 0x14c;
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Pointer,
    Tablet,
}

/// clockwise rotation of the panel content relative to the pen's native axes (0 / 90 / 180 / 270)
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PenCfg {
    pub mode: Mode,
    pub rot: u16,
    /// native maxima of X / Y
    pub max: (i32, i32),
    /// pointer mode: desktop px per native unit
    pub gain: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PenState {
    pub x: i32,
    pub y: i32,
    pub pressure: i32,
    pub tilt: (i32, i32),
    pub near: bool,
    pub touch: bool,
    pub stylus: bool,
    pub stylus2: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PenOut {
    Rel(i32, i32),
    Button(u16, bool),
    /// one tablet frame (rotated coordinates)
    Abs(PenState),
}

/// native -> rotated coordinates and the rotated maxima
pub fn rotate(x: i32, y: i32, max: (i32, i32), rot: u16) -> (i32, i32, (i32, i32)) {
    match rot % 360 {
        90 => (max.1 - y, x, (max.1, max.0)),
        180 => (max.0 - x, max.1 - y, max),
        270 => (y, max.0 - x, (max.1, max.0)),
        _ => (x, y, max),
    }
}

#[derive(Clone, Debug)]
pub struct Pen {
    pub cfg: PenCfg,
    cur: PenState,
    prev: PenState,
    last_pos: Option<(i32, i32)>,
    frac: (f32, f32),
}

impl Pen {
    pub fn new(cfg: PenCfg) -> Pen {
        Pen { cfg, cur: PenState::default(), prev: PenState::default(), last_pos: None, frac: (0.0, 0.0) }
    }

    pub fn state(&self) -> PenState {
        self.cur
    }

    /// one evdev event of the real pen; outputs at each SYN_REPORT
    pub fn feed(&mut self, type_: u16, code: u16, value: i32) -> Vec<PenOut> {
        match (type_, code) {
            (EV_ABS, ABS_X) => self.cur.x = value,
            (EV_ABS, ABS_Y) => self.cur.y = value,
            (EV_ABS, ABS_PRESSURE) => self.cur.pressure = value,
            (EV_ABS, ABS_TILT_X) => self.cur.tilt.0 = value,
            (EV_ABS, ABS_TILT_Y) => self.cur.tilt.1 = value,
            (EV_KEY, BTN_TOOL_PEN) | (EV_KEY, BTN_TOOL_RUBBER) => self.cur.near = value != 0,
            (EV_KEY, BTN_TOUCH) => self.cur.touch = value != 0,
            (EV_KEY, BTN_STYLUS) => self.cur.stylus = value != 0,
            (EV_KEY, BTN_STYLUS2) => self.cur.stylus2 = value != 0,
            (EV_SYN, 0) => return self.frame(),
            _ => {}
        }
        vec![]
    }

    fn frame(&mut self) -> Vec<PenOut> {
        let mut out = vec![];
        let c = self.cur;
        let (rx, ry, _) = rotate(c.x, c.y, self.cfg.max, self.cfg.rot);
        match self.cfg.mode {
            Mode::Tablet => {
                if c != self.prev {
                    let mut s = c;
                    s.x = rx;
                    s.y = ry;
                    out.push(PenOut::Abs(s));
                }
            }
            Mode::Pointer => {
                if c.near {
                    if let Some((lx, ly)) = self.last_pos {
                        let fx = (rx - lx) as f32 * self.cfg.gain + self.frac.0;
                        let fy = (ry - ly) as f32 * self.cfg.gain + self.frac.1;
                        let (ix, iy) = (fx.trunc() as i32, fy.trunc() as i32);
                        self.frac = (fx - ix as f32, fy - iy as f32);
                        if ix != 0 || iy != 0 {
                            out.push(PenOut::Rel(ix, iy));
                        }
                    }
                    self.last_pos = Some((rx, ry));
                } else {
                    self.last_pos = None;
                    self.frac = (0.0, 0.0);
                }
                let tip = c.touch && c.near;
                for (now, was, b) in [(tip, self.prev.touch && self.prev.near, 0x110u16), (c.stylus && c.near, self.prev.stylus && self.prev.near, 0x111), (c.stylus2 && c.near, self.prev.stylus2 && self.prev.near, 0x112)] {
                    if now != was {
                        out.push(PenOut::Button(b, now));
                    }
                }
            }
        }
        self.prev = c;
        out
    }

    /// everything up (before the grab ends)
    pub fn release(&mut self) -> Vec<PenOut> {
        let mut out = vec![];
        match self.cfg.mode {
            Mode::Pointer => {
                let p = self.prev;
                for (was, b) in [(p.touch && p.near, 0x110u16), (p.stylus && p.near, 0x111), (p.stylus2 && p.near, 0x112)] {
                    if was {
                        out.push(PenOut::Button(b, false));
                    }
                }
            }
            Mode::Tablet => {
                if self.prev.near || self.prev.touch {
                    let (x, y, _) = rotate(self.prev.x, self.prev.y, self.cfg.max, self.cfg.rot);
                    out.push(PenOut::Abs(PenState { x, y, ..Default::default() }));
                }
            }
        }
        self.prev = PenState::default();
        self.cur = PenState::default();
        self.last_pos = None;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(p: &mut Pen, v: &[(u16, u16, i32)]) -> Vec<PenOut> {
        let mut o = vec![];
        for &(t, c, x) in v {
            o.extend(p.feed(t, c, x));
        }
        o.extend(p.feed(EV_SYN, 0, 0));
        o
    }

    #[test]
    fn rotation() {
        let m = (1000, 600);
        assert_eq!(rotate(100, 50, m, 0), (100, 50, m));
        assert_eq!(rotate(100, 50, m, 180), (900, 550, m));
        assert_eq!(rotate(100, 50, m, 90), (550, 100, (600, 1000)));
        assert_eq!(rotate(100, 50, m, 270), (50, 900, (600, 1000)));
        // corners stay inside the rotated range
        for r in [0, 90, 180, 270] {
            for (x, y) in [(0, 0), (1000, 600), (0, 600), (1000, 0)] {
                let (a, b, mm) = rotate(x, y, m, r);
                assert!((0..=mm.0).contains(&a) && (0..=mm.1).contains(&b));
            }
        }
    }

    #[test]
    fn pointer_mode() {
        let mut p = Pen::new(PenCfg { mode: Mode::Pointer, rot: 0, max: (1000, 600), gain: 0.5 });
        assert!(ev(&mut p, &[(EV_KEY, BTN_TOOL_PEN, 1), (EV_ABS, ABS_X, 100), (EV_ABS, ABS_Y, 100)]).is_empty(), "first hover frame: no jump");
        assert_eq!(ev(&mut p, &[(EV_ABS, ABS_X, 111)]), vec![PenOut::Rel(5, 0)]);
        assert_eq!(ev(&mut p, &[(EV_ABS, ABS_X, 112)]), vec![PenOut::Rel(1, 0)], "the half unit carried over");
        assert_eq!(ev(&mut p, &[(EV_KEY, BTN_TOUCH, 1), (EV_ABS, ABS_PRESSURE, 300)]), vec![PenOut::Button(0x110, true)]);
        assert_eq!(ev(&mut p, &[(EV_KEY, BTN_STYLUS, 1)]), vec![PenOut::Button(0x111, true)]);
        assert_eq!(p.release(), vec![PenOut::Button(0x110, false), PenOut::Button(0x111, false)]);
        // out of range and back: no jump
        let mut p = Pen::new(PenCfg { mode: Mode::Pointer, rot: 0, max: (1000, 600), gain: 1.0 });
        ev(&mut p, &[(EV_KEY, BTN_TOOL_PEN, 1), (EV_ABS, ABS_X, 100)]);
        ev(&mut p, &[(EV_KEY, BTN_TOOL_PEN, 0)]);
        assert!(ev(&mut p, &[(EV_KEY, BTN_TOOL_PEN, 1), (EV_ABS, ABS_X, 900)]).is_empty());
    }

    #[test]
    fn tablet_mode() {
        let mut p = Pen::new(PenCfg { mode: Mode::Tablet, rot: 180, max: (1000, 600), gain: 1.0 });
        let o = ev(&mut p, &[(EV_KEY, BTN_TOOL_PEN, 1), (EV_ABS, ABS_X, 100), (EV_ABS, ABS_Y, 50), (EV_ABS, ABS_TILT_X, 10)]);
        assert_eq!(o, vec![PenOut::Abs(PenState { x: 900, y: 550, pressure: 0, tilt: (10, 0), near: true, touch: false, stylus: false, stylus2: false })]);
        assert!(ev(&mut p, &[]).is_empty(), "no change: no frame");
        let o = ev(&mut p, &[(EV_KEY, BTN_TOUCH, 1), (EV_ABS, ABS_PRESSURE, 2000)]);
        assert!(matches!(o.as_slice(), [PenOut::Abs(s)] if s.touch && s.pressure == 2000));
        let r = p.release();
        assert!(matches!(r.as_slice(), [PenOut::Abs(s)] if !s.near && !s.touch));
    }
}
