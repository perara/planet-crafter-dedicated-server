//! The world's controls: stop it (every player told, the world saved, the bank ended), start it
//! again (or another world) in the active bank, switch to another world. While stopped the front
//! runs on without a bank: joins are refused, and the admin page and API stay, to start a world.
//! The admin routes that do these are answered here (admin_routes.rs names them).

use std::time::Instant;

use serde_json::{json, Value};

use super::admin_routes::error;
use super::bank_link::Bank;
use super::{Front, Phase, STOPPED};
use crate::updater;

/// A world file this server reads (an old layout, a file cut short and the like are refused,
/// each saying what to do), checked before a world that runs is stopped for it.
fn readable(save: &str, name: &str) -> Result<(), String> {
    crate::save::SaveFile::load(save).map(drop).map_err(|e| format!("world {name}: {e}"))
}

impl Front {
    /// POST /api/world/stop
    pub(super) fn answer_world_stop(&mut self) -> (u16, String) {
        match self.stop_world(STOPPED) {
            Ok(()) => (200, json!({"ok": true, "world": self.world_name()}).to_string()),
            Err(e) if self.phase == Phase::Stopped => (500, json!({"error": e, "stopped": true}).to_string()),
            Err(e) => error(409, &e),
        }
    }

    /// POST /api/world/start {"world": NAME}: the world that ran, or another one.
    pub(super) fn answer_world_start(&mut self, body: &Value) -> (u16, String) {
        if !body["world"].is_null() && !body["world"].is_string() {
            return error(400, "'world': a world's name (or leave it out for the world that ran)");
        }
        let name = body["world"].as_str().map(str::to_string);
        match self.start_world(name.as_deref()) {
            Ok(w) => (200, json!({"ok": true, "world": w}).to_string()),
            Err(e) => error(409, &e),
        }
    }

    /// POST /api/world/switch {"world": NAME}: another world; the players told and let go, this
    /// one saved, that one started.
    pub(super) fn answer_world_switch(&mut self, body: &Value) -> (u16, String) {
        let Some(name) = body["world"].as_str().map(str::to_string) else { return error(400, "'world' is required") };
        if name == self.world_name() && self.phase != Phase::Stopped {
            return error(409, &format!("{name} is the world that runs"));
        }
        if !crate::worlds::file(&self.save, &name).is_file() || crate::worlds::name(&name).is_err() {
            return error(400, &format!("no world {name}"));
        }
        if let Err(e) = readable(&crate::worlds::file(&self.save, &name).to_string_lossy(), &name) {
            // Before the world that runs is stopped for it.
            return error(400, &e);
        }
        let stopped = if self.phase == Phase::Stopped { Ok(()) } else { self.stop_world(&format!("The server moves to another world ({name}): join again")) };
        match stopped.and_then(|()| self.start_world(Some(&name))) {
            Ok(w) => (200, json!({"ok": true, "world": w}).to_string()),
            Err(e) => (500, json!({"error": e, "stopped": self.phase == Phase::Stopped}).to_string()),
        }
    }

    /// Every player told `why` and let go, the world saved and its bank ended: Stopped.
    pub(super) fn stop_world(&mut self, why: &str) -> Result<(), String> {
        if let Some((_, doing)) = &self.updates.busy {
            return Err(format!("an update is under way ({doing}): wait for it"));
        }
        if self.phase != Phase::Idle || self.live.is_none() {
            return Err(format!("the world cannot stop now ({})", if self.live.is_none() { "it is not running".to_string() } else { self.phase.label().to_string() }));
        }
        let reason = crate::bank::reason_batch(why);
        self.dismiss_all(&reason, Instant::now());
        self.net.flush(true);
        self.queue.clear();
        self.admin.refuse_answers(503, "the world stopped");
        let saved = self.stop_live();
        // The save has the world now (or the bank kept it beside the save): the last flip's
        // snapshot is older.
        self.last_snapshot = None;
        self.recover_from = None;
        self.enter(Phase::Stopped);
        println!("world {}: stopped by the admin", self.world_name());
        saved
    }

    /// The name of the world that runs (or ran): its save's file name without the extension.
    pub(super) fn world_name(&self) -> String {
        crate::worlds::stem(&self.save)
    }

    /// The world (`name`, else the one it ran) started in the active bank: Idle. A world other
    /// than the last one takes this server's lock of it and is remembered in server.env
    /// (PCDS_WORLD), so the next start runs it too; but only once it runs: one that does not
    /// start leaves the server as it was (stopped, on the world it stopped), and server.env
    /// naming the world that last ran, so a restart does not fail on it.
    pub(super) fn start_world(&mut self, name: Option<&str>) -> Result<String, String> {
        if self.phase != Phase::Stopped {
            return Err("the world runs: stop it first, or run another one".into());
        }
        let mut moved = None;
        if let Some(name) = name.filter(|n| *n != self.world_name()) {
            let name = crate::worlds::name(name)?;
            let save = crate::worlds::file(&self.save, &name).to_string_lossy().to_string();
            if !std::path::Path::new(&save).is_file() {
                return Err(format!("no world {name}"));
            }
            readable(&save, &name)?;
            let lock = crate::cli::hold(std::path::Path::new(&format!("{save}.lock")), "another server runs that world")?;
            moved = Some((std::mem::replace(&mut self.save, save), self.world_lock.replace(lock), name));
        }
        let started = self.start_bank();
        match (started, moved) {
            (Err(e), Some((save, lock, name))) => {
                (self.save, self.world_lock) = (save, lock);
                Err(format!("world {name} did not start: {e}; the server stays stopped on world {}: start it, or run another world", self.world_name()))
            }
            (Ok(world), Some((_, _, name))) => {
                if let Some(home) = crate::env("PCDS_HOME")
                    && let Err(e) = crate::cli::set_env_file(&std::path::Path::new(&home).join("server.env"), "PCDS_WORLD", &name)
                {
                    eprintln!("world {name}: not remembered for the next start ({e})");
                }
                Ok(world)
            }
            (started, None) => started,
        }
    }

    /// The world of `self.save` booted in the active bank: Idle.
    fn start_bank(&mut self) -> Result<String, String> {
        let bank = self.banks.as_deref().map(updater::active).unwrap_or_else(|| "a".into());
        let mut kept = self.kept_snapshot()?;
        let snapshot = kept.as_ref().map(|(fd, _)| fd.try_clone()).transpose().map_err(|e| e.to_string())?;
        let b = self.boot_bank(&bank, snapshot, &mut kept)?;
        if let Some((fd, at)) = kept {
            self.last_snapshot = Some((fd, at));
        }
        self.game_version = b.game_version().to_string();
        self.admin.set_page(self.web_page(&bank));
        self.live = Some(Bank { heard: Instant::now(), ..b });
        self.flipped = None;
        self.enter(Phase::Idle);
        self.autosave.restart();
        let name = self.world_name();
        println!("world {name}: running (bank {bank}), started by the admin");
        Ok(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    /// A world that does not start leaves the server stopped on the world it stopped, with its
    /// lock, and server.env naming that world (a restart would fail on the other).
    #[test]
    fn a_world_that_does_not_start_is_not_remembered() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pcds-start-world-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("banks/a")).unwrap();
        let program = dir.join("banks/a/pcds");
        std::fs::write(&program, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (a, b, env) = (dir.join("a.json"), dir.join("b.json"), dir.join("server.env"));
        std::fs::write(&a, "{}").unwrap();
        std::fs::write(&b, crate::save::SaveFile::tiny_text("b")).unwrap();
        std::fs::write(dir.join("c.json"), crate::save::SaveFile::old_format_text()).unwrap();
        std::fs::write(&env, "PCDS_WORLD=a\n").unwrap();
        // SAFETY: only start_world reads it, and no other test calls that.
        unsafe { std::env::set_var("PCDS_HOME", &dir) };
        let mut front = Front::new("data", &a.to_string_lossy(), Some(dir.join("banks")), None, 0, Arc::new(AtomicBool::new(false)));
        front.phase = Phase::Stopped;
        front.world_lock = Some(crate::cli::hold(&dir.join("a.json.lock"), "busy").unwrap());
        // A world in an old layout is refused before any bank is started or lock taken.
        let e = front.start_world(Some("c")).unwrap_err();
        assert!(e.contains("world c") && e.contains("pcds world import") && !dir.join("c.json.lock").exists(), "{e}");
        let e = front.start_world(Some("b")).unwrap_err();
        assert!(e.contains("world b did not start") && e.contains("stays stopped on world a"), "{e}");
        assert_eq!((front.world_name(), front.phase), ("a".to_string(), Phase::Stopped));
        assert_eq!(std::fs::read_to_string(&env).unwrap(), "PCDS_WORLD=a\n");
        // The lock of a is still this server's; b's is free again.
        assert!(crate::cli::hold(&dir.join("a.json.lock"), "busy").is_err());
        assert!(crate::cli::hold(&dir.join("b.json.lock"), "busy").is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
