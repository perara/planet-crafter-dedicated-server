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

    fn timer(&self) -> String {
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
    fn show(&self, property: &str) -> Result<String> {
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

    /// The units' files: the server, the update check and its timer.
    fn unit_files(&self) -> Result<[(String, String); 3]> {
        let (run, env, exe) = (quoted(&self.run.display().to_string())?, plain(&self.env_file.display().to_string())?, quoted(&self.installed().display().to_string())?.replace('$', "$$"));
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
# the unit at once: pcds run therefore ends only after the front and its banks (a front
# that crashed leaves its banks to save first, up to 60 s). What is still there after
# the timeout is killed.
KillMode=mixed
TimeoutStopSec=90

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
"
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
        for (name, text) in self.unit_files()? {
            super::write(&self.unit_dir.join(name), text)?;
        }
        check(&mut self.systemctl(&["daemon-reload"]))
    }

    /// Units written by an older pcds (the bash one ran a program that is gone) are written
    /// again when they differ from this one's.
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
            return Err("run 'pcds setup' first".into());
        }
        self.install_server()?;
        self.write_unit()?;
        check(&mut self.systemctl(&["enable", "--now", &self.unit(), &self.timer()]))?;
        say("the server now starts at login and checks for game updates hourly");
        say(format!("to keep it running while you are logged out: loginctl enable-linger {}", crate::env("USER").unwrap_or_default()));
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
            check(&mut self.systemctl(&["restart", &self.unit()]))?;
            say("server restarted");
        }
        Ok(())
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

    /// Another installation's unit of the same name is refused; an older pcds's unit of this
    /// installation is written again.
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
}
