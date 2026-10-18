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
    pcds_at(Path::new(PCDS), dir, path, args)
}

/// The same with a copy of pcds at `program`.
fn pcds_at(program: &Path, dir: &Path, path: &Path, args: &[&str]) -> Output {
    let search = format!("{}:{}", path.display(), std::env::var("PATH").unwrap_or_default());
    let mut cmd = Command::new(program);
    cmd.args(args).env("PCDS_HOME", dir.join("run")).env("PCDS_UNIT", "pcds-cli-itest").env("PATH", search).env_remove("PCDS_CONTAINER");
    // A program just written may still be open in a child another test forks at that moment.
    for _ in 0..100 {
        match cmd.output() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => std::thread::sleep(std::time::Duration::from_millis(20)),
            out => return out.unwrap(),
        }
    }
    panic!("{}: text file busy", program.display())
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

/// A bank holding a stand-in program (a shell script) that says which tables format it reads.
fn fake_bank(dir: &Path, bank: &str, script: &str, tables_format: u32) {
    let bank = dir.join("run/bin").join(bank);
    std::fs::create_dir_all(&bank).unwrap();
    std::fs::write(bank.join("pcds"), format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(bank.join("pcds"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let info = serde_json::json!({"version": "9.0.0", "protocol": 3, "format": 1, "reads": [0, 1], "tablesFormat": tables_format});
    std::fs::write(bank.join("bank.json"), info.to_string()).unwrap();
}

/// run/bin/pcds, what the units run, hands every command to the active bank's program.
#[test]
fn the_launcher_hands_commands_to_the_active_bank() {
    let dir = test_dir("hand-over");
    let fake = fake_systemctl(&dir, "echo inactive");
    std::fs::create_dir_all(dir.join("run/bin")).unwrap();
    std::fs::copy(PCDS, dir.join("run/bin/pcds")).unwrap();
    fake_bank(&dir, "a", "echo \"bank a: $* in $PCDS_HOME\"", 1);
    let out = pcds_at(&dir.join("run/bin/pcds"), &dir, &fake, &["status", "--json"]);
    assert_eq!(text(&out).trim(), format!("bank a: status --json in {}", dir.join("run").display()));
    // No bank yet: the launcher runs the command itself.
    std::fs::remove_dir_all(dir.join("run/bin/a")).unwrap();
    let out = pcds_at(&dir.join("run/bin/pcds"), &dir, &fake, &["status"]);
    assert!(text(&out).contains("state=stopped"), "{}", text(&out));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `pcds extract` and `pcds update` read the tables with the active bank's program, and read
/// them again when that program reads another tables format than the tables on disk.
#[test]
fn tables_are_read_by_the_active_bank_in_its_format() {
    let dir = test_dir("reader");
    let fake = fake_systemctl(&dir, "echo inactive");
    let game = dir.join("game");
    std::fs::create_dir_all(game.join("Planet Crafter_Data/Managed")).unwrap();
    std::fs::write(game.join("Planet Crafter.exe"), "").unwrap();
    std::fs::write(game.join("Planet Crafter_Data/Managed/Assembly-CSharp.dll"), "code").unwrap();
    // `read-game GAME OUT`: the tables, with the format in the file next to the program.
    let reader = r#"[ "$1" = read-game ] || exit 2
mkdir -p "$3" && echo '{"gameVersion": "2.103"}' > "$3/netdata.json"
id=$(cat "$(dirname "$0")/build") && printf %s "$id" > "$3/buildid"
cp "$(dirname "$0")/format" "$3/tables-format"
echo "read by bank a" > "$3/reader""#;
    fake_bank(&dir, "a", reader, 7);
    std::fs::write(dir.join("run/bin/a/format"), "7").unwrap();
    // The build pcds computes (xxh64 of the game code), for the stand-in to write.
    std::fs::write(dir.join("run/bin/a/build"), format!("{:016x}", xxhash_rust::xxh64::xxh64(b"code", 0))).unwrap();
    let out = pcds(&dir, &fake, &["extract", "--game", game.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out));
    let tables = dir.join("run/data/2.103");
    assert_eq!(std::fs::read_to_string(tables.join("reader")).unwrap(), "read by bank a\n");
    let out = pcds(&dir, &fake, &["update", "--game", game.to_str().unwrap()]);
    assert!(out.status.success() && text(&out).contains("up to date"), "{}", text(&out));
    // A newer bank that reads format 8: update reads the tables again with it.
    fake_bank(&dir, "a", reader, 8);
    std::fs::write(dir.join("run/bin/a/format"), "8").unwrap();
    let out = pcds(&dir, &fake, &["update", "--game", game.to_str().unwrap()]);
    assert!(out.status.success() && text(&out).contains("of tables format 7, the server reads format 8"), "{}", text(&out));
    assert_eq!(std::fs::read_to_string(tables.join("tables-format")).unwrap(), "8");
    std::fs::remove_dir_all(&dir).unwrap();
}
