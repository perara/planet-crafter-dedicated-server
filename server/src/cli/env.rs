//! The settings (run/server.env), the world names, and which game tables go with the game.

use std::fs;
use std::path::{Path, PathBuf};

use super::{say, Ctx, Result, APP_ID};
use crate::updater;

/// The KEY=VALUE lines of a settings file, in order; comments and blank lines left out, quotes
/// around a value taken off (as systemd's EnvironmentFile= reads it).
pub fn parse_env(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.trim().split_once('=')?;
            let key = key.trim();
            if key.is_empty() || key.starts_with('#') || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return None;
            }
            let value = value.trim();
            let unquoted = ['"', '\''].iter().find_map(|q| value.strip_prefix(*q)?.strip_suffix(*q)).unwrap_or(value);
            Some((key.to_string(), unquoted.to_string()))
        })
        .collect()
}

/// The text with KEY set to VALUE: every KEY= line replaced, else one added at the end.
pub fn set_env_text(text: &str, key: &str, value: &str) -> String {
    let prefix = format!("{key}=");
    let mut found = false;
    let mut out: Vec<String> = text
        .lines()
        .map(|line| {
            if line.starts_with(&prefix) {
                found = true;
                format!("{key}={value}")
            } else {
                line.to_string()
            }
        })
        .collect();
    if !found {
        out.push(format!("{key}={value}"));
    }
    out.join("\n") + "\n"
}

/// One setting of a settings file changed (written beside and moved over, mode 600: it holds
/// the admin token).
pub fn set_env_file(path: &Path, key: &str, value: &str) -> Result {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    super::replace_file(path, set_env_text(&text, key, value).as_bytes(), 0o600)
}

/// A world name from an argument (a save's file name works too).
pub fn world_arg(arg: &str) -> Result<String> {
    let name = arg.strip_suffix(".json").unwrap_or(arg);
    let name = name.rsplit('/').next().unwrap_or(name);
    let ok = name.len() <= 64
        && name.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    if !ok {
        return Err(format!("world names are letters, digits, '.', '_' and '-' (up to 64): '{arg}'"));
    }
    Ok(name.to_string())
}

/// The game build: a hash of the game's code, which changes with every game update. One way
/// for every installation (Steam, steamcmd or a copied folder).
pub fn build_id(game: &Path) -> Result<String> {
    let path = game.join("Planet Crafter_Data/Managed/Assembly-CSharp.dll");
    let code = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!("{:016x}", xxhash_rust::xxh64::xxh64(&code, 0)))
}

/// What `pcds config migrate` would do to a server.env and a rules.json.
#[derive(Debug, Default, PartialEq)]
pub struct Migration {
    /// The new server.env.
    pub env: String,
    /// The new rules.json (None: unchanged).
    pub rules: Option<serde_json::Value>,
    /// One line per retired setting found: what became of it. Never a secret's value.
    pub report: Vec<String>,
    /// The names that stay in server.env.
    pub kept: Vec<String>,
    /// Retired settings it cannot move: left where they are, for the owner.
    pub by_hand: usize,
}

/// A retired setting's text as its rule's JSON value (None: not what the rule takes).
fn rule_value(conv: crate::config::Conv, text: &str) -> Option<serde_json::Value> {
    use crate::config::Conv;
    match conv {
        Conv::Number => text.parse::<u64>().ok().map(Into::into),
        Conv::OnOff => match text {
            "on" => Some(true.into()),
            "off" => Some(false.into()),
            _ => None,
        },
        Conv::Flag => Some(true.into()),
    }
}

/// The settings that were only ever read from the environment of a first `pcds setup`, or that
/// a launcher sets: in a server.env they do nothing, so the line goes.
const DEAD_IN_SERVER_ENV: &[&str] = &["PCDS_PORT", "PCDS_ADMIN_BIND", "PCDS_ENV_FILE", "PCDS_BANKS", "PCDS_BANK"];

/// The retired settings of a server.env moved: into `rules` (the text of rules.json, if there is
/// one) when they are rules, dropped when they did nothing there, and left, with what to do,
/// when only the owner can say (a rules file elsewhere, a page of their own, a developer hook).
/// A rule that rules.json sets already keeps its value there. `home` is PCDS_HOME.
pub fn migrate_plan(env_text: &str, rules_text: Option<&str>, home: &Path) -> Result<Migration> {
    use crate::config::{retired_message, Fate, RETIRED};
    let mut rules: serde_json::Value = match rules_text.map(str::trim) {
        None | Some("") => serde_json::json!({}),
        Some(text) => serde_json::from_str(text).map_err(|e| format!("rules.json: not JSON ({e}); fix or move it away first"))?,
    };
    let defaults = serde_json::to_value(crate::rules::Rules::default()).unwrap_or_default();
    let mut m = Migration::default();
    let mut changed = false;
    // The lines to drop, and those of their comments.
    let lines: Vec<&str> = env_text.lines().collect();
    let mut drop = vec![false; lines.len()];
    let parsed = |line: &str| parse_env(line).into_iter().next();
    for (i, line) in lines.iter().enumerate() {
        let Some((key, value)) = parsed(line) else { continue };
        let Some((name, fate)) = RETIRED.iter().find(|(n, _)| *n == key) else {
            m.kept.push(key);
            continue;
        };
        let said = retired_message(name, *fate);
        match fate {
            _ if value.is_empty() => {
                m.report.push(format!("{name}= (empty)  -> removed: it set nothing"));
                drop[i] = true;
            }
            Fate::Rule(rule, conv) => {
                let (section, field) = rule.split_once('.').unwrap_or_default();
                let Some(new) = rule_value(*conv, &value) else {
                    m.report.push(format!("{name}={value}  -> NOT moved: {said}, and {value:?} is not what that rule takes; set it yourself"));
                    m.by_hand += 1;
                    continue;
                };
                let default = match crate::config::at(&defaults, rule) {
                    serde_json::Value::Null if *rule == "limits.maxPlayers" => crate::rules::DEFAULT_MAX_PLAYERS.into(),
                    d => d,
                };
                let set = crate::config::at(&rules, rule);
                drop[i] = true;
                if !set.is_null() {
                    m.report.push(format!("{name}={value}  -> removed: rules.json sets {rule}={set} already, and that stays"));
                } else if new == default {
                    m.report.push(format!("{name}={value}  -> removed: {rule} is {value} by default, nothing to write"));
                } else {
                    rules[section][field] = new;
                    changed = true;
                    m.report.push(format!("{name}={value}  -> rules.json: {rule} = {value}"));
                }
            }
            Fate::File(file) if std::path::Path::new(&value) == home.join(file) => {
                m.report.push(format!("{name}={value}  -> removed: it is where {file} is anyway"));
                drop[i] = true;
            }
            Fate::Gone(_) if DEAD_IN_SERVER_ENV.contains(name) => {
                m.report.push(format!("{name}={value}  -> removed: {said}"));
                drop[i] = true;
            }
            _ => {
                m.report.push(format!("{name}={value}  -> NOT moved: {said}"));
                m.by_hand += 1;
            }
        }
    }
    // A comment block right above a dropped line described it: it goes too.
    for i in 0..lines.len() {
        if drop[i] {
            let mut j = i;
            while j > 0 && lines[j - 1].trim_start().starts_with('#') && !drop[j - 1] {
                drop[j - 1] = true;
                j -= 1;
            }
        }
    }
    let kept: Vec<&str> = lines.iter().enumerate().filter(|(i, _)| !drop[*i]).map(|(_, l)| *l).collect();
    m.env = if kept.is_empty() { String::new() } else { kept.join("\n") + "\n" };
    if changed {
        crate::rules::Rules::default().patched(&rules).map_err(|e| format!("the moved settings do not make valid rules: {e}"))?;
        m.rules = Some(rules);
    }
    Ok(m)
}

impl Ctx {
    /// `pcds config migrate [--apply]`: server.env's retired settings into the rules.json and
    /// server.env of this version. Without --apply it only says what it would do.
    pub fn config_migrate(&self, apply: bool) -> Result {
        let text = fs::read_to_string(&self.env_file).map_err(|e| format!("{}: {e}", self.env_file.display()))?;
        let rules_file = self.run.join("rules.json");
        let rules_text = match fs::read_to_string(&rules_file) {
            Ok(t) => Some(t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("{}: {e}", rules_file.display())),
        };
        let plan = migrate_plan(&text, rules_text.as_deref(), &self.run)?;
        if plan.report.is_empty() {
            say(format!("{}: nothing to migrate", self.env_file.display()));
            return Ok(());
        }
        say(format!("{} ({})", self.env_file.display(), if apply { "migrating" } else { "dry run: nothing is written, --apply writes it" }));
        plan.report.iter().for_each(|line| println!("  {line}"));
        // Names only: the values of the settings that stay are never shown (the admin token is one).
        println!("  server.env keeps: {}", if plan.kept.is_empty() { "nothing".to_string() } else { plan.kept.join(", ") });
        if let Some(rules) = &plan.rules {
            println!("  {} gets: {}", rules_file.display(), serde_json::to_string(rules).unwrap_or_default());
        }
        if !apply {
            return Ok(());
        }
        // rules.json first: a stop in between leaves the old settings in server.env, which the
        // next run of this command finds again (rules.json then has them already).
        if let Some(rules) = &plan.rules {
            let body = serde_json::to_string_pretty(rules).unwrap_or_default() + "\n";
            super::replace_file(&rules_file, body.as_bytes(), 0o644)?;
        }
        let backup = self.env_file.with_file_name("server.env.before-migrate");
        super::replace_file(&backup, text.as_bytes(), 0o600)?;
        super::replace_file(&self.env_file, plan.env.as_bytes(), 0o600)?;
        say(format!("done; the old file is kept as {} (it holds the admin token: delete it when the server runs)", backup.display()));
        if plan.by_hand > 0 {
            return Err(format!("{} setting(s) are left in server.env for you (marked NOT moved above); the server refuses to start until they are gone", plan.by_hand));
        }
        Ok(())
    }
}

impl Ctx {
    /// A setting from server.env (the last line for it); None when empty or missing.
    pub fn env_get(&self, key: &str) -> Option<String> {
        let text = fs::read_to_string(&self.env_file).ok()?;
        parse_env(&text).into_iter().rev().find(|(k, _)| k == key).map(|(_, v)| v).filter(|v| !v.is_empty())
    }

    /// One setting changed: the new file is written beside (only this user reads it, as it holds
    /// the admin token) and moved over, so a crash leaves the old one or the new one.
    pub fn set_env(&self, key: &str, value: &str) -> Result {
        let text = fs::read_to_string(&self.env_file).map_err(|e| format!("{}: {e}", self.env_file.display()))?;
        super::replace_file(&self.env_file, set_env_text(&text, key, value).as_bytes(), 0o600)
    }

    /// The direct-IP port (PCDS_LISTEN's).
    pub fn port(&self) -> String {
        let listen = self.env_get("PCDS_LISTEN").unwrap_or_default();
        listen.rsplit(':').next().unwrap_or_default().to_string()
    }

    pub fn gui_url(&self) -> String {
        format!("http://{}/#token={}", self.env_get("PCDS_ADMIN").unwrap_or_default(), self.env_get("PCDS_ADMIN_TOKEN").unwrap_or_default())
    }

    /// The settings with their defaults, once; the admin token makes it this user's only.
    pub fn write_env(&self) -> Result {
        if self.env_file.exists() {
            return Ok(());
        }
        super::mkdir(&self.run)?;
        // The first file starts from the environment's addresses (the Docker image sets PCDS_ADMIN,
        // a second server gets PCDS_LISTEN), so a setting that moved is refused here too.
        crate::config::check_env(crate::env)?;
        let listen = crate::env("PCDS_LISTEN").unwrap_or_else(|| "0.0.0.0:7777".into());
        let admin = crate::env("PCDS_ADMIN").unwrap_or_else(|| "127.0.0.1:7782".into());
        let token = super::token()?;
        let text = format!(
            "# Planet Crafter dedicated server settings. Apply with: pcds restart
# Saves, backups, updates and the rest of what is tuned are rules (run/rules.json, the admin page).
# Direct IP (UDP): players join with HOST:PORT; \"off\" disables.
PCDS_LISTEN={listen}
# Invite codes through Steam (an anonymous Steam game server; needs steamclient.so,
# see README); \"off\" disables.
PCDS_STEAM=on
# A game server login token (https://steamcommunity.com/dev/managegameservers, app {APP_ID})
# keeps the invite code the same across restarts; without one it changes at every start.
PCDS_STEAM_TOKEN=
# Last four digits of the invite code.
PCDS_SESSION=4242
# Admin API and web page (pcds gui); keep it on 127.0.0.1 or behind a proxy. \"off\" disables.
PCDS_ADMIN={admin}
PCDS_ADMIN_TOKEN={token}
# The world the server runs: worlds/<name>.json (pcds worlds, pcds world use NAME).
PCDS_WORLD=world
"
        );
        super::replace_file(&self.env_file, text.as_bytes(), 0o600)?;
        say(format!("settings written to {}", self.env_file.display()));
        Ok(())
    }

    /// The world the server runs: worlds/<PCDS_WORLD>.json.
    pub fn world_name(&self) -> String {
        crate::env("PCDS_WORLD").or_else(|| self.env_get("PCDS_WORLD")).unwrap_or_else(|| "world".into())
    }

    pub fn world_file(&self) -> PathBuf {
        self.worlds.join(format!("{}.json", self.world_name()))
    }

    /// The tables of this game build for the program that runs the server: run/data/<version>/
    /// whose buildid is the build and whose tables-format is that program's.
    pub fn tables(&self, build: &str) -> Result<PathBuf> {
        find_tables(&self.run.join("data"), build, self.tables_format()?)
    }

    /// The program that runs the server and reads its tables: the active bank's, else (no
    /// bank installed yet) this one.
    pub fn program(&self) -> PathBuf {
        let banks = self.banks();
        let program = banks.join(updater::active(&banks)).join("pcds");
        if program.is_file() { program } else { self.exe.clone() }
    }

    /// The format of the game tables program() reads and writes (its bank.json says).
    pub fn tables_format(&self) -> Result<u32> {
        let banks = self.banks();
        let active = updater::active(&banks);
        if !banks.join(&active).join("pcds").is_file() {
            return Ok(crate::native::tables::TABLES_FORMAT);
        }
        let info = updater::info(&banks, &active).ok_or_else(|| format!("bank {active} has no readable bank.json in {}", banks.display()))?;
        let format = info["tablesFormat"].as_u64().and_then(|f| u32::try_from(f).ok());
        format.ok_or_else(|| {
            format!(
                "bank {active} (pcds {}) does not say which game tables it reads (a build from before table formats): put a newer one in its place (make build and pcds restart, or pcds upgrade --restart)",
                info["version"].as_str().unwrap_or("?")
            )
        })
    }

    /// The tables of the installed game.
    pub fn data_dir(&self) -> Result<PathBuf> {
        let game = self.game_dir(None)?;
        self.tables(&build_id(&game)?)
    }
}

/// The tables in `data` (run/data) for a game build in a tables format. Folders whose name
/// starts with "." are extractions not finished yet.
pub fn find_tables(data: &Path, build: &str, format: u32) -> Result<PathBuf> {
    let (mut any, mut other_format) = (false, None);
    for entry in fs::read_dir(data).into_iter().flatten().flatten() {
        let dir = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') || !dir.is_dir() {
            continue;
        }
        any = true;
        if !fs::read_to_string(dir.join("buildid")).is_ok_and(|id| id.trim() == build) {
            continue;
        }
        let theirs = fs::read_to_string(dir.join("tables-format")).map_or_else(|_| "none".to_string(), |f| f.trim().to_string());
        if theirs == format.to_string() {
            return Ok(dir);
        }
        other_format = Some((dir, theirs));
    }
    if let Some((dir, theirs)) = other_format {
        return Err(format!("the tables in {} are of tables format {theirs}, the server reads format {format}: run `pcds update`, which reads them again", dir.display()));
    }
    if !any {
        return Err(format!("no game tables in {}: run `pcds update` (see README)", data.display()));
    }
    Err(format!("the game changed: run `pcds update` (no tables in {} for game build {build})", data.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server.env as pcds 0.6 wrote it (with the owner's changes) becomes a new server.env and a
    /// rules.json patch; what only the owner can place stays, and no secret is in the report.
    #[test]
    fn migration_moves_what_it_can_and_leaves_the_rest() {
        let old = "# Planet Crafter dedicated server settings. Apply with: pcds restart
# Direct IP (UDP): players join with HOST:PORT; \"off\" disables.
PCDS_LISTEN=0.0.0.0:7777
PCDS_STEAM=on
PCDS_STEAM_TOKEN=STEAMSECRET
# Last four digits of the invite code.
PCDS_SESSION=4242
# Minutes between saves (0: only on stop), and the save backups kept in worlds/backups/.
PCDS_AUTOSAVE=10
PCDS_BACKUPS=24
PCDS_BACKUP_MINUTES=30
# Admin API and web page.
PCDS_ADMIN=127.0.0.1:7782
PCDS_ADMIN_TOKEN=ADMINSECRET
PCDS_WORLD=world
PCDS_MAX_PLAYERS=8
PCDS_AUTO_UPDATE=on
PCDS_UPDATE_CHECK_HOURS=24
PCDS_TRACE=1
PCDS_PORT=7778
PCDS_RULES=/srv/run/rules.json
PCDS_ACCESS=/elsewhere/access.json
PCDS_WEB=/my/page.html
";
        let home = Path::new("/srv/run");
        let m = migrate_plan(old, Some("{\"speed\": {\"growth\": 2}, \"save\": {\"backupMinutes\": 45}}"), home).unwrap();
        // Moved into the rules: what differs from the default, and what rules.json does not set.
        assert_eq!(
            m.rules.unwrap(),
            serde_json::json!({"speed": {"growth": 2}, "save": {"autosaveMinutes": 10, "backupMinutes": 45}, "limits": {"maxPlayers": 8}, "update": {"auto": true}, "log": {"trace": true}})
        );
        let report = m.report.join("\n");
        for line in [
            "PCDS_AUTOSAVE=10  -> rules.json: save.autosaveMinutes = 10",
            "PCDS_BACKUPS=24  -> removed: save.backupsKept is 24 by default, nothing to write",
            "PCDS_BACKUP_MINUTES=30  -> removed: rules.json sets save.backupMinutes=45 already, and that stays",
            "PCDS_UPDATE_CHECK_HOURS=24  -> removed: update.checkHours is 24 by default",
            "PCDS_PORT=7778  -> removed: PCDS_PORT is gone",
            "PCDS_RULES=/srv/run/rules.json  -> removed: it is where rules.json is anyway",
            "PCDS_ACCESS=/elsewhere/access.json  -> NOT moved: PCDS_ACCESS is gone: the file is access.json in PCDS_HOME",
            "PCDS_WEB=/my/page.html  -> NOT moved: PCDS_WEB is gone",
        ] {
            assert!(report.contains(line), "{line}\n{report}");
        }
        assert_eq!(m.by_hand, 2);
        assert!(!report.contains("SECRET") && !report.contains("PCDS_STEAM_TOKEN"), "{report}");
        // The new server.env: the settings that stay, the owner's lines left, the comments of
        // the moved ones gone.
        assert_eq!(
            m.env,
            "# Planet Crafter dedicated server settings. Apply with: pcds restart
# Direct IP (UDP): players join with HOST:PORT; \"off\" disables.
PCDS_LISTEN=0.0.0.0:7777
PCDS_STEAM=on
PCDS_STEAM_TOKEN=STEAMSECRET
# Last four digits of the invite code.
PCDS_SESSION=4242
# Admin API and web page.
PCDS_ADMIN=127.0.0.1:7782
PCDS_ADMIN_TOKEN=ADMINSECRET
PCDS_WORLD=world
PCDS_ACCESS=/elsewhere/access.json
PCDS_WEB=/my/page.html
"
        );
        assert_eq!(m.kept, ["PCDS_LISTEN", "PCDS_STEAM", "PCDS_STEAM_TOKEN", "PCDS_SESSION", "PCDS_ADMIN", "PCDS_ADMIN_TOKEN", "PCDS_WORLD"]);
        // Nothing to move: nothing planned, and the new file is the old one.
        let clean = "PCDS_LISTEN=off\n# kept\nPCDS_WORLD=a\n";
        let m = migrate_plan(clean, None, home).unwrap();
        assert!(m.report.is_empty() && m.rules.is_none() && m.env == clean);
        // A moved value that is out of range is not written.
        let m = migrate_plan("PCDS_AUTOSAVE=100000\nPCDS_BACKUPS=x\n", None, home);
        assert!(m.is_err() || m.as_ref().is_ok_and(|m| m.rules.is_none() && m.by_hand >= 1), "{m:?}");
        assert!(migrate_plan("PCDS_AUTOSAVE=7\n", Some("[1"), home).is_err(), "a rules.json that is not JSON is not touched");
    }

    /// The settings file a first setup writes holds the defaults of config::KNOBS (nothing in
    /// the environment changes the port or the admin address here).
    #[test]
    fn the_first_settings_file_has_the_defaults_of_the_schema() {
        let dir = super::super::test_dir("env-defaults");
        let ctx = super::super::test_ctx(&dir);
        ctx.write_env().unwrap();
        let env = parse_env(&fs::read_to_string(&ctx.env_file).unwrap());
        for key in ["PCDS_LISTEN", "PCDS_SESSION", "PCDS_WORLD", "PCDS_STEAM", "PCDS_ADMIN"] {
            let written = env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
            assert_eq!(written, Some(crate::config::knob_for(key).unwrap().default), "{key}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn settings_are_read_and_edited_line_by_line() {
        let text = "# PCDS_WORLD=commented\nPCDS_LISTEN=0.0.0.0:7777\nPCDS_STEAM_TOKEN=\nPCDS_WORLD=\"base\"\n  PCDS_SESSION = 1234 \nnot a setting\nPCDS_WORLD=second\n";
        let env = parse_env(text);
        let get = |k: &str| env.iter().rev().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        assert_eq!(get("PCDS_LISTEN"), Some("0.0.0.0:7777"));
        assert_eq!(get("PCDS_STEAM_TOKEN"), Some(""));
        assert_eq!(get("PCDS_WORLD"), Some("second"), "the last line wins");
        assert_eq!(get("PCDS_SESSION"), Some("1234"));
        assert_eq!(env.len(), 5);
        assert_eq!(parse_env("A='quoted value'")[0].1, "quoted value");

        let edited = set_env_text(text, "PCDS_WORLD", "third");
        assert_eq!(edited.matches("PCDS_WORLD=third").count(), 2);
        assert!(edited.contains("# PCDS_WORLD=commented") && edited.contains("PCDS_LISTEN=0.0.0.0:7777\n"));
        assert_eq!(set_env_text("A=1\n", "B", "2"), "A=1\nB=2\n");
        assert_eq!(set_env_text("", "B", "2"), "B=2\n");
    }

    #[test]
    fn a_setting_is_changed_by_replacing_the_file() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = super::super::test_dir("set-env");
        let ctx = super::super::test_ctx(&dir);
        fs::create_dir_all(&ctx.run).unwrap();
        fs::write(&ctx.env_file, "# settings\nPCDS_ADMIN_TOKEN=secret\nPCDS_WORLD=world\n").unwrap();
        fs::set_permissions(&ctx.env_file, fs::Permissions::from_mode(0o644)).unwrap();
        let before = fs::metadata(&ctx.env_file).unwrap().ino();
        ctx.set_env("PCDS_WORLD", "other").unwrap();
        assert_eq!(fs::read_to_string(&ctx.env_file).unwrap(), "# settings\nPCDS_ADMIN_TOKEN=secret\nPCDS_WORLD=other\n");
        let after = fs::metadata(&ctx.env_file).unwrap();
        assert_ne!(after.ino(), before, "moved over, not rewritten in place");
        assert_eq!(after.mode() & 0o777, 0o600);
        assert!(!dir.join("run/server.env.new").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn world_names() {
        assert_eq!(world_arg("world").unwrap(), "world");
        assert_eq!(world_arg("my.world_2-b").unwrap(), "my.world_2-b");
        assert_eq!(world_arg("/home/me/saves/Standard-1.json").unwrap(), "Standard-1");
        assert_eq!(world_arg(&"a".repeat(64)).unwrap().len(), 64);
        for bad in ["", ".hidden", "-x", "two words", "ünï", "a/", &"a".repeat(65)] {
            assert!(world_arg(bad).is_err(), "{bad}");
        }
    }

    /// The tables of a game build in the active bank's tables format; another format or build
    /// is named in the error.
    #[test]
    fn tables_are_found_by_build_and_format() {
        let dir = super::super::test_dir("find-tables");
        let ctx = super::super::test_ctx(&dir);
        let data = dir.join("run/data");
        let tables = |name: &str, build: &str, format: Option<&str>| {
            fs::create_dir_all(data.join(name)).unwrap();
            fs::write(data.join(name).join("buildid"), build).unwrap();
            if let Some(f) = format {
                fs::write(data.join(name).join("tables-format"), f).unwrap();
            }
        };
        assert!(ctx.tables("b1").unwrap_err().contains("no game tables"));
        let ours = crate::native::tables::TABLES_FORMAT;
        tables("2.103", "b1", None);
        tables(".new-1", "b1", Some(&ours.to_string()));
        let e = ctx.tables("b1").unwrap_err();
        assert!(e.contains("tables format none") && e.contains("pcds update"), "{e}");
        assert!(ctx.tables("b2").unwrap_err().contains("game build b2"));
        fs::write(data.join("2.103/tables-format"), format!("{ours}\n")).unwrap();
        assert_eq!(ctx.tables("b1").unwrap(), data.join("2.103"));
        // The active bank's program reads another format: its bank.json says which.
        let bank = dir.join("run/bin/a");
        fs::create_dir_all(&bank).unwrap();
        fs::write(bank.join("pcds"), "").unwrap();
        updater::write_info(&bank, &serde_json::json!({"version": "9.0.0", "tablesFormat": ours + 1})).unwrap();
        assert_eq!(ctx.program(), bank.join("pcds"));
        assert!(ctx.tables("b1").unwrap_err().contains(&format!("the server reads format {}", ours + 1)));
        updater::write_info(&bank, &serde_json::json!({"version": "0.5.9"})).unwrap();
        assert!(ctx.tables("b1").unwrap_err().contains("from before table formats"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_build_id_is_a_hash_of_the_game_code() {
        let dir = std::env::temp_dir().join(format!("pcds-build-id-{}", std::process::id()));
        fs::create_dir_all(dir.join("Planet Crafter_Data/Managed")).unwrap();
        fs::write(dir.join("Planet Crafter_Data/Managed/Assembly-CSharp.dll"), b"code").unwrap();
        let id = build_id(&dir).unwrap();
        assert_eq!(id, format!("{:016x}", xxhash_rust::xxh64::xxh64(b"code", 0)));
        assert_eq!(id.len(), 16);
        fs::remove_dir_all(&dir).unwrap();
        assert!(build_id(&dir).is_err());
    }
}
