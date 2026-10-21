//! How a world gets to disk: the one owner of saving. The game thread makes the text of a save
//! (`persist`, `persist_later`); a thread of its own backs up the file before it and writes the
//! new one (an autosave, the front's timer); the end of a save is handled on the game thread again
//! (`saved`: the status, the log, the S3 hand-off of the newest backup, the plugins). When the
//! bank ends and the file cannot be written, `final_save` keeps the world as a snapshot beside it.
//! What a panic left behind is kept as <save>.before-panic first (panic.rs). The status reads the
//! outcomes (`last_save`, `backups`).

use std::time::Duration;

use serde::Serialize;

use crate::game::Game;
use crate::save;

/// The last save's outcome, for GET /api/status: {"ok", "at" (Unix seconds), "error"}.
#[derive(Clone, Serialize)]
pub struct LastSave {
    pub ok: bool,
    pub at: u64,
    pub error: Option<String>,
}

/// A save is written in two parts: the game thread makes the text (the world as it is now: one
/// ms for a 2.5k-object world, 36 for 100k objects) and a thread of its own makes the backup
/// and writes the file (copies, fsyncs, a rename: tens to hundreds of ms on a busy disk), so
/// the world does not stand still for the disk.
struct Writing {
    path: String,
    done: std::sync::mpsc::Receiver<Written>,
    /// When the text was made: the save's duration is measured from here (GET /api/metrics).
    started: std::time::Instant,
}

/// What writing a save came to: the file, and the backup made before it (Ok(false): none was due).
struct Written {
    saved: Result<(), String>,
    backup: Result<bool, String>,
}

thread_local! {
    /// The last save's outcome, for GET /api/status: {"ok", "at" (Unix seconds), "error"}. The
    /// game loop's thread saves and answers the API.
    static LAST_SAVE: std::cell::RefCell<Option<LastSave>> = const { std::cell::RefCell::new(None) };
    /// The autosave being written, if any (at most one: a save never overtakes an older one).
    static WRITING: std::cell::RefCell<Option<Writing>> = const { std::cell::RefCell::new(None) };
    /// The backups' outcome, for GET /api/status and POST /api/save: when one was last made, and
    /// the last failure (cleared by the next backup made).
    static BACKUPS: std::cell::RefCell<(Option<u64>, Option<(u64, String)>)> = const { std::cell::RefCell::new((None, None)) };
    /// How long the last save took, in milliseconds (GET /api/metrics).
    static SAVE_MILLIS: std::cell::Cell<Option<f64>> = const { std::cell::Cell::new(None) };
}

/// How long the last save took, from its text being made to the file being on disk.
pub(super) fn save_millis() -> Option<f64> {
    SAVE_MILLIS.get()
}

/// The last save's outcome (None before the first save of this process).
pub(super) fn last_save() -> Option<LastSave> {
    LAST_SAVE.with_borrow(Clone::clone)
}

/// The backups' outcome: when one was last made since this process started, and the last failure
/// with its time.
pub(super) fn backups() -> (Option<u64>, Option<(u64, String)>) {
    BACKUPS.with_borrow(Clone::clone)
}

pub(super) fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A backup attempt's outcome kept for the status (and logged when it failed).
pub(super) fn note_backup(result: &Result<bool, String>) {
    match result {
        Ok(true) => BACKUPS.set((Some(unix_now()), None)),
        Ok(false) => {}
        Err(e) => {
            error!("backup failed: {e}");
            BACKUPS.with_borrow_mut(|b| b.1 = Some((unix_now(), e.clone())));
        }
    }
}

/// How long a test's save writes take (a slow disk).
#[cfg(test)]
pub static WRITE_DELAY_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The backup and the write of a save's text (what persist does, in its order); `files` are the
/// rules of the backups.
/// `after_panic`: the file on disk is first kept as <path>.before-panic (taint).
fn write_save(path: &str, text: &[u8], files: &crate::rules::Save, after_panic: bool) -> Written {
    #[cfg(test)]
    std::thread::sleep(Duration::from_millis(WRITE_DELAY_MS.load(std::sync::atomic::Ordering::SeqCst)));
    if after_panic {
        let before = format!("{path}.before-panic");
        match std::fs::read(path).map_err(|e| e.to_string()).and_then(|bytes| save::write_atomic(&before, &bytes).map_err(|e| e.to_string())) {
            Ok(()) => warn!("the world is saved after a panic; the save before it is kept as {before}"),
            Err(e) if !std::path::Path::new(path).exists() => error!("a panic was contained, and there is no save yet to keep ({e})"),
            Err(e) => error!("the save before a panic could not be kept as {before} ({e}); it is replaced by the save that follows"),
        }
    }
    let backup = save::backup(path, Duration::from_secs(u64::from(files.backup_minutes) * 60), files.backups_kept as usize).map_err(|e| format!("{path}: {e}"));
    Written { saved: save::write_atomic(path, text).map_err(|e| format!("{path}: {e}")), backup }
}

/// What a save's end does on the game thread: the status, the log, the newest backup
/// off-site (on a thread), when S3 is set and it is not there yet, and the plugins: they hear
/// of it and keep their state beside the world's.
fn saved(game: &mut Game, path: &str, written: &Written, took: Duration) {
    let (at, saved) = (unix_now(), &written.saved);
    SAVE_MILLIS.set(Some((took.as_secs_f64() * 1e4).round() / 10.0));
    note_backup(&written.backup);
    if matches!(written.backup, Ok(true)) {
        verify_if_on(game, path);
    }
    LAST_SAVE.set(Some(LastSave { ok: saved.is_ok(), at, error: saved.as_ref().err().cloned() }));
    match saved {
        Ok(()) => info!("path" = path, "millis" = (took.as_secs_f64() * 1e4).round() / 10.0; "saved {path}"),
        Err(e) => error!("save failed: {e}"),
    }
    if let Err(e) = crate::s3::after_backup(&game.rules.backup, path, false) {
        warn!("backup: {e}");
    }
    super::plugins_glue::world_saved(game, saved.is_ok());
}

/// What reading the newest backup back came to (the rule save.verifyBackups), for the metrics.
#[derive(Clone, Serialize)]
pub struct Verified {
    pub file: String,
    pub ok: bool,
    pub at: u64,
    pub world_objects: usize,
    pub error: Option<String>,
}

static VERIFIED: std::sync::Mutex<Option<Verified>> = std::sync::Mutex::new(None);

/// The last backup that was read back, if the rule is on and one has been.
pub(super) fn verified() -> Option<Verified> {
    VERIFIED.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// A backup file as a restore would read it: the layout, then the content against the tables.
/// Returns its world objects.
fn check_backup(path: &std::path::Path, data: &crate::gamedata::GameData, net: &crate::data::NetData) -> Result<usize, String> {
    let save = save::SaveFile::load_deferred(&path.to_string_lossy()).map_err(|e| e.to_string())?;
    crate::game::check_save_content(&save, data, net)?;
    save.check_entries()?;
    Ok(save.object_count())
}

/// After a backup was made: it is read back when the rule save.verifyBackups is on. The newest
/// backup of the world at `save_path` is read on a thread of its own (the loop never waits for
/// the disk): the outcome is logged and kept for the metrics.
pub(super) fn verify_if_on(game: &Game, save_path: &str) {
    if !game.rules.save.verify_backups {
        return;
    }
    let Some(file) = crate::s3::newest_backup(save_path) else { return };
    let (data, net) = (game.data.clone(), game.net_data.clone());
    let name = file.file_name().unwrap_or_default().to_string_lossy().to_string();
    let spawned = std::thread::Builder::new().name("pcds-verify".into()).spawn(move || {
        let result = check_backup(&file, &data, &net);
        match &result {
            Ok(n) => info!("file" = name, "worldObjects" = n; "backup {name} reads back: {n} world objects"),
            Err(e) => error!("file" = name; "backup {name} does NOT load: {e}; restore an older one with pcds backup restore, after pcds backup verify finds one that loads"),
        }
        *VERIFIED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Verified { file: name, ok: result.is_ok(), at: unix_now(), world_objects: *result.as_ref().unwrap_or(&0), error: result.err() });
    });
    if let Err(e) = spawned {
        warn!("backup verification: no thread ({e})");
    }
}

/// The save being written, once it has ended (`wait`: waiting for it). A failed one is tried
/// again at the next autosave.
pub(super) fn settle(game: &mut Game, wait: bool) {
    let Some(w) = WRITING.take() else { return };
    let result = if wait {
        w.done.recv().ok()
    } else {
        match w.done.try_recv() {
            Ok(r) => Some(r),
            Err(std::sync::mpsc::TryRecvError::Empty) => return WRITING.set(Some(w)),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => None,
        }
    };
    let result = result.unwrap_or_else(|| Written { saved: Err(format!("{}: the thread writing it ended", w.path)), backup: Ok(false) });
    saved(game, &w.path, &result, w.started.elapsed());
    if result.saved.is_err() {
        game.advanced = true;
    }
}

/// A save written beside the game: the text now, the backup and the file by a thread. Nothing
/// when one is being written (the caller asks again at its next turn).
pub(super) fn persist_later(game: &mut Game, path: &str) -> bool {
    settle(game, false);
    if WRITING.with_borrow(Option::is_some) {
        return false;
    }
    let started = std::time::Instant::now();
    let text = game.save_text();
    let (tx, done) = std::sync::mpsc::channel();
    let (to, files, after_panic) = (path.to_string(), game.rules.save.clone(), super::panic::take_taint());
    let spawned = std::thread::Builder::new().name("pcds-save".into()).spawn(move || {
        let r = std::panic::catch_unwind(|| write_save(&to, text.as_bytes(), &files, after_panic)).unwrap_or_else(|_| Written { saved: Err(format!("{to}: the write panicked")), backup: Ok(false) });
        let _ = tx.send(r);
    });
    match spawned {
        Ok(_) => WRITING.set(Some(Writing { path: path.to_string(), done, started })),
        Err(e) => {
            // No thread to write in: the world stands still for it, as it always did.
            warn!("save: no thread ({e}); writing here");
            return persist(game, path).is_ok();
        }
    }
    true
}

/// A save made and written before this returns (the admin's, a panic's, the last one). First
/// the one being written, which it must not be overtaken by.
pub(super) fn persist(game: &mut Game, path: &str) -> Result<(), String> {
    settle(game, true);
    let started = std::time::Instant::now();
    let written = write_save(path, game.save_text().as_bytes(), &game.rules.save.clone(), super::panic::take_taint());
    saved(game, path, &written, started.elapsed());
    written.saved
}

/// The front's autosave. A world that stood still since the last one (rules:
/// world.pauseWhenEmpty) is that save already: no write, and no backup that only repeats the
/// one before. A save that failed is tried again at the next one. The text is made here and
/// written by a thread (persist_later); a write still going holds this one back.
pub(super) fn autosave(game: &mut Game, path: &str) {
    settle(game, false);
    if std::mem::take(&mut game.advanced) && !persist_later(game, path) {
        game.advanced = true;
    }
}

/// The save before the bank ends, and the code it ends with. When the save fails (a full disk,
/// a folder made read-only) the world is kept as a snapshot beside it, where the front keeps
/// one (front::kept_snapshot: the next start runs it), else in the temporary folder; the code
/// is 1, and the front says so.
pub(super) fn final_save(game: &mut Game, path: &str) -> i32 {
    let Err(e) = persist(game, path) else { return 0 };
    let beside = crate::worlds::snapshot_path(path);
    let name = std::path::Path::new(&beside).file_name().unwrap_or_default().to_string_lossy().to_string();
    let elsewhere = std::env::temp_dir().join(format!("pcds-{}-{name}", std::process::id())).to_string_lossy().to_string();
    let mut bytes = Vec::new();
    let kept = crate::handoff::snapshot_to(game, &mut bytes).and_then(|()| {
        let write = |to: &str| save::write_atomic(to, &bytes).map_err(|e| format!("{to}: {e}"));
        write(&beside).map(|()| None).or_else(|b| write(&elsewhere).map(|()| Some(b)))
    });
    match kept {
        Ok(None) => error!("the world could not be saved ({e}): it is kept in {beside}, and the next start runs it"),
        Ok(Some(b)) => error!("the world could not be saved ({e}) nor kept beside it ({b}): it is kept in {elsewhere}; move it to {beside} for the next start to run it"),
        Err(k) => error!("the world could not be saved ({e}) nor kept ({k}): what it did since the last save is lost"),
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game;
    use crate::save::SaveFile;

    /// With the rule save.verifyBackups on, the backup a save makes is read back on a thread of its own
    /// and the outcome is kept; a file that does not load is said so, with the reason.
    #[test]
    fn a_backup_is_read_back_when_the_rule_is_on() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let dir = std::env::temp_dir().join(format!("pcds-verify-backup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("world.json").to_string_lossy().to_string();
        // Off: a backup is made and nothing reads it.
        *VERIFIED.lock().unwrap() = None;
        game.rules.save.backup_minutes = 0;
        assert_eq!(persist(&mut game, &path), Ok(()));
        assert_eq!(persist(&mut game, &path), Ok(()));
        std::thread::sleep(Duration::from_millis(300));
        assert!(verified().is_none() && std::fs::read_dir(dir.join("backups")).unwrap().count() >= 1);
        // On: the next backup is read back.
        game.rules.save.verify_backups = true;
        assert_eq!(persist(&mut game, &path), Ok(()));
        let started = std::time::Instant::now();
        let v = loop {
            if let Some(v) = verified() {
                break v;
            }
            assert!(started.elapsed() < Duration::from_secs(20), "the backup was not read back");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(v.ok && v.error.is_none() && v.world_objects > 0 && v.file.starts_with("world-"), "{}", serde_json::to_string(&v).unwrap());
        // A backup that does not load is told apart, with what is wrong.
        let bad = dir.join("backups/world-20300101-000000.json");
        std::fs::write(&bad, "this is not a save").unwrap();
        let e = check_backup(&bad, &game.data, &game.net_data).unwrap_err();
        assert!(!e.is_empty(), "{e}");
        assert_eq!(check_backup(&dir.join("world.json"), &game.data, &game.net_data).map(|n| n > 0), Ok(true));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// An autosave is written beside the game: the call returns with the text made, the file
    /// arrives, and a save made right after it (a stop) is never overtaken by it.
    #[test]
    fn an_autosave_is_written_by_a_thread_and_never_overtakes_a_later_save() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let dir = std::env::temp_dir().join(format!("pcds-autosave-thread-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("world.json").to_string_lossy().to_string();
        game.advanced = true;
        autosave(&mut game, &path);
        assert!(!game.advanced && WRITING.with_borrow(Option::is_some), "the write is under way");
        // Asked again while it is: nothing starts, the next autosave tries again.
        game.advanced = true;
        autosave(&mut game, &path);
        settle(&mut game, true);
        let first = SaveFile::load(&path).unwrap().objects.len();
        let group = game.data.group("Iron").unwrap();
        game.create_world_object(group, 0, true);
        game.advanced = true;
        autosave(&mut game, &path);
        // The stop's save waits for the autosave and is the last word.
        assert_eq!(final_save(&mut game, &path), 0);
        assert_eq!(SaveFile::load(&path).unwrap().objects.len(), first + 1);
        assert!(WRITING.with_borrow(Option::is_none));
        assert!(last_save().is_some_and(|l| l.ok));
        assert_eq!(std::fs::read_dir(&dir).unwrap().filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().ends_with(".tmp")).count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The autosave does not wait for the disk: the loop gets on while a slow file is written, a
    /// save asked for meanwhile waits for that write and lands after it (the newer state wins),
    /// and a world that stood still writes nothing.
    #[test]
    fn the_autosave_does_not_wait_for_a_slow_disk_and_a_still_world_writes_nothing() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let dir = std::env::temp_dir().join(format!("pcds-autosave-slow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("world.json").to_string_lossy().to_string();
        WRITE_DELAY_MS.store(600, std::sync::atomic::Ordering::SeqCst);
        game.advanced = true;
        let started = std::time::Instant::now();
        autosave(&mut game, &path);
        let returned = started.elapsed();
        assert!(returned < Duration::from_millis(300), "the autosave waited {returned:?} for a 600 ms write");
        assert!(WRITING.with_borrow(Option::is_some));
        // The world moves on, an autosave comes while the write goes on (held back, not lost),
        // and a save asked for by the admin waits and writes the newer world last.
        let id = game.create_world_object(game.data.group("Iron").unwrap(), 0, true);
        game.advanced = true;
        autosave(&mut game, &path);
        assert!(game.advanced, "held back, not lost");
        persist(&mut game, &path).unwrap();
        WRITE_DELAY_MS.store(0, std::sync::atomic::Ordering::SeqCst);
        let on_disk = SaveFile::load(&path).unwrap();
        assert!(on_disk.objects.iter().any(|o| o["id"] == id), "the later save is the one on disk");
        // Standing still since: no write.
        game.advanced = false;
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        autosave(&mut game, &path);
        settle(&mut game, true);
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), before);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
