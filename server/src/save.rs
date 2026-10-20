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
    #[serde(with = "crate::bincodec::json_value")]
    pub world: Value,
    #[serde(with = "crate::bincodec::json_values")]
    pub planets: Vec<Value>,
    #[serde(with = "crate::bincodec::json_values")]
    pub players: Vec<Value>,
    #[serde(with = "crate::bincodec::json_values")]
    pub objects: Vec<Value>,
    #[serde(with = "crate::bincodec::json_values")]
    pub inventories: Vec<Value>,
    #[serde(with = "crate::bincodec::json_value")]
    pub stats: Value,
    #[serde(with = "crate::bincodec::json_values")]
    pub messages: Vec<Value>,
    #[serde(with = "crate::bincodec::json_values")]
    pub story_events: Vec<Value>,
    #[serde(with = "crate::bincodec::json_value")]
    pub settings: Value,
    #[serde(with = "crate::bincodec::json_values")]
    pub procedural: Vec<Value>,
    /// The world objects and inventories of a save read for a start, still as text (`objects` and
    /// `inventories` are empty then): see `parse_deferred`. Never in a snapshot.
    #[serde(skip)]
    pub(crate) pending: Option<Box<Pending>>,
}

/// The file's text and where the entries of the world objects' and the inventories' sections
/// are in it.
pub(crate) struct Pending {
    text: String,
    objects: Vec<(u32, u32)>,
    inventories: Vec<(u32, u32)>,
}

impl Pending {
    fn entries<'a>(&'a self, places: &'a [(u32, u32)]) -> impl Iterator<Item = Result<Value, String>> + 'a {
        places.iter().map(|&(from, to)| {
            let entry = &self.text[from as usize..to as usize];
            serde_json::from_str(entry).map_err(|e| format!("{e} in {}", entry.chars().take(80).collect::<String>()))
        })
    }
}

/// Splits on `delimiter` outside JSON strings: a name or a sign text may contain '@' or '|'
/// (JSONExport masks names before splitting for the same reason).
pub(crate) fn split_outside_strings(text: &str, delimiter: char) -> Vec<&str> {
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

/// The entries of a section; one that is not JSON (a save cut short, edited by hand) is an error:
/// dropped quietly it would be gone from the next save.
fn entries(section: &str) -> Result<Vec<Value>, String> {
    split_outside_strings(section, '|')
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| serde_json::from_str(s).map_err(|e| format!("{e} in {}", s.chars().take(80).collect::<String>())))
        .collect()
}

/// The game modes and dying consequences JsonableGameState names (DataConfig.GameSettingMode, GameSettingDyingConsequences).
pub const MODES: [&str; 6] = ["Chill", "Standard", "Intense", "Hardcore", "Creative", "Custom"];
pub const DYING: [&str; 4] = ["NoConsequences", "DropSomeItems", "DropAllItems", "DeleteSaveFile"];

/// What the errors about an old save tell the user to run.
pub const IMPORT_HINT: &str = "`pcds world import SAVE.json`";

/// What a section's entry is, by the fields that tell the kinds apart ("other": none of them).
fn kind(v: &Value) -> &'static str {
    let has = |k: &str| v.get(k).is_some();
    if !v.is_object() {
        "something that is no record"
    } else if has("layerId") {
        "colour layers"
    } else if has("playerPosition") {
        "a player"
    } else if has("gId") {
        "world objects"
    } else if has("woIds") {
        "inventories"
    } else if has("unitOxygenLevel") {
        if has("planetId") { "planets" } else { "unit levels" }
    } else if has("isRead") {
        "messages"
    } else if has("stringId") {
        "story events"
    } else if has("craftedObjects") {
        "stats"
    } else if has("terraTokens") {
        "the world state"
    } else if has("dyingConsequencesLabel") || has("worldSeed") || has("saveDisplayName") || has("mode") {
        "game settings"
    } else if has("owner") && has("seed") {
        "procedural instances"
    } else {
        "other"
    }
}

/// The ten sections of the layout JSONExport writes, with the kinds an entry of each may be.
const LAYOUT: [(&str, &[&str]); 10] = [
    ("world state", &["the world state"]),
    ("planets", &["planets"]),
    ("players", &["a player"]),
    ("world objects", &["world objects"]),
    ("inventories", &["inventories"]),
    ("stats", &["stats"]),
    ("messages", &["messages", "story events"]),
    ("story events", &["story events"]),
    ("game settings", &["game settings"]),
    ("procedural instances", &["procedural instances"]),
];

/// Refuses a file that is not a save in the current layout, in one line: what the file is and
/// what to do. Loaded as one, an old save (the game's layout before several planets: unit
/// levels first, one player, no planets) would be an empty world, its objects "players", and
/// the next save would write that over it. `sections` are the file's parts split at '@'; the
/// last one is what follows the final end mark.
fn layout(sections: &[&str], save: &SaveFile) -> Result<(), String> {
    let n = sections.len() - 1;
    let firsts = [
        Some(&save.world).filter(|v| !v.is_null()),
        save.planets.first(),
        save.players.first(),
        save.objects.first(),
        save.inventories.first(),
        Some(&save.stats).filter(|v| !v.is_null()),
        save.messages.first(),
        save.story_events.first(),
        Some(&save.settings).filter(|v| !v.is_null()),
        save.procedural.first(),
    ];
    for (i, (name, accepted)) in LAYOUT.iter().enumerate() {
        let found = firsts[i];
        let Some(found) = found else { continue };
        let k = kind(found);
        if k == "other" || accepted.contains(&k) {
            continue;
        }
        let old = matches!(k, "unit levels" | "colour layers") || (i == 1 && k == "a player");
        let where_ = format!("{n} sections, {k} in section {} where a save has {name}", i + 1);
        return Err(if old {
            format!("{where_}: the game's old save format (before several planets); {IMPORT_HINT} converts it")
        } else {
            format!("{where_}: not a layout this server reads (a save has 9 or 10 sections; the file may be cut short or no Planet Crafter save)")
        });
    }
    if !sections[n].trim().is_empty() {
        return Err(format!("the file ends inside section {} without its end mark: cut short (is the copy complete?)", n + 1));
    }
    if !(9..=10).contains(&n) {
        return Err(format!("{n} sections: a save has 9 or 10 (the file may be cut short or no Planet Crafter save)"));
    }
    Ok(())
}

/// A file with no game settings is no save, or one cut short (a copy still under way): read as an
/// empty world, the next save would write over what it held. Settings of the game's older
/// versions (a game mode, no dying consequences) are the game's to convert, which `pcds world
/// import` does as the game does.
pub fn check_settings(settings: &Value) -> Result<(), String> {
    if !settings.is_object() {
        return Err("no game settings in it: it is not a Planet Crafter save, or it was cut short (is the copy complete?)".into());
    }
    match settings["mode"].as_str() {
        Some(mode) if settings["dyingConsequencesLabel"].is_null() => Err(format!("a save from an old game version (game mode \"{mode}\", no dying consequences): {IMPORT_HINT} converts it")),
        _ => Ok(()),
    }
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

/// A section's entries written by a function (SaveFile::text_with).
pub type Section<'a> = &'a dyn Fn(&mut String);

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
            pending: None,
        }
    }

    /// The save a file holds, if it is in the layout this server writes (`layout`): an old or
    /// unknown layout, a file cut short, or one with no usable game settings is an error naming
    /// what the file is and what to do.
    pub fn load(path: &str) -> std::io::Result<SaveFile> {
        let bad = |e: String| std::io::Error::new(std::io::ErrorKind::InvalidData, e);
        let save = SaveFile::parse(&std::fs::read_to_string(path)?).map_err(bad)?;
        check_settings(&save.settings).map_err(bad)?;
        Ok(save)
    }

    #[cfg(test)]
    /// A save as the game wrote it in 2022, before several planets: unit levels, one player (no
    /// id, no name), world objects, inventories, stats, messages, story events, a game mode, colour
    /// layers (nine sections, nothing in them is the current layout).
    pub fn old_format_text() -> String {
        let sections = [
            r#"{"unitOxygenLevel":15.5,"unitHeatLevel":1.0,"unitPressureLevel":2.0,"unitBiomassLevel":3.0}"#,
            r#"{"playerPosition":"1,2,3","playerRotation":"0,0,0,1","unlockedGroups":"Pod4x,BootsSpeed1","playerGaugeOxygen":370.0,"playerGaugeThirst":99.0,"playerGaugeHealth":98.0}"#,
            "{\"id\":208716012,\"gId\":\"EscapePod\",\"liId\":0,\"pos\":\"4,5,6\",\"rot\":\"0,0,0,1\",\"wear\":0,\"grwth\":0}|\n{\"id\":107604566,\"gId\":\"ice\",\"liId\":0,\"pos\":\"0,0,0\",\"wear\":0}",
            "{\"id\":1,\"woIds\":\"208716012\",\"size\":35}|\n{\"id\":2,\"woIds\":\"\",\"size\":12}",
            r#"{"craftedObjects":765,"totalSaveFileLoad":0,"totalSaveFileTime":0}"#,
            r#"{"stringId":"Message_YouAreAConvict","isRead":true}"#,
            r#"{"stringId":"StoryEvent-FirstRain"}"#,
            r#"{"mode":"Standard","hasPlayedIntro":false}"#,
            r#"{"layerId":"PCLayer1","colorBase":"0.2-0.7-0.2-1","colorCustom":"1-1-1-1","colorBaseLerp":100,"colorCustomLerp":0}"#,
        ];
        sections.iter().map(|s| format!("\n{s}\n@")).collect()
    }

    /// The smallest world this server loads, named `name` (CLI tests need a world that loads).
    #[cfg(test)]
    pub fn tiny_text(name: &str) -> String {
        let mut save = SaveFile::empty();
        save.world = serde_json::json!({"terraTokens": 0});
        save.stats = serde_json::json!({"craftedObjects": 0});
        save.settings = serde_json::json!({"saveDisplayName": name, "planetId": "Prime", "mode": "Standard", "dyingConsequencesLabel": "DropSomeItems"});
        save.to_text()
    }

    /// A save that was written by this server or the game; panics on one that is not (tests,
    /// the bench: `parse` and `load` say why).
    #[cfg(test)]
    pub fn from_text(text: &str) -> SaveFile {
        SaveFile::parse(text).unwrap_or_else(|e| panic!("not a save: {e}"))
    }

    /// The sections of a save in the current layout; anything else is refused (`layout`).
    pub fn parse(text: &str) -> Result<SaveFile, String> {
        SaveFile::parse_text(text, false)
    }

    /// `load`, with the world objects and the inventories left in the text (`parse_deferred`).
    pub fn load_deferred(path: &str) -> std::io::Result<SaveFile> {
        let bad = |e: String| std::io::Error::new(std::io::ErrorKind::InvalidData, e);
        let save = SaveFile::parse_deferred(std::fs::read_to_string(path)?).map_err(bad)?;
        check_settings(&save.settings).map_err(bad)?;
        Ok(save)
    }

    /// `parse`, but the entries of the world objects' and the inventories' sections stay in the
    /// text, which the save keeps: a world of 200,000 objects is 12 MB of text and 100 MB as a
    /// tree of values, and the game turns each entry into what it keeps one at a time
    /// (`world_objects`, `inventory_entries`: a damaged entry is an error there, as it is in `parse`).
    pub fn parse_deferred(text: String) -> Result<SaveFile, String> {
        let mut save = SaveFile::parse_text(&text, true)?;
        let body = text.trim_start_matches('\u{feff}');
        let sections = split_outside_strings(body, '@');
        let places = |i: usize| -> Vec<(u32, u32)> {
            let section = sections.get(i).copied().unwrap_or("");
            let base = text.as_ptr() as usize;
            split_outside_strings(section, '|')
                .into_iter()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| {
                    let from = s.as_ptr() as usize - base;
                    (from as u32, (from + s.len()) as u32)
                })
                .collect()
        };
        if text.len() >= u32::MAX as usize {
            return Err("a save of 4 GB or more".into());
        }
        let (objects, inventories) = (places(3), places(4));
        save.pending = Some(Box::new(Pending { text, objects, inventories }));
        Ok(save)
    }

    /// The world objects: the entries of section 3, parsed one at a time when the save was read
    /// deferred. An Err is a damaged entry.
    pub fn world_objects(&self) -> Box<dyn Iterator<Item = Result<Value, String>> + '_> {
        match &self.pending {
            Some(p) => Box::new(p.entries(&p.objects)),
            None => Box::new(self.objects.iter().cloned().map(Ok)),
        }
    }

    /// The inventories (section 4), as `world_objects`.
    pub fn inventory_entries(&self) -> Box<dyn Iterator<Item = Result<Value, String>> + '_> {
        match &self.pending {
            Some(p) => Box::new(p.entries(&p.inventories)),
            None => Box::new(self.inventories.iter().cloned().map(Ok)),
        }
    }

    /// How many world objects the save has.
    pub fn object_count(&self) -> usize {
        self.pending.as_ref().map_or(self.objects.len(), |p| p.objects.len())
    }

    /// `load`'s refusals for a save read deferred: every world object and inventory entry is read and
    /// let go, and a damaged one is the error `parse` would have given.
    pub fn check_entries(&self) -> Result<(), String> {
        for (section, entries) in [(3, self.world_objects()), (4, self.inventory_entries())] {
            for entry in entries {
                entry.map_err(|e| format!("section {section}: {e}"))?;
            }
        }
        Ok(())
    }

    /// `load`, for a caller that only asks whether the file is a save this server reads (a world to
    /// start or restore): the entries are checked one at a time, not kept as a tree.
    pub fn check_file(path: &str) -> std::io::Result<SaveFile> {
        let save = SaveFile::load_deferred(path)?;
        save.check_entries().map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(save)
    }

    /// Lets go of the world objects and inventories, and of the text they were in: the game has them.
    pub fn release_world(&mut self) {
        self.objects = Vec::new();
        self.inventories = Vec::new();
        self.pending = None;
    }

    fn parse_text(text: &str, defer: bool) -> Result<SaveFile, String> {
        let text = text.trim_start_matches('\u{feff}');
        let sections = split_outside_strings(text, '@');
        let section = |i: usize| sections.get(i).copied().unwrap_or("");
        let list = |i: usize| entries(section(i)).map_err(|e| format!("section {i}: {e}"));
        let first = |i: usize| list(i).map(|v| v.into_iter().next().unwrap_or(Value::Null));
        // Deferred: only the first entry of the two sections is read now, for the check of the layout.
        let head = |i: usize| -> Result<Vec<Value>, String> {
            let section = section(i);
            let first = split_outside_strings(section, '|').into_iter().map(str::trim).find(|s| !s.is_empty());
            first.map(|s| serde_json::from_str(s).map(|v| vec![v]).map_err(|e| format!("section {i}: {e} in {}", s.chars().take(80).collect::<String>()))).unwrap_or(Ok(Vec::new()))
        };
        let mut save = SaveFile {
            world: first(0)?,
            planets: list(1)?,
            players: list(2)?,
            objects: if defer { head(3)? } else { list(3)? },
            inventories: if defer { head(4)? } else { list(4)? },
            stats: first(5)?,
            messages: list(6)?,
            story_events: list(7)?,
            settings: first(8)?,
            procedural: list(9)?,
            pending: None,
        };
        layout(&sections, &save)?;
        if defer {
            save.objects.clear();
            save.inventories.clear();
        }
        Ok(save)
    }

    pub fn to_text(&self) -> String {
        self.text_with(None)
    }

    /// to_text, with the world objects' and the inventories' sections written by the two
    /// functions given (each appends its entries, "|\n" between them, already as the game
    /// writes them) instead of taken from `self.objects` and `self.inventories`.
    pub fn text_with(&self, raw: Option<(Section, Section)>) -> String {
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
    // Not through a link: the file the rename puts there replaces the link, and a link someone else
    // planted (/tmp/pcds-<pid>-...) must not choose its mode.
    let mode = std::fs::symlink_metadata(path).ok().filter(|m| m.is_file()).map(|m| m.permissions().mode() & 0o7777).unwrap_or(0o600);
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
    // Only a file this call made is removed again if the write fails.
    let ours = created.is_ok();
    let written = created.and_then(|mut file| {
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    });
    if written.is_err() && ours {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    let dir = crate::util::parent_dir(Path::new(path));
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
            info!("removed {}: a write cut short by a process that ended", dir.join(&name).display());
        }
    }
}

/// UTC "YYYYMMDD-HHMMSS".
pub fn stamp(t: SystemTime) -> String {
    let (y, m, d, h, mi, s) = crate::util::utc(t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0));
    format!("{y:04}{m:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// The time stamps of a save's backups in `dir`, oldest first. A stamp after `now` comes from a
/// clock that has stepped back since (NTP, a wrong RTC at boot): the backup was made earlier than
/// anything stamped now, so it is ordered before all the others, however far ahead its stamp is.
/// Names cannot tell more than that, and nothing here ever refuses a backup for a stamp ahead.
pub fn backup_stamps(dir: &Path, stem: &str, now: &str) -> std::io::Result<Vec<String>> {
    // This save's backups by time ("world-2-<time>" belongs to world-2, not world).
    let mut times: Vec<String> = std::fs::read_dir(dir)?
        .filter_map(|e| Some(e.ok()?.file_name().to_string_lossy().strip_prefix(&format!("{stem}-"))?.strip_suffix(".json")?.to_string()))
        .filter(|t| t.len() == 15 && t.bytes().enumerate().all(|(i, b)| if i == 8 { b == b'-' } else { b.is_ascii_digit() }))
        .collect();
    times.sort();
    times.dedup();
    let (past, ahead): (Vec<String>, Vec<String>) = times.into_iter().partition(|t| t.as_str() <= now);
    Ok(ahead.into_iter().chain(past).collect())
}

/// Before a save is replaced: a copy in backups/ next to it, <stem>-<UTC time>.json, unless
/// one is younger than `every`; only the newest `keep` stay (0: no backups). Whether a copy
/// was made (false: none was due). A backup stamped in the future (the clock stepped back) is not
/// "younger than `every`" and is the first to go (`backup_stamps`).
pub fn backup(path: &str, every: Duration, keep: usize) -> std::io::Result<bool> {
    let path = Path::new(path);
    if keep == 0 || !path.exists() {
        return Ok(false);
    }
    let dir = path.with_file_name("backups");
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    std::fs::create_dir_all(&dir)?;
    let now = SystemTime::now();
    let time = stamp(now);
    let mut times = backup_stamps(&dir, &stem, &time)?;
    let since = stamp(now.checked_sub(every).unwrap_or(SystemTime::UNIX_EPOCH));
    if times.iter().any(|t| *t > since && *t <= time) {
        return Ok(false);
    }
    // Whole or not at all: a copy cut short (a full disk) under a backup's name would count as
    // this hour's backup.
    write_atomic(&dir.join(format!("{stem}-{time}.json")).to_string_lossy(), &std::fs::read(path)?)?;
    // The copy just made is the newest, whatever the others are stamped.
    times.retain(|t| *t != time);
    times.push(time);
    for old in &times[..times.len().saturating_sub(keep)] {
        std::fs::remove_file(dir.join(format!("{stem}-{old}.json")))?;
    }
    Ok(true)
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
        let world = SaveFile::from_text(&std::fs::read_to_string(&path).unwrap()).world;
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

    /// A path that is a link to a file of another mode (a planted /tmp/pcds-<pid>-... in front of
    /// the fallback snapshot, say) must not lend that file's mode to the world written over it.
    #[test]
    fn a_link_in_the_way_does_not_lend_its_targets_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pcds-write-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("public");
        std::fs::write(&target, "x").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        let path = dir.join("pcds-1-world.snapshot");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        write_atomic(path.to_str().unwrap(), b"private").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777, 0o600, "a new file is 0600 whatever the link pointed at");
        assert!(!std::fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "x", "the target is not written through");
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
        let back = SaveFile::from_text(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(back.players.len(), 2);
        assert_eq!(back.players[0]["name"], "Bob@Home|x");
        assert_eq!(back.objects.len(), 3);
        assert_eq!(back.objects[0]["text"], "a@b|c");
        assert_eq!(back.settings["planetId"], "Prime");
        std::fs::remove_dir_all(dir).ok();
    }

    /// A save read deferred has the same world objects and inventories as one read whole, and
    /// parses them one at a time: a damaged entry is an error where it is read, naming the section.
    #[test]
    fn a_deferred_save_has_the_entries_of_the_whole_one_and_a_damaged_entry_is_an_error() {
        let mut save = SaveFile::empty();
        save.world = json!({"terraTokens": 1});
        save.objects = (0..50).map(|i| json!({"id": 200_000_000 + i, "gId": "Iron", "text": format!("a|b@{i}"), "pos": "1,2,3"})).collect();
        save.inventories = (0..20).map(|i| json!({"id": i, "woIds": "1,2", "size": 12})).collect();
        save.settings = json!({"planetId": "Prime", "mode": "Standard", "dyingConsequencesLabel": "DropSomeItems"});
        let text = save.to_text();
        let whole = SaveFile::parse(&text).unwrap();
        let deferred = SaveFile::parse_deferred(text.clone()).unwrap();
        assert!(deferred.objects.is_empty() && deferred.inventories.is_empty());
        assert_eq!(deferred.world_objects().collect::<Result<Vec<_>, _>>().unwrap(), whole.objects);
        assert_eq!(deferred.inventory_entries().collect::<Result<Vec<_>, _>>().unwrap(), whole.inventories);
        assert_eq!(whole.world_objects().collect::<Result<Vec<_>, _>>().unwrap(), whole.objects);
        assert_eq!(deferred.world, whole.world);
        let mut released = SaveFile::parse_deferred(text.clone()).unwrap();
        released.release_world();
        assert_eq!(released.world_objects().count(), 0);
        // The same refusals: an old layout, and a damaged entry (found where it is read).
        let old = SaveFile::old_format_text();
        assert_eq!(SaveFile::parse(&old).err(), SaveFile::parse_deferred(old).err());
        let damaged = text.replacen("\"id\":200000007", "\"id\":", 1);
        let (whole_error, deferred_error) = (SaveFile::parse(&damaged).err().unwrap(), SaveFile::parse_deferred(damaged).unwrap());
        let found = deferred_error.world_objects().find_map(Result::err).unwrap();
        assert!(whole_error.starts_with("section 3: ") && whole_error.contains(&found), "{whole_error} / {found}");
        // A caller that only asks whether a file is a save reads the entries one at a time, with the same answers.
        let dir = std::env::temp_dir().join(format!("pcds-check-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (good, bad) = (dir.join("good.json"), dir.join("bad.json"));
        std::fs::write(&good, &text).unwrap();
        std::fs::write(&bad, text.replacen("\"id\":200000007", "\"id\":", 1)).unwrap();
        assert_eq!(SaveFile::check_file(&good.to_string_lossy()).unwrap().object_count(), 50);
        let (refused, loaded) = (SaveFile::check_file(&bad.to_string_lossy()).err().unwrap(), SaveFile::load(&bad.to_string_lossy()).err().unwrap());
        assert_eq!((refused.kind(), refused.to_string()), (loaded.kind(), loaded.to_string()));
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
        let save = SaveFile::from_text(&std::fs::read_to_string(&path).unwrap());
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
        let save = many_objects(50_000);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(save.to_text().len());
        });
        rx.recv_timeout(Duration::from_secs(60)).expect("the text of 50 000 objects in under a minute");
    }

    /// An entry that is not JSON (a save cut short) stops the load, naming it: it would
    /// otherwise be dropped, and gone from the next save.
    #[test]
    fn a_save_with_a_damaged_entry_is_refused() {
        let good = "\u{feff}\r{\"terraTokens\":0}\r@\r\r@\r\r@\r{\"id\":5,\"gId\":\"Iron\"}|\n{\"id\":6,\"gId\":\"Ir";
        let err = SaveFile::parse(good).err().expect("refused");
        assert!(err.contains("section 3") && err.contains("\"gId\":\"Ir"), "{err}");
        let dir = std::env::temp_dir().join(format!("pcds-damaged-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("w.json");
        std::fs::write(&path, good).unwrap();
        assert_eq!(SaveFile::load(path.to_str().unwrap()).err().map(|e| e.kind()), Some(std::io::ErrorKind::InvalidData));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn an_old_format_save_is_refused_naming_the_import() {
        let err = SaveFile::parse(&SaveFile::old_format_text()).err().expect("refused");
        assert!(err.contains("9 sections") && err.contains("unit levels in section 1") && err.contains("old save format") && err.contains("pcds world import"), "{err}");
        // Units first or a player in the planets' place: the same old layout.
        let text = SaveFile::old_format_text().replacen("unitOxygenLevel", "x", 1);
        let err = SaveFile::parse(&text).err().expect("refused");
        assert!(err.contains("a player in section 2") && err.contains("pcds world import"), "{err}");
        let dir = std::env::temp_dir().join(format!("pcds-old-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.json");
        std::fs::write(&path, SaveFile::old_format_text()).unwrap();
        let err = SaveFile::load(path.to_str().unwrap()).err().expect("refused");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(!err.to_string().contains('\n') && err.to_string().contains("pcds world import"), "{err}");
        std::fs::remove_dir_all(dir).ok();
    }

    /// A save in the current layout with one of every kind of entry.
    fn current_text() -> String {
        let mut save = SaveFile::empty();
        save.world = json!({"terraTokens": 3, "allTimeTerraTokens": 3, "unlockedGroups": "", "openedInstanceSeed": 0, "openedInstanceTimeLeft": 0, "logisticsPaused": false});
        save.planets = vec![json!({"planetId": "Prime", "unitOxygenLevel": 1.5, "unitHeatLevel": 0.0, "unitPressureLevel": 0.0, "unitPlantsLevel": 0.0, "unitInsectsLevel": 0.0, "unitAnimalsLevel": 0.0, "unitPurificationLevel": -1.0})];
        save.players = vec![json!({"id": 0, "name": "Convict-1", "inventoryId": 1, "equipmentId": 2, "playerPosition": "1,2,3", "playerRotation": "0,0,0,1", "host": true, "planetId": "Prime"})];
        save.objects = vec![json!({"id": 5, "gId": "Iron"}), json!({"id": 6, "gId": "ice"})];
        save.inventories = vec![json!({"id": 1, "woIds": "5", "size": 12})];
        save.stats = json!({"craftedObjects": 1, "totalSaveFileLoad": 0, "totalSaveFileTime": 0});
        save.messages = vec![json!({"stringId": "Message_Help1", "isRead": true})];
        save.story_events = vec![json!({"stringId": "StoryEvent-FirstRain"})];
        save.settings = json!({"saveDisplayName": "w", "planetId": "Prime", "mode": "Standard", "dyingConsequencesLabel": "DropSomeItems", "worldSeed": 7});
        save.procedural = vec![json!({"owner": 0, "planet": 1, "index": 0, "seed": 5})];
        save.to_text()
    }

    /// Whatever is not a save in this layout is refused with a reason, and never read as a smaller
    /// world: a file cut anywhere, text that is no save, a section too many or too few.
    #[test]
    fn truncated_and_garbage_files_are_refused() {
        let good = current_text();
        assert!(SaveFile::parse(&good).is_ok());
        let ends: Vec<usize> = good.match_indices("\r@").map(|(i, _)| i + 2).collect();
        assert_eq!(ends.len(), 10);
        for cut in (0..good.len()).filter(|c| good.is_char_boundary(*c)) {
            let prefix = &good[..cut];
            let whole_sections = ends.iter().filter(|e| **e <= cut).count();
            // Cut at a section's end mark: nine sections are a save with no instances, fewer are not.
            let ok = cut == good.len() || (whole_sections == 9 && good[ends[8]..cut].trim().is_empty());
            let result = SaveFile::parse(prefix);
            assert_eq!(result.is_ok(), ok, "cut at {cut} ({whole_sections} sections): {:?}", result.err());
            if let Err(e) = result {
                assert!(!e.is_empty() && !e.contains('\n'), "{e}");
            }
        }
        for garbage in ["", "\u{feff}", "hello", "{}", "[1,2]", "@@@@@@@@@@", "\0\0\0@", &"x@".repeat(10), &good.replace("\r@", "\r@\r@"), &format!("{good}\r{{\"a\":1}}\r@")] {
            assert!(SaveFile::parse(garbage).and_then(|s| check_settings(&s.settings)).is_err(), "{garbage:?}");
        }
        // Sections in the wrong places (a world object where the players belong): no silent shift.
        let swapped = good.replacen("\"Convict-1\"", "\"x\",\"gId\":\"Iron\"", 1).replacen("\"playerPosition\":\"1,2,3\",", "", 1);
        assert!(SaveFile::parse(&swapped).err().is_some_and(|e| e.contains("world objects in section 3")), "{swapped}");
    }

    #[test]
    fn settings_of_an_old_game_version_are_refused_naming_the_import() {
        assert!(check_settings(&json!({"mode": "Hardcore"})).unwrap_err().contains("Hardcore"));
        assert!(check_settings(&json!({"mode": "Hardcore"})).unwrap_err().contains("pcds world import"));
        assert!(check_settings(&json!({"mode": "Chill", "dyingConsequencesLabel": "DropAllItems"})).is_ok());
        assert!(check_settings(&json!({"planetId": "Prime"})).is_ok());
        assert!(check_settings(&Value::Null).unwrap_err().contains("cut short"));
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

    /// The wall clock stepped back (NTP, a wrong RTC at boot): backups stamped ahead of now do not
    /// block the next backup, are the first to be pruned, and are not "the newest" for S3. The
    /// copy just made is never the one removed, whatever `keep` is.
    #[test]
    fn backups_stamped_in_the_future_block_nothing_and_go_first() {
        let dir = std::env::temp_dir().join(format!("pcds-backup-future-{}", std::process::id()));
        let backups = dir.join("backups");
        std::fs::create_dir_all(&backups).unwrap();
        let save = dir.join("world.json");
        std::fs::write(&save, "v1").unwrap();
        let ahead = |secs: u64| format!("world-{}.json", stamp(SystemTime::now() + Duration::from_secs(secs)));
        let behind = |secs: u64| format!("world-{}.json", stamp(SystemTime::now() - Duration::from_secs(secs)));
        let (f1, f2, p1) = (ahead(3600), ahead(7200), behind(86_400));
        for f in [&f1, &f2, &p1] {
            std::fs::write(backups.join(f), "old").unwrap();
        }
        let names = || {
            let mut n: Vec<String> = std::fs::read_dir(&backups).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
            n.sort();
            n
        };
        // A backup is made although two are stamped ahead; room for all four.
        assert!(backup(save.to_str().unwrap(), Duration::from_secs(3600), 4).unwrap());
        assert_eq!(names().len(), 4);
        // The newest for S3 is the one just made, not a stamp from the future.
        let newest = crate::s3::newest_backup(save.to_str().unwrap()).unwrap();
        assert_eq!(std::fs::read_to_string(&newest).unwrap(), "v1");
        assert!(![&f1, &f2].contains(&&newest.file_name().unwrap().to_string_lossy().to_string()));
        // Room for two: the futures go before the day-old past one, and the new copy stays. (The copy
        // of v1 goes first: its stamp is the second the test is in, and a second copy a second later
        // would be one more past backup.)
        for n in names() {
            if ![&f1, &f2, &p1].contains(&&n) {
                std::fs::remove_file(backups.join(n)).unwrap();
            }
        }
        std::fs::write(&save, "v2").unwrap();
        assert!(backup(save.to_str().unwrap(), Duration::ZERO, 2).unwrap());
        let left = names();
        assert_eq!(left.len(), 2, "{left:?}");
        assert!(left.contains(&p1) && !left.contains(&f1) && !left.contains(&f2), "{left:?}");
        // Room for one: only the copy just made is left.
        std::fs::write(&save, "v3").unwrap();
        std::fs::write(backups.join(ahead(60)), "old").unwrap();
        assert!(backup(save.to_str().unwrap(), Duration::ZERO, 1).unwrap());
        let left = names();
        assert_eq!(left.len(), 1, "{left:?}");
        assert_eq!(std::fs::read_to_string(backups.join(&left[0])).unwrap(), "v3");
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(any(fuzzing, pcds_corpus))]
#[path = "../fuzz/targets/save.rs"]
pub mod fuzz;
