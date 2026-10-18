//! Extracting the game tables: the game runs once under Wine with BepInEx and the extractor
//! plugin (extractor/), on a world of its own on 127.0.0.1, and its API hands over the tables,
//! which go to run/data/<game version>/.

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::Value;

use super::env::build_id;
use super::{check, need, quiet, say, sleep, wait_for, Ctx, Result, APP_ID};

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
    use std::os::fd::AsRawFd;
    let file = fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    // SAFETY: flock on a descriptor this function owns; the lock lasts as long as the file.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("another extraction is running".into());
    }
    Ok(file)
}

/// A planet's travel request through a scripted player (PlanetData's stable hash of its id).
fn travel_script(planet: &str) -> String {
    format!(
        r#"{{"name":"Traveller","steps":[{{"rpc":"PlanetNetworkLoader.SwitchToPlanetServerRpc","on":"PlanetNetworkLoader","args":[{},0,0],"after":2000}},{{"wait":5000}}]}}"#,
        crate::data::stable_hash(planet)
    )
}

/// The free planets in the game data, in its order (the ones the server can run).
fn free_planets(gamedata: &Value) -> Vec<String> {
    let assets = gamedata["assets"].as_object().into_iter().flatten();
    assets.filter(|(k, v)| k.starts_with("PlanetData:") && v["steamAppId"] == 0 && v["showInBuild"].as_bool() == Some(true)).filter_map(|(_, v)| v["id"].as_str().map(str::to_string)).collect()
}

/// Someone joined the extractor's game from outside: a player object with an owner.
fn someone_joined(netdata: &Value) -> bool {
    netdata["objects"].as_array().into_iter().flatten().any(|o| o["player"].as_bool() == Some(true) && o["owner"].as_u64().is_some_and(|owner| owner != 0))
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
        super::api::request(EXT_API, token, method, path, if method == "GET" { "" } else { "{}" })
    }

    /// The planet the extractor's game is on.
    fn ext_planet(&self) -> Result<String> {
        let status: Value = serde_json::from_str(&self.ext_api("GET", "/api/status")?).map_err(|e| format!("extractor status: {e}"))?;
        Ok(status["planet"].as_str().unwrap_or_default().to_string())
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
        game.spawn().map_err(|e| format!("wine: {e}"))?;
        RUNNING.store(true, Ordering::SeqCst);
        let running = Extractor(self);
        for _ in 0..600 {
            if self.log_says("PCDS READY") {
                return Ok(running);
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

    /// The scripted player (pcds probe) against the extractor's game, its output to `log`.
    fn probe(&self, tables: &Path, script: &Path, log: Option<&Path>) -> Result<std::process::Child> {
        let mut c = Command::new(&self.exe);
        c.arg("probe").arg(format!("127.0.0.1:{EXT_PORT}")).arg(tables).arg(script).stdin(Stdio::null());
        match log {
            Some(log) => {
                let out = fs::File::create(log).map_err(|e| format!("{}: {e}", log.display()))?;
                c.stderr(out.try_clone().map_err(|e| e.to_string())?).stdout(out);
            }
            None => {
                c.stdout(Stdio::null()).stderr(Stdio::null());
            }
        }
        c.spawn().map_err(|e| format!("{}: {e}", self.exe.display()))
    }

    /// Runs the game once with the extractor and writes run/data/<game version>/.
    pub fn extract(&self, game: Option<&Path>) -> Result {
        let game = self.game_dir(game)?;
        super::mkdir(&self.run)?;
        say(format!("extracting the game tables from {}", game.display()));
        let _lock = lock(&self.run.join(".extract.lock"))?;
        self.prepare_extractor(&game)?;
        let ctx = self.clone();
        let _ = ctrlc::set_handler(move || {
            ctx.stop_extractor();
            std::process::exit(130);
        });
        let running = self.start_extractor()?;
        let status: Value = serde_json::from_str(&self.ext_api("GET", "/api/status")?).map_err(|e| format!("extractor status: {e}"))?;
        let version = status["gameVersion"].as_str().ok_or("the extractor reports no game version")?.to_string();
        // Hidden until complete: tables() only picks finished versions.
        let out = self.run.join(format!("data/.{version}.new"));
        let _ = fs::remove_dir_all(&out);
        super::mkdir(&out.join("scenes"))?;
        let (raw, script) = (self.ext.join("netdata-raw.json"), self.ext.join("probe-script.json"));
        let extract_raw = |quiet: bool| {
            let mut c = Command::new(&self.exe);
            c.arg("extract-raw").arg(&raw).arg(&out);
            if quiet {
                c.stdout(Stdio::null());
            }
            check(&mut c)
        };
        let netdata = || -> Result<String> {
            let text = self.ext_api("GET", "/api/netdata")?;
            super::write(&raw, &text)?;
            Ok(text)
        };
        netdata()?;
        extract_raw(true)?;
        // Again with a scripted player joined (with the tables just written), for the layout a
        // joining player gets.
        super::write(&script, r#"{"name":"Extractor","ready":false,"steps":[{"wait":90000}]}"#)?;
        let probe_log = self.ext.join("probe.log");
        let mut joiner = self.probe(&out, &script, Some(&probe_log))?;
        let mut joined = false;
        for _ in 0..40 {
            sleep(2);
            if serde_json::from_str::<Value>(&netdata()?).is_ok_and(|n| someone_joined(&n)) {
                joined = true;
                break;
            }
        }
        let _ = joiner.kill();
        let _ = joiner.wait();
        if !joined {
            return Err(format!("the scripted player did not join the game; see {}", probe_log.display()));
        }
        extract_raw(false)?;
        let gamedata = self.ext_api("GET", "/api/gamedata")?;
        super::write(&out.join("gamedata.json"), &gamedata)?;
        super::write(&out.join("wrecks.json"), self.ext_api("GET", "/api/wreckdata")?)?;
        let planets = free_planets(&serde_json::from_str(&gamedata).map_err(|e| format!("gamedata.json: {e}"))?);
        drop(gamedata);
        for planet in &planets {
            let mut current = self.ext_planet()?;
            if current != *planet {
                say(format!("travelling to {planet}"));
                super::write(&script, travel_script(planet))?;
                // The game refuses players for a while after the game data dump (it is still
                // removing what it spawned), so travel is retried until the planet changes.
                'travel: for _ in 0..5 {
                    wait_for(&mut self.probe(&out, &script, None)?, Duration::from_secs(60));
                    for _ in 0..12 {
                        sleep(5);
                        current = self.ext_planet().unwrap_or_default();
                        if current == *planet {
                            break 'travel;
                        }
                    }
                }
                if current != *planet {
                    return Err(format!("the game did not reach {planet}"));
                }
                sleep(15);
            }
            super::write(&out.join(format!("scenes/{planet}.json")), self.ext_api("GET", "/api/scenedata")?)?;
        }
        let _ = fs::remove_file(&raw);
        let _ = fs::remove_file(&script);
        drop(running);
        super::write(&out.join("buildid"), format!("{}\n", build_id(&game)?))?;
        let tables = self.run.join(format!("data/{version}"));
        let _ = fs::remove_dir_all(&tables);
        fs::rename(&out, &tables).map_err(|e| format!("{}: {e}", tables.display()))?;
        say(format!("game tables for {version} in {} (planets: {})", tables.display(), planets.join(" ")));
        Ok(())
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
        if self.active() {
            check(&mut self.systemctl(&["restart", &self.unit()]))?;
            say("server restarted on the new tables");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn travel_targets_and_joins() {
        assert_eq!(travel_script("Prime"), r#"{"name":"Traveller","steps":[{"rpc":"PlanetNetworkLoader.SwitchToPlanetServerRpc","on":"PlanetNetworkLoader","args":[-1140328421,0,0],"after":2000},{"wait":5000}]}"#);
        let gamedata = json!({"assets": {
            "PlanetData:PlanetData-Prime": {"id": "Prime", "steamAppId": 0, "showInBuild": true},
            "PlanetData:PlanetData-Humble": {"id": "Humble", "steamAppId": 3142540, "showInBuild": true},
            "PlanetData:PlanetData-Test": {"id": "Test", "steamAppId": 0, "showInBuild": false},
            "GroupDataItem:Iron": {"id": "Iron"},
            "PlanetData:PlanetData-Aqualis": {"id": "Aqualis", "steamAppId": 0, "showInBuild": true},
        }});
        assert_eq!(free_planets(&gamedata), ["Prime", "Aqualis"]);
        assert!(!someone_joined(&json!({"objects": [{"player": true, "owner": 0}, {"player": false, "owner": 3}]})));
        assert!(someone_joined(&json!({"objects": [{"player": true, "owner": 1}]})));
    }
}
