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

/// JsonableGameState.CleanSaveStringOfVariables: ,"key":<digit> loses the key and that digit.
const CLEANED: [&str; 3] = [",\"gameMode\":", ",\"gameDyingConsequences\":", ",\"gameStartLocation\":"];

/// The save's text as the game writes it, entry by entry: each entry's JSON with DEFAULTS cut
/// out (only whole fields: ",\"hunger\":0.0" must not cut ",\"hunger\":0.07"; every field starts
/// with ",\"" and is followed by "," or "}" whatever else is cut), then CLEANED. One pass over
/// each entry, so a save costs what it weighs however many fields are cut.
struct TextWriter {
    text: String,
    /// Per CLEANED key: its cleaning ended. The text is cleaned from its start, and a key's
    /// cleaning stops at the first of its occurrences that is not followed by a digit.
    stopped: [bool; 3],
}

impl TextWriter {
    /// "\r" + the entries joined by "|\n" + "\r@".
    fn section(&mut self, entries: &[Value]) {
        self.text.push('\r');
        for (i, v) in entries.iter().enumerate() {
            if i > 0 {
                self.text.push_str("|\n");
            }
            self.entry(&serde_json::to_string(v).unwrap_or_default());
        }
        self.text.push_str("\r@");
    }

    /// A section whose entries are already as written (no defaults, nothing to clean).
    fn raw_section(&mut self, entries: &dyn Fn(&mut String)) {
        self.text.push('\r');
        entries(&mut self.text);
        self.text.push_str("\r@");
    }

    fn entry(&mut self, raw: &str) {
        let (start, out) = (self.text.len(), &mut self.text);
        let (mut from, mut i) = (0, 0);
        while let Some(at) = raw[i..].find(",\"").map(|p| p + i) {
            let rest = &raw[at..];
            match DEFAULTS.iter().find(|d| rest.starts_with(**d) && rest[d.len()..].starts_with([',', '}'])) {
                Some(d) => {
                    out.push_str(&raw[from..at]);
                    (from, i) = (at + d.len(), at + d.len());
                }
                None => i = at + 2,
            }
        }
        out.push_str(&raw[from..]);
        for (k, pattern) in CLEANED.iter().enumerate() {
            let mut at = start;
            while !self.stopped[k] && let Some(p) = out[at..].find(pattern).map(|p| p + at) {
                if out[p + pattern.len()..].starts_with(|c: char| c.is_ascii_digit()) {
                    out.replace_range(p..p + pattern.len() + 1, "");
                    at = p;
                } else {
                    self.stopped[k] = true;
                }
            }
        }
    }
}

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
        self.text_with(None)
    }

    /// to_text, with the world objects' and the inventories' sections written by the two
    /// functions given (each appends its entries, "|\n" between them, already as the game
    /// writes them) instead of taken from `self.objects` and `self.inventories`.
    pub fn text_with(&self, raw: Option<(&dyn Fn(&mut String), &dyn Fn(&mut String))>) -> String {
        let mut w = TextWriter { text: String::from("\u{feff}"), stopped: [false; 3] };
        w.section(std::slice::from_ref(&self.world));
        w.section(&self.planets);
        w.section(&self.players);
        match raw {
            Some((objects, inventories)) => {
                w.raw_section(objects);
                w.raw_section(inventories);
            }
            None => {
                w.section(&self.objects);
                w.section(&self.inventories);
            }
        }
        w.section(std::slice::from_ref(&self.stats));
        w.section(&self.messages);
        w.section(&self.story_events);
        w.section(std::slice::from_ref(&self.settings));
        w.section(&self.procedural);
        w.text
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
    let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = format!("{path}.{}-{n}.tmp", std::process::id());
    let mode = std::fs::metadata(path).map(|m| m.permissions().mode() & 0o7777).unwrap_or(0o600);
    let written = std::fs::File::create_new(&tmp).and_then(|mut file| {
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    });
    if written.is_err() {
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

    /// The text as it was made before the one pass per entry: every default cut from the whole
    /// text in turn (quadratic: minutes for a big world).
    fn reference_text(save: &SaveFile) -> String {
        let one = |v: &Value| serde_json::to_string(v).unwrap_or_default();
        let list = |items: &[Value]| items.iter().map(one).collect::<Vec<_>>().join("|\n");
        let sections = [one(&save.world), list(&save.planets), list(&save.players), list(&save.objects), list(&save.inventories), one(&save.stats), list(&save.messages), list(&save.story_events), one(&save.settings), list(&save.procedural)];
        let mut text = String::from("\u{feff}");
        for s in sections {
            text.push('\r');
            text.push_str(&s);
            text.push_str("\r@");
        }
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
        for key in ["gameMode", "gameDyingConsequences", "gameStartLocation"] {
            let pattern = format!(",\"{key}\":");
            while let Some(at) = text.find(&pattern).filter(|at| text[at + pattern.len()..].starts_with(|c: char| c.is_ascii_digit())) {
                text.replace_range(at..at + pattern.len() + 1, "");
            }
        }
        text
    }

    /// Cutting defaults in one pass writes the very bytes the whole-text cuts did, on fields in
    /// every combination (default or not, last or not, in text that looks like a field), and on
    /// the save PCDS_BENCH_SAVE names.
    #[test]
    fn the_text_is_what_the_whole_text_cuts_made() {
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        // Every default, a near miss of it, and a field that is not one, in one object's order.
        let fields: Vec<(&str, Vec<Value>)> = vec![
            ("liId", vec![json!(0), json!(5), json!(0)]),
            ("siIds", vec![json!(""), json!("1,2")]),
            ("pos", vec![json!("0,0,0"), json!("0,0,0.5"), json!("1,2,3")]),
            ("rot", vec![json!("0,0,0,0"), json!("0,0,0,1")]),
            ("planet", vec![json!(0), json!(-1140328421)]),
            ("liPlanet", vec![json!(0), json!(3)]),
            ("pnls", vec![json!(""), json!("1,2")]),
            ("color", vec![json!(""), json!("1-0-0-1")]),
            ("text", vec![json!(""), json!(",\"set\":0"), json!("a\"},\"pos\":\"0,0,0\"")]),
            ("grwth", vec![json!(0), json!(100)]),
            ("set", vec![json!(0), json!(1), json!(0)]),
            ("trtInd", vec![json!(0), json!(2)]),
            ("trtVal", vec![json!(0), json!(7)]),
            ("hunger", vec![json!(0.0), json!(0.07), json!(5.0)]),
            ("count", vec![json!("0,0"), json!("0,1")]),
            ("linkedWo", vec![json!(0), json!(9)]),
        ];
        let mut save = SaveFile::empty();
        save.world = json!({"terraTokens": 1});
        for id in 0..400 {
            let mut o = serde_json::Map::new();
            o.insert("id".into(), json!(id));
            o.insert("gId".into(), json!("Iron"));
            for (k, values) in &fields {
                if next(3) > 0 {
                    o.insert((*k).into(), values[next(values.len() as u64) as usize].clone());
                }
            }
            save.objects.push(Value::Object(o));
            let (supply, demand) = (if next(2) == 0 { "" } else { "Iron" }, if next(2) == 0 { "" } else { "Iron,Cobalt" });
            save.inventories.push(json!({"id": id, "woIds": "", "size": 4, "demandGrps": demand, "supplyGrps": supply, "priority": next(2)}));
        }
        save.players = vec![json!({"id": 1, "name": "A", "hunger": 0.0, "set": 0})];
        for settings in [json!({"gameMode": 3, "gameDyingConsequences": 12, "gameStartLocation": 0, "x": 1}), json!({"gameMode": "a", "gameDyingConsequences": 1}), json!({"planetId": "Prime"})] {
            save.settings = settings;
            assert_eq!(save.to_text(), reference_text(&save));
        }
        // A cleaned key whose first occurrence is no digit stops the cleaning there, in either.
        save.world = json!({"gameMode": "x"});
        save.settings = json!({"gameMode": 3});
        assert_eq!(save.to_text(), reference_text(&save));
        if let Ok(path) = std::env::var("PCDS_BENCH_SAVE") {
            let real = SaveFile::load(&path).unwrap();
            assert_eq!(real.to_text(), reference_text(&real));
        }
    }

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
