//! The worlds beside the running one (listing, new, remove, file, restore: dashboard.rs does the
//! files), and the backups: one now, and its copy to S3.

use serde::Serialize;

use super::{answer, error, field, json, Reply, Request};
use crate::bank::persist::note_backup;
use crate::game::Game;
use crate::{dashboard, save};
use std::time::Duration;

/// GET /api/worlds: the worlds, what a new one can be, the backups kept and S3.
pub(super) fn list(game: &mut Game, r: &Request) -> Reply {
    let mut w = dashboard::worlds(r.ctx.save_path);
    w["choices"] = crate::newworld::choices(&game.net_data, &game.data);
    w["backupsKept"] = game.rules.save.backups_kept.into();
    w["s3"] = serde_json::to_value(crate::s3::status(&game.rules.backup)).unwrap_or_default();
    (200, w.to_string())
}

pub(super) fn new(game: &mut Game, r: &Request) -> Reply {
    match dashboard::body::<dashboard::NewWorld>(r.body) {
        Ok(w) => answer(dashboard::world_new(game, r.ctx.save_path, w)),
        Err(e) => e,
    }
}

pub(super) fn remove(_: &mut Game, r: &Request) -> Reply {
    match field(r.body, "name") {
        Some(n) => answer(dashboard::world_remove(r.ctx.save_path, &n)),
        None => error(400, "'name' is required"),
    }
}

pub(super) fn file(_: &mut Game, r: &Request) -> Reply {
    match field(r.body, "name") {
        Some(n) => answer(dashboard::world_file(r.ctx.save_path, &n, r.body["backup"] == true)),
        None => error(400, "'name' is required"),
    }
}

pub(super) fn restore(_: &mut Game, r: &Request) -> Reply {
    match (field(r.body, "backup"), field(r.body, "name")) {
        (Some(b), Some(n)) => answer(dashboard::world_restore(r.ctx.save_path, &b, &n)),
        _ => error(400, "'backup' and 'name' are required"),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Backup {
    backup: Option<String>,
    s3: Option<String>,
    s3_error: Option<String>,
}

#[derive(Serialize)]
struct Key {
    key: Option<String>,
}

/// POST /api/backup: a backup now (after a save), copied to S3 when that is set.
pub(super) fn backup(game: &mut Game, r: &Request) -> Reply {
    let save_path = r.ctx.save_path;
    if game.rules.save.backups_kept == 0 {
        return error(400, "backups are off (the rule save.backupsKept is 0)");
    }
    if let Err(e) = save::write_atomic(save_path, game.save_text().as_bytes()) {
        return error(500, format!("save failed: {e}"));
    }
    let made = save::backup(save_path, Duration::ZERO, game.rules.save.backups_kept as usize).map_err(|e| format!("{save_path}: {e}"));
    note_backup(&made);
    if let Err(e) = made {
        return error(500, format!("backup failed: {e}"));
    }
    crate::bank::persist::verify_if_on(game, save_path);
    let made = crate::s3::newest_backup(save_path).and_then(|p| p.file_stem().map(|f| f.to_string_lossy().to_string()));
    let s3 = crate::s3::after_backup(&game.rules.backup, save_path, false);
    json(200, &Backup { backup: made, s3: s3.as_ref().ok().cloned().flatten(), s3_error: s3.err() })
}

/// POST /api/backup/s3: the newest backup to S3, now.
pub(super) fn backup_to_s3(game: &mut Game, r: &Request) -> Reply {
    answer(crate::s3::after_backup(&game.rules.backup, r.ctx.save_path, true).map(|key| Key { key }))
}

#[cfg(test)]
mod tests {
    use super::super::{json, request};
    use super::{Backup, Key};
    use crate::bank::status::Ctx;
    use crate::game;
    use game::Game;
    use serde_json::{json, Value};

    /// The answers of the backup endpoints print the bytes the json! macro made.
    #[test]
    fn the_answers_of_the_backup_endpoints_print_what_the_json_macro_did() {
        let cases: [(Option<&str>, Result<Option<&str>, &str>); 4] = [(Some("world-20260101-000000"), Ok(None)), (Some("w"), Ok(Some("pcds/w.json"))), (None, Err("HTTP 403: \"denied\"")), (Some("w\u{e5}"), Err("no thread"))];
        for (made, s3) in cases {
            let (made, s3) = (made.map(str::to_string), s3.map(|k| k.map(str::to_string)).map_err(str::to_string));
            let old = json!({"backup": made, "s3": s3.as_ref().ok().cloned().flatten(), "s3Error": s3.clone().err()}).to_string();
            assert_eq!(json(200, &Backup { backup: made, s3: s3.as_ref().ok().cloned().flatten(), s3_error: s3.err() }).1, old);
        }
        for key in [None, Some("pcds/w.json".to_string())] {
            assert_eq!(json(200, &Key { key: key.clone() }).1, json!({"key": key}).to_string());
        }
    }

    /// Worlds made, read, restored and removed through the API, a backup on demand, and the
    /// admin's pause holding the clock.
    #[test]
    fn worlds_backups_and_the_pause_through_the_api() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let dir = std::env::temp_dir().join(format!("pcds-bank-worlds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let save_path = dir.join("world.json").to_string_lossy().to_string();
        game.snapshot().write(&save_path).unwrap();
        let ctx = Ctx { ticks: &Default::default(), save_path: &save_path, build_id: None, data_dir: "", loop_rate: 0, busy: Default::default() };
        let call = |game: &mut Game, method: &str, path: &str, body: Value| {
            let (status, answer) = request(game, method, path, &body, &ctx);
            (status, serde_json::from_str::<Value>(&answer).unwrap())
        };
        let (status, w) = call(&mut game, "GET", "/api/worlds", json!({}));
        assert_eq!((status, w["current"].as_str()), (200, Some("world")));
        let planet = w["choices"]["planets"][0]["id"].as_str().unwrap().to_string();
        let (status, made) = call(&mut game, "POST", "/api/worlds/new", json!({"name": "second", "planet": planet, "seed": 7, "mode": "Chill"}));
        assert_eq!((status, made["seed"].as_u64()), (200, Some(7)), "{made}");
        let made = crate::save::SaveFile::load(&dir.join("second.json").to_string_lossy()).unwrap();
        assert_eq!((made.settings["planetId"].as_str(), made.settings["mode"].as_str()), (Some(planet.as_str()), Some("Chill")));
        for bad in [json!({"name": "second"}), json!({"name": "../x"}), json!({"name": "rules"}), json!({"name": "x", "mode": "Easy"}), json!({"name": "x", "planet": "Nowhere"})] {
            assert_eq!(call(&mut game, "POST", "/api/worlds/new", bad.clone()).0, 400, "{bad}");
        }
        let (status, file) = call(&mut game, "POST", "/api/worlds/file", json!({"name": "second"}));
        assert_eq!((status, file["text"].as_str().map(|t| t.contains("planetId"))), (200, Some(true)));
        // A backup now, then that backup as a new world.
        let (status, b) = call(&mut game, "POST", "/api/backup", json!({}));
        let backup = b["backup"].as_str().unwrap_or_else(|| panic!("{status} {b}")).to_string();
        assert!(backup.starts_with("world-") && b["s3"].is_null() && b["s3Error"].is_null(), "S3 off: {b}");
        assert_eq!(call(&mut game, "POST", "/api/worlds/restore", json!({"backup": backup, "name": "third"})).0, 200);
        assert!(dir.join("third.json").is_file());
        assert_eq!(call(&mut game, "POST", "/api/backup/s3", json!({})).0, 400, "S3 off");
        // Removed into backups/, never the running world.
        assert_eq!(call(&mut game, "POST", "/api/worlds/remove", json!({"name": "world"})).0, 400);
        assert_eq!(call(&mut game, "POST", "/api/worlds/remove", json!({"name": "second"})).0, 200);
        assert!(!dir.join("second.json").exists() && std::fs::read_dir(dir.join("backups")).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("second-removed-")));
        // Paused: the clock holds with a player in; resumed: it goes on.
        game::tests::join(&mut game, 1, "Ann");
        assert_eq!(call(&mut game, "POST", "/api/pause", json!({"paused": true})).0, 200);
        let before = game.time;
        game.sim_time = Some(game.sim_time.unwrap_or(0.0) + 5.0);
        game.tick();
        assert_eq!(game.time, before, "paused");
        assert_eq!(call(&mut game, "GET", "/api/status", json!({})).1["held"], true);
        assert_eq!(call(&mut game, "POST", "/api/pause", json!({"paused": false})).0, 200);
        game.sim_time = Some(game.sim_time.unwrap() + 5.0);
        game.tick();
        assert!(game.time > before, "resumed");
        std::fs::remove_dir_all(&dir).unwrap();
    }

}
