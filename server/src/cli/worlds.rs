//! The worlds in run/worlds/: one save per world; PCDS_WORLD names the one the server runs.

use std::fs;
use std::path::Path;
use std::process::Command;

use super::{say, Ctx, Result};

/// --force out of a world's options, the rest for `pcds new-save`.
fn force_and_options(args: &[String]) -> (bool, Vec<String>) {
    let force = args.iter().any(|a| a == "--force");
    (force, args.iter().filter(|a| *a != "--force").cloned().collect())
}

impl Ctx {
    /// A world's file goes to worlds/backups/ before something replaces or removes it.
    pub fn keep_copy(&self, name: &str, why: &str) -> Result {
        let backups = self.worlds.join(crate::worlds::BACKUPS);
        super::mkdir(&backups)?;
        let to = super::free_path(&backups, &crate::worlds::kept_stem(name, why, std::time::SystemTime::now()));
        let from = self.worlds.join(format!("{name}.json"));
        let bytes = fs::read(&from).map_err(|e| format!("{}: {e}", from.display()))?;
        super::replace_file(&to, &bytes, 0o644)?;
        let modified = fs::metadata(&from).and_then(|m| m.modified()).map_err(|e| e.to_string())?;
        fs::File::options().write(true).open(&to).and_then(|f| f.set_modified(modified)).map_err(|e| format!("{}: {e}", to.display()))
    }

    /// A new world at a path: made beside, then moved over, so a failed creation leaves what
    /// was there as it was.
    pub fn new_world(&self, path: &Path, options: &[String]) -> Result {
        let data = self.data_dir()?;
        let beside = self.run.join(".new");
        super::mkdir(&self.worlds)?;
        super::mkdir(&beside)?;
        // Named as the world: the save's display name in the game's list is its file's name.
        let made = beside.join(path.file_name().ok_or("no world name")?);
        let _ = fs::remove_file(&made);
        let out = Command::new(self.program()).arg("new-save").arg(&made).arg(&data).args(options).output().map_err(|e| format!("pcds new-save: {e}"))?;
        if !out.status.success() {
            // new-save says what is wrong on its first line; its usage line is for the tools.
            let said = String::from_utf8_lossy(&out.stderr);
            return Err(said.lines().find(|l| !l.starts_with("usage:")).unwrap_or("pcds new-save failed").to_string());
        }
        // What it made is told by the file's final place, not the one beside it.
        print!("{}", String::from_utf8_lossy(&out.stdout).replace(&*made.to_string_lossy(), &path.to_string_lossy()));
        fs::rename(&made, path).map_err(|e| format!("{}: {e}", path.display()))?;
        let _ = fs::remove_dir(&beside);
        Ok(())
    }

    /// `pcds new`: the current world replaced by a new one (the old one kept in backups/).
    pub fn new_world_here(&self, args: &[String]) -> Result {
        let (force, options) = force_and_options(args);
        let world = self.world_file();
        if world.is_file() {
            if !force {
                return Err(format!("a world exists at {}; pass --force to replace it (it is kept in backups/), or make another: pcds world new NAME", world.display()));
            }
            if self.active()? {
                return Err("stop the server first (pcds stop)".into());
            }
            self.keep_copy(&self.world_name(), "replaced")?;
        }
        self.new_world(&world, &options)
    }

    /// The game version of the tables, "" when there are none yet (an imported old save says which
    /// game it was saved by).
    fn game_version(&self) -> String {
        let text = self.data_dir().ok().and_then(|d| fs::read_to_string(d.join("netdata.json")).ok());
        text.and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()).and_then(|v| v["gameVersion"].as_str().map(str::to_string)).unwrap_or_default()
    }

    pub fn worlds(&self) -> Result {
        let current = self.world_name();
        let mut files = crate::worlds::list(&self.worlds, &|_| true);
        if files.is_empty() {
            say(format!("no worlds in {} yet: 'pcds world new NAME', 'pcds world import SAVE.json', or copy a save there", self.worlds.display()));
            return Ok(());
        }
        // By file name, as they were always listed (a world "x-2" before "x").
        files.sort_by_key(|e| format!("{}.json", e.name));
        for e in files {
            let mark = if e.name == current { '*' } else { ' ' };
            println!("{mark} {:<28} {:>7}  {}", e.name, super::human_size(e.bytes), super::local_time(e.modified.unwrap_or(std::time::UNIX_EPOCH)));
        }
        println!("(* the server runs it; add a world by copying a save into {})", self.worlds.display());
        Ok(())
    }

    pub fn world(&self, args: &[String]) -> Result {
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        let file = |name: &str| self.worlds.join(format!("{name}.json"));
        match a.as_slice() {
            ["use", name] => {
                let name = crate::worlds::name_arg(name)?;
                if !file(&name).is_file() {
                    return Err(format!("no world {name} in {} ('pcds worlds' lists them)", self.worlds.display()));
                }
                super::check_world(&file(&name))?;
                if crate::env("PCDS_WORLD").is_some_and(|w| w != name) {
                    return Err("PCDS_WORLD is set in this environment (the container's): change it there".into());
                }
                self.write_env()?;
                self.set_env("PCDS_WORLD", &name)?;
                say(format!("the server runs {name} from now on"));
                self.restart_if_running()
            }
            ["new", name, ..] => {
                let name = crate::worlds::name_arg(name)?;
                let (force, options) = force_and_options(&args[2..]);
                if file(&name).is_file() {
                    if !force {
                        return Err(format!("world {name} exists (--force replaces it; the old one is kept in backups/)"));
                    }
                    if name == self.world_name() && self.active()? {
                        return Err(format!("stop the server first: it runs {name}"));
                    }
                    self.keep_copy(&name, "replaced")?;
                }
                self.new_world(&file(&name), &options)?;
                say(format!("world {name} created; 'pcds world use {name}' plays it"));
                Ok(())
            }
            ["import", save] | ["import", save, _] => {
                let save = Path::new(save);
                if !save.is_file() {
                    return Err(format!("pcds world import SAVE.json [NAME]: no save at '{}'", save.display()));
                }
                let name = crate::worlds::arg(a.get(2).copied().unwrap_or(&save.to_string_lossy()))?;
                if file(&name).exists() {
                    return Err(format!("world {name} exists: pass another NAME"));
                }
                // A save of the current layout is copied as it is; one of the game's old layouts is
                // converted as the game converts it when it opens one.
                let text = fs::read_to_string(save).map_err(|e| format!("{}: {e}", save.display()))?;
                let seed = crate::game::Rng::seeded().range(0, i32::MAX) as u32;
                let converted = crate::oldsave::import(&text, seed, &self.game_version()).map_err(|e| format!("{}: {e}", save.display()))?;
                super::mkdir(&self.worlds)?;
                super::replace_file(&file(&name), converted.as_bytes(), 0o644)?;
                let how = if converted == text { "" } else { " (converted from the game's old save format)" };
                say(format!("world {name} imported{how}; 'pcds world use {name}' plays it"));
                Ok(())
            }
            ["remove", name] => {
                let name = crate::worlds::name_arg(name)?;
                if !file(&name).is_file() {
                    return Err(format!("no world {name} in {}", self.worlds.display()));
                }
                if name == self.world_name() {
                    return Err(format!("the server runs {name}: 'pcds world use' another one first"));
                }
                // Moved into backups/ rather than deleted.
                let backups = self.worlds.join(crate::worlds::BACKUPS);
                super::mkdir(&backups)?;
                let to = super::free_path(&backups, &crate::worlds::kept_stem(&name, "removed", std::time::SystemTime::now()));
                fs::rename(file(&name), &to).map_err(|e| format!("{}: {e}", to.display()))?;
                say(format!("world {name} removed (it is kept in {})", backups.display()));
                Ok(())
            }
            _ => Err("pcds world use NAME | new NAME [OPTIONS] | import SAVE.json [NAME] | remove NAME".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::save::SaveFile;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// A world this server cannot read (the game's old layout, a file cut short, no save at all)
    /// is neither switched to, imported nor made the world by `setup --world`; the error names
    /// the file and, for an old save, `pcds world import`.
    #[test]
    fn a_world_the_server_cannot_read_is_refused_by_use_import_and_setup() {
        let dir = super::super::test_dir("refuse-worlds");
        let ctx = super::super::test_ctx(&dir);
        fs::create_dir_all(&ctx.worlds).unwrap();
        let good = SaveFile::tiny_text("good");
        let cut = &good[..good.len() - 30];
        for (name, text) in [("cut", cut.to_string()), ("garbage", "not a save".to_string()), ("empty", String::new())] {
            fs::write(ctx.worlds.join(format!("{name}.json")), &text).unwrap();
            let e = ctx.world(&args(&["use", name])).unwrap_err();
            assert!(e.contains(&format!("{name}.json")) && !e.contains('\n'), "{e}");
            let e = ctx.world(&args(&["import", &ctx.worlds.join(format!("{name}.json")).to_string_lossy(), "copy"])).unwrap_err();
            assert!(e.contains(&format!("{name}.json")), "{e}");
            assert!(!ctx.worlds.join("copy.json").exists());
            let e = ctx.replace_world(&ctx.worlds.join(format!("{name}.json"))).unwrap_err();
            assert!(e.contains(&format!("{name}.json")), "{e}");
        }
        fs::write(ctx.worlds.join("old.json"), SaveFile::old_format_text()).unwrap();
        let old = ctx.worlds.join("old.json");
        let e = ctx.world(&args(&["use", "old"])).unwrap_err();
        assert!(e.contains("old.json") && e.contains("old save format") && e.contains("pcds world import"), "{e}");
        let e = ctx.replace_world(&old).unwrap_err();
        assert!(e.contains("pcds world import"), "{e}");
        assert!(!ctx.world_file().exists() && !ctx.worlds.join("backups").exists(), "nothing was installed");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// `world import` converts an old save into a world that loads, copies a current one byte for
    /// byte, and never writes over a world.
    #[test]
    fn import_converts_an_old_save_and_copies_a_current_one() {
        let dir = super::super::test_dir("import-world");
        let ctx = super::super::test_ctx(&dir);
        let old = dir.join("Survival-Konsider.json");
        fs::write(&old, SaveFile::old_format_text()).unwrap();
        ctx.world(&args(&["import", &old.to_string_lossy()])).unwrap();
        // The name is the file's.
        let made = ctx.worlds.join("Survival-Konsider.json");
        let save = SaveFile::load(&made.to_string_lossy()).unwrap();
        assert_eq!((save.objects.len(), save.players.len(), save.planets[0]["planetId"].as_str(), save.settings["dyingConsequencesLabel"].as_str()), (2, 1, Some("Prime"), Some("DropSomeItems")));
        assert!(save.settings["worldSeed"].as_u64().is_some_and(|s| s > 0));
        assert_eq!(fs::read_to_string(&old).unwrap(), SaveFile::old_format_text(), "the old save is left as it is");
        assert!(ctx.world(&args(&["import", &old.to_string_lossy()])).unwrap_err().contains("exists"));
        let current = dir.join("cur.json");
        fs::write(&current, SaveFile::tiny_text("cur")).unwrap();
        ctx.world(&args(&["import", &current.to_string_lossy(), "copy"])).unwrap();
        assert_eq!(fs::read_to_string(ctx.worlds.join("copy.json")).unwrap(), SaveFile::tiny_text("cur"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
