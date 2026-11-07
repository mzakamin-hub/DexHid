// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! M3 small feature logic (pure, unit-tested):
//! - text editing: the cursor joystick (displacement -> arrow key + repeat interval) and select-while-moving;
//! - presentation: the talk timer;
//! - keyboard extras: the local word list + the word being typed + suggestions; the emoji panel's sets;
//! - clipboard history (text, optionally images) for the strip;
//! - typing text through a keymap (char -> keycode + level), used to paste / accept words on HID targets;
//! - mirror preview: the preview rect and a nearest-neighbour downscale of BGRx frames.

use crate::geom::Rect;
use std::collections::{HashMap, VecDeque};

// ------------------------------------------------------------------------------------------- text editing

pub const KEY_UP: u16 = 103;
pub const KEY_DOWN: u16 = 108;
pub const KEY_LEFT: u16 = 105;
pub const KEY_RIGHT: u16 = 106;
pub const KEY_HOME: u16 = 102;
pub const KEY_END: u16 = 107;
pub const KEY_PAGEUP: u16 = 104;
pub const KEY_PAGEDOWN: u16 = 109;

/// keys that move the caret (select-while-moving adds Shift to them)
pub fn is_nav(code: u16) -> bool {
    matches!(code, KEY_UP | KEY_DOWN | KEY_LEFT | KEY_RIGHT | KEY_HOME | KEY_END | KEY_PAGEUP | KEY_PAGEDOWN)
}

/// joystick: the arrow key for a displacement from the touch-down point (mm, y down) and its repeat interval;
/// None inside the dead zone. The dominant axis wins; further out = faster (400 ms at the dead zone edge down to
/// 40 ms 15 mm further).
pub fn joystick(dx: f32, dy: f32, dead_mm: f32) -> Option<(u16, i64)> {
    let d = (dx * dx + dy * dy).sqrt();
    if d < dead_mm {
        return None;
    }
    let key = if dx.abs() >= dy.abs() {
        if dx < 0.0 { KEY_LEFT } else { KEY_RIGHT }
    } else if dy < 0.0 {
        KEY_UP
    } else {
        KEY_DOWN
    };
    let iv = (400.0 - (d - dead_mm) * 24.0).clamp(40.0, 400.0) as i64;
    Some((key, iv))
}

// ------------------------------------------------------------------------------------------- presentation timer

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Timer {
    pub total_ms: i64,
    started: Option<i64>,
    acc_ms: i64,
}

impl Timer {
    pub fn new(minutes: i64) -> Timer {
        Timer { total_ms: minutes * 60_000, started: None, acc_ms: 0 }
    }
    pub fn running(&self) -> bool {
        self.started.is_some()
    }
    pub fn start(&mut self, now: i64) {
        if self.started.is_none() {
            self.started = Some(now);
        }
    }
    pub fn pause(&mut self, now: i64) {
        if let Some(s) = self.started.take() {
            self.acc_ms += now - s;
        }
    }
    pub fn toggle(&mut self, now: i64) {
        if self.running() { self.pause(now) } else { self.start(now) }
    }
    pub fn reset(&mut self) {
        self.started = None;
        self.acc_ms = 0;
    }
    pub fn elapsed(&self, now: i64) -> i64 {
        self.acc_ms + self.started.map(|s| now - s).unwrap_or(0)
    }
    /// "12:34 left", "+0:42 over" (count-down) or "3:05" (no total: stopwatch); "" before the first start
    pub fn text(&self, now: i64) -> String {
        let e = self.elapsed(now);
        if e == 0 && !self.running() {
            return String::new();
        }
        let f = |ms: i64| format!("{}:{:02}", ms / 60_000, (ms / 1000) % 60);
        let paused = if self.running() { "" } else { " (paused)" };
        if self.total_ms <= 0 {
            return format!("{}{}", f(e), paused);
        }
        let left = self.total_ms - e;
        if left >= 0 {
            format!("{} left{}", f(left + 999), paused)
        } else {
            format!("+{} over{}", f(-left), paused)
        }
    }
}

// ------------------------------------------------------------------------------------------- word suggestions

/// a small built-in English list (most frequent words first); the user's own words come from words.txt
pub const WORDS: &str = "the be to of and a in that have it for not on with he as you do at this but his by from they we say her she or an \
will my one all would there their what so up out if about who get which go me when make can like time no just him know take people into \
year your good some could them see other than then now look only come its over think also back after use two how our work first well way \
even new want because any these give day most us is was are been has had were said did made found thing tell very through still \
between something should never same another while last might great before must home around long little world under school right \
house again place help small number off always next point keep start hand without different following example important question \
during until government system program problem public information company group available possible already together \
together tomorrow yesterday today thanks thank please hello meeting message monday tuesday wednesday thursday friday weekend \
sorry maybe really actually probably definitely everything everyone somebody anything nothing because however although \
should would could about above across against along among around before behind below beneath beside between beyond";

#[derive(Clone, Debug, Default)]
pub struct WordList {
    words: Vec<String>,
}

impl WordList {
    pub fn builtin() -> WordList {
        let mut seen = std::collections::HashSet::new();
        WordList { words: WORDS.split_whitespace().filter(|w| seen.insert(*w)).map(|w| w.to_string()).collect() }
    }
    /// user words (one per line, '#' comments) go first: they are the user's own vocabulary
    pub fn with_user(mut self, text: &str) -> WordList {
        let mut u: Vec<String> = text.lines().map(|l| l.trim()).filter(|l| !l.is_empty() && !l.starts_with('#')).map(|l| l.to_string()).collect();
        u.extend(self.words.drain(..).filter(|w| !text.lines().any(|l| l.trim() == w)));
        self.words = u;
        self
    }
    pub fn len(&self) -> usize {
        self.words.len()
    }
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }
    /// up to n completions of a prefix (case-insensitive; the case of the typed prefix is kept), the prefix itself
    /// excluded
    pub fn suggest(&self, prefix: &str, n: usize) -> Vec<String> {
        if prefix.chars().count() < 2 {
            return vec![];
        }
        let lp = prefix.to_lowercase();
        let upper_first = prefix.chars().next().is_some_and(|c| c.is_uppercase());
        self.words
            .iter()
            .filter(|w| w.to_lowercase().starts_with(&lp) && w.to_lowercase() != lp)
            .take(n)
            .map(|w| {
                if upper_first {
                    let mut c = w.chars();
                    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
                } else {
                    w.clone()
                }
            })
            .collect()
    }
}

/// the word being typed, from the characters the panel's keys produce; anything that may move the caret (arrows,
/// a pad touch, a click, Enter, punctuation) ends it
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WordTracker {
    pub word: String,
}

impl WordTracker {
    /// a typed character (None = a key without text: ends the word)
    pub fn typed(&mut self, ch: Option<char>) {
        match ch {
            Some(c) if c.is_alphabetic() || c == '\'' => self.word.push(c),
            _ => self.word.clear(),
        }
    }
    pub fn backspace(&mut self) {
        self.word.pop();
    }
    pub fn reset(&mut self) {
        self.word.clear();
    }
    /// accepting `word`: the characters to delete and the text to type (with a trailing space)
    pub fn accept(&mut self, word: &str) -> (usize, String) {
        let del = self.word.chars().count();
        self.word.clear();
        (del, format!("{} ", word))
    }
}

// ------------------------------------------------------------------------------------------- emoji

/// the emoji panel: (category, emoji list)
pub const EMOJI: &[(&str, &str)] = &[
    ("Smileys", "😀 😃 😄 😁 😆 😅 😂 🙂 😉 😊 😇 😍 😘 😋 😜 🤔 🤨 😐 😑 😶 🙄 😏 😴 😎 🤓 😕 😟 😮 😲 😳 😢 😭 😤 😡 🥳 🤗"),
    ("Gestures", "👍 👎 👌 ✌ 🤞 🤟 🤘 👏 🙌 👐 🤝 🙏 💪 👋 ☝ 👆 👇 👉 👈 ✋ 🤚 🖐 🖖 ✍"),
    ("Hearts", "❤ 🧡 💛 💚 💙 💜 🖤 🤍 💔 ❣ 💕 💞 💓 💗 💖 💘 💝 ⭐ 🌟 ✨ 🔥 💯 🎉 🎊"),
    ("Objects", "☕ 🍕 🍺 🍷 🎂 🍎 📱 💻 ⌨ 🖥 📷 🎵 🎧 📚 ✏ 📌 📎 🔑 🔒 💡 ⏰ 📅 ✉ 🚀"),
    ("Symbols", "✅ ❌ ❓ ❗ ⚠ ➡ ⬅ ⬆ ⬇ ↩ 🔁 ➕ ➖ ✖ ➗ ♻ ✔ ☑ ⭕ 🆗 🆕 ©️ ®️ ™️"),
];

pub fn emoji_page(i: usize) -> Vec<String> {
    EMOJI.get(i).map(|e| e.1.split_whitespace().map(|s| s.to_string()).collect()).unwrap_or_default()
}

// ------------------------------------------------------------------------------------------- clipboard history

#[derive(Clone, Debug, PartialEq)]
pub enum ClipData {
    Text(String),
    /// mime type, bytes
    Image(String, Vec<u8>),
}

#[derive(Clone, Debug, Default)]
pub struct ClipHistory {
    pub items: VecDeque<ClipData>,
    pub cap: usize,
}

/// the largest item kept (text or image), so the history stays small; it lives in memory only, never on disk
pub const CLIP_MAX_BYTES: usize = 8 << 20;

impl ClipHistory {
    pub fn new(cap: usize) -> ClipHistory {
        ClipHistory { items: VecDeque::new(), cap: cap.clamp(1, 20) }
    }
    /// newest first; an equal item moves to the front; empty / whitespace-only text and huge items are not kept
    pub fn push(&mut self, d: ClipData) -> bool {
        match &d {
            ClipData::Text(t) if t.trim().is_empty() || t.len() > CLIP_MAX_BYTES => return false,
            ClipData::Image(_, b) if b.is_empty() || b.len() > CLIP_MAX_BYTES => return false,
            _ => {}
        }
        if self.items.front() == Some(&d) {
            return false;
        }
        self.items.retain(|x| x != &d);
        self.items.push_front(d);
        self.items.truncate(self.cap);
        true
    }
    pub fn set_cap(&mut self, cap: usize) {
        self.cap = cap.clamp(1, 20);
        self.items.truncate(self.cap);
    }
    /// one-line labels for the strip
    pub fn labels(&self, max_chars: usize) -> Vec<String> {
        self.items.iter().map(|d| label(d, max_chars)).collect()
    }
}

pub fn label(d: &ClipData, max_chars: usize) -> String {
    match d {
        ClipData::Text(t) => {
            let one: String = t.split_whitespace().collect::<Vec<_>>().join(" ");
            if one.chars().count() > max_chars {
                one.chars().take(max_chars.saturating_sub(1)).collect::<String>() + "\u{2026}"
            } else {
                one
            }
        }
        ClipData::Image(m, b) => format!("{} {} KB", m.trim_start_matches("image/").to_uppercase(), b.len().div_ceil(1024)),
    }
}

// ------------------------------------------------------------------------------------------- typing text

/// (keycode, level 0..3) for each character of a keymap's legends (level 1 = Shift, 2 = AltGr, 3 = both);
/// the lowest level wins
pub fn reverse_map(labels: &HashMap<u16, [String; 4]>) -> HashMap<char, (u16, u8)> {
    let mut m: HashMap<char, (u16, u8)> = HashMap::new();
    let mut codes: Vec<&u16> = labels.keys().collect();
    codes.sort();
    for c in codes {
        for (lvl, s) in labels[c].iter().enumerate() {
            let mut it = s.chars();
            if let (Some(ch), None) = (it.next(), it.next()) {
                let e = m.entry(ch).or_insert((*c, lvl as u8));
                if (lvl as u8) < e.1 {
                    *e = (*c, lvl as u8);
                }
            }
        }
    }
    m.entry(' ').or_insert((57, 0));
    m.entry('\n').or_insert((28, 0));
    m.entry('\t').or_insert((15, 0));
    m
}

/// key events that type `text` (press / release, Shift = 42, AltGr = 100 around a level 1-3 key); characters
/// the keymap cannot type are returned separately
pub fn type_text(text: &str, rev: &HashMap<char, (u16, u8)>) -> (Vec<(u16, bool)>, Vec<char>) {
    let mut ev = vec![];
    let mut missing = vec![];
    for ch in text.chars() {
        let Some(&(code, lvl)) = rev.get(&ch) else {
            if !missing.contains(&ch) {
                missing.push(ch);
            }
            continue;
        };
        let mods: Vec<u16> = match lvl {
            1 => vec![42],
            2 => vec![100],
            3 => vec![42, 100],
            _ => vec![],
        };
        for &m in &mods {
            ev.push((m, true));
        }
        ev.push((code, true));
        ev.push((code, false));
        for &m in mods.iter().rev() {
            ev.push((m, false));
        }
    }
    (ev, missing)
}

// ------------------------------------------------------------------------------------------- mirror preview

/// the preview rect inside the touchpad region: top-right corner, `pct` % of the pad width, the source aspect
pub fn mirror_rect(pad: &Rect, src_w: u32, src_h: u32, pct: f32, margin: f32) -> Rect {
    let w = (pad.w * pct / 100.0).max(20.0).min(pad.w - 2.0 * margin);
    let aspect = if src_w > 0 && src_h > 0 { src_h as f32 / src_w as f32 } else { 9.0 / 16.0 };
    let h = (w * aspect).min(pad.h - 2.0 * margin);
    let w = if h < w * aspect { h / aspect } else { w };
    Rect::new(pad.right() - margin - w, pad.y + margin, w, h)
}

/// nearest-neighbour downscale of a BGRx / xRGB 32-bit frame (stride in bytes) to dw x dh pixels (0xffRRGGBB)
pub fn downscale(src: &[u8], w: u32, h: u32, stride: u32, dw: u32, dh: u32) -> Vec<u32> {
    let mut out = vec![0xff00_0000u32; (dw * dh) as usize];
    if w == 0 || h == 0 || dw == 0 || dh == 0 {
        return out;
    }
    for y in 0..dh {
        let sy = (y as u64 * h as u64 / dh as u64) as u32;
        for x in 0..dw {
            let sx = (x as u64 * w as u64 / dw as u64) as u32;
            let i = (sy * stride + sx * 4) as usize;
            if i + 3 < src.len() {
                let (b, g, r) = (src[i] as u32, src[i + 1] as u32, src[i + 2] as u32);
                out[(y * dw + x) as usize] = 0xff00_0000 | r << 16 | g << 8 | b;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joystick_and_nav() {
        assert_eq!(joystick(1.0, 1.0, 3.0), None);
        assert_eq!(joystick(4.0, 1.0, 3.0), Some((KEY_RIGHT, 373)));
        assert_eq!(joystick(-30.0, 2.0, 3.0), Some((KEY_LEFT, 40)), "far out = fastest");
        assert_eq!(joystick(1.0, -8.0, 3.0).map(|j| j.0), Some(KEY_UP));
        assert_eq!(joystick(0.0, 5.0, 3.0).map(|j| j.0), Some(KEY_DOWN));
        assert!(is_nav(KEY_HOME) && !is_nav(30));
    }

    #[test]
    fn timer() {
        let mut t = Timer::new(10);
        assert_eq!(t.text(0), "");
        t.start(1000);
        assert_eq!(t.text(1000), "10:00 left");
        assert_eq!(t.text(1000 + 61_500), "8:59 left");
        t.pause(1000 + 61_500);
        assert_eq!(t.text(999_999), "8:59 left (paused)");
        t.toggle(2_000_000);
        assert_eq!(t.text(2_000_000 + 540_000), "+0:01 over");
        t.reset();
        assert_eq!(t.text(5), "");
        let mut s = Timer::new(0);
        s.start(0);
        assert_eq!(s.text(185_000), "3:05", "stopwatch without a total");
    }

    #[test]
    fn words() {
        let w = WordList::builtin();
        assert!(w.len() > 150);
        assert_eq!(w.suggest("t", 3), Vec::<String>::new(), "two letters first");
        let s = w.suggest("th", 3);
        assert_eq!(s, vec!["the", "that", "this"]);
        assert_eq!(w.suggest("Th", 2), vec!["The", "That"], "case of the typed prefix");
        assert!(w.suggest("the", 5).iter().all(|x| x != "the"), "the prefix itself is not offered");
        let u = WordList::builtin().with_user("# mine\nthermostat\n");
        assert_eq!(u.suggest("th", 1), vec!["thermostat"], "user words first");
        let mut tr = WordTracker::default();
        for c in "hel".chars() {
            tr.typed(Some(c));
        }
        tr.typed(Some('x'));
        tr.backspace();
        assert_eq!(tr.word, "hel");
        assert_eq!(tr.accept("hello"), (3, "hello ".to_string()));
        assert_eq!(tr.word, "");
        tr.typed(Some('a'));
        tr.typed(None);
        assert_eq!(tr.word, "", "a key without text ends the word");
        tr.typed(Some('a'));
        tr.typed(Some('.'));
        assert_eq!(tr.word, "");
    }

    #[test]
    fn emoji_and_clipboard() {
        assert_eq!(emoji_page(0).len(), 36);
        assert!(EMOJI.iter().all(|e| !e.1.is_empty()));
        assert!(emoji_page(99).is_empty());
        let mut h = ClipHistory::new(3);
        assert!(h.push(ClipData::Text("one".into())));
        assert!(!h.push(ClipData::Text("one".into())), "same as the newest");
        assert!(!h.push(ClipData::Text("  \n".into())));
        h.push(ClipData::Text("two\nlines  here".into()));
        h.push(ClipData::Image("image/png".into(), vec![0; 3000]));
        h.push(ClipData::Text("one".into()));
        assert_eq!(h.labels(8), vec!["one", "PNG 3 KB", "two lin\u{2026}"]);
        h.set_cap(2);
        assert_eq!(h.items.len(), 2);
        assert!(!h.push(ClipData::Image("image/png".into(), vec![])));
    }

    #[test]
    fn typing_through_a_keymap() {
        let mut l: HashMap<u16, [String; 4]> = HashMap::new();
        let s = |a: &str, b: &str, c: &str, d: &str| [a.to_string(), b.to_string(), c.to_string(), d.to_string()];
        l.insert(30, s("a", "A", "æ", "Æ"));
        l.insert(16, s("q", "Q", "@", "Ω"));
        l.insert(3, s("2", "\"", "²", ""));
        l.insert(44, s("y", "Y", "»", ""));
        let r = reverse_map(&l);
        let (ev, miss) = type_text("aA@ y\"", &r);
        assert_eq!(
            ev,
            vec![(30, true), (30, false), (42, true), (30, true), (30, false), (42, false), (100, true), (16, true), (16, false), (100, false), (57, true), (57, false), (44, true), (44, false), (42, true), (3, true), (3, false), (42, false)]
        );
        assert!(miss.is_empty());
        let (_, miss) = type_text("z😀z", &r);
        assert_eq!(miss, vec!['z', '😀']);
    }

    #[test]
    fn mirror() {
        let pad = Rect::new(100.0, 200.0, 800.0, 400.0);
        let r = mirror_rect(&pad, 1920, 1080, 30.0, 10.0);
        assert_eq!((r.w, (r.h * 100.0).round() / 100.0), (240.0, 135.0));
        assert_eq!(r.right(), 890.0);
        assert_eq!(r.y, 210.0);
        // a 2x2 BGRx frame (stride 8) to 4x2
        let src = [1, 2, 3, 0, 10, 20, 30, 0, 4, 5, 6, 0, 40, 50, 60, 0];
        let d = downscale(&src, 2, 2, 8, 4, 2);
        assert_eq!(d, vec![0xff030201, 0xff030201, 0xff1e140a, 0xff1e140a, 0xff060504, 0xff060504, 0xff3c3228, 0xff3c3228]);
    }
}
