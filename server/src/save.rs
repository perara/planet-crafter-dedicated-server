//! The game's save file, read and written like SpaceCraft.JSONExport: a UTF-8 BOM, then ten
//! sections, each "\r" + JSON (lists joined by "|\n") + "\r@": 0 world state, 1 planets,
//! 2 players, 3 world objects, 4 inventories, 5 stats, 6 messages, 7 story events, 8 game
//! settings, 9 procedural instances. Fields at their default value are then cut out of the text.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde_json::Value;

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
        let text = std::fs::read_to_string(path)?;
        let text = text.trim_start_matches('\u{feff}');
        let sections = split_outside_strings(text, '@');
        let section = |i: usize| sections.get(i).copied().unwrap_or("");
        let first = |i: usize| entries(section(i)).into_iter().next().unwrap_or(Value::Null);
        Ok(SaveFile {
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
        })
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
        // Only whole fields: ",\"hunger\":0.0" must not cut ",\"hunger\":0.07".
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
            // JsonableGameState.CleanSaveStringOfVariables: ,"key":<digit>
            let pattern = format!(",\"{key}\":");
            while let Some(at) = text.find(&pattern).filter(|at| text[at + pattern.len()..].starts_with(|c: char| c.is_ascii_digit())) {
                text.replace_range(at..at + pattern.len() + 1, "");
            }
        }
        text
    }

    /// Writes atomically: a crash mid-write leaves the previous save.
    pub fn write(&self, path: &str) -> std::io::Result<()> {
        let tmp = format!("{path}.tmp");
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(self.to_text().as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
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
    std::fs::copy(path, dir.join(format!("{stem}-{time}.json")))?;
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
