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
    /// evdev code -> (level 1, level 2)
    pub map: HashMap<u16, (String, String)>,
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
            let b = text_of(1, c);
            if !a.is_empty() {
                map.insert(c, (a, b));
            }
        }
        km_unref(km);
        ctx_unref(ctx);
        Ok(Labels { map, layouts, group: g })
    }
}
