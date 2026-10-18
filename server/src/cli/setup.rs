//! From nothing to a running server: setup, up, the Docker container's start, the selftest,
//! and `pcds run`, the server as the service runs it.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use super::env::{build_id, parse_env};
use super::{need, say, sleep, wait_for, Ctx, Result};
use crate::updater;

impl Ctx {
    /// The server runs from run/bin, so rebuilding or upgrading never touches a running server:
    /// run/bin/pcds (what the units run) with what it loads beside it, as in a release, and the
    /// banks run/bin/a and run/bin/b, each a whole install, with run/bin/active naming the one
    /// the front starts (docs/hot-update.md). This program goes into the standby bank and
    /// becomes the active one when it is newer than the active bank's and in neither bank yet
    /// (so a rollback stays rolled back); the running server moves to it at its next start, or
    /// at once with `pcds upgrade --restart`.
    pub fn install_server(&self) -> Result {
        let lib = self.exe.with_file_name("libsteam_api.so");
        if !lib.is_file() {
            return Err(format!("no libsteam_api.so next to {} (make build puts it there)", self.exe.display()));
        }
        let bin = self.run.join("bin");
        let launcher = self.installed();
        if !super::same_file(&self.exe, &launcher) && (!launcher.is_file() || super::modified(&self.exe) > super::modified(&launcher)) {
            super::mkdir(&bin.join("web"))?;
            super::install_file(&self.exe, &launcher)?;
            super::install_file(&lib, &bin.join("libsteam_api.so"))?;
            super::install_file(&self.web, &bin.join("web/index.html"))?;
        }
        let banks = self.banks();
        let active = updater::active(&banks);
        let in_bank = |b: &str| program_equal(&self.exe, &banks.join(b).join("pcds"));
        if updater::BANKS.iter().any(|b| in_bank(b)) {
            return Ok(());
        }
        let target = match updater::info(&banks, &active) {
            None => active.clone(),
            Some(_) if super::modified(&self.exe) > super::modified(&banks.join(&active).join("pcds")) => updater::other(&active).to_string(),
            Some(_) => return Ok(()),
        };
        self.install_bank(&target, &lib)?;
        updater::set_active(&banks, &target)?;
        say(format!("pcds {} installed in bank {target} ({}): the server runs it from its next start", env!("CARGO_PKG_VERSION"), banks.join(&target).display()));
        Ok(())
    }

    /// This program as a bank: its files beside the bank, then moved over it (the files keep
    /// their times: a later build is newer than the bank).
    fn install_bank(&self, bank: &str, lib: &Path) -> Result {
        let banks = self.banks();
        let new = banks.join(format!(".{bank}.new"));
        let _ = fs::remove_dir_all(&new);
        super::mkdir(&new.join("web"))?;
        let copy = |from: &Path, to: &Path| -> Result {
            fs::copy(from, to).map_err(|e| format!("{} -> {}: {e}", from.display(), to.display()))?;
            let time = fs::metadata(from).and_then(|m| m.modified()).map_err(|e| e.to_string())?;
            fs::File::options().write(true).open(to).and_then(|f| f.set_modified(time)).map_err(|e| format!("{}: {e}", to.display()))
        };
        let result = (|| {
            copy(&self.exe, &new.join("pcds"))?;
            copy(lib, &new.join("libsteam_api.so"))?;
            copy(&self.web, &new.join("web/index.html"))?;
            let mut info = crate::bank::info();
            info["installed"] = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0).into();
            info["from"] = self.exe.display().to_string().into();
            updater::write_info(&new, &info)?;
            updater::put(&banks, bank, &new)
        })();
        let _ = fs::remove_dir_all(&new);
        result
    }

    /// After an upgrade (a new build or release), the newer program into run/bin.
    pub fn install_newer(&self) -> Result {
        self.install_server()
    }

    /// The server in the foreground, as the unit runs it: the front from the active bank, on
    /// the installed game's tables, the world, the settings. Returns only when it cannot start.
    pub fn run_server(&self) -> Result {
        let data = self.data_dir()?;
        let world = self.world_file();
        if !world.is_file() {
            return Err(format!("no world at {}: 'pcds worlds', 'pcds world use NAME' or 'pcds world new NAME'", world.display()));
        }
        self.install_newer()?;
        let banks = self.banks();
        let front = banks.join(updater::active(&banks)).join("pcds");
        let mut server = Command::new(&front);
        server.arg("serve").arg(&data).arg(&world).env("PCDS_BANKS", &banks);
        // Started by hand: the settings file (the unit has systemd read it).
        let mut settings = HashMap::new();
        if crate::env("PCDS_ADMIN_TOKEN").is_none() && self.env_file.is_file() {
            settings.extend(parse_env(&fs::read_to_string(&self.env_file).map_err(|e| format!("{}: {e}", self.env_file.display()))?));
        }
        server.envs(&settings);
        let get = |key: &str| settings.get(key).filter(|v| !v.is_empty()).cloned().or_else(|| crate::env(key));
        // Access lists and rules are the installation's, whichever world runs.
        server.env("PCDS_ACCESS", get("PCDS_ACCESS").unwrap_or_else(|| self.run.join("access.json").display().to_string()));
        server.env("PCDS_RULES", get("PCDS_RULES").unwrap_or_else(|| self.run.join("rules.json").display().to_string()));
        Err(format!("{}: {}", front.display(), server.exec()))
    }

    pub fn setup(&self, args: &[String]) -> Result {
        let (mut game, mut world, mut steam_user, mut new, mut options) = (None, None, None, false, Vec::new());
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let mut value = |what: &str| it.next().cloned().ok_or_else(|| what.to_string());
            match arg.as_str() {
                "--game" => game = Some(PathBuf::from(value("--game DIR")?)),
                "--world" => world = Some(PathBuf::from(value("--world SAVE.json")?)),
                "--steam-user" => steam_user = Some(value("--steam-user NAME")?),
                "--new" => new = true,
                _ if new => options.push(arg.clone()),
                other => return Err(format!("unknown option {other}")),
            }
        }
        for tool in ["curl", "unzip", "tar"] {
            need(tool, "install it with your package manager")?;
        }
        if !self.container {
            need("systemctl", "pcds runs the server as a systemd user service (or use the Docker image)")?;
        }
        super::mkdir(&self.run)?;
        self.write_env()?;
        self.install_server()?;
        if let Some(user) = steam_user {
            self.download_game(&user)?;
            game = Some(self.run.join("game"));
        }
        let game = self.game_dir(game.as_deref())?;
        if self.tables(&build_id(&game)?).is_err() {
            self.extract(Some(&game))?;
        }
        if world.is_some() && new {
            return Err("--world SAVE.json and --new: one or the other".into());
        }
        let current = self.world_file();
        let name = self.world_name();
        if let Some(world) = world {
            self.replace_world(&world)?;
        } else if new {
            if current.is_file() {
                say(format!("keeping the world {name} ('pcds world new NAME' makes another)"));
            } else {
                self.new_world(&current, &options)?;
            }
        } else if !current.is_file() {
            return Err("no world yet: pass --world SAVE.json (a save from your game) or --new".into());
        }
        if !self.container {
            self.write_unit()?;
        }
        self.ensure_steamclient();
        say("ready: 'pcds start' runs it now, 'pcds install-service' starts it at login");
        Ok(())
    }

    /// `setup --world`: a save becomes the current world (the one it replaces is kept in
    /// backups/), copied beside it and then moved over it.
    fn replace_world(&self, save: &Path) -> Result {
        let (current, name) = (self.world_file(), self.world_name());
        if !save.is_file() {
            return Err(format!("no save at {}", save.display()));
        }
        if !super::is_save(save) {
            return Err(format!("{} is not a Planet Crafter save", save.display()));
        }
        if super::same_file(save, &current) {
            return Err(format!("{} is the world {name} already: leave out --world", save.display()));
        }
        if self.active() {
            return Err("stop the server first (pcds stop): it would save over the imported world".into());
        }
        if current.is_file() {
            self.keep_copy(&name, "replaced")?;
        }
        super::mkdir(&self.worlds)?;
        super::install_file(save, &current)?;
        say(format!("world {name} copied from {}", save.display()));
        Ok(())
    }

    /// From nothing to a running server: setup (game tables, a world, settings), the service,
    /// a check that players get in, and how to join.
    pub fn up(&self, args: &[String]) -> Result {
        let mut args = args.to_vec();
        if !args.iter().any(|a| a == "--world" || a == "--new") && !self.world_file().is_file() {
            match self.pick_save() {
                Some(save) => args.extend(["--world".to_string(), save.display().to_string()]),
                None => args.push("--new".into()),
            }
        }
        self.setup(&args)?;
        if !self.start_service()? {
            say("no systemd user session here: start the server with 'pcds run'");
            return Ok(());
        }
        for _ in 0..60 {
            if self.api("GET", "/api/status", None).is_ok() {
                break;
            }
            sleep(1);
        }
        self.selftest()?;
        self.summary();
        Ok(())
    }

    /// On a terminal with saves of the game around: which one becomes the world (or a new one).
    fn pick_save(&self) -> Option<PathBuf> {
        if !super::is_tty() {
            return None;
        }
        let saves = self.local_saves();
        if saves.is_empty() {
            return None;
        }
        eprintln!("Your saves of The Planet Crafter on this machine:");
        for (i, save) in saves.iter().enumerate() {
            eprintln!("  {}) {}  ({})", i + 1, save.file_stem().unwrap_or_default().to_string_lossy(), super::local_time(super::modified(save)));
        }
        eprint!("Play one of them on the server (1-{}), or Enter for a new world: ", saves.len());
        let _ = std::io::stderr().flush();
        let mut choice = String::new();
        std::io::stdin().lock().read_line(&mut choice).ok()?;
        let n: usize = choice.trim().parse().ok().filter(|n| (1..=saves.len()).contains(n))?;
        Some(saves[n - 1].clone())
    }

    /// How players get in, once the server answers.
    fn summary(&self) {
        let steam_off = self.env_get("PCDS_STEAM").as_deref() == Some("off");
        let mut code = None;
        for _ in 0..30 {
            code = self.invite_code();
            if code.is_some() || steam_off {
                break;
            }
            sleep(2);
        }
        // The address this machine reaches the internet from (a UDP connect sends nothing).
        let ip = std::net::UdpSocket::bind("0.0.0.0:0").and_then(|s| s.connect("1.1.1.1:80").and_then(|_| s.local_addr())).map(|a| a.ip().to_string());
        let code = match code {
            Some(c) => format!("{c}   (it changes at every start unless PCDS_STEAM_TOKEN is set)"),
            None => "none yet (see pcds logs; pcds code prints it)".into(),
        };
        println!();
        println!("  The server is running the world \"{}\".", self.world_name());
        println!("    Invite code: {code}");
        println!("    Direct IP:   {}:{} (UDP; forward it on your router for players outside your network)", ip.unwrap_or_else(|_| "this machine".into()), self.port());
        println!("    Admin page:  {}", self.gui_url());
        println!("    Manage it:   {} status | players | worlds | rules | logs", self.exe.display());
        println!();
    }

    /// A scripted player joins over direct IP and gets the world, without becoming a player
    /// (nothing is saved for it).
    pub fn selftest(&self) -> Result {
        if !self.active() {
            return Err("start the server first (pcds start)".into());
        }
        let data = self.data_dir()?;
        let listen = self.env_get("PCDS_LISTEN").unwrap_or_default();
        if listen == "off" {
            return Err("direct IP is off (PCDS_LISTEN=off): nothing to test".into());
        }
        let host = listen.rsplit_once(':').map_or(listen.as_str(), |(h, _)| h);
        let host = if ["0.0.0.0", "[::]", ""].contains(&host) { "127.0.0.1" } else { host };
        let address = format!("{host}:{}", self.port());
        let (script, log) = (self.run.join(".selftest.json"), self.run.join(".selftest.log"));
        super::write(&script, r#"{"name":"PcdsSelftest","ready":false,"steps":[{"wait":2000}]}"#)?;
        let out = fs::File::create(&log).map_err(|e| format!("{}: {e}", log.display()))?;
        let err = out.try_clone().map_err(|e| e.to_string())?;
        let probe = Command::new(&self.exe).arg("probe").arg(&address).arg(&data).arg(&script).stdin(Stdio::null()).stdout(out).stderr(err).spawn();
        match probe {
            Ok(mut probe) => {
                wait_for(&mut probe, Duration::from_secs(60));
            }
            Err(e) => say(format!("the scripted player did not start: {}: {e}", self.exe.display())),
        }
        let said = fs::read_to_string(&log).unwrap_or_default();
        let _ = fs::remove_file(&script);
        let _ = fs::remove_file(&log);
        if said.contains("\"approved\"") && said.contains("\"joined\"") {
            say(format!("selftest PASS: a connection over direct IP on {address} was approved and got the world"));
            return Ok(());
        }
        let lines: Vec<&str> = said.lines().collect();
        eprintln!("{}", lines[lines.len().saturating_sub(5)..].join("\n"));
        Err("selftest FAIL (see above and pcds logs)".into())
    }

    /// The Docker image's start: the game tables (from the game mounted at /game, or downloaded
    /// with STEAM_USER), a world, steamclient for invite codes, then the server as this process.
    pub fn container(&self) -> Result {
        super::mkdir(&self.worlds)?;
        self.write_env()?;
        self.install_server()?;
        let downloaded = self.run.join("game");
        let steam_user = crate::env("STEAM_USER");
        if let Some(user) = &steam_user
            && !downloaded.join("Planet Crafter.exe").is_file()
        {
            if !super::is_tty() {
                return Err("STEAM_USER: the first download needs a terminal for the Steam login (docker run -it ...)".into());
            }
            self.download_game(user)?;
        }
        self.update_downloaded_game();
        // The game steamcmd downloaded, else the one mounted at /game.
        let game = if steam_user.is_some() || self.run.join("steam-user").is_file() { downloaded } else { PathBuf::from("/game") };
        let game = self.game_dir(Some(&game)).map_err(|e| {
            format!("{e}: mount your game folder at /game (-v '/path/to/The Planet Crafter:/game:ro'), or start once with 'docker run -it -e STEAM_USER=yourname ...' to download the game")
        })?;
        // The first start, or a game update: this game build's tables.
        if self.tables(&build_id(&game)?).is_err() {
            self.extract(Some(&game))?;
        }
        if !self.world_file().is_file() {
            let newest = fs::read_dir(&self.worlds).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "json")).max_by_key(|p| super::modified(p));
            match newest {
                Some(newest) if crate::env("PCDS_WORLD").is_none() => {
                    self.set_env("PCDS_WORLD", &newest.file_stem().unwrap_or_default().to_string_lossy())?;
                    say(format!("running the world {} (the newest in {})", self.world_name(), self.worlds.display()));
                }
                _ => {
                    say(format!("no world yet: making a new one, {}", self.world_name()));
                    let options: Vec<String> = crate::env("PCDS_NEW_WORLD").unwrap_or_default().split_whitespace().map(str::to_string).collect();
                    self.new_world(&self.world_file(), &options)?;
                }
            }
        }
        self.ensure_steamclient();
        say("manage it with: docker exec CONTAINER pcds worlds | players | rules | gui");
        self.run_server()
    }
}

/// Whether two programs are the same build: the same size and modification time (a bank's
/// copy keeps its source's time).
fn program_equal(a: &Path, b: &Path) -> bool {
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.len() == b.len() && a.modified().ok() == b.modified().ok(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_current_world_is_not_copied_over_itself() {
        let dir = super::super::test_dir("replace-world");
        let ctx = super::super::test_ctx(&dir);
        let current = ctx.world_file();
        fs::create_dir_all(current.parent().unwrap()).unwrap();
        fs::write(&current, "{\"saveDisplayName\":\"live\"}").unwrap();
        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&current, &link).unwrap();
        for same in [&current, &link] {
            assert!(ctx.replace_world(same).unwrap_err().contains("already"), "{}", same.display());
        }
        assert_eq!(fs::read_to_string(&current).unwrap(), "{\"saveDisplayName\":\"live\"}");
        assert!(!dir.join("run/worlds/backups").exists());

        let other = dir.join("other.json");
        fs::write(&other, "{\"saveDisplayName\":\"other\"}").unwrap();
        ctx.replace_world(&other).unwrap();
        assert_eq!(fs::read_to_string(&current).unwrap(), "{\"saveDisplayName\":\"other\"}");
        let backups: Vec<_> = fs::read_dir(dir.join("run/worlds/backups")).unwrap().flatten().map(|e| fs::read_to_string(e.path()).unwrap()).collect();
        assert_eq!(backups, ["{\"saveDisplayName\":\"live\"}"]);
        assert!(!dir.join("run/worlds/world.new").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// This program goes into a bank once: the first install into a, a newer build into the
    /// standby bank (then the active one), the same build never again (a rollback to the
    /// other bank stays).
    #[test]
    fn builds_go_into_the_standby_bank_once() {
        let dir = super::super::test_dir("install-bank");
        let ctx = super::super::test_ctx(&dir);
        fs::create_dir_all(dir.join("build/web")).unwrap();
        for (f, text) in [("pcds", "build 1"), ("libsteam_api.so", "lib"), ("web/index.html", "page")] {
            fs::write(dir.join("build").join(f), text).unwrap();
        }
        let banks = ctx.banks();
        let program = |b: &str| fs::read_to_string(banks.join(b).join("pcds")).ok();
        ctx.install_server().unwrap();
        assert_eq!((updater::active(&banks), program("a"), program("b")), ("a".into(), Some("build 1".into()), None));
        assert_eq!(updater::info(&banks, "a").unwrap()["protocol"], crate::ipc::PROTOCOL);
        assert!(banks.join("a/libsteam_api.so").is_file() && banks.join("a/web/index.html").is_file() && ctx.installed().is_file());
        ctx.install_server().unwrap();
        assert!(!banks.join("b").exists(), "the same build again: nothing");
        // A later build.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(dir.join("build/pcds"), "build 2").unwrap();
        ctx.install_server().unwrap();
        assert_eq!((updater::active(&banks), program("a"), program("b")), ("b".into(), Some("build 1".into()), Some("build 2".into())));
        // Rolled back to a: the build in b is not installed over it again.
        updater::set_active(&banks, "a").unwrap();
        ctx.install_server().unwrap();
        assert_eq!((updater::active(&banks), program("a")), ("a".into(), Some("build 1".into())));
        fs::remove_dir_all(&dir).unwrap();
    }
}
