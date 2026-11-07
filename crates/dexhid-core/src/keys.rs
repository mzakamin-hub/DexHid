// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! On-screen key state machine. Behaviour (reimplemented from the timing described in Denial's INPUT_DECK.md, no
//! code copied): a tap commits on release; a stationary hold of `hold_ms` (150 ms) presses the key and keeps it
//! down so the desktop's own key repeat runs; modifiers press at once; while a modifier is held, other keys press at
//! once (chords); sliding off a key before it committed cancels it, sliding off a pressed key releases it.
//! Action keys (page switch, layout cycle) fire on release.

use crate::keycodes;
use crate::layout::{Action, KeyKind};
use std::collections::HashMap;

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
}

#[derive(Debug, Clone)]
struct Kc {
    key: usize,
    kind: KeyKind,
    start: i64,
    st: St,
}

#[derive(Debug, Default)]
pub struct KeyMachine {
    pub hold_ms: i64,
    contacts: HashMap<ContactId, Kc>,
    held: HashMap<u16, u32>,
}

impl KeyMachine {
    pub fn new(hold_ms: i64) -> KeyMachine {
        KeyMachine { hold_ms, ..Default::default() }
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
                out.push(KeyOut::Key(code, false));
            }
        }
    }

    fn modifier_held(&self) -> bool {
        self.held.keys().any(|&c| keycodes::is_modifier(c))
    }

    pub fn down(&mut self, id: ContactId, key: usize, kind: &KeyKind, t: i64) -> Vec<KeyOut> {
        let mut out = vec![];
        let st = match kind {
            KeyKind::Code(c) if keycodes::is_modifier(*c) || self.modifier_held() => {
                self.press(*c, &mut out);
                St::Pressed
            }
            KeyKind::Gap => St::Cancelled,
            _ => St::Pending,
        };
        self.contacts.insert(id, Kc { key, kind: kind.clone(), start: t, st });
        out
    }

    /// the contact moved; `key_under` is the key now under it (None = no key)
    pub fn motion(&mut self, id: ContactId, key_under: Option<usize>) -> Vec<KeyOut> {
        let mut out = vec![];
        let Some(c) = self.contacts.get(&id).cloned() else { return out };
        if key_under == Some(c.key) || c.st == St::Cancelled {
            return out;
        }
        if c.st == St::Pressed {
            if let KeyKind::Code(code) = c.kind {
                self.release(code, &mut out);
            }
        }
        self.contacts.get_mut(&id).unwrap().st = St::Cancelled;
        out
    }

    pub fn up(&mut self, id: ContactId) -> Vec<KeyOut> {
        let mut out = vec![];
        let Some(c) = self.contacts.remove(&id) else { return out };
        match (c.st, &c.kind) {
            (St::Pending, KeyKind::Code(code)) => {
                self.press(*code, &mut out);
                self.release(*code, &mut out);
            }
            (St::Pending, KeyKind::Action(a)) => out.push(KeyOut::Action(a.clone())),
            (St::Pressed, KeyKind::Code(code)) => self.release(*code, &mut out),
            _ => {}
        }
        out
    }

    pub fn tick(&mut self, t: i64) -> Vec<KeyOut> {
        let mut out = vec![];
        let due: Vec<(ContactId, u16)> = self
            .contacts
            .iter()
            .filter(|(_, c)| c.st == St::Pending && t - c.start >= self.hold_ms)
            .filter_map(|(id, c)| if let KeyKind::Code(code) = c.kind { Some((*id, code)) } else { None })
            .collect();
        for (id, code) in due {
            self.press(code, &mut out);
            self.contacts.get_mut(&id).unwrap().st = St::Pressed;
        }
        out
    }

    pub fn next_deadline(&self) -> Option<i64> {
        self.contacts
            .values()
            .filter(|c| c.st == St::Pending && matches!(c.kind, KeyKind::Code(_)))
            .map(|c| c.start + self.hold_ms)
            .min()
    }

    /// lift everything (release order: before an ungrab / device destroy)
    pub fn release_all(&mut self) -> Vec<KeyOut> {
        self.contacts.clear();
        let mut codes: Vec<u16> = self.held.keys().copied().collect();
        codes.sort_unstable();
        self.held.clear();
        codes.into_iter().map(|c| KeyOut::Key(c, false)).collect()
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
        let mut v: Vec<usize> = self.contacts.values().filter(|c| c.st != St::Cancelled).map(|c| c.key).collect();
        v.sort_unstable();
        v.dedup();
        v
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

    fn code(c: u16) -> KeyKind {
        KeyKind::Code(c)
    }

    #[test]
    fn tap_commits_on_release() {
        let mut m = KeyMachine::new(150);
        assert!(m.down(1, 0, &code(A), 0).is_empty());
        assert!(m.tick(100).is_empty());
        assert_eq!(m.up(1), vec![KeyOut::Key(A, true), KeyOut::Key(A, false)]);
        assert!(m.held_codes().is_empty());
    }

    #[test]
    fn hold_presses_once_and_releases() {
        let mut m = KeyMachine::new(150);
        m.down(1, 0, &code(A), 0);
        assert_eq!(m.next_deadline(), Some(150));
        assert_eq!(m.tick(150), vec![KeyOut::Key(A, true)]);
        assert!(m.tick(600).is_empty(), "no own repeat: the desktop repeats");
        assert_eq!(m.up(1), vec![KeyOut::Key(A, false)]);
    }

    #[test]
    fn modifier_chord() {
        let mut m = KeyMachine::new(150);
        assert_eq!(m.down(1, 5, &code(SHIFT), 0), vec![KeyOut::Key(SHIFT, true)]);
        assert!(m.shift_held());
        // with a modifier down, the next key presses at once
        assert_eq!(m.down(2, 0, &code(A), 50), vec![KeyOut::Key(A, true)]);
        assert_eq!(m.up(2), vec![KeyOut::Key(A, false)]);
        assert_eq!(m.up(1), vec![KeyOut::Key(SHIFT, false)]);
    }

    #[test]
    fn slide_off_cancels_or_releases() {
        let mut m = KeyMachine::new(150);
        m.down(1, 0, &code(A), 0);
        assert!(m.motion(1, Some(0)).is_empty());
        assert!(m.motion(1, Some(1)).is_empty());
        assert!(m.up(1).is_empty(), "cancelled: nothing typed");
        m.down(2, 0, &code(A), 1000);
        m.tick(1200);
        assert_eq!(m.motion(2, None), vec![KeyOut::Key(A, false)]);
        assert!(m.up(2).is_empty());
        assert!(m.held_codes().is_empty());
    }

    #[test]
    fn same_key_two_fingers_refcount() {
        let mut m = KeyMachine::new(150);
        m.down(1, 0, &code(SHIFT), 0);
        m.down(2, 9, &code(SHIFT), 0);
        assert_eq!(m.up(1), vec![]);
        assert_eq!(m.up(2), vec![KeyOut::Key(SHIFT, false)]);
    }

    #[test]
    fn actions_fire_on_release_and_release_all() {
        let mut m = KeyMachine::new(150);
        m.down(1, 3, &KeyKind::Action(Action::Page("sym".into())), 0);
        assert!(m.tick(1000).is_empty());
        assert_eq!(m.up(1), vec![KeyOut::Action(Action::Page("sym".into()))]);
        m.down(2, 0, &code(SHIFT), 0);
        m.down(3, 1, &code(A), 0);
        let mut r = m.release_all();
        r.sort_by_key(|k| format!("{:?}", k));
        assert_eq!(r.len(), 2);
        assert!(m.held_codes().is_empty() && m.lit_keys().is_empty());
    }
}
