// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! Keyboard layouts as data files (TOML). Format, by example:
//!
//! ```toml
//! name = "mini"                 # file stem by convention; used by `dexhid layout mini`
//! title = "Mini"
//! [[part]]                      # one part per screen region: keyboard | keyboard_left | keyboard_right | numpad
//! region = "keyboard"
//! optional = "numpad"           # optional: shown only when this option is in keyboard_options
//! [[part.page]]
//! id = "main"                   # pages switch with action keys ("page:<id>"); the first page is the start page
//! [[part.page.row]]
//! height = 1.0                  # relative row height (default 1)
//! optional = "function"         # optional row, shown only with that option
//! keys = [ { key = "ESC", label = "Esc" }, { gap = 0.5 }, { key = "A", w = 1.5 }, { action = "page:sym", label = "?123" } ]
//! ```
//! A key is exactly one of: `key` (evdev name, see keycodes.rs), `action` (page:<id>, layout:next, keyboard:hide),
//! or `gap` (an empty slot of that width). `w` is the width in key units (default 1). `label` / `shift_label`
//! override the legend; without them, symbol keys take their legend from the desktop's xkb keymap and the rest
//! from the US table.

use crate::geom::Rect;
use crate::keycodes;
use serde::Deserialize;
use std::collections::HashSet;

pub const REGIONS: &[&str] = &["keyboard", "keyboard_left", "keyboard_right", "numpad"];

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
struct FileDef {
    name: String,
    title: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default, rename = "part")]
    parts: Vec<PartDef>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
struct PartDef {
    region: String,
    optional: Option<String>,
    #[serde(rename = "page")]
    pages: Vec<PageDef>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
struct PageDef {
    #[serde(default = "main_id")]
    id: String,
    #[serde(rename = "row")]
    rows: Vec<RowDef>,
}
fn main_id() -> String {
    "main".into()
}
fn one() -> f32 {
    1.0
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
struct RowDef {
    #[serde(default = "one")]
    height: f32,
    optional: Option<String>,
    keys: Vec<KeyDef>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
struct KeyDef {
    key: Option<String>,
    action: Option<String>,
    gap: Option<f32>,
    w: Option<f32>,
    label: Option<String>,
    shift_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Page(String),
    LayoutNext,
    KeyboardHide,
}

#[derive(Debug, Clone, PartialEq)]
pub enum KeyKind {
    Code(u16),
    Action(Action),
    Gap,
}

#[derive(Debug, Clone)]
pub struct Key {
    pub kind: KeyKind,
    pub w: f32,
    pub label: Option<String>,
    pub shift_label: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub height: f32,
    pub optional: Option<String>,
    pub keys: Vec<Key>,
}

#[derive(Debug, Clone)]
pub struct Page {
    pub id: String,
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone)]
pub struct Part {
    pub region: String,
    pub optional: Option<String>,
    pub pages: Vec<Page>,
}

#[derive(Debug, Clone)]
pub struct Layout {
    pub name: String,
    pub title: String,
    pub description: String,
    pub parts: Vec<Part>,
    /// where it was loaded from ("builtin" or a path)
    pub source: String,
}

/// one key placed on screen
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedKey {
    pub rect: Rect,
    pub kind: KeyKind,
    pub label: Option<String>,
    pub shift_label: Option<String>,
    pub region: String,
}

impl Layout {
    /// the built-in "none" layout: touchpad only
    pub fn none() -> Layout {
        Layout { name: "none".into(), title: "Touchpad only".into(), description: String::new(), parts: vec![], source: "builtin".into() }
    }

    pub fn parse(text: &str, source: &str) -> Result<Layout, String> {
        let f: FileDef = toml::from_str(text).map_err(|e| format!("{}: {}", source, e))?;
        let err = |m: String| Err(format!("{}: {}", source, m));
        if f.name.is_empty() || !f.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return err(format!("bad layout name {:?} (use a-z, 0-9, - and _)", f.name));
        }
        let mut parts = vec![];
        let mut regions = HashSet::new();
        for (pi, p) in f.parts.iter().enumerate() {
            if !REGIONS.contains(&p.region.as_str()) {
                return err(format!("part {}: unknown region {:?} (one of {:?})", pi + 1, p.region, REGIONS));
            }
            if !regions.insert(p.region.clone()) {
                return err(format!("part {}: region {:?} used twice", pi + 1, p.region));
            }
            if p.pages.is_empty() {
                return err(format!("part {}: no pages", pi + 1));
            }
            let mut pages = vec![];
            for (gi, g) in p.pages.iter().enumerate() {
                if g.rows.is_empty() {
                    return err(format!("part {} page {:?}: no rows", pi + 1, g.id));
                }
                let mut rows = vec![];
                for (ri, r) in g.rows.iter().enumerate() {
                    if !(r.height > 0.0 && r.height <= 4.0) {
                        return err(format!("part {} page {} row {}: height {} out of range", pi + 1, gi + 1, ri + 1, r.height));
                    }
                    let mut keys = vec![];
                    for (ki, k) in r.keys.iter().enumerate() {
                        let at = format!("part {} page {:?} row {} key {}", pi + 1, g.id, ri + 1, ki + 1);
                        let n = k.key.is_some() as u8 + k.action.is_some() as u8 + k.gap.is_some() as u8;
                        if n != 1 {
                            return err(format!("{}: needs exactly one of key / action / gap", at));
                        }
                        let w = k.gap.or(k.w).unwrap_or(1.0);
                        if !(w > 0.0 && w <= 12.0) {
                            return err(format!("{}: width {} out of range", at, w));
                        }
                        let kind = if let Some(name) = &k.key {
                            match keycodes::code(name) {
                                Some(c) => KeyKind::Code(c),
                                None => return err(format!("{}: unknown key name {:?}", at, name)),
                            }
                        } else if let Some(a) = &k.action {
                            KeyKind::Action(match parse_action(a) {
                                Some(a) => a,
                                None => return err(format!("{}: unknown action {:?}", at, a)),
                            })
                        } else {
                            KeyKind::Gap
                        };
                        keys.push(Key { kind, w, label: k.label.clone(), shift_label: k.shift_label.clone() });
                    }
                    if keys.is_empty() {
                        return err(format!("part {} page {} row {}: no keys", pi + 1, gi + 1, ri + 1));
                    }
                    rows.push(Row { height: r.height, optional: r.optional.clone(), keys });
                }
                pages.push(Page { id: g.id.clone(), rows });
            }
            parts.push(Part { region: p.region.clone(), optional: p.optional.clone(), pages });
        }
        // every page action must point at a page that exists somewhere in the layout
        let ids: HashSet<&str> = parts.iter().flat_map(|p| p.pages.iter().map(|g| g.id.as_str())).collect();
        for p in &parts {
            for g in &p.pages {
                for r in &g.rows {
                    for k in &r.keys {
                        if let KeyKind::Action(Action::Page(id)) = &k.kind {
                            if !ids.contains(id.as_str()) {
                                return err(format!("action page:{} points at a page that does not exist", id));
                            }
                        }
                    }
                }
            }
        }
        if parts.is_empty() {
            return err("no [[part]]: use the built-in \"none\" layout for touchpad only".into());
        }
        Ok(Layout { name: f.name.clone(), title: f.title.unwrap_or(f.name), description: f.description, parts, source: source.into() })
    }

    /// the parts shown with the given options
    pub fn visible_parts<'a>(&'a self, options: &'a HashSet<String>) -> impl Iterator<Item = &'a Part> + 'a {
        self.parts.iter().filter(move |p| p.optional.as_ref().is_none_or(|o| options.contains(o)))
    }

    pub fn regions(&self, options: &HashSet<String>) -> Vec<String> {
        self.visible_parts(options).map(|p| p.region.clone()).collect()
    }

    pub fn first_page(&self) -> String {
        self.parts.first().and_then(|p| p.pages.first()).map(|g| g.id.clone()).unwrap_or_else(main_id)
    }

    /// size of a part's page in key units: (widest row, sum of row heights)
    pub fn units(page: &Page, options: &HashSet<String>) -> (f32, f32) {
        let rows: Vec<&Row> = page.rows.iter().filter(|r| r.optional.as_ref().is_none_or(|o| options.contains(o))).collect();
        let w = rows.iter().map(|r| r.keys.iter().map(|k| k.w).sum::<f32>()).fold(0.0, f32::max);
        let h = rows.iter().map(|r| r.height).sum::<f32>();
        (w, h)
    }

    /// the page of `part` for the current page id (a part without that page shows its first page)
    pub fn page_of<'a>(part: &'a Part, page_id: &str) -> &'a Page {
        part.pages.iter().find(|g| g.id == page_id).unwrap_or(&part.pages[0])
    }

    /// Place the keys of one part inside `r` (logical px). All rows share one key-unit width so columns line up;
    /// shorter rows are centred. `gap` is the space between keys in px.
    pub fn place(part: &Part, page_id: &str, r: &Rect, options: &HashSet<String>, gap: f32) -> Vec<PlacedKey> {
        let page = Self::page_of(part, page_id);
        let (uw, uh) = Self::units(page, options);
        if uw <= 0.0 || uh <= 0.0 {
            return vec![];
        }
        let kw = r.w / uw;
        let kh = r.h / uh;
        let mut out = vec![];
        let mut y = r.y;
        for row in page.rows.iter().filter(|row| row.optional.as_ref().is_none_or(|o| options.contains(o))) {
            let rw: f32 = row.keys.iter().map(|k| k.w).sum();
            let mut x = r.x + (uw - rw) * kw / 2.0;
            let h = row.height * kh;
            for k in &row.keys {
                let w = k.w * kw;
                if k.kind != KeyKind::Gap {
                    out.push(PlacedKey {
                        rect: Rect::new(x + gap / 2.0, y + gap / 2.0, (w - gap).max(1.0), (h - gap).max(1.0)),
                        kind: k.kind.clone(),
                        label: k.label.clone(),
                        shift_label: k.shift_label.clone(),
                        region: part.region.clone(),
                    });
                }
                x += w;
            }
            y += h;
        }
        out
    }
}

fn parse_action(a: &str) -> Option<Action> {
    if let Some(p) = a.strip_prefix("page:") {
        if !p.is_empty() {
            return Some(Action::Page(p.to_string()));
        }
    }
    match a {
        "layout:next" => Some(Action::LayoutNext),
        "keyboard:hide" => Some(Action::KeyboardHide),
        _ => None,
    }
}

/// The three layouts shipped with DexHid (also installed to /usr/share/dexhid/layouts/).
pub const BUILTIN: &[(&str, &str)] = &[
    ("full", include_str!("../../../data/layouts/full.toml")),
    ("tablet", include_str!("../../../data/layouts/tablet.toml")),
    ("split", include_str!("../../../data/layouts/split.toml")),
];

/// Load all layouts: built-ins, then every *.toml of `dirs` in order (a later file with the same name replaces an
/// earlier one). Broken files are reported and skipped.
pub fn load_all(dirs: &[std::path::PathBuf]) -> (Vec<Layout>, Vec<String>) {
    let mut out: Vec<Layout> = vec![Layout::none()];
    let mut errs = vec![];
    let add = |l: Layout, out: &mut Vec<Layout>| {
        if let Some(i) = out.iter().position(|x| x.name == l.name) {
            out[i] = l;
        } else {
            out.push(l);
        }
    };
    for (n, t) in BUILTIN {
        match Layout::parse(t, &format!("builtin:{}", n)) {
            Ok(l) => add(l, &mut out),
            Err(e) => errs.push(e),
        }
    }
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(d) else { continue };
        let mut files: Vec<_> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "toml")).collect();
        files.sort();
        for p in files {
            match std::fs::read_to_string(&p) {
                Ok(t) => match Layout::parse(&t, &p.display().to_string()) {
                    Ok(l) if l.name == "none" => errs.push(format!("{}: the name \"none\" is reserved", p.display())),
                    Ok(l) => add(l, &mut out),
                    Err(e) => errs.push(e),
                },
                Err(e) => errs.push(format!("{}: {}", p.display(), e)),
            }
        }
    }
    (out, errs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(v: &[&str]) -> HashSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn builtins_parse() {
        let (all, errs) = load_all(&[]);
        assert!(errs.is_empty(), "{:?}", errs);
        let names: Vec<&str> = all.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, vec!["none", "full", "tablet", "split"]);
        let full = &all[1];
        // full PC keyboard: function row, Esc, Tab, Ctrl/Alt/Meta, arrows, Home/End/PgUp/PgDn, Ins/Del, numpad (optional)
        let codes: HashSet<u16> = full
            .parts
            .iter()
            .flat_map(|p| p.pages.iter().flat_map(|g| g.rows.iter().flat_map(|r| r.keys.iter())))
            .filter_map(|k| if let KeyKind::Code(c) = k.kind { Some(c) } else { None })
            .collect();
        for n in [
            "ESC", "F1", "F12", "TAB", "LEFTCTRL", "LEFTALT", "LEFTMETA", "RIGHTALT", "UP", "DOWN", "LEFT", "RIGHT", "HOME",
            "END", "PAGEUP", "PAGEDOWN", "INSERT", "DELETE", "KP7", "KPENTER", "A", "Z", "SPACE", "ENTER", "BACKSPACE",
        ] {
            assert!(codes.contains(&keycodes::code(n).unwrap()), "full layout lacks {}", n);
        }
        assert_eq!(full.regions(&opts(&["function"])), vec!["keyboard"]);
        assert_eq!(full.regions(&opts(&["function", "numpad"])), vec!["keyboard", "numpad"]);
        let tablet = &all[2];
        assert!(tablet.parts[0].pages.len() >= 2, "tablet layout needs a symbols page");
        let split = &all[3];
        assert_eq!(split.regions(&opts(&[])), vec!["keyboard_left", "keyboard_right"]);
    }

    #[test]
    fn errors_are_reported() {
        let bad = r#"name = "x"
[[part]]
region = "keyboard"
[[part.page]]
[[part.page.row]]
keys = [ { key = "NOSUCHKEY" } ]
"#;
        let e = Layout::parse(bad, "t").unwrap_err();
        assert!(e.contains("unknown key name"), "{}", e);
        let bad2 = bad.replace("{ key = \"NOSUCHKEY\" }", "{ key = \"A\", gap = 1.0 }");
        assert!(Layout::parse(&bad2, "t").unwrap_err().contains("exactly one"));
        let bad3 = bad.replace("{ key = \"NOSUCHKEY\" }", "{ action = \"page:nope\" }");
        assert!(Layout::parse(&bad3, "t").unwrap_err().contains("does not exist"));
        let bad4 = bad.replace("keyboard", "middle");
        assert!(Layout::parse(&bad4, "t").unwrap_err().contains("unknown region"));
        let bad5 = bad.replace("keys =", "colour = 1\nkeys =");
        assert!(Layout::parse(&bad5, "t").is_err());
    }

    #[test]
    fn placement() {
        let t = r#"name = "mini"
[[part]]
region = "keyboard"
[[part.page]]
[[part.page.row]]
keys = [ { key = "Q" }, { key = "W" }, { gap = 1 }, { key = "E", w = 2 } ]
[[part.page.row]]
height = 2
optional = "big"
keys = [ { key = "A" } ]
"#;
        let l = Layout::parse(t, "t").unwrap();
        let r = Rect::new(0.0, 0.0, 500.0, 100.0);
        let k = Layout::place(&l.parts[0], "main", &r, &opts(&[]), 0.0);
        assert_eq!(k.len(), 3);
        assert_eq!(k[0].rect, Rect::new(0.0, 0.0, 100.0, 100.0));
        assert_eq!(k[2].rect, Rect::new(300.0, 0.0, 200.0, 100.0));
        let k = Layout::place(&l.parts[0], "main", &r, &opts(&["big"]), 0.0);
        assert_eq!(k.len(), 4);
        // second row is twice as high and centred
        assert!((k[3].rect.y - 33.333).abs() < 0.01 && (k[3].rect.h - 66.666).abs() < 0.01);
        assert!((k[3].rect.x - 200.0).abs() < 0.01);
    }

    #[test]
    fn user_dir_overrides_and_adds() {
        let d = std::env::temp_dir().join(format!("dexhid-layout-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("mini.toml"), "name = \"mini\"\n[[part]]\nregion = \"keyboard\"\n[[part.page]]\n[[part.page.row]]\nkeys = [ { key = \"A\" } ]\n").unwrap();
        std::fs::write(d.join("split.toml"), "name = \"split\"\ntitle = \"My split\"\n[[part]]\nregion = \"keyboard_left\"\n[[part.page]]\n[[part.page.row]]\nkeys = [ { key = \"A\" } ]\n").unwrap();
        std::fs::write(d.join("broken.toml"), "name = ").unwrap();
        let (all, errs) = load_all(std::slice::from_ref(&d));
        assert_eq!(errs.len(), 1);
        assert!(all.iter().any(|l| l.name == "mini"));
        assert_eq!(all.iter().find(|l| l.name == "split").unwrap().title, "My split");
        std::fs::remove_dir_all(&d).ok();
    }
}
