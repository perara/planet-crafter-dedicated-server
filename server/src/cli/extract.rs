//! Extracting the game tables into run/data/<game version>/: read from the game's files
//! (native/); only the wreck layouts come from the game running once under Wine with BepInEx
//! and the extractor plugin (extractor/), on 127.0.0.1.

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};


use super::env::build_id;
use super::{check, need, quiet, say, sleep, Ctx, Result, APP_ID};

const BEPINEX_VERSION: &str = "5.4.23.5";
const BEPINEX_SHA256: &str = "82f9878551030f54657792c0740d9d51a09500eeae1fba21106b0c441e6732c4";
/// The extractor's game (direct IP) and its API, both on 127.0.0.1.
const EXT_PORT: u16 = 7791;
const EXT_API: u16 = 7781;

/// The game runs: an interrupted extraction (Ctrl-C, a stopped unit) still stops it.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// The started game; stopped when this goes, however the extraction ends.
struct Extractor<'a>(&'a Ctx);

impl Drop for Extractor<'_> {
    fn drop(&mut self) {
        self.0.stop_extractor();
    }
}

/// One extraction at a time (the hourly update and a manual one would share the game).
fn lock(path: &Path) -> Result<fs::File> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let mut file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    // SAFETY: flock on a descriptor this function owns; the lock lasts as long as the file.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let holder = fs::read_to_string(path).unwrap_or_default();
        return Err(format!("another extraction is running (process {}); stop it or wait for it", holder.trim()));
    }
    // The holder, for the message above.
    let _ = file.set_len(0).and_then(|_| write!(file, "{}", std::process::id()));
    Ok(file)
}

/// The tables being made: removed unless they were finished (moved into place).
struct Unfinished(PathBuf);

impl Drop for Unfinished {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

impl Ctx {
    fn wine(&self, program: &str) -> Command {
        let mut c = Command::new(program);
        c.env("WINEPREFIX", self.ext.join("prefix")).env("WINEDEBUG", "-all").env("WINEDLLOVERRIDES", "winhttp=n,b");
        c
    }

    fn fetch_bepinex(&self) -> Result<PathBuf> {
        let zip = self.cache.join(format!("BepInEx_win_x64_{BEPINEX_VERSION}.zip"));
        if !zip.is_file() {
            super::mkdir(&self.cache)?;
            say(format!("downloading BepInEx {BEPINEX_VERSION} (the mod loader the extractor runs in)"));
            super::download(&format!("https://github.com/BepInEx/BepInEx/releases/download/v{BEPINEX_VERSION}/BepInEx_win_x64_{BEPINEX_VERSION}.zip"), &zip)?;
        }
        let bytes = fs::read(&zip).map_err(|e| format!("{}: {e}", zip.display()))?;
        if super::sha256::hex(&super::sha256::sha256(&bytes)) != BEPINEX_SHA256 {
            let _ = fs::remove_file(&zip);
            return Err("BepInEx download corrupt; run again".into());
        }
        Ok(zip)
    }

    /// A view of the game with BepInEx and the extractor installed: the program files are
    /// copied (Wine finds the game's folder from them), everything else is linked, so nothing
    /// is written into your game.
    fn prepare_extractor(&self, game: &Path) -> Result {
        let zip = self.fetch_bepinex()?;
        let new = self.ext_game.with_extension("new");
        let _ = fs::remove_dir_all(&new);
        super::mkdir(&new)?;
        // A leftover game from an interrupted extraction would hold the folder and the ports.
        quiet(self.wine("wineserver").arg("-k"));
        let entries = fs::read_dir(game).map_err(|e| format!("{}: {e}", game.display()))?;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let (from, to) = (entry.path(), new.join(&name));
            // Hidden files, and the game's own mod loader if it has one, stay out: the extractor
            // brings its own, and nothing is written through a link into the game folder.
            if name.starts_with('.') || ["BepInEx", "doorstop_config.ini", "winhttp.dll", "steam_appid.txt"].contains(&name.as_str()) {
                continue;
            }
            let copied = if name.ends_with(".exe") || name.ends_with(".dll") { fs::copy(&from, &to).map(|_| ()) } else { std::os::unix::fs::symlink(&from, &to) };
            copied.map_err(|e| format!("{}: {e}", to.display()))?;
        }
        need("unzip", "install it with your package manager")?;
        check(Command::new("unzip").arg("-qo").arg(&zip).current_dir(&new))?;
        // The game then starts without asking for the Steam client.
        super::write(&new.join("steam_appid.txt"), format!("{APP_ID}\n"))?;
        super::mkdir(&new.join("BepInEx/plugins"))?;
        super::mkdir(&new.join("BepInEx/config"))?;
        let config = format!("[Extractor]\r\nSaveName = Extract-1\r\nPort = {EXT_PORT}\r\nApiPort = {EXT_API}\r\nApiToken = {}\r\n", super::token()?);
        super::write(&new.join("BepInEx/config/pcds.extractor.cfg"), config)?;
        let _ = fs::remove_dir_all(&self.ext_game);
        fs::rename(&new, &self.ext_game).map_err(|e| format!("{}: {e}", self.ext_game.display()))?;
        let plugin = self.ext_game.join("BepInEx/plugins/PcdsExtractor.dll");
        fs::copy(&self.extractor_dll, &plugin).map_err(|e| format!("{} -> {}: {e}", self.extractor_dll.display(), plugin.display()))?;
        Ok(())
    }

    /// The extractor's API, with the token of its configuration.
    fn ext_api(&self, method: &str, path: &str) -> Result<String> {
        let config = self.ext_game.join("BepInEx/config/pcds.extractor.cfg");
        let text = fs::read_to_string(&config).map_err(|e| format!("{}: {e}", config.display()))?;
        let token = text.lines().find_map(|l| l.strip_prefix("ApiToken = ")).unwrap_or_default().trim();
        // Laying out every wreck takes the game minutes; stopping it, seconds.
        let timeout = if path == "/api/wreckdata" { 30 * 60 } else { 30 };
        super::api::request(EXT_API, token, method, path, if method == "GET" { "" } else { "{}" }, Some(std::time::Duration::from_secs(timeout)))
    }

    fn log_says(&self, what: &str) -> bool {
        fs::read(&self.ext_log).is_ok_and(|b| b.windows(what.len()).any(|w| w == what.as_bytes()))
    }

    fn start_extractor(&self) -> Result<Extractor<'_>> {
        need("wine", "to run the game once for the extraction (e.g. 'apt install wine', 'dnf install wine', 'pacman -S wine')")?;
        if !self.ext.join("prefix/drive_c").is_dir() {
            say("creating the Wine prefix (first run only)");
            quiet(self.wine("wineboot").arg("-i"));
        }
        // No sound device: the game would otherwise play its music on this machine.
        quiet(self.wine("wine").args(["reg", "add", r"HKCU\Software\Wine\Drivers", "/v", "Audio", "/d", "", "/f"]));
        // Those started Wine's session without a display: the game would join it and could not
        // make its window under Xvfb. It starts its own.
        quiet(self.wine("wineserver").arg("-k"));
        quiet(self.wine("wineserver").arg("-w"));
        let mut game = if crate::env("DISPLAY").is_none() && crate::env("WAYLAND_DISPLAY").is_none() {
            need("xvfb-run", "no display: install Xvfb (xvfb-run; package xvfb or xorg-x11-server-Xvfb) for the extraction")?;
            // A 24-bit screen: Unity cannot make its window on xvfb-run's default 8-bit one.
            let mut c = self.wine("xvfb-run");
            c.args(["-a", "-s", "-screen 0 1280x720x24", "wine"]);
            c
        } else {
            self.wine("wine")
        };
        let _ = fs::remove_file(&self.ext_log);
        let log = fs::File::create(self.ext.join("wine.log")).map_err(|e| format!("{}: {e}", self.ext.display()))?;
        let err = log.try_clone().map_err(|e| e.to_string())?;
        let player_log = format!("Z:{}\\player.log", self.ext.display().to_string().replace('/', "\\"));
        game.args(["Planet Crafter.exe", "-batchmode", "-nographics", "-logFile", &player_log]).current_dir(&self.ext_game).stdin(Stdio::null()).stdout(log).stderr(err);
        // Its own session (setsid): the game outlives a closed terminal until it is stopped.
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            game.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = game.spawn().map_err(|e| format!("wine: {e}"))?;
        RUNNING.store(true, Ordering::SeqCst);
        let running = Extractor(self);
        for _ in 0..600 {
            if self.log_says("PCDS READY") {
                return Ok(running);
            }
            if let Ok(Some(status)) = child.try_wait() {
                drop(running);
                return Err(format!("the game exited ({status}) before it was ready; see {} and {}", self.ext_log.display(), self.ext.join("wine.log").display()));
            }
            sleep(1);
        }
        drop(running);
        Err(format!("the game did not start; see {} and {}", self.ext_log.display(), self.ext.join("wine.log").display()))
    }

    fn stop_extractor(&self) {
        if !RUNNING.swap(false, Ordering::SeqCst) {
            return;
        }
        let _ = self.ext_api("POST", "/api/stop");
        for _ in 0..30 {
            if self.log_says("PCDS STOPPED") {
                break;
            }
            sleep(1);
        }
        sleep(2);
        quiet(self.wine("wineserver").arg("-k"));
    }

    /// The game's tables into run/data/<game version>/: read from its files (native/), all but
    /// the wreck interiors, which the game lays out itself (Tessera) when it runs: for those it
    /// runs once under Wine with the extractor plugin.
    pub fn extract(&self, game: Option<&Path>) -> Result {
        let game = self.game_dir(game)?;
        super::mkdir(&self.run.join("data"))?;
        let _lock = lock(&self.run.join(".extract.lock"))?;
        // Earlier extractions that failed (none runs: the lock is ours).
        for e in fs::read_dir(self.run.join("data")).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().starts_with(".new-") {
                let _ = fs::remove_dir_all(e.path());
            }
        }
        // Hidden until complete: tables() only picks finished versions.
        let out = Unfinished(self.run.join(format!("data/.new-{}", std::process::id())));
        say(format!("reading the game tables from {}", game.display()));
        let version = crate::native::tables::read_game(&game, &out.0)?;
        super::write(&out.0.join("wrecks.json"), self.wreck_layouts(&game)?)?;
        let tables = self.run.join(format!("data/{version}"));
        let _ = fs::remove_dir_all(&tables);
        fs::rename(&out.0, &tables).map_err(|e| format!("{}: {e}", tables.display()))?;
        say(format!("game tables for {version} in {}", tables.display()));
        Ok(())
    }

    /// wrecks.json: the game's own wreck layouts (GET /api/wreckdata), from one run under Wine.
    fn wreck_layouts(&self, game: &Path) -> Result<String> {
        say("laying out the wreck interiors: the game runs once under Wine");
        self.prepare_extractor(game)?;
        let ctx = self.clone();
        let out = self.run.join(format!("data/.new-{}", std::process::id()));
        let _ = ctrlc::set_handler(move || {
            ctx.stop_extractor();
            let _ = fs::remove_dir_all(&out);
            std::process::exit(130);
        });
        let running = self.start_extractor()?;
        let wrecks = self.ext_api("GET", "/api/wreckdata")?;
        drop(running);
        Ok(wrecks)
    }

    /// After a game update: the new build's tables, and the server restarted on them.
    pub fn update(&self, game: Option<&Path>) -> Result {
        self.update_downloaded_game();
        let game = self.game_dir(game)?;
        let build = build_id(&game)?;
        if self.tables(&build).is_ok() {
            say(format!("up to date (game build {build})"));
            return Ok(());
        }
        say(format!("game build {build} has no tables yet"));
        self.extract(Some(&game))?;
        self.restart_after_update()
    }
}
