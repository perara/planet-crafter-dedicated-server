//! The command line that installs and runs a server (`pcds COMMAND`, the HELP below). It keeps
//! everything in run/ of the release or checkout folder, or in PCDS_HOME, and runs the server as
//! a systemd user service (or as the Docker container's own process).

mod api;
mod env;
mod extract;
mod setup;
mod sha256;
mod steam;
mod systemd;
mod worlds;

use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime};

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
  pcds new [--planet ID] [--start LOCATION] [--seed N] [--set KEY=VALUE]... [--force]
                         replace the current world with a new one (kept in backups/)
  pcds start | stop | restart | status | logs
  pcds install-service | uninstall-service    start at login, check for game updates hourly
  pcds code | players | save | kick PLAYER | ban PLAYER | unban NAME_OR_IP
  pcds gui | token | config | api METHOD PATH [JSON]
  pcds rules [set SECTION.RULE=VALUE ... | reset]
                         show or change the server rules (rules.json; README \"Server rules\")
  pcds extract [--game DIR]   extract the game tables again (needs Wine)
  pcds update [--game DIR]    after a game update: extract and restart when the game changed
  pcds selftest               a scripted player joins over direct IP and checks the answers
  pcds run                    run the server in the foreground (what the service runs)
  pcds container              the Docker image's start (tables, world, then run)

The server and its tools:
  pcds serve DATA_DIR SAVE.json          the server itself, settings from the environment
  pcds new-save SAVE.json DATA_DIR [NEW OPTIONS]
  pcds probe HOST:PORT DATA_DIR SCRIPT.json
  pcds load HOST:PORT DATA_DIR [PLAYERS] [SECONDS] [ACTION_MS]
  pcds extract-raw RAW.json OUT_DIR
  pcds --version [DATA_DIR]

Everything the server keeps lives in run/ of the release or checkout folder (PCDS_HOME
overrides it): worlds/ (one save per world, with their backups/), server.env, access.json,
rules.json, data/<game version>/ and the extractor. The game folder is --game DIR,
PCDS_GAME_DIR, the one used last (run/game-dir), else the Steam library that lists the game.
";

/// Where this installation's files are.
#[derive(Clone)]
pub struct Ctx {
    /// This program. Its folder is a release (the program, libsteam_api.so, PcdsExtractor.dll
    /// and web/ side by side, as in the Docker image), or server/target/<profile>/ of the
    /// checkout it was built in: the web page and the extractor are then the checkout's.
    exe: PathBuf,
    web: PathBuf,
    extractor_dll: PathBuf,
    run: PathBuf,
    env_file: PathBuf,
    worlds: PathBuf,
    steamcmd: PathBuf,
    ext: PathBuf,
    ext_game: PathBuf,
    ext_log: PathBuf,
    cache: PathBuf,
    unit_dir: PathBuf,
    /// PCDS_HOME and PCDS_UNIT give a second server on the same machine its own folder and units.
    unit_name: String,
    /// In the container there is no systemd: the server is the container's own process.
    container: bool,
    home: PathBuf,
}

pub fn run(args: &[String]) -> ! {
    let code = Ctx::new().and_then(|ctx| ctx.command(args)).unwrap_or_else(|e| {
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
        let (root, web, extractor_dll) = if dir.starts_with(checkout.join("target")) {
            let root = checkout.parent().ok_or("no checkout folder")?.to_path_buf();
            (root.clone(), checkout.join("web/index.html"), root.join("extractor/prebuilt/PcdsExtractor.dll"))
        } else {
            (dir.clone(), dir.join("web/index.html"), dir.join("PcdsExtractor.dll"))
        };
        let run = crate::env("PCDS_HOME").map(PathBuf::from).unwrap_or_else(|| root.join("run"));
        let run = std::path::absolute(&run).map_err(|e| format!("{}: {e}", run.display()))?;
        let home = PathBuf::from(crate::env("HOME").ok_or("HOME is not set")?);
        let unit_dir = crate::env("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config")).join("systemd/user");
        let ext = run.join("extractor");
        let ext_game = ext.join("game");
        let ctx = Ctx {
            exe,
            web,
            extractor_dll,
            env_file: run.join("server.env"),
            worlds: run.join("worlds"),
            steamcmd: run.join("steamcmd"),
            ext_log: ext_game.join("BepInEx/LogOutput.log"),
            ext_game,
            ext,
            cache: run.join("cache"),
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
                return passthrough(&mut self.systemctl(&[what, &self.unit()]));
            }
            ["status"] => self.status()?,
            ["logs"] => {
                self.not_in_container("logs")?;
                return passthrough(Command::new("journalctl").args(["--user", "-o", "cat", "-u", &self.unit(), "-n", "100", "-f"]));
            }
            ["install-service"] => self.install_service()?,
            ["uninstall-service"] => self.uninstall_service()?,
            ["code"] => self.code()?,
            ["players"] => self.players()?,
            ["save"] => println!("{}", self.api("POST", "/api/save", None)?),
            ["kick", player] => println!("{}", self.api("POST", "/api/kick", Some(&serde_json::json!({"player": player}).to_string()))?),
            ["kick", ..] => return usage("pcds kick PLAYER"),
            ["ban", player] => println!("{}", self.api("POST", "/api/ban", Some(&serde_json::json!({"player": player}).to_string()))?),
            ["ban", ..] => return usage("pcds ban PLAYER"),
            ["unban", value] => println!("{}", self.api("POST", "/api/unban", Some(&serde_json::json!({"value": value}).to_string()))?),
            ["unban", ..] => return usage("pcds unban NAME_OR_IP"),
            ["gui"] => println!("{}", self.gui_url()),
            ["token"] => println!("{}", self.env_get("PCDS_ADMIN_TOKEN").unwrap_or_default()),
            ["config"] => return passthrough(Command::new(crate::env("EDITOR").unwrap_or_else(|| "nano".into())).arg(&self.env_file)),
            ["api", method, path] => println!("{}", self.api(method, path, None)?),
            ["api", method, path, body] => println!("{}", self.api(method, path, Some(body))?),
            ["api", ..] => return usage("pcds api METHOD PATH [JSON]"),
            ["rules", ..] => self.rules(&args[1..])?,
            ["extract", ..] => self.extract(game_option(&a[1..])?.as_deref())?,
            ["update", ..] => self.update(game_option(&a[1..])?.as_deref())?,
            ["selftest"] => self.selftest()?,
            ["run"] => self.run_server()?,
            ["container"] => self.container()?,
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
    let new = to.with_extension("new");
    fs::copy(from, &new).and_then(|_| fs::rename(&new, to)).map(|_| ()).map_err(|e| format!("{} -> {}: {e}", from.display(), to.display()))
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

/// A random token (the admin API's, the extractor API's).
fn token() -> Result<String> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).map_err(|e| format!("/dev/urandom: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
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

fn modified(path: &Path) -> SystemTime {
    fs::metadata(path).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH)
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

/// Whether a file is a save of the game (the bash pcds grepped for this).
fn is_save(path: &Path) -> bool {
    fs::read(path).is_ok_and(|b| b.windows(15).any(|w| w == b"saveDisplayName"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
