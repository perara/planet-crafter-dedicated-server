//! The world's own endpoints: status, ping, save, pause, map and the plugins' (their own router).

use std::time::Instant;

use serde::Serialize;

use super::{error, json, ok, Reply, Request};
use crate::bank::persist::persist;
use crate::bank::status::{self, backup_status, BackupStatus};
use crate::game::Game;
use crate::{dashboard, plugins};

pub(super) fn status(game: &mut Game, r: &Request) -> Reply {
    json(200, &status::build(game, r.ctx, Instant::now()))
}

pub(super) fn ping(_: &mut Game, _: &Request) -> Reply {
    ok()
}

#[derive(Serialize)]
struct Saved {
    ok: bool,
    backup: BackupStatus,
}

#[derive(Serialize)]
struct Paused {
    paused: bool,
}

/// POST /api/save: the save, and what became of its backup.
pub(super) fn save(game: &mut Game, r: &Request) -> Reply {
    match persist(game, r.ctx.save_path) {
        Ok(()) => json(200, &Saved { ok: true, backup: backup_status(&game.rules.save) }),
        Err(e) => error(500, e),
    }
}

pub(super) fn plugins(game: &mut Game, r: &Request) -> Reply {
    plugins::admin::request(game, r.method, r.path, r.body)
}

/// POST /api/pause {"paused": bool}.
pub(super) fn pause(game: &mut Game, r: &Request) -> Reply {
    match r.body["paused"].as_bool() {
        Some(p) => {
            if game.held != p {
                game.held = p;
                println!("world {} by the admin", if p { "paused" } else { "resumed" });
                game.ext.chat.replies.push_back(if p { "The world is paused by the server admin." } else { "The world goes on." }.to_string());
            }
            json(200, &Paused { paused: p })
        }
        None => error(400, "'paused': true or false"),
    }
}

pub(super) fn map(game: &mut Game, _: &Request) -> Reply {
    (200, dashboard::map(game).to_string())
}

#[cfg(test)]
mod tests {
    use super::super::request;
    use super::*;
    use crate::bank::persist::{autosave, final_save, settle};
    use crate::bank::status::{Ctx, TickTimes};
    use crate::game;
    use crate::save::SaveFile;
    use serde_json::{json, Value};
    use std::time::Duration;
    /// The answers of save and pause print the bytes the json! macro made.
    #[test]
    fn the_answers_of_save_and_pause_print_what_the_json_macro_did() {
        let files = crate::rules::Save::default();
        crate::bank::persist::note_backup(&Err("backup \"x\" failed".to_string()));
        let backup = serde_json::to_value(backup_status(&files)).unwrap();
        assert_eq!(json(200, &Saved { ok: true, backup: backup_status(&files) }).1, json!({"ok": true, "backup": backup}).to_string());
        for p in [true, false] {
            assert_eq!(json(200, &Paused { paused: p }).1, json!({"paused": p}).to_string());
        }
        assert_eq!(json(200, &Paused { paused: true }).1, r#"{"paused":true}"#);
    }

    /// A backup that fails is no secret kept in the log: /api/status and the answer of POST /api/save
    /// say so (the save itself still goes through), until a backup is made.
    #[test]
    fn a_backup_that_fails_is_in_the_status_and_in_the_answer_of_a_save() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let dir = std::env::temp_dir().join(format!("pcds-backup-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("world.json").to_string_lossy().to_string();
        let ctx = Ctx { ticks: &Default::default(), save_path: &path, build_id: None, data_dir: "", loop_rate: 0, busy: Default::default() };
        std::fs::write(&path, "{}").unwrap();
        // backups/ is a file: no backup can be made.
        std::fs::write(dir.join("backups"), "in the way").unwrap();
        let ask = |game: &mut Game, method: &str, path: &str| {
            let (status, answer) = request(game, method, path, &json!({}), &ctx);
            (status, serde_json::from_str::<Value>(&answer).unwrap())
        };
        let (status, saved) = ask(&mut game, "POST", "/api/save");
        assert_eq!(status, 200, "the save itself went through: {saved}");
        assert!(saved["backup"]["error"]["message"].as_str().is_some_and(|m| m.contains("world.json")) && saved["backup"]["error"]["at"].as_u64() > Some(0) && saved["backup"]["lastOkAt"].is_null(), "{saved}");
        assert_eq!(SaveFile::load(&path).map(|s| !s.objects.is_empty()).ok(), Some(true));
        let (_, status) = ask(&mut game, "GET", "/api/status");
        assert_eq!(status["backup"], saved["backup"]);
        assert_eq!(status["backup"]["enabled"], true);
        // Mended: the next save makes the backup, and the failure is gone.
        std::fs::remove_file(dir.join("backups")).unwrap();
        let (_, saved) = ask(&mut game, "POST", "/api/save");
        assert!(saved["backup"]["error"].is_null() && saved["backup"]["lastOkAt"].as_u64() > Some(0), "{saved}");
        assert_eq!(std::fs::read_dir(dir.join("backups")).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A save that cannot be written (here a folder where the save goes): /api/save says so, as
    /// the status does; the autosave tries again; the last save keeps the world as a snapshot the
    /// next start runs, and the bank ends with 1.
    #[test]
    fn a_failed_save_is_reported_retried_and_kept() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let dir = std::env::temp_dir().join(format!("pcds-savefail-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("world.json")).unwrap();
        let path = dir.join("world.json").to_string_lossy().to_string();
        let ctx = Ctx { ticks: &Default::default(), save_path: &path, build_id: None, data_dir: "", loop_rate: 0, busy: Default::default() };
        let (status, answer) = request(&mut game, "POST", "/api/save", &json!({}), &ctx);
        assert!(status == 500 && answer.contains("world.json"), "{status} {answer}");
        let (_, s) = request(&mut game, "GET", "/api/status", &json!({}), &ctx);
        let last = serde_json::from_str::<Value>(&s).unwrap()["lastSave"].clone();
        assert!(last["ok"] == false && last["at"].as_u64() > Some(0) && last["error"].as_str().is_some_and(|e| e.contains("world.json")), "{last}");
        // The autosave writes on a thread: its failure comes back as a retry at the next one.
        game.advanced = true;
        autosave(&mut game, &path);
        settle(&mut game, true);
        assert!(game.advanced, "the next autosave tries again");
        let (_, s) = request(&mut game, "GET", "/api/status", &json!({}), &ctx);
        assert_eq!(serde_json::from_str::<Value>(&s).unwrap()["lastSave"]["ok"], false);
        assert_eq!(final_save(&mut game, &path), 1);
        let kept = std::fs::read(crate::front::snapshot_path(&path)).unwrap();
        let tables = crate::test_data().unwrap();
        let (n, d, s) = crate::tables(tables);
        let scenes = crate::scene_tables(tables, &n, &d);
        let back = crate::handoff::restore_bytes(n, d, s, &kept, scenes).unwrap();
        assert_eq!(back.wos.len(), game.wos.len());
        // Written: the status says so.
        std::fs::remove_dir(&path).unwrap();
        game.advanced = true;
        autosave(&mut game, &path);
        settle(&mut game, true);
        assert!(!game.advanced);
        let (_, s) = request(&mut game, "GET", "/api/status", &json!({}), &ctx);
        assert_eq!(serde_json::from_str::<Value>(&s).unwrap()["lastSave"]["ok"], true);
        assert_eq!(final_save(&mut game, &path), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The status carries the tick times.
    #[test]
    fn the_status_carries_the_tick_times() {
        let mut t = TickTimes::default();
        let start = Instant::now();
        t.record(start, Duration::from_micros(40));
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let ctx = Ctx { ticks: &t, save_path: "", build_id: None, data_dir: "", loop_rate: 0, busy: Default::default() };
        let (_, status) = request(&mut game, "GET", "/api/status", &json!({}), &ctx);
        let status: Value = serde_json::from_str(&status).unwrap();
        assert_eq!(status["tick"]["windowSeconds"], 60);
        assert!(status["tick"]["p50Micros"].is_number() || status["tick"]["samples"] == 0);
    }
}
