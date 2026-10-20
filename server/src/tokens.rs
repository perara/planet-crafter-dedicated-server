//! Scoped tokens for programs that integrate with the server (docs/plugins.md, "Scoped tokens"): a
//! token that may read the event stream and say something in the chat, and nothing else, so that a
//! Discord bridge does not hold the admin token.
//!
//! - `run/plugins/tokens.json` (0600) holds, for each token, a name, the **sha256 of the token** (the
//!   token itself is shown once, when it is made, and kept nowhere), its scopes and its rate. The
//!   admin thread of the front reads the file (again when it changes: a revoked token is refused on
//!   its next request) and checks `Authorization: Bearer pcdst_...` against it.
//! - A token authorises the (method, path) pairs of its scopes (`scope_for`) and nothing else: the
//!   rest of the API, and `/api/plugins/tokens` above all, take the admin token only.
//! - The hash is compared in time that does not depend on where it differs; a token is never
//!   written to the log, an answer or a listing.
//! - Each token has a request rate per minute; over it the answer is 429 with `Retry-After`.
//!
//! The module is in the base layer: the front's HTTP thread checks, the bank lists and creates
//! (the dashboard), the command line does both without a server.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

pub const FILE: &str = "tokens.json";
/// What every token begins with, so a leaked one is recognised by a scanner.
pub const PREFIX: &str = "pcdst_";
pub const DEFAULT_RATE: u32 = 600;
pub const MAX_RATE: u32 = 6000;
pub const MAX_TOKENS: usize = 32;

/// The scopes, and the requests each allows.
pub const SCOPES: [(&str, &str); 5] = [
    ("events.read", "read the plugin event stream (POST /api/plugins/events)"),
    ("chat.read", "read the chat (GET /api/chat/events)"),
    ("chat.send", "write to the chat as the server (POST /api/chat)"),
    ("player.kick", "kick a player (POST /api/kick)"),
    ("player.teleport", "move a player (POST /api/teleport)"),
];

/// The scope a request needs, if a token may make it at all.
pub fn scope_for(method: &str, path: &str) -> Option<&'static str> {
    Some(match (method, path) {
        ("POST", "/api/plugins/events") => "events.read",
        ("GET", "/api/chat/events") => "chat.read",
        ("POST", "/api/chat") => "chat.send",
        ("POST", "/api/kick") => "player.kick",
        ("POST", "/api/teleport") => "player.teleport",
        _ => return None,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Token {
    /// The first 8 hex digits of the hash: what listings call it.
    pub id: String,
    pub name: String,
    /// sha256 of the token, hex.
    pub hash: String,
    pub scopes: Vec<String>,
    pub rate_per_minute: u32,
    /// Unix seconds.
    pub created_at: u64,
}

impl Token {
    /// The token as a listing shows it: everything but the hash.
    pub fn public(&self) -> serde_json::Value {
        serde_json::json!({"id": self.id, "name": self.name, "scopes": self.scopes, "ratePerMinute": self.rate_per_minute, "createdAt": self.created_at})
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tokens {
    #[serde(default)]
    pub tokens: Vec<Token>,
}

fn hash(token: &str) -> String {
    crate::util::to_hex(ring::digest::digest(&ring::digest::SHA256, token.as_bytes()).as_ref())
}

/// Equal byte strings, in time that depends on their length and not on where they differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |d, (x, y)| d | (x ^ y)) == 0
}

fn now() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Tokens {
    /// The file in `dir`; none there is no tokens. One that is there and does not read is an
    /// error: no token is valid then.
    pub fn load(dir: &Path) -> Result<Tokens, String> {
        let path = dir.join(FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) if text.trim().is_empty() => Ok(Tokens::default()),
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Tokens::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn save(&self, dir: &Path) -> Result<(), String> {
        let path = dir.join(FILE);
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        crate::save::write_atomic(&path.to_string_lossy(), format!("{text}\n").as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// A token by its id, or its name.
    pub fn find(&self, id_or_name: &str) -> Result<&Token, String> {
        self.tokens.iter().find(|t| t.id == id_or_name || t.name == id_or_name).ok_or_else(|| format!("no token {id_or_name}"))
    }
}

/// Makes a token and stores its hash. The token is returned here and never again.
pub fn create(dir: &Path, name: &str, scopes: &[String], rate: Option<u32>) -> Result<(Token, String), String> {
    if name.is_empty() || name.len() > 32 || !name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
        return Err("a token's name is 1 to 32 letters, digits, . _ or -".into());
    }
    if scopes.is_empty() {
        return Err(format!("a token needs at least one scope: {}", SCOPES.map(|(n, _)| n).join(", ")));
    }
    if let Some(bad) = scopes.iter().find(|s| !SCOPES.iter().any(|(n, _)| n == s)) {
        return Err(format!("{bad:?} is not a scope ({})", SCOPES.map(|(n, _)| n).join(", ")));
    }
    let rate = rate.unwrap_or(DEFAULT_RATE);
    if rate == 0 || rate > MAX_RATE {
        return Err(format!("the rate is 1 to {MAX_RATE} requests a minute"));
    }
    let mut all = Tokens::load(dir)?;
    if all.tokens.len() >= MAX_TOKENS {
        return Err(format!("{MAX_TOKENS} tokens already: revoke one"));
    }
    if all.tokens.iter().any(|t| t.name == name) {
        return Err(format!("a token named {name} exists: revoke it first, or choose another name"));
    }
    let mut random = [0u8; 24];
    SystemRandom::new().fill(&mut random).map_err(|_| "the system has no random numbers to make a token from")?;
    let token = format!("{PREFIX}{}", crate::util::to_hex(&random));
    let mut scopes: Vec<String> = scopes.to_vec();
    scopes.sort();
    scopes.dedup();
    let h = hash(&token);
    let made = Token { id: h[..8].to_string(), name: name.to_string(), hash: h, scopes, rate_per_minute: rate, created_at: now() };
    all.tokens.push(made.clone());
    all.save(dir)?;
    Ok((made, token))
}

/// Removes a token (by id or name); it is refused from its next request.
pub fn revoke(dir: &Path, id_or_name: &str) -> Result<Token, String> {
    let mut all = Tokens::load(dir)?;
    let id = all.find(id_or_name)?.id.clone();
    let at = all.tokens.iter().position(|t| t.id == id).ok_or("no such token")?;
    let gone = all.tokens.remove(at);
    all.save(dir)?;
    Ok(gone)
}

/// Why a request with a token was not let through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denied {
    /// No such token (or none is valid: the file does not read).
    Unknown,
    /// A good token without the scope the request needs (`None`: no token may make this request).
    Scope(Option<&'static str>),
    /// Over its rate: seconds to wait.
    Rate(u64),
}

struct Cache {
    stamp: Option<(u128, u64)>,
    tokens: Vec<Token>,
    /// Per token id: the minute it counts in and its requests in it.
    windows: HashMap<String, (Instant, u32)>,
    error_said: bool,
}

/// The tokens of one installation, as the front's HTTP thread checks them.
pub struct Checker {
    dir: Option<PathBuf>,
    cache: Mutex<Cache>,
}

fn stamp(path: &Path) -> Option<(u128, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_nanos(), m.len()))
}

impl Checker {
    /// Tokens kept in `dir/tokens.json`.
    pub fn new(dir: PathBuf) -> Checker {
        Checker { dir: Some(dir), cache: Mutex::new(Cache { stamp: None, tokens: Vec::new(), windows: HashMap::new(), error_said: false }) }
    }

    /// No tokens at all: only the admin token opens anything.
    #[cfg(test)]
    pub fn none() -> Checker {
        Checker { dir: None, cache: Mutex::new(Cache { stamp: None, tokens: Vec::new(), windows: HashMap::new(), error_said: false }) }
    }

    /// Whether `authorization` (the header's value) is a token that may make this request: the
    /// token's id, or why not. Counts the request against the token's rate.
    pub fn authorize(&self, authorization: &str, method: &str, path: &str) -> Result<String, Denied> {
        let presented = authorization.strip_prefix("Bearer ").filter(|t| t.starts_with(PREFIX) && t.len() <= 128).ok_or(Denied::Unknown)?;
        let dir = self.dir.as_ref().ok_or(Denied::Unknown)?;
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let file = dir.join(FILE);
        let seen = stamp(&file);
        if seen != cache.stamp || (seen.is_none() && !cache.tokens.is_empty()) {
            match Tokens::load(dir) {
                Ok(t) => {
                    cache.tokens = t.tokens;
                    cache.error_said = false;
                }
                Err(e) => {
                    // Fail closed, say so once.
                    cache.tokens.clear();
                    if !std::mem::replace(&mut cache.error_said, true) {
                        eprintln!("admin: {e}: no scoped token is valid until that is fixed");
                    }
                }
            }
            cache.stamp = seen;
            let ids: Vec<String> = cache.tokens.iter().map(|t| t.id.clone()).collect();
            cache.windows.retain(|id, _| ids.contains(id));
        }
        let wanted = hash(presented);
        // Every stored hash is compared, in time that does not depend on where they differ.
        let mut found: Option<Token> = None;
        for t in &cache.tokens {
            if same(t.hash.as_bytes(), wanted.as_bytes()) {
                found = Some(t.clone());
            }
        }
        let token = found.ok_or(Denied::Unknown)?;
        let needs = scope_for(method, path);
        if !needs.is_some_and(|n| token.scopes.iter().any(|s| s == n)) {
            return Err(Denied::Scope(needs));
        }
        let (since, count) = cache.windows.entry(token.id.clone()).or_insert((Instant::now(), 0));
        if since.elapsed() >= Duration::from_secs(60) {
            (*since, *count) = (Instant::now(), 0);
        }
        if *count >= token.rate_per_minute {
            return Err(Denied::Rate(60u64.saturating_sub(since.elapsed().as_secs()).max(1)));
        }
        *count += 1;
        Ok(token.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pcds-tokens-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn scopes(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_token_is_shown_once_and_only_its_hash_is_kept() {
        let d = dir("once");
        let (made, token) = create(&d, "relay", &scopes(&["chat.send", "events.read", "events.read"]), None).unwrap();
        assert!(token.starts_with(PREFIX) && token.len() == PREFIX.len() + 48, "{token}");
        assert_eq!((made.scopes.clone(), made.rate_per_minute, made.id.len()), (scopes(&["chat.send", "events.read"]), DEFAULT_RATE, 8));
        let file = std::fs::read_to_string(d.join(FILE)).unwrap();
        assert!(!file.contains(&token) && file.contains(&made.hash), "{file}");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(d.join(FILE)).unwrap().permissions().mode() & 0o777, 0o600);
        // A listing has everything but the hash.
        let listed = made.public().to_string();
        assert!(!listed.contains(&made.hash) && listed.contains("relay"), "{listed}");
        // Two tokens differ; a name is used once; the form is checked.
        let (_, other) = create(&d, "metrics", &scopes(&["events.read"]), Some(60)).unwrap();
        assert_ne!(token, other);
        assert!(create(&d, "relay", &scopes(&["events.read"]), None).unwrap_err().contains("exists"));
        assert!(create(&d, "bad name", &scopes(&["events.read"]), None).is_err());
        assert!(create(&d, "x", &[], None).unwrap_err().contains("at least one scope"));
        assert!(create(&d, "x", &scopes(&["admin"]), None).unwrap_err().contains("not a scope"));
        assert!(create(&d, "x", &scopes(&["events.read"]), Some(0)).is_err() && create(&d, "x", &scopes(&["events.read"]), Some(MAX_RATE + 1)).is_err());
        for i in 0..MAX_TOKENS {
            let _ = create(&d, &format!("t{i}"), &scopes(&["events.read"]), None);
        }
        assert!(create(&d, "one-more", &scopes(&["events.read"]), None).unwrap_err().contains("already"));
    }

    #[test]
    fn a_token_opens_the_requests_of_its_scopes_and_nothing_else() {
        let d = dir("scopes");
        let (_, token) = create(&d, "relay", &scopes(&["events.read", "chat.send"]), None).unwrap();
        let check = Checker::new(d.clone());
        let bearer = format!("Bearer {token}");
        assert!(check.authorize(&bearer, "POST", "/api/plugins/events").is_ok());
        assert!(check.authorize(&bearer, "POST", "/api/chat").is_ok());
        // A scope it lacks, and every request no scope covers (the admin token's alone).
        assert_eq!(check.authorize(&bearer, "POST", "/api/kick"), Err(Denied::Scope(Some("player.kick"))));
        assert_eq!(check.authorize(&bearer, "GET", "/api/chat/events"), Err(Denied::Scope(Some("chat.read"))));
        for (m, p) in [("GET", "/api/status"), ("GET", "/api/plugins"), ("POST", "/api/plugins/enable"), ("POST", "/api/plugins/tokens"), ("GET", "/api/plugins/tokens"), ("POST", "/api/plugins/trust"), ("POST", "/api/rules"), ("POST", "/api/stop"), ("GET", "/api/logs"), ("GET", "/api/config"), ("POST", "/api/chat/"), ("GET", "/api/plugins/events"), ("POST", "/api/plugins/events/x")] {
            assert_eq!(check.authorize(&bearer, m, p), Err(Denied::Scope(None)), "{m} {p}");
        }
        // Not tokens: other schemes, the admin way, truncations, a token of the right look that was never made.
        for h in ["", "Bearer ", "Bearer x", &format!("bearer {token}"), &format!("Bearer {}", &token[..token.len() - 1]), &format!("Bearer {token}x"), &format!("Bearer {PREFIX}{}", "0".repeat(48)), &format!("Basic {token}"), &format!("Bearer {}", "a".repeat(5000))] {
            assert_eq!(check.authorize(h, "POST", "/api/chat"), Err(Denied::Unknown), "{h:.30}");
        }
        assert_eq!(Checker::none().authorize(&bearer, "POST", "/api/chat"), Err(Denied::Unknown));
    }

    #[test]
    fn a_revoked_token_is_refused_at_once_and_a_broken_file_trusts_nobody() {
        let d = dir("revoke");
        let (relay, token) = create(&d, "relay", &scopes(&["chat.send"]), None).unwrap();
        let (_, other) = create(&d, "other", &scopes(&["chat.send"]), None).unwrap();
        let check = Checker::new(d.clone());
        let (a, b) = (format!("Bearer {token}"), format!("Bearer {other}"));
        assert!(check.authorize(&a, "POST", "/api/chat").is_ok() && check.authorize(&b, "POST", "/api/chat").is_ok());
        // The file changes under the running checker (the revoke is another process's).
        std::thread::sleep(Duration::from_millis(5));
        revoke(&d, &relay.id).unwrap();
        assert_eq!(check.authorize(&a, "POST", "/api/chat"), Err(Denied::Unknown));
        assert!(check.authorize(&b, "POST", "/api/chat").is_ok(), "the other one is not touched");
        assert!(revoke(&d, "relay").is_err() && revoke(&d, "other").is_ok());
        assert_eq!(check.authorize(&b, "POST", "/api/chat"), Err(Denied::Unknown));
        // A file that does not read: nothing is valid (and it is not an empty list that lets anything in).
        let (_, again) = create(&d, "again", &scopes(&["chat.send"]), None).unwrap();
        let c = format!("Bearer {again}");
        assert!(check.authorize(&c, "POST", "/api/chat").is_ok());
        std::thread::sleep(Duration::from_millis(5));
        std::fs::write(d.join(FILE), "{ broken").unwrap();
        assert_eq!(check.authorize(&c, "POST", "/api/chat"), Err(Denied::Unknown));
        std::fs::remove_file(d.join(FILE)).unwrap();
        assert_eq!(check.authorize(&c, "POST", "/api/chat"), Err(Denied::Unknown));
    }

    #[test]
    fn a_token_has_a_rate_of_its_own() {
        let d = dir("rate");
        let (_, slow) = create(&d, "slow", &scopes(&["chat.send"]), Some(3)).unwrap();
        let (_, fast) = create(&d, "fast", &scopes(&["chat.send"]), Some(1000)).unwrap();
        let check = Checker::new(d);
        let (s, f) = (format!("Bearer {slow}"), format!("Bearer {fast}"));
        for _ in 0..3 {
            assert!(check.authorize(&s, "POST", "/api/chat").is_ok());
        }
        assert!(matches!(check.authorize(&s, "POST", "/api/chat"), Err(Denied::Rate(wait)) if (1..=60).contains(&wait)));
        // A request refused for its scope does not use the rate; the other token is not affected.
        assert!(check.authorize(&f, "POST", "/api/chat").is_ok());
        assert_eq!(check.authorize(&f, "POST", "/api/kick"), Err(Denied::Scope(Some("player.kick"))));
        // After a minute it starts again.
        check.cache.lock().unwrap().windows.get_mut(&hash(&slow)[..8]).unwrap().0 = Instant::now() - Duration::from_secs(61);
        assert!(check.authorize(&s, "POST", "/api/chat").is_ok());
    }

    #[test]
    fn the_scope_table_matches_what_a_token_can_reach() {
        for (name, what) in SCOPES {
            assert!(!what.is_empty());
            assert!(["POST /api/plugins/events", "GET /api/chat/events", "POST /api/chat", "POST /api/kick", "POST /api/teleport"].iter().any(|r| {
                let (m, p) = r.split_once(' ').unwrap();
                scope_for(m, p) == Some(name) && what.contains(r)
            }), "{name}");
        }
    }
}
