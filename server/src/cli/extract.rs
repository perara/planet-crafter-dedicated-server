//! Reading the game tables into run/data/<game version>/, straight from the game's files
//! (native/): network tables, game data, every free planet's scenes and the wreck layouts.

use std::fs;
use std::path::{Path, PathBuf};

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
    /// The game's tables into run/data/<game version>/.
    pub fn extract(&self, game: Option<&Path>) -> Result {
        let game = self.game_dir(game)?;
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
        say(format!("reading the game tables from {}", game.display()));
        let version = crate::native::tables::read_game(&game, &out.0)?;
        let tables = self.run.join(format!("data/{version}"));
        let _ = fs::remove_dir_all(&tables);
        fs::rename(&out.0, &tables).map_err(|e| format!("{}: {e}", tables.display()))?;
        say(format!("game tables for {version} in {}", tables.display()));
        Ok(())
    }

    /// After a game update: the new build's tables, and the server restarted on them.
    pub fn update(&self, game: Option<&Path>) -> Result {
        self.update_downloaded_game();
        let game = self.game_dir(game)?;
        let build = build_id(&game)?;
        if self.tables(&build).is_ok() {
            say(format!("up to date (game build {build})"));
            return Ok(());
        }
        say(format!("game build {build} has no tables yet"));
        self.extract(Some(&game))?;
        self.restart_after_update()
    }
}
