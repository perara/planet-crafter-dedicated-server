//! The command line that installs and runs a server (`pcds COMMAND`, the HELP below). It keeps
//! everything in run/ of the release or checkout folder, or in PCDS_HOME, and runs the server as
//! a systemd user service (or as the Docker container's own process).

mod api;
mod env;
pub use env::{build_id, find_tables, set_env_file};
mod extract;
mod plugin;
mod setup;
mod steam;
mod systemd;
mod upgrade;
mod worlds;

use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime};

use crate::util::modified;

/// Every failure ends the command with this message.
pub type Result<T = ()> = std::result::Result<T, String>;

const APP_ID: &str = "1284190";

const HELP: &str = "\
pcds: install and run a Planet Crafter dedicated server on Linux.

  pcds up [--game DIR] [--world SAVE.json | --new [NEW OPTIONS]] [--steam-user NAME]
                         everything at once: setup, the service, a selftest, how to join
  pcds setup [--game DIR] [--world SAVE.json | --new [NEW OPTIONS]] [--steam-user NAME]
                         extract the game tables from your game (--steam-user: download the
                         game with steamcmd first), put a world in place and write the
                         settings and the systemd unit
  pcds worlds                 the worlds in run/worlds/ (* = the one the server runs)
  pcds world use NAME | new NAME [NEW OPTIONS] | import SAVE.json [NAME] | remove NAME
                         import also converts a save of the game's old layouts (2022, one
                         planet) as the game does when it opens one; setup --world, world
                         use and the start refuse such a save and say so
  pcds new [--planet ID] [--start LOCATION] [--seed N] [--set KEY=VALUE]... [--force]
                         replace the current world with a new one (kept in backups/)
  pcds start | stop | restart | status [--json] | logs
  pcds check-settings         read server.env as the next start will, and say what stops it (nothing is restarted)
  pcds upgrade [VERSION] [--restart] | --rollback | --check
                         a new server release from GitHub (the latest, or VERSION) into the
                         standby bank, and the running world handed over to it with nobody
                         disconnected; --rollback hands it back to the other bank; --restart
                         installs it and restarts the server instead (everyone reconnects)
  pcds install-service | uninstall-service    start at login, check for game updates hourly
  pcds code | players | save | kick PLAYER | ban PLAYER | unban NAME_OR_IP
  pcds gui | token | config | api METHOD PATH [JSON]
  pcds rules [set SECTION.RULE=VALUE ... | reset]
                         show or change the server rules (rules.json; README \"Server rules\")
  pcds plugin list | check PATH | install PATH | enable ID | disable ID | reload | new ID
                         plugins: commands, rules and reactions added without changing the server
                         (docs/plugins-guide.md); run pcds plugin for the details
  pcds extract [--game DIR]   read the game tables again (from the game's files, no Wine)
  pcds update [--game DIR]    after a game update, or for a release that reads another tables
                         format: read the tables and restart when they changed
  pcds selftest               a scripted player joins over direct IP and checks the answers
  pcds run                    run the server in the foreground (what the service runs)
  pcds container [NEW OPTIONS]  the Docker image's start (tables, world, then run); the
                         options make the first world when there is none

The server and its tools:
  pcds serve DATA_DIR SAVE.json          the server itself, settings from the environment
  pcds new-save SAVE.json DATA_DIR [NEW OPTIONS]
  pcds probe HOST:PORT DATA_DIR SCRIPT.json
  pcds load HOST:PORT DATA_DIR [PLAYERS] [SECONDS] [ACTION_MS] [NAME]
  pcds read-game GAME_DIR OUT_DIR        the game tables, read from its files
  pcds --version [DATA_DIR]

Everything the server keeps lives in run/ of the release or checkout folder (PCDS_HOME
overrides it): worlds/ (one save per world, with their backups/), server.env, access.json,
rules.json, data/<game version>/ (the game tables), and bin/ (the program the units run, and the
two banks a/ and b/ the server runs from, with bin/active). The game folder is --game DIR,
PCDS_GAME_DIR, the one used last (run/game-dir), else the Steam library that lists the game.
";

/// Where this installation's files are.
#[derive(Clone)]
pub struct Ctx {
    /// This program. Its folder is a release (the program, libsteam_api.so and web/ side by
    /// side, as in the Docker image), or server/target/<profile>/ of the checkout it was built
    /// in: the web page is then the checkout's.
    exe: PathBuf,
    web: PathBuf,
    run: PathBuf,
    env_file: PathBuf,
    worlds: PathBuf,
    steamcmd: PathBuf,
    unit_dir: PathBuf,
    /// PCDS_HOME and PCDS_UNIT give a second server on the same machine its own folder and units.
    unit_name: String,
    /// In the container there is no systemd: the server is the container's own process.
    container: bool,
    home: PathBuf,
}

pub fn run(args: &[String]) -> ! {
    let code = Ctx::new().and_then(|ctx| ctx.hand_over(args).and_then(|_| ctx.command(args))).unwrap_or_else(|e| {
        eprintln!("pcds: {e}");
        1
    });
    std::process::exit(code)
}

impl Ctx {
    fn new() -> Result<Ctx> {
        let exe = std::env::current_exe().map_err(|e| format!("where is this program: {e}"))?;
        let dir = exe.parent().ok_or("this program has no folder")?.to_path_buf();
        let checkout = Path::new(env!("CARGO_MANIFEST_DIR"));
        let (root, web) = if dir.starts_with(checkout.join("target")) {
            (checkout.parent().ok_or("no checkout folder")?.to_path_buf(), checkout.join("web/index.html"))
        } else {
            (dir.clone(), dir.join("web/index.html"))
        };
        let run = crate::env("PCDS_HOME").map(PathBuf::from).unwrap_or_else(|| root.join("run"));
        let run = std::path::absolute(&run).map_err(|e| format!("{}: {e}", run.display()))?;
        let home = PathBuf::from(crate::env("HOME").ok_or("HOME is not set")?);
        let unit_dir = crate::env("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config")).join("systemd/user");
        let ctx = Ctx {
            exe,
            web,
            env_file: run.join("server.env"),
            worlds: run.join("worlds"),
            steamcmd: run.join("steamcmd"),
            run,
            unit_dir,
            unit_name: crate::env("PCDS_UNIT").unwrap_or_else(|| "pcds".into()),
            container: crate::env("PCDS_CONTAINER").is_some(),
            home,
        };
        // In the container, a command run as root (docker exec) goes on as the owner of /data.
        // SAFETY: plain libc calls, before any other thread exists.
        if ctx.container
            && unsafe { libc::geteuid() } == 0
            && let Some(m) = fs::metadata(&ctx.run).ok().filter(|m| m.uid() != 0)
            && unsafe { libc::setgroups(0, std::ptr::null()) != 0 || libc::setgid(m.gid()) != 0 || libc::setuid(m.uid()) != 0 }
        {
            return Err(format!("cannot become the owner of {}: {}", ctx.run.display(), std::io::Error::last_os_error()));
        }
        if unsafe { libc::geteuid() } == 0 {
            return Err("run pcds as the user the server runs as, not as root".into());
        }
        Ok(ctx)
    }

    /// run/bin/pcds, what the units run, hands every command to the active bank's program
    /// (exec, same pid). So the one rule is: the active bank's program is the server, the
    /// launcher (`pcds run`: its supervisor), the game-update check and the tables' reader,
    /// whichever build last wrote run/bin/pcds; an upgrade through the API, the automatic
    /// update or `pcds upgrade` moves all of them at once. Any other copy of pcds (a release
    /// folder, a checkout's build) runs as itself, and installs itself into a bank when newer.
    fn hand_over(&self, args: &[String]) -> Result {
        let program = self.program();
        if !same_file(&self.exe, &self.installed()) || same_file(&program, &self.exe) {
            return Ok(());
        }
        use std::os::unix::process::CommandExt;
        let e = Command::new(&program).args(args).env("PCDS_HOME", &self.run).exec();
        Err(format!("{}: {e}", program.display()))
    }

    /// Runs one command; its exit code.
    fn command(&self, args: &[String]) -> Result<i32> {
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        let usage = |text: &str| Err::<i32, String>(text.to_string());
        match a.as_slice() {
            ["up", ..] => self.up(&args[1..])?,
            ["setup", ..] => self.setup(&args[1..])?,
            ["new", ..] => self.new_world_here(&args[1..])?,
            ["worlds"] => self.worlds()?,
            ["world", ..] => self.world(&args[1..])?,
            [what @ ("start" | "stop" | "restart")] => {
                self.not_in_container(what)?;
                if !self.unit_dir.join(self.unit()).is_file() {
                    return Err("run 'pcds setup' first".into());
                }
                // Another installation's unit of the same name is not ours to run.
                self.check_owner()?;
                // After `make build` or in a newer release folder: the unit runs the new program.
                if *what != "stop" {
                    self.check_settings()?;
                    self.install_server()?;
                    self.refresh_unit()?;
                    // A newer bank may read another tables format.
                    self.ensure_tables(None)?;
                    // Start requests count toward the unit's start limit (5 in 10 minutes).
                    quiet(&mut self.systemctl(&["reset-failed", &self.unit()]));
                }
                return passthrough(&mut self.systemctl(&[what, &self.unit()]));
            }
            ["upgrade", ..] => self.upgrade(&args[1..])?,
            ["check-settings"] => {
                self.check_settings()?;
                say("server.env is fine for this release");
            }
            ["status"] => self.status()?,
            ["status", "--json"] => self.status_json()?,
            ["logs"] => {
                self.not_in_container("logs")?;
                return passthrough(Command::new("journalctl").args(["--user", "-o", "cat", "-u", &self.unit(), "-n", "100", "-f"]));
            }
            ["install-service"] => self.install_service()?,
            ["uninstall-service"] => self.uninstall_service()?,
            ["code"] => self.code()?,
            ["players"] => self.players()?,
            ["save"] => println!("{}", self.api("POST", "/api/save", None)?),
            ["kick", player] => println!("{}", self.api("POST", "/api/kick", Some(&serde_json::json!({"name": player}).to_string()))?),
            ["kick", ..] => return usage("pcds kick PLAYER"),
            ["ban", player] => println!("{}", self.api("POST", "/api/ban", Some(&serde_json::json!({"name": player}).to_string()))?),
            ["ban", ..] => return usage("pcds ban PLAYER"),
            ["unban", value] => println!("{}", self.api("POST", "/api/unban", Some(&serde_json::json!({"value": value}).to_string()))?),
            ["unban", ..] => return usage("pcds unban NAME_OR_IP"),
            ["gui"] => println!("{}", self.gui_url()),
            ["token"] => println!("{}", self.env_get("PCDS_ADMIN_TOKEN").ok_or(format!("no PCDS_ADMIN_TOKEN in {}", self.env_file.display()))?),
            ["config"] => {
                // EDITOR may carry arguments ("code -w").
                let editor = crate::env("EDITOR").unwrap_or_else(|| "nano".into());
                let mut words = editor.split_whitespace();
                let program = words.next().ok_or("EDITOR is empty")?;
                return passthrough(Command::new(program).args(words).arg(&self.env_file));
            }
            ["api", method, path] => println!("{}", self.api(method, path, None)?),
            ["api", method, path, body] => println!("{}", self.api(method, path, Some(body))?),
            ["api", ..] => return usage("pcds api METHOD PATH [JSON]"),
            ["rules", ..] => self.rules(&args[1..])?,
            ["plugin", ..] => self.plugin(&args[1..])?,
            ["extract", ..] => self.extract(game_option(&a[1..])?.as_deref())?,
            ["update", ..] => self.update(game_option(&a[1..])?.as_deref())?,
            ["selftest"] => self.selftest()?,
            ["run"] => self.run_server()?,
            ["container", ..] => self.container(&args[1..])?,
            _ => {
                print!("{HELP}");
                return Ok(1);
            }
        }
        Ok(0)
    }

    fn not_in_container(&self, what: &str) -> Result {
        if self.container {
            return Err(format!("in the container: use 'docker {what} CONTAINER' instead"));
        }
        Ok(())
    }
}

/// `--game DIR`, the only option of extract and update.
fn game_option(args: &[&str]) -> Result<Option<PathBuf>> {
    match args {
        [] => Ok(None),
        ["--game", dir] => Ok(Some(PathBuf::from(dir))),
        ["--game"] => Err("--game DIR".into()),
        [other, ..] => Err(format!("unknown option {other}")),
    }
}

fn say(message: impl std::fmt::Display) {
    println!("pcds: {message}");
}

/// An exclusive lock on `path` (flock), held while the file lives: Err with `busy` and the
/// holder's pid when another process has it. The file holds the holder's pid.
pub fn hold(path: &Path, busy: &str) -> Result<fs::File> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let mut file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    // SAFETY: flock on a descriptor this function owns; the lock lasts as long as the file.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let holder = fs::read_to_string(path).unwrap_or_default();
        return Err(format!("{busy} (process {}, which holds {}); stop it or wait for it", holder.trim(), path.display()));
    }
    // The holder, for the message above.
    let _ = file.set_len(0).and_then(|_| write!(file, "{}", std::process::id()));
    Ok(file)
}

/// Whether a program is on the PATH.
fn have(program: &str) -> bool {
    let executable = |p: PathBuf| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|d| executable(d.join(program))))
}

fn need(program: &str, why: &str) -> Result {
    if have(program) { Ok(()) } else { Err(format!("{program} is needed: {why}")) }
}

fn describe(cmd: &Command) -> String {
    std::iter::once(cmd.get_program()).chain(cmd.get_args()).map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" ")
}

/// Runs a command to its end; Err unless it succeeds.
fn check(cmd: &mut Command) -> Result {
    match cmd.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("{} failed ({s})", describe(cmd))),
        Err(e) => Err(format!("{}: {e}", describe(cmd))),
    }
}

/// Runs a command without its output; whether it succeeded.
fn quiet(cmd: &mut Command) -> bool {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// What a command prints, when it succeeds.
fn output(cmd: &mut Command) -> Option<String> {
    let out = cmd.stdin(Stdio::null()).stderr(Stdio::null()).output().ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Runs a command on this terminal; its exit code.
fn passthrough(cmd: &mut Command) -> Result<i32> {
    let status = cmd.status().map_err(|e| format!("{}: {e}", describe(cmd)))?;
    Ok(status.code().unwrap_or(1))
}

/// Waits for a child at most `limit` (timeout(1)); None when it had to be killed.
fn wait_for(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

fn sleep(seconds: u64) {
    std::thread::sleep(Duration::from_secs(seconds));
}

/// Downloads with curl (curl's only use).
fn download(url: &str, path: &Path) -> Result {
    let part = path.with_extension("part");
    need("curl", "install it with your package manager")?;
    check(Command::new("curl").args(["-fsSL", "-o"]).arg(&part).arg(url))?;
    fs::rename(&part, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Copy then rename, so a running server keeps its mapped copy.
fn install_file(from: &Path, to: &Path) -> Result {
    let bytes = fs::read(from).map_err(|e| format!("{}: {e}", from.display()))?;
    let mode = fs::metadata(from).map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions())).map_err(|e| format!("{}: {e}", from.display()))?;
    replace_file(to, &bytes, mode)
}

/// A file written whole or not at all: beside it, flushed to disk, then moved over it (and the
/// move flushed), so a crash or a full disk leaves the old file or the new one, never part.
pub fn replace_file(path: &Path, bytes: &[u8], mode: u32) -> Result {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let new = path.with_file_name(format!(".{name}.new"));
    let _ = fs::remove_file(&new);
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&new).map_err(err)?;
    let written = f.write_all(bytes).and_then(|_| f.sync_all()).and_then(|_| fs::rename(&new, path));
    if let Err(e) = written {
        let _ = fs::remove_file(&new);
        return Err(err(e));
    }
    if let Some(dir) = path.parent() {
        fs::File::open(dir).and_then(|d| d.sync_all()).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    Ok(())
}

/// A path in `dir` for "<base>.json" that does not exist yet (a counter when one does), so
/// two backups in the same second never replace each other.
pub fn free_path(dir: &Path, base: &str) -> PathBuf {
    let mut path = dir.join(format!("{base}.json"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{base}-{n}.json"));
        n += 1;
    }
    path
}

fn write(path: &Path, text: impl AsRef<[u8]>) -> Result {
    fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

fn mkdir(path: &Path) -> Result {
    fs::create_dir_all(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn is_tty() -> bool {
    // SAFETY: isatty only looks at the descriptor.
    unsafe { libc::isatty(0) == 1 }
}

/// A random token (the admin API's).
fn token() -> Result<String> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).map_err(|e| format!("/dev/urandom: {e}"))?;
    Ok(crate::util::to_hex(&bytes))
}

/// "YYYY-MM-DD HH:MM" in local time (date -r FILE '+%Y-%m-%d %H:%M').
fn local_time(t: SystemTime) -> String {
    let secs = t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as libc::time_t).unwrap_or(0);
    // SAFETY: localtime_r fills the tm it is given.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&secs, &mut tm);
        tm
    };
    format!("{:04}-{:02}-{:02} {:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
}

/// A file size as du -h writes it (1024s, one decimal under 10).
fn human_size(bytes: u64) -> String {
    let mut value = bytes as f64;
    let mut unit = "";
    for u in ["K", "M", "G", "T"] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = u;
    }
    match unit {
        "" => bytes.to_string(),
        _ if value < 10.0 => format!("{:.1}{unit}", (value * 10.0).ceil() / 10.0),
        _ => format!("{}{unit}", value.ceil()),
    }
}

/// Whether two paths are one file (through symbolic or hard links too).
fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// Whether a file is a save of the game (the bash pcds grepped for this).
fn is_save(path: &Path) -> bool {
    fs::read(path).is_ok_and(|b| b.windows(15).any(|w| w == b"saveDisplayName"))
}

/// Whether a file is a world this server reads: the refusals of `SaveFile::load` (an old
/// layout, a file cut short, no game settings), each naming the file.
fn check_world(path: &Path) -> Result {
    crate::save::SaveFile::load(&path.to_string_lossy()).map(drop).map_err(|e| format!("{}: {e}", path.display()))
}

/// An installation in `dir` for tests: the program in build/, the home in home/, run/, and a
/// unit name no server uses.
#[cfg(test)]
fn test_ctx(dir: &Path) -> Ctx {
    let run = dir.join("run");
    Ctx {
        exe: dir.join("build/pcds"),
        web: dir.join("build/web/index.html"),
        env_file: run.join("server.env"),
        worlds: run.join("worlds"),
        steamcmd: run.join("steamcmd"),
        run,
        unit_dir: dir.join("systemd"),
        unit_name: "pcds-cli-test".into(),
        container: false,
        home: dir.join("home"),
    }
}

/// A new empty folder for one test.
#[cfg(test)]
fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pcds-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_is_held_by_one_holder_at_a_time() {
        let dir = test_dir("hold");
        let path = dir.join("world.json.lock");
        let first = hold(&path, "another server runs this world").unwrap();
        let e = hold(&path, "another server runs this world").unwrap_err();
        assert!(e.contains("another server runs this world") && e.contains(&std::process::id().to_string()), "{e}");
        drop(first);
        hold(&path, "another server runs this world").unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sizes_read_like_du() {
        assert_eq!(human_size(0), "0");
        assert_eq!(human_size(1023), "1023");
        assert_eq!(human_size(1536), "1.5K");
        assert_eq!(human_size(3 << 20), "3.0M");
        assert_eq!(human_size(25 << 20), "25M");
    }

    #[test]
    fn options_of_extract_and_update() {
        assert_eq!(game_option(&[]).unwrap(), None);
        assert_eq!(game_option(&["--game", "/g"]).unwrap(), Some(PathBuf::from("/g")));
        assert!(game_option(&["--game"]).is_err());
        assert!(game_option(&["--force"]).unwrap_err().contains("--force"));
    }
}
