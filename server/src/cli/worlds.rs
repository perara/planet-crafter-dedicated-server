//! The worlds in run/worlds/: one save per world; PCDS_WORLD names the one the server runs.

use std::fs;
use std::path::Path;
use std::process::Command;

use super::env::world_arg;
use super::{check, say, Ctx, Result};

/// --force out of a world's options, the rest for `pcds new-save`.
fn force_and_options(args: &[String]) -> (bool, Vec<String>) {
    let force = args.iter().any(|a| a == "--force");
    (force, args.iter().filter(|a| *a != "--force").cloned().collect())
}

impl Ctx {
    /// A world's file goes to worlds/backups/ before something replaces or removes it.
    pub fn keep_copy(&self, name: &str, why: &str) -> Result {
        let backups = self.worlds.join("backups");
        super::mkdir(&backups)?;
        let to = super::free_path(&backups, &format!("{name}-{why}-{}", crate::save::stamp(std::time::SystemTime::now())));
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
        let made = beside.join("world.json");
        check(Command::new(&self.exe).arg("new-save").arg(&made).arg(&data).args(options))?;
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

    pub fn worlds(&self) -> Result {
        let current = self.world_name();
        let mut files: Vec<_> = fs::read_dir(&self.worlds).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "json")).collect();
        if files.is_empty() {
            say(format!("no worlds in {} yet: 'pcds world new NAME', 'pcds world import SAVE.json', or copy a save there", self.worlds.display()));
            return Ok(());
        }
        files.sort();
        for f in files {
            let name = f.file_stem().unwrap_or_default().to_string_lossy().to_string();
            let size = fs::metadata(&f).map(|m| m.len()).unwrap_or(0);
            let mark = if name == current { '*' } else { ' ' };
            println!("{mark} {name:<28} {:>7}  {}", super::human_size(size), super::local_time(super::modified(&f)));
        }
        println!("(* the server runs it; add a world by copying a save into {})", self.worlds.display());
        Ok(())
    }

    pub fn world(&self, args: &[String]) -> Result {
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        let file = |name: &str| self.worlds.join(format!("{name}.json"));
        match a.as_slice() {
            ["use", name] => {
                let name = world_arg(name)?;
                if !file(&name).is_file() {
                    return Err(format!("no world {name} in {} ('pcds worlds' lists them)", self.worlds.display()));
                }
                if crate::env("PCDS_WORLD").is_some_and(|w| w != name) {
                    return Err("PCDS_WORLD is set in this environment (the container's): change it there".into());
                }
                self.write_env()?;
                self.set_env("PCDS_WORLD", &name)?;
                say(format!("the server runs {name} from now on"));
                self.restart_if_running()
            }
            ["new", name, ..] => {
                let name = world_arg(name)?;
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
                if !super::is_save(save) {
                    return Err(format!("{} is not a Planet Crafter save", save.display()));
                }
                let name = world_arg(a.get(2).copied().unwrap_or(&save.to_string_lossy()))?;
                if file(&name).exists() {
                    return Err(format!("world {name} exists: pass another NAME"));
                }
                super::mkdir(&self.worlds)?;
                let part = self.worlds.join(format!("{name}.json.part"));
                fs::copy(save, &part).and_then(|_| fs::rename(&part, file(&name))).map_err(|e| format!("{}: {e}", file(&name).display()))?;
                say(format!("world {name} imported; 'pcds world use {name}' plays it"));
                Ok(())
            }
            ["remove", name] => {
                let name = world_arg(name)?;
                if !file(&name).is_file() {
                    return Err(format!("no world {name} in {}", self.worlds.display()));
                }
                if name == self.world_name() {
                    return Err(format!("the server runs {name}: 'pcds world use' another one first"));
                }
                // Moved into backups/ rather than deleted.
                let backups = self.worlds.join("backups");
                super::mkdir(&backups)?;
                let to = super::free_path(&backups, &format!("{name}-removed-{}", crate::save::stamp(std::time::SystemTime::now())));
                fs::rename(file(&name), &to).map_err(|e| format!("{}: {e}", to.display()))?;
                say(format!("world {name} removed (it is kept in {})", backups.display()));
                Ok(())
            }
            _ => Err("pcds world use NAME | new NAME [OPTIONS] | import SAVE.json [NAME] | remove NAME".into()),
        }
    }
}
