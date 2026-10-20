//! Reading the game tables into run/data/<game version>/, straight from the game's files
//! (native/): network tables, game data, every planet's scenes (the DLC planets' too) and the wreck layouts. The
//! reader is the program that runs the server (the active bank's `pcds read-game`), so the
//! tables are always in the format the server loads.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::env::build_id;
use super::{say, Ctx, Result};

/// The tables being made: removed unless they were finished (moved into place).
struct Unfinished(PathBuf);

impl Drop for Unfinished {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `new` becomes `tables`, in one step: where `tables` exists the two are exchanged (renameat2
/// RENAME_EXCHANGE), so a bank that loads its tables at that moment finds the old ones or the
/// new ones, never a folder that is missing or half there; the old ones are then in `new`. Where
/// there are none, a plain rename.
fn swap_in(new: &Path, tables: &Path) -> Result {
    let error = |e: std::io::Error| format!("{}: {e}", tables.display());
    if fs::symlink_metadata(tables).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
        return fs::rename(new, tables).map_err(error);
    }
    exchange(new, tables).map_err(|e| match e.raw_os_error() {
        Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP) => format!("{}: this file system cannot exchange two folders in one step ({e}): move run/ to one that can (ext4, xfs, btrfs, tmpfs), or remove {} and read the tables again", tables.display(), tables.display()),
        _ => error(e),
    })
}

fn exchange(a: &Path, b: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput));
    let (a, b) = (c(a)?, c(b)?);
    // SAFETY: renameat2 with two NUL-terminated paths that live through the call.
    let done = unsafe { libc::syscall(libc::SYS_renameat2, libc::AT_FDCWD, a.as_ptr(), libc::AT_FDCWD, b.as_ptr(), libc::RENAME_EXCHANGE) };
    if done == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

impl Ctx {
    /// The game's tables into run/data/<game version>/, read by program().
    pub fn extract(&self, game: Option<&Path>) -> Result {
        let game = self.game_dir(game)?;
        let build = build_id(&game)?;
        super::mkdir(&self.run.join("data"))?;
        // One extraction at a time (the hourly update and a manual one).
        let _lock = crate::util::hold(&self.run.join(".extract.lock"), "another extraction is running")?;
        // Earlier extractions that failed (none runs: the lock is ours).
        for e in fs::read_dir(self.run.join("data")).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().starts_with(".new-") {
                let _ = fs::remove_dir_all(e.path());
            }
        }
        // Hidden until complete: tables() only picks finished versions.
        let out = Unfinished(self.run.join(format!("data/.new-{}", std::process::id())));
        let program = self.program()?;
        say(format!("reading the game tables from {} with {}", game.display(), program.display()));
        super::check(Command::new(&program).arg("read-game").arg(&game).arg(&out.0).stdout(Stdio::null()))?;
        let netdata = out.0.join("netdata.json");
        let text = fs::read_to_string(&netdata).map_err(|e| format!("{}: {e}", netdata.display()))?;
        let version: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", netdata.display()))?;
        let version = version["gameVersion"].as_str().filter(|v| crate::worlds::arg(v).is_ok_and(|name| name == *v)).ok_or_else(|| format!("{}: no gameVersion to name the folder by", netdata.display()))?.to_string();
        let tables = self.run.join(format!("data/{version}"));
        // The old tables of this version (a re-read) end up in `out`, which is removed.
        swap_in(&out.0, &tables)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Re-reading the tables of a version replaces its folder while a bank may be loading it:
    /// a reader that opens the folder at any moment finds the old tables or the new ones whole,
    /// never no folder, never an empty one, and what it reads through that open folder is one
    /// version's. (A reader that opens its files by path one after the other can still see the
    /// swap between two of them; a swap is rare and a load takes seconds.)
    #[test]
    fn tables_are_replaced_in_one_step() {
        let dir = super::super::test_dir("swap-tables");
        let tables = dir.join("2.103");
        let made = |name: &str, content: &str| {
            let d = dir.join(name);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("netdata.json"), content).unwrap();
            fs::write(d.join("gamedata.json"), content).unwrap();
            d
        };
        // None there yet: a plain move.
        swap_in(&made(".new-1", "v0"), &tables).unwrap();
        assert_eq!(fs::read_to_string(tables.join("netdata.json")).unwrap(), "v0");
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let reader = {
            let (done, tables) = (done.clone(), tables.clone());
            std::thread::spawn(move || {
                let mut seen = std::collections::BTreeSet::new();
                while !done.load(Ordering::SeqCst) {
                    // The folder is opened once: what it holds is one version's.
                    let d = fs::File::open(&tables).unwrap_or_else(|e| panic!("no tables folder at some moment: {e}"));
                    let path = format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&d));
                    let a = fs::read_to_string(format!("{path}/netdata.json")).unwrap_or_else(|e| panic!("a table is missing: {e}"));
                    let b = fs::read_to_string(format!("{path}/gamedata.json")).unwrap_or_else(|e| panic!("a table is missing: {e}"));
                    assert_eq!(a, b, "tables of two versions in one folder");
                    seen.insert(a);
                }
                seen
            })
        };
        // The old tables end up in the folder the caller removes; here they stay until the reader
        // is done (it holds one of them open: a reader that opened the old folder just before a
        // swap would else find it gone).
        let mut olds = Vec::new();
        for round in 1..=300 {
            let content = format!("v{round}");
            let new = made(&format!(".new-{}", round + 1), &content);
            swap_in(&new, &tables).unwrap();
            assert_eq!(fs::read_to_string(new.join("netdata.json")).unwrap(), format!("v{}", round - 1));
            olds.push(new);
        }
        done.store(true, Ordering::SeqCst);
        let seen = reader.join().unwrap();
        olds.iter().for_each(|d| fs::remove_dir_all(d).unwrap());
        assert!(seen.len() > 1, "the reader saw the tables change: {seen:?}");
        assert_eq!(fs::read_to_string(tables.join("netdata.json")).unwrap(), "v300");
        fs::remove_dir_all(&dir).unwrap();
    }
}
