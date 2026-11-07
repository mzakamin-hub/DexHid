// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! On-screen key state machine. Behaviour (reimplemented from the timing described in Denial's INPUT_DECK.md, no
//! code copied): a tap commits on release; a stationary hold of `hold_ms` (150 ms) presses the key and keeps it
//! down so the desktop's own key repeat runs; modifiers press at once; while a modifier is held, other keys press at
//! once (chords); sliding off a key before it committed cancels it, sliding off a pressed key releases it.
//! Action keys (page switch, layout cycle) fire on release.
//!
//! M2: combo keys (one key = a chord, e.g. Shift+2), long-press alternates (a key with `long` sends its alternate
//! after `long_ms` instead of repeating), sticky modifiers (tap a modifier: it stays down for the next key; tap it
//! twice: locked until tapped again), the Fn layer (hold Fn, or tap it to lock the layer; see keycodes::fn_map).
//!
//! M2.1: the hold must be STATIONARY: a pending key whose contact moves more than `slop` (px) from its anchor
//! restarts its hold timer from there (it can still commit a tap on release); leaving the key by more than the slop
//! (the router decides `on_key` with the key rect grown by the slop) cancels it. A held modifier that is cancelled
//! (slid off, a whole-screen swipe, any release_all) without having been used with another key is let go behind a
//! neutral "mask" key (`mask`, default KEY_UNKNOWN in the router): compositors treat a modifier that saw another key
//! as a chord, so its lone-tap action (Super opening the launcher) does not run.

use crate::keycodes;
use crate::layout::{Action, KeyKind};
use std::collections::{BTreeMap, HashMap, HashSet};

pub type ContactId = u32;

#[derive(Debug, Clone, PartialEq)]
pub enum KeyOut {
    /// evdev code, pressed
    Key(u16, bool),
    Action(Action),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum St {
    Pending,
    Pressed,
    Cancelled,
    /// the long-press alternate fired: the release does nothing
    Fired,
}

#[derive(Debug, Clone)]
struct Kc {
    key: usize,
    kind: KeyKind,
    long: Option<KeyKind>,
    start: i64,
    st: St,
    /// the codes pressed for this contact (after the Fn mapping), released in reverse
    sent: Vec<u16>,
    /// modifier / Fn contacts: another key was used while this one was down
    used_other: bool,
    /// where the stationary hold is measured from (px)
    ax: f32,
    ay: f32,
}

#[derive(Debug, Default)]
pub struct KeyMachine {
    pub hold_ms: i64,
    pub long_ms: i64,
    pub sticky: bool,
    /// M2.1: movement (px) that restarts a pending hold (the router also grows the key rect by it for slide-off)
    pub slop: f32,
    /// M2.1: neutral key sent between a cancelled lone modifier's press and release (None = off)
    pub mask: Option<u16>,
    contacts: HashMap<ContactId, Kc>,
    held: HashMap<u16, u32>,
    /// held modifiers that saw another key (a chord): their release needs no mask
    mod_used: HashSet<u16>,
    /// sticky modifiers kept down: code -> locked
    latched: BTreeMap<u16, bool>,
    fn_hold: u32,
    fn_locked: bool,
}

impl KeyMachine {
    pub fn new(hold_ms: i64) -> KeyMachine {
        KeyMachine { hold_ms, long_ms: 450, ..Default::default() }
    }

    fn press(&mut self, code: u16, out: &mut Vec<KeyOut>) {
        let n = self.held.entry(code).or_insert(0);
        *n += 1;
        if *n == 1 {
            out.push(KeyOut::Key(code, true));
        }
    }

    fn release(&mut self, code: u16, out: &mut Vec<KeyOut>) {
        if let Some(n) = self.held.get_mut(&code) {
            *n -= 1;
            if *n == 0 {
                self.held.remove(&code);
                self.mod_used.remove(&code);
                out.push(KeyOut::Key(code, false));
            }
        }
    }

    /// the mask key tap (only when a held modifier is about to go without having been used)
    fn mask_if_lone(&mut self, mods: &[u16], out: &mut Vec<KeyOut>) {
        let Some(m) = self.mask else { return };
        if mods.iter().any(|c| keycodes::is_modifier(*c) && self.held.get(c) == Some(&1) && !self.mod_used.contains(c)) {
            out.push(KeyOut::Key(m, true));
            out.push(KeyOut::Key(m, false));
            for c in mods {
                self.mod_used.insert(*c);
            }
        }
    }

    fn modifier_held(&self) -> bool {
        self.held.keys().any(|&c| keycodes::is_modifier(c))
    }

    pub fn fn_active(&self) -> bool {
        self.fn_hold > 0 || self.fn_locked
    }

    /// sticky modifiers currently latched (code, locked)
    pub fn latched(&self) -> Vec<(u16, bool)> {
        self.latched.iter().map(|(&c, &l)| (c, l)).collect()
    }

    fn codes_of(&self, kind: &KeyKind) -> Vec<u16> {
        match kind {
            KeyKind::Code(c) if self.fn_active() => vec![keycodes::fn_map(*c)],
            KeyKind::Code(c) => vec![*c],
            KeyKind::Combo(v) => v.clone(),
            _ => vec![],
        }
    }

    fn mark_used(&mut self) {
        let mods: Vec<u16> = self.held.keys().copied().filter(|&c| keycodes::is_modifier(c)).collect();
        self.mod_used.extend(mods);
        for c in self.contacts.values_mut() {
            if matches!(c.kind, KeyKind::Code(k) if keycodes::is_modifier(k)) || c.kind == KeyKind::Action(Action::Fn) {
                c.used_other = true;
            }
        }
    }

    /// after a non-modifier key committed: let go of the sticky modifiers that are not locked
    fn release_latched(&mut self, out: &mut Vec<KeyOut>) {
        let free: Vec<u16> = self.latched.iter().filter(|(_, &l)| !l).map(|(&c, _)| c).collect();
        for c in free {
            // a latched modifier still physically held by a finger stays down
            if self.contacts.values().any(|k| k.kind == KeyKind::Code(c)) {
                continue;
            }
            self.latched.remove(&c);
            self.release(c, out);
        }
    }

    /// a contact starts on key `key` at `at` (px)
    pub fn down(&mut self, id: ContactId, key: usize, kind: &KeyKind, long: Option<&KeyKind>, t: i64, at: (f32, f32)) -> Vec<KeyOut> {
        let mut out = vec![];
        let mut kc = Kc { key, kind: kind.clone(), long: long.cloned(), start: t, st: St::Pending, sent: vec![], used_other: false, ax: at.0, ay: at.1 };
        match kind {
            KeyKind::Code(c) if keycodes::is_modifier(*c) => {
                if !self.latched.contains_key(c) {
                    self.press(*c, &mut out);
                    kc.sent = vec![*c];
                }
                kc.st = St::Pressed;
            }
            KeyKind::Action(Action::Fn) => {
                self.fn_hold += 1;
                kc.st = St::Pressed;
            }
            KeyKind::Code(_) | KeyKind::Combo(_) if self.modifier_held() && long.is_none() => {
                for c in self.codes_of(kind) {
                    self.press(c, &mut out);
                    kc.sent.push(c);
                }
                kc.st = St::Pressed;
                self.mark_used();
            }
            KeyKind::Gap => kc.st = St::Cancelled,
            _ => {}
        }
        self.contacts.insert(id, kc);
        out
    }

    /// the contact moved to `at`; `on_key` = still inside its key grown by the slop (false = slid off)
    pub fn motion(&mut self, id: ContactId, on_key: bool, at: (f32, f32), t: i64) -> Vec<KeyOut> {
        let mut out = vec![];
        let Some(c) = self.contacts.get(&id).cloned() else { return out };
        if matches!(c.st, St::Cancelled | St::Fired) || c.kind == KeyKind::Action(Action::Fn) {
            return out;
        }
        if on_key {
            // a pending key must be held still: moving beyond the slop restarts its hold from here
            if c.st == St::Pending && ((at.0 - c.ax).powi(2) + (at.1 - c.ay).powi(2)).sqrt() > self.slop {
                let k = self.contacts.get_mut(&id).unwrap();
                k.start = t;
                k.ax = at.0;
                k.ay = at.1;
            }
            return out;
        }
        if c.st == St::Pressed {
            if !c.used_other {
                self.mask_if_lone(&c.sent, &mut out);
            }
            for code in c.sent.iter().rev() {
                self.release(*code, &mut out);
            }
        }
        let k = self.contacts.get_mut(&id).unwrap();
        k.st = St::Cancelled;
        k.sent.clear();
        out
    }

    pub fn up(&mut self, id: ContactId) -> Vec<KeyOut> {
        let mut out = vec![];
        let Some(c) = self.contacts.remove(&id) else { return out };
        match (c.st, &c.kind) {
            (St::Pending, KeyKind::Code(_) | KeyKind::Combo(_)) => {
                let codes = self.codes_of(&c.kind);
                for &k in &codes {
                    self.press(k, &mut out);
                }
                for &k in codes.iter().rev() {
                    self.release(k, &mut out);
                }
                self.mark_used();
                self.release_latched(&mut out);
            }
            (St::Pending, KeyKind::Action(a)) => out.push(KeyOut::Action(a.clone())),
            (St::Pressed, KeyKind::Action(Action::Fn)) => {
                self.fn_hold = self.fn_hold.saturating_sub(1);
                if !c.used_other {
                    self.fn_locked = !self.fn_locked;
                }
            }
            (St::Pressed, KeyKind::Code(m)) if keycodes::is_modifier(*m) => {
                let m = *m;
                match (self.sticky && !c.used_other, self.latched.get(&m).copied()) {
                    // first tap: latch (stays down for the next key)
                    (true, None) => {
                        self.latched.insert(m, false);
                    }
                    // second tap: lock
                    (true, Some(false)) => {
                        self.latched.insert(m, true);
                    }
                    // tap on a locked one (or used in a chord): let go
                    (_, Some(_)) => {
                        self.latched.remove(&m);
                        self.release(m, &mut out);
                    }
                    (false, None) => {
                        for code in c.sent.iter().rev() {
                            self.release(*code, &mut out);
                        }
                    }
                }
            }
            (St::Pressed, _) => {
                for code in c.sent.iter().rev() {
                    self.release(*code, &mut out);
                }
                self.release_latched(&mut out);
            }
            _ => {}
        }
        out
    }

    pub fn tick(&mut self, t: i64) -> Vec<KeyOut> {
        let mut out = vec![];
        let due: Vec<ContactId> = self
            .contacts
            .iter()
            .filter(|(_, c)| c.st == St::Pending && matches!(c.kind, KeyKind::Code(_) | KeyKind::Combo(_)))
            .filter(|(_, c)| t - c.start >= if c.long.is_some() { self.long_ms } else { self.hold_ms })
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            let c = self.contacts[&id].clone();
            if let Some(alt) = &c.long {
                // long press: tap the alternate once
                let codes = self.codes_of(alt);
                for &k in &codes {
                    self.press(k, &mut out);
                }
                for &k in codes.iter().rev() {
                    self.release(k, &mut out);
                }
                if let KeyKind::Action(a) = alt {
                    out.push(KeyOut::Action(a.clone()));
                }
                self.contacts.get_mut(&id).unwrap().st = St::Fired;
            } else {
                let codes = self.codes_of(&c.kind);
                for &k in &codes {
                    self.press(k, &mut out);
                }
                let kc = self.contacts.get_mut(&id).unwrap();
                kc.st = St::Pressed;
                kc.sent = codes;
            }
            self.mark_used();
        }
        out
    }

    pub fn next_deadline(&self) -> Option<i64> {
        self.contacts
            .values()
            .filter(|c| c.st == St::Pending && matches!(c.kind, KeyKind::Code(_) | KeyKind::Combo(_)))
            .map(|c| c.start + if c.long.is_some() { self.long_ms } else { self.hold_ms })
            .min()
    }

    /// lift everything (release order: before an ungrab / device destroy, a swipe, a layout change). Ordinary keys
    /// go first; a modifier that was never used with another key gets the mask key before its release (a cancel
    /// must not run the modifier's lone-tap action).
    pub fn release_all(&mut self) -> Vec<KeyOut> {
        self.contacts.clear();
        self.latched.clear();
        self.fn_hold = 0;
        self.fn_locked = false;
        let mut codes: Vec<u16> = self.held.keys().copied().collect();
        codes.sort_unstable();
        let (mods, keys): (Vec<u16>, Vec<u16>) = codes.into_iter().partition(|&c| keycodes::is_modifier(c));
        let mut out: Vec<KeyOut> = keys.into_iter().map(|c| KeyOut::Key(c, false)).collect();
        if let Some(m) = self.mask {
            if mods.iter().any(|c| !self.mod_used.contains(c)) {
                out.push(KeyOut::Key(m, true));
                out.push(KeyOut::Key(m, false));
            }
        }
        out.extend(mods.into_iter().map(|c| KeyOut::Key(c, false)));
        self.held.clear();
        self.mod_used.clear();
        out
    }

    pub fn held_codes(&self) -> Vec<u16> {
        let mut v: Vec<u16> = self.held.keys().copied().collect();
        v.sort_unstable();
        v
    }

    pub fn shift_held(&self) -> bool {
        self.held.keys().any(|&c| keycodes::is_shift(c))
    }

    /// keys to draw as pressed (pending or held, not cancelled)
    pub fn lit_keys(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self.contacts.values().filter(|c| !matches!(c.st, St::Cancelled | St::Fired)).map(|c| c.key).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// the key (index) a contact started on
    pub fn key_of(&self, id: ContactId) -> Option<usize> {
        self.contacts.get(&id).map(|c| c.key)
    }

    pub fn has_contact(&self, id: ContactId) -> bool {
        self.contacts.contains_key(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: u16 = 30;
    const SHIFT: u16 = 42;
    const CTRL: u16 = 29;
    const K1: u16 = 2;

    fn code(c: u16) -> KeyKind {
        KeyKind::Code(c)
    }
    fn k(v: Vec<KeyOut>) -> Vec<(u16, bool)> {
        v.into_iter().filter_map(|o| if let KeyOut::Key(c, p) = o { Some((c, p)) } else { None }).collect()
    }

    #[test]
    fn tap_commits_on_release() {
        let mut m = KeyMachine::new(150);
        assert!(m.down(1, 0, &code(A), None, 0, (0.0, 0.0)).is_empty());
        assert!(m.tick(100).is_empty());
        assert_eq!(m.up(1), vec![KeyOut::Key(A, true), KeyOut::Key(A, false)]);
        assert!(m.held_codes().is_empty());
    }

    #[test]
    fn hold_presses_once_and_releases() {
        let mut m = KeyMachine::new(150);
        m.down(1, 0, &code(A), None, 0, (0.0, 0.0));
        assert_eq!(m.next_deadline(), Some(150));
        assert_eq!(m.tick(150), vec![KeyOut::Key(A, true)]);
        assert!(m.tick(600).is_empty(), "no own repeat: the desktop repeats");
        assert_eq!(m.up(1), vec![KeyOut::Key(A, false)]);
    }

    #[test]
    fn modifier_chord() {
        let mut m = KeyMachine::new(150);
        assert_eq!(m.down(1, 5, &code(SHIFT), None, 0, (0.0, 0.0)), vec![KeyOut::Key(SHIFT, true)]);
        assert!(m.shift_held());
        // with a modifier down, the next key presses at once
        assert_eq!(m.down(2, 0, &code(A), None, 50, (0.0, 0.0)), vec![KeyOut::Key(A, true)]);
        assert_eq!(m.up(2), vec![KeyOut::Key(A, false)]);
        assert_eq!(m.up(1), vec![KeyOut::Key(SHIFT, false)]);
    }

    #[test]
    fn slide_off_cancels_or_releases() {
        let mut m = KeyMachine::new(150);
        m.down(1, 0, &code(A), None, 0, (0.0, 0.0));
        assert!(m.motion(1, true, (1.0, 0.0), 10).is_empty());
        assert!(m.motion(1, false, (90.0, 0.0), 20).is_empty());
        assert!(m.up(1).is_empty(), "cancelled: nothing typed");
        m.down(2, 0, &code(A), None, 1000, (0.0, 0.0));
        m.tick(1200);
        assert_eq!(m.motion(2, false, (90.0, 0.0), 1210), vec![KeyOut::Key(A, false)]);
        assert!(m.up(2).is_empty());
        assert!(m.held_codes().is_empty());
    }

    /// M2.1: the 150 ms hold needs a stationary finger; a finger moving inside the key restarts the timer,
    /// and still commits a tap on release
    #[test]
    fn hold_needs_a_stationary_finger() {
        let mut m = KeyMachine::new(150);
        m.slop = 10.0;
        m.down(1, 0, &code(A), None, 0, (100.0, 100.0));
        assert!(m.motion(1, true, (105.0, 100.0), 60).is_empty(), "within the slop: timer keeps running");
        assert_eq!(m.next_deadline(), Some(150));
        m.motion(1, true, (118.0, 100.0), 100);
        assert_eq!(m.next_deadline(), Some(250), "moved beyond the slop: the hold restarts there");
        assert!(m.tick(200).is_empty());
        assert_eq!(m.tick(250), vec![KeyOut::Key(A, true)], "then held still 150 ms: real press");
        assert_eq!(m.up(1), vec![KeyOut::Key(A, false)]);
        m.down(2, 0, &code(A), None, 1000, (100.0, 100.0));
        m.motion(2, true, (120.0, 100.0), 1100);
        assert_eq!(m.up(2), vec![KeyOut::Key(A, true), KeyOut::Key(A, false)], "a moving finger that stays on the key taps");
    }

    /// M2.1: a cancelled lone modifier (Super) is released behind the mask key; a used one is not
    #[test]
    fn cancelled_lone_modifier_is_masked() {
        const META: u16 = 125;
        const MASK: u16 = 240;
        let mut m = KeyMachine::new(150);
        m.mask = Some(MASK);
        assert_eq!(m.down(1, 7, &code(META), None, 0, (0.0, 0.0)), vec![KeyOut::Key(META, true)]);
        assert_eq!(k(m.motion(1, false, (99.0, 0.0), 50)), vec![(MASK, true), (MASK, false), (META, false)], "slid off: masked");
        m.up(1);
        // cancelled by release_all (a swipe): masked too, ordinary keys first
        m.down(2, 7, &code(META), None, 100, (0.0, 0.0));
        assert_eq!(k(m.release_all()), vec![(MASK, true), (MASK, false), (META, false)]);
        // Super + A used as a chord, then cancelled: no mask needed
        m.down(3, 7, &code(META), None, 200, (0.0, 0.0));
        m.down(4, 0, &code(A), None, 210, (0.0, 0.0));
        assert_eq!(k(m.release_all()), vec![(A, false), (META, false)]);
        // an ordinary release of a lone Super is the user's own tap: no mask (the launcher may open, as wanted)
        m.down(5, 7, &code(META), None, 300, (0.0, 0.0));
        assert_eq!(k(m.up(5)), vec![(META, false)]);
        // mask off
        m.mask = None;
        m.down(6, 7, &code(META), None, 400, (0.0, 0.0));
        assert_eq!(k(m.release_all()), vec![(META, false)]);
    }

    #[test]
    fn same_key_two_fingers_refcount() {
        let mut m = KeyMachine::new(150);
        m.down(1, 0, &code(SHIFT), None, 0, (0.0, 0.0));
        m.down(2, 9, &code(SHIFT), None, 0, (0.0, 0.0));
        assert_eq!(m.up(1), vec![]);
        assert_eq!(m.up(2), vec![KeyOut::Key(SHIFT, false)]);
    }

    #[test]
    fn actions_fire_on_release_and_release_all() {
        let mut m = KeyMachine::new(150);
        m.down(1, 3, &KeyKind::Action(Action::Page("sym".into())), None, 0, (0.0, 0.0));
        assert!(m.tick(1000).is_empty());
        assert_eq!(m.up(1), vec![KeyOut::Action(Action::Page("sym".into()))]);
        m.down(2, 0, &code(SHIFT), None, 0, (0.0, 0.0));
        m.down(3, 1, &code(A), None, 0, (0.0, 0.0));
        let mut r = m.release_all();
        r.sort_by_key(|k| format!("{:?}", k));
        assert_eq!(r.len(), 2);
        assert!(m.held_codes().is_empty() && m.lit_keys().is_empty());
    }

    #[test]
    fn combo_key_taps_and_holds_its_chord() {
        let mut m = KeyMachine::new(150);
        let at = KeyKind::Combo(vec![SHIFT, 3]);
        m.down(1, 0, &at, None, 0, (0.0, 0.0));
        assert_eq!(k(m.up(1)), vec![(SHIFT, true), (3, true), (3, false), (SHIFT, false)]);
        m.down(2, 0, &at, None, 100, (0.0, 0.0));
        assert_eq!(k(m.tick(250)), vec![(SHIFT, true), (3, true)]);
        assert_eq!(k(m.up(2)), vec![(3, false), (SHIFT, false)]);
    }

    #[test]
    fn long_press_alternate() {
        let mut m = KeyMachine::new(150);
        m.long_ms = 450;
        let q = code(16);
        m.down(1, 0, &q, Some(&code(K1)), 0, (0.0, 0.0));
        assert!(m.tick(200).is_empty(), "no repeat on keys with an alternate");
        assert_eq!(m.next_deadline(), Some(450));
        assert_eq!(k(m.tick(450)), vec![(K1, true), (K1, false)]);
        assert!(m.up(1).is_empty(), "the release does nothing after the alternate");
        m.down(2, 0, &q, Some(&code(K1)), 1000, (0.0, 0.0));
        assert_eq!(k(m.up(2)), vec![(16, true), (16, false)], "short tap = the key itself");
    }

    #[test]
    fn sticky_modifiers_latch_lock_release() {
        let mut m = KeyMachine::new(150);
        m.sticky = true;
        // tap Shift: stays down
        assert_eq!(k(m.down(1, 5, &code(SHIFT), None, 0, (0.0, 0.0))), vec![(SHIFT, true)]);
        assert!(m.up(1).is_empty());
        assert_eq!(m.latched(), vec![(SHIFT, false)]);
        // next key presses at once (chord), its release lets go of Shift
        assert_eq!(k(m.down(2, 0, &code(A), None, 100, (0.0, 0.0))), vec![(A, true)]);
        assert_eq!(k(m.up(2)), vec![(A, false), (SHIFT, false)]);
        assert!(m.latched().is_empty());
        // double tap Ctrl: locked for several keys
        m.down(3, 6, &code(CTRL), None, 200, (0.0, 0.0));
        m.up(3);
        m.down(4, 6, &code(CTRL), None, 300, (0.0, 0.0));
        assert!(m.up(4).is_empty());
        assert_eq!(m.latched(), vec![(CTRL, true)]);
        m.down(5, 0, &code(A), None, 400, (0.0, 0.0));
        assert_eq!(k(m.up(5)), vec![(A, false)]);
        assert!(m.held_codes().contains(&CTRL));
        // third tap unlocks
        m.down(6, 6, &code(CTRL), None, 500, (0.0, 0.0));
        assert_eq!(k(m.up(6)), vec![(CTRL, false)]);
        // holding a modifier while typing is a normal chord (no latch)
        m.down(7, 5, &code(SHIFT), None, 600, (0.0, 0.0));
        m.down(8, 0, &code(A), None, 610, (0.0, 0.0));
        m.up(8);
        assert_eq!(k(m.up(7)), vec![(SHIFT, false)]);
        assert!(m.latched().is_empty());
    }

    #[test]
    fn fn_layer_hold_and_lock() {
        let mut m = KeyMachine::new(150);
        let f = KeyKind::Action(Action::Fn);
        m.down(1, 9, &f, None, 0, (0.0, 0.0));
        assert!(m.fn_active());
        m.down(2, 0, &code(K1), None, 10, (0.0, 0.0));
        assert_eq!(k(m.up(2)), vec![(59, true), (59, false)], "Fn+1 = F1");
        m.up(1);
        assert!(!m.fn_active(), "used as a modifier: not locked");
        // tap Fn alone: locked
        m.down(3, 9, &f, None, 100, (0.0, 0.0));
        m.up(3);
        assert!(m.fn_active());
        m.down(4, 0, &code(103), None, 200, (0.0, 0.0));
        assert_eq!(k(m.up(4)), vec![(104, true), (104, false)], "Up -> PgUp");
        m.down(5, 9, &f, None, 300, (0.0, 0.0));
        m.up(5);
        assert!(!m.fn_active());
    }
}
