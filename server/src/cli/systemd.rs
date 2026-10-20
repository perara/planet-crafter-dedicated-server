//! The systemd user units: the server (`pcds run`: the front, which starts the bank) and the
//! hourly game update check.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::Command;

use super::{check, output, quiet, say, Ctx, Result};

impl Ctx {
    pub fn unit(&self) -> String {
        format!("{}.service", self.unit_name)
    }

    pub(super) fn timer(&self) -> String {
        format!("{}-update.timer", self.unit_name)
    }

    pub fn systemctl(&self, args: &[&str]) -> Command {
        let mut c = Command::new("systemctl");
        c.arg("--user").args(args);
        c
    }

    /// Whether this installation's server runs, or is still starting or stopping (its last save
    /// may be under way): the unit is this installation's when it reads this settings file; in
    /// the container, the server is a `pcds serve` of this user.
    /// Err when systemctl itself fails (no user session, no D-Bus): that is not a stopped server.
    pub fn active(&self) -> Result<bool> {
        if self.container {
            return Ok(self.server_pid().is_some());
        }
        Ok(running(&self.show("ActiveState")?) && self.ours()?)
    }

    /// The unit is this installation's: it reads this settings file.
    fn ours(&self) -> Result<bool> {
        Ok(self.show("EnvironmentFiles")?.contains(&*self.env_file.to_string_lossy()))
    }

    /// A property of the server's unit (systemctl show); Err with what systemctl said when it
    /// fails.
    pub(super) fn show(&self, property: &str) -> Result<String> {
        let mut cmd = self.systemctl(&["show", "-p", property, "--value", &self.unit()]);
        let out = cmd.stdin(std::process::Stdio::null()).output().map_err(|e| format!("{}: {e}", super::describe(&cmd)))?;
        if !out.status.success() {
            return Err(format!("{} failed ({}): {}", super::describe(&cmd), out.status, String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    }

    /// Err when the unit file on disk is another installation's (it reads another settings
    /// file): writing or running it would take that server over.
    pub fn check_owner(&self) -> Result {
        let Ok(text) = fs::read_to_string(self.unit_dir.join(self.unit())) else { return Ok(()) };
        let theirs = text.lines().find_map(|l| l.strip_prefix("EnvironmentFile=")).map(|v| v.trim_start_matches('-').replace("%%", "%"));
        match theirs {
            Some(path) if path != self.env_file.to_string_lossy() => Err(format!("unit {} belongs to {path}; set PCDS_UNIT to give this installation units of its own", self.unit())),
            _ => Ok(()),
        }
    }

    /// The program the units run: run/bin/pcds, which a rebuild, `make clean` or a removed
    /// release folder leaves alone. It starts the front from the active bank (run/bin/a, b).
    pub fn installed(&self) -> PathBuf {
        self.run.join("bin/pcds")
    }

    /// The banks: run/bin/a, run/bin/b and run/bin/active (updater.rs).
    pub fn banks(&self) -> PathBuf {
        self.run.join("bin")
    }

    /// The pid of the server, from systemd, or in the container from /proc.
    pub fn server_pid(&self) -> Option<String> {
        if !self.container {
            return output(&mut self.systemctl(&["show", "-p", "MainPID", "--value", &self.unit()]));
        }
        // SAFETY: getuid only reads.
        let uid = unsafe { libc::getuid() };
        fs::read_dir("/proc").ok()?.flatten().find_map(|e| {
            let pid = e.file_name().to_string_lossy().to_string();
            let cmdline = fs::read(e.path().join("cmdline")).ok()?;
            let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
            let server = args.first().is_some_and(|a| a.ends_with(b"/pcds") || *a == b"pcds") && args.get(1) == Some(&&b"serve"[..]);
            (server && pid.bytes().all(|b| b.is_ascii_digit()) && e.metadata().ok()?.uid() == uid).then_some(pid)
        })
    }

    /// Whether systemd can give a user service a mount namespace here (ProtectSystem=, PrivateTmp=
    /// and the like need one, made through an unprivileged user namespace, which some machines
    /// forbid: the unit would then fail to start with status 226/NAMESPACE). Tried once per
    /// process with a throw-away unit that does nothing. Err says why not.
    pub fn sandbox_check(&self) -> std::result::Result<(), String> {
        static PROBED: std::sync::OnceLock<std::result::Result<(), String>> = std::sync::OnceLock::new();
        PROBED
            .get_or_init(|| {
                // The unit tests do not start units on the machine they run on.
                if cfg!(test) {
                    return Ok(());
                }
                if !super::have("systemd-run") {
                    return Err("systemd-run is not installed".into());
                }
                let mut probe = Command::new("systemd-run");
                probe.args(["--user", "--wait", "--collect", "--quiet", "-p", "ProtectSystem=strict", "-p", "ProtectHome=read-only", "-p", "PrivateTmp=yes", "-p", "PrivateDevices=yes", "-p", "ProtectKernelTunables=yes", "-p", "ProtectControlGroups=yes", "-p", "ProtectHostname=yes"]);
                probe.arg(&self.exe).arg("--version").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
                let mut child = probe.spawn().map_err(|e| format!("systemd-run: {e}"))?;
                let stderr = child.stderr.take();
                match super::wait_for(&mut child, std::time::Duration::from_secs(30)) {
                    Some(status) if status.success() => Ok(()),
                    Some(status) => {
                        let said = stderr.map(|mut s| { let mut t = String::new(); let _ = std::io::Read::read_to_string(&mut s, &mut t); t }).unwrap_or_default();
                        Err(format!("a test unit with the sandbox did not run ({status}): {}", said.trim().lines().last().unwrap_or("no reason given")))
                    }
                    None => Err("a test unit with the sandbox did not run within 30 s".into()),
                }
            })
            .clone()
    }

    /// The units' files: the server, the update check and its timer.
    pub(super) fn unit_files(&self) -> Result<[(String, String); 3]> {
        self.unit_files_with(self.sandbox_check().is_ok())
    }

    /// The units' files, with the mount sandbox (`mounts`) or without it.
    pub(super) fn unit_files_with(&self, mounts: bool) -> Result<[(String, String); 3]> {
        let (run, env, exe) = (quoted(&self.run.display().to_string())?, plain(&self.env_file.display().to_string())?, quoted(&self.installed().display().to_string())?.replace('$', "$$"));
        let sandbox = sandbox_options(&run, self.run.starts_with("/tmp") || self.run.starts_with("/var/tmp"), mounts);
        let light = SANDBOX_COMMON;
        let unit = quoted(&self.unit_name)?;
        let environment = format!("Environment=\"PCDS_HOME={run}\" \"PCDS_UNIT={unit}\"");
        Ok([
            (
                self.unit(),
                format!(
                    "[Unit]
Description=Planet Crafter dedicated server
After=network-online.target
# A server that keeps crashing stops after 5 starts in 10 minutes.
StartLimitIntervalSec=600
StartLimitBurst=5

[Service]
Type=simple
{environment}
EnvironmentFile={env}
ExecStart=\"{exe}\" run
Restart=on-failure
RestartSec=15
# The stop (SIGTERM) goes to pcds run alone, which passes it to the front; the front has
# its bank save the world and end. When the main process ends, systemd kills the rest of
# the unit at once: pcds run therefore ends only after the front and its banks (the front
# gives its bank 60 s to save and end; a front that crashed leaves its banks 20 s more).
# What is still there after the timeout is killed.
KillMode=mixed
TimeoutStopSec=90
{sandbox}
[Install]
WantedBy=default.target
"
                ),
            ),
            (
                format!("{}-update.service", self.unit_name),
                format!(
                    "[Unit]
Description=Planet Crafter dedicated server: pick up game updates

[Service]
Type=oneshot
{environment}
TimeoutStartSec=1h
ExecStart=\"{exe}\" update
# steamcmd keeps its login and the game in ~ and run/: the sandbox without a read-only file system.
{light}"
                ),
            ),
            (
                self.timer(),
                "[Unit]
Description=Planet Crafter dedicated server: hourly game update check

[Timer]
OnBootSec=10min
OnUnitActiveSec=1h

[Install]
WantedBy=timers.target
"
                .into(),
            ),
        ])
    }

    /// The units, for the program installed in run/bin. Refused over another installation's.
    pub fn write_unit(&self) -> Result {
        self.check_owner()?;
        super::mkdir(&self.unit_dir)?;
        let sandbox = self.sandbox_check();
        for (name, text) in self.unit_files_with(sandbox.is_ok())? {
            super::write(&self.unit_dir.join(name), text)?;
        }
        if let Err(why) = sandbox {
            say(format!("this machine does not let a user service have its own file system view ({why}), so the unit has the sandbox options that need none: docs/hardening.md says how to allow it"));
        }
        check(&mut self.systemctl(&["daemon-reload"]))
    }

    /// The units are written again when they differ from what this release writes.
    pub fn refresh_unit(&self) -> Result {
        let files = self.unit_files()?;
        if files.iter().any(|(name, text)| fs::read_to_string(self.unit_dir.join(name)).ok().as_deref() != Some(text.as_str())) {
            self.write_unit()?;
            say("the systemd units were written again for this version");
        }
        Ok(())
    }

    pub fn install_service(&self) -> Result {
        if !self.world_file().is_file() {
            return Err("no world yet: 'pcds up' sets a server up ('pcds setup' first, then 'pcds install-service', does the same in steps)".into());
        }
        self.install_server()?;
        self.write_unit()?;
        check(&mut self.systemctl(&["enable", "--now", &self.unit(), &self.timer()]))?;
        say("the server now starts at login and checks for game updates hourly");
        let user = crate::env("USER").unwrap_or_default();
        if output(Command::new("loginctl").args(["show-user", &user, "-p", "Linger", "--value"])).as_deref() != Some("yes") {
            say(format!("to keep it running while you are logged out: sudo loginctl enable-linger {user}"));
        }
        Ok(())
    }

    pub fn uninstall_service(&self) -> Result {
        quiet(&mut self.systemctl(&["disable", "--now", &self.unit(), &self.timer()]));
        for name in [self.unit(), format!("{}-update.service", self.unit_name), self.timer()] {
            let _ = fs::remove_file(self.unit_dir.join(name));
        }
        check(&mut self.systemctl(&["daemon-reload"]))?;
        say("removed the systemd units (run/ is kept)");
        Ok(())
    }

    /// After a settings change: the server picks it up.
    pub fn restart_if_running(&self) -> Result {
        if self.container {
            say("restart the container for it to take effect (docker restart CONTAINER)");
        } else if self.active()? {
            // A release that writes the unit differently (the sandbox, the limits) is in force from this start.
            self.refresh_unit()?;
            // systemd counts every start toward the unit's limit (5 in 10 minutes): an upgrade, a
            // rollback and an upgrade again must not leave the server failed and down.
            quiet(&mut self.systemctl(&["reset-failed", &self.unit()]));
            check(&mut self.systemctl(&["restart", &self.unit()]))?;
            say("server restarted");
        }
        Ok(())
    }

    /// In the container, `pcds upgrade --restart`: there is no unit to restart, so the server's
    /// supervisor (`pcds container`, the front's parent) is asked (SIGUSR1) to stop the server
    /// and start it again as the same process: the front saves and stops the bank, and the
    /// new start runs the active bank's front. Waits until the server answers again, and
    /// says which release runs. Err when the front has no such parent (it is not started by
    /// `pcds container`): nothing is restarted, and the message says what to do.
    pub fn restart_in_place(&self) -> Result {
        let front = self.server_pid().ok_or("the server is not running")?;
        let Some(supervisor) = supervisor_of(&front) else {
            return Err("this server is not started by 'pcds container' (the image's entrypoint), which is what restarts it in place: restart the container to run the new release (docker restart CONTAINER)".into());
        };
        // SAFETY: SIGUSR1 to the supervisor of this container's server, by the pid /proc names.
        if unsafe { libc::kill(supervisor, libc::SIGUSR1) } != 0 {
            return Err(format!("cannot signal the supervisor (pid {supervisor}): {}; restart the container instead (docker restart CONTAINER)", std::io::Error::last_os_error()));
        }
        say(format!("restarting the server in place (pid {front} stops, the world is saved)"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
        while std::time::Instant::now() < deadline {
            super::sleep(1);
            if self.server_pid().is_some_and(|now| now != front) && let Ok(status) = self.api_json("GET", "/api/status") {
                let live = &status["banks"]["live"];
                say(format!("the server is back: bank {} runs the world (pcds {})", live["bank"].as_str().unwrap_or("?"), status["serverVersion"].as_str().unwrap_or("?")));
                return Ok(());
            }
        }
        Err("the server did not come back within 10 minutes (docker logs CONTAINER)".into())
    }

    /// After `pcds update` made new tables: the server restarted on them, unless it is stopped.
    pub fn restart_after_update(&self) -> Result {
        if self.container {
            say("restart the container for the server to run on the new tables (docker restart CONTAINER)");
            return Ok(());
        }
        if !starts_after_update(&self.show("ActiveState")?) || !self.ours()? {
            return Ok(());
        }
        // Failing on the old tables may have used up its start limit.
        quiet(&mut self.systemctl(&["reset-failed", &self.unit()]));
        check(&mut self.systemctl(&["restart", &self.unit()]))?;
        say("server restarted on the new tables");
        Ok(())
    }

    /// Up for the first time: the units enabled, the server (re)started, kept running while
    /// logged out. False without a systemd user session.
    pub fn start_service(&self) -> Result<bool> {
        if self.container || !quiet(&mut self.systemctl(&["show-environment"])) {
            return Ok(false);
        }
        check(self.systemctl(&["enable", &self.unit(), &self.timer()]).stderr(std::process::Stdio::null()))?;
        check(&mut self.systemctl(&["restart", &self.unit()]))?;
        check(&mut self.systemctl(&["start", &self.timer()]))?;
        let user = crate::env("USER").unwrap_or_default();
        if quiet(Command::new("loginctl").args(["enable-linger", &user])) {
            say("the server starts with the machine and keeps running while you are logged out");
        } else {
            say(format!("to keep it running while you are logged out: sudo loginctl enable-linger {user}"));
        }
        Ok(true)
    }
}

/// What the server's unit and the update check's unit both carry: nothing here needs a mount
/// namespace, so it holds on every machine. Tried on systemd 262 with the real server and with
/// steamcmd (docs/hardening.md): NoNewPrivileges, the socket families a server uses (AF_NETLINK for
/// the network interface list Steam and many tools read), no realtime scheduling, no setuid
/// files, no namespaces, no personality changes, the service system calls (a refused call returns
/// EPERM), files private to the user. Not CapabilityBoundingSet=: a user manager can drop
/// capabilities only inside a user namespace, which the mount options bring, and the unit would
/// fail to start (status 218/CAPABILITIES) without one; a user's service has none to lose.
const SANDBOX_COMMON: &str = "NoNewPrivileges=yes
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
SystemCallFilter=@system-service
SystemCallErrorNumber=EPERM
UMask=0077
";

/// The sandbox of the server's unit: SANDBOX_COMMON, the limits, and with `mounts` the file
/// system and kernel protections that need a mount namespace (and the user namespace it comes in, which is
/// also what lets the capabilities be dropped): everything outside `run` read-only
/// (`run` is the path, already quoted), $HOME readable and not writable, a private /tmp (left
/// out when `run` itself is under /tmp), no devices, no writing /proc/sys, /sys or the cgroups.
fn sandbox_options(run: &str, run_in_tmp: bool, mounts: bool) -> String {
    let mut out = String::from("# The sandbox (docs/hardening.md).\n");
    out += SANDBOX_COMMON;
    out += "LimitNOFILE=65536\nTasksMax=1024\n";
    if mounts {
        out += &format!("CapabilityBoundingSet=\nProtectSystem=strict\nReadWritePaths=\"{run}\"\nProtectHome=read-only\n");
        out += if run_in_tmp { "" } else { "PrivateTmp=yes\n" };
        out += "PrivateDevices=yes\nProtectKernelTunables=yes\nProtectKernelModules=yes\nProtectKernelLogs=yes\nProtectControlGroups=yes\nProtectClock=yes\nProtectHostname=yes\n";
    }
    out
}

/// The pid of the supervisor of the server `front` (its parent) when that is `pcds container`,
/// which restarts it in place.
fn supervisor_of(front: &str) -> Option<i32> {
    let stat = fs::read_to_string(format!("/proc/{front}/stat")).ok()?;
    // pid (comm) state ppid ...: comm may hold spaces and parentheses.
    let ppid: i32 = stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()?;
    let cmdline = fs::read(format!("/proc/{ppid}/cmdline")).ok()?;
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    let ours = args.first().is_some_and(|a| a.ends_with(b"/pcds") || *a == b"pcds") && args.get(1) == Some(&&b"container"[..]);
    ours.then_some(ppid)
}

/// Whether a unit in this ActiveState runs (or is starting, stopping or reloading).
fn running(active_state: &str) -> bool {
    !matches!(active_state, "inactive" | "failed" | "")
}

/// A value inside a double-quoted unit setting: specifiers (%) taken literally, quotes and
/// backslashes escaped; a line break is refused. (ExecStart= also expands $: its caller doubles
/// those.)
fn quoted(value: &str) -> Result<String> {
    if value.contains(['\n', '\r']) {
        return Err(format!("{value:?}: a line break in a path systemd units cannot hold"));
    }
    Ok(value.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%"))
}

/// A value of an unquoted unit setting (EnvironmentFile=): specifiers taken literally.
fn plain(value: &str) -> Result<String> {
    if value.contains(['\n', '\r']) || value.starts_with(['-', ' ']) {
        return Err(format!("{value:?}: a path systemd units cannot hold"));
    }
    Ok(value.replace('%', "%%"))
}

/// Whether `pcds update` (re)starts the server on new tables, by the unit's ActiveState: a
/// server stopped by hand stays stopped; one that failed on the old tables (after a reboot
/// following a game update), or is starting or running, is started.
fn starts_after_update(active_state: &str) -> bool {
    active_state != "inactive"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stopped_server_stays_stopped_after_an_update() {
        assert!(!starts_after_update("inactive"));
        for state in ["active", "failed", "activating", "deactivating", "reloading"] {
            assert!(starts_after_update(state), "{state}");
        }
    }

    #[test]
    fn starting_and_stopping_count_as_running() {
        for state in ["active", "activating", "deactivating", "reloading"] {
            assert!(running(state), "{state}");
        }
        for state in ["inactive", "failed", ""] {
            assert!(!running(state), "{state}");
        }
    }

    #[test]
    fn unit_values_are_escaped() {
        assert_eq!(quoted(r#"/srv/50%"a\b"$HOME"#).unwrap(), r#"/srv/50%%\"a\\b\"$HOME"#);
        assert_eq!(plain("/srv/50%/server.env").unwrap(), "/srv/50%%/server.env");
        assert!(quoted("/srv/a\nb").is_err() && plain("-/x").is_err());
    }

    /// Another installation's unit of the same name is refused; a unit of this installation
    /// that differs is written again.
    #[test]
    fn units_of_another_installation_are_left_alone() {
        let dir = super::super::test_dir("unit-owner");
        let ctx = super::super::test_ctx(&dir);
        fs::create_dir_all(&ctx.unit_dir).unwrap();
        let path = ctx.unit_dir.join(ctx.unit());
        fs::write(&path, "[Service]\nEnvironmentFile=/srv/other/run/server.env\nExecStart=/srv/other/pcds run\n").unwrap();
        assert!(ctx.check_owner().unwrap_err().contains("belongs to /srv/other/run/server.env"));
        assert!(ctx.write_unit().is_err());
        assert!(fs::read_to_string(&path).unwrap().contains("/srv/other"), "not overwritten");
        fs::write(&path, format!("[Service]\nEnvironmentFile={}\nExecStart=/gone/pcds run\n", ctx.env_file.display())).unwrap();
        assert!(ctx.check_owner().is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_units_run_the_installed_program() {
        let dir = super::super::test_dir("units");
        let ctx = super::super::test_ctx(&dir);
        let units = ctx.unit_files().unwrap();
        let exec = |name: &str| units.iter().find(|(n, _)| n == name).unwrap().1.lines().find_map(|l| l.strip_prefix("ExecStart=")).map(str::to_string);
        let installed = dir.join("run/bin/pcds").display().to_string();
        assert_eq!(exec("pcds-cli-test.service").unwrap(), format!("\"{installed}\" run"));
        assert_eq!(exec("pcds-cli-test-update.service").unwrap(), format!("\"{installed}\" update"));
        assert!(units[0].1.contains(&format!("PCDS_HOME={}", dir.join("run").display())));
        assert_eq!(units[2].0, "pcds-cli-test-update.timer");
        assert!(units[0].1.contains("KillMode=mixed\n"));
        assert!(units[1].1.contains("TimeoutStartSec="));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The sandbox of the server's unit, with and without the mount options; the unit that checks
    /// for game updates gets the part that needs no mount.
    #[test]
    fn the_server_unit_is_sandboxed_and_the_update_unit_lightly() {
        let dir = super::super::test_dir("unit-sandbox");
        let ctx = super::super::test_ctx(&dir);
        let [server, update, _] = ctx.unit_files_with(true).unwrap();
        for option in ["NoNewPrivileges=yes", "ProtectSystem=strict", "ProtectHome=read-only", "PrivateDevices=yes", "ProtectKernelTunables=yes", "ProtectControlGroups=yes", "RestrictNamespaces=yes", "CapabilityBoundingSet=\n", "SystemCallFilter=@system-service", "RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK", "UMask=0077", "LimitNOFILE=65536", "TasksMax=1024"] {
            assert!(server.1.contains(&format!("\n{option}")), "{option}\n{}", server.1);
        }
        // Only the run folder is writable.
        assert!(server.1.contains(&format!("\nReadWritePaths=\"{}\"\n", dir.join("run").display())), "{}", server.1);
        // A run folder under /tmp would be hidden by a private /tmp: that one option is left out then.
        for (run, private) in [("/home/per/pcds/run", true), ("/tmp/pcds/run", false), ("/var/tmp/pcds/run", false), ("/srv/tmpfiles/run", true)] {
            let ctx = Ctx { run: std::path::PathBuf::from(run), ..ctx.clone() };
            assert_eq!(ctx.unit_files_with(true).unwrap()[0].1.contains("\nPrivateTmp=yes\n"), private, "{run}");
        }
        // Not the options that break what the server starts (docs/hardening.md).
        for broken in ["MemoryDenyWriteExecute", "SystemCallArchitectures"] {
            assert!(!server.1.contains(broken) && !update.1.contains(broken), "{broken} breaks node, java and steamcmd");
        }
        // The update check keeps the home folder writable for steamcmd: the options without a mount namespace only.
        assert!(update.1.contains("\nNoNewPrivileges=yes\n") && update.1.contains("\nSystemCallFilter=@system-service\n"));
        for mount in ["ProtectSystem", "ProtectHome", "ReadWritePaths", "PrivateTmp", "PrivateDevices", "ProtectKernel", "CapabilityBoundingSet"] {
            assert!(!update.1.contains(mount), "{mount} in the update unit:\n{}", update.1);
        }
        // A machine that cannot make the namespace gets the options that need none, and no file is half a sandbox.
        let [plain_server, ..] = ctx.unit_files_with(false).unwrap();
        assert!(plain_server.1.contains("\nNoNewPrivileges=yes\n") && plain_server.1.contains("\nLimitNOFILE=65536\n"));
        // (CapabilityBoundingSet= too: a user manager drops capabilities only inside a user namespace, and the unit fails with 218/CAPABILITIES.)
        for mount in ["ProtectSystem", "ProtectHome", "ReadWritePaths", "PrivateTmp", "PrivateDevices", "ProtectKernel", "ProtectControlGroups", "ProtectClock", "ProtectHostname", "CapabilityBoundingSet"] {
            assert!(!plain_server.1.contains(mount), "{mount} needs a mount namespace:\n{}", plain_server.1);
        }
        // Every line is a section, a comment, or Key=value.
        for unit in [&server.1, &update.1, &plain_server.1] {
            assert!(unit.lines().all(|l| l.is_empty() || l.starts_with(['#', '[']) || l.split_once('=').is_some_and(|(k, _)| k.bytes().all(|b| b.is_ascii_alphanumeric()))), "{unit}");
        }
        // The run folder's name is escaped like the other paths of the unit.
        let odd = Ctx { run: std::path::PathBuf::from("/srv/50%/a b\"c/run"), ..ctx.clone() };
        assert!(odd.unit_files_with(true).unwrap()[0].1.contains("ReadWritePaths=\"/srv/50%%/a b\\\"c/run\""), "{}", odd.unit_files_with(true).unwrap()[0].1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The restart in place goes only to `pcds container`, the front's parent.
    #[test]
    fn only_a_pcds_container_supervisor_is_restarted_in_place() {
        // This test's parent is cargo's test harness or a shell, not `pcds container`.
        assert_eq!(supervisor_of(&std::process::id().to_string()), None);
        // No such process.
        assert_eq!(supervisor_of("4294967295"), None);
        assert_eq!(supervisor_of("not-a-pid"), None);
        // (restart_in_place itself is not called: it would signal whichever pcds server of this
        // user runs on the machine running the tests. docker/test.sh runs it in a container.)
    }
}
