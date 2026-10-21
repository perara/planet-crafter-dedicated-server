//! The settings (run/server.env), the world names, and which game tables go with the game.

use std::fs;
use std::path::PathBuf;

use super::{say, Ctx, Result, APP_ID};
use crate::config::{parse_env, set_env_text};
use crate::tables::{build_id, find_tables};
use crate::updater;

impl Ctx {
    /// A setting from server.env (the last line for it); None when empty or missing.
    pub fn env_get(&self, key: &str) -> Option<String> {
        let text = fs::read_to_string(&self.env_file).ok()?;
        parse_env(&text).into_iter().rev().find(|(k, _)| k == key).map(|(_, v)| v).filter(|v| !v.is_empty())
    }

    /// A setting as the server reads it: server.env, else the process environment (the Docker image
    /// sets some there), else the default (docs/env.md). The commands that talk to the running
    /// server use this, so a server.env that lists only what differs works with them.
    pub fn effective(&self, key: &str) -> Option<String> {
        self.env_get(key).or_else(|| crate::env(key)).or_else(|| crate::config::knob_for(key).map(|k| k.default.to_string()).filter(|d| !d.is_empty()))
    }

    /// server.env, and the environment under it, as the server will check them at its start: Err
    /// with what stops it (a name that is no setting, a value that is not what it takes). `pcds
    /// start` and `restart` say so at once, not in the journal of a unit that fails.
    pub fn check_settings(&self) -> Result {
        self.check_settings_file()?;
        let file = fs::read_to_string(&self.env_file).map(|t| parse_env(&t)).unwrap_or_default();
        crate::config::check_env(|key| file.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.clone()).filter(|v| !v.is_empty()).or_else(|| crate::env(key)))
    }

    /// Err when server.env names a PCDS_ setting that does not exist.
    pub fn check_settings_file(&self) -> Result {
        let file = fs::read_to_string(&self.env_file).map(|t| parse_env(&t)).unwrap_or_default();
        crate::config::check_keys(&self.env_file.display().to_string(), file.into_iter().map(|(k, _)| k))
    }

    /// One setting changed: the new file is written beside (only this user reads it, as it holds
    /// the admin token) and moved over, so a crash leaves the old one or the new one.
    pub fn set_env(&self, key: &str, value: &str) -> Result {
        let text = fs::read_to_string(&self.env_file).map_err(|e| format!("{}: {e}", self.env_file.display()))?;
        crate::util::replace_file(&self.env_file, set_env_text(&text, key, value).as_bytes(), 0o600)
    }

    /// The direct-IP port (PCDS_LISTEN's).
    pub fn port(&self) -> String {
        let listen = self.effective("PCDS_LISTEN").unwrap_or_default();
        listen.rsplit(':').next().unwrap_or_default().to_string()
    }

    pub fn gui_url(&self) -> String {
        format!("http://{}/#token={}", self.effective("PCDS_ADMIN").unwrap_or_default(), self.effective("PCDS_ADMIN_TOKEN").unwrap_or_default())
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
        crate::util::replace_file(&self.env_file, text.as_bytes(), 0o600)?;
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
    pub fn program(&self) -> Result<PathBuf> {
        let banks = self.banks();
        let program = banks.join(updater::active(&banks)?).join("pcds");
        Ok(if program.is_file() { program } else { self.exe.clone() })
    }

    /// The format of the game tables program() reads and writes (its bank.json says).
    pub fn tables_format(&self) -> Result<u32> {
        let banks = self.banks();
        let active = updater::active(&banks)?;
        if !banks.join(&active).join("pcds").is_file() {
            return Ok(crate::native::tables::TABLES_FORMAT);
        }
        let info = updater::info(&banks, &active).ok_or_else(|| format!("bank {active} has no readable bank.json in {}", banks.display()))?;
        let format = info.tables_format.and_then(|f| u32::try_from(f).ok());
        format.ok_or_else(|| format!("bank {active} (pcds {}) does not say which game tables it reads: reinstall it (make build and pcds restart, or pcds upgrade --restart)", info.version()))
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

    /// A server.env that lists only what differs (the Docker image's, a hand-written one) works with the
    /// commands that talk to the server: the admin address and the game port are the documented defaults.
    #[test]
    fn a_server_env_without_the_addresses_means_the_defaults() {
        let dir = super::super::test_dir("env-effective");
        let ctx = super::super::test_ctx(&dir);
        fs::create_dir_all(&ctx.run).unwrap();
        fs::write(&ctx.env_file, "PCDS_STEAM=off\nPCDS_ADMIN_TOKEN=secret\n").unwrap();
        assert_eq!(ctx.effective("PCDS_ADMIN").as_deref(), Some("127.0.0.1:7782"));
        assert_eq!(ctx.port(), "7777");
        assert_eq!(ctx.gui_url(), "http://127.0.0.1:7782/#token=secret");
        fs::write(&ctx.env_file, "PCDS_ADMIN=127.0.0.1:9\nPCDS_LISTEN=off\n").unwrap();
        assert_eq!((ctx.effective("PCDS_ADMIN").as_deref(), ctx.effective("PCDS_LISTEN").as_deref()), (Some("127.0.0.1:9"), Some("off")), "the file wins");
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
        updater::write_info(&bank, &crate::msg::BankInfo { tables_format: Some(u64::from(ours) + 1), ..crate::msg::BankInfo::new("9.0.0", 0, 0, 0) }).unwrap();
        assert_eq!(ctx.program().unwrap(), bank.join("pcds"));
        assert!(ctx.tables("b1").unwrap_err().contains(&format!("the server reads format {}", ours + 1)));
        updater::write_info(&bank, &crate::msg::BankInfo { version: Some("9.0.0".into()), ..Default::default() }).unwrap();
        assert!(ctx.tables("b1").unwrap_err().contains("does not say which game tables it reads"));
        fs::remove_dir_all(&dir).unwrap();
    }

}
