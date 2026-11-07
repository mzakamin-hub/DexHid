// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! M3 profiles: a full setup (keyboard layout + the area arrangement + quick actions + any settings) saved under a
//! name, chosen per monitor and optionally per focused application.
//!
//! Files: $XDG_CONFIG_HOME/dexhid/profiles/<name>/profile.toml (+ the arrangement files of that profile next to it).
//! A profile's `settings` table is merged over the user's settings.toml (and under `--config`). Matching:
//! `monitors` holds monitor ids (the EDID hash prefix the daemon logs, or a connector name such as "DP-1"),
//! `apps` holds window classes (KWin resourceClass, e.g. "org.kde.konsole", "firefox"); "*" matches any monitor.
//! Selection order: an app match (when per-app switching is on), then a monitor match, then the profile named
//! "default", else none (the plain settings).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Profile {
    pub name: String,
    pub monitors: Vec<String>,
    pub apps: Vec<String>,
    /// keyboard layout of the profile (None = keep the current one)
    pub layout: Option<String>,
    /// settings overrides (the same keys as settings.toml)
    pub settings: toml::Table,
}

pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 40 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn dir_of(base: &Path, name: &str) -> PathBuf {
    base.join(name)
}

pub fn load_all(base: &Path) -> (Vec<Profile>, Vec<String>) {
    let mut v = vec![];
    let mut notes = vec![];
    let Ok(rd) = std::fs::read_dir(base) else { return (v, notes) };
    let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    for d in dirs {
        let f = d.join("profile.toml");
        match std::fs::read_to_string(&f).map_err(|e| e.to_string()).and_then(|s| toml::from_str::<Profile>(&s).map_err(|e| e.to_string())) {
            Ok(mut p) => {
                let dn = d.file_name().unwrap().to_string_lossy().into_owned();
                if p.name.is_empty() {
                    p.name = dn.clone();
                }
                if p.name != dn || !valid_name(&p.name) {
                    notes.push(format!("profile {}: name {:?} does not match its directory (skipped)", d.display(), p.name));
                    continue;
                }
                v.push(p);
            }
            Err(e) => notes.push(format!("profile {}: {}", f.display(), e)),
        }
    }
    (v, notes)
}

pub fn save(base: &Path, p: &Profile) -> Result<PathBuf, String> {
    if !valid_name(&p.name) {
        return Err(format!("bad profile name {:?} (a-z, 0-9, - and _, at most 40)", p.name));
    }
    let d = dir_of(base, &p.name);
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {}", d.display(), e))?;
    let f = d.join("profile.toml");
    let tmp = d.join("profile.toml.tmp");
    let s = format!("# DexHid profile (written by `dexhid profile save`)\n{}", toml::to_string(p).map_err(|e| e.to_string())?);
    std::fs::write(&tmp, s).map_err(|e| format!("{}: {}", tmp.display(), e))?;
    std::fs::rename(&tmp, &f).map_err(|e| format!("{}: {}", f.display(), e))?;
    Ok(f)
}

pub fn delete(base: &Path, name: &str) -> Result<(), String> {
    if !valid_name(name) {
        return Err(format!("bad profile name {:?}", name));
    }
    let d = dir_of(base, name);
    if !d.join("profile.toml").exists() {
        return Err(format!("no profile {}", name));
    }
    std::fs::remove_dir_all(&d).map_err(|e| format!("{}: {}", d.display(), e))
}

fn mon_match(p: &Profile, monitor: Option<&str>) -> bool {
    match monitor {
        Some(m) => p.monitors.iter().any(|x| x == "*" || x == m || (x.len() >= 6 && m.starts_with(x.as_str()))),
        None => false,
    }
}

/// the profile for this monitor / focused app (see the module doc for the order); `forced` = chosen by the user
pub fn select<'a>(ps: &'a [Profile], monitor: Option<&str>, app: Option<&str>, per_app: bool, forced: Option<&str>) -> Option<&'a Profile> {
    if let Some(f) = forced {
        if let Some(p) = ps.iter().find(|p| p.name == f) {
            return Some(p);
        }
    }
    if per_app {
        if let Some(a) = app.filter(|a| !a.is_empty()) {
            let a = a.to_lowercase();
            // an app profile for this monitor beats one for any monitor
            let hits: Vec<&Profile> = ps.iter().filter(|p| p.apps.iter().any(|x| x.to_lowercase() == a)).collect();
            if let Some(p) = hits.iter().find(|p| mon_match(p, monitor)).or(hits.first()) {
                return Some(p);
            }
        }
    }
    ps.iter()
        .filter(|p| p.apps.is_empty())
        .find(|p| mon_match(p, monitor) && !p.monitors.iter().all(|m| m == "*"))
        .or_else(|| ps.iter().filter(|p| p.apps.is_empty()).find(|p| mon_match(p, monitor)))
        .or_else(|| ps.iter().find(|p| p.name == "default"))
}

/// the settings table of the user's settings with a profile on top
pub fn layered(user: &toml::Table, p: Option<&Profile>) -> toml::Table {
    let mut t = user.clone();
    if let Some(p) = p {
        crate::config::merge(&mut t, p.settings.clone());
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str, mons: &[&str], apps: &[&str]) -> Profile {
        Profile { name: name.into(), monitors: mons.iter().map(|s| s.to_string()).collect(), apps: apps.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    #[test]
    fn selection_order() {
        let ps = vec![
            p("default", &[], &[]),
            p("office", &["a1b2c3d4"], &[]),
            p("anymon", &["*"], &[]),
            p("term", &[], &["org.kde.konsole"]),
            p("term-office", &["a1b2c3"], &["org.kde.konsole"]),
            p("draw", &[], &["krita"]),
        ];
        let n = |x: Option<&Profile>| x.map(|p| p.name.clone()).unwrap_or_default();
        assert_eq!(n(select(&ps, Some("a1b2c3d4e5"), None, true, None)), "office", "monitor match by hash prefix");
        assert_eq!(n(select(&ps, Some("ffff00"), None, true, None)), "anymon", "* when no specific monitor matches");
        assert_eq!(n(select(&ps, None, None, true, None)), "default");
        assert_eq!(n(select(&ps, Some("a1b2c3d4"), Some("org.kde.konsole"), true, None)), "term-office", "app + monitor beats app");
        assert_eq!(n(select(&ps, Some("ffff00"), Some("org.kde.Konsole"), true, None)), "term", "case-insensitive app class");
        assert_eq!(n(select(&ps, Some("a1b2c3d4"), Some("org.kde.konsole"), false, None)), "office", "per-app off");
        assert_eq!(n(select(&ps, Some("a1b2c3d4"), Some("krita"), true, Some("default"))), "default", "a forced profile wins");
        assert_eq!(n(select(&ps, Some("a1b2c3d4"), Some("unknown"), true, None)), "office");
        assert!(select(&[], None, None, true, None).is_none());
    }

    #[test]
    fn save_load_delete_layer() {
        let d = std::env::temp_dir().join(format!("dexhid-prof-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let mut a = p("office", &["DP-1"], &[]);
        a.layout = Some("laptop".into());
        crate::settings::set(&mut a.settings, "speed", toml::Value::Float(1.4));
        crate::settings::set(&mut a.settings, "features.gestures.enabled", toml::Value::Boolean(true));
        save(&d, &a).unwrap();
        assert!(save(&d, &p("bad name", &[], &[])).is_err());
        std::fs::create_dir_all(d.join("broken")).unwrap();
        std::fs::write(d.join("broken/profile.toml"), "nope = 1\n").unwrap();
        let (ps, notes) = load_all(&d);
        assert_eq!(ps, vec![a.clone()]);
        assert_eq!(notes.len(), 1, "{:?}", notes);
        let mut user = toml::Table::new();
        crate::settings::set(&mut user, "speed", toml::Value::Float(1.0));
        crate::settings::set(&mut user, "dim", toml::Value::Boolean(false));
        let l = layered(&user, Some(&a));
        assert_eq!(crate::settings::get(&toml::Value::Table(l.clone()), "speed"), Some(&toml::Value::Float(1.4)));
        assert_eq!(crate::settings::get(&toml::Value::Table(l.clone()), "dim"), Some(&toml::Value::Boolean(false)));
        let (c, notes) = crate::config::load_with(&[], &l, false);
        assert!(notes.iter().all(|n| !n.starts_with("ERROR")), "{:?}", notes);
        assert!(c.features.gestures.enabled && (c.speed - 1.4).abs() < 1e-6);
        delete(&d, "office").unwrap();
        assert!(delete(&d, "office").is_err());
        std::fs::remove_dir_all(&d).ok();
    }
}
