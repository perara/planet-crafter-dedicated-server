//! `pcds plugin list | check | install | enable | disable | reload | new | config | keygen | sign |
//! trust | untrust | keys` (docs/plugins-guide.md).
//! Plugins live in run/plugins/<id>/; what the admin approved is run/plugins/approved.json. The
//! commands change those files, and ask the running server (if there is one) to read them again.

use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{Ctx, Result};
use crate::plugins::{self, approvals, config, manifest, sign};

const USAGE: &str = "\
pcds plugin list                      the plugins, their state, what they asked for and what they cost
pcds plugin check PATH_OR_ID          check a plugin folder (or an installed plugin) and run it on sample events
pcds plugin install PATH [--force]    copy a plugin folder into run/plugins/ (it waits for approval)
pcds plugin enable ID [--grant CAP,CAP] [--yes]
                                      approve a plugin: what it asks for is shown first; --grant allows only some
pcds plugin disable ID                switch an approved plugin off
pcds plugin reload                    make the running server read run/plugins/ again
pcds plugin new ID [--kind wasm|data] [--api-path DIR]
                                      a folder for a new plugin, from a template
pcds plugin keygen NAME               make a signing key pair: NAME.key (keep it secret) and NAME.pub (give it to admins)
pcds plugin sign --key NAME.key DIR   sign the plugin in DIR: writes DIR/plugin.sig (the files must not change after)
pcds plugin trust KEY [--name NAME] [--grant CAP,CAP] [--force]
                                      trust a publisher: KEY is a .pub file or its 64 hex digits; --grant is the most it vouches for
pcds plugin untrust KEY               stop trusting a key (its id or the start of it); its plugins stop at the next look at run/plugins/
pcds plugin keys                      the trusted keys and what each vouches for, and the revoked ones
pcds plugin token create NAME --scope SCOPE,SCOPE [--rate PER_MINUTE]
                                      a token for a program that reads the event stream / chats / kicks (shown once)
pcds plugin token revoke NAME_OR_ID   refuse a token from its next request
pcds plugin tokens                    the tokens, their scopes and rates (never the tokens themselves)
pcds plugin config list [ID]          the values a plugin's manifest declares and whether they are set
pcds plugin config set ID KEY [VALUE] set a value; a secret one is read from the terminal (or stdin), not from the command line
pcds plugin config unset ID KEY       remove a value";

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
            ["config", rest @ ..] => self.plugin_config(rest),
            ["keygen", name] => plugin_keygen(name),
            ["sign", "--key", key, dir] => plugin_sign(key, dir),
            ["trust", key, rest @ ..] => self.plugin_trust(key, rest),
            ["untrust", key] => self.plugin_untrust(key),
            ["keys"] => self.plugin_keys(),
            ["token", "create", name, rest @ ..] => self.plugin_token_create(name, rest),
            ["token", "revoke", which] => {
                let gone = crate::tokens::revoke(&self.plugins_dir(), which)?;
                println!("pcds: the token {} ({}) is revoked; it is refused from its next request", gone.name, gone.id);
                Ok(())
            }
            ["tokens"] => self.plugin_tokens(),
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
            match p["signature"]["state"].as_str() {
                Some("trusted") => println!("    signed by {} ({})", p["signature"]["keyName"].as_str().unwrap_or(""), p["signature"]["keyId"].as_str().unwrap_or("")),
                Some("untrusted") => println!("    signed by the key {}, which this server does not trust", p["signature"]["keyId"].as_str().unwrap_or("")),
                Some("revoked") => println!("    signed by the key {}, which was revoked", p["signature"]["keyId"].as_str().unwrap_or("")),
                _ => {}
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
        report.facts.iter().for_each(|f| println!("  {}", plugins::clean(f)));
        report.warnings.iter().for_each(|w| println!("  warning: {}", plugins::clean(w)));
        report.errors.iter().for_each(|e| println!("  ERROR: {}", plugins::clean(e)));
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
        report.errors.iter().for_each(|e| println!("  ERROR: {}", plugins::clean(e)));
        report.warnings.iter().for_each(|w| println!("  warning: {}", plugins::clean(w)));
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
        let keys = sign::Keys::load(&dir)?;
        let found = plugins::read_folder_with(&dir.join(id), &keys);
        let Some(m) = &found.manifest else { return Err(format!("{id}: {}", found.status.detail().unwrap_or("not installed"))) };
        let given: Vec<String> = grant.clone().unwrap_or_else(|| found.requested.clone());
        println!("{} {} by {} ({}, {} licence)", m.plugin.name, m.plugin.version, if m.plugin.authors.is_empty() { "an unnamed author".into() } else { m.plugin.authors.join(", ") }, m.plugin.kind.name(), if m.plugin.license.is_empty() { "no".to_string() } else { m.plugin.license.clone() });
        println!("  {}", m.plugin.description);
        println!("  {}", found.signed.describe(&found.requested));
        for (name, allows) in pcds_plugin_api::caps::ALL.iter().filter(|(n, _)| found.requested.iter().any(|r| r == n)) {
            println!("  {} {name}: may {allows}", if given.iter().any(|g| g == name) { "grant:" } else { "deny: " });
        }
        if let Some(net) = &m.net {
            println!("  may send HTTP requests to:");
            for a in &net.allow {
                println!("    {a}{}", if a.contains("://*.") { "   (a wildcard: every name below it)" } else { "" });
            }
            if found.requested.iter().any(|r| r == pcds_plugin_api::caps::NET_HTTP_LOCAL) {
                println!("  and, with net.http.local, to this machine and your local network (loopback, private addresses)");
            } else {
                println!("  never to this machine or your local network: an address of those is refused whatever the name");
            }
        }
        for k in m.config.iter().filter(|k| plugins::config::Configs::load(&dir).ok().is_none_or(|c| c.0.get(id).is_none_or(|v| !v.contains_key(&k.key)))) {
            println!("  needs a value you set: pcds plugin config set {id} {}   ({}{})", k.key, k.description, if k.secret { "; a secret" } else { "" });
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
        // The files that were shown are the files approved, or nothing is.
        plugins::approve_digest(&dir, id, grant, Some(&found.digest))?;
        println!("pcds: {id} is approved");
        self.plugin_reload_running();
        let _ = approvals::FILE;
        Ok(())
    }

    fn plugin_trust(&self, key: &str, rest: &[&str]) -> Result {
        let (mut name, mut grant, mut force) = (String::new(), Vec::new(), false);
        let mut it = rest.iter();
        while let Some(o) = it.next() {
            match *o {
                "--force" => force = true,
                "--name" => name = it.next().ok_or("--name NAME")?.to_string(),
                "--grant" => grant = it.next().ok_or("--grant CAP,CAP")?.split(',').filter(|c| !c.is_empty()).map(str::to_string).collect(),
                other => return Err(format!("unknown option {other}\n{USAGE}")),
            }
        }
        let text = fs::read_to_string(key).unwrap_or_else(|_| key.to_string());
        let public = sign::parse_public(&text)?;
        let dir = self.plugins_dir();
        let mut keys = sign::Keys::load(&dir)?;
        let trusted = keys.trust(&public, &name, grant, force)?;
        keys.save(&dir)?;
        if sign::published(&trusted.id) {
            println!("pcds: WARNING: {}", sign::PUBLISHED_WARNING);
        }
        println!("pcds: trusting {} ({}); it vouches for: {}", trusted.name, trusted.id, if trusted.grant.is_empty() { "no capability (its plugins still wait for you to approve what they ask for, and then follow its updates)".to_string() } else { trusted.grant.join(", ") });
        self.plugin_reload_running();
        Ok(())
    }

    fn plugin_untrust(&self, key: &str) -> Result {
        let dir = self.plugins_dir();
        let mut keys = sign::Keys::load(&dir)?;
        let id = keys.find(key)?.id.clone();
        let revoked = keys.revoke(&id).ok_or("not trusted")?;
        keys.save(&dir)?;
        println!("pcds: {} ({}) is no longer trusted; plugins that run only because it signed them stop", revoked.name, revoked.id);
        self.plugin_reload_running();
        Ok(())
    }

    fn plugin_token_create(&self, name: &str, rest: &[&str]) -> Result {
        let (mut scopes, mut rate) = (Vec::new(), None);
        let mut it = rest.iter();
        while let Some(o) = it.next() {
            match *o {
                "--scope" => scopes = it.next().ok_or("--scope SCOPE,SCOPE")?.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect(),
                "--rate" => rate = Some(it.next().and_then(|r| r.parse::<u32>().ok()).ok_or("--rate PER_MINUTE")?),
                other => return Err(format!("unknown option {other}\n{USAGE}")),
            }
        }
        let (made, token) = crate::tokens::create(&self.plugins_dir(), name, &scopes, rate)?;
        println!("pcds: made the token {} ({}) for: {}, at most {} requests a minute", made.name, made.id, made.scopes.join(", "), made.rate_per_minute);
        println!("\n    {token}\n");
        println!("It is shown this once; the server keeps only its hash. Send it as `Authorization: Bearer TOKEN`. It opens only the requests of its scopes; everything else needs the admin token.");
        Ok(())
    }

    fn plugin_tokens(&self) -> Result {
        let all = crate::tokens::Tokens::load(&self.plugins_dir())?;
        if all.tokens.is_empty() {
            println!("no scoped tokens (pcds plugin token create NAME --scope events.read); scopes: {}", crate::tokens::SCOPES.map(|(n, _)| n).join(", "));
        }
        for t in &all.tokens {
            println!("{}  {:<20} {:>5}/min  {}", t.id, t.name, t.rate_per_minute, t.scopes.join(" "));
        }
        Ok(())
    }

    fn plugin_keys(&self) -> Result {
        let keys = sign::Keys::load(&self.plugins_dir())?;
        if keys.keys.is_empty() && keys.revoked.is_empty() {
            println!("no signing key is trusted (pcds plugin trust KEY); every plugin is approved by its own digest");
        }
        for k in &keys.keys {
            println!("{}  {:<20} vouches for: {}", k.id, k.name, if k.grant.is_empty() { "nothing (a signed plugin still needs its capabilities approved)".to_string() } else { k.grant.join(" ") });
        }
        for r in &keys.revoked {
            println!("{}  {:<20} REVOKED", r.id, r.name);
        }
        Ok(())
    }

    /// `pcds plugin config ...`: the values the admin gives a plugin (a webhook URL, a token).
    fn plugin_config(&self, args: &[&str]) -> Result {
        let dir = self.plugins_dir();
        let declared = |id: &str| -> std::result::Result<Vec<manifest::ConfigKey>, String> {
            let found = plugins::read_folder(&dir.join(id));
            found.manifest.map(|m| m.config).ok_or_else(|| format!("{id}: {}", found.status.detail().unwrap_or("not installed")))
        };
        match args {
            ["list"] | ["list", _] => {
                let all = config::Configs::load(&dir)?;
                let ids: Vec<String> = match args {
                    ["list", id] => vec![id.to_string()],
                    _ => plugins::prepare(&dir, "world").plugins.iter().map(|p| p.id.clone()).collect(),
                };
                for id in ids {
                    for k in declared(&id)? {
                        let value = all.0.get(&id).and_then(|v| v.get(&k.key));
                        let shown = match (value, k.secret) {
                            (None, _) => "not set".to_string(),
                            (Some(_), true) => "set (a secret: not shown)".to_string(),
                            (Some(v), false) => v.clone(),
                        };
                        println!("{id} {}: {shown}   {}", k.key, k.description);
                    }
                }
                Ok(())
            }
            ["set", id, key, rest @ ..] => {
                let keys = declared(id)?;
                let k = keys.iter().find(|k| k.key == *key).ok_or_else(|| format!("{id} declares no config key {key} (it declares: {})", keys.iter().map(|k| k.key.as_str()).collect::<Vec<_>>().join(", ")))?;
                let value = match (rest, k.secret) {
                    ([v], false) => v.to_string(),
                    ([], _) => read_line_hidden(&format!("{id} {key}: "), k.secret)?,
                    (_, true) => return Err(format!("{key} is a secret: leave the value off; it is read from the terminal (or standard input), so it stays out of the shell history and the process list")),
                    _ => return Err(USAGE.to_string()),
                };
                config::set(&dir, id, key, Some(value.trim_end_matches(['\r', '\n'])))?;
                println!("pcds: {id} {key} is set");
                self.plugin_reload_running();
                Ok(())
            }
            ["unset", id, key] => {
                config::set(&dir, id, key, None)?;
                println!("pcds: {id} {key} is removed");
                self.plugin_reload_running();
                Ok(())
            }
            _ => Err(USAGE.to_string()),
        }
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

/// One line from the terminal, not echoed when `hidden` (a secret), or from standard input.
fn read_line_hidden(prompt: &str, hidden: bool) -> std::result::Result<String, String> {
    let tty = unsafe { libc::isatty(0) } == 1;
    let mut saved: Option<libc::termios> = None;
    if tty {
        print!("{prompt}");
        let _ = std::io::stdout().flush();
        if hidden {
            // SAFETY: plain termios calls on standard input, undone below.
            unsafe {
                let mut t: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(0, &mut t) == 0 {
                    saved = Some(t);
                    t.c_lflag &= !libc::ECHO;
                    libc::tcsetattr(0, libc::TCSANOW, &t);
                }
            }
        }
    }
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);
    if let Some(t) = saved {
        // SAFETY: restores what tcgetattr gave.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
        println!();
    }
    read.map_err(|e| e.to_string())?;
    let line = line.trim_end_matches(['\r', '\n']).to_string();
    if line.is_empty() { Err("no value given".into()) } else { Ok(line) }
}

fn plugin_keygen(name: &str) -> Result {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
        return Err(format!("{name:?} is not a name for key files (letters, digits, - _ .)"));
    }
    let (private, public) = (PathBuf::from(".").join(format!("{name}.key")), PathBuf::from(".").join(format!("{name}.pub")));
    if private.exists() || public.exists() {
        return Err(format!("{} or {} exists already", private.display(), public.display()));
    }
    let (private_text, public_text) = sign::generate()?;
    crate::util::replace_file(&private, private_text.as_bytes(), 0o600)?;
    crate::util::replace_file(&public, public_text.as_bytes(), 0o644)?;
    println!("pcds: made {} (keep it secret: whoever has it can sign as you) and {} (give it to the admins who should trust you: pcds plugin trust {})", private.display(), public.display(), public.display());
    Ok(())
}

fn plugin_sign(key: &str, dir: &str) -> Result {
    let folder = Path::new(dir);
    let found = plugins::read_folder(folder);
    if let plugins::Status::Invalid(why) | plugins::Status::Incompatible(why) = &found.status {
        return Err(format!("{dir}: {why}: not signed"));
    }
    let private = fs::read_to_string(key).map_err(|e| format!("{key}: {e}"))?;
    let text = sign::sign(&private, &found.digest)?;
    crate::util::replace_file(&PathBuf::from(dir).join(sign::FILE), text.as_bytes(), 0o644)?;
    // Read back as a server would.
    match sign::check(folder, &found.digest, &sign::Keys::default()) {
        sign::Signed::Unknown { id } => println!("pcds: signed {dir} with the key {id}: {} (change a file of it and the signature no longer holds)", folder.join(sign::FILE).display()),
        other => return Err(format!("the signature just made does not check: {other:?}")),
    }
    Ok(())
}
