// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! Quick actions (M2): a configurable bar of action buttons (its own region, movable / resizable in edit mode).
//! Built-in actions are key chords sent through the virtual keyboard (so the desktop's shortcuts apply; the defaults
//! follow KDE Plasma), media / volume keys, or commands (argv lists, run as the user, never through a shell).
//! The user picks which actions show, their order and size (`quick_actions.actions = ["copy", "paste:2", ...]`) and
//! adds custom ones (`[[quick_actions.custom]]` with `keys = "LEFTCTRL+LEFTSHIFT+T"` or `command = ["app", "arg"]`).

use crate::config::{CustomAction, QuickCfg};
use crate::keycodes;

#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    /// evdev codes, pressed in order, released in reverse
    Chord(Vec<u16>),
    /// argv (no shell)
    Command(Vec<String>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub id: String,
    pub label: String,
    /// 1 or 2 (relative width)
    pub size: u32,
    pub act: Act,
}

/// (id, label, group, chord or command); `group` is for the settings sheet. Commands are only offered when their
/// program exists (`needs` = the program).
pub const BUILTIN: &[(&str, &str, &str, &str)] = &[
    ("copy", "Copy", "edit", "LEFTCTRL+C"),
    ("paste", "Paste", "edit", "LEFTCTRL+V"),
    ("cut", "Cut", "edit", "LEFTCTRL+X"),
    ("undo", "Undo", "edit", "LEFTCTRL+Z"),
    ("redo", "Redo", "edit", "LEFTCTRL+LEFTSHIFT+Z"),
    ("select_all", "Select all", "edit", "LEFTCTRL+A"),
    ("find", "Find", "edit", "LEFTCTRL+F"),
    ("save", "Save", "edit", "LEFTCTRL+S"),
    ("alt_tab", "Alt+Tab", "window", "LEFTALT+TAB"),
    ("show_desktop", "Desktop", "window", "LEFTMETA+D"),
    ("overview", "Overview", "window", "LEFTMETA+W"),
    ("close_window", "Close", "window", "LEFTALT+F4"),
    ("other_screen", "To screen", "window", "LEFTMETA+LEFTSHIFT+RIGHT"),
    ("fullscreen", "Fullscreen", "window", "F11"),
    ("screenshot", "Screenshot", "system", "SYSRQ"),
    ("vol_down", "Vol -", "system", "VOLUMEDOWN"),
    ("vol_up", "Vol +", "system", "VOLUMEUP"),
    ("mute", "Mute", "system", "MUTE"),
    ("play_pause", "Play", "media", "PLAYPAUSE"),
    ("next", "Next", "media", "NEXTSONG"),
    ("prev", "Prev", "media", "PREVIOUSSONG"),
    ("mon_bright_up", "Mon +", "system", "cmd:ddcutil setvcp 10 + 10"),
    ("mon_bright_down", "Mon -", "system", "cmd:ddcutil setvcp 10 - 10"),
    ("emoji", "Emoji", "system", "cmd:plasma-emojier"),
];

/// "LEFTCTRL+LEFTSHIFT+T" -> codes (None if any name is unknown)
pub fn parse_chord(s: &str) -> Option<Vec<u16>> {
    let v: Option<Vec<u16>> = s.split('+').map(|k| keycodes::code(k.trim())).collect();
    v.filter(|v| !v.is_empty())
}

/// the key events of a chord: press in order, release in reverse
pub fn chord_events(codes: &[u16]) -> Vec<(u16, bool)> {
    let mut v: Vec<(u16, bool)> = codes.iter().map(|&c| (c, true)).collect();
    v.extend(codes.iter().rev().map(|&c| (c, false)));
    v
}

fn builtin(id: &str) -> Option<(String, Act)> {
    let (_, label, _, spec) = BUILTIN.iter().find(|b| b.0 == id)?;
    let act = match spec.strip_prefix("cmd:") {
        Some(c) => Act::Command(c.split_whitespace().map(|s| s.to_string()).collect()),
        None => Act::Chord(parse_chord(spec)?),
    };
    Some((label.to_string(), act))
}

fn custom(c: &CustomAction) -> Option<(String, Act)> {
    let label = if c.label.is_empty() { c.id.clone() } else { c.label.clone() };
    if !c.command.is_empty() {
        return Some((label, Act::Command(c.command.clone())));
    }
    Some((label, Act::Chord(parse_chord(&c.keys)?)))
}

/// The items to show, in order. `have_program` says whether a command's program exists (unavailable commands are
/// skipped, e.g. monitor brightness without ddcutil). Returns the items and notes (unknown ids etc.).
pub fn resolve(cfg: &QuickCfg, have_program: &dyn Fn(&str) -> bool) -> (Vec<Item>, Vec<String>) {
    let mut out = vec![];
    let mut notes = vec![];
    if !cfg.enabled {
        return (out, notes);
    }
    for e in &cfg.actions {
        let (id, size) = match e.split_once(':') {
            Some((i, s)) => (i, s.parse::<u32>().unwrap_or(1).clamp(1, 2)),
            None => (e.as_str(), 1),
        };
        let found = builtin(id).or_else(|| cfg.custom.iter().find(|c| c.id == id).and_then(custom));
        match found {
            Some((label, act)) => {
                if let Act::Command(argv) = &act {
                    if argv.is_empty() || !have_program(&argv[0]) {
                        notes.push(format!("quick action {} skipped: {} not installed", id, argv.first().map(|s| s.as_str()).unwrap_or("?")));
                        continue;
                    }
                }
                out.push(Item { id: id.to_string(), label, size, act });
            }
            None => notes.push(format!("quick action {:?} unknown (or a broken custom chord)", id)),
        }
    }
    (out, notes)
}

/// the size of `id` in the list (0 = not shown)
pub fn size_of(cfg: &QuickCfg, id: &str) -> u32 {
    cfg.actions
        .iter()
        .find_map(|e| {
            let (i, s) = e.split_once(':').unwrap_or((e.as_str(), "1"));
            (i == id).then(|| s.parse::<u32>().unwrap_or(1).clamp(1, 2))
        })
        .unwrap_or(0)
}

/// settings sheet step for one action: off -> 1 -> 2 -> off (appended at the end when turned on)
pub fn cycle(actions: &[String], id: &str) -> Vec<String> {
    let mut v: Vec<String> = actions.to_vec();
    match v.iter().position(|e| e.split(':').next() == Some(id)) {
        None => v.push(id.to_string()),
        Some(i) => {
            if v[i].ends_with(":2") {
                v.remove(i);
            } else {
                v[i] = format!("{}:2", id);
            }
        }
    }
    v
}

/// move an action one place earlier
pub fn move_up(actions: &[String], id: &str) -> Vec<String> {
    let mut v: Vec<String> = actions.to_vec();
    if let Some(i) = v.iter().position(|e| e.split(':').next() == Some(id)) {
        if i > 0 {
            v.swap(i, i - 1);
        }
    }
    v
}

/// all ids for the sheet: built-ins then custom ones
pub fn all_ids(cfg: &QuickCfg) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = BUILTIN.iter().map(|b| (b.0.to_string(), b.1.to_string())).collect();
    v.extend(cfg.custom.iter().map(|c| (c.id.clone(), if c.label.is_empty() { c.id.clone() } else { c.label.clone() })));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_resolve_and_chords() {
        for b in BUILTIN {
            assert!(builtin(b.0).is_some(), "{} does not parse", b.0);
        }
        let mut q = QuickCfg::default();
        q.actions = vec!["copy".into(), "paste:2".into(), "nope".into(), "mon_bright_up".into(), "mine".into(), "run".into()];
        q.custom = vec![
            CustomAction { id: "mine".into(), label: "New tab".into(), keys: "LEFTCTRL+LEFTSHIFT+T".into(), command: vec![] },
            CustomAction { id: "run".into(), label: "".into(), keys: "".into(), command: vec!["konsole".into(), "--new-tab".into()] },
        ];
        let (items, notes) = resolve(&q, &|p| p == "konsole");
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["copy", "paste", "mine", "run"]);
        assert_eq!(items[1].size, 2);
        assert_eq!(items[0].act, Act::Chord(vec![29, 46]));
        assert_eq!(items[2].act, Act::Chord(vec![29, 42, 20]));
        assert_eq!(items[3].act, Act::Command(vec!["konsole".into(), "--new-tab".into()]), "argv list, no shell");
        assert_eq!(notes.len(), 2, "{:?}", notes);
        assert_eq!(chord_events(&[29, 42, 20]), vec![(29, true), (42, true), (20, true), (20, false), (42, false), (29, false)]);
        q.enabled = false;
        assert!(resolve(&q, &|_| true).0.is_empty());
    }

    #[test]
    fn sheet_edits() {
        let a: Vec<String> = vec!["copy".into(), "paste".into()];
        let a = cycle(&a, "paste");
        assert_eq!(a, vec!["copy", "paste:2"]);
        let a = cycle(&a, "paste");
        assert_eq!(a, vec!["copy"]);
        let a = cycle(&a, "undo");
        assert_eq!(a, vec!["copy", "undo"]);
        assert_eq!(move_up(&a, "undo"), vec!["undo", "copy"]);
        let q = QuickCfg { actions: vec!["undo:2".into()], ..Default::default() };
        assert_eq!(size_of(&q, "undo"), 2);
        assert_eq!(size_of(&q, "copy"), 0);
        assert!(all_ids(&QuickCfg::default()).len() >= 24);
    }
}
