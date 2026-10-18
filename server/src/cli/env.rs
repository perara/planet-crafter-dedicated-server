//! The settings (run/server.env), the world names, and which game tables go with the game.

use std::fs;
use std::path::{Path, PathBuf};

use super::{say, Ctx, Result, APP_ID};

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
        let port = crate::env("PCDS_PORT").unwrap_or_else(|| "7777".into());
        let admin = crate::env("PCDS_ADMIN_BIND").unwrap_or_else(|| "127.0.0.1:7782".into());
        let token = super::token()?;
        let text = format!(
            "# Planet Crafter dedicated server settings. Apply with: pcds restart
# Direct IP (UDP): players join with HOST:PORT; \"off\" disables.
PCDS_LISTEN=0.0.0.0:{port}
# Invite codes through Steam (an anonymous Steam game server; needs steamclient.so,
# see README); \"off\" disables.
PCDS_STEAM=on
# A game server login token (https://steamcommunity.com/dev/managegameservers, app {APP_ID})
# keeps the invite code the same across restarts; without one it changes at every start.
PCDS_STEAM_TOKEN=
# Last four digits of the invite code.
PCDS_SESSION=4242
# Minutes between saves (0: only on stop), and the save backups kept in worlds/backups/.
PCDS_AUTOSAVE=5
PCDS_BACKUPS=24
PCDS_BACKUP_MINUTES=60
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

    /// The tables extracted from this game build: run/data/<version>/ whose buildid is it.
    pub fn tables(&self, build: &str) -> Result<PathBuf> {
        let data = self.run.join("data");
        let mut any = false;
        for entry in fs::read_dir(&data).into_iter().flatten().flatten() {
            let dir = entry.path();
            // Hidden: an extraction not finished yet.
            if entry.file_name().to_string_lossy().starts_with('.') || !dir.is_dir() {
                continue;
            }
            any = true;
            if fs::read_to_string(dir.join("buildid")).is_ok_and(|id| id.trim() == build) {
                return Ok(dir);
            }
        }
        if !any {
            return Err(format!("no game tables in {}: run 'pcds extract' (see README)", data.display()));
        }
        Err(format!("the game changed: run `pcds update` (no tables in {} for game build {build})", data.display()))
    }

    /// The tables of the installed game.
    pub fn data_dir(&self) -> Result<PathBuf> {
        let game = self.game_dir(None)?;
        self.tables(&build_id(&game)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
