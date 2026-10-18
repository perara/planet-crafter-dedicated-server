//! The admin API client: HTTP/1.1 to 127.0.0.1 with a bearer token. And the commands that go
//! through the admin API.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::{Map, Value};

use super::{Ctx, Result};

/// The longest the admin API may take to answer (its own deadline is 10 s).
const ADMIN_TIMEOUT: Duration = Duration::from_secs(60);

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

/// One request to a server on 127.0.0.1: the body of a 2xx answer, else Err with what it said.
pub fn request(port: u16, token: &str, method: &str, path: &str, body: &str, timeout: Duration) -> Result<String> {
    let url = format!("http://127.0.0.1:{port}{path}");
    let fail = |e: String| format!("{method} {url}: {e}");
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|e| fail(e.to_string()))?;
    stream.set_read_timeout(Some(timeout)).and_then(|_| stream.set_write_timeout(Some(timeout))).map_err(|e| fail(e.to_string()))?;
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).and_then(|_| stream.write_all(body.as_bytes())).map_err(|e| fail(e.to_string()))?;
    let (status, answer) = read_response(&mut BufReader::new(stream)).map_err(fail)?;
    if status >= 400 {
        return Err(fail(format!("HTTP {status}: {answer}")));
    }
    Ok(answer)
}

/// JSON as python's json.tool prints it (4 spaces); anything else as it came.
fn print_json(text: &str) {
    let Ok(value) = serde_json::from_str::<Value>(text) else { return println!("{text}") };
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, serde_json::ser::PrettyFormatter::with_indent(b"    "));
    serde::Serialize::serialize(&value, &mut ser).expect("JSON to memory");
    println!("{}", String::from_utf8_lossy(&out));
}

/// `pcds rules set` arguments as a JSON merge patch: SECTION.RULE=VALUE, a JSON value or else
/// text; null resets a rule.
pub fn rules_patch(args: &[String]) -> Result<Value> {
    let mut patch = Value::Object(Map::new());
    for arg in args {
        let bad = || format!("{arg}: expected SECTION.RULE=VALUE");
        let (key, value) = arg.split_once('=').filter(|(k, _)| k.contains('.')).ok_or_else(bad)?;
        let value = serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_string()));
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
        let addr = self.env_get("PCDS_ADMIN").filter(|a| a != "off");
        let (Some(addr), Some(token)) = (addr, self.env_get("PCDS_ADMIN_TOKEN")) else {
            return Err(format!("no admin API in {} (PCDS_ADMIN / PCDS_ADMIN_TOKEN)", self.env_file.display()));
        };
        let port = addr
            .rsplit_once(':')
            .filter(|(host, _)| ["127.0.0.1", "0.0.0.0", "localhost"].contains(host))
            .and_then(|(_, port)| port.parse().ok())
            .ok_or_else(|| format!("PCDS_ADMIN={addr}: pcds reaches the admin API on 127.0.0.1 only (bind it to 127.0.0.1 or 0.0.0.0)"))?;
        let body = body.filter(|b| !b.is_empty()).unwrap_or(if method == "GET" { "" } else { "{}" });
        request(port, &token, method, path, body, ADMIN_TIMEOUT)
    }

    fn api_json(&self, method: &str, path: &str) -> Result<Value> {
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
        match self.game_dir(None).and_then(|game| super::build_id(&game)) {
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
        let answer = match a.as_slice() {
            [] => self.api("GET", "/api/rules", None)?,
            ["set"] => return Err("pcds rules set SECTION.RULE=VALUE ... ('pcds rules' lists them)".into()),
            ["set", ..] => self.api("POST", "/api/rules", Some(&rules_patch(&args[1..])?.to_string()))?,
            ["reset"] => {
                let rules = self.api_json("GET", "/api/rules")?;
                let reset: Map<String, Value> = rules["defaults"].as_object().into_iter().flatten().map(|(k, _)| (k.clone(), Value::Null)).collect();
                self.api("POST", "/api/rules", Some(&Value::Object(reset).to_string()))?
            }
            _ => return Err("pcds rules [set SECTION.RULE=VALUE ... | reset]".into()),
        };
        print_json(&answer);
        Ok(())
    }
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

    #[test]
    fn requests_go_to_127_0_0_1_with_the_token() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
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
        assert_eq!(request(port, "secret", "POST", "/api/save", "{}", ADMIN_TIMEOUT).unwrap(), "pong");
        assert!(request(port, "secret", "POST", "/api/save", "{}", ADMIN_TIMEOUT).unwrap_err().contains("HTTP 401: no, no."));
        let seen = server.join().unwrap();
        assert!(seen[0].starts_with("POST /api/save HTTP/1.1\r\n") && seen[0].contains("Authorization: Bearer secret\r\n") && seen[0].ends_with("Content-Length: 2\r\nConnection: close\r\n\r\n{}"));
    }

    #[test]
    fn rule_arguments_become_a_merge_patch() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let patch = rules_patch(&args(&["speed.growth=2", "gauges.noDeath=true", "death.consequences=DropAllItems", "limits.buildings.Drill4=10", "death.dropChance=null"])).unwrap();
        assert_eq!(patch, json!({"speed": {"growth": 2}, "gauges": {"noDeath": true}, "death": {"consequences": "DropAllItems", "dropChance": null}, "limits": {"buildings": {"Drill4": 10}}}));
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
        lines.push(format!("invite code {code}"));
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
