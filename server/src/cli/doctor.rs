//! `pcds doctor`: everything an operator needs to know before or while a server runs, as a list
//! of PASS, WARN and FAIL lines, each with the line to type that fixes it. It reads files and asks
//! the machine (ports, disk, limits, the clock, systemd, Steam, the release feed); when the server
//! runs it also asks the admin API for what only the server knows. It changes nothing. The exit
//! code is 0 unless a check FAILs; `--json` prints the same list for scripts.

use std::fs;
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};

use super::{output, Ctx, Result};
use crate::util::modified;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Pass,
    Warn,
    Fail,
}

impl Level {
    fn word(self) -> &'static str {
        match self {
            Level::Pass => "PASS",
            Level::Warn => "WARN",
            Level::Fail => "FAIL",
        }
    }
}

#[derive(Debug)]
pub struct Check {
    pub id: &'static str,
    pub level: Level,
    pub detail: String,
    pub fix: Option<String>,
}

/// The checks, in the order they ran.
#[derive(Debug, Default)]
pub struct Report(pub Vec<Check>);

impl Report {
    fn add(&mut self, id: &'static str, level: Level, detail: impl Into<String>, fix: Option<String>) {
        self.0.push(Check { id, level, detail: detail.into(), fix });
    }

    fn pass(&mut self, id: &'static str, detail: impl Into<String>) {
        self.add(id, Level::Pass, detail, None);
    }

    fn warn(&mut self, id: &'static str, detail: impl Into<String>, fix: impl Into<String>) {
        self.add(id, Level::Warn, detail, Some(fix.into()));
    }

    fn fail(&mut self, id: &'static str, detail: impl Into<String>, fix: impl Into<String>) {
        self.add(id, Level::Fail, detail, Some(fix.into()));
    }

    pub fn count(&self, level: Level) -> usize {
        self.0.iter().filter(|c| c.level == level).count()
    }

    pub fn ok(&self) -> bool {
        self.count(Level::Fail) == 0
    }

    pub fn json(&self, running: bool) -> Value {
        json!({
            "ok": self.ok(),
            "serverRunning": running,
            "summary": {"pass": self.count(Level::Pass), "warn": self.count(Level::Warn), "fail": self.count(Level::Fail)},
            "checks": self.0.iter().map(|c| json!({"id": c.id, "level": c.level.word().to_lowercase(), "detail": c.detail, "fix": c.fix})).collect::<Vec<_>>(),
        })
    }

    pub fn text(&self) -> String {
        let mut out = String::new();
        for c in &self.0 {
            out += &format!("{}  {:<17} {}\n", c.level.word(), c.id, c.detail.replace('\n', "\n                          "));
            if let Some(fix) = &c.fix {
                out += &format!("      fix: {fix}\n");
            }
        }
        out += &format!("\n{} FAIL, {} WARN, {} PASS\n", self.count(Level::Fail), self.count(Level::Warn), self.count(Level::Pass));
        out
    }
}

/// What the running server says, when it runs.
struct Live {
    status: Option<Value>,
    plugins: Option<Value>,
    /// Why there is none, when the unit runs but the admin API does not answer.
    unreachable: Option<String>,
}

impl Live {
    fn running(&self) -> bool {
        self.status.is_some()
    }

    fn status(&self, path: &str) -> &Value {
        let mut v = self.status.as_ref().unwrap_or(&Value::Null);
        for k in path.split('.') {
            v = &v[k];
        }
        v
    }
}

/// A size as people read it: 25MB, 1.5GB.
fn mb(bytes: u64) -> String {
    format!("{}B", super::human_size(bytes))
}

impl Ctx {
    /// A setting: server.env, else the environment (the Docker image sets some there).
    fn setting(&self, key: &str) -> Option<String> {
        self.env_get(key).or_else(|| crate::env(key))
    }

    pub fn doctor(&self, args: &[String]) -> Result<i32> {
        let json = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
            [] => false,
            ["--json"] => true,
            _ => return Err("usage: pcds doctor [--json]".into()),
        };
        let live = self.doctor_live();
        let mut r = Report::default();
        let tables = self.check_game(&mut r);
        let world = self.check_world(&mut r, tables.as_ref());
        self.check_files(&mut r, tables.as_ref());
        self.check_ports(&mut r, &live);
        self.check_modes(&mut r);
        self.check_disk(&mut r, world.as_ref());
        self.check_backups(&mut r, &live);
        self.check_clock(&mut r, world.as_ref());
        self.check_service(&mut r);
        self.check_steam(&mut r, &live);
        self.check_banks(&mut r, &live);
        self.check_feed(&mut r);
        self.check_plugins(&mut r, &live);
        self.check_resources(&mut r, &live, world.as_ref());
        self.check_live(&mut r, &live);
        if json {
            println!("{}", serde_json::to_string_pretty(&r.json(live.running())).unwrap_or_default());
        } else {
            println!("pcds doctor: {} ({}; the server is {})\n", self.run.display(), if self.container { "in the container".to_string() } else { format!("systemd unit {}", self.unit()) }, if live.running() { "running" } else { "not running" });
            print!("{}", r.text());
        }
        Ok(if r.ok() { 0 } else { 1 })
    }

    fn doctor_live(&self) -> Live {
        let status = self.api("GET", "/api/status", None).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
        let plugins = status.as_ref().and_then(|_| self.api("GET", "/api/plugins", None).ok()).and_then(|t| serde_json::from_str::<Value>(&t).ok());
        let unreachable = match &status {
            Some(_) => None,
            None if self.active().unwrap_or(false) => Some(self.api("GET", "/api/status", None).err().unwrap_or_else(|| "no answer".into())),
            None => None,
        };
        Live { status, plugins, unreachable }
    }

    /// The game folder, the tables for its build, and that the tables are whole. Returns the tables
    /// when they load.
    fn check_game(&self, r: &mut Report) -> Option<Tables> {
        let game = match self.game_dir_peek() {
            Ok(game) => game,
            Err(e) => {
                r.fail("game folder", e, "pcds up --game \"/path/to/The Planet Crafter\" (the folder with Planet Crafter.exe), or pcds up --steam-user NAME to download it");
                return None;
            }
        };
        let build = match crate::tables::build_id(&game) {
            Ok(b) => b,
            Err(e) => {
                r.fail("game folder", e, "the folder is not a whole game: reinstall it, or pcds update --game DIR");
                return None;
            }
        };
        r.pass("game folder", format!("{} (build {build})", game.display()));
        let dir = match self.tables(&build) {
            Ok(dir) => dir,
            Err(e) => {
                r.fail("game tables", e, "pcds update (it reads the tables of this game and restarts the server)");
                return None;
            }
        };
        let path = dir.display().to_string();
        let (net, data, _spec) = match crate::try_tables(&path) {
            Ok(t) => t,
            Err(e) => {
                r.fail("game tables", format!("{path}: {e}"), "pcds extract (reads them again from the game's files)");
                return None;
            }
        };
        let format = crate::native::tables::TABLES_FORMAT;
        let scenes = match crate::try_scene_tables(&path, &net, &data) {
            Ok(s) => s,
            Err(e) => {
                r.fail("game tables", format!("{path}: format {format}, game {}, but not complete: {e}", net.game_version), "pcds extract (reads them again from the game's files)");
                return None;
            }
        };
        let dlc: Vec<String> = net.planets.iter().filter(|p| p.steam_app_id != 0).map(|p| p.id.clone()).collect();
        let with_scenes: Vec<&str> = dlc.iter().map(String::as_str).filter(|p| scenes.planets.contains_key(*p)).collect();
        r.pass(
            "game tables",
            format!("game {} in {path}, format {format}, {} planets, DLC ground for {}", net.game_version, net.planets.len(), if with_scenes.is_empty() { "none of the DLC planets".to_string() } else { with_scenes.join(", ") }),
        );
        Some(Tables { net, data, scenes, dlc })
    }

    /// The world file loads with this server's loader, and its planet is playable: a DLC planet
    /// needs its ground in the tables and the rule world.dlcPlanets. Returns the world's size.
    fn check_world(&self, r: &mut Report, tables: Option<&Tables>) -> Option<World> {
        let file = self.world_file();
        let name = self.world_name();
        let bytes = match fs::metadata(&file) {
            Ok(m) => m.len(),
            Err(_) => {
                r.fail("world", format!("no world {name} at {}", file.display()), "pcds worlds lists them, pcds world use NAME picks one, pcds world new NAME makes one");
                return None;
            }
        };
        let save = match crate::save::SaveFile::load(&file.to_string_lossy()) {
            Ok(s) => s,
            Err(e) => {
                r.fail("world", format!("{}: {e}", file.display()), "restore a backup: pcds backup verify finds a good one, pcds backup restore NAME puts it back (docs/disaster-recovery.md)");
                return None;
            }
        };
        let Some(t) = tables else {
            r.warn("world", format!("{name}: the file reads ({}), but without game tables its content cannot be checked", mb(bytes)), "fix the game tables above, then run doctor again");
            return Some(World { bytes, planet: None });
        };
        if let Err(e) = crate::game::check_save_content(&save, &t.data, &t.net) {
            r.fail("world", format!("{name}: {e}"), "pcds update (the tables may be of another game version); else restore a backup: pcds backup verify");
            return Some(World { bytes, planet: None });
        }
        let planet = crate::game::world_planet(&save, &t.net).map(|(id, _)| id).unwrap_or_default();
        r.pass("world", format!("{name}: planet {planet}, {} world objects, {} inventories, {}", save.objects.len(), save.inventories.len(), mb(bytes)));
        self.check_dlc(r, t, &planet);
        Some(World { bytes, planet: Some(planet) })
    }

    fn check_dlc(&self, r: &mut Report, t: &Tables, planet: &str) {
        let rules = crate::rules::Rules::load(&self.rules_file()).unwrap_or_default();
        let stated = &rules.world.dlc_planets;
        if t.dlc.iter().any(|p| p == planet) {
            if !t.scenes.planets.contains_key(planet) {
                r.fail("DLC planet", format!("the world is on {planet}, a DLC planet, and the game tables have no ground for it (no scenes/{planet}.json)"), "install the DLC in the game folder, then pcds extract");
            } else if !stated.iter().any(|p| p == planet) {
                let owned: Vec<&str> = stated.iter().map(String::as_str).chain([planet]).collect();
                r.fail(
                    "DLC planet",
                    format!("the world is on {planet}, a DLC planet, and the rule world.dlcPlanets does not state that you own it (the server cannot ask Steam)"),
                    format!("put \"world\": {{\"dlcPlanets\": [{}]}} in {} (the server does not start the world without it, so `pcds rules set world.dlcPlanets={}` cannot be used first), then pcds restart", owned.iter().map(|p| format!("\"{p}\"")).collect::<Vec<_>>().join(", "), self.rules_file(), owned.join(",")),
                );
            } else {
                r.pass("DLC planet", format!("{planet}: the ground is in the tables and world.dlcPlanets states it"));
            }
        }
        for p in stated.iter().filter(|p| t.dlc.contains(p) && !t.scenes.planets.contains_key(*p)) {
            r.warn("DLC planet", format!("world.dlcPlanets lists {p}, and the tables have no ground for it: a world sent there would run empty"), "install the DLC in the game folder, then pcds extract");
        }
    }

    pub(super) fn rules_file(&self) -> String {
        self.run.join("rules.json").display().to_string()
    }

    /// server.env, rules.json and access.json are valid.
    fn check_files(&self, r: &mut Report, tables: Option<&Tables>) {
        if !self.env_file.is_file() {
            r.fail("server.env", format!("{} is missing", self.env_file.display()), "pcds up");
        } else {
            match self.check_settings() {
                Ok(()) => r.pass("server.env", format!("{} reads fine", self.env_file.display())),
                Err(e) => r.fail("server.env", e, "pcds config (delete or correct the line), then pcds check-settings"),
            }
            if let retired @ [_, ..] = crate::config::retired_in_env(|key| crate::env(key).filter(|_| self.env_get(key).is_none())).as_slice() {
                r.warn("environment", format!("ignored in the environment: {}", retired.join(", ")), "remove them; docs/upgrading.md says what replaced each");
            }
        }
        let rules = self.rules_file();
        match crate::rules::Rules::load(&rules).and_then(|loaded| tables.map_or(Ok(()), |t| loaded.validate(&t.data))) {
            Ok(()) if Path::new(&rules).is_file() => r.pass("rules.json", format!("{rules} is valid")),
            Ok(()) => r.pass("rules.json", "none yet: every rule has its default"),
            Err(e) => r.fail("rules.json", e, format!("edit {rules}, or move it away to start with the defaults (pcds rules lists the rules)")),
        }
        let access = self.run.join("access.json");
        let valid = match fs::read_to_string(&access) {
            Ok(text) if !text.trim().is_empty() => serde_json::from_str::<crate::access::Lists>(&text).map_err(|e| format!("line {}, column {}: {e}", e.line(), e.column())).and_then(|l| l.check()).map(|()| true),
            Ok(_) => Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.to_string()),
        };
        match valid {
            Ok(true) => r.pass("access.json", format!("{} is valid", access.display())),
            Ok(false) => r.pass("access.json", "none yet: everyone may join"),
            Err(e) => r.fail("access.json", format!("{}: {e}", access.display()), format!("edit {}, or move it away to start with no lists (a file that does not read would let in everyone it bans, so the server refuses it)", access.display())),
        }
    }

    fn check_ports(&self, r: &mut Report, live: &Live) {
        let running = live.running();
        let listen = self.setting("PCDS_LISTEN").unwrap_or_else(|| "0.0.0.0:7777".into());
        if listen == "off" {
            r.pass("game port", "direct IP is off (PCDS_LISTEN=off); players join by invite code only");
        } else {
            match listen.parse::<SocketAddr>() {
                Err(_) => r.fail("game port", format!("PCDS_LISTEN={listen} is not an address (HOST:PORT)"), "pcds config, then pcds restart"),
                Ok(addr) => match (UdpSocket::bind(addr), running) {
                    (Err(e), true) if e.kind() == std::io::ErrorKind::AddrInUse => r.pass("game port", format!("UDP {addr} is in use, by this server")),
                    (Ok(_), true) => r.fail("game port", format!("the server runs, but nothing listens on UDP {addr}"), "pcds logs says why; pcds restart"),
                    (Ok(_), false) => r.pass("game port", format!("UDP {addr} is free")),
                    (Err(e), _) => r.fail("game port", format!("UDP {addr} cannot be used: {e}"), "another program (a second pcds server?) may hold it: change PCDS_LISTEN in server.env (pcds config), or find it with: ss -ulpn | grep :PORT"),
                },
            }
        }
        let admin = self.setting("PCDS_ADMIN").unwrap_or_else(|| "127.0.0.1:7782".into());
        if admin == "off" {
            r.pass("admin port", "the admin API and page are off (PCDS_ADMIN=off)");
            return;
        }
        let Ok(addr) = admin.parse::<SocketAddr>() else {
            r.fail("admin port", format!("PCDS_ADMIN={admin} is not an address (HOST:PORT)"), "pcds config, then pcds restart");
            return;
        };
        match (TcpListener::bind(addr), running) {
            (Err(e), true) if e.kind() == std::io::ErrorKind::AddrInUse => r.pass("admin port", format!("TCP {addr} is in use, by this server")),
            (Ok(_), true) => r.fail("admin port", format!("the server runs, but nothing listens on TCP {addr}"), "pcds logs says why; pcds restart"),
            (Ok(_), false) => r.pass("admin port", format!("TCP {addr} is free")),
            (Err(e), _) => r.fail("admin port", format!("TCP {addr} cannot be used: {e}"), "another program (a second pcds server?) may hold it: change PCDS_ADMIN in server.env (pcds config), or find it with: ss -ltnp | grep :PORT"),
        }
        if self.setting("PCDS_ADMIN_TOKEN").is_none() {
            r.warn("admin token", "the admin API has no token, so it is off", "pcds setup writes one; or put PCDS_ADMIN_TOKEN=... in server.env (pcds config)");
        }
        if !addr.ip().is_loopback() {
            if self.container {
                r.warn(
                    "admin exposure",
                    format!("inside the container the admin API listens on {addr}; whether other machines reach it is decided by the port mapping, which doctor cannot see"),
                    "publish it on this machine only: -p 127.0.0.1:7782:7782 (compose.yaml does), and use a TLS proxy for anything else (docs/reverse-proxy.md)",
                );
            } else {
                r.warn(
                    "admin exposure",
                    format!("the admin API listens on {addr}, so other machines can reach it, over plain HTTP: the token and every action travel in the clear"),
                    "set PCDS_ADMIN=127.0.0.1:7782 (pcds config, pcds restart) and reach it by an SSH tunnel or an HTTPS reverse proxy (docs/reverse-proxy.md)",
                );
            }
        } else {
            r.pass("admin exposure", format!("the admin API listens on {addr}, this machine only"));
        }
    }

    fn check_modes(&self, r: &mut Report) {
        let secrets: [(std::path::PathBuf, bool); 4] = [(self.env_file.clone(), true), (self.run.join("plugins/tokens.json"), true), (self.run.join("rules.json"), false), (self.run.join("access.json"), false)];
        let mut loose = Vec::new();
        for (path, hard) in &secrets {
            let Ok(meta) = fs::metadata(path) else { continue };
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                loose.push((path.display().to_string(), mode, *hard && mode & 0o007 != 0));
            }
        }
        if loose.is_empty() {
            r.pass("file modes", "server.env, rules.json, access.json and the plugin tokens are for this user only");
            return;
        }
        let detail = loose.iter().map(|(p, m, _)| format!("{p} is {m:03o}")).collect::<Vec<_>>().join("; ");
        let fix = format!("chmod 600 {}", loose.iter().map(|(p, ..)| p.as_str()).collect::<Vec<_>>().join(" "));
        let text = format!("{detail}: they hold the admin token, the chat secret and the S3 keys");
        if loose.iter().any(|(.., everyone)| *everyone) {
            r.fail("file modes", text, fix);
        } else {
            r.warn("file modes", text, fix);
        }
    }

    fn check_disk(&self, r: &mut Report, world: Option<&World>) {
        let Some(free) = free_bytes(&self.run) else {
            r.warn("disk space", format!("cannot read the free space of {}", self.run.display()), "df -h on that folder");
            return;
        };
        let backups = self.worlds.join(crate::worlds::BACKUPS);
        let (files, backup_bytes) = fs::read_dir(&backups).into_iter().flatten().flatten().filter_map(|e| e.metadata().ok().filter(|m| m.is_file())).fold((0u64, 0u64), |(n, b), m| (n + 1, b + m.len()));
        let size = world.map_or(0, |w| w.bytes);
        let kept = u64::from(crate::rules::Rules::load(&self.rules_file()).unwrap_or_default().save.backups_kept);
        // A save writes a copy beside the world and a backup; the backups grow to `kept`.
        let growth = (kept * size).saturating_sub(backup_bytes);
        let detail = format!("{} free; the world is {}, {files} backups take {} (up to {kept} are kept, {} more to come)", mb(free), mb(size), mb(backup_bytes), mb(growth));
        if free < 2 * size + (16 << 20) {
            r.fail("disk space", detail, "free space on that disk, or move run/ to a bigger one (docs/disaster-recovery.md); a save that cannot be written loses the progress since the last one");
        } else if free < 2 * size + growth + size.max(256 << 20) {
            r.warn("disk space", detail, "the backups will fill the disk: free space, lower the rule save.backupsKept (pcds rules set save.backupsKept=N), or move run/ to a bigger disk");
        } else {
            r.pass("disk space", detail);
        }
    }

    /// The backups: on, there, recent, and whether any copy leaves this machine.
    fn check_backups(&self, r: &mut Report, live: &Live) {
        let rules = crate::rules::Rules::load(&self.rules_file()).unwrap_or_default();
        if rules.save.backups_kept == 0 {
            r.warn("backups", "backups are off (the rule save.backupsKept is 0): a damaged world file cannot be restored", "pcds rules set save.backupsKept=24 (docs/disaster-recovery.md)");
            return;
        }
        let list = self.backup_list();
        let off_site = if rules.backup.s3_bucket.is_some() { "; the S3 rules are set, so each is copied off this machine" } else { "; all of them are on this machine" };
        let detail = match list.first() {
            None => format!("none yet (one is made each {} minutes while the server runs){off_site}", rules.save.backup_minutes),
            Some(b) => {
                let age = SystemTime::now().duration_since(b.modified).unwrap_or_default().as_secs();
                let late = live.running() && rules.save.backup_minutes > 0 && age > u64::from(rules.save.backup_minutes) * 120 + 600;
                if late {
                    r.warn("backups", format!("{} backups, the newest {} old, though one is made each {} minutes while the server runs", list.len(), super::backup::age_text(age), rules.save.backup_minutes), "pcds logs --grep backup says why none was made (a full disk, permissions of worlds/backups)");
                    return;
                }
                format!("{} backups, the newest {} old{off_site}", list.len(), super::backup::age_text(age))
            }
        };
        if rules.backup.s3_bucket.is_none() {
            r.warn("backups", detail, "set the backup.s3* rules so a lost disk does not take the backups with it, and check them with pcds backup verify (docs/disaster-recovery.md)");
        } else {
            r.pass("backups", format!("{detail}; pcds backup verify loads them"));
        }
    }

    fn check_clock(&self, r: &mut Report, world: Option<&World>) {
        let now = SystemTime::now();
        let stamp = crate::save::stamp(now);
        let stem = self.world_name();
        let backups = self.worlds.join(crate::worlds::BACKUPS);
        let ahead: Vec<String> = crate::save::backup_stamps(&backups, &stem, &stamp).unwrap_or_default().into_iter().filter(|t| *t > stamp).collect();
        let world_ahead = world.is_some() && modified(&self.world_file()) > now + Duration::from_secs(120);
        if !ahead.is_empty() || world_ahead {
            r.warn(
                "clock",
                format!(
                    "the clock reads {stamp} UTC, and {} are stamped later: it stepped back at some time. The server keeps working (it orders such backups first), but the backup times mean less",
                    if ahead.is_empty() { "the world file's time is".to_string() } else { format!("{} backups ({}...)", ahead.len(), ahead[0]) }
                ),
                "set the clock right and keep it synchronised (timedatectl set-ntp true); old backups with a time ahead can stay until they are pruned",
            );
            return;
        }
        let ntp = output(std::process::Command::new("timedatectl").args(["show", "-p", "NTPSynchronized", "--value"]));
        match ntp.as_deref() {
            Some("no") => r.warn("clock", format!("{stamp} UTC, and the system clock is not synchronised"), "timedatectl set-ntp true (time zone: timedatectl)"),
            Some("yes") => r.pass("clock", format!("{stamp} UTC, synchronised; no backup is stamped in the future")),
            _ => r.pass("clock", format!("{stamp} UTC; no backup is stamped in the future (whether NTP runs is not known here)")),
        }
    }

    fn check_service(&self, r: &mut Report) {
        if self.container {
            r.pass("service", "in the container the server is the container's own process; docker restart policy keeps it running");
            return;
        }
        let unit = self.unit();
        let path = self.unit_dir.join(&unit);
        if !path.is_file() {
            r.fail("service", format!("no unit file {}", path.display()), "pcds up (or pcds install-service)");
            return;
        }
        if let Err(e) = self.check_owner() {
            r.fail("service", e, "set PCDS_UNIT to a name of its own for this installation");
            return;
        }
        let stale = self.unit_files().map(|files| files.iter().any(|(n, text)| fs::read_to_string(self.unit_dir.join(n)).ok().as_deref() != Some(text.as_str()))).unwrap_or(false);
        if !super::have("systemctl") {
            r.warn("service", format!("{unit} is written, but there is no systemctl to ask about it"), "pcds run runs the server in a terminal instead");
            return;
        }
        // is-enabled prints the state ("disabled", "static") and exits non-zero for it.
        let said = |mut cmd: std::process::Command| cmd.stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
        let enabled = said(self.systemctl(&["is-enabled", &unit]));
        let timer = said(self.systemctl(&["is-enabled", &self.timer()]));
        let state = self.show("ActiveState").unwrap_or_else(|_| "unknown".into());
        let detail = format!("{unit}: {state}, {}; the hourly game update check is {}", if enabled.is_empty() { "not enabled" } else { &enabled }, if timer.is_empty() { "not enabled" } else { &timer });
        if stale {
            r.warn("service", format!("{detail}; the unit file differs from what this release writes"), "pcds restart writes it again (it applies at the next start)");
        } else if enabled != "enabled" || timer != "enabled" {
            r.warn("service", detail, "pcds install-service (starts it at login and checks for game updates hourly)");
        } else {
            r.pass("service", detail);
        }
        if let Err(why) = self.sandbox_check() {
            r.warn(
                "sandbox",
                format!("this machine does not let a user service have its own file system view ({why}), so the unit has no read-only file system, no private /tmp and no protected kernel settings; the other options of the sandbox hold"),
                "docs/hardening.md says how to allow it (a kernel setting) or how to run the server as a system service instead",
            );
        } else if !stale {
            r.pass("sandbox", "the unit runs the server with no new privileges, no capabilities, a read-only file system outside run/, a private /tmp and a system call filter (docs/hardening.md)");
        }
        let user = crate::env("USER").unwrap_or_default();
        match output(std::process::Command::new("loginctl").args(["show-user", &user, "-p", "Linger", "--value"])).as_deref() {
            Some("yes") => r.pass("linger", format!("{user} stays logged in, so the server runs after logout and starts with the machine")),
            Some(_) => r.warn("linger", format!("{user} is not kept logged in: the server stops at logout and does not start at boot"), format!("sudo loginctl enable-linger {user}")),
            None => r.warn("linger", "loginctl does not answer, so whether the server survives a logout is not known", format!("sudo loginctl enable-linger {user}")),
        }
    }

    fn check_steam(&self, r: &mut Report, live: &Live) {
        if self.setting("PCDS_STEAM").as_deref() == Some("off") {
            r.pass("steam", "invite codes are off (PCDS_STEAM=off); players join by direct IP");
            return;
        }
        if self.setting("PCDS_STEAM_TOKEN").is_none() {
            r.warn(
                "steam token",
                "no game server login token: the invite code changes at every start, and players with the old one cannot join",
                "make a token for app 1284190 at https://steamcommunity.com/dev/managegameservers, put PCDS_STEAM_TOKEN=... in server.env (pcds config), pcds restart",
            );
        } else {
            r.pass("steam token", "a login token is set: the invite code stays the same across restarts");
        }
        let sdk = self.home.join(".steam/sdk64/steamclient.so");
        if sdk.is_file() {
            r.pass("steamclient", format!("{} is there", sdk.display()));
        } else {
            r.warn("steamclient", format!("no {}: invite codes cannot work, direct IP can", sdk.display()), "pcds setup gets it with steamcmd (it needs the 32-bit libraries, such as lib32gcc-s1)");
        }
        match reach("api.steampowered.com:443") {
            Ok(()) => r.pass("steam network", "api.steampowered.com:443 answers"),
            Err(e) => r.warn("steam network", format!("api.steampowered.com:443: {e}"), "check DNS and the firewall: the server needs outbound TCP 443 and UDP/TCP 27015-27050 to Steam"),
        }
        if live.running() {
            match live.status("inviteCode").as_str() {
                Some(code) => r.pass("invite code", format!("{code} (the last four digits are the rule PCDS_SESSION; the rest changes with each start unless a token is set)")),
                None if live.status("state") == "stopped" => {}
                None => r.fail("invite code", "the server runs, but Steam did not log in, so there is no invite code", "pcds logs says why (the token, steamclient.so, the network); direct IP works meanwhile"),
            }
        }
    }

    fn check_banks(&self, r: &mut Report, live: &Live) {
        let banks = self.banks();
        let active = match crate::updater::active(&banks) {
            Ok(a) => a,
            Err(e) => {
                r.warn("banks", e, "pcds setup installs the program into a bank");
                return;
            }
        };
        let described: Vec<String> = crate::updater::BANKS
            .iter()
            .map(|b| match crate::updater::info(&banks, b) {
                Some(i) => format!("{b}: pcds {} (protocol {}, snapshot format {}, tables format {}){}", i.version(), crate::msg::json(&i.protocol), crate::msg::json(&i.format), crate::msg::json(&i.tables_format), if *b == active { " [active]" } else { "" }),
                None => format!("{b}: empty{}", if *b == active { " [active]" } else { "" }),
            })
            .collect();
        if crate::updater::info(&banks, &active).is_none() || !banks.join(&active).join("pcds").is_file() {
            r.fail("banks", format!("the active bank {active} holds no program ({})", described.join("; ")), "pcds setup (or pcds start) installs this program into a bank");
            return;
        }
        let standby = crate::updater::other(&active);
        let mut detail = described.join("; ");
        if crate::updater::info(&banks, standby).is_none() {
            detail += "; the standby bank is empty, so there is nothing to roll back to until the next upgrade";
        }
        let given_up = if live.running() { serde_json::from_value::<crate::msg::RolledBack>(live.status("banks.rolledBack").clone()).ok() } else { crate::updater::rolled_back(&banks) };
        let fell_back = crate::updater::fallback(&banks);
        match (given_up, fell_back) {
            (Some(g), _) => r.warn("banks", format!("{detail}; the release in bank {} ({}) was rolled back from{}", g.bank, g.tag.as_deref().unwrap_or("local build"), g.reason.map(|x| format!(": {x}")).unwrap_or_default()), "pcds upgrade installs a newer release there; until then the older one runs and update.auto skips that release"),
            (None, Some(f)) => r.warn("banks", format!("{detail}; started from bank {} because bank {} {}", f.to, f.from, f.reason), "pcds logs shows why the other bank failed; pcds upgrade installs a fixed release"),
            (None, None) => r.pass("banks", detail),
        }
        if let Err(e) = self.tables_format() {
            r.fail("banks", e, "pcds restart (it installs the program again)");
        }
    }

    fn check_feed(&self, r: &mut Report) {
        let url = self.releases_url();
        let shown = crate::updater::shown_url(&url);
        let auto = crate::rules::Rules::load(&self.rules_file()).unwrap_or_default().update.auto;
        match crate::updater::latest_tag(&url) {
            Ok(tag) => {
                let newer = crate::updater::newer(&tag, env!("CARGO_PKG_VERSION"));
                r.pass("update feed", format!("{shown} answers: the latest release is {tag}, this is {}{}; update.auto is {}", env!("CARGO_PKG_VERSION"), if newer { " (a newer one is out: pcds upgrade)" } else { "" }, if auto { "on" } else { "off" }));
            }
            Err(e) => r.warn("update feed", e, "check the network (HTTPS_PROXY if you use a proxy); nothing else depends on it, only pcds upgrade and update.auto"),
        }
    }

    fn check_plugins(&self, r: &mut Report, live: &Live) {
        let list: Vec<Value> = match &live.plugins {
            Some(v) => v["plugins"].as_array().cloned().unwrap_or_default(),
            None => {
                let host = crate::plugins::prepare(&self.run.join("plugins"), "world");
                if let Some(e) = &host.approvals_error {
                    r.fail("plugins", e.clone(), "fix or remove run/plugins/approved.json (pcds plugin list); no plugin runs until then");
                    return;
                }
                host.plugins.iter().map(crate::plugins::admin::plugin_json).collect()
            }
        };
        if list.is_empty() {
            r.pass("plugins", "none installed");
            return;
        }
        let by = |state: &str| list.iter().filter(|p| p["status"] == state).map(|p| p["id"].as_str().unwrap_or("?").to_string()).collect::<Vec<_>>();
        let (pending, bad) = (by("pending"), [by("failed"), by("invalid"), by("incompatible"), by("changed")].concat());
        let detail = format!("{} plugins, {} running{}", list.len(), by("running").len(), if live.running() { "" } else { " once the server starts (state read from their folder)" });
        if !bad.is_empty() {
            r.warn("plugins", format!("{detail}; not working: {}", bad.join(", ")), "pcds plugin list shows why for each; pcds plugin check ID runs it on sample events");
        } else if !pending.is_empty() {
            r.warn("plugins", format!("{detail}; waiting for approval: {}", pending.join(", ")), "pcds plugin enable ID (it shows what the plugin asks for first)");
        } else {
            r.pass("plugins", detail);
        }
    }

    fn check_resources(&self, r: &mut Report, live: &Live, world: Option<&World>) {
        // Memory: a hot update holds the new bank and the snapshot beside the old bank.
        let live_pid = live.status("banks.live.pid").as_u64().map(|p| p as u32);
        let bank = match live_pid {
            Some(pid) => crate::front::process_of(pid).1,
            None => world.map_or(0, |w| estimated_bank(w.bytes)),
        };
        let source = if live_pid.is_some() { "the running bank holds" } else { "the bank is estimated at" };
        let memory = crate::front::cgroup_memory().map(|c| ("this container's", c)).or_else(|| self.unit_memory().map(|c| ("the unit's", c))).or_else(|| host_memory().map(|c| ("this machine's", c)));
        match memory {
            None => r.pass("memory", "no memory limit is visible here"),
            Some((whose, (limit, used))) => {
                let detail = format!("{whose} memory is {} with {} in use; {source} {}", mb(limit), mb(used), mb(bank));
                if limit < bank + bank / 4 {
                    r.fail("memory", detail, "raise the memory limit (docker run --memory, Compose mem_limit, a Kubernetes limit, MemoryMax in the unit): the kernel would kill the server");
                } else if let Err(why) = crate::front::flip_memory(bank, Some((limit, used + if live_pid.is_some() { 0 } else { bank }))) {
                    r.warn("memory", format!("{detail}; {why}"), "raise the limit as said, or accept restarts instead of hot updates");
                } else {
                    r.pass("memory", format!("{detail}; a hot update fits"));
                }
            }
        }
        let soft = match live.status("banks.live.pid").as_u64().or_else(|| self.server_pid().and_then(|p| p.parse().ok())) {
            Some(pid) => nofile_of(pid as u32),
            None => None,
        }
        .or_else(own_nofile);
        match soft {
            Some(n) if n < 4096 => r.warn("open files", format!("the limit is {n} open files; every player connection, the admin page and the world's files count"), "raise LimitNOFILE in the unit (pcds restart writes the release's unit), or docker run --ulimit nofile=65536"),
            Some(n) => r.pass("open files", format!("the limit is {n} open files")),
            None => r.pass("open files", "the limit is not known here"),
        }
    }

    /// MemoryMax of the unit (and what the machine has used).
    fn unit_memory(&self) -> Option<(u64, u64)> {
        if self.container || !super::have("systemctl") {
            return None;
        }
        let max: u64 = self.show("MemoryMax").ok()?.trim().parse().ok()?;
        let (_, used) = host_memory()?;
        Some((max, used))
    }

    /// What the running server says about itself.
    fn check_live(&self, r: &mut Report, live: &Live) {
        if let Some(why) = &live.unreachable {
            r.fail("server", format!("the service is running, but its admin API does not answer: {why}"), "pcds logs says why; pcds restart");
            return;
        }
        if !live.running() {
            return;
        }
        let s = &live.status;
        let s = s.as_ref().unwrap_or(&Value::Null);
        r.pass("server", format!("pcds {} runs the world \"{}\" ({} players, up {} s, phase {})", s["banks"]["frontVersion"].as_str().unwrap_or("?"), s["save"].as_str().unwrap_or("?"), s["players"].as_array().map_or(0, Vec::len), s["uptimeSeconds"], s["banks"]["phase"].as_str().unwrap_or("?")));
        if let Some(w) = s["tables"]["warning"].as_str().filter(|w| !w.is_empty()) {
            r.fail("game update", w.to_string(), "pcds update (reads the new game's tables and restarts the server)");
        }
        if let Some(e) = s["lastSave"].as_object().filter(|l| l["ok"] == false) {
            r.fail("last save", format!("the last save failed: {}", e["error"].as_str().unwrap_or("?")), "free disk space or fix the permissions of the worlds folder; the next autosave tries again");
        } else if s["lastSave"].is_null() {
            r.pass("last save", "none yet since the server started");
        } else {
            let age = crate::util::now_secs().saturating_sub(s["lastSave"]["at"].as_u64().unwrap_or(0));
            r.pass("last save", format!("ok, {age} s ago"));
        }
        if let Some(e) = s["backup"]["error"].as_object() {
            r.warn("backups", format!("the last backup failed: {}", e["message"].as_str().unwrap_or("?")), "free disk space or fix the permissions of worlds/backups");
        } else if s["backup"]["enabled"] == true {
            r.pass("backups", match s["backup"]["lastOkAt"].as_u64() {
                Some(at) => format!("the last one was made {} s ago", crate::util::now_secs().saturating_sub(at)),
                None => "on; none made yet since the server started".into(),
            });
        } else {
            r.warn("backups", "backups are off (save.backupsKept is 0)", "pcds rules set save.backupsKept=24");
        }
        if let Some(l) = s["s3"]["last"].as_object().filter(|l| l["ok"] == false) {
            r.warn("s3", format!("the last copy to the bucket failed: {}", l["error"].as_str().unwrap_or("?")), "check the backup.s3* rules (pcds rules) and the bucket's keys; POST /api/backup/s3 tries again");
        } else if let Some(m) = s["s3"]["missing"].as_array().filter(|m| !m.is_empty()) {
            r.warn("s3", format!("a bucket is set, but backup.{} are not", m.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", backup.")), "pcds rules set backup.s3Endpoint=... backup.s3AccessKey=... backup.s3SecretKey=...");
        }
        let crashes = s["banks"]["crashes"].as_u64().unwrap_or(0);
        if crashes > 0 {
            r.warn("crashes", format!("the game process crashed {crashes} times in the last 10 minutes (5 make the front give up)"), "pcds logs shows the panic; report it with the log lines (docs/troubleshooting.md)");
        }
        if s["held"] == true {
            r.warn("world", "the world is paused or held", "unpause it on the admin page, or pcds api POST /api/pause '{\"paused\":false}'");
        }
    }
}

struct Tables {
    net: crate::data::NetData,
    data: crate::gamedata::GameData,
    scenes: crate::Scenes,
    /// The DLC planets the tables list.
    dlc: Vec<String>,
}

struct World {
    bytes: u64,
    #[allow(dead_code)]
    planet: Option<String>,
}

/// What a bank holds for a world of this size: measured 39 MB for a new world and 79 MB for a
/// late-game world of 33,583 objects (a 2.4 MB file); docs/hot-update.md measured 321 MB for
/// 100,000 objects. 40 MB and 32 times the file reaches all three from above.
fn estimated_bank(world_bytes: u64) -> u64 {
    (40 << 20) + world_bytes * 32
}

/// Free bytes for this user where `path` is (statvfs).
fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs fills the struct it is given from a NUL-terminated path.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0).then(|| s.f_bavail as u64 * s.f_frsize as u64)
}

/// (total, in use) memory of the machine, from /proc/meminfo.
fn host_memory() -> Option<(u64, u64)> {
    let text = fs::read_to_string("/proc/meminfo").ok()?;
    let kb = |name: &str| text.lines().find_map(|l| l.strip_prefix(name)?.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()).map(|k| k * 1024);
    let total = kb("MemTotal:")?;
    Some((total, total.saturating_sub(kb("MemAvailable:")?)))
}

/// The soft open-files limit of a process, from /proc.
fn nofile_of(pid: u32) -> Option<u64> {
    let text = fs::read_to_string(format!("/proc/{pid}/limits")).ok()?;
    text.lines().find(|l| l.starts_with("Max open files"))?.split_whitespace().nth(3)?.parse().ok()
}

fn own_nofile() -> Option<u64> {
    let mut l = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: getrlimit fills the struct it is given.
    (unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut l) } == 0).then_some(l.rlim_cur)
}

/// Whether a TCP connection to HOST:PORT opens within 5 seconds (the name is looked up on a
/// thread of its own: a resolver that hangs must not hang the command).
fn reach(address: &str) -> std::result::Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let name = address.to_string();
    std::thread::spawn(move || {
        let found = name.to_socket_addrs().map_err(|e| format!("cannot look the name up ({e})")).and_then(|mut a| a.next().ok_or_else(|| "no address".to_string()));
        let _ = tx.send(found);
    });
    let addr = rx.recv_timeout(Duration::from_secs(5)).map_err(|_| "the name lookup took more than 5 s".to_string())??;
    TcpStream::connect_timeout(&addr, Duration::from_secs(5)).map(drop).map_err(|e| format!("cannot connect ({e})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn find<'a>(r: &'a Report, id: &str) -> Vec<&'a Check> {
        r.0.iter().filter(|c| c.id == id).collect()
    }

    fn level(r: &Report, id: &str) -> Level {
        find(r, id).iter().map(|c| c.level).max().unwrap_or_else(|| panic!("no check {id}: {:?}", r.0))
    }

    fn scratch(name: &str) -> (std::path::PathBuf, Ctx) {
        let dir = super::super::test_dir(name);
        let ctx = super::super::test_ctx(&dir);
        fs::create_dir_all(&ctx.run).unwrap();
        (dir, ctx)
    }

    #[test]
    fn the_report_counts_and_prints_every_level() {
        let mut r = Report::default();
        r.pass("a", "fine");
        r.warn("b", "hmm", "do this");
        r.fail("c", "bad\nsecond line", "do that");
        assert!(!r.ok());
        assert_eq!((r.count(Level::Pass), r.count(Level::Warn), r.count(Level::Fail)), (1, 1, 1));
        let text = r.text();
        assert!(text.contains("PASS  a") && text.contains("WARN  b") && text.contains("FAIL  c"), "{text}");
        assert!(text.contains("fix: do this") && text.contains("fix: do that") && text.contains("1 FAIL, 1 WARN, 1 PASS"), "{text}");
        let j = r.json(true);
        assert_eq!((j["ok"].as_bool(), j["serverRunning"].as_bool(), j["checks"][1]["level"].as_str(), j["checks"][1]["fix"].as_str()), (Some(false), Some(true), Some("warn"), Some("do this")));
        assert!(j["checks"][0]["fix"].is_null());
    }

    #[test]
    fn secrets_that_others_can_read_fail_and_group_readable_ones_warn() {
        let (dir, ctx) = scratch("doctor-modes");
        let mut r = Report::default();
        ctx.check_modes(&mut r);
        assert_eq!(level(&r, "file modes"), Level::Pass, "no file, nothing to say");
        fs::write(&ctx.env_file, "PCDS_ADMIN_TOKEN=x\n").unwrap();
        fs::set_permissions(&ctx.env_file, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(ctx.run.join("rules.json"), "{}").unwrap();
        fs::set_permissions(ctx.run.join("rules.json"), fs::Permissions::from_mode(0o640)).unwrap();
        let mut r = Report::default();
        ctx.check_modes(&mut r);
        assert_eq!(level(&r, "file modes"), Level::Warn);
        assert!(find(&r, "file modes")[0].fix.as_ref().unwrap().starts_with("chmod 600 ") && find(&r, "file modes")[0].detail.contains("640"));
        fs::set_permissions(&ctx.env_file, fs::Permissions::from_mode(0o644)).unwrap();
        let mut r = Report::default();
        ctx.check_modes(&mut r);
        assert_eq!(level(&r, "file modes"), Level::Fail, "the admin token is readable by everyone");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_free_port_passes_a_taken_one_fails_and_ours_is_in_use_by_us() {
        let (dir, ctx) = scratch("doctor-ports");
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
        let (u, t) = (udp.local_addr().unwrap(), tcp.local_addr().unwrap());
        fs::write(&ctx.env_file, format!("PCDS_LISTEN={u}\nPCDS_ADMIN={t}\nPCDS_ADMIN_TOKEN=x\n")).unwrap();
        let stopped = Live { status: None, plugins: None, unreachable: None };
        let mut r = Report::default();
        ctx.check_ports(&mut r, &stopped);
        assert_eq!((level(&r, "game port"), level(&r, "admin port")), (Level::Fail, Level::Fail));
        assert!(find(&r, "game port")[0].fix.as_ref().unwrap().contains("ss -ulpn"));
        let running = Live { status: Some(json!({"state": "running"})), plugins: None, unreachable: None };
        let mut r = Report::default();
        ctx.check_ports(&mut r, &running);
        assert_eq!((level(&r, "game port"), level(&r, "admin port")), (Level::Pass, Level::Pass));
        assert!(find(&r, "game port")[0].detail.contains("by this server"));
        drop((udp, tcp));
        let mut r = Report::default();
        ctx.check_ports(&mut r, &stopped);
        assert_eq!((level(&r, "game port"), level(&r, "admin port")), (Level::Pass, Level::Pass));
        let mut r = Report::default();
        ctx.check_ports(&mut r, &running);
        assert_eq!(level(&r, "game port"), Level::Fail, "the server runs and nothing listens");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_admin_port_beyond_the_loopback_warns_with_the_advice() {
        let (dir, mut ctx) = scratch("doctor-exposure");
        let stopped = Live { status: None, plugins: None, unreachable: None };
        for (admin, container, expect_warn, words) in [("127.0.0.1:0", false, false, "this machine only"), ("0.0.0.0:0", false, true, "reverse-proxy"), ("0.0.0.0:0", true, true, "127.0.0.1:7782:7782"), ("[::1]:0", false, false, "this machine only"), ("off", false, false, "off")] {
            ctx.container = container;
            fs::write(&ctx.env_file, format!("PCDS_LISTEN=off\nPCDS_ADMIN={admin}\nPCDS_ADMIN_TOKEN=x\n")).unwrap();
            let mut r = Report::default();
            ctx.check_ports(&mut r, &stopped);
            let admin_check = find(&r, "admin exposure").first().copied().unwrap_or(find(&r, "admin port")[0]);
            assert_eq!(admin_check.level == Level::Warn, expect_warn, "{admin} {container}: {r:?}");
            assert!(admin_check.detail.contains(words) || admin_check.fix.as_deref().is_some_and(|f| f.contains(words)), "{admin}: {admin_check:?}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn settings_files_that_do_not_hold_up_fail_with_the_fix() {
        let (dir, ctx) = scratch("doctor-files");
        let mut r = Report::default();
        ctx.check_files(&mut r, None);
        assert_eq!(level(&r, "server.env"), Level::Fail);
        assert!(find(&r, "server.env")[0].detail.contains("missing"));
        let unknown = format!("PCDS_{}", "NOT_A_SETTING");
        fs::write(&ctx.env_file, format!("PCDS_LISTEN=0.0.0.0:7777\n{unknown}=1\n")).unwrap();
        fs::write(ctx.run.join("rules.json"), r#"{"speed": {"nonsense": 2}}"#).unwrap();
        fs::write(ctx.run.join("access.json"), r#"{"bannedNames": "#).unwrap();
        let mut r = Report::default();
        ctx.check_files(&mut r, None);
        assert_eq!((level(&r, "server.env"), level(&r, "rules.json"), level(&r, "access.json")), (Level::Fail, Level::Fail, Level::Fail), "{r:?}");
        assert!(find(&r, "server.env")[0].detail.contains(&unknown));
        fs::write(&ctx.env_file, "PCDS_LISTEN=0.0.0.0:7777\n").unwrap();
        fs::write(ctx.run.join("rules.json"), r#"{"speed": {"growth": 2}}"#).unwrap();
        fs::write(ctx.run.join("access.json"), r#"{"bannedNames": ["Mallory"]}"#).unwrap();
        let mut r = Report::default();
        ctx.check_files(&mut r, None);
        assert_eq!((level(&r, "server.env"), level(&r, "rules.json"), level(&r, "access.json")), (Level::Pass, Level::Pass, Level::Pass), "{r:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_disk_must_hold_two_copies_of_the_world_and_the_backups_to_come() {
        let (dir, ctx) = scratch("doctor-disk");
        let free = free_bytes(&ctx.run).unwrap();
        let mut r = Report::default();
        ctx.check_disk(&mut r, Some(&World { bytes: free, planet: None }));
        assert_eq!(level(&r, "disk space"), Level::Fail, "a world as big as the free space cannot be saved");
        let mut r = Report::default();
        ctx.check_disk(&mut r, Some(&World { bytes: 1 << 20, planet: None }));
        assert_ne!(level(&r, "disk space"), Level::Fail);
        assert!(find(&r, "disk space")[0].detail.contains("free") && find(&r, "disk space")[0].detail.contains("backups"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn backups_off_or_only_on_this_machine_warn_and_a_bucket_passes() {
        let (dir, ctx) = scratch("doctor-backups");
        let stopped = Live { status: None, plugins: None, unreachable: None };
        let running = Live { status: Some(json!({"state": "running"})), plugins: None, unreachable: None };
        let check = |live: &Live| {
            let mut r = Report::default();
            ctx.check_backups(&mut r, live);
            r
        };
        fs::write(ctx.run.join("rules.json"), r#"{"save": {"backupsKept": 0}}"#).unwrap();
        let r = check(&stopped);
        assert_eq!(level(&r, "backups"), Level::Warn);
        assert!(find(&r, "backups")[0].detail.contains("backups are off") && find(&r, "backups")[0].fix.as_ref().unwrap().contains("save.backupsKept=24"));
        // On, none yet, no bucket: the warning is about where they are.
        fs::write(ctx.run.join("rules.json"), "{}").unwrap();
        let r = check(&stopped);
        assert!(find(&r, "backups")[0].detail.contains("none yet") && find(&r, "backups")[0].detail.contains("all of them are on this machine"), "{r:?}");
        let backups = ctx.worlds.join("backups");
        fs::create_dir_all(&backups).unwrap();
        fs::write(backups.join("world-20261001-120000.json"), "{}").unwrap();
        let r = check(&stopped);
        assert_eq!(level(&r, "backups"), Level::Warn);
        assert!(find(&r, "backups")[0].detail.contains("1 backups, the newest"), "{r:?}");
        // The file is old (the stamp is the name's; the age is the file's time): late while the server runs.
        let long_ago = fs::File::options().write(true).open(backups.join("world-20261001-120000.json")).unwrap();
        long_ago.set_modified(SystemTime::now() - Duration::from_secs(5 * 86_400)).unwrap();
        assert!(find(&check(&running), "backups")[0].detail.contains("5 days old, though one is made each 60 minutes"));
        assert!(!find(&check(&stopped), "backups")[0].detail.contains("though"), "a stopped server makes none");
        // A bucket: the backups leave the machine, and it is a pass.
        fs::write(ctx.run.join("rules.json"), r#"{"backup": {"s3Endpoint": "https://s3.example", "s3Bucket": "saves", "s3AccessKey": "a", "s3SecretKey": "b"}}"#).unwrap();
        let r = check(&stopped);
        assert_eq!(level(&r, "backups"), Level::Pass);
        assert!(find(&r, "backups")[0].detail.contains("copied off this machine"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_backup_stamped_in_the_future_warns_about_the_clock() {
        let (dir, ctx) = scratch("doctor-clock");
        let backups = ctx.worlds.join("backups");
        fs::create_dir_all(&backups).unwrap();
        let mut r = Report::default();
        ctx.check_clock(&mut r, None);
        assert_eq!(level(&r, "clock"), Level::Pass);
        fs::write(backups.join("world-20990101-000000.json"), "x").unwrap();
        let mut r = Report::default();
        ctx.check_clock(&mut r, None);
        assert_eq!(level(&r, "clock"), Level::Warn);
        assert!(find(&r, "clock")[0].detail.contains("20990101-000000") && find(&r, "clock")[0].fix.as_ref().unwrap().contains("set-ntp"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_service_checks_the_unit_file_and_the_container_has_none() {
        let (dir, mut ctx) = scratch("doctor-service");
        let mut r = Report::default();
        ctx.check_service(&mut r);
        assert_eq!(level(&r, "service"), Level::Fail);
        assert!(find(&r, "service")[0].fix.as_ref().unwrap().contains("pcds up"));
        ctx.container = true;
        let mut r = Report::default();
        ctx.check_service(&mut r);
        assert_eq!(level(&r, "service"), Level::Pass);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn steam_without_a_token_warns_that_the_code_changes() {
        let (dir, ctx) = scratch("doctor-steam");
        let stopped = Live { status: None, plugins: None, unreachable: None };
        fs::write(&ctx.env_file, "PCDS_STEAM=off\n").unwrap();
        let mut r = Report::default();
        ctx.check_steam(&mut r, &stopped);
        assert_eq!(level(&r, "steam"), Level::Pass);
        fs::write(&ctx.env_file, "PCDS_STEAM=on\nPCDS_STEAM_TOKEN=\n").unwrap();
        let mut r = Report::default();
        ctx.check_steam(&mut r, &stopped);
        assert_eq!(level(&r, "steam token"), Level::Warn);
        assert!(find(&r, "steam token")[0].detail.contains("changes at every start"));
        fs::write(&ctx.env_file, "PCDS_STEAM=on\nPCDS_STEAM_TOKEN=abc\n").unwrap();
        let mut r = Report::default();
        ctx.check_steam(&mut r, &stopped);
        assert_eq!(level(&r, "steam token"), Level::Pass);
        // A running server on Steam without an invite code is a failure.
        let running = Live { status: Some(json!({"state": "running", "inviteCode": null})), plugins: None, unreachable: None };
        let mut r = Report::default();
        ctx.check_steam(&mut r, &running);
        assert_eq!(level(&r, "invite code"), Level::Fail);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_running_server_reports_its_saves_backups_and_crashes() {
        let (dir, ctx) = scratch("doctor-live");
        let status = json!({
            "state": "running", "save": "world", "players": [], "uptimeSeconds": 5,
            "banks": {"frontVersion": "0.8.0", "phase": "idle", "crashes": 2},
            "tables": {"warning": "game build 1 has no tables: run pcds update"},
            "lastSave": {"ok": false, "at": 1, "error": "disk full"},
            "backup": {"enabled": true, "lastOkAt": null, "error": {"at": 1, "message": "no space"}},
            "s3": {"enabled": true, "missing": ["s3Endpoint"], "last": null},
            "held": false
        });
        let mut r = Report::default();
        ctx.check_live(&mut r, &Live { status: Some(status), plugins: None, unreachable: None });
        assert_eq!((level(&r, "game update"), level(&r, "last save"), level(&r, "backups"), level(&r, "s3"), level(&r, "crashes")), (Level::Fail, Level::Fail, Level::Warn, Level::Warn, Level::Warn), "{r:?}");
        let mut r = Report::default();
        ctx.check_live(&mut r, &Live { status: None, plugins: None, unreachable: Some("connection refused".into()) });
        assert_eq!(level(&r, "server"), Level::Fail);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn plugins_waiting_or_broken_warn_and_the_folder_is_read_when_stopped() {
        let (dir, ctx) = scratch("doctor-plugins");
        let mut r = Report::default();
        ctx.check_plugins(&mut r, &Live { status: None, plugins: None, unreachable: None });
        assert_eq!(level(&r, "plugins"), Level::Pass);
        let live = |plugins: Value| Live { status: Some(json!({})), plugins: Some(json!({"plugins": plugins})), unreachable: None };
        let mut r = Report::default();
        ctx.check_plugins(&mut r, &live(json!([{"id": "a", "status": "running"}, {"id": "b", "status": "pending"}])));
        assert_eq!(level(&r, "plugins"), Level::Warn);
        assert!(find(&r, "plugins")[0].detail.contains("waiting for approval: b"));
        let mut r = Report::default();
        ctx.check_plugins(&mut r, &live(json!([{"id": "a", "status": "running"}, {"id": "b", "status": "failed"}])));
        assert!(find(&r, "plugins")[0].detail.contains("not working: b"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_command_refuses_unknown_options_and_exits_one_on_a_failure() {
        let (dir, ctx) = scratch("doctor-run");
        assert!(ctx.doctor(&["--fast".into()]).unwrap_err().contains("usage: pcds doctor [--json]"));
        // No game, no settings, no world: failures, so exit code 1.
        fs::write(&ctx.env_file, "PCDS_STEAM=off\nPCDS_LISTEN=off\nPCDS_ADMIN=off\nPCDS_RELEASES=http://127.0.0.1:9\n").unwrap();
        assert_eq!(ctx.doctor(&["--json".into()]).unwrap(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_files_and_memory_are_read_from_the_kernel() {
        assert!(own_nofile().is_some_and(|n| n > 0));
        assert!(nofile_of(std::process::id()).is_some_and(|n| n > 0));
        assert!(host_memory().is_some_and(|(total, used)| total > used && used > 0));
        assert_eq!(estimated_bank(0), 40 << 20);
        assert!(estimated_bank(2_458_107) > 79 << 20 && estimated_bank(7 << 20) >= 264 << 20);
        assert!(mb(1) == "1B" && mb(3 << 20) == "3.0MB" && mb(25 << 20) == "25MB");
    }
}
