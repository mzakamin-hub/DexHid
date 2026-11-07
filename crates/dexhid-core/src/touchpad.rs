// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! Touchpad region -> virtual multitouch touchpad frames. Contacts are re-based to the pad rect and scaled to device
//! units; contacts that start in a click zone (bottom strip of the pad, left / right half) press BTN_LEFT / BTN_RIGHT
//! on the touchpad device instead of becoming fingers (a thumb on a zone + a moving finger = drag).

use crate::geom::Rect;
use crate::keys::ContactId;
use std::collections::HashMap;

pub const SLOTS: usize = 5;
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PadFrame {
    /// per slot: Some((x, y)) in device units while a finger is down
    pub slots: [Option<(i32, i32)>; SLOTS],
    /// button changes in order
    pub buttons: Vec<(u16, bool)>,
}

#[derive(Debug)]
pub struct PadMapper {
    pub rect: Rect,
    /// device units per logical px
    pub units_per_px: f32,
    /// click-zone height (px), 0 = no zones
    pub zone_h: f32,
    slot_of: HashMap<ContactId, usize>,
    pos: [Option<(i32, i32)>; SLOTS],
    btn_of: HashMap<ContactId, u16>,
    btn_count: HashMap<u16, u32>,
    buttons: Vec<(u16, bool)>,
    dirty: bool,
}

impl PadMapper {
    pub fn new(rect: Rect, units_per_px: f32, zone_h: f32) -> PadMapper {
        PadMapper {
            rect,
            units_per_px,
            zone_h: zone_h.min(rect.h / 3.0).max(0.0),
            slot_of: HashMap::new(),
            pos: [None; SLOTS],
            btn_of: HashMap::new(),
            btn_count: HashMap::new(),
            buttons: vec![],
            dirty: false,
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

    /// a contact starts on the pad; false = not accepted (all slots busy)
    pub fn down(&mut self, id: ContactId, x: f32, y: f32) -> bool {
        if let Some(b) = self.zone_button(x, y) {
            self.btn_of.insert(id, b);
            let n = self.btn_count.entry(b).or_insert(0);
            *n += 1;
            if *n == 1 {
                self.buttons.push((b, true));
                self.dirty = true;
            }
            return true;
        }
        let Some(s) = (0..SLOTS).find(|s| self.pos[*s].is_none()) else { return false };
        self.slot_of.insert(id, s);
        self.pos[s] = Some(self.to_dev(x, y));
        self.dirty = true;
        true
    }

    pub fn motion(&mut self, id: ContactId, x: f32, y: f32) {
        if let Some(&s) = self.slot_of.get(&id) {
            let p = Some(self.to_dev(x, y));
            if self.pos[s] != p {
                self.pos[s] = p;
                self.dirty = true;
            }
        }
    }

    pub fn up(&mut self, id: ContactId) {
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

    pub fn owns(&self, id: ContactId) -> bool {
        self.slot_of.contains_key(&id) || self.btn_of.contains_key(&id)
    }

    pub fn active(&self) -> bool {
        !self.slot_of.is_empty() || !self.btn_of.is_empty()
    }

    /// the frame to emit, if anything changed since the last call
    pub fn take_frame(&mut self) -> Option<PadFrame> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;
        Some(PadFrame { slots: self.pos, buttons: std::mem::take(&mut self.buttons) })
    }

    /// lift every finger and button (before an ungrab / destroy / layout change)
    pub fn release_all(&mut self) -> Option<PadFrame> {
        let ids: Vec<ContactId> = self.slot_of.keys().chain(self.btn_of.keys()).copied().collect();
        for id in ids {
            self.up(id);
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
        assert!(p.down(1, 150.0, 650.0));
        let f = p.take_frame().unwrap();
        assert_eq!(f.slots[0], Some((1000, 1000)));
        assert!(p.take_frame().is_none());
        // a finger that leaves the pad rect is clamped to the edge
        p.motion(1, 50.0, 2000.0);
        assert_eq!(p.take_frame().unwrap().slots[0], Some((0, 9999)));
        p.up(1);
        assert_eq!(p.take_frame().unwrap().slots[0], None);
        assert!(!p.active());
    }

    #[test]
    fn click_zones_press_buttons() {
        let mut p = pad();
        assert!(p.down(1, 200.0, 1050.0));
        let f = p.take_frame().unwrap();
        assert_eq!(f.buttons, vec![(BTN_LEFT, true)]);
        assert_eq!(f.slots, [None; SLOTS], "a zone contact is not a finger");
        // drag: thumb on the zone + a moving finger
        p.down(2, 500.0, 700.0);
        p.motion(2, 600.0, 700.0);
        assert_eq!(p.take_frame().unwrap().slots[0], Some((10000, 2000)));
        p.up(1);
        assert_eq!(p.take_frame().unwrap().buttons, vec![(BTN_LEFT, false)]);
        p.down(3, 900.0, 1090.0);
        assert_eq!(p.take_frame().unwrap().buttons, vec![(BTN_RIGHT, true)]);
        let f = p.release_all().unwrap();
        assert_eq!(f.buttons, vec![(BTN_RIGHT, false)]);
        assert_eq!(f.slots, [None; SLOTS]);
    }

    #[test]
    fn five_slots_max() {
        let mut p = PadMapper::new(Rect::new(0.0, 0.0, 1000.0, 1000.0), 10.0, 0.0);
        for i in 0..5 {
            assert!(p.down(i, 10.0 * i as f32, 10.0));
        }
        assert!(!p.down(9, 1.0, 1.0));
        assert!(p.zones().is_none());
        p.up(2);
        assert!(p.down(9, 1.0, 1.0));
        assert_eq!(p.take_frame().unwrap().slots[2], Some((10, 10)));
    }
}
