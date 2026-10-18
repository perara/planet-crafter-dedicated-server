//! The command line as the owner runs it, with the pcds program cargo built, an installation
//! folder of its own (PCDS_HOME) and a unit name no server uses: what needs no game tables and
//! no systemd user session (a stand-in systemctl first on the PATH plays systemd).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const PCDS: &str = env!("CARGO_BIN_EXE_pcds");

/// A new empty folder for one test.
fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pcds-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A stand-in `systemctl` in dir/fake (a shell script).
fn fake_systemctl(dir: &Path, script: &str) -> PathBuf {
    let fake = dir.join("fake");
    std::fs::create_dir_all(&fake).unwrap();
    let path = fake.join("systemctl");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    fake
}

/// `pcds ARGS` in the installation dir/run, with `path` first on the PATH.
fn pcds(dir: &Path, path: &Path, args: &[&str]) -> Output {
    let search = format!("{}:{}", path.display(), std::env::var("PATH").unwrap_or_default());
    Command::new(PCDS).args(args).env("PCDS_HOME", dir.join("run")).env("PCDS_UNIT", "pcds-cli-itest").env("PATH", search).env_remove("PCDS_CONTAINER").output().unwrap()
}

fn text(out: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

#[test]
fn status_fails_when_systemctl_does() {
    let dir = test_dir("status");
    let fake = fake_systemctl(&dir, "echo 'Failed to connect to bus: No medium found' >&2; exit 1");
    let out = pcds(&dir, &fake, &["status"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(text(&out).contains("Failed to connect to bus") && !text(&out).contains("state=stopped"), "{}", text(&out));
    // A unit systemd knows to be inactive: stopped, exit 0.
    let fake = fake_systemctl(&dir, "echo inactive");
    let out = pcds(&dir, &fake, &["status"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("state=stopped"), "{}", text(&out));
    std::fs::remove_dir_all(&dir).unwrap();
}
