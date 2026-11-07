// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! Emergency release chord (default Volume Up + Volume Down held 2 s), from passively read key devices.

use std::collections::HashSet;

#[derive(Debug, Default)]
pub struct Chord {
    pub keys: Vec<u16>,
    pub hold_ms: i64,
    down: HashSet<(usize, u16)>,
    since: Option<i64>,
    fired: bool,
}

impl Chord {
    pub fn new(keys: Vec<u16>, hold_ms: i64) -> Chord {
        Chord { keys, hold_ms, ..Default::default() }
    }

    /// a key event from device `dev` (value 2 = autorepeat, ignored)
    pub fn key(&mut self, dev: usize, code: u16, value: i32, t: i64) {
        if value == 2 || !self.keys.contains(&code) {
            return;
        }
        if value == 1 {
            self.down.insert((dev, code));
        } else {
            self.down.remove(&(dev, code));
        }
        let all = !self.keys.is_empty() && self.keys.iter().all(|k| self.down.iter().any(|(_, c)| c == k));
        if all {
            if self.since.is_none() {
                self.since = Some(t);
            }
        } else {
            self.since = None;
            self.fired = false;
        }
    }

    /// true once when the chord has been held long enough
    pub fn poll(&mut self, t: i64) -> bool {
        match self.since {
            Some(s) if !self.fired && t - s >= self.hold_ms => {
                self.fired = true;
                true
            }
            _ => false,
        }
    }

    pub fn deadline(&self) -> Option<i64> {
        if self.fired {
            None
        } else {
            self.since.map(|s| s + self.hold_ms)
        }
    }

    pub fn held_since(&self) -> Option<i64> {
        self.since
    }

    /// forget the key state of one device (it went away)
    pub fn forget_dev(&mut self, dev: usize) {
        self.down.retain(|(d, _)| *d != dev);
        self.since = None;
        self.fired = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const UP: u16 = 115;
    const DN: u16 = 114;

    #[test]
    fn same_schedule_as_m0() {
        let mut c = Chord::new(vec![UP, DN], 2000);
        // first key alone 2.2 s: no release
        c.key(0, UP, 1, 500);
        assert!(!c.poll(2700));
        c.key(0, UP, 0, 2700);
        // both 1.0 s: no release
        c.key(0, UP, 1, 3000);
        c.key(1, DN, 1, 3000);
        assert!(!c.poll(3999));
        c.key(0, UP, 0, 4000);
        c.key(1, DN, 0, 4000);
        assert!(!c.poll(4100));
        // both held: fires at +2000 ms, once
        c.key(0, UP, 1, 4500);
        c.key(1, DN, 1, 4500);
        c.key(1, DN, 2, 4600);
        assert_eq!(c.deadline(), Some(6500));
        assert!(!c.poll(6499));
        assert!(c.poll(6500));
        assert!(!c.poll(9000));
        c.key(0, UP, 0, 9100);
        c.key(0, UP, 1, 9200);
        assert!(c.poll(11200), "fires again after a fresh hold");
    }

    #[test]
    fn device_loss_resets() {
        let mut c = Chord::new(vec![UP, DN], 2000);
        c.key(0, UP, 1, 0);
        c.key(1, DN, 1, 0);
        c.forget_dev(1);
        assert!(!c.poll(5000));
    }
}
