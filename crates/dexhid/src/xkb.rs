// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! Key legends from the desktop's xkb keymap (the one the compositor sends on wl_keyboard.keymap, which also
//! applies to our virtual keyboard). libxkbcommon is dlopen()ed: without it the legends stay US.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};

type CtxNew = unsafe extern "C" fn(c_int) -> *mut c_void;
type KeymapNewFromString = unsafe extern "C" fn(*mut c_void, *const c_char, c_int, c_int) -> *mut c_void;
type NumLayouts = unsafe extern "C" fn(*mut c_void) -> u32;
type LayoutName = unsafe extern "C" fn(*mut c_void, u32) -> *const c_char;
type SymsByLevel = unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut *const u32) -> c_int;
type ToUtf8 = unsafe extern "C" fn(u32, *mut c_char, usize) -> c_int;
type GetName = unsafe extern "C" fn(u32, *mut c_char, usize) -> c_int;
type Unref = unsafe extern "C" fn(*mut c_void);

pub struct Labels {
    /// evdev code -> legends of levels 1-4 (plain, Shift, AltGr = level 3, Shift+AltGr); "" = none
    pub map: HashMap<u16, [String; 4]>,
    pub layouts: Vec<String>,
    pub group: u32,
}

unsafe fn sym<T: Copy>(h: *mut c_void, name: &str) -> Option<T> {
    let c = CString::new(name).ok()?;
    let p = libc::dlsym(h, c.as_ptr());
    if p.is_null() {
        None
    } else {
        Some(std::mem::transmute_copy::<*mut c_void, T>(&p))
    }
}

fn dead_key(name: &str) -> Option<&'static str> {
    Some(match name {
        "dead_circumflex" => "^",
        "dead_acute" => "\u{b4}",
        "dead_grave" => "`",
        "dead_tilde" => "~",
        "dead_diaeresis" => "\u{a8}",
        "dead_cedilla" => "\u{b8}",
        "dead_abovering" => "\u{b0}",
        "dead_caron" => "\u{2c7}",
        _ => return None,
    })
}

/// the legend of a key for a modifier state (M2.1): Shift = level 2, AltGr = level 3, both = level 4; Caps Lock
/// inverts Shift on alphabetic keys (whose level 2 is the upper case of level 1). Missing levels fall back to the
/// lower ones (level 4 -> 3 -> 1, level 2 -> 1).
pub fn pick(l: &[String; 4], shift: bool, altgr: bool, caps: bool) -> String {
    let alpha = !l[0].is_empty() && l[1] != l[0] && l[1] == l[0].to_uppercase();
    let sh = if alpha { shift ^ caps } else { shift };
    let i = sh as usize + 2 * altgr as usize;
    let cand = [i, if altgr { 2 } else { 0 }, sh as usize, 0];
    for c in cand {
        if !l[c].is_empty() {
            return l[c].clone();
        }
    }
    String::new()
}

/// "de(nodeadkeys)" -> ("de", Some("nodeadkeys")); "us" -> ("us", None)
pub fn split_layout(s: &str) -> (String, Option<String>) {
    let s = s.trim();
    match (s.find('('), s.ends_with(')')) {
        (Some(i), true) if i > 0 => {
            let v = &s[i + 1..s.len() - 1];
            (s[..i].trim().to_string(), if v.trim().is_empty() { None } else { Some(v.trim().to_string()) })
        }
        _ => (s.to_string(), None),
    }
}

/// compile a keymap from RMLVO names: an xkb layout code with an optional variant, "us" or "de(nodeadkeys)"
/// (the USB target's legends; debug renders on the PC: `--xkb-layout de`)
pub fn keymap_from_layout(layout: &str) -> Result<String, String> {
    #[repr(C)]
    struct Names {
        rules: *const c_char,
        model: *const c_char,
        layout: *const c_char,
        variant: *const c_char,
        options: *const c_char,
    }
    type FromNames = unsafe extern "C" fn(*mut c_void, *const Names, c_int) -> *mut c_void;
    type AsString = unsafe extern "C" fn(*mut c_void, c_int) -> *mut c_char;
    unsafe {
        let c = CString::new("libxkbcommon.so.0").unwrap();
        let h = libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if h.is_null() {
            return Err("libxkbcommon not found".into());
        }
        let (Some(ctx_new), Some(from), Some(as_str)) = (sym::<CtxNew>(h, "xkb_context_new"), sym::<FromNames>(h, "xkb_keymap_new_from_names"), sym::<AsString>(h, "xkb_keymap_get_as_string")) else {
            return Err("libxkbcommon: missing symbols".into());
        };
        let ctx = ctx_new(0);
        let (lay, var) = split_layout(layout);
        let l = CString::new(lay).map_err(|_| "bad layout")?;
        let v = match var {
            Some(v) => Some(CString::new(v).map_err(|_| "bad variant")?),
            None => None,
        };
        let n = Names { rules: std::ptr::null(), model: std::ptr::null(), layout: l.as_ptr(), variant: v.as_ref().map(|v| v.as_ptr()).unwrap_or(std::ptr::null()), options: std::ptr::null() };
        let km = from(ctx, &n, 0);
        if km.is_null() {
            return Err(format!("no keymap for layout {}", layout));
        }
        let s = as_str(km, 1);
        if s.is_null() {
            return Err("keymap to string failed".into());
        }
        Ok(CStr::from_ptr(s).to_string_lossy().into_owned())
    }
}

/// parse a keymap string; `codes` are the evdev codes to label
pub fn labels(keymap: &str, group: u32, codes: &[u16]) -> Result<Labels, String> {
    unsafe {
        let mut h = std::ptr::null_mut();
        for n in ["libxkbcommon.so.0", "libxkbcommon.so"] {
            let c = CString::new(n).unwrap();
            h = libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if !h.is_null() {
                break;
            }
        }
        if h.is_null() {
            return Err("libxkbcommon not found: US legends".into());
        }
        let (Some(ctx_new), Some(km_new), Some(nl), Some(lname), Some(syms), Some(utf8), Some(gname), Some(km_unref), Some(ctx_unref)) = (
            sym::<CtxNew>(h, "xkb_context_new"),
            sym::<KeymapNewFromString>(h, "xkb_keymap_new_from_string"),
            sym::<NumLayouts>(h, "xkb_keymap_num_layouts"),
            sym::<LayoutName>(h, "xkb_keymap_layout_get_name"),
            sym::<SymsByLevel>(h, "xkb_keymap_key_get_syms_by_level"),
            sym::<ToUtf8>(h, "xkb_keysym_to_utf8"),
            sym::<GetName>(h, "xkb_keysym_get_name"),
            sym::<Unref>(h, "xkb_keymap_unref"),
            sym::<Unref>(h, "xkb_context_unref"),
        ) else {
            return Err("libxkbcommon: missing symbols".into());
        };
        let ctx = ctx_new(0);
        if ctx.is_null() {
            return Err("xkb_context_new failed".into());
        }
        let text = CString::new(keymap.trim_end_matches('\0')).map_err(|_| "keymap has NUL".to_string())?;
        let km = km_new(ctx, text.as_ptr(), 1, 0);
        if km.is_null() {
            ctx_unref(ctx);
            return Err("keymap did not compile".into());
        }
        let n = nl(km);
        let layouts: Vec<String> = (0..n)
            .map(|i| {
                let p = lname(km, i);
                if p.is_null() {
                    String::new()
                } else {
                    CStr::from_ptr(p).to_string_lossy().into_owned()
                }
            })
            .collect();
        let g = if group < n { group } else { 0 };
        let mut map = HashMap::new();
        let text_of = |level: u32, code: u16| -> String {
            let mut out: *const u32 = std::ptr::null();
            let k = syms(km, code as u32 + 8, g, level, &mut out);
            if k < 1 || out.is_null() {
                return String::new();
            }
            let ks = *out;
            let mut b = [0 as c_char; 16];
            let r = utf8(ks, b.as_mut_ptr(), b.len());
            let s = if r > 1 { CStr::from_ptr(b.as_ptr()).to_string_lossy().into_owned() } else { String::new() };
            let printable = s.chars().next().is_some_and(|c| !c.is_control() && !('\u{300}'..='\u{36f}').contains(&c));
            if printable {
                return s;
            }
            let mut nb = [0 as c_char; 64];
            if gname(ks, nb.as_mut_ptr(), nb.len()) > 0 {
                let name = CStr::from_ptr(nb.as_ptr()).to_string_lossy().into_owned();
                if let Some(d) = dead_key(&name) {
                    return d.to_string();
                }
            }
            String::new()
        };
        for &c in codes {
            let a = text_of(0, c);
            if !a.is_empty() {
                map.insert(c, [a, text_of(1, c), text_of(2, c), text_of(3, c)]);
            }
        }
        km_unref(km);
        ctx_unref(ctx);
        Ok(Labels { map, layouts, group: g })
    }
}

#[cfg(test)]
mod tests {
    use super::pick;

    fn l(a: &str, b: &str, c: &str, d: &str) -> [String; 4] {
        [a.into(), b.into(), c.into(), d.into()]
    }

    #[test]
    fn legend_levels_shift_altgr_caps() {
        // German Q: q Q @ (none); E: e E euro; 2: 2 " superscript-2
        let q = l("q", "Q", "@", "");
        assert_eq!(pick(&q, false, false, false), "q");
        assert_eq!(pick(&q, true, false, false), "Q");
        assert_eq!(pick(&q, false, true, false), "@");
        assert_eq!(pick(&q, true, true, false), "@", "level 4 missing -> level 3");
        assert_eq!(pick(&q, false, false, true), "Q", "Caps: upper case");
        assert_eq!(pick(&q, true, false, true), "q", "Caps + Shift: lower case");
        let two = l("2", "\"", "\u{b2}", "");
        assert_eq!(pick(&two, false, false, true), "2", "Caps does not shift digits");
        assert_eq!(pick(&two, true, false, false), "\"");
        assert_eq!(pick(&two, false, true, false), "\u{b2}");
        // US: no level 3 -> AltGr shows the plain / shifted legend
        let us = l("a", "A", "", "");
        assert_eq!(pick(&us, false, true, false), "a");
        assert_eq!(pick(&us, true, true, false), "A");
    }
}
