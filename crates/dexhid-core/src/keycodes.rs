// SPDX-License-Identifier: GPL-3.0-or-later
//! evdev key codes (linux/input-event-codes.h) by name, with US fallback legends.
//! Layout files name keys without the `KEY_` prefix (`A`, `LEFTSHIFT`, `F1`, `KP7`, ...); `KEY_A` is accepted too.

/// (name, code, US legend, US shifted legend)
const TABLE: &[(&str, u16, &str, &str)] = &[
    ("ESC", 1, "Esc", ""),
    ("1", 2, "1", "!"),
    ("2", 3, "2", "@"),
    ("3", 4, "3", "#"),
    ("4", 5, "4", "$"),
    ("5", 6, "5", "%"),
    ("6", 7, "6", "^"),
    ("7", 8, "7", "&"),
    ("8", 9, "8", "*"),
    ("9", 10, "9", "("),
    ("0", 11, "0", ")"),
    ("MINUS", 12, "-", "_"),
    ("EQUAL", 13, "=", "+"),
    ("BACKSPACE", 14, "Bksp", ""),
    ("TAB", 15, "Tab", ""),
    ("Q", 16, "q", "Q"),
    ("W", 17, "w", "W"),
    ("E", 18, "e", "E"),
    ("R", 19, "r", "R"),
    ("T", 20, "t", "T"),
    ("Y", 21, "y", "Y"),
    ("U", 22, "u", "U"),
    ("I", 23, "i", "I"),
    ("O", 24, "o", "O"),
    ("P", 25, "p", "P"),
    ("LEFTBRACE", 26, "[", "{"),
    ("RIGHTBRACE", 27, "]", "}"),
    ("ENTER", 28, "Enter", ""),
    ("LEFTCTRL", 29, "Ctrl", ""),
    ("A", 30, "a", "A"),
    ("S", 31, "s", "S"),
    ("D", 32, "d", "D"),
    ("F", 33, "f", "F"),
    ("G", 34, "g", "G"),
    ("H", 35, "h", "H"),
    ("J", 36, "j", "J"),
    ("K", 37, "k", "K"),
    ("L", 38, "l", "L"),
    ("SEMICOLON", 39, ";", ":"),
    ("APOSTROPHE", 40, "'", "\""),
    ("GRAVE", 41, "`", "~"),
    ("LEFTSHIFT", 42, "Shift", ""),
    ("BACKSLASH", 43, "\\", "|"),
    ("Z", 44, "z", "Z"),
    ("X", 45, "x", "X"),
    ("C", 46, "c", "C"),
    ("V", 47, "v", "V"),
    ("B", 48, "b", "B"),
    ("N", 49, "n", "N"),
    ("M", 50, "m", "M"),
    ("COMMA", 51, ",", "<"),
    ("DOT", 52, ".", ">"),
    ("SLASH", 53, "/", "?"),
    ("RIGHTSHIFT", 54, "Shift", ""),
    ("KPASTERISK", 55, "*", ""),
    ("LEFTALT", 56, "Alt", ""),
    ("SPACE", 57, "", ""),
    ("CAPSLOCK", 58, "Caps", ""),
    ("F1", 59, "F1", ""),
    ("F2", 60, "F2", ""),
    ("F3", 61, "F3", ""),
    ("F4", 62, "F4", ""),
    ("F5", 63, "F5", ""),
    ("F6", 64, "F6", ""),
    ("F7", 65, "F7", ""),
    ("F8", 66, "F8", ""),
    ("F9", 67, "F9", ""),
    ("F10", 68, "F10", ""),
    ("NUMLOCK", 69, "Num", ""),
    ("SCROLLLOCK", 70, "ScrLk", ""),
    ("KP7", 71, "7", ""),
    ("KP8", 72, "8", ""),
    ("KP9", 73, "9", ""),
    ("KPMINUS", 74, "-", ""),
    ("KP4", 75, "4", ""),
    ("KP5", 76, "5", ""),
    ("KP6", 77, "6", ""),
    ("KPPLUS", 78, "+", ""),
    ("KP1", 79, "1", ""),
    ("KP2", 80, "2", ""),
    ("KP3", 81, "3", ""),
    ("KP0", 82, "0", ""),
    ("KPDOT", 83, ".", ""),
    ("102ND", 86, "<", ">"),
    ("F11", 87, "F11", ""),
    ("F12", 88, "F12", ""),
    ("KPENTER", 96, "Enter", ""),
    ("RIGHTCTRL", 97, "Ctrl", ""),
    ("KPSLASH", 98, "/", ""),
    ("SYSRQ", 99, "Print", ""),
    ("RIGHTALT", 100, "AltGr", ""),
    // M2.1: the neutral "mask" key for cancelled modifiers (xkeyboard-config maps <I248> to nothing)
    ("UNKNOWN", 240, "", ""),
    ("HOME", 102, "Home", ""),
    ("UP", 103, "\u{2191}", ""),
    ("PAGEUP", 104, "PgUp", ""),
    ("LEFT", 105, "\u{2190}", ""),
    ("RIGHT", 106, "\u{2192}", ""),
    ("END", 107, "End", ""),
    ("DOWN", 108, "\u{2193}", ""),
    ("PAGEDOWN", 109, "PgDn", ""),
    ("INSERT", 110, "Ins", ""),
    ("DELETE", 111, "Del", ""),
    ("MUTE", 113, "Mute", ""),
    ("VOLUMEDOWN", 114, "Vol-", ""),
    ("VOLUMEUP", 115, "Vol+", ""),
    ("POWER", 116, "Power", ""),
    ("KPEQUAL", 117, "=", ""),
    ("PAUSE", 119, "Pause", ""),
    ("LEFTMETA", 125, "Meta", ""),
    ("RIGHTMETA", 126, "Meta", ""),
    ("COMPOSE", 127, "Menu", ""),
    ("MENU", 139, "Menu", ""),
    ("NEXTSONG", 163, "Next", ""),
    ("PLAYPAUSE", 164, "Play", ""),
    ("PREVIOUSSONG", 165, "Prev", ""),
    ("F13", 183, "F13", ""),
    ("F14", 184, "F14", ""),
    ("F15", 185, "F15", ""),
    ("F16", 186, "F16", ""),
    ("F17", 187, "F17", ""),
    ("F18", 188, "F18", ""),
    ("F19", 189, "F19", ""),
    ("F20", 190, "F20", ""),
    ("F21", 191, "F21", ""),
    ("F22", 192, "F22", ""),
    ("F23", 193, "F23", ""),
    ("F24", 194, "F24", ""),
    ("BRIGHTNESSDOWN", 224, "Bri-", ""),
    ("BRIGHTNESSUP", 225, "Bri+", ""),
];

pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_RIGHTSHIFT: u16 = 54;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_RIGHTCTRL: u16 = 97;
pub const KEY_RIGHTALT: u16 = 100;
pub const KEY_LEFTMETA: u16 = 125;
pub const KEY_RIGHTMETA: u16 = 126;
pub const KEY_VOLUMEDOWN: u16 = 114;
pub const KEY_VOLUMEUP: u16 = 115;

pub fn code(name: &str) -> Option<u16> {
    let n = name.trim();
    let n = n.strip_prefix("KEY_").unwrap_or(n);
    let up = n.to_ascii_uppercase();
    if let Some(e) = TABLE.iter().find(|e| e.0 == up) {
        return Some(e.1);
    }
    // numeric codes are accepted as-is
    n.parse::<u16>().ok().filter(|&c| c > 0 && c < 0x2ff)
}

pub fn name(code: u16) -> String {
    match TABLE.iter().find(|e| e.1 == code) {
        Some(e) => format!("KEY_{}", e.0),
        None => format!("{}", code),
    }
}

pub fn us_legend(code: u16) -> (&'static str, &'static str) {
    TABLE.iter().find(|e| e.1 == code).map(|e| (e.2, e.3)).unwrap_or(("?", ""))
}

pub fn is_modifier(code: u16) -> bool {
    matches!(
        code,
        KEY_LEFTCTRL | KEY_LEFTSHIFT | KEY_RIGHTSHIFT | KEY_LEFTALT | KEY_RIGHTCTRL | KEY_RIGHTALT | KEY_LEFTMETA | KEY_RIGHTMETA
    )
}

pub fn is_shift(code: u16) -> bool {
    code == KEY_LEFTSHIFT || code == KEY_RIGHTSHIFT
}

/// keys whose legend follows the desktop's xkb layout (letters, digits, punctuation, the ISO key, space)
pub fn is_symbol_key(code: u16) -> bool {
    matches!(code, 2..=13 | 16..=27 | 30..=41 | 43..=53 | 86)
}

/// The Fn layer (M2): number row -> F1-F12, F-row -> media / volume / brightness, arrows -> Home / End / PgUp /
/// PgDn, Backspace -> Delete. Other keys stay themselves.
pub fn fn_map(code: u16) -> u16 {
    match code {
        2..=11 => code - 2 + 59,  // 1..0 -> F1..F10
        12 => 87,                 // - -> F11
        13 => 88,                 // = -> F12
        59 => 113,                // F1 -> Mute
        60 => 114,                // F2 -> Vol-
        61 => 115,                // F3 -> Vol+
        63 => 165,                // F5 -> Prev
        64 => 164,                // F6 -> Play
        65 => 163,                // F7 -> Next
        87 => 224,                // F11 -> Brightness-
        88 => 225,                // F12 -> Brightness+
        103 => 104,               // Up -> PgUp
        108 => 109,               // Down -> PgDn
        105 => 102,               // Left -> Home
        106 => 107,               // Right -> End
        14 => 111,                // Backspace -> Delete
        c => c,
    }
}

/// every code the uinput keyboard announces
pub fn all_codes() -> Vec<u16> {
    let mut v: Vec<u16> = TABLE.iter().map(|e| e.1).filter(|&c| c != 116).collect();
    v.sort_unstable();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names() {
        assert_eq!(code("A"), Some(30));
        assert_eq!(code("KEY_A"), Some(30));
        assert_eq!(code("leftshift"), Some(42));
        assert_eq!(code("F24"), Some(194));
        assert_eq!(code("NOPE"), None);
        assert_eq!(name(30), "KEY_A");
        assert!(is_modifier(code("RIGHTALT").unwrap()));
        assert!(!is_modifier(30));
        assert!(is_symbol_key(30) && !is_symbol_key(1));
        assert!(!all_codes().contains(&116), "never a power key on the virtual keyboard");
    }
}
