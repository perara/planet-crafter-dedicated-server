//! Reading the game tables into run/data/<game version>/, straight from the game's files
//! (native/): network tables, game data, every free planet's scenes and the wreck layouts. The
//! reader is the program that runs the server (the active bank's `pcds read-game`), so the
//! tables are always in the format the server loads.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::env::build_id;
use super::{say, Ctx, Result};

/// One extraction at a time (the hourly update and a manual one).
fn lock(path: &Path) -> Result<fs::File> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let mut file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    // SAFETY: flock on a descriptor this function owns; the lock lasts as long as the file.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let holder = fs::read_to_string(path).unwrap_or_default();
        return Err(format!("another extraction is running (process {}); stop it or wait for it", holder.trim()));
    }
    // The holder, for the message above.
    let _ = file.set_len(0).and_then(|_| write!(file, "{}", std::process::id()));
    Ok(file)
}

/// The tables being made: removed unless they were finished (moved into place).
struct Unfinished(PathBuf);

impl Drop for Unfinished {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

impl Ctx {
    /// The game's tables into run/data/<game version>/, read by program().
    pub fn extract(&self, game: Option<&Path>) -> Result {
        let game = self.game_dir(game)?;
        let build = build_id(&game)?;
        super::mkdir(&self.run.join("data"))?;
        let _lock = lock(&self.run.join(".extract.lock"))?;
        // Earlier extractions that failed (none runs: the lock is ours).
        for e in fs::read_dir(self.run.join("data")).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().starts_with(".new-") {
                let _ = fs::remove_dir_all(e.path());
            }
        }
        // Hidden until complete: tables() only picks finished versions.
        let out = Unfinished(self.run.join(format!("data/.new-{}", std::process::id())));
        let program = self.program();
        say(format!("reading the game tables from {} with {}", game.display(), program.display()));
        super::check(Command::new(&program).arg("read-game").arg(&game).arg(&out.0).stdout(Stdio::null()))?;
        let netdata = out.0.join("netdata.json");
        let text = fs::read_to_string(&netdata).map_err(|e| format!("{}: {e}", netdata.display()))?;
        let version: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", netdata.display()))?;
        let version = version["gameVersion"].as_str().filter(|v| super::env::world_arg(v).is_ok_and(|name| name == *v)).ok_or_else(|| format!("{}: no gameVersion to name the folder by", netdata.display()))?.to_string();
        let tables = self.run.join(format!("data/{version}"));
        let _ = fs::remove_dir_all(&tables);
        fs::rename(&out.0, &tables).map_err(|e| format!("{}: {e}", tables.display()))?;
        say(format!("game tables for {version} in {}", tables.display()));
        // What the server will look for: this build in its tables format.
        self.tables(&build).map(|_| ())
    }

    /// The tables of the game (--game DIR, else the one used last) for the program that runs
    /// the server, read when there are none yet; whether they were read.
    pub fn ensure_tables(&self, game: Option<&Path>) -> Result<bool> {
        let game = self.game_dir(game)?;
        if self.tables(&build_id(&game)?).is_ok() {
            return Ok(false);
        }
        self.extract(Some(&game))?;
        Ok(true)
    }

    /// After a game update: the new build's tables, and the server restarted on them. A game
    /// download that steamcmd could not update fails the command (after the tables of the game
    /// as it is are looked after).
    pub fn update(&self, game: Option<&Path>) -> Result {
        let fetched = self.update_downloaded_game();
        let tables = self.update_tables(game);
        match fetched {
            Err(e) if tables.is_ok() => Err(e),
            Err(e) => {
                eprintln!("pcds: {e}");
                tables
            }
            Ok(()) => tables,
        }
    }

    fn update_tables(&self, game: Option<&Path>) -> Result {
        let game = self.game_dir(game)?;
        let build = build_id(&game)?;
        match self.tables(&build) {
            Ok(_) => {
                say(format!("up to date (game build {build})"));
                return Ok(());
            }
            Err(e) => say(e),
        }
        self.extract(Some(&game))?;
        self.restart_after_update()
    }
}
