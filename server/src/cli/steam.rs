//! Steam: the game folder (from Steam's library list), the game's own saves, and steamcmd
//! (Valve's command-line client, in run/steamcmd: steamclient.so for invite codes, and the
//! game for a machine without it).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{check, say, Ctx, Result, APP_ID};

/// A Valve KeyValues (.vdf) value: text, or keys with values.
#[derive(Debug, PartialEq)]
pub enum Vdf {
    Text(String),
    Keys(Vec<(String, Vdf)>),
}

impl Vdf {
    fn get(&self, key: &str) -> Option<&Vdf> {
        match self {
            Vdf::Keys(keys) => keys.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v),
            Vdf::Text(_) => None,
        }
    }
}

/// Parses KeyValues text: "key" "value" and "key" { ... }, with // comments.
pub fn parse_vdf(text: &str) -> Result<Vdf> {
    fn token(chars: &mut std::iter::Peekable<std::str::Chars>) -> Result<Option<String>> {
        loop {
            match chars.next() {
                None => return Ok(None),
                Some(c) if c.is_whitespace() => {}
                Some('/') if chars.peek() == Some(&'/') => {
                    chars.by_ref().find(|c| *c == '\n');
                }
                Some(c @ ('{' | '}')) => return Ok(Some(c.to_string())),
                Some('"') => {
                    let mut s = String::from("\"");
                    loop {
                        match chars.next().ok_or("unterminated string")? {
                            '"' => return Ok(Some(s)),
                            '\\' => s.push(match chars.next().ok_or("unterminated string")? {
                                'n' => '\n',
                                't' => '\t',
                                c => c,
                            }),
                            c => s.push(c),
                        }
                    }
                }
                Some(c) => return Err(format!("unexpected {c:?}")),
            }
        }
    }
    /// The deepest nesting read: Steam's files go four levels down. A longer one is a file that
    /// is no Steam's, and each level is a call of this function (a file of `"a"{` repeated would
    /// end the process with a stack overflow).
    const DEPTH: usize = 32;
    fn keys(chars: &mut std::iter::Peekable<std::str::Chars>, nested: bool, depth: usize) -> Result<Vdf> {
        if depth > DEPTH {
            return Err(format!("keys nested over {DEPTH} levels deep"));
        }
        let mut out = Vec::new();
        loop {
            let key = match token(chars)? {
                None if !nested => return Ok(Vdf::Keys(out)),
                Some(t) if t == "}" && nested => return Ok(Vdf::Keys(out)),
                Some(t) if t.starts_with('"') => t[1..].to_string(),
                other => return Err(format!("expected a key, not {other:?}")),
            };
            let value = match token(chars)?.as_deref() {
                Some("{") => keys(chars, true, depth + 1)?,
                Some(t) if t.starts_with('"') => Vdf::Text(t[1..].to_string()),
                other => return Err(format!("{key}: expected a value, not {other:?}")),
            };
            out.push((key, value));
        }
    }
    keys(&mut text.chars().peekable(), false, 0)
}

/// The libraries in libraryfolders.vdf: each one's folder and whether its "apps" lists the game.
pub fn libraries(vdf: &Vdf) -> Vec<(PathBuf, bool)> {
    let Some(Vdf::Keys(folders)) = vdf.get("libraryfolders") else { return Vec::new() };
    folders
        .iter()
        .filter_map(|(_, f)| match f.get("path")? {
            Vdf::Text(path) => Some((PathBuf::from(path), f.get("apps").is_some_and(|apps| apps.get(APP_ID).is_some()))),
            Vdf::Keys(_) => None,
        })
        .collect()
}

impl Ctx {
    /// Steam's library lists: a native Steam's (~/.steam/steam is Steam's own link to its
    /// folder, which is not always ~/.local/share/Steam) and the Flatpak's.
    fn library_lists(&self) -> Vec<PathBuf> {
        let roots = [".local/share/Steam", ".steam/steam", ".var/app/com.valvesoftware.Steam/.local/share/Steam"];
        roots.iter().map(|root| self.home.join(root).join("steamapps/libraryfolders.vdf")).collect()
    }

    /// The libraries in the library lists there are: each one's folder, whether it has the game,
    /// and the list that says so.
    /// The libraries in the library lists there are, with the lists that could not be read
    /// (named in the error when no library has the game).
    fn steam_libraries(&self) -> (Vec<(PathBuf, bool, PathBuf)>, Vec<String>) {
        let (mut out, mut broken) = (Vec::new(), Vec::new());
        for list in self.library_lists() {
            let text = match fs::read_to_string(&list) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                text => text.map_err(|e| format!("{}: {e}", list.display())),
            };
            match text.and_then(|t| parse_vdf(&t).map_err(|e| format!("{}: {e}", list.display()))) {
                Ok(vdf) => out.extend(libraries(&vdf).into_iter().map(|(folder, has)| (folder, has, list.clone()))),
                Err(e) => broken.push(e),
            }
        }
        (out, broken)
    }

    /// The game's folder in a library: its app manifest's installdir.
    fn installed_in(library: &Path) -> Result<PathBuf> {
        let manifest = library.join(format!("steamapps/appmanifest_{APP_ID}.acf"));
        let text = fs::read_to_string(&manifest).map_err(|e| format!("{}: {e}", manifest.display()))?;
        let vdf = parse_vdf(&text).map_err(|e| format!("{}: {e}", manifest.display()))?;
        let dir = match vdf.get("AppState") {
            Some(Vdf::Keys(state)) => state.iter().find_map(|(k, v)| match v {
                Vdf::Text(dir) if k.eq_ignore_ascii_case("installdir") => Some(dir.clone()),
                _ => None,
            }),
            _ => None,
        };
        Ok(library.join("steamapps/common").join(dir.ok_or(format!("{}: no installdir", manifest.display()))?))
    }

    /// The game folder: --game DIR, PCDS_GAME_DIR, the one used last (run/game-dir), else the
    /// Steam library that lists the game. The one chosen is remembered.
    pub fn game_dir(&self, arg: Option<&Path>) -> Result<PathBuf> {
        self.find_game_dir(arg, crate::env("PCDS_GAME_DIR"), true)
    }

    /// game_dir(None) that remembers nothing (`pcds doctor` only looks).
    pub fn game_dir_peek(&self) -> Result<PathBuf> {
        self.find_game_dir(None, crate::env("PCDS_GAME_DIR"), false)
    }

    /// game_dir with PCDS_GAME_DIR given (tests give their own, whatever the process has).
    fn find_game_dir(&self, arg: Option<&Path>, env_dir: Option<String>, remember: bool) -> Result<PathBuf> {
        let remembered = self.run.join("game-dir");
        let (dir, from) = if let Some(dir) = arg {
            (dir.to_path_buf(), "--game".to_string())
        } else if let Some(dir) = env_dir {
            (PathBuf::from(dir), "PCDS_GAME_DIR".to_string())
        } else if let Ok(text) = fs::read_to_string(&remembered) {
            (PathBuf::from(text.trim_end_matches('\n')), remembered.display().to_string())
        } else {
            let not_found = |why: String| format!("game not found: pass --game DIR (the folder with 'Planet Crafter.exe', e.g. copied from a PC) or set PCDS_GAME_DIR ({why})");
            let lists = || self.library_lists().iter().map(|l| l.display().to_string()).collect::<Vec<_>>().join(", ");
            let (libraries, broken) = self.steam_libraries();
            // Every library that lists the game, until one has its files.
            let mut tried = broken;
            let found = libraries.into_iter().filter(|(_, has, _)| *has).find_map(|(folder, _, list)| match Self::installed_in(&folder) {
                Ok(dir) if dir.join("Planet Crafter.exe").is_file() => Some((dir, list)),
                Ok(dir) => {
                    tried.push(format!("{} has no 'Planet Crafter.exe'", dir.display()));
                    None
                }
                Err(e) => {
                    tried.push(e);
                    None
                }
            });
            let why = if tried.is_empty() { format!("no Steam library list ({}) lists app {APP_ID}", lists()) } else { tried.join("; ") };
            let (dir, list) = found.ok_or_else(|| not_found(why))?;
            (dir, list.display().to_string())
        };
        if !dir.join("Planet Crafter.exe").is_file() || !dir.join("Planet Crafter_Data").is_dir() {
            return Err(format!("no game in {} (from {from}): the game folder holds 'Planet Crafter.exe' and 'Planet Crafter_Data'", dir.display()));
        }
        let dir = dir.canonicalize().map_err(|e| format!("{}: {e}", dir.display()))?;
        if remember && fs::read_to_string(&remembered).ok().as_deref() != Some(&format!("{}\n", dir.display())) {
            super::mkdir(&self.run)?;
            super::write(&remembered, format!("{}\n", dir.display()))?;
        }
        Ok(dir)
    }

    /// The game's own saves on this machine (Proton), newest first.
    pub fn local_saves(&self) -> Vec<PathBuf> {
        // Each library once: ~/.steam/steam's list is usually ~/.local/share/Steam's.
        let mut libraries: Vec<PathBuf> = self.steam_libraries().0.into_iter().filter_map(|(folder, ..)| folder.canonicalize().ok()).collect();
        libraries.sort();
        libraries.dedup();
        let mut saves: Vec<PathBuf> = libraries
            .into_iter()
            .map(|lib| lib.join(format!("steamapps/compatdata/{APP_ID}/pfx/drive_c/users/steamuser/AppData/LocalLow/MijuGames/Planet Crafter")))
            .flat_map(|dir| fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "json") && super::is_save(p))
            .collect();
        saves.sort_by_key(|p| std::cmp::Reverse(super::modified(p)));
        saves.truncate(9);
        saves
    }

    fn steamcmd_sh(&self) -> PathBuf {
        self.steamcmd.join("steamcmd.sh")
    }

    pub fn fetch_steamcmd(&self) -> Result {
        if self.steamcmd_sh().is_file() {
            return Ok(());
        }
        say("downloading steamcmd (Valve's command-line Steam client)");
        super::mkdir(&self.steamcmd)?;
        let tarball = self.steamcmd.join("steamcmd_linux.tar.gz");
        super::download("https://steamcdn-a.akamaihd.net/client/installer/steamcmd_linux.tar.gz", &tarball)?;
        check(Command::new("tar").arg("-xzf").arg(&tarball).arg("-C").arg(&self.steamcmd))?;
        let _ = fs::remove_file(&tarball);
        Ok(())
    }

    /// Invite codes need Valve's steamclient.so where the Steam API looks for it,
    /// ~/.steam/sdk64: the Steam client puts it there, else steamcmd's copy is linked there.
    pub fn ensure_steamclient(&self) {
        let sdk = self.home.join(".steam/sdk64/steamclient.so");
        if self.env_get("PCDS_STEAM").as_deref() == Some("off") || sdk.is_file() {
            return;
        }
        say("getting steamclient.so for invite codes (steamcmd's first run updates itself; a minute or two)");
        let log = self.steamcmd.join("first-run.log");
        if let Err(e) = self.fetch_steamcmd().and_then(|_| {
            let out = fs::File::create(&log).map_err(|e| e.to_string())?;
            let err = out.try_clone().map_err(|e| e.to_string())?;
            check(Command::new(self.steamcmd_sh()).arg("+quit").stdin(Stdio::null()).stdout(out).stderr(err))
        }) {
            say(format!("steamcmd: {e}"));
        }
        let theirs = self.steamcmd.join("linux64/steamclient.so");
        if theirs.is_file() {
            let _ = fs::create_dir_all(sdk.parent().unwrap_or(&sdk));
            let _ = fs::remove_file(&sdk);
            let _ = std::os::unix::fs::symlink(&theirs, &sdk);
        }
        if !sdk.is_file() {
            eprintln!(
                "pcds: no steamclient.so: steamcmd did not run (see {}; it needs 32-bit libraries such as lib32gcc-s1). Direct IP works; invite codes come once steamcmd runs",
                log.display()
            );
        }
    }

    /// The game (Windows build) with the owner's Steam account; steamcmd asks for the password
    /// and the Steam Guard code once and remembers the login for updates.
    pub fn download_game(&self, user: &str) -> Result {
        self.fetch_steamcmd()?;
        say(format!("downloading The Planet Crafter as Steam user {user} (steamcmd asks for your password and Steam Guard code)"));
        let game = self.run.join("game");
        let done = Command::new(self.steamcmd_sh())
            .args(["+@sSteamCmdForcePlatformType", "windows", "+force_install_dir"])
            .arg(&game)
            .args(["+login", user, "+app_update", APP_ID, "validate", "+quit"])
            .status()
            .is_ok_and(|s| s.success());
        if !done || !game.join("Planet Crafter.exe").is_file() {
            return Err("the game download did not finish (see above)".into());
        }
        super::write(&self.run.join("steam-user"), format!("{user}\n"))?;
        self.game_dir(Some(&game)).map(|_| ())
    }

    /// A downloaded game follows updates with the remembered login (no prompt: when Steam wants
    /// the password again it fails). Err when steamcmd fails, so the update check's unit shows
    /// it failed.
    pub fn update_downloaded_game(&self) -> Result {
        let Ok(user) = fs::read_to_string(self.run.join("steam-user")) else { return Ok(()) };
        if !self.steamcmd_sh().is_file() {
            return Err(format!("the game was downloaded with steamcmd, which is gone from {}: pcds setup --steam-user {} gets it again", self.steamcmd.display(), user.trim()));
        }
        let log = self.steamcmd.join("update.log");
        let ran = fs::File::create(&log).and_then(|out| {
            let err = out.try_clone()?;
            Command::new(self.steamcmd_sh())
                .args(["+@sSteamCmdForcePlatformType", "windows", "+@NoPromptForPassword", "1", "+force_install_dir"])
                .arg(self.run.join("game"))
                .args(["+login", user.trim(), "+app_update", APP_ID, "+quit"])
                .stdin(Stdio::null())
                .stdout(out)
                .stderr(err)
                .status()
        });
        match ran {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => Err(format!("the game update through steamcmd failed ({s}; see {}). If Steam wants the password again: pcds setup --steam-user {}", log.display(), user.trim())),
            Err(e) => Err(format!("the game update through steamcmd did not run: {e} ({})", log.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_library_that_lists_the_game() {
        let text = r#"
"libraryfolders"
{
	"0"
	{
		"path"		"/home/me/.local/share/Steam"
		"label"		""
		"apps"
		{
			"228980"		"128606066"
		}
	}
	// a second drive
	"1"
	{
		"path"		"/mnt/games/Steam \"Library\""
		"apps"
		{
			"1284190"		"5341207582"
			"294100"		"577359346"
		}
	}
}
"#;
        let vdf = parse_vdf(text).unwrap();
        assert_eq!(libraries(&vdf), vec![(PathBuf::from("/home/me/.local/share/Steam"), false), (PathBuf::from("/mnt/games/Steam \"Library\""), true)]);
        assert_eq!(libraries(&parse_vdf("\"other\" { }").unwrap()), vec![]);
        for bad in ["\"libraryfolders\" {", "\"a\" \"b", "\"a\" }", "x"] {
            assert!(parse_vdf(bad).is_err(), "{bad}");
        }
    }

    /// A file of braces opened and never closed is an error, not a stack overflow that ends the
    /// process (a megabyte of `"a" {` is 250,000 levels of recursion); Steam's own go four deep.
    #[test]
    fn a_file_nested_without_end_is_refused_not_recursed_into() {
        let deep = "\"a\" {".repeat(250_000);
        assert!(parse_vdf(&deep).unwrap_err().contains("nested over"));
        let ok = format!("{}{}", "\"a\" {".repeat(DEPTH_OK), "}".repeat(DEPTH_OK));
        assert!(parse_vdf(&ok).is_ok(), "{DEPTH_OK} levels are fine");
        const DEPTH_OK: usize = 30;
    }

    #[test]
    fn every_steam_root_is_looked_in() {
        let dir = super::super::test_dir("steam-roots");
        let ctx = super::super::test_ctx(&dir);
        let list = |root: &Path, apps: &str| {
            fs::create_dir_all(root.join("steamapps")).unwrap();
            let text = format!("\"libraryfolders\" {{ \"0\" {{ \"path\" \"{}\" \"apps\" {{ \"{apps}\" \"1\" }} }} }}", root.display());
            fs::write(root.join("steamapps/libraryfolders.vdf"), text).unwrap();
        };
        let save_in = |root: &Path, name: &str| {
            let saves = root.join(format!("steamapps/compatdata/{APP_ID}/pfx/drive_c/users/steamuser/AppData/LocalLow/MijuGames/Planet Crafter"));
            fs::create_dir_all(&saves).unwrap();
            fs::write(saves.join(name), "{\"saveDisplayName\":\"x\"}").unwrap();
        };
        assert!(ctx.find_game_dir(None, None, true).unwrap_err().contains(".var/app/com.valvesoftware.Steam"), "no list anywhere: all three named");

        // A native Steam without the game, also reached through ~/.steam/steam; the game in the Flatpak's.
        let native = dir.join("home/.local/share/Steam");
        list(&native, "228980");
        fs::create_dir_all(dir.join("home/.steam")).unwrap();
        std::os::unix::fs::symlink(&native, dir.join("home/.steam/steam")).unwrap();
        save_in(&native, "Native-1.json");
        let flatpak = dir.join("home/.var/app/com.valvesoftware.Steam/.local/share/Steam");
        list(&flatpak, APP_ID);
        save_in(&flatpak, "Flatpak-1.json");
        // Its manifest names the folder (installdir); the native one lists the game too, but
        // its folder lacks the files: the next library is tried.
        let manifest = |root: &Path| fs::write(root.join(format!("steamapps/appmanifest_{APP_ID}.acf")), "\"AppState\" { \"appid\" \"1284190\" \"installdir\" \"The Planet Crafter\" }").unwrap();
        manifest(&flatpak);
        let game = flatpak.join("steamapps/common/The Planet Crafter");
        fs::create_dir_all(game.join("Planet Crafter_Data")).unwrap();
        fs::write(game.join("Planet Crafter.exe"), "").unwrap();
        list(&native, &format!("228980\" \"1\" \"{APP_ID}"));
        manifest(&native);

        assert_eq!(ctx.find_game_dir(None, None, true).unwrap(), game.canonicalize().unwrap());
        let mut names: Vec<String> = ctx.local_saves().iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["Flatpak-1.json", "Native-1.json"], "each save once");
        fs::remove_dir_all(&dir).unwrap();
    }
}
