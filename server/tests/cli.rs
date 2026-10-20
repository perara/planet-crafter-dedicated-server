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
    let info = serde_json::json!({"version": "9.0.0", "protocol": 3, "format": 1, "tablesFormat": tables_format});
    std::fs::write(bank.join("bank.json"), info.to_string()).unwrap();
}

/// A release the server gave up for crashing (banks/rolled-back with a reason) is in `pcds status`,
/// a rollback of the owner's (no reason) is not explained there.
#[test]
fn status_says_when_a_release_was_given_up_for_crashing() {
    let dir = test_dir("given-up");
    let fake = fake_systemctl(&dir, "echo inactive");
    fake_bank(&dir, "a", "true", 1);
    fake_bank(&dir, "b", "true", 1);
    let note = dir.join("run/bin/rolled-back");
    std::fs::write(&note, r#"{"bank": "b", "tag": "v9.1.0", "reason": "the game ended 6 times within 10 minutes after it was flipped to", "at": 1}"#).unwrap();
    let out = pcds(&dir, &fake, &["status"]);
    assert!(text(&out).contains("the release in bank b (v9.1.0) was rolled back from automatically: the game ended 6 times"), "{}", text(&out));
    std::fs::write(&note, r#"{"bank": "b", "tag": "v9.1.0"}"#).unwrap();
    let out = pcds(&dir, &fake, &["status"]);
    assert!(!text(&out).contains("automatically"), "{}", text(&out));
    std::fs::remove_dir_all(&dir).unwrap();
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
    // A unit that differs from this release's (a shorter stop timeout): pcds run writes this release's in its place.
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

/// systemd counts every start toward the unit's limit (5 in 10 minutes), so `pcds restart` clears
/// the count first: an upgrade, a rollback and an upgrade again must not leave the unit failed.
#[test]
fn a_restart_clears_the_units_start_count_first() {
    let dir = test_dir("reset-failed");
    let log = dir.join("systemctl.log");
    let fake = fake_systemctl(&dir, &format!("echo \"$*\" >> {}; echo inactive", log.display()));
    let game = with_reader(&dir, 1);
    assert!(pcds(&dir, &fake, &["extract", "--game", game.to_str().unwrap()]).status.success());
    std::fs::create_dir_all(dir.join("run/worlds")).unwrap();
    std::fs::write(dir.join("run/worlds/world.json"), "{\"saveDisplayName\":\"w\"}").unwrap();
    std::fs::create_dir_all(dir.join("home/.config/systemd/user")).unwrap();
    std::fs::write(dir.join("home/.config/systemd/user/pcds-cli-itest.service"), "").unwrap();
    // restart puts this release into the banks first, and for that the program has its library and
    // page beside it (a test binary has not: a copy that has).
    let prog = dir.join("prog");
    std::fs::create_dir_all(prog.join("web")).unwrap();
    std::fs::copy(PCDS, prog.join("pcds")).unwrap();
    std::fs::write(prog.join("libsteam_api.so"), "x").unwrap();
    std::fs::write(prog.join("web/index.html"), "<html></html>").unwrap();
    let out = pcds_at(&prog.join("pcds"), &dir, &fake, &["restart"]);
    let calls = std::fs::read_to_string(&log).unwrap();
    let (reset, restart) = (calls.find("reset-failed pcds-cli-itest.service"), calls.find("restart pcds-cli-itest.service"));
    assert!(reset.is_some() && restart.is_some() && reset < restart, "{calls}\n{}", text(&out));
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

/// A name in server.env that is no setting stops the start in one line that names the file and
/// the key (never a value), at `pcds run` and before `pcds restart` asks systemd.
#[test]
fn an_unknown_setting_stops_the_start() {
    let dir = test_dir("unknown");
    let fake = fake_systemctl(&dir, "echo inactive");
    std::fs::create_dir_all(dir.join("run")).unwrap();
    // Names of settings that do not exist (not spelled out: the guard test of config.rs reads this file).
    let (autosave, backups) = (concat!("PCDS_", "AUTOSAVE"), concat!("PCDS_", "BACKUPS"));
    std::fs::write(dir.join("run/server.env"), format!("PCDS_ADMIN_TOKEN=s3cret-token\n{autosave}=10\n{backups}=5\n")).unwrap();
    let out = pcds(&dir, &fake, &["run"]);
    let said = text(&out);
    assert_eq!(out.status.code(), Some(1), "{said}");
    assert!(said.contains(&format!("{}: unknown setting {autosave}, {backups}", dir.join("run/server.env").display())), "{said}");
    assert!(!said.contains("s3cret-token") && said.trim().lines().count() == 1, "one line, no secrets: {said}");
    std::fs::create_dir_all(dir.join("home/.config/systemd/user")).unwrap();
    std::fs::write(dir.join("home/.config/systemd/user/pcds-cli-itest.service"), "").unwrap();
    let out = pcds(&dir, &fake, &["restart"]);
    assert!(text(&out).contains(&format!("unknown setting {autosave}")) && out.status.code() == Some(1), "{}", text(&out));
    assert!(said.contains(&format!("{autosave} is the rule save.autosaveMinutes now")), "{said}");
    // Without them the start goes on, to the missing world.
    std::fs::write(dir.join("run/server.env"), "PCDS_ADMIN_TOKEN=s3cret-token\n").unwrap();
    assert!(text(&pcds(&dir, &fake, &["run"])).contains("no world at"));
    // A retired setting in the process environment (a compose file's environment:) is no error, but is said.
    let out = Command::new(PCDS).arg("run").env("PCDS_HOME", dir.join("run")).env("PCDS_UNIT", "pcds-cli-itest").env("HOME", dir.join("home")).env("PATH", format!("{}:{}", fake.display(), std::env::var("PATH").unwrap_or_default())).env(backups, "0").output().unwrap();
    assert!(text(&out).contains(&format!("ignored in the environment: {backups} is the rule save.backupsKept now")) && text(&out).contains("no world at"), "{}", text(&out));
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

/// `pcds plugin ARGS` in the folder `cwd`, in the installation dir/run.
fn plugin(dir: &Path, cwd: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(PCDS);
    cmd.arg("plugin").args(args).current_dir(cwd).env("PCDS_HOME", dir.join("run")).env("HOME", dir.join("home")).env_remove("XDG_CONFIG_HOME").env_remove("PCDS_CONTAINER").env_remove("PCDS_UNIT");
    for _ in 0..100 {
        match cmd.output() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => std::thread::sleep(std::time::Duration::from_millis(20)),
            out => return out.unwrap(),
        }
    }
    panic!("text file busy")
}

#[test]
fn a_publisher_signs_a_plugin_and_an_admin_trusts_the_key_instead_of_the_digest() {
    let dir = test_dir("plugin-sign");
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(dir.join("home")).unwrap();
    assert!(plugin(&dir, &work, &["new", "demo", "--kind", "data"]).status.success());
    // The publisher makes a key (the private half is for nobody else) and signs.
    let out = plugin(&dir, &work, &["keygen", "acme"]);
    assert!(out.status.success(), "{}", text(&out));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(work.join("acme.key")).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(!plugin(&dir, &work, &["keygen", "acme"]).status.success(), "no key is overwritten");
    let out = plugin(&dir, &work, &["sign", "--key", "acme.key", "demo"]);
    assert!(out.status.success() && work.join("demo/plugin.sig").is_file(), "{}", text(&out));
    let out = plugin(&dir, &work, &["check", "demo"]);
    assert!(text(&out).contains("signed by the key") && text(&out).contains("which you do not trust"), "{}", text(&out));
    assert!(plugin(&dir, &work, &["install", "demo"]).status.success());
    assert!(dir.join("run/plugins/demo/plugin.sig").is_file(), "the signature is installed with the files");
    // Not trusted: pending, and the admin is told who signed.
    let out = plugin(&dir, &work, &["enable", "demo", "--grant", "chat.say", "--yes"]);
    assert!(text(&out).contains("which you do not trust"), "{}", text(&out));
    assert!(plugin(&dir, &work, &["disable", "demo"]).status.success());
    std::fs::remove_file(dir.join("run/plugins/approved.json")).unwrap();
    assert!(text(&plugin(&dir, &work, &["list"])).contains("pending"));
    // Trusting the key for what the plugin asks: it runs with no approval.
    let out = plugin(&dir, &work, &["trust", "acme.pub", "--name", "Acme", "--grant", "event.chat,event.player,chat.say"]);
    assert!(out.status.success() && text(&out).contains("trusting Acme"), "{}", text(&out));
    let listed = text(&plugin(&dir, &work, &["list"]));
    assert!(listed.contains("running") && listed.contains("signed by Acme"), "{listed}");
    assert!(!dir.join("run/plugins/approved.json").exists(), "nothing was approved: the key vouched");
    let keys = text(&plugin(&dir, &work, &["keys"]));
    assert!(keys.contains("Acme") && keys.contains("event.chat event.player chat.say"), "{keys}");
    // A file changed after signing: the signature no longer holds; the plugin waits.
    std::fs::write(dir.join("run/plugins/demo/rules.toml"), "[[command]]\nname = \"hello\"\nreply = \"changed\"\n").unwrap();
    let listed = text(&plugin(&dir, &work, &["list"]));
    assert!(listed.contains("pending") && listed.contains("other files than these"), "{listed}");
    // The key is taken away and cannot come back by accident.
    std::fs::copy(work.join("demo/rules.toml"), dir.join("run/plugins/demo/rules.toml")).unwrap();
    assert!(text(&plugin(&dir, &work, &["list"])).contains("running"));
    let id = keys.split_whitespace().next().unwrap().to_string();
    let out = plugin(&dir, &work, &["untrust", &id[..12]]);
    assert!(out.status.success() && text(&out).contains("no longer trusted"), "{}", text(&out));
    let listed = text(&plugin(&dir, &work, &["list"]));
    assert!(listed.contains("pending") && listed.contains("which was revoked"), "{listed}");
    assert!(text(&plugin(&dir, &work, &["keys"])).contains("REVOKED"));
    let out = plugin(&dir, &work, &["trust", "acme.pub"]);
    assert!(!out.status.success() && text(&out).contains("--force"), "{}", text(&out));
    assert!(plugin(&dir, &work, &["trust", "acme.pub", "--force"]).status.success());
    // A private key that is not one, a plugin that is not one.
    assert!(!plugin(&dir, &work, &["sign", "--key", "acme.pub", "demo"]).status.success());
    assert!(!plugin(&dir, &work, &["sign", "--key", "acme.key", "no-such-folder"]).status.success());
}

#[test]
fn a_scoped_token_is_made_shown_once_listed_and_revoked_from_the_command_line() {
    let dir = test_dir("plugin-token");
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(dir.join("home")).unwrap();
    assert!(text(&plugin(&dir, &work, &["tokens"])).contains("no scoped tokens"));
    let out = plugin(&dir, &work, &["token", "create", "relay", "--scope", "events.read,chat.send", "--rate", "90"]);
    let shown = text(&out);
    assert!(out.status.success() && shown.contains("shown this once"), "{shown}");
    let token = shown.split_whitespace().find(|w| w.starts_with("pcdst_")).unwrap().to_string();
    let listed = text(&plugin(&dir, &work, &["tokens"]));
    assert!(listed.contains("relay") && listed.contains("90/min") && listed.contains("chat.send events.read") && !listed.contains(&token), "{listed}");
    assert!(!std::fs::read_to_string(dir.join("run/plugins/tokens.json")).unwrap().contains(&token));
    // Wrong forms: no scope, an unknown scope, the same name again.
    for args in [vec!["token", "create", "x"], vec!["token", "create", "x", "--scope", "root"], vec!["token", "create", "relay", "--scope", "events.read"], vec!["token", "create", "x", "--scope", "events.read", "--rate", "0"]] {
        assert!(!plugin(&dir, &work, &args).status.success(), "{args:?}");
    }
    let out = plugin(&dir, &work, &["token", "revoke", "relay"]);
    assert!(out.status.success() && text(&out).contains("is revoked"), "{}", text(&out));
    assert!(!plugin(&dir, &work, &["token", "revoke", "relay"]).status.success());
    assert!(text(&plugin(&dir, &work, &["tokens"])).contains("no scoped tokens"));
}

#[test]
fn a_plugins_config_values_are_set_without_the_command_line_and_a_secret_is_never_shown() {
    use std::io::Write;
    let dir = test_dir("plugin-config");
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(dir.join("home")).unwrap();
    assert!(plugin(&dir, &work, &["new", "demo", "--kind", "data"]).status.success());
    let manifest = work.join("demo/plugin.toml");
    let text_of = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(&manifest, format!("{text_of}\n[[config]]\nkey = \"hook\"\ndescription = \"a webhook\"\n\n[[config]]\nkey = \"region\"\nsecret = false\n")).unwrap();
    assert!(plugin(&dir, &work, &["install", "demo"]).status.success());
    // Approving says which values the plugin needs.
    let out = plugin(&dir, &work, &["enable", "demo", "--yes"]);
    assert!(text(&out).contains("needs a value you set: pcds plugin config set demo hook") && text(&out).contains("a secret"), "{}", text(&out));
    // A secret is not taken from the command line; a key the manifest does not declare is refused.
    let out = plugin(&dir, &work, &["config", "set", "demo", "hook", "https://example.org/hook/abc"]);
    assert!(!out.status.success() && text(&out).contains("is a secret"), "{}", text(&out));
    assert!(!dir.join("run/plugins/config.json").exists());
    let out = plugin(&dir, &work, &["config", "set", "demo", "nothing", "x"]);
    assert!(!out.status.success() && text(&out).contains("declares no config key nothing"), "{}", text(&out));
    // From standard input (no terminal here).
    let mut cmd = Command::new(PCDS);
    cmd.args(["plugin", "config", "set", "demo", "hook"]).current_dir(&work).env("PCDS_HOME", dir.join("run")).env("HOME", dir.join("home")).env_remove("PCDS_CONTAINER").env_remove("PCDS_UNIT").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"https://example.org/hook/abc\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(!text(&out).contains("hook/abc"), "the value is not echoed: {}", text(&out));
    assert!(plugin(&dir, &work, &["config", "set", "demo", "region", "eu-1"]).status.success());
    let out = plugin(&dir, &work, &["config", "list", "demo"]);
    let listed = text(&out);
    assert!(listed.contains("demo hook: set (a secret: not shown)") && listed.contains("demo region: eu-1") && !listed.contains("hook/abc"), "{listed}");
    let file = dir.join("run/plugins/config.json");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(std::fs::read_to_string(&file).unwrap().contains("hook/abc"));
    assert!(plugin(&dir, &work, &["config", "unset", "demo", "hook"]).status.success());
    assert!(text(&plugin(&dir, &work, &["config", "list"])).contains("demo hook: not set"));
}

#[test]
fn a_plugin_is_made_checked_installed_approved_and_listed_without_a_server() {
    let dir = test_dir("plugin");
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(dir.join("home")).unwrap();
    let out = plugin(&dir, &work, &["new", "demo", "--kind", "data"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(work.join("demo/plugin.toml").is_file() && work.join("demo/rules.toml").is_file());
    let out = plugin(&dir, &work, &["check", "demo"]);
    assert!(out.status.success() && text(&out).contains("command sample: ok"), "{}", text(&out));
    // From inside the folder, as the guide does.
    let out = plugin(&dir, &work.join("demo"), &["check", "."]);
    assert!(out.status.success() && text(&out).contains("command sample: ok"), "{}", text(&out));
    let out = plugin(&dir, &work, &["install", "demo"]);
    assert!(out.status.success() && text(&out).contains("it does nothing until you approve it"), "{}", text(&out));
    assert!(dir.join("run/plugins/demo/plugin.toml").is_file() && !dir.join("run/plugins/approved.json").exists());
    // Installed twice needs --force; listed as waiting for approval.
    assert!(!plugin(&dir, &work, &["install", "demo"]).status.success());
    let out = plugin(&dir, &work, &["list"]);
    assert!(text(&out).contains("demo") && text(&out).contains("pending"), "{}", text(&out));
    // Approving needs an answer: no terminal here, so --yes.
    let out = plugin(&dir, &work, &["enable", "demo"]);
    assert!(!out.status.success() && text(&out).contains("--yes"), "{}", text(&out));
    let out = plugin(&dir, &work, &["enable", "demo", "--grant", "player.kick", "--yes"]);
    assert!(!out.status.success() && text(&out).contains("does not ask for player.kick"), "{}", text(&out));
    let out = plugin(&dir, &work, &["enable", "demo", "--yes"]);
    assert!(out.status.success() && text(&out).contains("grant: chat.say"), "{}", text(&out));
    assert!(text(&plugin(&dir, &work, &["list"])).contains("running"));
    // A file changed after the approval waits for the admin again.
    std::fs::write(dir.join("run/plugins/demo/rules.toml"), "[[command]]\nname = \"hello\"\nreply = \"changed\"\n").unwrap();
    assert!(text(&plugin(&dir, &work, &["list"])).contains("changed"));
    assert!(plugin(&dir, &work, &["enable", "demo", "--yes"]).status.success());
    assert!(plugin(&dir, &work, &["disable", "demo"]).status.success());
    assert!(text(&plugin(&dir, &work, &["list"])).contains("disabled"));
    // A plugin that does not pass the check is not installed.
    std::fs::write(work.join("demo/rules.toml"), "[[command]]\nname = \"x\"\nreply = \"{nope}\"\n").unwrap();
    let out = plugin(&dir, &work, &["check", "demo"]);
    assert!(!out.status.success() && text(&out).contains("{nope} is not a variable"), "{}", text(&out));
    assert!(!plugin(&dir, &work, &["install", "demo", "--force"]).status.success());
}

#[test]
fn the_wasm_template_builds_against_the_api_crate() {
    let dir = test_dir("plugin-template");
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(dir.join("home")).unwrap();
    let api = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin-api");
    let out = plugin(&dir, &work, &["new", "my-plugin", "--api-path", api.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out));
    let made = work.join("my-plugin");
    for f in ["Cargo.toml", "plugin.toml", "src/lib.rs", ".cargo/config.toml", "README.md"] {
        assert!(made.join(f).is_file(), "{f}");
    }
    let toml = std::fs::read_to_string(made.join("Cargo.toml")).unwrap();
    assert!(toml.contains("name = \"my_plugin\"") && toml.contains("pcds-plugin-api = { path = "), "{toml}");
    // The template compiles (for this machine: the wasm target is not needed to know it is right).
    std::fs::remove_dir_all(made.join(".cargo")).unwrap();
    let check = Command::new("cargo").args(["check", "--offline", "--manifest-path"]).arg(made.join("Cargo.toml")).env("CARGO_TARGET_DIR", dir.join("target")).output().unwrap();
    assert!(check.status.success(), "{}", text(&check));
    // The id must be one.
    assert!(!plugin(&dir, &work, &["new", "Bad Name"]).status.success());
}

/// Help is asked for and answered (exit 0); a mistyped command or an argument a command does not
/// take is named, not answered with the whole help; nothing set up yet says to run `pcds up`.
#[test]
fn mistakes_are_answered_in_words() {
    let dir = test_dir("mistakes");
    let fake = fake_systemctl(&dir, "echo inactive");
    for ask in [&["help"][..], &["--help"], &["-h"]] {
        let out = pcds(&dir, &fake, ask);
        assert_eq!(out.status.code(), Some(0), "{ask:?}");
        assert!(text(&out).contains("pcds up") && text(&out).contains("pcds rules"), "{}", text(&out));
    }
    let out = pcds(&dir, &fake, &["frobnicate"]);
    assert_eq!((out.status.code(), text(&out).trim().lines().count()), (Some(1), 1), "{}", text(&out));
    assert!(text(&out).contains("unknown command 'frobnicate'") && text(&out).contains("pcds help"), "{}", text(&out));
    let out = pcds(&dir, &fake, &["status", "--bogus"]);
    assert!(text(&out).contains("pcds status: cannot use '--bogus'") && text(&out).trim().lines().count() == 1, "{}", text(&out));
    assert!(text(&pcds(&dir, &fake, &["kick"])).contains("usage: pcds kick PLAYER"));
    assert!(text(&pcds(&dir, &fake, &["api", "FOO", "/api/status"])).contains("takes GET and POST") || text(&pcds(&dir, &fake, &["api", "FOO", "/api/status"])).contains("no server is set up"));
    // Nothing is set up here: the commands that need a server say how to get one.
    for ask in [&["players"][..], &["rules"], &["save"], &["gui"], &["token"], &["config"]] {
        let out = pcds(&dir, &fake, ask);
        assert_eq!(out.status.code(), Some(1), "{ask:?}");
        assert!(text(&out).contains("no server is set up here") && text(&out).contains("pcds up"), "{ask:?}: {}", text(&out));
    }
    assert!(text(&pcds(&dir, &fake, &["world", "new", "bad/name"])).contains("is not a world name"));
    assert!(text(&pcds(&dir, &fake, &["setup", "--world", "/nonexistent/save.json"])).contains("no save at /nonexistent/save.json"));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A direct-IP port another program holds stops the start with a sentence, not a panic.
#[test]
fn a_port_in_use_stops_the_start_in_words() {
    let dir = test_dir("port");
    std::fs::create_dir_all(&dir).unwrap();
    let held = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let out = Command::new(PCDS).arg("serve").arg(dir.join("no-tables")).arg(dir.join("world.json")).env("PCDS_LISTEN", held.local_addr().unwrap().to_string()).env("PCDS_STEAM", "off").env("PCDS_ADMIN", "off").output().unwrap();
    let said = text(&out);
    assert_eq!(out.status.code(), Some(1), "{said}");
    assert!(said.contains("direct IP 127.0.0.1:") && said.contains("PCDS_LISTEN") && !said.contains("panicked"), "{said}");
    std::fs::remove_dir_all(&dir).unwrap();
}
