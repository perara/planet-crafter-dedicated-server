//! `pcds backup list | verify | restore | fetch`: the backups of the world the server runs
//! (run/worlds/backups/), checked with the loader the server uses, put back as the world, and
//! fetched from the S3 bucket when the rules name one (docs/disaster-recovery.md).

use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::{say, Ctx, Result};
use crate::save::SaveFile;

const USAGE: &str = "\
pcds backup list                     the backups of this world, newest first
pcds backup verify [--count N | --all]
                                     load the newest backup (and N-1 older ones, 3 by default) the way the server
                                     loads a world, and say what each holds; check the bucket's copy too, when
                                     the rules name one. Exit 1 when one does not load
pcds backup restore NAME [--as WORLD] [--yes]
                                     put a backup back as the world (the one it replaces is kept in backups/ as
                                     <world>-replaced-<time>), or as a new world with --as; the server must be
                                     stopped for the first; NAME is a stamp (20261020-170930), a file name in
                                     backups/, or the path of a backup file
pcds backup fetch [NAME]             copy a backup from the S3 bucket into backups/ (the newest of this world
                                     without NAME)";

/// A backup file of the world: its name without `.json`, where it is, and what the file system says.
pub(super) struct Backup {
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub modified: SystemTime,
}

/// What loading a backup gave.
struct Loaded {
    planet: String,
    world_objects: usize,
    network_objects: usize,
    inventories: usize,
    players: usize,
    /// The reason the server would not run it, though it loads.
    refused: Option<String>,
}

impl Ctx {
    fn backups_dir(&self) -> PathBuf {
        self.worlds.join(crate::worlds::BACKUPS)
    }

    /// The backups of the current world, newest first (by the stamp in their names; one stamped
    /// in the future, a clock that stepped back, is not the newest: save::backup_stamps).
    pub(super) fn backup_list(&self) -> Vec<Backup> {
        let (dir, stem) = (self.backups_dir(), self.world_name());
        let now = crate::save::stamp(SystemTime::now());
        let stamps = crate::save::backup_stamps(&dir, &stem, &now).unwrap_or_default();
        stamps
            .into_iter()
            .rev()
            .filter_map(|t| {
                let name = format!("{stem}-{t}");
                let path = dir.join(format!("{name}.json"));
                let meta = fs::metadata(&path).ok()?;
                Some(Backup { name, path, bytes: meta.len(), modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH) })
            })
            .collect()
    }

    pub fn backup(&self, args: &[String]) -> Result<i32> {
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        match a.as_slice() {
            ["list"] => self.backup_show(),
            ["verify", rest @ ..] => self.backup_verify(rest),
            ["restore", name, rest @ ..] => self.backup_restore(name, rest).map(|()| 0),
            ["fetch"] => self.backup_fetch(None).map(|()| 0),
            ["fetch", name] => self.backup_fetch(Some(name)).map(|()| 0),
            _ => Err(USAGE.to_string()),
        }
    }

    fn backup_show(&self) -> Result<i32> {
        let list = self.backup_list();
        if list.is_empty() {
            return Err(self.no_backups());
        }
        let (title, size, made) = (format!("backup of the world {}", self.world_name()), "size", "made");
        println!("{title:<34} {size:>9}  {made}");
        for b in &list {
            println!("{:<34} {:>9}  {} ({})", b.name, super::human_size(b.bytes), super::local_time(b.modified), age(b.modified));
        }
        Ok(0)
    }

    fn no_backups(&self) -> String {
        let kept = crate::rules::Rules::load(&self.rules_file()).unwrap_or_default().save.backups_kept;
        if kept == 0 {
            "backups are off (the rule save.backupsKept is 0): pcds rules set save.backupsKept=24".into()
        } else {
            format!("no backups of the world {} in {} yet: the server makes one each save.backupMinutes minutes (pcds save makes a save, 'curl' of POST /api/backup a backup), or 'pcds backup fetch' brings one from the bucket", self.world_name(), self.backups_dir().display())
        }
    }

    /// The backup NAME names: a stamp, a name in backups/, or a file.
    fn backup_named(&self, name: &str) -> Result<PathBuf> {
        let stem = self.world_name();
        let dir = self.backups_dir();
        let tidy = name.strip_suffix(".json").unwrap_or(name);
        let candidates = [dir.join(format!("{tidy}.json")), dir.join(format!("{stem}-{tidy}.json"))];
        if let Some(found) = candidates.iter().find(|p| p.is_file()) {
            return Ok(found.clone());
        }
        let as_path = Path::new(name);
        if (name.contains('/') || name.ends_with(".json")) && as_path.is_file() {
            return Ok(as_path.to_path_buf());
        }
        Err(format!("no backup {name:?} in {}: 'pcds backup list' shows them ('pcds backup fetch' brings one from the bucket)", dir.display()))
    }

    /// A backup as the server would load it: the layout, the content against the tables, the world
    /// built (every object and inventory), and whether the server would start it. Err says why not.
    fn load_backup(&self, path: &Path) -> std::result::Result<Loaded, String> {
        let dir = self.data_dir().map_err(|e| format!("the game tables are needed to load a world: {e}"))?;
        let dir = dir.to_string_lossy().to_string();
        let (net, data, spec) = crate::try_tables(&dir)?;
        let scenes = crate::try_scene_tables(&dir, &net, &data)?;
        let save = SaveFile::load(&path.to_string_lossy()).map_err(|e| e.to_string())?;
        crate::game::check_save_content(&save, &data, &net)?;
        let players = save.players.len();
        let game = crate::game::Game::new(net, data, spec, save, scenes);
        let rules = crate::rules::Rules::load(&self.rules_file()).unwrap_or_default();
        let refused = crate::planets::ground_missing(&game, game.planet).or_else(|| crate::planets::ownership_unstated(&game, &rules));
        Ok(Loaded { planet: game.planet_id().to_string(), world_objects: game.wos.len(), network_objects: game.net.objects.len(), inventories: game.inventories.len(), players, refused })
    }

    fn backup_verify(&self, args: &[&str]) -> Result<i32> {
        let count = match args {
            [] => 3,
            ["--all"] => usize::MAX,
            ["--count", n] => n.parse::<usize>().ok().filter(|n| *n > 0).ok_or_else(|| format!("--count takes a number above 0, not {n:?}"))?,
            _ => return Err(USAGE.to_string()),
        };
        let list = self.backup_list();
        if list.is_empty() {
            return Err(self.no_backups());
        }
        let mut bad = 0;
        let started = std::time::Instant::now();
        for b in list.iter().take(count) {
            match self.load_backup(&b.path) {
                Ok(l) => {
                    let refused = l.refused.as_ref().map(|why| format!("; the server would not start it: {why}")).unwrap_or_default();
                    bad += usize::from(l.refused.is_some());
                    println!("{} {}  {:>9}  {}  planet {}, {} world objects, {} network objects, {} inventories, {} saved players{refused}", if l.refused.is_some() { "FAIL" } else { "ok  " }, b.name, super::human_size(b.bytes), age(b.modified), l.planet, l.world_objects, l.network_objects, l.inventories, l.players);
                }
                Err(e) => {
                    bad += 1;
                    println!("FAIL {}  {:>9}  {}  {e}", b.name, super::human_size(b.bytes), age(b.modified));
                }
            }
        }
        let checked = list.len().min(count);
        bad += usize::from(!self.backup_verify_bucket(&list[0]));
        say(format!("{checked} of {} backups loaded in {:.1} s: {} ok", list.len(), started.elapsed().as_secs_f64(), checked - bad.min(checked)));
        if bad > 0 {
            println!("pcds: restore one that is ok: pcds backup restore NAME (the world is kept first); 'pcds backup verify --all' checks the rest");
        }
        Ok(i32::from(bad > 0))
    }

    /// The bucket's copy of the newest backup (when the rules name a bucket): there, and the size it has.
    fn backup_verify_bucket(&self, newest: &Backup) -> bool {
        let rules = crate::rules::Rules::load(&self.rules_file()).unwrap_or_default();
        if rules.backup.s3_bucket.is_none() {
            println!("S3: off (the rule backup.s3Bucket is not set), so the backups exist on this machine only: docs/disaster-recovery.md");
            return true;
        }
        let key = format!("{}{}.json", rules.backup.s3_prefix, newest.name);
        match crate::s3::head(&rules.backup, &key) {
            Ok(Some(bytes)) if bytes == newest.bytes => {
                println!("ok   S3 s3://{}/{key}  {:>9}  the newest backup is in the bucket, the same size", rules.backup.s3_bucket.as_deref().unwrap_or(""), super::human_size(bytes));
                true
            }
            Ok(Some(bytes)) => {
                println!("FAIL S3 s3://{}/{key} is {} bytes, the file here {}: a copy that was cut short? pcds does not copy it again by itself: POST /api/backup/s3 does", rules.backup.s3_bucket.as_deref().unwrap_or(""), bytes, newest.bytes);
                false
            }
            Ok(None) => {
                println!("FAIL S3 s3://{}/{key} is not in the bucket: the copy failed or has not run yet (pcds doctor shows the last upload; POST /api/backup/s3 copies it again)", rules.backup.s3_bucket.as_deref().unwrap_or(""));
                false
            }
            Err(e) => {
                println!("FAIL S3: {e} (the rules backup.s3Endpoint, s3Bucket, s3AccessKey, s3SecretKey, s3Region)");
                false
            }
        }
    }

    fn backup_restore(&self, name: &str, rest: &[&str]) -> Result {
        let (mut as_world, mut yes) = (None::<String>, false);
        let mut it = rest.iter();
        while let Some(arg) = it.next() {
            match *arg {
                "--as" => as_world = Some(crate::worlds::name_arg(it.next().ok_or("usage: --as WORLD")?)?),
                "--yes" => yes = true,
                other => return Err(format!("unknown option {other} ({USAGE})")),
            }
        }
        let path = self.backup_named(name)?;
        let loaded = self.load_backup(&path).map_err(|e| format!("{} does not load: {e}. Nothing was changed. 'pcds backup verify' finds one that does", path.display()))?;
        if let Some(why) = &loaded.refused {
            return Err(format!("{} loads, but the server would not start it: {why}. Nothing was changed", path.display()));
        }
        let what = format!("{} world objects, {} inventories, planet {}, made {}", loaded.world_objects, loaded.inventories, loaded.planet, super::local_time(super::modified(&path)));
        if let Some(world) = as_world {
            let target = self.worlds.join(format!("{world}.json"));
            if target.exists() {
                return Err(format!("the world {world} exists already ({}): choose another name with --as, or 'pcds world remove {world}' first", target.display()));
            }
            super::mkdir(&self.worlds)?;
            super::install_file(&path, &target)?;
            say(format!("backup {name} ({what}) is the new world {world}: 'pcds world use {world}' makes it the one the server runs"));
            return Ok(());
        }
        // Over the current world: the server holds it open and saves over it.
        if self.active()? {
            return Err("stop the server first (pcds stop): it would save over the restored world. A running server can take a backup as a new world: pcds backup restore NAME --as WORLD".into());
        }
        let current = self.world_file();
        let replaced = if current.is_file() { format!("; the world now ({}) is kept in backups/ as {}-replaced-<time>", super::human_size(fs::metadata(&current).map(|m| m.len()).unwrap_or(0)), self.world_name()) } else { String::new() };
        if !yes {
            if !super::is_tty() {
                return Err(format!("this replaces the world {} with the backup {name}; pass --yes to do it without asking", self.world_name()));
            }
            eprint!("Replace the world {} with backup {name} ({what}){replaced}? [y/N] ", self.world_name());
            let mut answer = String::new();
            std::io::stdin().lock().read_line(&mut answer).map_err(|e| e.to_string())?;
            if !matches!(answer.trim(), "y" | "Y" | "yes") {
                return Err("not restored".into());
            }
        }
        self.replace_world(&path)?;
        // A snapshot the last stop left belongs to the world that was replaced.
        let _ = fs::remove_file(crate::worlds::snapshot_path(&current.to_string_lossy()));
        say(format!("restored: {what}. 'pcds start' runs it"));
        Ok(())
    }

    fn backup_fetch(&self, name: Option<&str>) -> Result {
        let rules = crate::rules::Rules::load(&self.rules_file()).unwrap_or_default();
        let b = &rules.backup;
        if b.s3_bucket.is_none() || b.s3_endpoint.is_none() {
            return Err("no bucket: set the rules backup.s3Endpoint, s3Bucket, s3AccessKey and s3SecretKey (pcds rules set ..., or put them in run/rules.json before the server starts)".into());
        }
        let stem = self.world_name();
        let key = match name {
            Some(n) => {
                let n = n.strip_suffix(".json").unwrap_or(n);
                let n = if n.starts_with(&format!("{stem}-")) { n.to_string() } else { format!("{stem}-{n}") };
                format!("{}{n}.json", b.s3_prefix)
            }
            None => {
                let mut keys = crate::s3::list(b, &format!("{}{stem}-", b.s3_prefix))?;
                keys.retain(|k| k.strip_prefix(&format!("{}{stem}-", b.s3_prefix)).and_then(|t| t.strip_suffix(".json")).is_some_and(|t| t.len() == 15));
                keys.sort();
                keys.pop().ok_or_else(|| format!("the bucket holds no backup of the world {stem} under {}{stem}-", b.s3_prefix))?
            }
        };
        let file = key.rsplit('/').next().unwrap_or(&key).to_string();
        let bytes = crate::s3::get(b, &key)?;
        let dir = self.backups_dir();
        super::mkdir(&dir)?;
        let target = dir.join(&file);
        crate::save::write_atomic(&target.to_string_lossy(), &bytes).map_err(|e| format!("{}: {e}", target.display()))?;
        say(format!("{} bytes of s3://{}/{key} are {}: 'pcds backup verify' loads it, 'pcds backup restore {}' puts it back", bytes.len(), b.s3_bucket.as_deref().unwrap_or(""), target.display(), file.trim_end_matches(".json")));
        Ok(())
    }
}

/// "3 h ago", "12 min ago".
fn age(t: SystemTime) -> String {
    format!("{} ago", age_text(SystemTime::now().duration_since(t).unwrap_or(Duration::ZERO).as_secs()))
}

/// "3 h", "12 min": a length of time in the unit that fits.
pub(super) fn age_text(s: u64) -> String {
    match s {
        0..=89 => format!("{s} s"),
        90..=5399 => format!("{} min", s / 60),
        5400..=129_599 => format!("{} h", s / 3600),
        _ => format!("{} days", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_read_in_the_unit_that_fits() {
        let ago = |s: u64| age(SystemTime::now() - Duration::from_secs(s));
        assert_eq!((ago(5).as_str(), ago(300).as_str(), ago(7_200).as_str(), ago(3 * 86_400).as_str()), ("5 s ago", "5 min ago", "2 h ago", "3 days ago"));
    }

    /// A backup is named by its stamp, its name in backups/ (with or without .json) or a file's path;
    /// the list is this world's, newest first, a stamp ahead of the clock last.
    #[test]
    fn backups_are_listed_newest_first_and_named_three_ways() {
        let dir = super::super::test_dir("backup-names");
        let ctx = super::super::test_ctx(&dir);
        let backups = ctx.backups_dir();
        fs::create_dir_all(&backups).unwrap();
        for f in ["world-20261001-000000.json", "world-20261003-000000.json", "world-20990101-000000.json", "world-2-20261005-000000.json", "world-replaced-20261006-000000.json", "world-20261002-000000.txt"] {
            fs::write(backups.join(f), "{}").unwrap();
        }
        let names: Vec<String> = ctx.backup_list().into_iter().map(|b| b.name).collect();
        // The one from the future (a clock that stepped back) is not the newest.
        assert_eq!(names, ["world-20261003-000000", "world-20261001-000000", "world-20990101-000000"]);
        for name in ["20261003-000000", "world-20261003-000000", "world-20261003-000000.json"] {
            assert_eq!(ctx.backup_named(name).unwrap(), backups.join("world-20261003-000000.json"), "{name}");
        }
        let elsewhere = dir.join("copy.json");
        fs::write(&elsewhere, "{}").unwrap();
        assert_eq!(ctx.backup_named(elsewhere.to_str().unwrap()).unwrap(), elsewhere);
        let e = ctx.backup_named("20300101-000000").unwrap_err();
        assert!(e.contains("no backup") && e.contains("pcds backup list") && e.contains("pcds backup fetch"), "{e}");
        assert!(ctx.no_backups().contains("no backups of the world world"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
