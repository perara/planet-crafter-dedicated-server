//! The commands of `pcds plugin` that an author and an admin use around the plugin itself: `test`,
//! `logs`, `stats`, `watch`, `pack`, `search`, and the sources `install` takes besides a folder (a
//! `.pcdsplugin` file, an https URL, the name of a plugin in an index). Plugins and what they may do are
//! docs/plugins.md; the commands are docs/plugins-guide.md.

use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::{Ctx, Result};
use crate::plugins::{self, manifest, pack, scenario, sign};

impl Ctx {
    pub(super) fn plugins_path(&self) -> PathBuf {
        self.run.join("plugins")
    }

    /// `pcds plugin test PATH [--scenario FILE]...`: the plugin in the real host against made-up worlds.
    pub(super) fn plugin_test(&self, path: &str, rest: &[&str]) -> Result {
        let mut named = Vec::new();
        let mut it = rest.iter();
        while let Some(o) = it.next() {
            match *o {
                "--scenario" => named.push(it.next().ok_or("--scenario FILE")?.to_string()),
                other => return Err(format!("unknown option {other}\n{}", super::plugin::USAGE)),
            }
        }
        let folder = Path::new(path);
        if !folder.join(manifest::FILE).is_file() {
            return Err(format!("{path}: no {} there", manifest::FILE));
        }
        let files = scenario::find(folder, &named)?;
        if files.is_empty() {
            return Err(format!("{path}: no scenario: put one in {path}/scenarios/NAME.toml (or scenario.toml), or name it with --scenario FILE; 'pcds plugin check {path}' runs the plugin on sample events without one. The format is in docs/plugins-guide.md"));
        }
        let mut failed = 0;
        for file in &files {
            let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
            println!("== {}", file.display());
            let scenario = match scenario::Scenario::parse(&text) {
                Ok(s) => s,
                Err(e) => {
                    println!("  ERROR: {}: {e}", file.display());
                    failed += 1;
                    continue;
                }
            };
            let mut rig = match scenario::Rig::new(folder, &scenario.config) {
                Ok(r) => r,
                Err(e) => {
                    println!("  ERROR: {e}");
                    failed += 1;
                    continue;
                }
            };
            let outcome = scenario::run(&mut rig, &scenario);
            outcome.lines.iter().for_each(|l| println!("{}", plugins::clean(l)));
            if !outcome.costs.is_empty() {
                println!("  cost of each kind of event:");
                outcome.costs.iter().for_each(|l| println!("    {l}"));
            }
            match outcome.state {
                Some((version, bytes)) => println!("  state at the end: {bytes} bytes, version {version}"),
                None => println!("  keeps no state"),
            }
            outcome.failures.iter().for_each(|f| println!("  FAILED: {}", plugins::clean(f)));
            println!("  {}", if outcome.passed() { "passed".to_string() } else { format!("{} things failed", outcome.failures.len()) });
            failed += usize::from(!outcome.passed());
        }
        if failed > 0 {
            return Err(format!("{failed} of {} scenarios failed", files.len()));
        }
        println!("pcds: {} scenarios passed", files.len());
        Ok(())
    }

    /// `pcds plugin logs NAME [-n N] [-f]`.
    pub(super) fn plugin_logs(&self, id: &str, rest: &[&str]) -> Result {
        let (mut n, mut follow) = (50u64, false);
        let mut it = rest.iter();
        while let Some(o) = it.next() {
            match *o {
                "-f" | "--follow" => follow = true,
                "-n" => n = it.next().and_then(|v| v.parse().ok()).ok_or("-n LINES")?,
                other => return Err(format!("unknown option {other}\n{}", super::plugin::USAGE)),
            }
        }
        let mut seen = 0u64;
        loop {
            let answer = self.api("POST", "/api/plugins/logs", Some(&json!({"id": id, "limit": n}).to_string())).map_err(|e| format!("{e} (the log of a plugin is kept by the running server)"))?;
            let v: Value = serde_json::from_str(&answer).map_err(|e| e.to_string())?;
            let floor = seen;
            for line in v["lines"].as_array().into_iter().flatten().filter(|l| l["seq"].as_u64().unwrap_or(0) > floor) {
                println!("{} {:<5} {}", clock(line["at"].as_u64().unwrap_or(0)), line["level"].as_str().unwrap_or(""), plugins::clean(line["text"].as_str().unwrap_or("")));
                seen = seen.max(line["seq"].as_u64().unwrap_or(0));
            }
            if !follow {
                return Ok(());
            }
            n = 200;
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }

    /// `pcds plugin stats [NAME]`: what each plugin costs, by kind of event.
    pub(super) fn plugin_stats(&self, which: Option<&str>) -> Result {
        let v: Value = serde_json::from_str(&self.api("GET", "/api/plugins", None)?).map_err(|e| e.to_string())?;
        let mut shown = 0;
        for p in v["plugins"].as_array().into_iter().flatten().filter(|p| which.is_none_or(|w| p["id"] == w)) {
            shown += 1;
            let s = &p["stats"];
            println!("{} {} ({}): {} calls, {} fuel, {:.1} us on average, {:.1} us at most; {} faults, {} restarts, {} slow, {} shed; {} actions, {} refused, {} denied",
                p["id"].as_str().unwrap_or(""), p["version"].as_str().unwrap_or(""), p["status"].as_str().unwrap_or(""),
                s["calls"], s["fuel"], s["averageMicros"].as_f64().unwrap_or(0.0), s["maxMicros"].as_f64().unwrap_or(0.0), s["faults"], s["restarts"], s["slow"], s["shed"], s["actions"], s["refused"], s["denied"]);
            for e in s["byEvent"].as_array().into_iter().flatten() {
                println!("    {:<18} {:>8} calls  avg {:>8} fuel  max {:>8} fuel  avg {:>7.1} us  max {:>7.1} us", e["event"].as_str().unwrap_or(""), e["calls"], e["averageFuel"], e["maxFuel"], e["averageMicros"].as_f64().unwrap_or(0.0), e["maxMicros"].as_f64().unwrap_or(0.0));
            }
            if s["http"]["requests"].as_u64().unwrap_or(0) > 0 {
                println!("    http: {} requests, {} answered, {} failed, {} bytes out, {} in", s["http"]["requests"], s["http"]["answered"], s["http"]["failed"], s["http"]["bytesOut"], s["http"]["bytesIn"]);
            }
            if let Some(d) = p["deniedCapabilities"].as_array().filter(|d| !d.is_empty()) {
                println!("    used without being granted: {}", d.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "));
            }
            for f in s["faultLog"].as_array().into_iter().flatten().rev().take(5) {
                println!("    fault {} in {}: {}", clock(f["at"].as_u64().unwrap_or(0)), f["during"].as_str().unwrap_or(""), plugins::clean(f["text"].as_str().unwrap_or("")));
            }
        }
        match (shown, which) {
            (0, Some(w)) => Err(format!("{w} is not a plugin of this server")),
            (0, None) => {
                println!("no plugins");
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// `pcds plugin pack DIR [--out FILE]`: the `.pcdsplugin` package.
    pub(super) fn plugin_pack(&self, dir: &str, rest: &[&str]) -> Result {
        let mut out: Option<PathBuf> = None;
        let mut it = rest.iter();
        while let Some(o) = it.next() {
            match *o {
                "--out" => out = Some(PathBuf::from(it.next().ok_or("--out FILE")?)),
                other => return Err(format!("unknown option {other}\n{}", super::plugin::USAGE)),
            }
        }
        let folder = Path::new(dir);
        let report = plugins::check::check_folder(folder, None);
        report.warnings.iter().for_each(|w| println!("  warning: {}", plugins::clean(w)));
        if !report.errors.is_empty() {
            report.errors.iter().for_each(|e| println!("  ERROR: {}", plugins::clean(e)));
            return Err(format!("{dir} does not pass pcds plugin check: not packed"));
        }
        let found = plugins::read_folder(folder);
        let m = found.manifest.ok_or("no manifest")?;
        let files = pack::files_of(folder, &m.plugin.entry)?;
        // The entry sits in the package under its own name; a path with folders (target/...) is flattened to its file name.
        let entry_name = Path::new(&m.plugin.entry).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if m.plugin.entry != entry_name {
            return Err(format!("plugin.entry is {:?}: a package carries the entry as a file at the top of the folder. Copy the build to {entry_name} next to plugin.toml and point plugin.entry at it (make package does this in the template)", m.plugin.entry));
        }
        let signed = files.iter().any(|(n, _)| n == sign::FILE);
        let bytes = pack::tar(&files)?;
        let path = out.unwrap_or_else(|| PathBuf::from(pack::file_name(m.id(), &m.plugin.version)));
        crate::util::replace_file(&path, &bytes, 0o644)?;
        let sha = pack::sha256(&bytes);
        println!("pcds: packed {} {} into {} ({} bytes, {} files{})", m.plugin.name, m.plugin.version, path.display(), bytes.len(), files.len(), if signed { ", signed" } else { ", not signed: pcds plugin sign --key KEY DIR first, if you have a key" });
        println!("sha256 {sha}");
        println!("for an index:");
        let entry = json!({"id": m.id(), "name": m.plugin.name, "version": m.plugin.version, "description": m.plugin.description, "author": m.plugin.authors.join(", "), "license": m.plugin.license, "url": path.file_name().map(|n| n.to_string_lossy().to_string()), "sha256": sha, "capabilities": m.plugin.capabilities});
        println!("{}", serde_json::to_string_pretty(&entry).unwrap_or_default());
        Ok(())
    }

    /// `pcds plugin search [TERM] --index SOURCE`.
    pub(super) fn plugin_search(&self, rest: &[&str]) -> Result {
        let (term, index) = split_index(rest)?;
        let (index, location) = load_index(&index.ok_or("search needs the index to look in: --index URL_OR_FILE (the plugins-index.json a community hosts; this program has no list of its own)")?)?;
        let found = index.search(&term);
        println!("{} ({location}): {} of {} plugins", if index.name.is_empty() { "index" } else { &index.name }, found.len(), index.plugins.len());
        for e in found {
            println!("{:<22} {:<8} {} {}", e.id, e.version, plugins::clean(&e.name), if e.description.is_empty() { String::new() } else { format!("- {}", plugins::clean(&e.description)) });
            println!("{:<22} by {}; licence {}; asks for: {}", "", if e.author.is_empty() { "an unnamed author" } else { &e.author }, if e.license.is_empty() { "unstated" } else { &e.license }, if e.capabilities.is_empty() { "nothing".to_string() } else { e.capabilities.join(", ") });
        }
        println!("pcds plugin install ID --index {location}   installs one (it waits for your approval: pcds plugin enable ID)");
        Ok(())
    }

    /// `pcds plugin install SOURCE [--force] [--yes] [--sha256 HEX] [--index SOURCE]`: SOURCE is a folder, a
    /// `.pcdsplugin` file, an https URL of one, or the id of a plugin in the index.
    pub(super) fn plugin_install(&self, source: &str, rest: &[&str]) -> Result {
        let (mut force, mut yes, mut pin, mut index) = (false, false, None::<String>, None::<String>);
        let mut it = rest.iter();
        while let Some(o) = it.next() {
            match *o {
                "--force" => force = true,
                "--yes" => yes = true,
                "--sha256" => pin = Some(it.next().ok_or("--sha256 HEX")?.to_lowercase()),
                "--index" => index = Some(it.next().ok_or("--index URL_OR_FILE")?.to_string()),
                other => return Err(format!("unknown option {other}\n{}", super::plugin::USAGE)),
            }
        }
        let path = Path::new(source);
        if path.is_dir() {
            if pin.is_some() || index.is_some() {
                return Err("--sha256 and --index are for a package or an index entry; a folder is installed as it is".into());
            }
            return self.install_folder(path, force);
        }
        // A package: from a file, an https URL, or an index entry. What it is, where it came from and its sha256 are shown.
        let (bytes, origin, entry) = if source.starts_with("https://") || source.starts_with("http://") {
            (crate::updater::fetch(source, pack::MAX_PACKAGE_BYTES as u64)?, crate::updater::shown_url(source), None)
        } else if path.is_file() {
            (fs::read(path).map_err(|e| format!("{source}: {e}"))?, format!("the file {source}"), None)
        } else if manifest::valid_id(source) {
            let location = index.ok_or_else(|| format!("{source} is neither a folder nor a file here; to install a plugin by name, say where the index is: --index URL_OR_FILE"))?;
            let (idx, loc) = load_index(&location)?;
            let e = idx.find(source).ok_or_else(|| format!("{source} is not in the index {loc}{}", manifest::hint(source, idx.plugins.iter().map(|e| e.id.as_str()))))?.clone();
            let url = pack::resolve(&loc, &e.url)?;
            pin = pin.or_else(|| Some(e.sha256.to_lowercase()));
            (crate::updater::fetch(&url, pack::MAX_PACKAGE_BYTES as u64)?, format!("the index {loc}"), Some(e))
        } else {
            return Err(format!("{source}: not a plugin folder, a .{} file, an https address of one, or the id of a plugin in an index (--index)", pack::EXTENSION));
        };
        let sha = pack::sha256(&bytes);
        if let Some(p) = &pin
            && *p != sha
        {
            return Err(format!("the package's sha256 is {sha}, not the {p} that was expected: nothing was installed (it is not the package that was listed, or it was changed on the way)"));
        }
        let files = pack::untar(&bytes).map_err(|e| format!("not a plugin package: {e}"))?;
        let scratch = self.plugins_path().join(format!(".incoming-{}", std::process::id()));
        let _ = fs::remove_dir_all(&scratch);
        super::mkdir(&scratch)?;
        let cleanup = Cleanup(scratch.clone());
        for (name, content) in &files {
            let target = scratch.join(name);
            super::mkdir(target.parent().unwrap_or(&scratch))?;
            fs::write(&target, content).map_err(|e| format!("{}: {e}", target.display()))?;
        }
        let found = plugins::read_folder_with(&scratch, &sign::Keys::load(&self.plugins_path()).unwrap_or_default());
        let m = found.manifest.clone().ok_or_else(|| format!("the package has a manifest that does not read: {}", found.status.detail().unwrap_or("")))?;
        if let Some(e) = &entry {
            if m.id() != e.id || m.plugin.version != e.version {
                return Err(format!("the index lists {} {}, the package is {} {}: nothing installed", e.id, e.version, m.id(), m.plugin.version));
            }
            if let Some(extra) = m.plugin.capabilities.iter().find(|c| !e.capabilities.contains(c)) {
                return Err(format!("the package asks for {extra}, which the index does not list for it: nothing installed (the index is wrong or the package is not the one it described)"));
            }
        }
        println!("package {} {} by {} ({} licence)", plugins::clean(&m.plugin.name), m.plugin.version, if m.plugin.authors.is_empty() { "an unnamed author".to_string() } else { plugins::clean(&m.plugin.authors.join(", ")) }, if m.plugin.license.is_empty() { "no".to_string() } else { plugins::clean(&m.plugin.license) });
        println!("  from {origin}; sha256 {sha}; {} files, {} bytes", files.len(), bytes.len());
        println!("  {}", found.signed.describe(&found.requested));
        println!("  asks for: {}", if found.requested.is_empty() { "nothing".to_string() } else { found.requested.join(", ") });
        // A pin the user typed is their decision; one from an index, or none at all, is asked about.
        let typed = pin.is_some() && entry.is_none();
        if !typed && !yes {
            if unsafe { libc::isatty(0) } != 1 {
                return Err("installing a package needs your answer: run it in a terminal, pass --yes, or pin the package with --sha256 HEX".into());
            }
            print!("Install this package? It does nothing until you approve what it asks for. [y/N] ");
            let _ = std::io::stdout().flush();
            let mut answer = String::new();
            std::io::stdin().lock().read_line(&mut answer).map_err(|e| e.to_string())?;
            if !answer.trim().eq_ignore_ascii_case("y") {
                return Err("not installed".into());
            }
        }
        let result = self.install_folder(&scratch, force);
        drop(cleanup);
        result
    }

    /// Copies a plugin folder into the plugins folder: the manifest, the entry and the signature, after the check.
    pub(super) fn install_folder(&self, from: &Path, force: bool) -> Result {
        let report = plugins::check::check_folder(from, Some(&self.plugins_path()));
        report.errors.iter().for_each(|e| println!("  ERROR: {}", plugins::clean(e)));
        report.warnings.iter().for_each(|w| println!("  warning: {}", plugins::clean(w)));
        if !report.errors.is_empty() {
            return Err(format!("{} does not pass pcds plugin check: not installed", from.display()));
        }
        let found = plugins::read_folder(from);
        let m = found.manifest.ok_or("no manifest")?;
        let to = self.plugins_path().join(m.id());
        if to.exists() && !force {
            return Err(format!("{} is installed already; --force installs this one over it (its saved state stays; what it asks for more than before is shown when you approve it again)", m.id()));
        }
        // The manifest and the entry only: nothing else of the folder (a target/ with a build in it) is copied.
        for name in [manifest::FILE, m.plugin.entry.as_str()] {
            let bytes = plugins::read_regular(&from.join(name), plugins::limits::MAX_MODULE_BYTES).map_err(|e| format!("{}: {e}", from.join(name).display()))?;
            let target = to.join(name);
            super::mkdir(target.parent().unwrap_or(&to))?;
            crate::util::replace_file(&target, &bytes, 0o644)?;
        }
        // A signature goes with the files it was made for; one that no longer holds is not copied.
        let sig = from.join(sign::FILE);
        if sig.is_file() {
            let bytes = fs::read(&sig).map_err(|e| format!("{}: {e}", sig.display()))?;
            crate::util::replace_file(&to.join(sign::FILE), &bytes, 0o644)?;
        } else {
            let _ = fs::remove_file(to.join(sign::FILE));
        }
        println!("pcds: installed {} {} in {}", m.plugin.name, m.plugin.version, to.display());
        for c in &m.plugin.capabilities {
            println!("  asks for {c}");
        }
        println!("pcds: it does nothing until you approve it: pcds plugin enable {}", m.id());
        self.plugin_reload_running();
        Ok(())
    }

    /// `pcds plugin watch DIR [--approve] [--once]`: when the plugin's files change, check them, install them over the
    /// installed ones and, with `--approve`, approve them again if they ask for nothing that was not approved.
    pub(super) fn plugin_watch(&self, dir: &str, rest: &[&str]) -> Result {
        let (mut approve, mut once) = (false, false);
        for o in rest {
            match *o {
                "--approve" => approve = true,
                "--once" => once = true,
                other => return Err(format!("unknown option {other}\n{}", super::plugin::USAGE)),
            }
        }
        let folder = PathBuf::from(dir);
        let m = plugins::read_folder(&folder).manifest.ok_or_else(|| format!("{dir}: no readable {}", manifest::FILE))?;
        let watched = [folder.join(manifest::FILE), folder.join(&m.plugin.entry), folder.join(sign::FILE)];
        let stamp = || -> Vec<(u128, u64)> { watched.iter().map(|p| fs::metadata(p).map(|m| (m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos()), m.len())).unwrap_or((0, 0))).collect() };
        if !once {
            println!("pcds: watching {} (Ctrl-C ends); build the plugin in another terminal, and every new build is installed{}", watched[1].display(), if approve { " and approved again if it asks for nothing new" } else { "" });
        }
        let mut seen: Option<Vec<(u128, u64)>> = None;
        let mut settled = stamp();
        loop {
            let now = stamp();
            // Acted on when two looks in a row agree: a build still being written is not read half way.
            if now == settled && Some(&now) != seen.as_ref() {
                seen = Some(now.clone());
                match self.watch_round(&folder, approve) {
                    Ok(line) => println!("pcds: {line}"),
                    Err(e) => println!("pcds: {e}"),
                }
                if once {
                    return Ok(());
                }
            }
            settled = now;
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    fn watch_round(&self, folder: &Path, approve: bool) -> Result<String> {
        let dir = self.plugins_path();
        let report = plugins::check::check_folder(folder, Some(&dir));
        report.errors.iter().for_each(|e| println!("  ERROR: {}", plugins::clean(e)));
        if !report.errors.is_empty() {
            return Err(format!("{} errors: not installed; the version that is installed goes on", report.errors.len()));
        }
        let found = plugins::read_folder(folder);
        let m = found.manifest.clone().ok_or("no manifest")?;
        let approvals = plugins::approvals::Approvals::load(&dir)?;
        let before = approvals.0.get(m.id()).cloned();
        self.install_folder(folder, true)?;
        let Some(before) = before.filter(|a| a.enabled) else { return Ok(format!("installed {}; it was never approved here: pcds plugin enable {}", m.id(), m.id())) };
        let allow = m.net.as_ref().map(|n| n.allow.clone()).unwrap_or_default();
        let diff = before.diff(&found.requested, &allow);
        if !diff.added.is_empty() || !diff.added_hosts.is_empty() {
            return Ok(format!("{} now also asks for {}{}: it keeps the old version running until you approve it again: pcds plugin enable {}", m.id(), diff.added.join(", "), if diff.added_hosts.is_empty() { String::new() } else { format!(" and may reach {}", diff.added_hosts.join(", ")) }, m.id()));
        }
        if !approve {
            return Ok(format!("{} installed; run pcds plugin enable {} (or watch with --approve)", m.id(), m.id()));
        }
        let grant: Vec<String> = before.grant.iter().filter(|g| found.requested.contains(g)).cloned().collect();
        plugins::approve_digest(&dir, m.id(), Some(grant), Some(&found.digest))?;
        self.plugin_reload_running();
        Ok(format!("{} {} approved again with the same grant; its state was handed over to the new version", m.id(), m.plugin.version))
    }
}

struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn clock(unix: u64) -> String {
    let s = unix % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
}

/// `[TERM] --index SOURCE` in any order.
fn split_index(rest: &[&str]) -> Result<(String, Option<String>)> {
    let (mut term, mut index) = (Vec::new(), None);
    let mut it = rest.iter();
    while let Some(o) = it.next() {
        match *o {
            "--index" => index = Some(it.next().ok_or("--index URL_OR_FILE")?.to_string()),
            other if other.starts_with("--") => return Err(format!("unknown option {other}\n{}", super::plugin::USAGE)),
            word => term.push(word),
        }
    }
    Ok((term.join(" "), index))
}

/// An index from an https address or a file; and where it was read from.
fn load_index(source: &str) -> Result<(pack::Index, String)> {
    let bytes = if source.starts_with("https://") || source.starts_with("http://") { crate::updater::fetch(source, pack::MAX_INDEX_BYTES as u64)? } else { fs::read(source).map_err(|e| format!("{source}: {e}"))? };
    let shown = if source.contains("://") { crate::updater::shown_url(source) + source.split_once("://").and_then(|(_, r)| r.find('/').map(|i| &r[i..])).unwrap_or("").split(['?', '#']).next().unwrap_or("") } else { source.to_string() };
    Ok((pack::Index::parse(&bytes)?, if source.contains("://") { source.split(['?', '#']).next().unwrap_or(source).to_string() } else { shown }))
}
