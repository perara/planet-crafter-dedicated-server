//! The systemd user units: the server (`pcds run`) and the hourly game update check.

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

    /// Whether this installation's server runs: the unit is this installation's when it reads
    /// this settings file; in the container, the server is a `pcds serve` of this user.
    pub fn active(&self) -> bool {
        if self.container {
            return self.server_pid().is_some();
        }
        quiet(&mut self.systemctl(&["is-active", "--quiet", &self.unit()])) && self.ours()
    }

    /// The unit is this installation's: it reads this settings file.
    fn ours(&self) -> bool {
        output(&mut self.systemctl(&["show", "-p", "EnvironmentFiles", "--value", &self.unit()])).is_some_and(|files| files.contains(&*self.env_file.to_string_lossy()))
    }

    /// The program the units run: run/bin/pcds, which a rebuild, `make clean` or a removed
    /// release folder leaves alone.
    pub fn installed(&self) -> PathBuf {
        self.run.join("bin/pcds")
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
    fn unit_files(&self) -> [(String, String); 3] {
        let installed = self.installed();
        let (run, env, unit, exe) = (self.run.display(), self.env_file.display(), &self.unit_name, installed.display());
        let environment = format!("Environment=\"PCDS_HOME={run}\" \"PCDS_UNIT={unit}\"");
        [
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
TimeoutStopSec=60

[Install]
WantedBy=default.target
"
                ),
            ),
            (
                format!("{unit}-update.service"),
                format!(
                    "[Unit]
Description=Planet Crafter dedicated server: pick up game updates

[Service]
Type=oneshot
{environment}
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
        ]
    }

    /// The units, for the program installed in run/bin.
    pub fn write_unit(&self) -> Result {
        super::mkdir(&self.unit_dir)?;
        for (name, text) in self.unit_files() {
            super::write(&self.unit_dir.join(name), text)?;
        }
        check(&mut self.systemctl(&["daemon-reload"]))
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
        } else if self.active() {
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
        let state = output(&mut self.systemctl(&["show", "-p", "ActiveState", "--value", &self.unit()])).unwrap_or_default();
        if !starts_after_update(&state) || !self.ours() {
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
    fn the_units_run_the_installed_program() {
        let dir = super::super::test_dir("units");
        let ctx = super::super::test_ctx(&dir);
        let units = ctx.unit_files();
        let exec = |name: &str| units.iter().find(|(n, _)| n == name).unwrap().1.lines().find_map(|l| l.strip_prefix("ExecStart=")).map(str::to_string);
        let installed = dir.join("run/bin/pcds").display().to_string();
        assert_eq!(exec("pcds-cli-test.service").unwrap(), format!("\"{installed}\" run"));
        assert_eq!(exec("pcds-cli-test-update.service").unwrap(), format!("\"{installed}\" update"));
        assert!(units[0].1.contains(&format!("PCDS_HOME={}", dir.join("run").display())));
        assert_eq!(units[2].0, "pcds-cli-test-update.timer");
        fs::remove_dir_all(&dir).unwrap();
    }
}
