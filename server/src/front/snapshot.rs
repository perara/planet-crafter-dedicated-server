//! The snapshot the front keeps for a world that is in no bank: the last flip's, which a crashed
//! bank restarts from when it is newer than the save, and which the front leaves beside the save
//! (`<save>.snapshot`) when it stops with no bank to save the world.

use std::os::fd::OwnedFd;
use std::path::Path;
use std::time::SystemTime;

use super::Front;
use crate::ipc;
use crate::msg::{self, Hello, SnapshotHead};

use crate::worlds::snapshot_path;

/// Why a snapshot can never run on the bank of this HELLO: it is on another game's tables, in
/// a format the bank does not read, or no snapshot at all. None: it may (any other error is the
/// bank's to tell).
pub(super) fn never_runs(snapshot: &[u8], hello: &Hello) -> Option<String> {
    let head = snapshot.split(|b| *b == b'\n').next().and_then(|h| msg::parse::<SnapshotHead>(h).ok());
    let Some(head) = head else { return Some("not a snapshot".into()) };
    if hello.game_version.as_ref() != Some(&head.game_version) {
        return Some(format!("it is on game {}'s tables, the bank has {}'s", msg::json(&head.game_version), msg::json(&hello.game_version)));
    }
    if hello.info.format != Some(head.format) {
        return Some(format!("snapshot format {}, the bank reads {}", head.format, msg::json(&hello.info.format)));
    }
    None
}

impl Front {
    /// The snapshot keep_snapshot() left beside the save, when it is newer than the save (an
    /// older one is removed: the save has its world).
    pub(super) fn kept_snapshot(&self) -> Result<Option<(OwnedFd, SystemTime)>, String> {
        let path = snapshot_path(&self.save);
        let Ok(at) = std::fs::metadata(&path).and_then(|m| m.modified()) else { return Ok(None) };
        if at <= crate::util::modified(Path::new(&self.save)) {
            std::fs::remove_file(&path).map_err(|e| format!("{path}: {e}"))?;
            info!("removed {path}: the save is newer");
            return Ok(None);
        }
        let bytes = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
        let fd = ipc::memfd("pcds-snapshot", |w| std::io::Write::write_all(w, &bytes).map_err(|e| e.to_string())).map_err(|e| format!("{path}: {e}"))?;
        Ok(Some((fd, at)))
    }

    /// The world is in no bank (the live one died, the next one is not running it yet): the
    /// last flip's snapshot, when it is newer than the save, is kept beside the save, with the
    /// snapshot's time; the next start runs it.
    pub(super) fn keep_snapshot(&self) {
        let Some((fd, at)) = &self.last_snapshot else { return };
        if *at <= crate::util::modified(Path::new(&self.save)) {
            return;
        }
        let path = snapshot_path(&self.save);
        let kept = ipc::read_memfd(fd)
            .map_err(|e| e.to_string())
            .and_then(|bytes| crate::util::replace_file(std::path::Path::new(&path), &bytes, 0o600))
            .and_then(|_| std::fs::File::options().write(true).open(&path).and_then(|f| f.set_modified(*at)).map_err(|e| e.to_string()));
        match kept {
            Ok(()) => warn!("the world is in no bank: the last flip's snapshot (newer than the save) is kept in {path}; the next start runs it"),
            Err(e) => error!("the world is in no bank, and the last flip's snapshot could not be kept in {path} ({e}): the next start runs the save, which is older"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A kept snapshot of other tables, of a format the bank does not read, or none at all can
    /// never run; one the bank may run goes to it (its own errors stop the start).
    #[test]
    fn a_snapshot_that_can_never_run_is_told_apart() {
        let hello: Hello = serde_json::from_value(serde_json::json!({"gameVersion": "2.103", "format": 1})).unwrap();
        assert_eq!(never_runs(b"{\"format\":1,\"game_version\":\"2.103\"}\n{\"broken", &hello), None);
        for bad in [&b"{\"format\":1,\"game_version\":\"0.0.1\"}\n{}"[..], b"{\"format\":7,\"game_version\":\"2.103\"}\n{}", b"garbage"] {
            assert!(never_runs(bad, &hello).is_some(), "{}", String::from_utf8_lossy(bad));
        }
    }
}
