//! `pcds doctor` on hostile files: whatever server.env, rules.json, access.json, the world, the
//! backups' names, the plugin approvals and the paths of the installation hold, the file checks end
//! with a report that prints and serialises, and the unit files written for any path have the lines
//! they have for a plain one (no line break of a path adds a setting). Only the checks that read
//! files: those that start programs or open sockets are not run.
use super::*;

fn ctx(dir: &Path, run: &Path, unit_name: &str) -> Ctx {
    Ctx {
        exe: dir.join("build/pcds"),
        web: dir.join("build/web/index.html"),
        env_file: run.join("server.env"),
        worlds: run.join("worlds"),
        steamcmd: run.join("steamcmd"),
        run: run.to_path_buf(),
        unit_dir: dir.join("systemd"),
        unit_name: unit_name.into(),
        container: false,
        home: dir.join("home"),
    }
}

pub fn run(data: &[u8]) {
    let parts: Vec<&[u8]> = data.split(|b| *b == 0xff).collect();
    let part = |i: usize| parts.get(i).copied().unwrap_or_default();
    let dir = std::env::temp_dir().join(format!("pcds-fuzz-doctor-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let run = dir.join("run");
    let c = ctx(&dir, &run, "pcds-fuzz");
    let backups = c.worlds.join(crate::worlds::BACKUPS);
    std::fs::create_dir_all(&backups).unwrap();
    std::fs::create_dir_all(run.join("plugins")).unwrap();
    for (path, i) in [(c.env_file.clone(), 0), (run.join("rules.json"), 1), (run.join("access.json"), 2), (c.world_file(), 3), (run.join("plugins/approved.json"), 4)] {
        std::fs::write(path, part(i)).unwrap();
    }
    // The server.env of the input may rename the world; the backups of the input are named by lines.
    for name in String::from_utf8_lossy(part(5)).lines().take(50) {
        let name = name.replace(['/', '\0'], "_");
        if !name.is_empty() && name != "." && name != ".." {
            let _ = std::fs::write(backups.join(name), b"{}");
        }
    }
    let live = Live { status: None, plugins: None, unreachable: None };
    let mut r = Report::default();
    let world = c.check_world(&mut r, None);
    c.check_files(&mut r, None);
    c.check_modes(&mut r);
    c.check_disk(&mut r, world.as_ref());
    c.check_backups(&mut r, &live);
    c.check_plugins(&mut r, &live);
    assert!(!r.text().is_empty());
    let _ = serde_json::to_string(&r.json(false)).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    // The units for paths with whatever characters: as many lines as for a plain path.
    let text = |i: usize| String::from_utf8_lossy(part(6 + i)).into_owned();
    let hostile = ctx(Path::new("/srv/a"), Path::new(&text(0)), &text(1));
    // (A run folder under /tmp has no private /tmp of its own to ask for: one line less, by design.)
    let plain = ctx(Path::new("/srv/a"), Path::new(if hostile.run.starts_with("/tmp") || hostile.run.starts_with("/var/tmp") { "/tmp/a/run" } else { "/srv/a/run" }), "pcds");
    for mounts in [false, true] {
        let Ok(files) = hostile.unit_files_with(mounts) else { continue };
        let expect = plain.unit_files_with(mounts).unwrap();
        for ((_, got), (_, want)) in files.iter().zip(&expect) {
            assert_eq!(got.lines().count(), want.lines().count(), "{got}");
        }
    }
}
