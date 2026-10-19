//! `pcds plugin list | check | install | enable | disable | reload | new` (docs/plugins-guide.md).
//! Plugins live in run/plugins/<id>/; what the admin approved is run/plugins/approved.json. The
//! commands change those files, and ask the running server (if there is one) to read them again.

use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{Ctx, Result};
use crate::plugins::{self, approvals, manifest};

const USAGE: &str = "\
pcds plugin list                      the plugins, their state, what they asked for and what they cost
pcds plugin check PATH_OR_ID          check a plugin folder (or an installed plugin) and run it on sample events
pcds plugin install PATH [--force]    copy a plugin folder into run/plugins/ (it waits for approval)
pcds plugin enable ID [--grant CAP,CAP] [--yes]
                                      approve a plugin: what it asks for is shown first; --grant allows only some
pcds plugin disable ID                switch an approved plugin off
pcds plugin reload                    make the running server read run/plugins/ again
pcds plugin new ID [--kind wasm|data] [--api-path DIR]
                                      a folder for a new plugin, from a template";

impl Ctx {
    fn plugins_dir(&self) -> PathBuf {
        self.run.join("plugins")
    }

    pub fn plugin(&self, args: &[String]) -> Result {
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        match a.as_slice() {
            ["list"] => self.plugin_list(),
            ["check", target] => self.plugin_check(target),
            ["install", path, rest @ ..] => self.plugin_install(path, rest),
            ["enable", id, rest @ ..] => self.plugin_enable(id, rest),
            ["disable", id] => {
                plugins::disable(&self.plugins_dir(), id)?;
                println!("pcds: {id} is switched off");
                self.plugin_reload_running();
                Ok(())
            }
            ["reload"] => match self.api("POST", "/api/plugins/reload", None) {
                Ok(answer) => {
                    println!("{}", serde_json::from_str::<Value>(&answer).ok().and_then(|v| v["message"].as_str().map(str::to_string)).unwrap_or(answer));
                    Ok(())
                }
                Err(e) => Err(format!("the server did not take it: {e}")),
            },
            ["new", id, rest @ ..] => self.plugin_new(id, rest),
            _ => Err(USAGE.to_string()),
        }
    }

    /// Tells a running server to read the plugin folder again; says so when there is none.
    fn plugin_reload_running(&self) {
        match self.api("POST", "/api/plugins/reload", None) {
            Ok(_) => println!("pcds: the running server has read the change"),
            Err(_) => println!("pcds: the server is not running (or has no admin API): the change takes effect when it starts"),
        }
    }

    fn plugin_list(&self) -> Result {
        let dir = self.plugins_dir();
        // The running server knows more (counters, faults); the folder alone gives the rest.
        let (plugins, consumers, from) = match self.api("GET", "/api/plugins", None).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            Some(v) => (v["plugins"].as_array().cloned().unwrap_or_default(), v["stream"]["consumers"].as_array().cloned().unwrap_or_default(), "from the running server"),
            None => {
                let host = plugins::prepare(&dir, "world");
                if let Some(e) = &host.approvals_error {
                    println!("pcds: {e}: no plugin is approved until that is fixed");
                }
                (host.plugins.iter().map(plugins::admin::plugin_json).collect(), Vec::new(), "the server is not running; showing what its folder holds")
            }
        };
        let print_consumers = || {
            for c in &consumers {
                let kinds: Vec<&str> = c["kinds"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                println!("stream consumer {}: {} (last poll {} s ago, {} polls)", c["name"].as_str().unwrap_or(""), kinds.join(" "), c["lastPollSecondsAgo"], c["polls"]);
            }
        };
        if plugins.is_empty() {
            println!("no plugins in {}; pcds plugin new ID makes one, pcds plugin install PATH adds one (a folder put there is found within a few seconds)", dir.display());
            print_consumers();
            return Ok(());
        }
        println!("({from})");
        println!("{:<20} {:<8} {:<5} {:<10} {:>8} {:>6}  CAPABILITIES (granted of asked)", "ID", "VERSION", "KIND", "STATE", "CALLS", "FAULTS");
        for p in &plugins {
            let list = |k: &str| p[k].as_array().map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>()).unwrap_or_default();
            let asked = list("requested").len();
            println!(
                "{:<20} {:<8} {:<5} {:<10} {:>8} {:>6}  {}/{}: {}",
                p["id"].as_str().unwrap_or(""),
                p["version"].as_str().unwrap_or("-"),
                p["kind"].as_str().unwrap_or("-"),
                p["status"].as_str().unwrap_or(""),
                p["stats"]["calls"].to_string(),
                p["stats"]["faults"].to_string(),
                list("granted").len(),
                asked,
                list("requested").join(" ")
            );
            if let Some(d) = p["detail"].as_str() {
                println!("    {d}");
            }
            if let Some(n) = p["notice"].as_str() {
                println!("    notice: {n}");
            }
            for w in p["warnings"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                println!("    warning: {w}");
            }
            if let Some(e) = p["stats"]["lastError"].as_str() {
                println!("    last fault: {e}");
            }
        }
        print_consumers();
        Ok(())
    }

    fn plugin_check(&self, target: &str) -> Result {
        let dir = self.plugins_dir();
        let folder = if Path::new(target).join(manifest::FILE).is_file() { PathBuf::from(target) } else if manifest::valid_id(target) && dir.join(target).is_dir() { dir.join(target) } else { return Err(format!("{target}: no {} there, and no installed plugin of that id", manifest::FILE)) };
        let report = plugins::check::check_folder(&folder, Some(&dir));
        report.facts.iter().for_each(|f| println!("  {f}"));
        report.warnings.iter().for_each(|w| println!("  warning: {w}"));
        report.errors.iter().for_each(|e| println!("  ERROR: {e}"));
        if report.errors.is_empty() {
            println!("pcds: {} is fine ({} warnings)", folder.display(), report.warnings.len());
            Ok(())
        } else {
            Err(format!("{} has {} errors", folder.display(), report.errors.len()))
        }
    }

    fn plugin_install(&self, path: &str, rest: &[&str]) -> Result {
        let force = rest.contains(&"--force");
        if let Some(bad) = rest.iter().find(|r| **r != "--force") {
            return Err(format!("unknown option {bad}\n{USAGE}"));
        }
        let from = Path::new(path);
        let report = plugins::check::check_folder(from, Some(&self.plugins_dir()));
        report.errors.iter().for_each(|e| println!("  ERROR: {e}"));
        report.warnings.iter().for_each(|w| println!("  warning: {w}"));
        if !report.errors.is_empty() {
            return Err(format!("{path} does not pass pcds plugin check: not installed"));
        }
        let found = plugins::read_folder(from);
        let m = found.manifest.ok_or("no manifest")?;
        let to = self.plugins_dir().join(m.id());
        if to.exists() && !force {
            return Err(format!("{} is installed already; --force installs this one over it (its saved state stays, its approval must be given again)", m.id()));
        }
        // The manifest and the entry only: nothing else of the folder (a target/ with a build in it) is copied.
        for name in [manifest::FILE, m.plugin.entry.as_str()] {
            let bytes = fs::read(from.join(name)).map_err(|e| format!("{}: {e}", from.join(name).display()))?;
            let target = to.join(name);
            super::mkdir(target.parent().unwrap_or(&to))?;
            super::replace_file(&target, &bytes, 0o644)?;
        }
        println!("pcds: installed {} {} in {}", m.plugin.name, m.plugin.version, to.display());
        for c in &m.plugin.capabilities {
            println!("  asks for {c}");
        }
        println!("pcds: it does nothing until you approve it: pcds plugin enable {}", m.id());
        self.plugin_reload_running();
        Ok(())
    }

    fn plugin_enable(&self, id: &str, rest: &[&str]) -> Result {
        let (mut grant, mut yes) = (None, false);
        let mut it = rest.iter();
        while let Some(o) = it.next() {
            match *o {
                "--yes" => yes = true,
                "--grant" => grant = Some(it.next().ok_or("--grant CAP,CAP")?.split(',').filter(|c| !c.is_empty()).map(str::to_string).collect::<Vec<_>>()),
                other => return Err(format!("unknown option {other}\n{USAGE}")),
            }
        }
        let dir = self.plugins_dir();
        let found = plugins::read_folder(&dir.join(id));
        let Some(m) = &found.manifest else { return Err(format!("{id}: {}", found.status.detail().unwrap_or("not installed"))) };
        let given: Vec<String> = grant.clone().unwrap_or_else(|| found.requested.clone());
        println!("{} {} by {} ({}, {} licence)", m.plugin.name, m.plugin.version, if m.plugin.authors.is_empty() { "an unnamed author".into() } else { m.plugin.authors.join(", ") }, m.plugin.kind.name(), if m.plugin.license.is_empty() { "no".to_string() } else { m.plugin.license.clone() });
        println!("  {}", m.plugin.description);
        for (name, allows) in pcds_plugin_api::caps::ALL.iter().filter(|(n, _)| found.requested.iter().any(|r| r == n)) {
            println!("  {} {name}: may {allows}", if given.iter().any(|g| g == name) { "grant:" } else { "deny: " });
        }
        println!("  digest {}", found.digest);
        if !yes {
            if unsafe { libc::isatty(0) } != 1 {
                return Err("approving needs your answer: run it in a terminal, or pass --yes".into());
            }
            print!("Approve with these capabilities? [y/N] ");
            let _ = std::io::stdout().flush();
            let mut answer = String::new();
            std::io::stdin().lock().read_line(&mut answer).map_err(|e| e.to_string())?;
            if !answer.trim().eq_ignore_ascii_case("y") {
                return Err("not approved".into());
            }
        }
        plugins::approve(&dir, id, grant)?;
        println!("pcds: {id} is approved");
        self.plugin_reload_running();
        let _ = approvals::FILE;
        Ok(())
    }

    fn plugin_new(&self, id: &str, rest: &[&str]) -> Result {
        let (mut kind, mut api_path) = ("wasm", None);
        let mut it = rest.iter();
        while let Some(o) = it.next() {
            match *o {
                "--kind" => kind = it.next().copied().filter(|k| ["wasm", "data"].contains(k)).ok_or("--kind wasm|data")?,
                "--api-path" => api_path = Some(PathBuf::from(it.next().ok_or("--api-path DIR")?)),
                other => return Err(format!("unknown option {other}\n{USAGE}")),
            }
        }
        if !manifest::valid_id(id) {
            return Err(format!("{id:?} is not a plugin id: a lowercase letter, then lowercase letters, digits and dashes, 32 at most"));
        }
        let dir = PathBuf::from(id);
        if dir.exists() {
            return Err(format!("{id} exists already"));
        }
        let crate_name = id.replace('-', "_");
        let api = match &api_path {
            Some(p) => format!("path = \"{}\"", fs::canonicalize(p).map_err(|e| format!("{}: {e}", p.display()))?.display()),
            None => format!("git = \"https://github.com/perara/planet-crafter-dedicated-server\", tag = \"v{}\"", env!("CARGO_PKG_VERSION")),
        };
        let fill = |t: &str| t.replace("{id}", id).replace("{crate}", &crate_name).replace("{api}", &api).replace("{version}", env!("CARGO_PKG_VERSION"));
        let files: &[(&str, &str)] = if kind == "wasm" {
            &[
                ("plugin.toml", include_str!("../../templates/wasm/plugin.toml")),
                ("Cargo.toml", include_str!("../../templates/wasm/Cargo.toml")),
                ("src/lib.rs", include_str!("../../templates/wasm/lib.rs")),
                (".cargo/config.toml", include_str!("../../templates/wasm/cargo-config.toml")),
                (".gitignore", "/target\n"),
                ("README.md", include_str!("../../templates/wasm/README.md")),
            ]
        } else {
            &[("plugin.toml", include_str!("../../templates/data/plugin.toml")), ("rules.toml", include_str!("../../templates/data/rules.toml")), ("README.md", include_str!("../../templates/data/README.md"))]
        };
        for (name, text) in files {
            let path = dir.join(name);
            super::mkdir(path.parent().unwrap_or(&dir))?;
            fs::write(&path, fill(text)).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        println!("pcds: made {id}/ ({kind} plugin); next steps are in {id}/README.md");
        Ok(())
    }
}
