//! How the bank gets its world, at START or at RESTORE, and the files beside it: the save (checked
//! before anything loads), the rules and access files (a file that does not hold up stops the
//! start; a restored game takes both again from their files), the folders that hold the world's
//! backups. `enforce` kicks everyone the access lists no longer let in.

use std::time::Duration;

use super::persist::note_backup;
use crate::game::Game;
use crate::ipc::{self, Frame};
use crate::msg::{self, Restore};
use crate::save::SaveFile;
use crate::{access, config, rules, save};

/// The world from the save, as a start of the whole server loads it. Exits on what stops a
/// start (no save, an old save format, rules that do not hold up).
pub(super) fn load(dir: &str, save_path: &str, net_data: crate::data::NetData, data: crate::gamedata::GameData, spec: crate::rpcspec::Spec, scenes: crate::Scenes) -> Game {
    let save = SaveFile::load(save_path).unwrap_or_else(|e| {
        match e.kind() {
            std::io::ErrorKind::NotFound => eprintln!("no save {save_path}: make a new world with `pcds new-save {save_path} {dir}`"),
            _ => eprintln!("{save_path}: {e}"),
        }
        std::process::exit(1)
    });
    if let Err(e) = crate::game::check_save_content(&save, &data) {
        eprintln!("{save_path}: {e}");
        std::process::exit(1);
    }
    // A rules or access file that does not hold up stops the start, before the world loads.
    let rules_path = rules_path(save_path);
    let loaded_rules = rules::Rules::load(&rules_path).and_then(|r| r.validate(&data).map(|_| r)).unwrap_or_else(|e| {
        eprintln!("rules: {e}");
        std::process::exit(1)
    });
    let loaded_access = load_access(&access_path(save_path)).unwrap_or_else(|e| {
        eprintln!("access: {e}");
        std::process::exit(1)
    });
    let folder = std::path::Path::new(save_path).parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    save::sweep(folder);
    save::sweep(&folder.join("backups"));
    // The save as it was before this server touched it (as every backup: none with save.backupsKept 0).
    // At most one per 10 minutes: a crash loop must not push the hourly backups out.
    note_backup(&save::backup(save_path, Duration::from_secs(600), loaded_rules.save.backups_kept as usize).map_err(|e| format!("{save_path}: {e}")));
    let session_number = config::env_number("PCDS_SESSION") as u32;
    let version = net_data.game_version.clone();
    let mut game = Game::new(net_data, data, spec, save, scenes);
    // A world on a DLC planet needs that planet's ground in the tables: not silently empty.
    if let Some(why) = crate::planets::ground_missing(&game, game.planet) {
        eprintln!("{save_path}: the world cannot start: {why}");
        std::process::exit(1);
    }
    rules::start(&mut game, loaded_rules, rules_path);
    game.access = loaded_access;
    game.session_number = session_number;
    println!("world: {} world objects, {} network objects, planet {}, game {version}", game.wos.len(), game.net.objects.len(), game.planet_id());
    game
}

pub(super) fn rules_path(save_path: &str) -> String {
    config::installation_file(&crate::env, save_path, "rules.json")
}

pub(super) fn access_path(save_path: &str) -> String {
    config::installation_file(&crate::env, save_path, "access.json")
}

/// The access lists from their file (none: empty lists). Err for a file that is there and does
/// not read: empty lists in its place would let in everyone it bans.
fn load_access(path: &str) -> Result<access::Access, String> {
    match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => {
            serde_json::from_str::<access::Lists>(&text).map_err(|e| e.to_string()).and_then(|l| l.check()).map_err(|e| format!("{path}: {e}"))?;
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{path}: {e}")),
    }
    Ok(access::Access::load(Some(path.to_string())))
}

/// A restored game's access lists and rules from their files, which every change is written to
/// before it is made: the snapshot's may be older (a ban or a rules change after the last flip,
/// then a crash and a restart from that flip's snapshot).
pub(super) fn reread(game: &mut Game, access_path: &str, rules_path: &str) -> Result<(), String> {
    game.access = load_access(access_path)?;
    let rules = rules::Rules::load(rules_path).and_then(|r| r.validate(&game.data).map(|_| r))?;
    game.rules_path = Some(rules_path.to_string());
    if rules != game.rules {
        rules::set(game, rules)?;
    }
    enforce(game);
    Ok(())
}

/// Kicks everyone the access lists no longer let in.
pub fn enforce(game: &mut Game) {
    let refused: Vec<u64> = game
        .clients
        .values()
        .filter(|c| {
            let ip_refused = c.verified_steam_id.is_none() && c.ip.as_deref().is_some_and(|ip| !game.access.ip_allowed(ip));
            let name_refused = c.ready.as_ref().is_some_and(|s| !game.access.name_allowed(&s.name, c.verified_steam_id));
            ip_refused || name_refused
        })
        .map(|c| c.id)
        .collect();
    for c in refused {
        game.kick(c, access::REFUSED);
    }
}

/// How the check of a restored state came out (PCDS_DEV=flip-verify, tests): `verified` is true,
/// or null when it did not run, and `ms` what it took.
pub(super) struct Verified {
    pub verified: Option<bool>,
    pub ms: f64,
}

/// The world from the snapshot of the bank before this one (RESTORE), as that bank left it: with
/// the access lists and the rules from their files, which are newer than it if anything changed
/// since. After a crash (`recover`) the clients in it are gone. Exits on a snapshot that does not
/// restore (the front then tries the old bank again).
pub(super) fn restore(f: &Frame, net_data: crate::data::NetData, data: crate::gamedata::GameData, spec: crate::rpcspec::Spec, scenes: crate::Scenes, save_path: &str) -> (Game, Verified) {
    let (mut verified, mut verify_ms) = (None, 0.0);
    let recover = msg::parse::<Restore>(&f.body).is_ok_and(|b| b.recover);
    let snapshot = f.fd.as_ref().ok_or("no memfd".to_string()).and_then(|fd| ipc::map_memfd(fd).map_err(|e| e.to_string()));
    let restored = snapshot.and_then(|s| crate::handoff::restore_bytes(net_data, data, spec, &s, scenes).map(|g| (g, s)));
    match restored {
        Ok((mut g, snapshot)) => {
            if config::dev("flip-verify").is_some() {
                let t = std::time::Instant::now();
                if let Err(e) = crate::handoff::verify(&g, &snapshot) {
                    eprintln!("bank: {e}");
                    std::process::exit(3);
                }
                verified = Some(true);
                verify_ms = t.elapsed().as_secs_f64() * 1000.0;
            }
            if let Err(e) = reread(&mut g, &access_path(save_path), &rules_path(save_path)) {
                eprintln!("bank: restore failed: {e}");
                std::process::exit(3);
            }
            if recover {
                // After a crash: the clients in it are gone (the front dropped them).
                let clients: Vec<u64> = g.clients.keys().copied().collect();
                clients.into_iter().for_each(|c| g.disconnect(c));
            }
            (g, Verified { verified, ms: verify_ms })
        }
        Err(e) => {
            eprintln!("bank: restore failed: {e}");
            std::process::exit(3);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game;

    /// A ban and a rules change written after a snapshot are in the game restored from it.
    #[test]
    fn a_restored_game_takes_access_and_rules_from_their_files() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let dir = std::env::temp_dir().join(format!("pcds-bank-reread-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (access, rules) = (dir.join("access.json").to_string_lossy().to_string(), dir.join("rules.json").to_string_lossy().to_string());
        std::fs::write(&access, r#"{"bannedNames": ["Mallory"]}"#).unwrap();
        std::fs::write(&rules, r#"{"speed": {"growth": 2}}"#).unwrap();
        reread(&mut game, &access, &rules).unwrap();
        assert_eq!((game.access.lists.banned_names.clone(), game.rules.speed.growth), (vec!["Mallory".to_string()], 2.0));
        assert!(!game.access.name_allowed("mallory", None));
        // A file that does not read is an error, not empty lists.
        std::fs::write(&access, r#"{"bannedNames": "#).unwrap();
        assert!(reread(&mut game, &access, &rules).unwrap_err().contains("access.json"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
