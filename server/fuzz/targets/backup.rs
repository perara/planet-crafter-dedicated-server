//! `pcds backup`: the names of backups. Whatever file names lie in backups/ and whatever NAME the
//! operator types, the list holds this world's backups only, newest first, each a file there, and a
//! name resolves to a file that exists, inside backups/ unless it is a path.
use super::*;
use std::fs;

fn scratch() -> PathBuf {
    std::env::temp_dir().join(format!("pcds-fuzz-backup-{}", std::process::id()))
}

pub fn run(data: &[u8]) {
    let parts: Vec<String> = data.split(|b| *b == 0xff).map(|p| String::from_utf8_lossy(p).into_owned()).collect();
    let dir = scratch();
    let _ = fs::remove_dir_all(&dir);
    let run = dir.join("run");
    let ctx = Ctx {
        exe: dir.join("build/pcds"),
        web: dir.join("build/web/index.html"),
        env_file: run.join("server.env"),
        worlds: run.join("worlds"),
        steamcmd: run.join("steamcmd"),
        run: run.clone(),
        unit_dir: dir.join("systemd"),
        unit_name: "pcds-fuzz".into(),
        container: false,
        home: dir.join("home"),
    };
    let backups = ctx.backups_dir();
    fs::create_dir_all(&backups).unwrap();
    // The world is named by a line of server.env; the files are whatever the input names (one
    // path component each: a file name holds no slash and no NUL).
    let world = parts.first().map(|w| w.replace(['\n', '\r', '\0'], "")).unwrap_or_default();
    fs::write(&ctx.env_file, format!("PCDS_WORLD={world}\n")).unwrap();
    let mut made = 0;
    for name in parts.iter().skip(1).take(parts.len().saturating_sub(2)) {
        let name = name.replace(['/', '\0'], "_");
        if !name.is_empty() && name != "." && name != ".." && fs::write(backups.join(&name), b"{}").is_ok() {
            made += 1;
        }
    }
    let stem = ctx.world_name();
    let list = ctx.backup_list();
    assert!(list.len() <= made);
    for b in &list {
        assert!(b.name.starts_with(&format!("{stem}-")) && b.path.starts_with(&backups) && b.path.is_file(), "{b:?}", b = b.name);
        assert_eq!(b.path, backups.join(format!("{}.json", b.name)));
    }
    let typed = parts.last().cloned().unwrap_or_default();
    if let Ok(path) = ctx.backup_named(&typed) {
        assert!(path.is_file(), "{path:?}");
        assert!(path.starts_with(&backups) || typed.contains('/') || typed.ends_with(".json"), "{typed:?} gave {path:?}");
    }
    let _ = age_text(typed.len() as u64 * 1_000_003);
}
