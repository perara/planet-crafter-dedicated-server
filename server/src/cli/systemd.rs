//! The systemd user units: the server (`pcds run`) and the hourly game update check.

use std::fs;
use std::os::unix::fs::MetadataExt;
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
        quiet(&mut self.systemctl(&["is-active", "--quiet", &self.unit()]))
            && output(&mut self.systemctl(&["show", "-p", "EnvironmentFiles", "--value", &self.unit()])).is_some_and(|files| files.contains(&*self.env_file.to_string_lossy()))
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

    pub fn write_unit(&self) -> Result {
        super::mkdir(&self.unit_dir)?;
        let (run, env, unit, exe) = (self.run.display(), self.env_file.display(), &self.unit_name, self.exe.display());
        let environment = format!("Environment=\"PCDS_HOME={run}\" \"PCDS_UNIT={unit}\"");
        super::write(
            &self.unit_dir.join(self.unit()),
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
        )?;
        super::write(
            &self.unit_dir.join(format!("{unit}-update.service")),
            format!(
                "[Unit]
Description=Planet Crafter dedicated server: pick up game updates

[Service]
Type=oneshot
{environment}
ExecStart=\"{exe}\" update
"
            ),
        )?;
        super::write(
            &self.unit_dir.join(self.timer()),
            "[Unit]
Description=Planet Crafter dedicated server: hourly game update check

[Timer]
OnBootSec=10min
OnUnitActiveSec=1h

[Install]
WantedBy=timers.target
",
        )?;
        check(&mut self.systemctl(&["daemon-reload"]))
    }

    pub fn install_service(&self) -> Result {
        if !self.world_file().is_file() {
            return Err("run 'pcds setup' first".into());
        }
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
