// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! evdev multitouch protocol B -> contact events (down / move / up) at each SYN_REPORT.

use crate::keys::ContactId;

pub const EV_SYN: u16 = 0x00;
pub const EV_ABS: u16 = 0x03;
pub const SYN_REPORT: u16 = 0;
pub const SYN_DROPPED: u16 = 3;
pub const ABS_MT_SLOT: u16 = 0x2f;
pub const ABS_MT_TOUCH_MAJOR: u16 = 0x30;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Contact {
    Down { id: ContactId, x: i32, y: i32, major: i32 },
    Move { id: ContactId, x: i32, y: i32, major: i32 },
    Up { id: ContactId },
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    tid: i32,
    x: i32,
    y: i32,
    major: i32,
    /// our contact id while down
    id: Option<ContactId>,
    /// tracking id changed / position changed since the last report
    new: bool,
    moved: bool,
    /// contact present when we lost sync: ignored until it lifts
    ignore: bool,
}

pub struct Mt {
    slots: Vec<Slot>,
    cur: usize,
    next: ContactId,
    dropping: bool,
}

impl Mt {
    pub fn new(nslots: usize) -> Mt {
        let s = Slot { tid: -1, x: 0, y: 0, major: 0, id: None, new: false, moved: false, ignore: false };
        Mt { slots: vec![s; nslots.max(1)], cur: 0, next: 1, dropping: false }
    }

    pub fn active(&self) -> usize {
        self.slots.iter().filter(|s| s.tid != -1).count()
    }

    /// Feed one event. Returns the contact changes at SYN_REPORT.
    pub fn feed(&mut self, ty: u16, code: u16, value: i32) -> Vec<Contact> {
        if ty == EV_SYN && code == SYN_DROPPED {
            self.dropping = true;
            return self.lift_all();
        }
        if self.dropping {
            // events until the next SYN_REPORT are incomplete: discard them
            if ty == EV_SYN && code == SYN_REPORT {
                self.dropping = false;
            }
            return vec![];
        }
        let n = self.slots.len();
        match (ty, code) {
            (EV_ABS, ABS_MT_SLOT) => self.cur = (value.max(0) as usize).min(n - 1),
            (EV_ABS, ABS_MT_TRACKING_ID) => {
                let s = &mut self.slots[self.cur];
                if value == -1 {
                    s.tid = -1;
                    s.ignore = false;
                } else if s.tid != value {
                    // a new contact (a slot can change tracking id without an explicit -1 in between);
                    // a new tracking id also ends the "ignore until lift" of a slot we lost sync on
                    s.tid = value;
                    s.new = true;
                    s.ignore = false;
                }
            }
            (EV_ABS, ABS_MT_POSITION_X) => {
                self.slots[self.cur].x = value;
                self.slots[self.cur].moved = true;
            }
            (EV_ABS, ABS_MT_POSITION_Y) => {
                self.slots[self.cur].y = value;
                self.slots[self.cur].moved = true;
            }
            (EV_ABS, ABS_MT_TOUCH_MAJOR) => self.slots[self.cur].major = value,
            (EV_SYN, SYN_REPORT) => return self.report(),
            _ => {}
        }
        vec![]
    }

    fn report(&mut self) -> Vec<Contact> {
        let mut out = vec![];
        // ups first, so a finger lifted and another put down in the same frame frees its slot first
        for s in self.slots.iter_mut() {
            if let Some(id) = s.id {
                if s.tid == -1 || s.new {
                    out.push(Contact::Up { id });
                    s.id = None;
                }
            }
        }
        for s in self.slots.iter_mut() {
            if s.tid != -1 && !s.ignore {
                if s.id.is_none() {
                    let id = self.next;
                    self.next = self.next.wrapping_add(1).max(1);
                    s.id = Some(id);
                    out.push(Contact::Down { id, x: s.x, y: s.y, major: s.major });
                } else if s.moved {
                    out.push(Contact::Move { id: s.id.unwrap(), x: s.x, y: s.y, major: s.major });
                }
            }
            s.new = false;
            s.moved = false;
        }
        out
    }

    /// lift every contact we track and ignore the slots that are down until they lift (after SYN_DROPPED, a grab,
    /// or at start when contacts are already down)
    pub fn lift_all(&mut self) -> Vec<Contact> {
        let mut out = vec![];
        for s in self.slots.iter_mut() {
            if let Some(id) = s.id.take() {
                out.push(Contact::Up { id });
            }
            if s.tid != -1 {
                s.ignore = true;
            }
            s.new = false;
            s.moved = false;
        }
        out
    }

    /// set the live slot state (EVIOCGMTSLOTS tracking ids): slots already down are ignored until they lift
    pub fn sync_tids(&mut self, tids: &[i32]) {
        for (s, &t) in self.slots.iter_mut().zip(tids) {
            s.tid = t;
            s.ignore = t != -1;
            s.id = None;
        }
    }

    /// Full resync at grab time (M2 fix): tracking ids, the last positions / touch major of every slot and the
    /// current slot (EVIOCGMTSLOTS + EVIOCGABS(ABS_MT_SLOT)). The kernel drops ABS values equal to the stored
    /// ones, so a new contact at the exact spot of the previous one carries no position at all: without the
    /// stored positions it would land at (0, 0) (M1 T10: a second Shift press after a restart did nothing).
    pub fn sync_full(&mut self, tids: &[i32], xs: &[i32], ys: &[i32], majors: &[i32], cur: i32) {
        self.sync_tids(tids);
        for (i, s) in self.slots.iter_mut().enumerate() {
            if let Some(&x) = xs.get(i) {
                s.x = x;
            }
            if let Some(&y) = ys.get(i) {
                s.y = y;
            }
            if let Some(&m) = majors.get(i) {
                s.major = m;
            }
        }
        self.cur = (cur.max(0) as usize).min(self.slots.len() - 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(m: &mut Mt, v: &[(u16, u16, i32)]) -> Vec<Contact> {
        let mut out = vec![];
        for &(t, c, val) in v {
            out.extend(m.feed(t, c, val));
        }
        out
    }

    #[test]
    fn down_move_up() {
        let mut m = Mt::new(10);
        let r = ev(&mut m, &[(EV_ABS, ABS_MT_SLOT, 0), (EV_ABS, ABS_MT_TRACKING_ID, 7), (EV_ABS, ABS_MT_POSITION_X, 100), (EV_ABS, ABS_MT_POSITION_Y, 200), (EV_SYN, SYN_REPORT, 0)]);
        assert_eq!(r, vec![Contact::Down { id: 1, x: 100, y: 200, major: 0 }]);
        let r = ev(&mut m, &[(EV_ABS, ABS_MT_POSITION_X, 110), (EV_SYN, SYN_REPORT, 0)]);
        assert_eq!(r, vec![Contact::Move { id: 1, x: 110, y: 200, major: 0 }]);
        // second finger in slot 3
        let r = ev(&mut m, &[(EV_ABS, ABS_MT_SLOT, 3), (EV_ABS, ABS_MT_TRACKING_ID, 8), (EV_ABS, ABS_MT_POSITION_X, 5), (EV_SYN, SYN_REPORT, 0)]);
        assert_eq!(r, vec![Contact::Down { id: 2, x: 5, y: 0, major: 0 }]);
        assert_eq!(m.active(), 2);
        let r = ev(&mut m, &[(EV_ABS, ABS_MT_SLOT, 0), (EV_ABS, ABS_MT_TRACKING_ID, -1), (EV_SYN, SYN_REPORT, 0)]);
        assert_eq!(r, vec![Contact::Up { id: 1 }]);
        // new contact in slot 0 at the same position: the kernel sends no position, the old one is kept
        let r = ev(&mut m, &[(EV_ABS, ABS_MT_TRACKING_ID, 9), (EV_SYN, SYN_REPORT, 0)]);
        assert_eq!(r, vec![Contact::Down { id: 3, x: 110, y: 200, major: 0 }]);
    }

    #[test]
    fn tracking_id_change_without_lift() {
        let mut m = Mt::new(2);
        ev(&mut m, &[(EV_ABS, ABS_MT_TRACKING_ID, 1), (EV_SYN, SYN_REPORT, 0)]);
        let r = ev(&mut m, &[(EV_ABS, ABS_MT_TRACKING_ID, 2), (EV_SYN, SYN_REPORT, 0)]);
        assert_eq!(r, vec![Contact::Up { id: 1 }, Contact::Down { id: 2, x: 0, y: 0, major: 0 }]);
    }

    #[test]
    fn syn_dropped_lifts_and_ignores_until_lift() {
        let mut m = Mt::new(2);
        ev(&mut m, &[(EV_ABS, ABS_MT_TRACKING_ID, 1), (EV_SYN, SYN_REPORT, 0)]);
        let r = ev(&mut m, &[(EV_SYN, SYN_DROPPED, 0), (EV_ABS, ABS_MT_POSITION_X, 9), (EV_SYN, SYN_REPORT, 0)]);
        assert_eq!(r, vec![Contact::Up { id: 1 }]);
        // the old contact moves: ignored
        assert!(ev(&mut m, &[(EV_ABS, ABS_MT_POSITION_X, 10), (EV_SYN, SYN_REPORT, 0)]).is_empty());
        ev(&mut m, &[(EV_ABS, ABS_MT_TRACKING_ID, -1), (EV_SYN, SYN_REPORT, 0)]);
        let r = ev(&mut m, &[(EV_ABS, ABS_MT_TRACKING_ID, 5), (EV_SYN, SYN_REPORT, 0)]);
        assert!(matches!(r[0], Contact::Down { .. }));
    }

    #[test]
    fn sync_full_keeps_positions_for_deduplicated_events() {
        // M1 T10 root cause: daemon B started after the replay device's slot 0 last touched (5000, 9000). The next
        // press at the same spot only carries a tracking id (the kernel drops equal ABS values), and slot 1 was the
        // current slot (no ABS_MT_SLOT event either when it does not change).
        let mut m = Mt::new(3);
        m.sync_full(&[-1, -1, -1], &[5000, 7, 0], &[9000, 8, 0], &[20, 0, 0], 1);
        let r = ev(&mut m, &[(EV_ABS, ABS_MT_TRACKING_ID, 33), (EV_SYN, SYN_REPORT, 0)]);
        assert_eq!(r, vec![Contact::Down { id: 1, x: 7, y: 8, major: 0 }]);
        let r = ev(&mut m, &[(EV_ABS, ABS_MT_SLOT, 0), (EV_ABS, ABS_MT_TRACKING_ID, 34), (EV_SYN, SYN_REPORT, 0)]);
        assert_eq!(r, vec![Contact::Down { id: 2, x: 5000, y: 9000, major: 20 }]);
        // the old behaviour (fresh Mt, no sync) put that contact at (0, 0)
        let mut old = Mt::new(3);
        let r = ev(&mut old, &[(EV_ABS, ABS_MT_TRACKING_ID, 34), (EV_SYN, SYN_REPORT, 0)]);
        assert_eq!(r, vec![Contact::Down { id: 1, x: 0, y: 0, major: 0 }]);
    }

    #[test]
    fn sync_tids_ignores_contacts_already_down() {
        let mut m = Mt::new(3);
        m.sync_tids(&[-1, 44, -1]);
        assert_eq!(m.active(), 1);
        assert!(ev(&mut m, &[(EV_ABS, ABS_MT_SLOT, 1), (EV_ABS, ABS_MT_POSITION_X, 1), (EV_SYN, SYN_REPORT, 0)]).is_empty());
    }
}
