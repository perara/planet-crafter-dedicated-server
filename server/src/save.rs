//! The game's save file, read and written like SpaceCraft.JSONExport: a UTF-8 BOM, then ten
//! sections, each "\r" + JSON (lists joined by "|\n") + "\r@": 0 world state, 1 planets,
//! 2 players, 3 world objects, 4 inventories, 5 stats, 6 messages, 7 story events, 8 game
//! settings, 9 procedural instances. Fields at their default value are then cut out of the text.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde_json::Value;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct SaveFile {
    pub world: Value,
    pub planets: Vec<Value>,
    pub players: Vec<Value>,
    pub objects: Vec<Value>,
    pub inventories: Vec<Value>,
    pub stats: Value,
    pub messages: Vec<Value>,
    pub story_events: Vec<Value>,
    pub settings: Value,
    pub procedural: Vec<Value>,
}

/// Splits on `delimiter` outside JSON strings: a name or a sign text may contain '@' or '|'
/// (JSONExport masks names before splitting for the same reason).
fn split_outside_strings(text: &str, delimiter: char) -> Vec<&str> {
    let (mut parts, mut start, mut in_string, mut escaped) = (Vec::new(), 0, false, false);
    for (i, c) in text.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            c if c == delimiter && !in_string => {
                parts.push(&text[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

fn entries(section: &str) -> Vec<Value> {
    split_outside_strings(section, '|').into_iter().map(str::trim).filter(|s| !s.is_empty()).filter_map(|s| serde_json::from_str(s).ok()).collect()
}

/// What JSONExport.SaveStringsInFile removes after serializing.
const DEFAULTS: [&str; 18] = [
    ",\"demandGrps\":\"\",\"supplyGrps\":\"\",\"priority\":0",
    ",\"set\":0",
    ",\"grwth\":0",
    ",\"hunger\":0.0",
    ",\"trtVal\":0",
    ",\"trtInd\":0",
    ",\"siIds\":\"\"",
    ",\"pnls\":\"\"",
    ",\"color\":\"\"",
    ",\"text\":\"\"",
    ",\"liGrps\":\"\"",
    ",\"liId\":0",
    ",\"pos\":\"0,0,0\"",
    ",\"rot\":\"0,0,0,0\"",
    ",\"planet\":0",
    ",\"liPlanet\":0",
    ",\"count\":\"0,0\"",
    ",\"linkedWo\":0",
];

impl SaveFile {
    pub fn empty() -> SaveFile {
        SaveFile {
            world: Value::Null,
            planets: Vec::new(),
            players: Vec::new(),
            objects: Vec::new(),
            inventories: Vec::new(),
            stats: Value::Null,
            messages: Vec::new(),
            story_events: Vec::new(),
            settings: Value::Null,
            procedural: Vec::new(),
        }
    }

    pub fn load(path: &str) -> std::io::Result<SaveFile> {
        Ok(SaveFile::from_text(&std::fs::read_to_string(path)?))
    }

    pub fn from_text(text: &str) -> SaveFile {
        let text = text.trim_start_matches('\u{feff}');
        let sections = split_outside_strings(text, '@');
        let section = |i: usize| sections.get(i).copied().unwrap_or("");
        let first = |i: usize| entries(section(i)).into_iter().next().unwrap_or(Value::Null);
        SaveFile {
            world: first(0),
            planets: entries(section(1)),
            players: entries(section(2)),
            objects: entries(section(3)),
            inventories: entries(section(4)),
            stats: first(5),
            messages: entries(section(6)),
            story_events: entries(section(7)),
            settings: first(8),
            procedural: entries(section(9)),
        }
    }

    pub fn to_text(&self) -> String {
        let one = |v: &Value| serde_json::to_string(v).unwrap_or_default();
        let list = |items: &[Value]| items.iter().map(one).collect::<Vec<_>>().join("|\n");
        let sections = [
            one(&self.world),
            list(&self.planets),
            list(&self.players),
            list(&self.objects),
            list(&self.inventories),
            one(&self.stats),
            list(&self.messages),
            list(&self.story_events),
            one(&self.settings),
            list(&self.procedural),
        ];
        let mut text = String::from("\u{feff}");
        for s in sections {
            text.push('\r');
            text.push_str(&s);
            text.push_str("\r@");
        }
        // Only whole fields: ",\"hunger\":0.0" must not cut ",\"hunger\":0.07". Found in the text as
        // it is, cut in one pass: removing them one by one moved the rest of the text each time
        // (40 000 objects took half a minute).
        let mut cuts: Vec<(usize, usize)> = Vec::new();
        for d in DEFAULTS {
            let mut from = 0;
            while let Some(at) = text[from..].find(d).map(|i| i + from) {
                from = at + d.len();
                if text[from..].starts_with([',', '}']) {
                    cuts.push((at, from));
                }
            }
        }
        cuts.sort_unstable();
        let mut kept = String::with_capacity(text.len());
        let mut last = 0;
        for (from, to) in cuts {
            kept.push_str(&text[last..from]);
            last = to;
        }
        kept.push_str(&text[last..]);
        text = kept;
        for key in ["gameMode", "gameDyingConsequences", "gameStartLocation"] {
            // JsonableGameState.CleanSaveStringOfVariables: ,"key":<digit>
            let pattern = format!(",\"{key}\":");
            while let Some(at) = text.find(&pattern).filter(|at| text[at + pattern.len()..].starts_with(|c: char| c.is_ascii_digit())) {
                text.replace_range(at..at + pattern.len() + 1, "");
            }
        }
        text
    }

    /// Writes atomically (write_atomic).
    pub fn write(&self, path: &str) -> std::io::Result<()> {
        write_atomic(path, self.to_text().as_bytes())
    }
}

/// Writes a file atomically: a crash mid-write leaves the previous one, and the rename is on
/// disk when this returns (the folder fsynced). The file beside it is this write's own (two
/// banks may write at once, as when the front ends during a flip): each rename moves one whole
/// file over the path. The file keeps its mode (a chmod 600 holds); a new one is 0600
/// (rules.json and access.json hold the chat secret and webhook URLs).
pub fn write_atomic(path: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mode = std::fs::metadata(path).map(|m| m.permissions().mode() & 0o7777).unwrap_or(0o600);
    // A name of its own: a leftover of an earlier process with this pid (a container's pids repeat)
    // and this counter is not ours to overwrite, nor a reason to fail the save: the next name.
    let (mut tmp, mut created) = (String::new(), Err(std::io::ErrorKind::AlreadyExists.into()));
    for _ in 0..1000 {
        tmp = format!("{path}.{}-{}.tmp", std::process::id(), WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        created = std::fs::File::create_new(&tmp);
        if created.as_ref().is_err_and(|e| e.kind() != std::io::ErrorKind::AlreadyExists) || created.is_ok() {
            break;
        }
    }
    let written = created.and_then(|mut file| {
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    });
    if written.is_err() && !tmp.is_empty() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    let dir = Path::new(path).parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::File::open(dir)?.sync_all()
}

/// Removes what write_atomic left in `dir` when its process died mid-write: <name>.<pid>-<n>.tmp
/// of a pid that no longer runs.
pub fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for name in entries.flatten().map(|e| e.file_name().to_string_lossy().to_string()) {
        let Some(pid) = name.strip_suffix(".tmp").and_then(|n| n.rsplit_once('.')).and_then(|(_, t)| t.split_once('-')).filter(|(_, n)| n.parse::<u64>().is_ok()).and_then(|(p, _)| p.parse::<i32>().ok()) else { continue };
        // SAFETY: signal 0 only asks whether the process is there.
        let gone = pid > 0 && unsafe { libc::kill(pid, 0) } < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if gone && std::fs::remove_file(dir.join(&name)).is_ok() {
            println!("removed {}: a write cut short by a process that ended", dir.join(&name).display());
        }
    }
}

/// UTC "YYYYMMDD-HHMMSS" (days to civil date: Howard Hinnant's algorithm).
pub fn stamp(t: SystemTime) -> String {
    let s = t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (z, secs) = (s.div_euclid(86_400) + 719_468, s.rem_euclid(86_400));
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, m) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// Before a save is replaced: a copy in backups/ next to it, <stem>-<UTC time>.json, unless
/// one is younger than `every`; only the newest `keep` stay (0: no backups).
pub fn backup(path: &str, every: Duration, keep: usize) -> std::io::Result<()> {
    let path = Path::new(path);
    if keep == 0 || !path.exists() {
        return Ok(());
    }
    let dir = path.with_file_name("backups");
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    std::fs::create_dir_all(&dir)?;
    // This save's backups by time ("world-2-<time>" belongs to world-2, not world).
    let mut times: Vec<String> = std::fs::read_dir(&dir)?
        .filter_map(|e| Some(e.ok()?.file_name().to_string_lossy().strip_prefix(&format!("{stem}-"))?.strip_suffix(".json")?.to_string()))
        .filter(|t| t.len() == 15 && t.bytes().enumerate().all(|(i, b)| if i == 8 { b == b'-' } else { b.is_ascii_digit() }))
        .collect();
    let now = SystemTime::now();
    let since = stamp(now.checked_sub(every).unwrap_or(SystemTime::UNIX_EPOCH));
    if times.iter().any(|t| *t > since) {
        return Ok(());
    }
    let time = stamp(now);
    // Whole or not at all: a copy cut short (a full disk) under a backup's name would count as
    // this hour's backup.
    write_atomic(&dir.join(format!("{stem}-{time}.json")).to_string_lossy(), &std::fs::read(path)?)?;
    times.push(time);
    times.sort();
    times.dedup();
    for old in &times[..times.len().saturating_sub(keep)] {
        std::fs::remove_file(dir.join(format!("{stem}-{old}.json")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Two writers of one save at once: every write lands whole, nothing is left beside it.
    #[test]
    fn saves_written_at_once_never_mix() {
        let dir = std::env::temp_dir().join(format!("pcds-save-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("world.json").to_string_lossy().to_string();
        let writer = |name: &'static str| {
            let path = path.clone();
            std::thread::spawn(move || {
                let mut save = SaveFile::empty();
                save.world = json!({"terraTokens": 1, "who": name.repeat(20_000)});
                (0..50).try_for_each(|_| save.write(&path))
            })
        };
        let (a, b) = (writer("a"), writer("b"));
        a.join().unwrap().unwrap();
        b.join().unwrap().unwrap();
        let world = SaveFile::load(&path).unwrap().world;
        assert!(world["who"] == "a".repeat(20_000) || world["who"] == "b".repeat(20_000));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A leftover of an earlier process with this pid and a counter value is not a reason to
    /// fail a write (containers start with the same pid every time), and it is not overwritten.
    #[test]
    fn a_leftover_temporary_file_of_the_same_pid_does_not_fail_the_write() {
        let dir = std::env::temp_dir().join(format!("pcds-write-leftover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("world.json").to_string_lossy().to_string();
        for n in 0..300 {
            std::fs::write(format!("{path}.{}-{n}.tmp", std::process::id()), "left over").unwrap();
        }
        write_atomic(&path, b"{}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
        assert_eq!(std::fs::read_to_string(format!("{path}.{}-0.tmp", std::process::id())).unwrap(), "left over");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A file keeps its mode across writes, a new one is 0600, and the sweep removes only what
    /// writers that ended left.
    #[test]
    fn writes_keep_the_mode_and_dead_writers_leftovers_are_swept() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pcds-write-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rules.json");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        write_atomic(path.to_str().unwrap(), b"{}").unwrap();
        assert_eq!(mode(&path), 0o600);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_atomic(path.to_str().unwrap(), b"{}").unwrap();
        assert_eq!(mode(&path), 0o640);
        // A pid that ran and ended, and this one.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let (stale, live) = (format!("world.json.{dead}-3.tmp"), format!("world.json.{}-0.tmp", std::process::id()));
        for name in [&stale, &live, &"notes.tmp".to_string()] {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        sweep(&dir);
        let mut left: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
        left.sort();
        assert_eq!(left, ["notes.tmp".to_string(), "rules.json".into(), live]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn delimiters_inside_strings_and_partial_defaults_survive() {
        let mut save = SaveFile::empty();
        save.world = json!({"terraTokens": 1});
        save.players = vec![json!({"id": 1, "name": "Bob@Home|x"}), json!({"id": 2, "name": "Ann"})];
        save.objects = vec![json!({"id": 5, "gId": "Sign", "text": "a@b|c"}), json!({"id": 6, "gId": "Animal", "hunger": 0.07, "grwth": 3}), json!({"id": 7, "gId": "Animal", "hunger": 0.0})];
        save.settings = json!({"planetId": "Prime"});
        let text = save.to_text();
        assert!(text.contains("\"hunger\":0.07,\"grwth\":3"), "{text}");
        assert!(!text.contains("\"id\":7,\"gId\":\"Animal\",\"hunger\""), "the default hunger is cut");
        let dir = std::env::temp_dir().join(format!("pcds-delims-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("w.json");
        std::fs::write(&path, &text).unwrap();
        let back = SaveFile::load(path.to_str().unwrap()).unwrap();
        assert_eq!(back.players.len(), 2);
        assert_eq!(back.players[0]["name"], "Bob@Home|x");
        assert_eq!(back.objects.len(), 3);
        assert_eq!(back.objects[0]["text"], "a@b|c");
        assert_eq!(back.settings["planetId"], "Prime");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn round_trips_a_game_save() {
        // As the game writes it: no default fields, "\r@" after every section.
        let original = "\u{feff}\r{\"terraTokens\":0}\r@\r{\"planetId\":\"Prime\"}\r@\r{\"id\":1,\"name\":\"A\"}\r@\r{\"id\":208217122,\"gId\":\"EscapePod\",\"pos\":\"1,2,3\",\"planet\":-1140328421}|\n{\"id\":3,\"gId\":\"astrofood\"}\r@\r{\"id\":1,\"woIds\":\"3\",\"size\":12}\r@\r{\"craftedObjects\":0}\r@\r{\"stringId\":\"m\",\"isRead\":false}\r@\r{\"stringId\":\"e\"}\r@\r{\"planetId\":\"Prime\"}\r@\r\r@";
        let dir = std::env::temp_dir().join(format!("pcds-save-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.json");
        std::fs::write(&path, original).unwrap();
        let save = SaveFile::load(path.to_str().unwrap()).unwrap();
        assert_eq!(save.objects.len(), 2);
        assert_eq!(save.to_text(), original);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The old way of cutting the defaults (each one cut in place, the rest of the text moved
    /// each time): the new one makes the same text.
    fn cut_one_by_one(mut text: String) -> String {
        for d in DEFAULTS {
            let mut from = 0;
            while let Some(at) = text[from..].find(d).map(|i| i + from) {
                if text[at + d.len()..].starts_with([',', '}']) {
                    text.replace_range(at..at + d.len(), "");
                    from = at;
                } else {
                    from = at + d.len();
                }
            }
        }
        text
    }

    fn many_objects(n: usize) -> SaveFile {
        let mut save = SaveFile::empty();
        save.world = json!({"terraTokens": 1});
        save.objects = (0..n)
            .map(|i| {
                let mut o = json!({"id": 200_000_000 + i as i32, "gId": "Iron", "liId": 0, "siIds": "", "liGrps": "", "pos": "0,0,0", "rot": "0,0,0,0", "planet": 0, "liPlanet": 0, "pnls": "", "color": "", "text": "", "grwth": 0, "set": 0, "trtInd": 0, "trtVal": 0, "hunger": 0.0, "count": "0,0", "linkedWo": 0});
                match i % 4 {
                    1 => o["hunger"] = json!(0.07),
                    2 => o["text"] = json!("a,\"set\":0}"),
                    3 => (o["set"], o["pos"], o["count"]) = (json!(3), json!("1,2,3"), json!("0,5")),
                    _ => {}
                }
                o
            })
            .collect();
        save
    }

    #[test]
    fn the_defaults_are_cut_as_they_were_cut_one_by_one() {
        let save = many_objects(400);
        let text = save.to_text();
        let mut uncut = save;
        // The same text without the cuts: the other steps of to_text are the same.
        let all = uncut.objects.iter().map(|o| serde_json::to_string(o).unwrap()).collect::<Vec<_>>().join("|\n");
        uncut.objects.clear();
        let expected = cut_one_by_one(all);
        let cut = text.split("\r@").nth(3).unwrap().trim_start_matches('\r');
        assert_eq!(cut, cut_one_by_one(cut.to_string()), "nothing left to cut");
        assert_eq!(cut, expected);
    }

    /// A big world saves in time: the cuts were quadratic in the size of the save (40 000 objects
    /// took 50 s in a release build). In a thread: the old way would not end within the test.
    #[test]
    fn a_big_save_is_written_in_time() {
        let save = many_objects(100_000);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(save.to_text().len());
        });
        rx.recv_timeout(Duration::from_secs(60)).expect("the text of 100 000 objects in under a minute");
    }

    #[test]
    fn strips_default_fields() {
        let mut save = SaveFile::empty();
        save.objects = vec![serde_json::json!({"id": 5, "gId": "Iron", "liId": 0, "siIds": "", "pos": "0,0,0", "rot": "0,0,0,0", "planet": 0, "hunger": 0.0, "count": "0,0", "linkedWo": 0})];
        assert!(save.to_text().contains("{\"id\":5,\"gId\":\"Iron\"}"));
    }

    /// A backup cut short (by a file size limit here, as by a full disk) leaves nothing under a
    /// backup's name: the next save makes one again. In a child: the limit is per process.
    #[test]
    fn a_cut_short_backup_is_not_kept() {
        use std::os::unix::process::CommandExt;
        let dir = std::env::temp_dir().join(format!("pcds-backup-cut-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("world.json"), vec![b'x'; 64 << 10]).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child.args(["--exact", "save::tests::backup_under_a_size_limit", "--ignored"]).env("PCDS_BACKUP_CUT_DIR", &dir).stdout(std::process::Stdio::null());
        // SAFETY: between fork and exec only async-signal-safe calls: signal and setrlimit.
        unsafe {
            child.pre_exec(|| {
                libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
                let limit = libc::rlimit { rlim_cur: 4096, rlim_max: 4096 };
                if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        assert!(child.status().unwrap().success());
        let left: Vec<_> = std::fs::read_dir(dir.join("backups")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert!(left.is_empty(), "{left:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[ignore = "run by a_cut_short_backup_is_not_kept"]
    fn backup_under_a_size_limit() {
        let Ok(dir) = std::env::var("PCDS_BACKUP_CUT_DIR") else { return };
        let save = Path::new(&dir).join("world.json");
        assert!(backup(save.to_str().unwrap(), Duration::ZERO, 5).is_err());
    }

    #[test]
    fn backups_are_spaced_and_pruned() {
        assert_eq!(stamp(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000)), "20010909-014640");
        let dir = std::env::temp_dir().join(format!("pcds-backup-test-{}", std::process::id()));
        let backups = dir.join("backups");
        std::fs::create_dir_all(&backups).unwrap();
        let save = dir.join("world.json");
        std::fs::write(&save, "v1").unwrap();
        for old in ["world-20200101-000000.json", "world-20200101-010000.json", "world-2-20200101-000000.json"] {
            std::fs::write(backups.join(old), "old").unwrap();
        }
        let hour = Duration::from_secs(3600);
        backup(save.to_str().unwrap(), hour, 2).unwrap();
        std::fs::write(&save, "v2").unwrap();
        backup(save.to_str().unwrap(), hour, 2).unwrap();
        let mut names: Vec<String> = std::fs::read_dir(&backups).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
        names.sort();
        assert_eq!(names.len(), 3, "{names:?}");
        assert_eq!(names[..2], ["world-2-20200101-000000.json", "world-20200101-010000.json"], "the oldest pruned, other saves' kept");
        assert_eq!(std::fs::read_to_string(backups.join(&names[2])).unwrap(), "v1", "one per hour: the second save made none");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
