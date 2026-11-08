// SPDX-License-Identifier: GPL-3.0-or-later
//! Relative-mouse fallback (M2, `mode = "mouse"`): our own recognizer for desktops whose touchpad handling is
//! missing or poor (X11 evdev driver) or as a user choice. Input: the touchpad frames (slot positions in device
//! units, as sent to the virtual touchpad); output: REL motion, buttons, hi-res wheel. Numbers follow Denial's
//! INPUT_DECK.md (gain 1.5x, tap <= 250 ms and small movement, 2-finger tap = right, double-tap-hold drag,
//! 2-finger scroll); no gestures.

pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;

#[derive(Clone, Debug, PartialEq)]
pub enum MOut {
    Rel(i32, i32),
    Button(u16, bool),
    /// hi-res wheel units (120 = one notch): vertical, horizontal
    Wheel(i32, i32),
}

#[derive(Clone, Debug)]
pub struct MouseCfg {
    /// device units per mm of the touchpad frames
    pub units_per_mm: f32,
    /// pointer px per mm of finger travel (includes the 1.5x gain and the speed setting)
    pub px_per_mm: f32,
    pub tap_ms: i64,
    pub tap_slop_mm: f32,
    /// finger travel per wheel notch
    pub scroll_mm_per_notch: f32,
    pub natural_scroll: bool,
}

impl Default for MouseCfg {
    fn default() -> MouseCfg {
        MouseCfg { units_per_mm: 124.0, px_per_mm: 6.0 * 1.5, tap_ms: 250, tap_slop_mm: 2.0, scroll_mm_per_notch: 5.0, natural_scroll: false }
    }
}

#[derive(Default)]
pub struct Mouse {
    pub cfg: MouseCfg,
    prev: Vec<Option<(i32, i32)>>,
    t0: i64,
    start: Option<(i32, i32)>,
    max_n: usize,
    moved: bool,
    last_tap: Option<i64>,
    drag: bool,
    acc: (f32, f32),
    wacc: (f32, f32),
}

impl Mouse {
    pub fn new(cfg: MouseCfg) -> Mouse {
        Mouse { cfg, ..Default::default() }
    }

    /// one frame of slot positions at time `t` (ms)
    pub fn feed(&mut self, slots: &[Option<(i32, i32)>], t: i64) -> Vec<MOut> {
        let mut out = vec![];
        let n = slots.iter().filter(|s| s.is_some()).count();
        let pn = self.prev.iter().filter(|s| s.is_some()).count();
        let upm = self.cfg.units_per_mm.max(1.0);
        if pn == 0 && n > 0 {
            self.t0 = t;
            self.start = slots.iter().flatten().next().copied();
            self.max_n = n;
            self.moved = false;
            self.acc = (0.0, 0.0);
            self.wacc = (0.0, 0.0);
            if n == 1 && self.last_tap.is_some_and(|lt| t - lt <= self.cfg.tap_ms) {
                self.drag = true;
                self.last_tap = None;
                out.push(MOut::Button(BTN_LEFT, true));
            }
        }
        self.max_n = self.max_n.max(n);
        // average delta (mm) of the fingers present in both frames
        let (mut dx, mut dy, mut k) = (0.0f32, 0.0f32, 0);
        for (i, s) in slots.iter().enumerate() {
            if let (Some((x, y)), Some(Some((px, py)))) = (s, self.prev.get(i)) {
                dx += (x - px) as f32 / upm;
                dy += (y - py) as f32 / upm;
                k += 1;
            }
        }
        if k > 0 {
            dx /= k as f32;
            dy /= k as f32;
        }
        if let (Some((sx, sy)), Some((x, y))) = (self.start, slots.iter().flatten().next()) {
            let d = (((x - sx) as f32 / upm).powi(2) + ((y - sy) as f32 / upm).powi(2)).sqrt();
            if d > self.cfg.tap_slop_mm {
                self.moved = true;
            }
        }
        if k > 0 && (dx != 0.0 || dy != 0.0) {
            if n == 1 {
                self.acc.0 += dx * self.cfg.px_per_mm;
                self.acc.1 += dy * self.cfg.px_per_mm;
                let (rx, ry) = (self.acc.0.trunc(), self.acc.1.trunc());
                self.acc.0 -= rx;
                self.acc.1 -= ry;
                if rx != 0.0 || ry != 0.0 {
                    out.push(MOut::Rel(rx as i32, ry as i32));
                }
            } else if n == 2 {
                let sign = if self.cfg.natural_scroll { 1.0 } else { -1.0 };
                let per = 120.0 / self.cfg.scroll_mm_per_notch.max(0.5);
                self.wacc.0 += sign * dy * per;
                self.wacc.1 += -sign * dx * per;
                let (v, h) = (self.wacc.0.trunc(), self.wacc.1.trunc());
                self.wacc.0 -= v;
                self.wacc.1 -= h;
                if v != 0.0 || h != 0.0 {
                    out.push(MOut::Wheel(v as i32, h as i32));
                }
            }
        }
        if pn > 0 && n == 0 {
            let quick = t - self.t0 <= self.cfg.tap_ms && !self.moved;
            if self.drag {
                self.drag = false;
                out.push(MOut::Button(BTN_LEFT, false));
            } else if quick {
                let b = match self.max_n {
                    1 => Some(BTN_LEFT),
                    2 => Some(BTN_RIGHT),
                    3 => Some(BTN_MIDDLE),
                    _ => None,
                };
                if let Some(b) = b {
                    out.push(MOut::Button(b, true));
                    out.push(MOut::Button(b, false));
                    self.last_tap = if b == BTN_LEFT { Some(t) } else { None };
                }
            }
            self.start = None;
        }
        self.prev = slots.to_vec();
        out
    }

    /// everything up (release order)
    pub fn release_all(&mut self) -> Vec<MOut> {
        self.prev.clear();
        self.start = None;
        self.last_tap = None;
        if std::mem::take(&mut self.drag) {
            vec![MOut::Button(BTN_LEFT, false)]
        } else {
            vec![]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s1(x: i32, y: i32) -> Vec<Option<(i32, i32)>> {
        vec![Some((x, y)), None]
    }

    #[test]
    fn move_tap_right_tap_scroll_drag() {
        let mut m = Mouse::new(MouseCfg::default());
        // move 10 mm right: 90 px
        m.feed(&s1(1000, 1000), 0);
        let mut px = 0;
        for i in 1..=10 {
            for o in m.feed(&s1(1000 + i * 124, 1000), i as i64 * 10) {
                if let MOut::Rel(x, y) = o {
                    px += x;
                    assert_eq!(y, 0);
                }
            }
        }
        assert_eq!(px, 90);
        assert!(m.feed(&[None, None], 400).is_empty(), "a move is no tap");
        // tap
        m.feed(&s1(500, 500), 1000);
        assert_eq!(m.feed(&[None, None], 1100), vec![MOut::Button(BTN_LEFT, true), MOut::Button(BTN_LEFT, false)]);
        // tap again within 250 ms and hold + move: drag
        let o = m.feed(&s1(500, 500), 1200);
        assert_eq!(o, vec![MOut::Button(BTN_LEFT, true)]);
        m.feed(&s1(900, 500), 1400);
        assert_eq!(m.feed(&[None, None], 1500), vec![MOut::Button(BTN_LEFT, false)]);
        // two-finger tap = right click
        m.feed(&[Some((100, 100)), Some((800, 100))], 3000);
        assert_eq!(m.feed(&[None, None], 3100), vec![MOut::Button(BTN_RIGHT, true), MOut::Button(BTN_RIGHT, false)]);
        // two fingers down by 5 mm: one notch of scroll down (wheel negative = down)
        m.feed(&[Some((100, 100)), Some((800, 100))], 5000);
        let o = m.feed(&[Some((100, 720)), Some((800, 720))], 5050);
        assert_eq!(o, vec![MOut::Wheel(-120, 0)]);
        assert!(m.feed(&[None, None], 5100).is_empty(), "scrolling is no right click");
        let mut n = Mouse::new(MouseCfg { natural_scroll: true, ..Default::default() });
        n.feed(&[Some((100, 100)), Some((800, 100))], 0);
        assert_eq!(n.feed(&[Some((100, 720)), Some((800, 720))], 50), vec![MOut::Wheel(120, 0)]);
    }
}
