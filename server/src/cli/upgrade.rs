//! `pcds upgrade`: a server release into the standby bank and the world handed over to it
//! (through the running front: POST /api/update), or, with the server stopped, the release
//! made the active bank for the next start. And the banks in `pcds status`.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{say, Ctx, Result};
use crate::msg::{BankInfo, Fallback, RolledBack};
use crate::updater;

const USAGE: &str = "pcds upgrade [VERSION] [--restart] | --rollback | --check";

impl Ctx {
    pub fn upgrade(&self, args: &[String]) -> Result {
        let (mut version, mut restart, mut rollback, mut check) = (None, false, false, false);
        for a in args {
            match a.as_str() {
                "--restart" => restart = true,
                "--rollback" => rollback = true,
                "--check" => check = true,
                v if !v.starts_with('-') && version.is_none() => version = Some(v.to_string()),
                _ => return Err(USAGE.into()),
            }
        }
        if (rollback && (version.is_some() || check)) || (check && restart) {
            return Err(USAGE.into());
        }
        let banks = self.banks();
        if check {
            let release = updater::lookup(&self.releases_url(), version.as_deref())?;
            println!("{}: {}", if version.is_some() { "found" } else { "latest release" }, release.tag);
            self.print_banks(None);
            return Ok(());
        }
        let running = self.active()?;
        if running && !restart {
            let body = if rollback { json!({"rollback": true}) } else { json!({"version": version}) };
            let answer: Value = serde_json::from_str(&self.api("POST", "/api/update", Some(&body.to_string())).map_err(api_error)?).map_err(|e| e.to_string())?;
            let seq = answer["seq"].as_u64().ok_or("the server gave the update no number")?;
            say(if rollback { "handing the world back to the other bank".to_string() } else { format!("installing {} into the standby bank", version.as_deref().unwrap_or("the latest release")) });
            return self.wait_for_update(seq);
        }
        // Stopped, or a restart: the release into the bank the server does not run from.
        let live = if running { self.live_bank() } else { None };
        let active = live.unwrap_or_else(|| updater::active(&banks));
        let target = updater::other(&active).to_string();
        if rollback {
            let info = updater::info(&banks, &target).ok_or_else(|| format!("bank {target} holds no release to go back to"))?;
            updater::set_active(&banks, &target)?;
            updater::set_rolled_back(&banks, &active)?;
            updater::clear_fallback(&banks)?;
            say(format!("bank {target} ({}) is the active one now", describe(&info)));
        } else {
            let release = updater::lookup(&self.releases_url(), version.as_deref())?;
            say(format!("downloading {} into bank {target}", release.tag));
            let info = updater::install(&release, &banks, &target, None)?;
            updater::set_active(&banks, &target)?;
            updater::clear_rolled_back(&banks)?;
            updater::clear_fallback(&banks)?;
            say(format!("{} installed in bank {target} (sha256 checked), the active one now: {}", release.tag, describe(&info)));
        }
        if running {
            // The release now active reads server.env its own way (a setting it retired stops its
            // start), and may read another tables format (a stopped server gets them at 'pcds
            // start'). Both are looked at while the server still runs.
            self.settings_or_undo(&banks, &active)?;
            self.tables_or_undo(None, &banks, &active)?;
            say("restarting the server: every player is disconnected and reconnects");
            if self.container {
                return self.restart_in_place();
            }
            self.restart_if_running()?;
            return self.wait_until_back();
        }
        say("the server runs it from its next start ('pcds start')");
        Ok(())
    }

    /// Whether the now-active bank's release accepts server.env (its `pcds check-settings`: it,
    /// not the release that runs, knows which settings it has retired). When it does not, `active`
    /// goes back to `previous` and nothing is restarted. A release that has no such command
    /// (it prints its help and no error) is not asked.
    fn settings_or_undo(&self, banks: &std::path::Path, previous: &str) -> Result {
        let now = updater::active(banks);
        let out = std::process::Command::new(banks.join(&now).join("pcds")).arg("check-settings").env("PCDS_HOME", &self.run).stdin(std::process::Stdio::null()).output().map_err(|e| format!("bank {now}: {e}"))?;
        let said = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if out.status.success() || said.is_empty() {
            return Ok(());
        }
        let back = match updater::set_active(banks, previous) {
            Ok(()) => format!("the server was not restarted, and bank {previous} is still the active one"),
            Err(undo) => format!("and bank {previous} could not be made the active one again ({undo}): fix run/bin/active before the server restarts"),
        };
        Err(format!("{said}\nthe release just installed would not start with this server.env: edit it (docs/upgrading.md), then run the same command again\n{back}"))
    }

    /// After the restart of the unit: until the world runs again. Err when the unit gave up
    /// (a start that fails is retried by systemd, 15 s apart, and then left failed).
    fn wait_until_back(&self) -> Result {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(300) {
            std::thread::sleep(Duration::from_secs(1));
            if self.api_json("GET", "/api/status").is_ok_and(|s| s["state"] == "running") {
                say(format!("the server is back after {} s", started.elapsed().as_secs()));
                return Ok(());
            }
            if let Ok(state) = self.show("SubState") && matches!(state.as_str(), "auto-restart" | "failed") {
                return Err(format!("the server did not start again (the unit is {state}): 'pcds logs' says why, and 'pcds upgrade --rollback --restart' goes back to the other release"));
            }
        }
        Err("the world did not run again within 5 minutes ('pcds logs')".into())
    }

    /// The tables the now-active bank's release reads, read if there are none (`ensure_tables`).
    /// When that fails, `active` goes back to `previous`, the bank that runs: the server is not
    /// restarted, and a later start (a reboot, a crash) must not meet a bank whose tables cannot
    /// be had, which `pcds run` cannot start and does not fall back from.
    fn tables_or_undo(&self, game: Option<&std::path::Path>, banks: &std::path::Path, previous: &str) -> Result {
        self.ensure_tables(game).map(|_| ()).map_err(|e| match updater::set_active(banks, previous) {
            Ok(()) => format!("{e}\nthe server was not restarted, and bank {previous} is still the active one"),
            Err(undo) => format!("{e}\nand bank {previous} could not be made the active one again ({undo}): fix run/bin/active before the server restarts"),
        })
    }

    /// Where releases are looked up: PCDS_RELEASES from the environment, else from server.env
    /// (what the running server uses), else GitHub's.
    fn releases_url(&self) -> String {
        crate::env("PCDS_RELEASES").or_else(|| self.env_get("PCDS_RELEASES")).unwrap_or_else(updater::releases_url)
    }

    /// In the container: which release the server runs from, against the image's. A hot update
    /// puts newer banks into /data/bin, and a bank newer than the image wins over it (bank_for),
    /// so a pinned image tag does not pin the server; this says so. None outside the container.
    pub fn image_note(&self, running: Option<&str>) -> Option<String> {
        if !self.container {
            return None;
        }
        let banks = self.banks();
        let bank = running.map_or_else(|| updater::active(&banks), str::to_string);
        let held = updater::info(&banks, &bank)?;
        let (image, held) = (env!("CARGO_PKG_VERSION"), held.version().to_string());
        let how = if held == image { "the image's version".to_string() } else if updater::newer(&held, image) {
            format!("newer than the image's: a hot update put it in /data/bin, and a bank newer than the image wins over it, so the image tag does not pin the server. 'pcds upgrade v{image} --restart' runs the image's release again; an image newer than the banks replaces them at its first start")
        } else {
            format!("older than the image's: the bank 'pcds upgrade --rollback' chose is kept (or the image's release could not be installed: see the log above). 'pcds upgrade v{image} --restart' runs the image's release")
        };
        Some(format!("image: pcds {image}; the server runs pcds {held} from bank {bank}: {how}"))
    }

    /// The bank the running front runs the world in.
    pub fn live_bank(&self) -> Option<String> {
        let status: Value = serde_json::from_str(&self.api("GET", "/api/status", None).ok()?).ok()?;
        status["banks"]["live"]["bank"].as_str().map(str::to_string)
    }

    /// Until the server has done update `seq` (a download takes a while; a flip a moment).
    fn wait_for_update(&self, seq: u64) -> Result {
        let deadline = Instant::now() + Duration::from_secs(1200);
        let mut said = String::new();
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(500));
            let Ok(text) = self.api("GET", "/api/update", None) else { continue };
            let status: Value = serde_json::from_str(&text).unwrap_or_default();
            if let Some(done) = status["history"].as_array().and_then(|h| h.iter().find(|e| e["seq"].as_u64() == Some(seq))) {
                let message = done["message"].as_str().unwrap_or("").to_string();
                return if done["ok"] == true {
                    say(message);
                    Ok(())
                } else {
                    Err(message)
                };
            }
            if let Some(doing) = status["doing"].as_str().filter(|d| *d != said) {
                say(doing);
                said = doing.to_string();
            }
        }
        Err("no answer from the server about the update within 20 minutes (see pcds logs)".into())
    }

    /// The banks' lines of `pcds status`; `live`: the running front's view of them.
    pub fn print_banks(&self, live: Option<&Value>) {
        let banks = self.banks();
        let active = updater::active(&banks);
        for bank in updater::BANKS {
            let mut marks = Vec::new();
            if bank == active {
                marks.push("active".to_string());
            }
            if let Some(l) = live.filter(|l| l["live"]["bank"] == bank) {
                marks.push(format!("running, pid {}", l["live"]["pid"]));
            }
            let marks = if marks.is_empty() { String::new() } else { format!("  [{}]", marks.join(", ")) };
            match updater::info(&banks, bank) {
                Some(info) => println!("bank {bank}: {}{marks}", describe(&info)),
                None => println!("bank {bank}: empty{marks}"),
            }
        }
        if let Some(note) = self.image_note(live.and_then(|l| l["live"]["bank"].as_str())) {
            println!("{note}");
        }
        // The running front's view, else the note on disk.
        let fallback = live.and_then(|l| serde_json::from_value::<Fallback>(l["fallback"].clone()).ok()).or_else(|| updater::fallback(&banks));
        if let Some(f) = fallback {
            println!("started from bank {} because bank {} {}", f.to, f.from, f.reason);
        }
        // The running front's view, else the note on disk: a release given up for crashes.
        let given_up = live.and_then(|l| serde_json::from_value::<RolledBack>(l["rolledBack"].clone()).ok()).or_else(|| updater::rolled_back(&banks));
        if let Some(r) = given_up.filter(|r| r.reason.is_some()) {
            println!("the release in bank {} ({}) was rolled back from automatically: {}", r.bank, r.tag.as_deref().unwrap_or("null"), r.reason.as_deref().unwrap_or_default());
        }
    }
}

/// What the server said in an error answer ("HTTP 400: {"error": "..."}"): the words, else the
/// whole.
fn api_error(e: String) -> String {
    e.find("{\"error\"").and_then(|i| serde_json::from_str::<Value>(&e[i..]).ok()).and_then(|v| v["error"].as_str().map(str::to_string)).unwrap_or(e)
}

/// "pcds 0.6.0 (v0.6.0), front protocol 2, snapshot format 1, installed 2026-10-18 15:00".
fn describe(info: &BankInfo) -> String {
    let tag = info.tag.as_ref().map(|t| format!(" ({t})")).unwrap_or_else(|| " (a local build)".into());
    let when = info.installed.map(|t| format!(", installed {}", super::local_time(std::time::UNIX_EPOCH + Duration::from_secs(t)))).unwrap_or_default();
    format!("pcds {}{tag}, front protocol {}, snapshot format {}{when}", info.version(), crate::msg::json(&info.protocol), crate::msg::json(&info.format))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options() {
        let dir = super::super::test_dir("upgrade-options");
        let ctx = super::super::test_ctx(&dir);
        for bad in [&["--rollback", "v1.0.0"][..], &["--check", "--restart"], &["--frobnicate"], &["v1", "v2"]] {
            let args: Vec<String> = bad.iter().map(|s| s.to_string()).collect();
            assert_eq!(ctx.upgrade(&args).unwrap_err(), USAGE, "{bad:?}");
        }
        assert_eq!(api_error("POST http://x/api/update: HTTP 400: {\"error\":\"it needs a restart\"}".into()), "it needs a restart");
        assert_eq!(api_error("connection refused".into()), "connection refused");
        // Tables that cannot be had: the bank that runs stays the active one.
        let banks = ctx.banks();
        updater::set_active(&banks, "b").unwrap();
        let e = ctx.tables_or_undo(Some(&dir.join("no-game-here")), &banks, "a").unwrap_err();
        assert!(e.contains("is still the active one") && e.contains("not restarted"), "{e}");
        assert_eq!(updater::active(&banks), "a");
        assert!(describe(&serde_json::from_value(json!({"version": "0.6.0", "tag": "v0.6.0", "protocol": 2, "format": 1})).unwrap()).starts_with("pcds 0.6.0 (v0.6.0), front protocol 2, snapshot format 1"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A release that refuses server.env is not made the active one by an upgrade that restarts
    /// a running server: the bank that runs stays active, and the message says what to edit.
    /// A release that has no such command is not asked.
    #[test]
    fn an_upgrade_that_would_not_start_with_server_env_changes_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = super::super::test_dir("upgrade-settings");
        let ctx = super::super::test_ctx(&dir);
        let banks = ctx.banks();
        let put = |bank: &str, script: &str| {
            std::fs::create_dir_all(banks.join(bank)).unwrap();
            std::fs::write(banks.join(bank).join("pcds"), format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(banks.join(bank).join("pcds"), std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        put("b", "[ \"$1\" = check-settings ] && echo \"pcds: $PCDS_HOME/server.env: unknown setting PCDS_OLD\" >&2 && exit 1");
        updater::set_active(&banks, "b").unwrap();
        let e = ctx.settings_or_undo(&banks, "a").unwrap_err();
        assert!(e.contains("unknown setting PCDS_OLD") && e.contains("not restarted") && e.contains("bank a is still the active one"), "{e}");
        assert_eq!(updater::active(&banks), "a");
        // Accepted, and an older release that prints its help and no error: both go on.
        put("b", "exit 0");
        updater::set_active(&banks, "b").unwrap();
        ctx.settings_or_undo(&banks, "a").unwrap();
        put("b", "echo 'pcds: install and run a Planet Crafter dedicated server'; exit 1");
        ctx.settings_or_undo(&banks, "a").unwrap();
        assert_eq!(updater::active(&banks), "b");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// In the container, `pcds status` and the start say which release runs against the image's.
    #[test]
    fn the_container_says_which_release_runs_against_the_images() {
        let dir = super::super::test_dir("image-note");
        let mut ctx = super::super::test_ctx(&dir);
        let banks = ctx.banks();
        let image = env!("CARGO_PKG_VERSION");
        for b in updater::BANKS {
            std::fs::create_dir_all(banks.join(b)).unwrap();
        }
        let put = |bank: &str, version: &str| {
            std::fs::write(banks.join(bank).join("pcds"), "").unwrap();
            updater::write_info(&banks.join(bank), &BankInfo { version: Some(version.to_string()), ..Default::default() }).unwrap();
        };
        put("a", image);
        assert_eq!(ctx.image_note(None), None, "outside the container there is no image");
        ctx.container = true;
        let note = ctx.image_note(None).unwrap();
        assert!(note.starts_with(&format!("image: pcds {image}; the server runs pcds {image} from bank a")) && note.contains("the image's version"), "{note}");
        // A hot update put a newer release in the active bank: it wins, and the note says how to go back.
        put("a", "99.0.0");
        let note = ctx.image_note(None).unwrap();
        assert!(note.contains("pcds 99.0.0 from bank a") && note.contains("does not pin the server") && note.contains(&format!("pcds upgrade v{image} --restart")), "{note}");
        // The bank that runs (the live one) is the one described, not the active one.
        put("b", image);
        assert!(ctx.image_note(Some("b")).unwrap().contains("the image's version"));
        // An older release left active (a rollback).
        put("a", "0.0.1");
        assert!(ctx.image_note(None).unwrap().contains("older than the image's"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
