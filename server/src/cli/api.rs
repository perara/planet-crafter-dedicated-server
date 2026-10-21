//! The admin API client: HTTP/1.1 to the loopback with a bearer token. And the commands that go
//! through the admin API.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use serde_json::{Map, Value};

use super::{Ctx, Result};

/// The longest the admin API may take to answer (its own deadline is 10 s), and for the world
/// controls (stop, start, switch), which it waits for up to 200 s (admin.rs WORLD_ANSWER).
const ADMIN_TIMEOUT: Duration = Duration::from_secs(60);
const WORLD_TIMEOUT: Duration = Duration::from_secs(210);

/// An HTTP/1.1 answer: its status and its body (Content-Length long, else to the end). The
/// admin API sends a Content-Length (admin.rs): a chunked answer is refused rather than read as
/// its body.
pub fn read_response(r: &mut impl BufRead) -> Result<(u16, String)> {
    let mut line = String::new();
    r.read_line(&mut line).map_err(|e| e.to_string())?;
    let status = line.strip_prefix("HTTP/1.").and_then(|l| l.split_whitespace().nth(1)).and_then(|s| s.parse().ok());
    let status = status.ok_or_else(|| format!("not an HTTP answer: {:?}", line.trim_end()))?;
    let mut length = None;
    loop {
        let mut header = String::new();
        if r.read_line(&mut header).map_err(|e| e.to_string())? == 0 || header.trim().is_empty() {
            break;
        }
        let Some((k, v)) = header.split_once(':') else { continue };
        if k.trim().eq_ignore_ascii_case("transfer-encoding") {
            return Err(format!("unexpected Transfer-Encoding: {} (pcds reads answers with a Content-Length)", v.trim()));
        }
        if k.trim().eq_ignore_ascii_case("content-length") {
            length = Some(v.trim().parse::<usize>().map_err(|_| format!("bad {k}: {}", v.trim()))?);
        }
    }
    let mut body = Vec::new();
    match length {
        Some(n) => {
            body.resize(n, 0);
            r.read_exact(&mut body)
        }
        None => r.read_to_end(&mut body).map(|_| ()),
    }
    .map_err(|e| e.to_string())?;
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

/// One request to a server on the loopback: the body of a 2xx answer, else Err with what it said.
pub fn request(addr: SocketAddr, token: &str, method: &str, path: &str, body: &str, timeout: Duration) -> Result<String> {
    let url = format!("http://{addr}{path}");
    let fail = |e: String| format!("{method} {url}: {e}");
    let mut stream = TcpStream::connect(addr).map_err(|e| fail(e.to_string()))?;
    stream.set_read_timeout(Some(timeout)).and_then(|_| stream.set_write_timeout(Some(timeout))).map_err(|e| fail(e.to_string()))?;
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).and_then(|_| stream.write_all(body.as_bytes())).map_err(|e| fail(e.to_string()))?;
    let (status, answer) = read_response(&mut BufReader::new(stream)).map_err(fail)?;
    if status >= 400 {
        return Err(fail(format!("HTTP {status}: {answer}")));
    }
    Ok(answer)
}

/// An admin API failure in words that say what to do: the server's own message for a refused
/// request, and for the two usual mistakes (a stopped server, a token the server does not run with).
fn explain(e: String, addr: &str) -> String {
    if e.contains("Connection refused") {
        return format!("the server does not answer on {addr}: it is stopped or still starting ('pcds status'; 'pcds start'; 'pcds logs' when it stops again)");
    }
    if e.contains("HTTP 401") {
        return "the server refused the admin token in server.env: it reads the token when it starts, so after a change run 'pcds restart'".into();
    }
    super::upgrade::api_error(e)
}

/// `pcds rules set` arguments as a JSON merge patch: SECTION.RULE=VALUE, a JSON value or else
/// text; null resets a rule.
pub fn rules_patch(args: &[String]) -> Result<Value> {
    let mut patch = Value::Object(Map::new());
    for arg in args {
        let bad = || format!("{arg}: expected SECTION.RULE=VALUE");
        let (key, value) = arg.split_once('=').filter(|(k, _)| k.contains('.')).ok_or_else(bad)?;
        // A rule that is text takes 12345 or true as that text (a bucket or a secret of digits):
        // read as a JSON number it would be refused as "not text" with no way to say it but quotes.
        let kind = crate::config::knob_for(key).map(|k| k.kind);
        let is_text = kind == Some(crate::config::Type::String);
        // A list rule takes JSON (["a","b"]) or plain names split at commas: world.dlcPlanets=Humble,Toxicity.
        let is_list = kind == Some(crate::config::Type::List);
        let value = match serde_json::from_str::<Value>(value) {
            Ok(Value::Number(_) | Value::Bool(_)) if is_text => Value::String(value.to_string()),
            Ok(json) => json,
            Err(_) if is_list => Value::Array(value.split(',').filter(|n| !n.is_empty()).map(|n| Value::String(n.trim().to_string())).collect()),
            Err(_) => Value::String(value.to_string()),
        };
        let mut path: Vec<&str> = key.split('.').collect();
        let last = path.pop().unwrap_or_default();
        let mut node = &mut patch;
        for p in path {
            if !node[p].is_object() && !node[p].is_null() {
                return Err(format!("{arg}: {p} is a rule, not a section"));
            }
            node = &mut node[p];
        }
        node[last] = value;
    }
    Ok(patch)
}

impl Ctx {
    /// The admin API of this installation's server.
    pub fn api(&self, method: &str, path: &str, body: Option<&str>) -> Result<String> {
        let addr = self.effective("PCDS_ADMIN").filter(|a| a != "off");
        let (Some(addr), Some(token)) = (addr, self.effective("PCDS_ADMIN_TOKEN")) else {
            return Err(self.no_admin());
        };
        let sock = crate::config::admin_loopback(&addr)?;
        if !["GET", "POST"].contains(&method) {
            return Err(format!("{method}: the admin API takes GET and POST (docs/admin-api.md)"));
        }
        let body = body.filter(|b| !b.is_empty()).unwrap_or(if method == "GET" { "" } else { "{}" });
        request(sock, &token, method, path, body, if path.starts_with("/api/world/") { WORLD_TIMEOUT } else { ADMIN_TIMEOUT }).map_err(|e| explain(e, &addr))
    }

    /// `pcds health`: Ok when the server answers GET /api/health with ok, which needs no token; Err
    /// (exit 1) with what to do otherwise. The Docker image's health check runs this.
    pub fn health(&self) -> Result {
        let addr = self.effective("PCDS_ADMIN").filter(|a| a != "off").ok_or_else(|| self.no_admin())?;
        let sock = crate::config::admin_loopback(&addr)?;
        match request(sock, "", "GET", "/api/health", "", Duration::from_secs(10)) {
            Ok(_) => {
                println!("ok");
                Ok(())
            }
            Err(e) if e.contains("HTTP 503") => Err("the server runs but serves no world: it is stopped, still loading (the first start reads the game tables), or starting the game process again after a crash ('pcds status' says which, 'pcds logs' why)".into()),
            Err(e) => Err(explain(e, &addr)),
        }
    }

    /// Why there is no admin API to ask: no server set up here yet, or the settings turn it off.
    pub(super) fn no_admin(&self) -> String {
        if self.env_file.is_file() {
            format!("the admin API is off or has no token in {} (PCDS_ADMIN, PCDS_ADMIN_TOKEN): 'pcds config' edits it, 'pcds restart' applies it", self.env_file.display())
        } else {
            format!("no server is set up here ({} is missing): 'pcds up' sets one up", self.env_file.display())
        }
    }

    pub(super) fn api_json(&self, method: &str, path: &str) -> Result<Value> {
        let text = self.api(method, path, None)?;
        serde_json::from_str(&text).map_err(|e| format!("{method} {path}: {e}"))
    }

    /// Err (exit 1) when systemctl fails, or when the server runs and its admin API does not
    /// answer (the Docker image's health check).
    pub fn status(&self) -> Result {
        let game = self.game_line();
        if !self.active()? {
            println!("state=stopped");
            self.print_banks(None);
            game.iter().for_each(|l| println!("{l}"));
            return Ok(());
        }
        println!("state=running pid={}", self.server_pid().unwrap_or_default());
        game.iter().for_each(|l| println!("{l}"));
        match self.api_json("GET", "/api/status") {
            Ok(status) => {
                self.print_banks(Some(&status["banks"]));
                for line in status_lines(&status) {
                    println!("{line}");
                }
                if status["inviteCode"].is_null() && status["state"] != "stopped" && self.env_get("PCDS_STEAM").as_deref() != Some("off") {
                    println!("no invite code: Steam did not log in ('pcds logs' says why); players can still join by direct IP");
                }
                Ok(())
            }
            Err(e) => {
                self.print_banks(None);
                Err(format!("admin API: {e}"))
            }
        }
    }

    /// A line for `pcds status` when the installed game has no tables for the server (a game
    /// update that pcds update has not read yet), or cannot be found.
    fn game_line(&self) -> Option<String> {
        match self.game_dir(None).and_then(|game| crate::tables::build_id(&game)) {
            Err(e) => Some(format!("game: {e}")),
            Ok(build) => self.tables(&build).err().map(|e| format!("game build {build} has no tables: {e}")),
        }
    }

    /// GET /api/status as JSON, for scripts.
    pub fn status_json(&self) -> Result {
        println!("{}", serde_json::to_string_pretty(&self.api_json("GET", "/api/status")?).unwrap_or_default());
        Ok(())
    }

    pub fn players(&self) -> Result {
        let status = self.api_json("GET", "/api/status")?;
        let players = status["players"].as_array().cloned().unwrap_or_default();
        for p in &players {
            let steam = match p["steamId"].as_str().filter(|s| !s.is_empty()) {
                Some(id) if p["verified"].as_bool() == Some(true) => format!("{id} (verified)"),
                Some(id) => id.to_string(),
                None => "-".into(),
            };
            let name = p["name"].as_str().filter(|n| !n.is_empty()).unwrap_or("(joining)");
            println!("{:>4}  {:<24} {:<16} {:<28} {}s", p["id"].to_string(), name, p["ip"].as_str().unwrap_or(""), steam, p["connectedSeconds"]);
        }
        println!("{} online", players.len());
        Ok(())
    }

    /// The invite code, when the server has one.
    pub fn invite_code(&self) -> Option<String> {
        let status = self.api_json("GET", "/api/status").ok()?;
        status["inviteCode"].as_str().filter(|c| !c.is_empty()).map(str::to_string)
    }

    pub fn code(&self) -> Result {
        let code = self.invite_code().ok_or("no invite code (server stopped, Steam off, or Steam not reachable; see pcds logs)")?;
        println!("{code}");
        Ok(())
    }

    /// The rules through the admin API: shown, set, or all reset to the game's behaviour.
    pub fn rules(&self, args: &[String]) -> Result {
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        match a.as_slice() {
            [] => {
                for line in rule_lines(&self.api_json("GET", "/api/config")?) {
                    println!("{line}");
                }
            }
            ["set"] => return Err("usage: pcds rules set SECTION.RULE=VALUE ... ('pcds rules' lists them)".into()),
            ["set", ..] => {
                let patch = rules_patch(&args[1..])?;
                self.api("POST", "/api/rules", Some(&patch.to_string()))?;
                for (key, value) in patch_leaves(&patch) {
                    let (secret, shown) = (crate::config::knob_for(&key).is_some_and(|k| k.secret), show(&value));
                    println!("{key} {}", if value.is_null() { "is back to its default".into() } else if secret { "is set".into() } else { format!("= {shown}") });
                }
            }
            ["reset"] => {
                let rules = self.api_json("GET", "/api/rules")?;
                let reset: Map<String, Value> = rules["defaults"].as_object().into_iter().flatten().map(|(k, _)| (k.clone(), Value::Null)).collect();
                // A world standing on a DLC planet refuses to lose the statement that you own it: reset
                // everything else and say so (world.dlcPlanets stays until you unset it by name).
                match self.api("POST", "/api/rules", Some(&Value::Object(reset.clone()).to_string())) {
                    Err(e) if e.contains("world.dlcPlanets") => {
                        let mut world = Map::new();
                        for key in rules["defaults"]["world"].as_object().into_iter().flatten().map(|(k, _)| k).filter(|k| *k != "dlcPlanets") {
                            world.insert(key.clone(), Value::Null);
                        }
                        let mut keep = reset;
                        keep.insert("world".into(), Value::Object(world));
                        self.api("POST", "/api/rules", Some(&Value::Object(keep).to_string()))?;
                        println!("every rule is back to its default except world.dlcPlanets: the world stands on a DLC planet you state you own ({e})");
                    }
                    other => {
                        other?;
                        println!("every rule is back to its default");
                    }
                }
            }
            _ => return Err("usage: pcds rules [set SECTION.RULE=VALUE ... | reset]".into()),
        }
        Ok(())
    }
}

/// A rule's value as one short piece of text.
fn show(v: &Value) -> String {
    match v {
        Value::Null => "unset".into(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.as_f64().filter(|f| f.fract() == 0.0 && f.abs() < 1e15).map_or_else(|| n.to_string(), |f| format!("{f}")),
        other => other.to_string(),
    }
}

/// The "section.rule" paths a merge patch names, with their new values (a map such as
/// limits.buildings.Drill4 is named down to the building).
fn patch_leaves(patch: &Value) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for (section, rules) in patch.as_object().into_iter().flatten() {
        for (rule, value) in rules.as_object().into_iter().flatten() {
            match value.as_object().filter(|_| rule == "buildings") {
                Some(map) => out.extend(map.iter().map(|(k, v)| (format!("{section}.{rule}.{k}"), v.clone()))),
                None => out.push((format!("{section}.{rule}"), value.clone())),
            }
        }
    }
    out
}

/// `pcds rules`: every rule with its value and default, a star on the ones that were changed.
fn rule_lines(config: &Value) -> Vec<String> {
    let knobs: Vec<&Value> = config["knobs"].as_array().into_iter().flatten().filter(|k| k["key"].as_str().is_some_and(|key| !key.starts_with("PCDS_") && !key.starts_with("access."))).collect();
    let mut lines = vec![format!("  {:<31} {:<14} {}", "rule", "value", "default")];
    for k in &knobs {
        let (key, secret) = (k["key"].as_str().unwrap_or_default(), k["secret"].as_bool() == Some(true));
        let (value, default) = (&k["value"], &k["default"]);
        let shown = if secret && !value.is_null() && value != &Value::Array(vec![]) { "(set)".to_string() } else { show(value) };
        let changed = if value == default { ' ' } else { '*' };
        lines.push(format!("{changed} {key:<31} {shown:<14} {}{}", show(default), k["unit"].as_str().filter(|u| !u.is_empty() && !default.is_null()).map(|u| format!(" {u}")).unwrap_or_default()));
    }
    lines.push("(* changed. 'pcds rules set SECTION.RULE=VALUE ...' changes them live, 'null' as the value and 'pcds rules reset' go back to the default; README, \"Server rules\")".into());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Read;

    fn parse(raw: &str) -> Result<(u16, String)> {
        read_response(&mut std::io::Cursor::new(raw.as_bytes()))
    }

    #[test]
    fn http_answers_are_read_by_their_length() {
        assert_eq!(parse("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"ok\":true}").unwrap(), (200, "{\"ok\":true}".into()));
        // Keep-alive: nothing past Content-Length is read.
        assert_eq!(parse("HTTP/1.1 400 Bad Request\r\ncontent-length: 2\r\n\r\n{}NEXT").unwrap(), (400, "{}".into()));
        // No length: to the end.
        assert_eq!(parse("HTTP/1.0 404 Not Found\r\n\r\nno such page").unwrap(), (404, "no such page".into()));
        assert!(parse("HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort").is_err());
        assert!(parse("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\npong\r\n0\r\n\r\n").unwrap_err().contains("chunked"));
        assert!(parse("SSH-2.0-OpenSSH\r\n").is_err());
        assert!(parse("").is_err());
    }

    /// `pcds health` asks the health route without a token and turns a 503 into what it means.
    #[test]
    fn health_is_asked_without_a_token_and_a_503_is_explained() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut heads = Vec::new();
            for answer in ["HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}", "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 12\r\n\r\n{\"ok\":false}"] {
                let (mut s, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(s.try_clone().unwrap());
                let mut head = String::new();
                while !head.ends_with("\r\n\r\n") {
                    reader.read_line(&mut head).unwrap();
                }
                heads.push(head);
                s.write_all(answer.as_bytes()).unwrap();
            }
            heads
        });
        let dir = super::super::test_dir("health");
        let ctx = super::super::test_ctx(&dir);
        assert!(ctx.health().unwrap_err().contains("no server is set up"));
        std::fs::create_dir_all(&ctx.run).unwrap();
        std::fs::write(&ctx.env_file, format!("PCDS_ADMIN={addr}\n")).unwrap();
        ctx.health().unwrap();
        let e = ctx.health().unwrap_err();
        assert!(e.contains("serves no world") && e.contains("pcds status"), "{e}");
        let heads = server.join().unwrap();
        assert!(heads[0].starts_with("GET /api/health HTTP/1.1\r\n") && heads.iter().all(|h| h.contains("Bearer \r\n")), "{heads:?}");
        std::fs::write(&ctx.env_file, "PCDS_ADMIN=off\n").unwrap();
        assert!(ctx.health().unwrap_err().contains("the admin API is off"));
        // Nothing listens there (port 1).
        std::fs::write(&ctx.env_file, "PCDS_ADMIN=127.0.0.1:1\n").unwrap();
        assert!(ctx.health().unwrap_err().contains("does not answer"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn requests_go_to_127_0_0_1_with_the_token() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let listener_addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut answers = Vec::new();
            for answer in ["HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong", "HTTP/1.1 401 Unauthorized\r\nContent-Length: 7\r\n\r\nno, no."] {
                let (mut s, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(s.try_clone().unwrap());
                let mut request = String::new();
                while !request.ends_with("\r\n\r\n") {
                    reader.read_line(&mut request).unwrap();
                }
                let mut body = [0u8; 2];
                reader.read_exact(&mut body).unwrap();
                answers.push(request + std::str::from_utf8(&body).unwrap());
                s.write_all(answer.as_bytes()).unwrap();
            }
            answers
        });
        assert_eq!(request(listener_addr, "secret", "POST", "/api/save", "{}", ADMIN_TIMEOUT).unwrap(), "pong");
        assert!(request(listener_addr, "secret", "POST", "/api/save", "{}", ADMIN_TIMEOUT).unwrap_err().contains("HTTP 401: no, no."));
        assert!(explain("POST http://127.0.0.1:1/api/save: HTTP 401: x".into(), "127.0.0.1:1").contains("pcds restart"));
        assert!(explain("GET http://x/api/status: Connection refused (os error 111)".into(), "127.0.0.1:1").contains("pcds start"));
        assert_eq!(explain("POST http://x/api/rules: HTTP 400: {\"error\":\"speed.growth: 0 is not a number from 0.01 to 100\"}".into(), "a"), "speed.growth: 0 is not a number from 0.01 to 100");
        let seen = server.join().unwrap();
        assert!(seen[0].starts_with("POST /api/save HTTP/1.1\r\n") && seen[0].contains("Authorization: Bearer secret\r\n") && seen[0].ends_with("Content-Length: 2\r\nConnection: close\r\n\r\n{}"));
    }

    /// `pcds rules reset` on a world that stands on a DLC planet resets every rule but the statement
    /// that you own the planet (the server refuses to lose it), and says so.
    #[test]
    fn rules_reset_keeps_the_dlc_statement_when_the_world_needs_it() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let defaults = r#"{"defaults":{"speed":{"growth":1},"world":{"dlcPlanets":[],"pauseWhenEmpty":false}}}"#;
            let refusal = r#"{"error":"world.dlcPlanets: the world is on Humble, a DLC planet"}"#;
            let mut bodies = Vec::new();
            for (status, answer) in [("200 OK", defaults), ("400 Bad Request", refusal), ("200 OK", "{}")] {
                let (mut s, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(s.try_clone().unwrap());
                let (mut head, mut length) = (String::new(), 0usize);
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if let Some(n) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = n.trim().parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                    head += &line;
                }
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).unwrap();
                bodies.push(String::from_utf8(body).unwrap());
                s.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{answer}", answer.len()).as_bytes()).unwrap();
            }
            bodies
        });
        let dir = super::super::test_dir("rules-reset");
        let ctx = super::super::test_ctx(&dir);
        std::fs::create_dir_all(&ctx.run).unwrap();
        std::fs::write(&ctx.env_file, format!("PCDS_ADMIN={addr}\nPCDS_ADMIN_TOKEN=t\n")).unwrap();
        ctx.rules(&["reset".to_string()]).unwrap();
        let bodies = server.join().unwrap();
        let second: Value = serde_json::from_str(&bodies[2]).unwrap();
        assert_eq!(second, json!({"speed": null, "world": {"pauseWhenEmpty": null}}), "everything but world.dlcPlanets");
        assert_eq!(serde_json::from_str::<Value>(&bodies[1]).unwrap(), json!({"speed": null, "world": null}));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rule_arguments_become_a_merge_patch() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let patch = rules_patch(&args(&["speed.growth=2", "gauges.noDeath=true", "death.consequences=DropAllItems", "limits.buildings.Drill4=10", "death.dropChance=null"])).unwrap();
        assert_eq!(patch, json!({"speed": {"growth": 2}, "gauges": {"noDeath": true}, "death": {"consequences": "DropAllItems", "dropChance": null}, "limits": {"buildings": {"Drill4": 10}}}));
        // Text rules take digits as text; null still resets; quotes still work.
        let text = rules_patch(&args(&["chat.secret=12345", "backup.s3Bucket=123456789", "chat.commandPrefix=true", "backup.s3Region=null", "backup.s3Prefix=\"7\""])).unwrap();
        assert_eq!(text, json!({"chat": {"secret": "12345", "commandPrefix": "true"}, "backup": {"s3Bucket": "123456789", "s3Region": null, "s3Prefix": "7"}}));
        // A list rule takes names split at commas, or JSON; empty clears it.
        let lists = rules_patch(&args(&["world.dlcPlanets=Humble,Toxicity", "chat.webhooks=[\"https://a.example/x,y\"]", "access.whitelist=", "chat.exec=null"])).unwrap();
        assert_eq!(lists, json!({"world": {"dlcPlanets": ["Humble", "Toxicity"]}, "chat": {"webhooks": ["https://a.example/x,y"], "exec": null}, "access": {"whitelist": []}}));
        assert!(rules_patch(&args(&["speed=2"])).is_err());
        assert!(rules_patch(&args(&["speed.growth"])).is_err());
        assert!(rules_patch(&args(&["speed.growth=2", "speed.growth.x=1"])).is_err());
    }
}

/// `pcds status`: the running server in a few lines (`pcds status --json` has all of it).
fn status_lines(s: &Value) -> Vec<String> {
    let text = |v: &Value| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
    if s["state"] == "stopped" {
        return vec![format!("world {} is stopped: players cannot join (start it from the admin page, or POST /api/world/start)", text(&s["save"]))];
    }
    let players = s["players"].as_array().map_or(0, Vec::len);
    let up = s["uptimeSeconds"].as_u64().unwrap_or(0);
    let stage = s["stages"]["current"].as_str().map(|k| k.rsplit(['-', ':']).next().unwrap_or(k).to_string());
    let u = &s["update"];
    let mut lines = vec![
        format!("world {} on {}, game {}, pcds {}", text(&s["save"]), text(&s["planet"]), text(&s["gameVersion"]), text(&s["serverVersion"])),
        format!("{players} online, up {}h {}m, loop {}/s ({}% busy)", up / 3600, up % 3600 / 60, s["fps"], s["busyPercent"]),
        format!("terraformation {:.0} Ti{}", s["terraformation"].as_f64().unwrap_or(0.0), stage.map(|n| format!(", stage {n}")).unwrap_or_default()),
        format!("network {}, port {}, autosave every {} min", text(&s["network"]), s["port"], s["autosaveMinutes"]),
    ];
    if let Some(code) = s["inviteCode"].as_str() {
        lines.push(format!("invite code {code} (it changes at every restart unless PCDS_STEAM_TOKEN is set)"));
    }
    match (u["latest"].as_str(), u["available"].as_bool()) {
        (Some(tag), Some(true)) => lines.push(format!("release {tag} available: pcds upgrade")),
        (Some(tag), _) => lines.push(format!("latest release {tag}: up to date")),
        _ => {}
    }
    if let Some(doing) = u["doing"].as_str() {
        lines.push(format!("update under way: {doing}"));
    }
    lines
}

#[cfg(test)]
mod status_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn status_reads_as_a_few_lines() {
        let s = json!({"save": "world", "planet": "Prime", "gameVersion": "2.103", "serverVersion": "0.6.0", "players": [{}, {}], "uptimeSeconds": 3725,
            "fps": 197, "busyPercent": 0.6, "terraformation": 16271011.0, "stages": {"current": "TerraformStage:TerraformStage4-Water"},
            "network": "ip", "port": 7777, "autosaveMinutes": 5, "inviteCode": null, "update": {"latest": "v0.6.1", "available": true}});
        assert_eq!(status_lines(&s), [
            "world world on Prime, game 2.103, pcds 0.6.0",
            "2 online, up 1h 2m, loop 197/s (0.6% busy)",
            "terraformation 16271011 Ti, stage Water",
            "network ip, port 7777, autosave every 5 min",
            "release v0.6.1 available: pcds upgrade",
        ]);
        assert_eq!(status_lines(&json!({"state": "stopped", "save": "world"}))[0], "world world is stopped: players cannot join (start it from the admin page, or POST /api/world/start)");
    }
}
