// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! M2.1: whole-screen 3/4-finger swipe arbitration (behaviour reimplemented from Denial's INPUT_DECK.md, no code
//! copied). Three or four contacts anywhere on the input screen (keys, strip, quick bar, pad) form a candidate; it
//! is recognised only when EVERY participant has moved at least `min_px` from where it joined and all movements
//! point the same way (each within `max_angle_deg` of their mean direction). A resting chord, a stationary modifier
//! with one moving pad finger, and one / two finger input never qualify. A fourth contact joins a candidate before
//! recognition; after recognition the gesture keeps its finger count and later contacts are only captured.
//! Pure logic: the router does the take-over (release keys / buttons, synthetic touchpad fingers).

use crate::keys::ContactId;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SwipeCfg {
    pub enabled: bool,
    /// minimum travel of every participant (px)
    pub min_px: f32,
    pub max_angle_deg: f32,
}

impl Default for SwipeCfg {
    fn default() -> SwipeCfg {
        SwipeCfg { enabled: true, min_px: 50.0, max_angle_deg: 35.0 }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Recognised {
    pub ids: Vec<ContactId>,
    /// mean unit direction (logical px)
    pub dir: (f32, f32),
    /// mean travel so far (logical px)
    pub travel: (f32, f32),
}

#[derive(Clone, Debug, PartialEq, Eq, Copy)]
pub enum Phase {
    Idle,
    Candidate,
    /// recognised: the gesture owns the screen until every contact lifted
    Active,
}

#[derive(Debug)]
pub struct Swipe {
    pub cfg: SwipeCfg,
    phase: Phase,
    refs: Vec<(ContactId, f32, f32)>,
}

impl Swipe {
    pub fn new(cfg: SwipeCfg) -> Swipe {
        Swipe { cfg, phase: Phase::Idle, refs: vec![] }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// the eligible contacts changed (a down or an up); `live` = (id, x, y) of every eligible contact now
    pub fn contacts_changed(&mut self, live: &[(ContactId, f32, f32)]) {
        if self.phase == Phase::Active {
            return;
        }
        if !self.cfg.enabled || !(3..=4).contains(&live.len()) {
            self.phase = Phase::Idle;
            self.refs.clear();
            return;
        }
        // participants that are still down keep their reference, new ones join where they are
        self.refs.retain(|r| live.iter().any(|l| l.0 == r.0));
        for l in live {
            if !self.refs.iter().any(|r| r.0 == l.0) {
                self.refs.push(*l);
            }
        }
        self.phase = Phase::Candidate;
    }

    /// after a motion: the recognised gesture, once
    pub fn check(&mut self, pos: &HashMap<ContactId, (f32, f32)>) -> Option<Recognised> {
        if self.phase != Phase::Candidate {
            return None;
        }
        let mut moves = vec![];
        for (id, x0, y0) in &self.refs {
            let (x, y) = *pos.get(id)?;
            let (dx, dy) = (x - x0, y - y0);
            let d = (dx * dx + dy * dy).sqrt();
            if d < self.cfg.min_px {
                return None;
            }
            moves.push((dx, dy, d));
        }
        let n = moves.len() as f32;
        let (sx, sy) = moves.iter().fold((0.0, 0.0), |a, m| (a.0 + m.0 / m.2, a.1 + m.1 / m.2));
        let ml = (sx * sx + sy * sy).sqrt();
        if ml < 1e-3 {
            return None;
        }
        let dir = (sx / ml, sy / ml);
        let cmin = self.cfg.max_angle_deg.to_radians().cos();
        if moves.iter().any(|m| (m.0 / m.2) * dir.0 + (m.1 / m.2) * dir.1 < cmin) {
            return None;
        }
        let travel = moves.iter().fold((0.0, 0.0), |a, m| (a.0 + m.0 / n, a.1 + m.1 / n));
        self.phase = Phase::Active;
        Some(Recognised { ids: self.refs.iter().map(|r| r.0).collect(), dir, travel })
    }

    /// every contact lifted (or a release): back to idle
    pub fn reset(&mut self) {
        self.phase = Phase::Idle;
        self.refs.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(v: &[(u32, f32, f32)]) -> HashMap<u32, (f32, f32)> {
        v.iter().map(|p| (p.0, (p.1, p.2))).collect()
    }

    #[test]
    fn three_fingers_moving_together_are_recognised() {
        let mut s = Swipe::new(SwipeCfg::default());
        s.contacts_changed(&[(1, 100.0, 500.0), (2, 200.0, 500.0)]);
        assert_eq!(s.phase(), Phase::Idle, "two fingers: ordinary input");
        s.contacts_changed(&[(1, 100.0, 500.0), (2, 200.0, 500.0), (3, 300.0, 500.0)]);
        assert_eq!(s.phase(), Phase::Candidate);
        assert!(s.check(&pos(&[(1, 100.0, 470.0), (2, 200.0, 470.0), (3, 300.0, 470.0)])).is_none(), "not far enough");
        let r = s.check(&pos(&[(1, 103.0, 440.0), (2, 198.0, 445.0), (3, 300.0, 440.0)])).unwrap();
        assert_eq!(r.ids.len(), 3);
        assert!(r.dir.1 < -0.99, "up: {:?}", r.dir);
        assert!(s.check(&pos(&[(1, 0.0, 0.0)])).is_none(), "recognised once");
        s.contacts_changed(&[(1, 0.0, 0.0), (2, 0.0, 0.0), (3, 0.0, 0.0), (4, 0.0, 0.0)]);
        assert_eq!(s.phase(), Phase::Active, "a later contact does not change the gesture");
        s.reset();
        assert_eq!(s.phase(), Phase::Idle);
    }

    #[test]
    fn resting_chord_and_one_moving_finger_are_not_swipes() {
        let mut s = Swipe::new(SwipeCfg::default());
        s.contacts_changed(&[(1, 100.0, 500.0), (2, 200.0, 500.0), (3, 900.0, 900.0)]);
        // Ctrl + Alt resting, one pad finger moving far
        assert!(s.check(&pos(&[(1, 100.0, 500.0), (2, 201.0, 500.0), (3, 1200.0, 700.0)])).is_none());
        // fingers moving apart (a spread): not one direction
        assert!(s.check(&pos(&[(1, 30.0, 500.0), (2, 200.0, 560.0), (3, 990.0, 900.0)])).is_none());
    }

    #[test]
    fn fourth_finger_promotes_and_five_drops() {
        let mut s = Swipe::new(SwipeCfg::default());
        s.contacts_changed(&[(1, 100.0, 500.0), (2, 200.0, 500.0), (3, 300.0, 500.0)]);
        s.contacts_changed(&[(1, 100.0, 490.0), (2, 200.0, 490.0), (3, 300.0, 490.0), (4, 400.0, 500.0)]);
        let r = s.check(&pos(&[(1, 180.0, 500.0), (2, 280.0, 500.0), (3, 380.0, 500.0), (4, 480.0, 500.0)])).unwrap();
        assert_eq!(r.ids.len(), 4, "promoted to four fingers");
        assert!(r.dir.0 > 0.99);
        let mut s = Swipe::new(SwipeCfg::default());
        let five: Vec<(u32, f32, f32)> = (1..=5).map(|i| (i, i as f32 * 100.0, 500.0)).collect();
        s.contacts_changed(&five);
        assert_eq!(s.phase(), Phase::Idle, "five fingers are the release hold, not a swipe");
        let mut off = Swipe::new(SwipeCfg { enabled: false, ..Default::default() });
        off.contacts_changed(&five[..3]);
        assert_eq!(off.phase(), Phase::Idle);
    }
}
