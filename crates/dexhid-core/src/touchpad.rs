// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! Touchpad region -> virtual multitouch touchpad frames. Contacts are re-based to the pad rect and scaled to device
//! units; contacts that start in a click zone (bottom strip of the pad, left / right half) press BTN_LEFT / BTN_RIGHT
//! on the touchpad device instead of becoming fingers (a thumb on a zone + a moving finger = drag).
//!
//! M2.1:
//! - click zones hold their buttons independently of each other and of taps; with `chord_middle` both zones
//!   pressed within `CHORD_MS` give BTN_MIDDLE (as libinput's middle-button emulation: the first zone waits up to
//!   50 ms for the second; a finger starting on the pad or the zone's release ends the wait at once);
//! - two-finger scroll inertia: when both fingers of a two-finger scroll lift while moving faster than
//!   `Inertia::start_mm_s`, the two virtual fingers stay down and keep moving with an exponentially decaying speed
//!   (`friction` per second) until slower than `stop_mm_s`, the pad edge, or any new touch; libinput turns that into
//!   ordinary finger-scroll events. The first finger to lift is kept down for up to `HOLD_MS` while waiting for the
//!   second (fingers never lift at exactly the same time).

use crate::geom::Rect;
use crate::keys::ContactId;
use std::collections::{HashMap, VecDeque};

pub const SLOTS: usize = 5;
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;
/// window for the left + right zone chord (ms)
pub const CHORD_MS: i64 = 50;
/// how long the first lifted finger of a scroll is kept for the second (ms)
pub const HOLD_MS: i64 = 100;
/// inertia step (ms)
pub const STEP_MS: i64 = 8;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PadFrame {
    /// per slot: Some((x, y)) in device units while a finger is down
    pub slots: [Option<(i32, i32)>; SLOTS],
    /// button changes in order
    pub buttons: Vec<(u16, bool)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Inertia {
    pub enabled: bool,
    /// decay rate per second (v(t) = v0 * exp(-friction * t))
    pub friction: f32,
    pub start_mm_s: f32,
    pub stop_mm_s: f32,
}

impl Default for Inertia {
    fn default() -> Inertia {
        Inertia { enabled: false, friction: 4.0, start_mm_s: 120.0, stop_mm_s: 15.0 }
    }
}

#[derive(Debug, Clone)]
struct Held {
    slot: usize,
    other: ContactId,
    since: i64,
    /// the two-finger velocity when the first finger lifted
    v: (f32, f32),
}

#[derive(Debug, Clone)]
struct Coast {
    slots: [usize; 2],
    /// device units per ms
    v: (f32, f32),
    pos: [(f32, f32); 2],
    last: i64,
}

#[derive(Debug)]
pub struct PadMapper {
    pub rect: Rect,
    /// device units per logical px
    pub units_per_px: f32,
    /// click-zone height (px), 0 = no zones
    pub zone_h: f32,
    /// device units per mm (for the inertia thresholds)
    pub units_per_mm: f32,
    pub chord_middle: bool,
    pub inertia: Inertia,
    slot_of: HashMap<ContactId, usize>,
    pos: [Option<(i32, i32)>; SLOTS],
    btn_of: HashMap<ContactId, u16>,
    btn_count: HashMap<u16, u32>,
    buttons: Vec<(u16, bool)>,
    dirty: bool,
    /// a zone press waiting for the chord partner: (button, contact, since)
    pending: Option<(u16, ContactId, i64)>,
    /// two-finger centroid history (t, x, y) in device units
    hist: VecDeque<(i64, f32, f32)>,
    held: Option<Held>,
    coast: Option<Coast>,
}

impl PadMapper {
    pub fn new(rect: Rect, units_per_px: f32, zone_h: f32) -> PadMapper {
        PadMapper {
            rect,
            units_per_px,
            zone_h: zone_h.min(rect.h / 3.0).max(0.0),
            units_per_mm: 124.0,
            chord_middle: false,
            inertia: Inertia::default(),
            slot_of: HashMap::new(),
            pos: [None; SLOTS],
            btn_of: HashMap::new(),
            btn_count: HashMap::new(),
            buttons: vec![],
            dirty: false,
            pending: None,
            hist: VecDeque::new(),
            held: None,
            coast: None,
        }
    }

    /// device axis maxima (inclusive)
    pub fn dims(&self) -> (i32, i32) {
        (((self.rect.w * self.units_per_px).round() as i32 - 1).max(1), ((self.rect.h * self.units_per_px).round() as i32 - 1).max(1))
    }

    pub fn zones(&self) -> Option<(Rect, Rect)> {
        if self.zone_h <= 0.0 {
            return None;
        }
        let y = self.rect.bottom() - self.zone_h;
        let hw = self.rect.w / 2.0;
        Some((Rect::new(self.rect.x, y, hw, self.zone_h), Rect::new(self.rect.x + hw, y, hw, self.zone_h)))
    }

    fn zone_button(&self, x: f32, y: f32) -> Option<u16> {
        let (l, r) = self.zones()?;
        if l.contains(x, y) {
            Some(BTN_LEFT)
        } else if r.contains(x, y) {
            Some(BTN_RIGHT)
        } else {
            None
        }
    }

    fn to_dev(&self, x: f32, y: f32) -> (i32, i32) {
        let (mx, my) = self.dims();
        (
            (((x - self.rect.x) * self.units_per_px).round() as i32).clamp(0, mx),
            (((y - self.rect.y) * self.units_per_px).round() as i32).clamp(0, my),
        )
    }

    fn btn_press(&mut self, id: ContactId, b: u16) {
        self.btn_of.insert(id, b);
        let n = self.btn_count.entry(b).or_insert(0);
        *n += 1;
        if *n == 1 {
            self.buttons.push((b, true));
            self.dirty = true;
        }
    }

    /// the waiting zone press becomes a normal button press now
    fn flush_pending(&mut self) {
        if let Some((b, id, _)) = self.pending.take() {
            self.btn_press(id, b);
        }
    }

    /// virtual fingers kept after their real finger lifted (scroll hold / inertia)
    pub fn coasting(&self) -> bool {
        self.held.is_some() || self.coast.is_some()
    }

    /// lift the held / coasting virtual fingers (a new touch, a swipe, a release)
    pub fn stop_coast(&mut self) {
        if let Some(h) = self.held.take() {
            self.pos[h.slot] = None;
            self.dirty = true;
        }
        if let Some(c) = self.coast.take() {
            for s in c.slots {
                self.pos[s] = None;
            }
            self.dirty = true;
        }
        self.hist.clear();
    }

    /// a contact starts on the pad; false = not accepted (all slots busy)
    pub fn down(&mut self, id: ContactId, x: f32, y: f32, t: i64) -> bool {
        self.stop_coast();
        if let Some(b) = self.zone_button(x, y) {
            if self.chord_middle {
                match self.pending {
                    Some((pb, pid, _)) if pb != b => {
                        // both zones within the window: one middle button for both contacts
                        self.pending = None;
                        self.btn_press(pid, BTN_MIDDLE);
                        self.btn_press(id, BTN_MIDDLE);
                    }
                    Some(_) => {
                        self.flush_pending();
                        self.pending = Some((b, id, t));
                    }
                    None => self.pending = Some((b, id, t)),
                }
            } else {
                self.btn_press(id, b);
            }
            return true;
        }
        // a finger on the pad: a waiting zone press is a drag button now
        self.flush_pending();
        let Some(s) = (0..SLOTS).find(|s| self.pos[*s].is_none()) else { return false };
        self.slot_of.insert(id, s);
        self.pos[s] = Some(self.to_dev(x, y));
        self.dirty = true;
        self.hist.clear();
        true
    }

    fn two_finger_centroid(&self) -> Option<(f32, f32)> {
        if self.slot_of.len() != 2 || !self.btn_of.is_empty() {
            return None;
        }
        let mut sx = 0.0;
        let mut sy = 0.0;
        for s in self.slot_of.values() {
            let (x, y) = self.pos[*s]?;
            sx += x as f32;
            sy += y as f32;
        }
        Some((sx / 2.0, sy / 2.0))
    }

    fn record(&mut self, t: i64) {
        if let Some((x, y)) = self.two_finger_centroid() {
            self.hist.push_back((t, x, y));
            while self.hist.front().is_some_and(|h| t - h.0 > 80) {
                self.hist.pop_front();
            }
        }
    }

    /// recent two-finger velocity (device units per ms)
    fn velocity(&self) -> Option<(f32, f32)> {
        let a = self.hist.front()?;
        let b = self.hist.back()?;
        let dt = (b.0 - a.0) as f32;
        if dt < 15.0 {
            return None;
        }
        Some(((b.1 - a.1) / dt, (b.2 - a.2) / dt))
    }

    fn speed_mm_s(&self, v: (f32, f32)) -> f32 {
        (v.0 * v.0 + v.1 * v.1).sqrt() / self.units_per_mm.max(1.0) * 1000.0
    }

    pub fn motion(&mut self, id: ContactId, x: f32, y: f32, t: i64) {
        if let Some(&s) = self.slot_of.get(&id) {
            let p = Some(self.to_dev(x, y));
            if self.pos[s] != p {
                // the held virtual finger follows the remaining real one
                if let (Some(h), Some(old), Some(new)) = (self.held.clone(), self.pos[s], p) {
                    if h.other == id {
                        if let Some(hp) = self.pos[h.slot] {
                            let (mx, my) = self.dims();
                            self.pos[h.slot] = Some(((hp.0 + new.0 - old.0).clamp(0, mx), (hp.1 + new.1 - old.1).clamp(0, my)));
                        }
                    }
                }
                self.pos[s] = p;
                self.dirty = true;
                if self.held.is_none() {
                    self.record(t);
                } else if let Some(h) = &self.held {
                    // keep sampling the remaining finger
                    if h.other == id {
                        if let Some((nx, ny)) = p {
                            self.hist.push_back((t, nx as f32, ny as f32));
                            while self.hist.front().is_some_and(|q| t - q.0 > 80) {
                                self.hist.pop_front();
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn up(&mut self, id: ContactId, t: i64) {
        if let Some(s) = self.slot_of.remove(&id) {
            let fast = self.inertia.enabled && self.velocity().is_some_and(|v| self.speed_mm_s(v) >= self.inertia.start_mm_s);
            match self.held.clone() {
                None if fast && self.slot_of.len() == 1 && self.btn_of.is_empty() && self.coast.is_none() => {
                    // first finger of a fast two-finger scroll: keep its virtual finger for the second one
                    let other = *self.slot_of.keys().next().unwrap();
                    let v = self.velocity().unwrap_or((0.0, 0.0));
                    self.held = Some(Held { slot: s, other, since: t, v });
                    // continue the history with the remaining finger only
                    self.hist.clear();
                    if let Some(p) = self.pos[self.slot_of[&other]] {
                        self.hist.push_back((t, p.0 as f32, p.1 as f32));
                    }
                }
                Some(h) if h.other == id => {
                    self.held = None;
                    // the remaining finger's own recent velocity, else the one at the first lift
                    let v = self.velocity().or(Some(h.v)).filter(|v| self.speed_mm_s(*v) >= self.inertia.start_mm_s);
                    match (v, self.pos[h.slot], self.pos[s]) {
                        (Some(v), Some(a), Some(b)) => {
                            self.coast = Some(Coast { slots: [h.slot, s], v, pos: [(a.0 as f32, a.1 as f32), (b.0 as f32, b.1 as f32)], last: t });
                        }
                        _ => {
                            self.pos[h.slot] = None;
                            self.pos[s] = None;
                            self.dirty = true;
                        }
                    }
                }
                _ => {
                    self.pos[s] = None;
                    self.dirty = true;
                }
            }
            if self.coast.is_none() && self.held.is_none() {
                self.hist.clear();
            }
        }
        if self.pending.is_some_and(|p| p.1 == id) {
            // a short zone tap inside the chord window: a click
            self.flush_pending();
        }
        if let Some(b) = self.btn_of.remove(&id) {
            if let Some(n) = self.btn_count.get_mut(&b) {
                *n -= 1;
                if *n == 0 {
                    self.btn_count.remove(&b);
                    self.buttons.push((b, false));
                    self.dirty = true;
                }
            }
        }
    }

    /// timers: the chord window, the scroll hold, the inertia steps
    pub fn tick(&mut self, t: i64) {
        if self.pending.is_some_and(|p| t - p.2 >= CHORD_MS) {
            self.flush_pending();
        }
        if self.held.as_ref().is_some_and(|h| t - h.since >= HOLD_MS) {
            let h = self.held.take().unwrap();
            self.pos[h.slot] = None;
            self.dirty = true;
            self.hist.clear();
        }
        if let Some(mut c) = self.coast.take() {
            let (mx, my) = self.dims();
            let mut done = false;
            while t - c.last >= STEP_MS && !done {
                let dt = STEP_MS as f32;
                let k = (-self.inertia.friction * dt / 1000.0).exp();
                c.v = (c.v.0 * k, c.v.1 * k);
                c.last += STEP_MS;
                for p in c.pos.iter_mut() {
                    let nx = p.0 + c.v.0 * dt;
                    let ny = p.1 + c.v.1 * dt;
                    if nx < 0.0 || ny < 0.0 || nx > mx as f32 || ny > my as f32 {
                        done = true;
                    }
                    *p = (nx.clamp(0.0, mx as f32), ny.clamp(0.0, my as f32));
                }
                if self.speed_mm_s(c.v) < self.inertia.stop_mm_s {
                    done = true;
                }
            }
            for (i, s) in c.slots.iter().enumerate() {
                self.pos[*s] = Some((c.pos[i].0.round() as i32, c.pos[i].1.round() as i32));
            }
            self.dirty = true;
            if done {
                for s in c.slots {
                    self.pos[s] = None;
                }
            } else {
                self.coast = Some(c);
            }
        }
    }

    pub fn next_deadline(&self) -> Option<i64> {
        let mut d: Option<i64> = None;
        let mut m = |x: i64| d = Some(d.map_or(x, |y: i64| y.min(x)));
        if let Some(p) = self.pending {
            m(p.2 + CHORD_MS);
        }
        if let Some(h) = &self.held {
            m(h.since + HOLD_MS);
        }
        if let Some(c) = &self.coast {
            m(c.last + STEP_MS);
        }
        d
    }

    pub fn owns(&self, id: ContactId) -> bool {
        self.slot_of.contains_key(&id) || self.btn_of.contains_key(&id) || self.pending.is_some_and(|p| p.1 == id)
    }

    pub fn active(&self) -> bool {
        !self.slot_of.is_empty() || !self.btn_of.is_empty() || self.pending.is_some() || self.coasting()
    }

    /// buttons currently down (for tests / status)
    pub fn buttons_down(&self) -> Vec<u16> {
        let mut v: Vec<u16> = self.btn_count.keys().copied().collect();
        v.sort_unstable();
        v
    }

    /// the frame to emit, if anything changed since the last call
    pub fn take_frame(&mut self) -> Option<PadFrame> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;
        Some(PadFrame { slots: self.pos, buttons: std::mem::take(&mut self.buttons) })
    }

    /// M3: a button click on the touchpad (gesture actions: middle click) with the fingers where they are
    pub fn click(&mut self, btn: u16) -> Vec<PadFrame> {
        let mut v = vec![];
        if let Some(f) = self.take_frame() {
            v.push(f);
        }
        v.push(PadFrame { slots: self.pos, buttons: vec![(btn, true)] });
        v.push(PadFrame { slots: self.pos, buttons: vec![(btn, false)] });
        v
    }

    /// lift every finger and button (before an ungrab / destroy / layout change / swipe)
    pub fn release_all(&mut self) -> Option<PadFrame> {
        self.pending = None;
        self.stop_coast();
        let ids: Vec<ContactId> = self.slot_of.keys().chain(self.btn_of.keys()).copied().collect();
        for id in ids {
            if let Some(s) = self.slot_of.remove(&id) {
                self.pos[s] = None;
                self.dirty = true;
            }
            if let Some(b) = self.btn_of.remove(&id) {
                if let Some(n) = self.btn_count.get_mut(&b) {
                    *n -= 1;
                    if *n == 0 {
                        self.btn_count.remove(&b);
                        self.buttons.push((b, false));
                        self.dirty = true;
                    }
                }
            }
        }
        self.take_frame()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad() -> PadMapper {
        // 1000 x 500 px pad at (100, 600), 20 device units per px, 100 px click zone
        PadMapper::new(Rect::new(100.0, 600.0, 1000.0, 500.0), 20.0, 100.0)
    }

    #[test]
    fn rebase_and_clamp() {
        let mut p = pad();
        assert_eq!(p.dims(), (19999, 9999));
        assert!(p.down(1, 150.0, 650.0, 0));
        let f = p.take_frame().unwrap();
        assert_eq!(f.slots[0], Some((1000, 1000)));
        assert!(p.take_frame().is_none());
        // a finger that leaves the pad rect is clamped to the edge
        p.motion(1, 50.0, 2000.0, 10);
        assert_eq!(p.take_frame().unwrap().slots[0], Some((0, 9999)));
        p.up(1, 20);
        assert_eq!(p.take_frame().unwrap().slots[0], None);
        assert!(!p.active());
    }

    #[test]
    fn click_zones_press_buttons() {
        let mut p = pad();
        assert!(p.down(1, 200.0, 1050.0, 0));
        let f = p.take_frame().unwrap();
        assert_eq!(f.buttons, vec![(BTN_LEFT, true)]);
        assert_eq!(f.slots, [None; SLOTS], "a zone contact is not a finger");
        // drag: thumb on the zone + a moving finger
        p.down(2, 500.0, 700.0, 10);
        p.motion(2, 600.0, 700.0, 20);
        assert_eq!(p.take_frame().unwrap().slots[0], Some((10000, 2000)));
        p.up(1, 30);
        assert_eq!(p.take_frame().unwrap().buttons, vec![(BTN_LEFT, false)]);
        p.down(3, 900.0, 1090.0, 40);
        assert_eq!(p.take_frame().unwrap().buttons, vec![(BTN_RIGHT, true)]);
        let f = p.release_all().unwrap();
        assert_eq!(f.buttons, vec![(BTN_RIGHT, false)]);
        assert_eq!(f.slots, [None; SLOTS]);
    }

    /// M2.1 item 4: the zones are independent buttons (both can be held, either released first), a resting thumb
    /// keeps its button while fingers come and go on the pad
    #[test]
    fn zones_independent_and_thumb_drag() {
        let mut p = pad();
        p.down(1, 200.0, 1050.0, 0);
        p.down(2, 900.0, 1050.0, 5);
        let f = p.take_frame().unwrap();
        assert_eq!(f.buttons, vec![(BTN_LEFT, true), (BTN_RIGHT, true)], "both zones chord: both buttons");
        p.up(1, 100);
        assert_eq!(p.take_frame().unwrap().buttons, vec![(BTN_LEFT, false)], "left released alone, right still held");
        assert_eq!(p.buttons_down(), vec![BTN_RIGHT]);
        p.up(2, 120);
        assert_eq!(p.take_frame().unwrap().buttons, vec![(BTN_RIGHT, false)]);
        // thumb rests on the left zone, two separate finger strokes on the pad: the button stays down throughout
        p.down(3, 200.0, 1050.0, 200);
        for (i, id) in [10u32, 11].iter().enumerate() {
            let t = 300 + i as i64 * 200;
            p.down(*id, 500.0, 700.0, t);
            p.motion(*id, 700.0, 750.0, t + 50);
            p.up(*id, t + 100);
        }
        let mut btns = vec![];
        while let Some(f) = p.take_frame() {
            btns.extend(f.buttons);
        }
        assert_eq!(btns, vec![(BTN_LEFT, true)], "no release while the thumb rests");
        p.up(3, 900);
        assert_eq!(p.take_frame().unwrap().buttons, vec![(BTN_LEFT, false)]);
    }

    #[test]
    fn zones_chord_to_middle() {
        let mut p = pad();
        p.chord_middle = true;
        p.down(1, 200.0, 1050.0, 0);
        assert!(p.take_frame().is_none(), "the first zone waits for a partner");
        assert_eq!(p.next_deadline(), Some(CHORD_MS));
        p.down(2, 900.0, 1050.0, 30);
        assert_eq!(p.take_frame().unwrap().buttons, vec![(BTN_MIDDLE, true)]);
        p.up(1, 100);
        assert!(p.take_frame().is_none(), "middle stays while one zone is held");
        p.up(2, 120);
        assert_eq!(p.take_frame().unwrap().buttons, vec![(BTN_MIDDLE, false)]);
        // alone: pressed after the window
        p.down(3, 200.0, 1050.0, 1000);
        p.tick(1049);
        assert!(p.take_frame().is_none());
        p.tick(1050);
        assert_eq!(p.take_frame().unwrap().buttons, vec![(BTN_LEFT, true)]);
        p.up(3, 1100);
        // a quick zone tap inside the window still clicks
        p.take_frame();
        p.down(4, 200.0, 1050.0, 2000);
        p.up(4, 2020);
        assert_eq!(p.take_frame().unwrap().buttons, vec![(BTN_LEFT, true), (BTN_LEFT, false)]);
        // a pad finger ends the wait at once (drag)
        p.down(5, 200.0, 1050.0, 3000);
        p.down(6, 500.0, 700.0, 3010);
        assert_eq!(p.take_frame().unwrap().buttons, vec![(BTN_LEFT, true)]);
    }

    fn scroll(p: &mut PadMapper, t0: i64, step_px: f32) -> i64 {
        p.down(1, 400.0, 900.0, t0);
        p.down(2, 600.0, 900.0, t0);
        let mut t = t0;
        for i in 1..=10 {
            t = t0 + i * 8;
            p.motion(1, 400.0, 900.0 - step_px * i as f32, t);
            p.motion(2, 600.0, 900.0 - step_px * i as f32, t);
        }
        while p.take_frame().is_some() {}
        t
    }

    #[test]
    fn scroll_inertia_coasts_and_decays() {
        let mut p = pad();
        p.inertia.enabled = true;
        p.units_per_mm = 124.0;
        // 2 px per 8 ms = 40 device units per 8 ms = 5 u/ms = 40 mm/s: too slow, no inertia
        let t = scroll(&mut p, 0, 2.0);
        p.up(1, t + 2);
        p.up(2, t + 4);
        let f = p.take_frame().unwrap();
        assert!(f.slots.iter().all(|s| s.is_none()), "slow scroll: lifted normally");
        assert!(!p.coasting());
        // fast: 20 px per 8 ms = 50 u/ms = 403 mm/s
        let t = scroll(&mut p, 1000, 20.0);
        p.up(1, t + 2);
        assert!(p.take_frame().is_none(), "first finger kept down for the second");
        assert!(p.coasting());
        p.up(2, t + 6);
        assert!(p.take_frame().is_none(), "both kept: coasting");
        let y0 = p.pos[0].unwrap().1;
        let mut last_dy = i32::MAX;
        let mut tt = t + 6;
        let mut steps = 0;
        while p.coasting() && steps < 500 {
            tt = p.next_deadline().unwrap();
            p.tick(tt);
            if let Some(f) = p.take_frame() {
                if let (Some(a), Some(b)) = (f.slots[0], f.slots[1]) {
                    assert_eq!(a.1, b.1, "both fingers move together");
                }
            }
            steps += 1;
            if let Some(y) = p.pos[0].map(|x| x.1) {
                let dy = y0 - y;
                assert!(dy <= y0, "keeps going up");
                last_dy = dy;
            }
        }
        assert!(steps > 5 && !p.coasting(), "coasted {} steps then stopped", steps);
        assert!(last_dy > 0, "moved on after the lift");
        assert!(p.pos.iter().all(|s| s.is_none()), "fingers lifted at the end");
        let _ = tt;
    }

    #[test]
    fn inertia_cancelled_by_a_touch_and_held_finger_timeout() {
        let mut p = pad();
        p.inertia.enabled = true;
        let t = scroll(&mut p, 0, 20.0);
        p.up(1, t + 2);
        p.up(2, t + 4);
        p.tick(t + 12);
        assert!(p.coasting());
        p.take_frame();
        p.down(9, 500.0, 700.0, t + 20);
        let f = p.take_frame().unwrap();
        assert_eq!(f.slots.iter().filter(|s| s.is_some()).count(), 1, "coast lifted, only the new finger");
        assert!(!p.coasting());
        p.up(9, t + 40);
        p.take_frame();
        // only one finger lifts and the other stays: the held virtual finger goes after HOLD_MS
        let t = scroll(&mut p, 5000, 20.0);
        p.up(1, t);
        assert!(p.coasting());
        p.motion(2, 650.0, 700.0, t + 30);
        let f = p.take_frame().unwrap();
        assert_eq!(f.slots.iter().filter(|s| s.is_some()).count(), 2, "held finger follows the other");
        p.tick(t + HOLD_MS);
        let f = p.take_frame().unwrap();
        assert_eq!(f.slots.iter().filter(|s| s.is_some()).count(), 1);
        // inertia off: plain lift
        let mut q = pad();
        let t = scroll(&mut q, 0, 20.0);
        q.up(1, t);
        assert!(!q.coasting());
    }

    #[test]
    fn five_slots_max() {
        let mut p = PadMapper::new(Rect::new(0.0, 0.0, 1000.0, 1000.0), 10.0, 0.0);
        for i in 0..5 {
            assert!(p.down(i, 10.0 * i as f32, 10.0, 0));
        }
        assert!(!p.down(9, 1.0, 1.0, 0));
        assert!(p.zones().is_none());
        p.up(2, 10);
        assert!(p.down(9, 1.0, 1.0, 20));
        assert_eq!(p.take_frame().unwrap().slots[2], Some((10, 10)));
    }
}
