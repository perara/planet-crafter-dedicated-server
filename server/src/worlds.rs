//! The world files, in one place: how a world is named, where its save and its backups are, and
//! which files in a worlds folder are worlds. `pcds world ...` (cli/worlds.rs), the admin API
//! (dashboard.rs) and the front's world controls (front/world_control.rs) used to spell these
//! rules each in their own way.
//!
//! A world is `<dir>/<name>.json`; the running one is the server's save file. Its backups are
//! `<dir>/backups/<world>-YYYYMMDD-HHMMSS.json` (save.rs makes them), and a world that is
//! removed, or replaced by a new one, is kept as `<dir>/backups/<name>-<why>-<time>.json`.
//!
//! Where the callers differ on purpose, they still do: the command line takes a path or a file
//! name for a world (`arg`), the API only a name (`name`, which also refuses the settings files);
//! the command line lists worlds by file name, the API by world name; a removal by the API
//! refuses a second one in the same second, the command line numbers it (cli::free_path).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// The folder of a world's backups, beside its save.
pub const BACKUPS: &str = "backups";
/// The settings files that live beside the worlds when there is no installation folder: not worlds.
const NOT_WORLDS: [&str; 2] = ["access", "rules"];

/// Whether this is a name a world can have: letters, digits, '.', '_' and '-', up to 64,
/// starting with a letter or a digit.
pub fn valid(name: &str) -> bool {
    name.len() <= 64 && name.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// A world's name from the admin API: valid, and not one of the settings files.
pub fn name(name: &str) -> Result<String, String> {
    if !valid(name) || NOT_WORLDS.contains(&name) {
        return Err(format!("world names are letters, digits, '.', '_' and '-' (up to 64), not access or rules: {name:?}"));
    }
    Ok(name.to_string())
}

/// A world's name as typed (`pcds world use NAME`): the name itself, never a path.
pub fn name_arg(arg: &str) -> Result<String, String> {
    if valid(arg) && !NOT_WORLDS.contains(&arg) {
        return Ok(arg.to_string());
    }
    Err(format!("{arg:?} is not a world name: letters, digits, '.', '_' and '-', up to 64, starting with a letter or digit ('pcds worlds' lists them)"))
}

/// A world's name from a save's file name or path (`pcds world import`'s default).
pub fn arg(arg: &str) -> Result<String, String> {
    let name = arg.strip_suffix(".json").unwrap_or(arg);
    let name = name.rsplit('/').next().unwrap_or(name);
    if !valid(name) {
        return Err(format!("world names are letters, digits, '.', '_' and '-' (up to 64): '{arg}'"));
    }
    Ok(name.to_string())
}

/// The name of the world a save file is: its file name without the extension.
pub fn stem(save_path: &str) -> String {
    Path::new(save_path).file_stem().unwrap_or_default().to_string_lossy().to_string()
}

/// The folder the worlds are in: the one the save is in.
pub fn dir(save_path: &str) -> &Path {
    crate::util::parent_dir(Path::new(save_path))
}

/// The save file of the world `name` beside the save at `save_path`.
pub fn file(save_path: &str, name: &str) -> PathBuf {
    Path::new(save_path).with_file_name(format!("{name}.json"))
}

/// The folder of the backups beside the save at `save_path`.
pub fn backups(save_path: &str) -> PathBuf {
    dir(save_path).join(BACKUPS)
}

/// Where the front keeps the last flip's snapshot of the world at `save_path` when it stops with
/// the world in no bank (`<save>.snapshot`); the bank looks there too.
pub fn snapshot_path(save_path: &str) -> String {
    format!("{save_path}.snapshot")
}

/// What a world is kept as in backups/ (without `.json`) when it is replaced or removed:
/// `<name>-<why>-<time>`.
pub fn kept_stem(name: &str, why: &str, at: SystemTime) -> String {
    format!("{name}-{why}-{}", crate::save::stamp(at))
}

/// A file of a worlds folder: the world's name, its size and when it was written.
pub struct Entry {
    pub name: String,
    pub bytes: u64,
    pub modified: Option<SystemTime>,
}

/// The `.json` files of a folder that `keep` takes by their name without the extension, in the
/// order the file system gives them.
pub fn list(dir: &Path, keep: &dyn Fn(&str) -> bool) -> Vec<Entry> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok().filter(|m| m.is_file())?;
            let name = e.file_name().to_string_lossy().strip_suffix(".json").filter(|n| !n.is_empty() && keep(n))?.to_string();
            Some(Entry { name, bytes: meta.len(), modified: meta.modified().ok() })
        })
        .collect()
}

/// The worlds beside a save: every `.json` file there but the settings files.
pub fn worlds(save_path: &str) -> Vec<Entry> {
    list(dir(save_path), &|n| !NOT_WORLDS.contains(&n))
}

/// The worlds beside the save and this world's backups as the admin API lists them (GET
/// /api/worlds): by name, with their size and modification time (Unix seconds).
pub fn overview(save_path: &str) -> Value {
    let current = stem(save_path);
    let listed = |mut entries: Vec<Entry>| {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
            .into_iter()
            .filter_map(|e| {
                let modified = e.modified?.duration_since(UNIX_EPOCH).ok()?.as_secs();
                Some(json!({ "name": e.name, "bytes": e.bytes, "modified": modified }))
            })
            .collect::<Vec<Value>>()
    };
    let worlds = listed(worlds(save_path));
    // save::backup's names: <world>-YYYYMMDD-HHMMSS.
    let backups = listed(list(&backups(save_path), &|n| n.strip_prefix(&format!("{current}-")).is_some_and(|t| t.len() == 15)));
    json!({ "current": current, "worlds": worlds, "backups": backups })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_one_rule_with_two_ways_in() {
        for good in ["world", "my.world_2-b", "Standard-1", &"a".repeat(64)] {
            assert!(valid(good), "{good}");
            assert_eq!(name(good).unwrap(), good);
            assert_eq!(arg(good).unwrap(), good);
        }
        for bad in ["", ".hidden", "-x", "a b", "two words", "a/b", "ä", "ünï", &"a".repeat(65)] {
            assert!(!valid(bad), "{bad:?}");
            assert!(name(bad).is_err(), "{bad:?}");
        }
        // The API refuses the settings files; a command line argument may be a path.
        assert!(name("access").is_err() && name("rules").is_err());
        assert_eq!(arg("/home/me/saves/Standard-1.json").unwrap(), "Standard-1");
        assert_eq!(arg("world.json").unwrap(), "world");
        assert!(arg("a/").is_err() && arg("").is_err());
        assert!(arg("../x y").is_err());
        assert!(name("access").unwrap_err().contains("not access or rules") && !arg("a b").unwrap_err().contains("access"));
    }

    #[test]
    fn a_worlds_files_are_beside_the_save() {
        let save = "/srv/worlds/main.json";
        assert_eq!((stem(save), dir(save), file(save, "b"), backups(save)), ("main".into(), Path::new("/srv/worlds"), PathBuf::from("/srv/worlds/b.json"), PathBuf::from("/srv/worlds/backups")));
        assert_eq!((dir("world.json"), file("world.json", "b")), (Path::new("."), PathBuf::from("b.json")));
        let at = SystemTime::UNIX_EPOCH;
        assert_eq!(kept_stem("b", "removed", at), format!("b-removed-{}", crate::save::stamp(at)));
    }

    #[test]
    fn the_listing_takes_json_files_and_skips_the_settings_files() {
        let dir = std::env::temp_dir().join(format!("pcds-worlds-list-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("backups")).unwrap();
        for f in ["world.json", "other.json", "access.json", "rules.json", "world.json.tmp", ".json", "notes.txt"] {
            std::fs::write(dir.join(f), "{}").unwrap();
        }
        let mut names: Vec<String> = worlds(dir.join("world.json").to_str().unwrap()).into_iter().map(|e| e.name).collect();
        names.sort();
        assert_eq!(names, ["other", "world"]);
        let all = list(&dir, &|_| true);
        assert!(all.iter().any(|e| e.name == "access") && all.iter().all(|e| e.bytes == 2 && e.modified.is_some()));
        assert!(list(&dir.join("nothing"), &|_| true).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn worlds_lists_saves_and_this_worlds_backups() {
        let dir = std::env::temp_dir().join(format!("pcds-worlds-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("backups")).unwrap();
        for f in ["world.json", "other.json", "access.json", "rules.json", "world.json.tmp", "backups/world-20261018-120000.json", "backups/other-20261018-120000.json"] {
            std::fs::write(dir.join(f), "{}").unwrap();
        }
        let w = overview(dir.join("world.json").to_str().unwrap());
        let names = |k: &str| w[k].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!((w["current"].as_str(), names("worlds"), names("backups")), (Some("world"), vec!["other".to_string(), "world".into()], vec!["world-20261018-120000".to_string()]));
        assert_eq!(w["worlds"][0]["bytes"], 2);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
