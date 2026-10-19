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
    cmd.args(args).env("PCDS_HOME", dir.join("run")).env("PCDS_UNIT", "pcds-cli-itest").env("PATH", search).env("HOME", dir.join("home")).env_remove("XDG_CONFIG_HOME").env_remove("PCDS_CONTAINER").env_remove("PCDS_GAME_DIR");
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

/// A stand-in `pcds read-game GAME OUT`: the tables, of the build and format in the files next
/// to it.
const READER: &str = r#"[ "$1" = read-game ] || exit 2
mkdir -p "$3" && echo '{"gameVersion": "2.103"}' > "$3/netdata.json"
id=$(cat "$(dirname "$0")/build") && printf %s "$id" > "$3/buildid"
cp "$(dirname "$0")/format" "$3/tables-format"
echo "read by bank a" > "$3/reader""#;

/// An installation in dir/run with a stand-in game (dir/game) and a stand-in bank a whose
/// `read-game` writes tables of the format in run/bin/a/format; the game.
fn with_reader(dir: &Path, format: u32) -> PathBuf {
    let game = dir.join("game");
    std::fs::create_dir_all(game.join("Planet Crafter_Data/Managed")).unwrap();
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::fs::write(game.join("Planet Crafter.exe"), "").unwrap();
    std::fs::write(game.join("Planet Crafter_Data/Managed/Assembly-CSharp.dll"), "code").unwrap();
    fake_bank(dir, "a", READER, format);
    std::fs::write(dir.join("run/bin/a/format"), format.to_string()).unwrap();
    // The build pcds computes (xxh64 of the game code), for the stand-in to write.
    std::fs::write(dir.join("run/bin/a/build"), format!("{:016x}", xxhash_rust::xxh64::xxh64(b"code", 0))).unwrap();
    game
}

/// `pcds extract` and `pcds update` read the tables with the active bank's program, and read
/// them again when that program reads another tables format than the tables on disk. `pcds
/// status` says when the game has no tables.
#[test]
fn tables_are_read_by_the_active_bank_in_its_format() {
    let dir = test_dir("reader");
    let fake = fake_systemctl(&dir, "echo inactive");
    let game = with_reader(&dir, 7);
    let game_arg = game.to_str().unwrap();
    let out = pcds(&dir, &fake, &["status"]);
    assert!(text(&out).contains("game: game not found"), "{}", text(&out));
    let out = pcds(&dir, &fake, &["extract", "--game", game_arg]);
    assert!(out.status.success(), "{}", text(&out));
    let tables = dir.join("run/data/2.103");
    assert_eq!(std::fs::read_to_string(tables.join("reader")).unwrap(), "read by bank a\n");
    let out = pcds(&dir, &fake, &["update", "--game", game_arg]);
    assert!(out.status.success() && text(&out).contains("up to date"), "{}", text(&out));
    let out = pcds(&dir, &fake, &["status"]);
    assert!(out.status.success() && !text(&out).contains("game"), "{}", text(&out));
    // A newer bank that reads format 8: status says so, and update reads them again with it.
    fake_bank(&dir, "a", READER, 8);
    std::fs::write(dir.join("run/bin/a/format"), "8").unwrap();
    let out = pcds(&dir, &fake, &["status"]);
    assert!(text(&out).contains("has no tables: the tables in") && text(&out).contains("of tables format 7, the server reads format 8"), "{}", text(&out));
    let out = pcds(&dir, &fake, &["update", "--game", game_arg]);
    assert!(out.status.success() && text(&out).contains("of tables format 7, the server reads format 8"), "{}", text(&out));
    assert_eq!(std::fs::read_to_string(tables.join("tables-format")).unwrap(), "8");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A game downloaded with steamcmd that steamcmd cannot update fails `pcds update` (the
/// update check's unit), after the tables of the game as it is are looked after.
#[test]
fn a_failed_game_download_fails_the_update() {
    let dir = test_dir("steamcmd");
    let fake = fake_systemctl(&dir, "echo inactive");
    let game = with_reader(&dir, 1);
    let game_arg = game.to_str().unwrap();
    assert!(pcds(&dir, &fake, &["extract", "--game", game_arg]).status.success());
    std::fs::write(dir.join("run/steam-user"), "someone\n").unwrap();
    std::fs::create_dir_all(dir.join("run/steamcmd")).unwrap();
    let steamcmd = dir.join("run/steamcmd/steamcmd.sh");
    std::fs::write(&steamcmd, "#!/bin/sh\necho 'Login Failure: Account Logon Denied'\nexit 5\n").unwrap();
    std::fs::set_permissions(&steamcmd, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = pcds(&dir, &fake, &["update", "--game", game_arg]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(text(&out).contains("up to date") && text(&out).contains("the game update through steamcmd failed") && text(&out).contains("--steam-user someone"), "{}", text(&out));
    assert!(std::fs::read_to_string(dir.join("run/steamcmd/update.log")).unwrap().contains("Logon Denied"));
    std::fs::write(&steamcmd, "#!/bin/sh\nexit 0\n").unwrap();
    assert!(pcds(&dir, &fake, &["update", "--game", game_arg]).status.success());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `pcds run` whose active bank's front cannot start the world (exit 75) starts the other
/// bank's front, makes it the active one, and says so in pcds status.
#[test]
fn a_start_falls_back_to_the_other_bank() {
    let dir = test_dir("fallback");
    let fake = fake_systemctl(&dir, "echo inactive");
    let game = with_reader(&dir, 1);
    assert!(pcds(&dir, &fake, &["extract", "--game", game.to_str().unwrap()]).status.success());
    std::fs::create_dir_all(dir.join("run/worlds")).unwrap();
    std::fs::write(dir.join("run/worlds/world.json"), "{\"saveDisplayName\":\"w\"}").unwrap();
    let serving = |serve: &str| format!("if [ \"$1\" = serve ]; then {serve}; fi\n{READER}");
    fake_bank(&dir, "a", &serving("echo 'no bank could start the world' >&2; exit 75"), 1);
    fake_bank(&dir, "b", &serving("echo \"bank b runs $3\"; exit 0"), 1);
    for f in ["format", "build"] {
        std::fs::copy(dir.join("run/bin/a").join(f), dir.join("run/bin/b").join(f)).unwrap();
    }
    // The unit a 0.5 release folder wrote: pcds run writes this version's in its place.
    let units = dir.join("home/.config/systemd/user");
    std::fs::create_dir_all(&units).unwrap();
    let old = format!("[Service]\nEnvironmentFile={}\nExecStart=\"{}/pcds\" run\nTimeoutStopSec=60\n", dir.join("run/server.env").display(), dir.display());
    std::fs::write(units.join("pcds-cli-itest.service"), old).unwrap();
    let out = pcds(&dir, &fake, &["run"]);
    let said = text(&out);
    assert_eq!(out.status.code(), Some(0), "{said}");
    let unit = std::fs::read_to_string(units.join("pcds-cli-itest.service")).unwrap();
    assert!(unit.contains("KillMode=mixed") && unit.contains("TimeoutStopSec=90") && said.contains("written again"), "{unit}\n{said}");
    assert!(said.contains("bank a could not start the world: its front (pcds 9.0.0) ended without running it (exit 75") && said.contains("Starting bank b"), "{said}");
    assert!(said.contains(&format!("bank b runs {}", dir.join("run/worlds/world.json").display())), "{said}");
    assert_eq!(std::fs::read_to_string(dir.join("run/bin/active")).unwrap().trim(), "b");
    let out = pcds(&dir, &fake, &["status"]);
    assert!(text(&out).contains("started from bank b because bank a could not start the world"), "{}", text(&out));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A second server on a world another one runs is refused before it touches anything.
#[test]
fn one_server_per_world() {
    use std::os::fd::AsRawFd;
    let dir = test_dir("world-lock");
    let save = dir.join("world.json");
    let lock = std::fs::File::create(dir.join("world.json.lock")).unwrap();
    // SAFETY: flock on a file this test owns.
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    let out = Command::new(PCDS).arg("serve").arg(dir.join("no-tables")).arg(&save).env("PCDS_STEAM", "off").env("PCDS_LISTEN", "off").env("PCDS_ADMIN", "off").output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(text(&out).contains("another server runs this world"), "{}", text(&out));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A setting that moved to the rules, still set in server.env, stops the start in one line that
/// says where it went; `pcds config migrate` is the way out (config_migrate below).
#[test]
fn retired_settings_stop_the_start_and_name_their_replacement() {
    let dir = test_dir("retired");
    let fake = fake_systemctl(&dir, "echo inactive");
    std::fs::create_dir_all(dir.join("run")).unwrap();
    std::fs::write(dir.join("run/server.env"), "PCDS_ADMIN_TOKEN=s3cret-token\nPCDS_AUTOSAVE=10\nPCDS_BACKUPS=5\n").unwrap();
    let out = pcds(&dir, &fake, &["run"]);
    let said = text(&out);
    assert_eq!(out.status.code(), Some(1), "{said}");
    assert!(said.contains("PCDS_AUTOSAVE is now the rule save.autosaveMinutes: edit rules.json or POST /api/config") && said.contains("PCDS_BACKUPS is now the rule save.backupsKept"), "{said}");
    assert!(!said.contains("s3cret-token") && said.matches("server.env: ").count() == 1, "one line, no secrets: {said}");
    // The front refuses it too, set in its environment.
    let out = Command::new(PCDS).arg("serve").arg(dir.join("no-tables")).arg(dir.join("world.json")).env("PCDS_TRACE", "1").output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("PCDS_TRACE is now the rule log.trace"), "{}", text(&out));
    // What an older launcher sets for the front is taken when it is where PCDS_HOME has it, and
    // stops the front when it is not.
    let home = dir.join("run");
    let serve = |vars: &[(&str, &Path)]| Command::new(PCDS).arg("serve").arg(dir.join("no-tables")).arg(dir.join("world.json")).env("PCDS_HOME", &home).envs(vars.iter().copied()).output().unwrap();
    let elsewhere = dir.join("elsewhere/rules.json");
    let out = serve(&[("PCDS_RULES", &elsewhere)]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains(&format!("PCDS_RULES={}, but this version reads {}", elsewhere.display(), home.join("rules.json").display())), "{}", text(&out));
    let out = serve(&[("PCDS_BANKS", &dir.join("bin"))]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("PCDS_BANKS=") && text(&out).contains("PCDS_HOME/bin"), "{}", text(&out));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `pcds config migrate` shows what it would do and writes nothing, then with --apply moves the
/// retired settings into rules.json, keeps the old file and never prints a secret; after it the
/// server no longer stops at the old settings.
#[test]
fn config_migrate_moves_the_retired_settings() {
    let dir = test_dir("migrate");
    let fake = fake_systemctl(&dir, "echo inactive");
    let run = dir.join("run");
    std::fs::create_dir_all(&run).unwrap();
    let old = "PCDS_LISTEN=0.0.0.0:7777\nPCDS_STEAM_TOKEN=STEAMSECRET\nPCDS_AUTOSAVE=10\nPCDS_BACKUPS=24\nPCDS_BACKUP_MINUTES=60\nPCDS_ADMIN=127.0.0.1:7782\nPCDS_ADMIN_TOKEN=ADMINSECRET\nPCDS_AUTO_UPDATE=on\nPCDS_WORLD=world\n";
    std::fs::write(run.join("server.env"), old).unwrap();
    let out = pcds(&dir, &fake, &["config", "migrate"]);
    let said = text(&out);
    assert_eq!(out.status.code(), Some(0), "{said}");
    assert!(said.contains("dry run") && said.contains("PCDS_AUTOSAVE=10  -> rules.json: save.autosaveMinutes = 10") && said.contains("PCDS_AUTO_UPDATE=on  -> rules.json: update.auto = on"), "{said}");
    assert!(said.contains(r#"{"save":{"autosaveMinutes":10},"update":{"auto":true}}"#), "{said}");
    assert!(!said.contains("SECRET"), "no secret in the output: {said}");
    assert_eq!(std::fs::read_to_string(run.join("server.env")).unwrap(), old, "a dry run writes nothing");
    assert!(!run.join("rules.json").exists());
    // The server still stops at the old file.
    assert!(text(&pcds(&dir, &fake, &["run"])).contains("PCDS_AUTOSAVE is now the rule save.autosaveMinutes"));
    let out = pcds(&dir, &fake, &["config", "migrate", "--apply"]);
    let said = text(&out);
    assert_eq!(out.status.code(), Some(0), "{said}");
    assert!(!said.contains("SECRET"), "{said}");
    assert_eq!(std::fs::read_to_string(run.join("server.env")).unwrap(), "PCDS_LISTEN=0.0.0.0:7777\nPCDS_STEAM_TOKEN=STEAMSECRET\nPCDS_ADMIN=127.0.0.1:7782\nPCDS_ADMIN_TOKEN=ADMINSECRET\nPCDS_WORLD=world\n");
    let rules: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(run.join("rules.json")).unwrap()).unwrap();
    assert_eq!(rules, serde_json::json!({"save": {"autosaveMinutes": 10}, "update": {"auto": true}}));
    assert_eq!(std::fs::read_to_string(run.join("server.env.before-migrate")).unwrap(), old);
    assert_eq!(std::fs::metadata(run.join("server.env")).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(run.join("server.env.before-migrate")).unwrap().permissions().mode() & 0o777, 0o600);
    // Done: nothing more to move, and the start goes past the settings (to the missing world).
    assert!(text(&pcds(&dir, &fake, &["config", "migrate"])).contains("nothing to migrate"));
    let said = text(&pcds(&dir, &fake, &["run"]));
    assert!(said.contains("no world at") && !said.contains("is now the rule"), "{said}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A server.env value that is not what the setting takes stops the start, naming it (and
/// never showing a secret's value).
#[test]
fn bad_settings_stop_the_start() {
    let dir = test_dir("settings");
    let fake = fake_systemctl(&dir, "echo inactive");
    std::fs::create_dir_all(dir.join("run")).unwrap();
    std::fs::write(dir.join("run/server.env"), "PCDS_STEAM=maybe\nPCDS_SESSION=12345\n").unwrap();
    let out = pcds(&dir, &fake, &["run"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(text(&out).contains("server.env: ") && text(&out).contains("PCDS_STEAM: \"maybe\" is not one of") && text(&out).contains("PCDS_SESSION: 12345"), "{}", text(&out));
    // The front itself checks its environment too.
    let out = Command::new(PCDS).arg("serve").arg(dir.join("no-tables")).arg(dir.join("world.json")).env("PCDS_SESSION", "lots").output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("PCDS_SESSION: \"lots\""), "{}", text(&out));
    std::fs::remove_dir_all(&dir).unwrap();
}
