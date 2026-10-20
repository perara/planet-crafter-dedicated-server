//! From nothing to a running server: setup, up, the Docker container's start, the selftest,
//! and `pcds run`, the server as the service runs it.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, Write};
use std::os::unix::process::CommandExt;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use super::env::{build_id, parse_env};
use super::{need, say, sleep, wait_for, Ctx, Result};
#[cfg(test)]
use crate::msg::BankInfo;
use crate::updater;

impl Ctx {
    /// The server runs from run/bin, so rebuilding or upgrading never touches a running server:
    /// run/bin/pcds (what the units run) with what it loads beside it, as in a release, and the
    /// banks run/bin/a and run/bin/b, each a whole install, with run/bin/active naming the one
    /// the front starts (docs/hot-update.md). The launcher is replaced by a later build of this
    /// program (its copy keeps the build's time). Whether this program also goes into a bank
    /// is bank_for()'s rule; the running server moves to it at its next start, or at once with
    /// `pcds upgrade --restart`.
    pub fn install_server(&self) -> Result {
        let running = self.active()?;
        let live = if running { self.live_bank() } else { None };
        if running && live.is_none() {
            say(format!("pcds {} is not put into a bank now: the server runs and its admin API does not say which bank runs the world (stop it, then 'pcds start')", env!("CARGO_PKG_VERSION")));
            return Ok(());
        }
        self.install_beside(live.as_deref())
    }

    /// install_server() beside the bank `live` runs the world in (None: no server runs, or
    /// `pcds run`, which starts one); never into that bank.
    fn install_beside(&self, live: Option<&str>) -> Result {
        let lib = self.exe.with_file_name("libsteam_api.so");
        if !lib.is_file() {
            return Err(format!("no libsteam_api.so next to {} (make build puts it there)", self.exe.display()));
        }
        let bin = self.run.join("bin");
        let launcher = self.installed();
        if !super::same_file(&self.exe, &launcher) && (!launcher.is_file() || super::modified(&self.exe) > super::modified(&launcher)) {
            super::mkdir(&bin.join("web"))?;
            super::install_file(&self.exe, &launcher)?;
            keep_time(&self.exe, &launcher)?;
            super::install_file(&lib, &bin.join("libsteam_api.so"))?;
            super::install_file(&self.web, &bin.join("web/index.html"))?;
        }
        let banks = self.banks();
        let target = match self.bank_for(&banks, live)? {
            Ok(target) => target,
            Err(Some(why)) => {
                say(why);
                return Ok(());
            }
            Err(None) => return Ok(()),
        };
        self.install_bank(&target, &lib)?;
        // A later version than the one rolled back from: the rollback note is done with.
        if updater::rolled_back(&banks).is_some_and(|r| r.bank == target) {
            updater::clear_rolled_back(&banks)?;
        }
        updater::set_active(&banks, &target)?;
        say(format!("pcds {} installed in bank {target} ({}): the server runs it from its next start", env!("CARGO_PKG_VERSION"), banks.join(&target).display()));
        Ok(())
    }

    /// The bank this program goes into (`pcds run`, start, restart, setup and the container's
    /// start all ask), or Err with why not (None: nothing to say). The rule:
    /// - a bank that holds this very program (the same bytes) is enough: nothing;
    /// - both banks are empty: the active one (the first install);
    /// - else the standby bank (the one beside the bank that runs the world, `live`, else
    ///   beside the active one; the active one while it is empty), and only when this program
    ///   is newer than what each bank holds: a later version, or the same version built later
    ///   (the program file's time; a bank's copy keeps the time of what it was copied from).
    ///   So an older launcher never replaces or downgrades a release `pcds upgrade` installed
    ///   (it says so: a downgrade is `pcds upgrade VERSION --restart`), and a bank that holds a
    ///   rollback target keeps it;
    /// - not the bank the owner rolled back from (updater::rolled_back) with that same release
    ///   or a build of it: only a later version than the one rolled back from goes there (and
    ///   lifts the note), as an explicit `pcds upgrade` or the next rollback do;
    /// - never the bank that runs the world.
    fn bank_for(&self, banks: &Path, live: Option<&str>) -> Result<std::result::Result<String, Option<String>>> {
        let active = match live {
            Some(live) => live.to_string(),
            None => updater::active(banks)?,
        };
        let program = |b: &str| banks.join(b).join("pcds");
        for b in updater::BANKS {
            if program(b).is_file() && program_equal(&self.exe, &program(b))? {
                return Ok(Err(None));
            }
        }
        if updater::BANKS.iter().all(|b| updater::info(banks, b).is_none()) {
            return Ok(Ok(active));
        }
        let version = env!("CARGO_PKG_VERSION");
        // What a bank holds: its release's tag when it came from one (a release whose program
        // forgot to bump its version is still that release), else its program's version.
        let held_of = |b: &str| updater::info(banks, b).map(|i| updater::held_release(banks, b, i.version.as_deref().unwrap_or("")));
        let newer_than = |b: &str| match held_of(b) {
            None => true,
            Some(held) => updater::newer(version, &held) || (!updater::newer(&held, version) && super::modified(&self.exe) > super::modified(&program(b))),
        };
        if !updater::BANKS.iter().all(|b| newer_than(b)) {
            let newer = updater::BANKS.iter().find_map(|b| held_of(b).filter(|held| updater::newer(held, version)).map(|held| (*b, held.trim_start_matches('v').to_string())));
            return Ok(Err(newer.map(|(b, held)| {
                format!("pcds {version} is not installed: bank {b} holds pcds {held}, which is newer, and the server goes on with what the banks hold. To go back to {version}: pcds upgrade v{version} --restart")
            })));
        }
        // The standby bank, or the active one while it is empty (never the live one, which
        // holds the world and so is not empty).
        let target = if updater::info(banks, &active).is_none() && live.is_none() { active } else { updater::other(&active).to_string() };
        let rolled_from = updater::rolled_back(banks).filter(|r| r.bank == target).and_then(|_| held_of(&target));
        if let Some(held) = rolled_from.filter(|held| !updater::newer(version, held)) {
            return Ok(Err(Some(format!("pcds {version} is not installed: bank {target} holds pcds {}, the release the server was rolled back from, and is kept ('pcds upgrade' installs a release there)", held.trim_start_matches('v')))));
        }
        Ok(Ok(target))
    }

    /// This program as a bank: its files beside the bank, then moved over it (the files keep
    /// their times: a later build is newer than the bank).
    fn install_bank(&self, bank: &str, lib: &Path) -> Result {
        let banks = self.banks();
        let new = updater::staging(&banks, bank);
        let _ = fs::remove_dir_all(&new);
        super::mkdir(&new.join("web"))?;
        let copy = |from: &Path, to: &Path| -> Result {
            fs::copy(from, to).map_err(|e| format!("{} -> {}: {e}", from.display(), to.display()))?;
            keep_time(from, to)
        };
        let result = (|| {
            copy(&self.exe, &new.join("pcds"))?;
            copy(lib, &new.join("libsteam_api.so"))?;
            copy(&self.web, &new.join("web/index.html"))?;
            let mut info = crate::bank::info();
            info.installed = Some(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0));
            info.extra.insert("from".into(), self.exe.display().to_string().into());
            updater::write_info(&new, &info)?;
            updater::put(&banks, bank, &new)
        })();
        let _ = fs::remove_dir_all(&new);
        result
    }

    /// The server in the foreground, as the unit runs it: the front from the active bank, on
    /// the installed game's tables, the world, the settings; this process exits with the
    /// front's code once the front and its banks have ended (supervise). Returns only when it
    /// cannot start.
    pub fn run_server(&self) -> Result {
        self.start_front(true)
    }

    /// run_server(); `install`: whether this program goes into a bank first (the container's
    /// start did that already, and said what it found once).
    fn start_front(&self, install: bool) -> Result {
        // By hand while the service runs it (under the unit, this process is its main one).
        if !self.container && self.active()? {
            let pid = self.server_pid().unwrap_or_default();
            if pid != std::process::id().to_string() {
                return Err(format!("the service {} runs this installation's server (pid {pid}): 'pcds stop' first, or 'pcds logs' to watch it", self.unit()));
            }
        }
        // Started by hand: the settings file (the unit has systemd read it).
        let mut settings = HashMap::new();
        if crate::env("PCDS_ADMIN_TOKEN").is_none() && self.env_file.is_file() {
            settings.extend(parse_env(&fs::read_to_string(&self.env_file).map_err(|e| format!("{}: {e}", self.env_file.display()))?));
        }
        let get = |key: &str| settings.get(key).filter(|v| !v.is_empty()).cloned().or_else(|| crate::env(key));
        self.check_settings_file()?;
        crate::config::check_env(get)?;
        let world = self.world_file();
        if !world.is_file() {
            return Err(format!("no world at {}: 'pcds worlds', 'pcds world use NAME' or 'pcds world new NAME'", world.display()));
        }
        // A unit this release would write differently (another program path, a changed stop
        // timeout) is written again; systemd takes the new text at the next start.
        if !self.container
            && self.unit_dir.join(self.unit()).is_file()
            && let Err(e) = self.refresh_unit()
        {
            eprintln!("pcds: the systemd units could not be written again ({e}); 'pcds install-service' writes them");
        }
        // A newer build into a bank is not what the start needs: when it fails (a full disk),
        // the server starts from the banks as they are.
        if install
            && let Err(e) = self.install_beside(None)
        {
            eprintln!("pcds: {} was not put into a bank ({e}); starting the server from the banks as they are", self.exe.display());
        }
        // The tables of the bank that runs (installing may have changed which one).
        let game = self.game_dir(None)?;
        let data = self.tables(&build_id(&game)?)?;
        let banks = self.banks();
        let front = |bank: &str| {
            let mut server = Command::new(banks.join(bank).join("pcds"));
            server.arg("serve").arg(&data).arg(&world).envs(&settings);
            // The installation's folder: the front finds server.env (where it remembers which
            // world it runs: the dashboard can run another), the rules and access lists (the
            // installation's, whichever world runs) and the banks (bin/) in it.
            server.env("PCDS_HOME", &self.run);
            // The game the tables are for: /api/status says when it has changed (bank/status.rs).
            server.env("PCDS_GAME_DIR", &game);
            server
        };
        let first = updater::active(&banks)?;
        let mut code = supervise(front(&first), self.container).map_err(|e| format!("{}: {e}", banks.join(&first).display()))?;
        // The release of the active bank cannot run the world (its front tried both banks, and
        // the other is of another front protocol, or it fell over): the other bank's own front,
        // which ran before, unless the owner rolled back from it. Said loudly and noted
        // (pcds status, /api/status); a second failure puts everything back as it was.
        if code == crate::front::BOOT_FAILED && !STOP_ASKED.load(Ordering::SeqCst) {
            match fallback_bank(&banks, &first) {
                Ok(other) => {
                    let reason = format!("could not start the world: its front (pcds {}) ended without running it (exit {code}; see pcds logs)", updater::info(&banks, &first).map_or("?".into(), |i| i.version().to_string()));
                    eprintln!("pcds: bank {first} {reason}. Starting bank {other}, which ran before");
                    updater::note_fallback(&banks, &first, &other, &reason)?;
                    updater::set_active(&banks, &other)?;
                    code = supervise(front(&other), self.container).map_err(|e| format!("{}: {e}", banks.join(&other).display()))?;
                    if code == crate::front::BOOT_FAILED && !STOP_ASKED.load(Ordering::SeqCst) {
                        eprintln!("pcds: bank {other} could not start the world either; bank {first} is the active one again");
                        updater::set_active(&banks, &first)?;
                        updater::clear_fallback(&banks)?;
                    }
                }
                Err(why) => eprintln!("pcds: bank {first} could not start the world, and bank {}: {why}", updater::other(&first)),
            }
        }
        // In the container: `pcds upgrade --restart` asked for a restart in place (restart_in_place).
        // The same program starts again as this very process (same pid, same container), and so
        // starts the front of the bank that is the active one now.
        if self.container && RESTART_ASKED.swap(false, Ordering::SeqCst) {
            say("restarting the server in place");
            let e = Command::new(&self.exe).args(std::env::args_os().skip(1)).exec();
            return Err(format!("{}: {e}", self.exe.display()));
        }
        std::process::exit(code)
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
        // The save first: nothing is downloaded or installed for one this server cannot read.
        if let Some(world) = world.as_deref().filter(|w| w.is_file()) {
            super::check_world(world)?;
        }
        for tool in ["curl", "tar"] {
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
        self.ensure_tables(game.as_deref())?;
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
    pub(super) fn replace_world(&self, save: &Path) -> Result {
        let (current, name) = (self.world_file(), self.world_name());
        if !save.is_file() {
            return Err(format!("no save at {}", save.display()));
        }
        super::check_world(save)?;
        if super::same_file(save, &current) {
            return Err(format!("{} is the world {name} already: leave out --world", save.display()));
        }
        if self.active()? {
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
        if !self.active()? {
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
        let probe = Command::new(self.program()?).arg("probe").arg(&address).arg(&data).arg(&script).stdin(Stdio::null()).stdout(out).stderr(err).spawn();
        match probe {
            Ok(mut probe) => {
                wait_for(&mut probe, Duration::from_secs(60));
            }
            Err(e) => say(format!("the scripted player did not start: {e}")),
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
    /// with STEAM_USER), a world (`new_world`: the options of the new one made when there is
    /// none), steamclient for invite codes, then the server as this process.
    pub fn container(&self, new_world: &[String]) -> Result {
        super::mkdir(&self.worlds)?;
        self.write_env()?;
        self.install_server()?;
        if let Some(note) = self.image_note(None) {
            say(note);
        }
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
        // A game update that fails is said, and the server starts on the game as it is (a
        // Steam outage must not keep the container down; pcds update tries again).
        if let Err(e) = self.update_downloaded_game() {
            eprintln!("pcds: {e}; starting on the game as it is");
        }
        // The game steamcmd downloaded, else the one mounted at /game.
        let game = if steam_user.is_some() || self.run.join("steam-user").is_file() { downloaded } else { PathBuf::from("/game") };
        let game = self.game_dir(Some(&game)).map_err(|e| {
            format!("{e}: mount your game folder at /game (-v '/path/to/The Planet Crafter:/game:ro'), or start once with 'docker run -it -e STEAM_USER=yourname ...' to download the game")
        })?;
        // The first start, a game update or a release that reads another tables format.
        self.ensure_tables(Some(&game))?;
        if !self.world_file().is_file() {
            let newest = fs::read_dir(&self.worlds).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "json")).max_by_key(|p| super::modified(p));
            match newest {
                Some(newest) if crate::env("PCDS_WORLD").is_none() => {
                    self.set_env("PCDS_WORLD", &newest.file_stem().unwrap_or_default().to_string_lossy())?;
                    say(format!("running the world {} (the newest in {})", self.world_name(), self.worlds.display()));
                }
                _ => {
                    say(format!("no world yet: making a new one, {}", self.world_name()));
                    self.new_world(&self.world_file(), new_world)?;
                }
            }
        }
        self.ensure_steamclient();
        say("manage it with: docker exec CONTAINER pcds worlds | players | rules | gui");
        self.start_front(false)
    }
}

/// How long the banks of a front that ended get to save and end before they are killed. A
/// stop gives the front 60 s (front/mod.rs STOP_GRACE twice), then this: 80 s, under the unit's
/// TimeoutStopSec=90.
const BANKS_GRACE: Duration = Duration::from_secs(20);

/// A stop (SIGTERM, SIGINT, SIGHUP) came to supervise(): a front that then ends is no failure
/// to fall back from.
static STOP_ASKED: AtomicBool = AtomicBool::new(false);

/// A restart in place came to supervise() (SIGUSR1, container only): a stop, after which
/// run_server() starts the server again.
static RESTART_ASKED: AtomicBool = AtomicBool::new(false);

/// The bank `pcds run` falls back to when the active one's front cannot start the world: the
/// other one, when it holds a release and is not the one the owner rolled back from.
fn fallback_bank(banks: &Path, failed: &str) -> std::result::Result<String, String> {
    let other = updater::other(failed);
    if updater::info(banks, other).is_none() {
        return Err("holds no release to fall back to".into());
    }
    if updater::rolled_back(banks).is_some_and(|r| r.bank == other) {
        return Err("is the one the server was rolled back from: it is not started (pcds upgrade --rollback goes back to it)".into());
    }
    Ok(other.to_string())
}

/// The front, run as a child of this process, which ends only after every process the front
/// left behind. A front that crashes leaves its banks running; they save the world when they
/// find it gone (bank/serve.rs) and end. Under systemd (KillMode=mixed) and in Docker (tini, then
/// the container's PID namespace) the end of the main process kills what is left of the unit
/// or container at once, before any bank could save: this process is that main process, a
/// subreaper the orphaned banks are handed to, and it waits for them (BANKS_GRACE at most,
/// then kills the front's process group). SIGTERM, SIGINT and SIGHUP go on to the front, which
/// stops its banks with a save itself. The front's exit code, or 128 + its signal.
fn supervise(mut front: Command, restartable: bool) -> Result<i32> {
    // A signal that comes before the front's pid is known waits in PENDING: whichever of the
    // handler and this thread sees both sends it (one swap takes it).
    static FRONT: AtomicI32 = AtomicI32::new(0);
    static PENDING: AtomicI32 = AtomicI32::new(0);
    fn pass(pid: i32) {
        let signal = PENDING.swap(0, Ordering::SeqCst);
        if pid > 0 && signal != 0 {
            // SAFETY: kill(2) is async-signal-safe.
            unsafe { libc::kill(pid, signal) };
        }
    }
    extern "C" fn pass_on(signal: libc::c_int) {
        STOP_ASKED.store(true, Ordering::SeqCst);
        // SIGUSR1 (a restart in place) stops the front like SIGTERM does.
        if signal == libc::SIGUSR1 {
            RESTART_ASKED.store(true, Ordering::SeqCst);
        }
        PENDING.store(if signal == libc::SIGUSR1 { libc::SIGTERM } else { signal }, Ordering::SeqCst);
        pass(FRONT.load(Ordering::SeqCst));
    }
    // SAFETY: prctl and signal dispositions of this process; the handler only touches atomics
    // and calls kill(2).
    unsafe {
        if libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) != 0 {
            return Err(format!("cannot wait for the banks (PR_SET_CHILD_SUBREAPER): {}", std::io::Error::last_os_error()));
        }
        for s in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP].into_iter().chain(restartable.then_some(libc::SIGUSR1)) {
            libc::signal(s, pass_on as *const () as libc::sighandler_t);
        }
    }
    // Its own process group: what is left of it can be killed as one, and a terminal's Ctrl-C
    // reaches it once, through this process.
    let child = front.process_group(0).spawn().map_err(|e| e.to_string())?;
    FRONT.store(child.id() as i32, Ordering::SeqCst);
    pass(child.id() as i32);
    let pid = FRONT.load(Ordering::SeqCst);
    let (mut code, mut deadline) = (None, None);
    loop {
        let mut status = 0;
        let flags = if code.is_some() { libc::WNOHANG } else { 0 };
        // SAFETY: waits for any child of this process (the front, then the orphaned banks).
        let ended = unsafe { libc::waitpid(-1, &mut status, flags) };
        if ended == pid {
            let c = if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { 128 + libc::WTERMSIG(status) };
            code = Some(c);
            deadline = Some(std::time::Instant::now() + BANKS_GRACE);
            FRONT.store(0, Ordering::SeqCst);
        } else if ended > 0 {
            continue;
        } else if ended == 0 {
            if deadline.is_some_and(|d| std::time::Instant::now() > d) {
                eprintln!("pcds: the banks of the front did not end within {} s of it: killed", BANKS_GRACE.as_secs());
                // SAFETY: the front's process group, which only it and its banks are in.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        } else {
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) => {}
                // No child left: the front and every bank have ended.
                _ => break,
            }
        }
    }
    code.ok_or_else(|| "the front was never waited for".to_string())
}

/// Whether two programs are the same build: the same bytes (sizes first, then sha256).
fn program_equal(a: &Path, b: &Path) -> Result<bool> {
    let size = |p: &Path| fs::metadata(p).map(|m| m.len()).map_err(|e| format!("{}: {e}", p.display()));
    if size(a)? != size(b)? {
        return Ok(false);
    }
    let sum = |p: &Path| fs::read(p).map(|bytes| ring::digest::digest(&ring::digest::SHA256, &bytes)).map_err(|e| format!("{}: {e}", p.display()));
    Ok(sum(a)?.as_ref() == sum(b)?.as_ref())
}

/// A copy's time made its source's (a later build is newer than what a bank holds).
fn keep_time(from: &Path, to: &Path) -> Result {
    let time = fs::metadata(from).and_then(|m| m.modified()).map_err(|e| format!("{}: {e}", from.display()))?;
    fs::File::options().write(true).open(to).and_then(|f| f.set_modified(time)).map_err(|e| format!("{}: {e}", to.display()))
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
        fs::write(&current, crate::save::SaveFile::tiny_text("live")).unwrap();
        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&current, &link).unwrap();
        for same in [&current, &link] {
            assert!(ctx.replace_world(same).unwrap_err().contains("already"), "{}", same.display());
        }
        assert_eq!(fs::read_to_string(&current).unwrap(), crate::save::SaveFile::tiny_text("live"));
        assert!(!dir.join("run/worlds/backups").exists());

        let other = dir.join("other.json");
        fs::write(&other, crate::save::SaveFile::tiny_text("other")).unwrap();
        ctx.replace_world(&other).unwrap();
        assert_eq!(fs::read_to_string(&current).unwrap(), crate::save::SaveFile::tiny_text("other"));
        let backups: Vec<_> = fs::read_dir(dir.join("run/worlds/backups")).unwrap().flatten().map(|e| fs::read_to_string(e.path()).unwrap()).collect();
        assert_eq!(backups, [crate::save::SaveFile::tiny_text("live")]);
        assert!(!dir.join("run/worlds/world.new").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// supervise() in a process of its own: a subreaper is handed every orphan of its process,
    /// which the test harness must not be. PCDS_SUPERVISE_TEST names the folder; "crash" or
    /// "stop" what the stand-in front does.
    #[test]
    #[ignore]
    fn supervise_in_a_process_of_its_own() {
        let (Some(dir), Ok(what)) = (std::env::var_os("PCDS_SUPERVISE_TEST"), std::env::var("PCDS_SUPERVISE_WHAT")) else { return };
        let script = match what.as_str() {
            // It starts a bank that saves a moment after the front is gone, and crashes.
            "crash" => "(sleep 0.5; echo saved > \"$0/saved\") & exit 3",
            // The stop goes to the front; it has its bank save, and ends.
            _ => "trap '(sleep 0.3; echo saved > \"$0/saved\") & exit 0' TERM; echo up > \"$0/up\"; while :; do sleep 0.05; done",
        };
        let mut front = Command::new("sh");
        front.arg("-c").arg(script).arg(&dir);
        std::process::exit(supervise(front, what == "restart").map(|code| if RESTART_ASKED.load(Ordering::SeqCst) { 77 } else { code }).unwrap());
    }

    #[test]
    fn the_banks_of_a_front_that_ended_get_to_save_first() {
        for what in ["crash", "stop", "restart"] {
            let dir = super::super::test_dir(&format!("supervise-{what}"));
            let mut helper = Command::new(std::env::current_exe().unwrap())
                .args(["cli::setup::tests::supervise_in_a_process_of_its_own", "--ignored", "--exact", "--nocapture"])
                .env("PCDS_SUPERVISE_TEST", &dir)
                .env("PCDS_SUPERVISE_WHAT", what)
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            if what != "crash" {
                let deadline = std::time::Instant::now() + Duration::from_secs(20);
                while !dir.join("up").is_file() {
                    assert!(std::time::Instant::now() < deadline, "the front did not start");
                    std::thread::sleep(Duration::from_millis(20));
                }
                // SAFETY: the helper process this test started (systemctl stop's SIGTERM, or
                // pcds upgrade --restart's SIGUSR1 in the container).
                assert_eq!(unsafe { libc::kill(helper.id() as i32, if what == "restart" { libc::SIGUSR1 } else { libc::SIGTERM }) }, 0);
            }
            let status = super::super::wait_for(&mut helper, Duration::from_secs(30)).expect("the helper ends");
            // A restart in place: the helper says so with 77 (run_server starts the server again).
            assert_eq!(status.code(), Some(match what { "crash" => 3, "restart" => 77, _ => 0 }), "{what}: the front's exit code");
            assert_eq!(fs::read_to_string(dir.join("saved")).ok().as_deref(), Some("saved\n"), "{what}: the bank saved before the end");
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// A stop: the front's grace for its bank, then the supervisor's for orphans, all before
    /// systemd's kill.
    #[test]
    fn systemd_kills_last_at_a_stop() {
        let dir = super::super::test_dir("stop-timeouts");
        let units = super::super::test_ctx(&dir).unit_files().unwrap();
        let timeout: u64 = units[0].1.lines().find_map(|l| l.strip_prefix("TimeoutStopSec=")).unwrap().parse().unwrap();
        assert!(crate::front::STOP_GRACE * 2 + BANKS_GRACE + Duration::from_secs(5) <= Duration::from_secs(timeout));
        let compose = include_str!("../../../compose.yaml");
        assert!(compose.contains(&format!("stop_grace_period: {timeout}s")), "compose.yaml stops as late as the unit");
        assert!(include_str!("../../../README.md").contains(&format!("--stop-timeout {timeout}")), "so does the README's docker run");
        let docs = include_str!("../../../docs/docker.md");
        assert!(docs.contains(&format!("--stop-timeout {timeout}")) && docs.contains(&format!("stop_grace_period: {timeout}s")), "and docs/docker.md");
        // Kubernetes: the grace period of the pod covers the unit's stop, with room.
        let grace: u64 = include_str!("../../../docs/kubernetes.yaml").lines().find_map(|l| l.trim().strip_prefix("terminationGracePeriodSeconds:")?.trim().parse().ok()).expect("a terminationGracePeriodSeconds in docs/kubernetes.yaml");
        assert!(grace >= timeout, "the pod's grace period {grace} s is shorter than the unit's {timeout} s");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The other bank, unless it is empty or the one rolled back from.
    #[test]
    fn a_start_falls_back_to_the_bank_that_ran_before() {
        let dir = super::super::test_dir("fallback-bank");
        let banks = dir.join("bin");
        assert!(fallback_bank(&banks, "a").unwrap_err().contains("no release"));
        fs::create_dir_all(banks.join("b")).unwrap();
        fs::write(banks.join("b/pcds"), "").unwrap();
        updater::write_info(&banks.join("b"), &BankInfo { version: Some("0.6.0".into()), ..Default::default() }).unwrap();
        assert_eq!(fallback_bank(&banks, "a").unwrap(), "b");
        updater::set_rolled_back(&banks, "b").unwrap();
        assert!(fallback_bank(&banks, "a").unwrap_err().contains("rolled back from"));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The rule of bank_for(): a build into a bank once (the first into the active bank, a
    /// later one into the standby bank, which becomes the active one); the launcher, which is
    /// that build, adds nothing; nothing over a newer release, nor over the bank rolled back
    /// from.
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
        let build = |text: &str| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            fs::write(dir.join("build/pcds"), text).unwrap();
        };
        ctx.install_beside(None).unwrap();
        assert_eq!((updater::active(&banks).unwrap(), program("a"), program("b")), ("a".into(), Some("build 1".into()), None));
        assert_eq!(updater::info(&banks, "a").unwrap().protocol, Some(crate::ipc::PROTOCOL));
        assert!(banks.join("a/libsteam_api.so").is_file() && banks.join("a/web/index.html").is_file() && ctx.installed().is_file());
        ctx.install_beside(None).unwrap();
        assert!(!banks.join("b").exists(), "the same build again: nothing");
        // pcds run is the launcher, run/bin/pcds: a copy of build 1, in bank a already.
        let launcher = Ctx { exe: ctx.installed(), ..ctx.clone() };
        launcher.install_beside(None).unwrap();
        assert!(!banks.join("b").exists(), "the launcher's own build: nothing");
        // A later build.
        build("build 2");
        ctx.install_beside(None).unwrap();
        assert_eq!((updater::active(&banks).unwrap(), program("a"), program("b")), ("b".into(), Some("build 1".into()), Some("build 2".into())));
        // Rolled back to a: neither the build in b nor the launcher go over a or b again.
        updater::set_active(&banks, "a").unwrap();
        updater::set_rolled_back(&banks, "b").unwrap();
        ctx.install_beside(None).unwrap();
        launcher.install_beside(None).unwrap();
        assert_eq!((updater::active(&banks).unwrap(), program("a"), program("b")), ("a".into(), Some("build 1".into()), Some("build 2".into())));
        // A later build still leaves alone the bank rolled back from.
        build("build 3");
        ctx.install_beside(None).unwrap();
        assert_eq!((updater::active(&banks).unwrap(), program("b")), ("a".into(), Some("build 2".into())));
        // A later version than the release rolled back from does go there, and ends the note
        // (a newer release folder's pcds start, as an upgrade from it).
        updater::write_info(&banks.join("b"), &BankInfo::new("0.0.1", crate::ipc::PROTOCOL, 0, 0)).unwrap();
        ctx.install_beside(None).unwrap();
        assert_eq!((updater::active(&banks).unwrap(), program("b")), ("b".into(), Some("build 3".into())));
        assert!(updater::rolled_back(&banks).is_none());
        updater::clear_rolled_back(&banks).unwrap();
        ctx.install_beside(None).unwrap();
        assert_eq!((updater::active(&banks).unwrap(), program("b")), ("b".into(), Some("build 3".into())));
        // A release of a later version in the standby bank (pcds upgrade): an older build never
        // goes over it, however new its file.
        updater::write_info(&banks.join("a"), &BankInfo { tag: Some("v99.0.0".into()), ..BankInfo::new("99.0.0", crate::ipc::PROTOCOL, 0, 0) }).unwrap();
        build("build 4");
        ctx.install_beside(None).unwrap();
        assert_eq!((updater::active(&banks).unwrap(), program("a"), program("b")), ("b".into(), Some("build 1".into()), Some("build 3".into())));
        // That downgrade is said, with how to make it.
        let why = ctx.bank_for(&banks, None).unwrap().unwrap_err().unwrap();
        assert!(why.contains("bank a holds pcds 99.0.0, which is newer") && why.contains(&format!("pcds upgrade v{} --restart", env!("CARGO_PKG_VERSION"))), "{why}");
        // A later build while bank a runs the world (the active file says b): into b, never a.
        updater::write_info(&banks.join("a"), &BankInfo::new("0.0.1", crate::ipc::PROTOCOL, 0, 0)).unwrap();
        build("build 5");
        assert_eq!(ctx.bank_for(&banks, Some("a")).unwrap(), Ok("b".to_string()));
        assert_eq!(ctx.bank_for(&banks, Some("b")).unwrap(), Ok("a".to_string()));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// bank_for() on the version sequences a server meets: the first install, an update, a rebuild
    /// of the same version, a pre-release, a downgrade, a rollback with its note, and a release
    /// whose program says another version than its tag.
    #[test]
    fn the_standby_bank_rule_on_real_version_sequences() {
        let dir = super::super::test_dir("bank-for");
        let ctx = super::super::test_ctx(&dir);
        fs::create_dir_all(dir.join("build/web")).unwrap();
        fs::write(dir.join("build/pcds"), "this build").unwrap();
        let banks = ctx.banks();
        let me = env!("CARGO_PKG_VERSION");
        // A bank holding `version` (and `tag`), its program written `later` than this build's or before it.
        let bank = |name: &str, held: Option<(&str, Option<&str>)>, later: bool| {
            let _ = fs::remove_dir_all(banks.join(name));
            let Some((version, tag)) = held else { return };
            fs::create_dir_all(banks.join(name)).unwrap();
            let program = banks.join(name).join("pcds");
            fs::write(&program, format!("{name} {version}")).unwrap();
            let base = fs::metadata(dir.join("build/pcds")).unwrap().modified().unwrap();
            let at = if later { base + std::time::Duration::from_secs(60) } else { base - std::time::Duration::from_secs(60) };
            fs::File::options().write(true).open(&program).unwrap().set_modified(at).unwrap();
            let info = BankInfo { version: Some(version.to_string()), tag: tag.map(str::to_string), ..Default::default() };
            updater::write_info(&banks.join(name), &info).unwrap();
        };
        let rule = |active: &str, live: Option<&str>| {
            updater::set_active(&banks, active).unwrap();
            ctx.bank_for(&banks, live).unwrap()
        };
        let skipped = |r: std::result::Result<String, Option<String>>| match r {
            Err(why) => why.unwrap_or_else(|| "(nothing said)".into()),
            Ok(bank) => panic!("went into bank {bank}"),
        };
        // The first install: the active bank, whichever it is.
        bank("a", None, false);
        bank("b", None, false);
        assert_eq!(rule("a", None), Ok("a".to_string()));
        // 0.6.1 runs from a: this build goes into b. And with 0.6.0 in b: over it.
        bank("a", Some(("0.5.0", None)), false);
        assert_eq!(rule("a", None), Ok("b".to_string()));
        bank("b", Some(("0.4.0", Some("v0.4.0"))), false);
        assert_eq!(rule("a", Some("a")), Ok("b".to_string()), "a running bank a: into b");
        assert_eq!(rule("b", Some("b")), Ok("a".to_string()), "a running bank b: into a, never the one that runs");
        // The same version, built before the banks' programs: nothing said (it is installed); built after: goes in.
        bank("a", Some((me, None)), true);
        bank("b", Some((me, None)), true);
        assert!(skipped(rule("a", None)).contains("nothing said"), "an older build of the same version");
        bank("b", Some((me, None)), false);
        bank("a", Some((me, None)), false);
        assert_eq!(rule("a", None), Ok("b".to_string()), "a later build of the same version");
        // A later version in a bank (a release put there by pcds upgrade): this build is a downgrade, said with how.
        bank("a", Some(("99.0.0", Some("v99.0.0"))), false);
        bank("b", Some(("0.5.0", None)), false);
        let why = skipped(rule("b", None));
        assert!(why.contains("bank a holds pcds 99.0.0") && why.contains("--restart"), "{why}");
        // A pre-release of a later version is later; a pre-release of this version is earlier.
        bank("a", Some(("99.0.0-rc.1", None)), false);
        assert!(skipped(rule("b", None)).contains("99.0.0-rc.1"));
        bank("a", Some((&format!("{me}-rc.1"), None)), false);
        bank("b", Some(("0.5.0", None)), false);
        assert_eq!(rule("a", None), Ok("b".to_string()), "this release is later than its own release candidate");
        // Rolled back from the release in b (0.6.2, this version): this build does not go there; a later one does.
        bank("a", Some(("0.5.0", None)), false);
        bank("b", Some((me, Some(&format!("v{me}")))), false);
        updater::set_rolled_back(&banks, "b").unwrap();
        let why = skipped(rule("a", None));
        assert!(why.contains("rolled back from"), "{why}");
        bank("b", Some(("0.5.5", Some("v0.5.5"))), false);
        updater::set_rolled_back(&banks, "b").unwrap();
        assert_eq!(rule("a", None), Ok("b".to_string()), "a later version than the one rolled back from");
        updater::clear_rolled_back(&banks).unwrap();
        // A release whose program says 0.5.0 under the tag v99.0.1: it is v99.0.1 that this build is compared with.
        bank("a", Some(("0.5.0", None)), false);
        bank("b", Some(("0.5.0", Some("v99.0.1"))), false);
        let why = skipped(rule("a", None));
        assert!(why.contains("bank b holds pcds 99.0.1"), "{why}");
        fs::remove_dir_all(&dir).unwrap();
    }
}
