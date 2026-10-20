//! `net.http`: a plugin's outbound HTTP(S), the one way a sandboxed module reaches the network
//! (docs/plugins.md, "net.http"). Nothing here runs on the game thread, and nothing a plugin writes
//! decides where a connection goes:
//!
//! - **Where**: the manifest's `[net] allow` patterns (`scheme://host[:port]`, one `*.` at the
//!   front of a name that is not a public suffix), pinned by the approval's digest. A pattern with
//!   an IP address needs `net.http.local`. The URL a plugin asks for is checked against them again
//!   for every redirect.
//! - **Which machine**: the host resolves the name itself, once for each connection, and the
//!   addresses it connects to are exactly the ones it checked: loopback, private, link-local,
//!   shared (CGNAT), documentation, multicast, reserved and the non-global IPv6 ranges are refused
//!   unless `net.http.local` is granted. A name with one such address among its answers is refused
//!   whole. A second check in the connector looks at the addresses ureq is about to use. So a name
//!   that points at the local network (or changes its mind between two lookups) reaches nothing.
//! - **How much**: a request is at most `MAX_HTTP_REQUEST` bytes; the answer is cut at the
//!   manifest's `max_response_bytes` (an answer over it is an error, not a short body); one wall
//!   clock covers DNS, connect, TLS, sending and reading, so a server that trickles bytes is let go
//!   at the timeout; at most `HTTP_IN_FLIGHT` requests of a plugin are alive and `HTTP_PER_SECOND`
//!   start in a second; `HTTP_WORKERS` threads serve every plugin, behind a queue of `HTTP_QUEUE`.
//! - **When**: `http_request` returns a ticket at once; the answer is an `HttpDone` event delivered
//!   by the host between two ticks (`plugins::poll`), to that plugin only.
//! - **What is told**: errors reach the plugin as short fixed phrases (never the URL, never an
//!   address); the log gets one line per request with `METHOD scheme://host[:port]` and no path or
//!   query; values the admin set for the plugin (`${config.KEY}` in the path, the query or a header
//!   value) are put in by the host after the plugin's call and are in no log, listing or event.
//!
//! TLS is ureq's rustls with the Mozilla roots and cannot be turned off; there is no proxy.

use std::collections::{BTreeMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use pcds_plugin_api::HttpRequest;
use ureq::http::Uri;
use ureq::unversioned::resolver::{ArrayVec, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{ConnectionDetails, Connector, DefaultConnector, NextTimeout};

use super::limits;

// ---- where a plugin may go -----------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    pub fn name(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }
    pub fn default_port(self) -> u16 {
        match self {
            Scheme::Http => 80,
            Scheme::Https => 443,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HostPattern {
    Exact(String),
    /// `*.example.com`: any name below example.com (stored: `example.com`), not example.com itself.
    Below(String),
    Ip(IpAddr),
}

/// One entry of a manifest's `[net] allow`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    pub text: String,
    scheme: Scheme,
    host: HostPattern,
    port: u16,
}

/// Names under which anyone can publish a site: `*.github.io` would allow every page on GitHub.
/// Deliberately short (the Public Suffix List is thousands of lines and a new dependency): the
/// two-label suffixes that are by far the most used, the hosting platforms plugins would
/// plausibly point at, and the rule that a wildcard needs a name of two labels at least. The
/// approval screen shows every pattern, and a wildcard is flagged there.
const PUBLIC_SUFFIXES: &[&str] = &[
    "co.uk", "org.uk", "ac.uk", "gov.uk", "me.uk", "ltd.uk", "plc.uk", "net.uk", "sch.uk", "com.au", "net.au", "org.au", "edu.au", "gov.au", "co.nz", "org.nz", "net.nz", "co.jp", "ne.jp", "or.jp", "ac.jp", "co.kr", "or.kr", "co.in", "net.in", "org.in", "co.za", "org.za", "com.br", "net.br", "org.br", "com.cn", "net.cn", "org.cn",
    "com.mx", "com.ar", "com.tr", "com.tw", "com.hk", "com.sg", "com.my", "com.ua", "com.pl", "com.ru", "co.il", "co.id", "co.th", "github.io", "githubusercontent.com", "gitlab.io", "pages.dev", "workers.dev", "netlify.app", "vercel.app", "herokuapp.com", "appspot.com", "web.app", "firebaseapp.com", "cloudfront.net", "amazonaws.com", "compute.amazonaws.com",
    "s3.amazonaws.com", "azurewebsites.net", "blob.core.windows.net", "cloudapp.net", "trafficmanager.net", "blogspot.com", "wordpress.com", "repl.co", "glitch.me", "onrender.com", "fly.dev", "railway.app", "ngrok.io", "ngrok-free.app", "duckdns.org", "no-ip.org", "dyndns.org", "myftp.org", "ddns.net", "hopto.org", "zapto.org", "000webhostapp.com", "weebly.com", "wixsite.com", "carrd.co",
];

fn valid_label(l: &str) -> bool {
    !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A host name as patterns and URLs spell it (lowercase letters, digits, hyphens, dots; the last
/// label begins with a letter, which keeps `127.1`, `0x7f.1` and `2130706433` from being names).
fn valid_name(name: &str) -> bool {
    name.len() <= 253 && name.split('.').all(valid_label) && name.rsplit('.').next().is_some_and(|tld| tld.starts_with(|c: char| c.is_ascii_lowercase()))
}

impl Pattern {
    /// `local`: the manifest also asks for `net.http.local` (the only case an IP address may be named).
    pub fn parse(text: &str, local: bool) -> Result<Pattern, String> {
        let bad = |why: &str| format!("net.allow {text:?}: {why}");
        let (scheme, rest) = match text.split_once("://") {
            Some(("https", r)) => (Scheme::Https, r),
            Some(("http", r)) => (Scheme::Http, r),
            _ => return Err(bad("it begins with http:// or https://")),
        };
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        if rest.is_empty() || rest.contains(['/', '?', '#', '@', ' ', '\\']) || !rest.is_ascii() {
            return Err(bad("scheme://host or scheme://host:port, with no path, query or user name"));
        }
        let (host, port) = if let Some(inner) = rest.strip_prefix('[') {
            let (ip, after) = inner.split_once(']').ok_or_else(|| bad("an IPv6 address has a closing ]"))?;
            (ip, after.strip_prefix(':'))
        } else {
            match rest.rsplit_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (rest, None),
            }
        };
        let port = match port {
            None => scheme.default_port(),
            Some(p) => p.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(|| bad("the port is a number from 1 to 65535"))?,
        };
        let host_pattern = if let Ok(ip) = host.parse::<IpAddr>() {
            if !local {
                return Err(bad("an IP address can only be named by a plugin that also asks for net.http.local"));
            }
            HostPattern::Ip(ip)
        } else if let Some(base) = host.strip_prefix("*.") {
            if !valid_name(base) {
                return Err(bad("a wildcard is *. and then a host name"));
            }
            if base.split('.').count() < 2 || PUBLIC_SUFFIXES.contains(&base) {
                return Err(bad("a wildcard cannot cover a public suffix (everyone's sites under it): name the host"));
            }
            HostPattern::Below(base.to_string())
        } else if valid_name(host) {
            HostPattern::Exact(host.to_string())
        } else {
            return Err(bad("the host is a lowercase DNS name (letters, digits, hyphens), or *.name"));
        };
        Ok(Pattern { text: text.trim_end_matches('/').to_string(), scheme, host: host_pattern, port })
    }

    pub fn is_ip(&self) -> bool {
        matches!(self.host, HostPattern::Ip(_))
    }

    fn matches(&self, t: &Target) -> bool {
        self.scheme == t.scheme
            && self.port == t.port
            && match &self.host {
                HostPattern::Exact(h) => *h == t.host,
                HostPattern::Below(base) => t.host.len() > base.len() + 1 && t.host.ends_with(base.as_str()) && t.host[..t.host.len() - base.len()].ends_with('.'),
                HostPattern::Ip(ip) => t.host.parse::<IpAddr>().is_ok_and(|h| h == *ip),
            }
    }
}

/// Scheme, host and port of a URL, as the rules see them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub scheme: Scheme,
    /// Lowercase, without brackets.
    pub host: String,
    pub port: u16,
}

impl Target {
    /// `scheme://host[:port]`: what the log and the listing show of a URL.
    pub fn shown(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        if self.port == self.scheme.default_port() { format!("{}://{host}", self.scheme.name()) } else { format!("{}://{host}:{}", self.scheme.name(), self.port) }
    }
}

/// A URL a plugin may name: absolute http or https, ASCII, no user name, no fragment.
pub fn parse_url(url: &str) -> Result<(Uri, Target), String> {
    if url.len() > limits::MAX_HTTP_URL {
        return Err(format!("the URL is over {} bytes", limits::MAX_HTTP_URL));
    }
    if url.bytes().any(|b| b <= b' ' || b >= 0x7f || b == b'\\' || b == b'#') {
        return Err("the URL has a space, a control character, a backslash, a fragment or a character that is not ASCII".into());
    }
    let uri: Uri = url.parse().map_err(|_| "not a URL".to_string())?;
    let scheme = match uri.scheme_str() {
        Some("https") => Scheme::Https,
        Some("http") => Scheme::Http,
        _ => return Err("the URL begins with http:// or https://".into()),
    };
    let authority = uri.authority().ok_or("the URL has no host")?;
    if authority.as_str().contains('@') {
        return Err("a URL with a user name or password is refused: put a secret in a header".into());
    }
    let host = authority.host().to_ascii_lowercase();
    let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(&host).to_string();
    // An address, or a name as patterns spell names: no percent escape, no `127.1`, `0x7f.1` or `2130706433`
    // for a resolver to read as an address, no trailing dot.
    if host.parse::<IpAddr>().is_err() && !valid_name(&host) {
        return Err("the host is not a DNS name or an IP address".into());
    }
    let port = authority.port_u16().unwrap_or(scheme.default_port());
    Ok((uri, Target { scheme, host, port }))
}

/// Where one plugin may go and how much it may take, built from its manifest and approval.
#[derive(Debug, Clone)]
pub struct Policy {
    pub patterns: Vec<Pattern>,
    /// `net.http.local` is granted: loopback, private and link-local addresses may be reached.
    pub local: bool,
    pub timeout: Duration,
    pub max_response: usize,
}

impl Policy {
    pub fn permits(&self, t: &Target) -> bool {
        self.patterns.iter().any(|p| p.matches(t))
    }

    /// Whether a connection to `ip` is allowed.
    pub fn ip_ok(&self, ip: IpAddr) -> bool {
        match classify(ip) {
            Class::Global => true,
            Class::Local => self.local,
            Class::Never => false,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Class {
    /// A public address.
    Global,
    /// This machine or a private network: loopback, RFC 1918, link-local, shared address space, unique-local.
    Local,
    /// Never a peer: unspecified, multicast, broadcast, reserved, documentation, the translation ranges.
    Never,
}

fn classify(ip: IpAddr) -> Class {
    match ip {
        IpAddr::V4(a) => classify_v4(a),
        IpAddr::V6(a) => classify_v6(a),
    }
}

fn classify_v4(a: Ipv4Addr) -> Class {
    let o = a.octets();
    match o {
        [127, ..] | [10, ..] | [169, 254, ..] | [192, 168, ..] => Class::Local,
        [172, b, ..] if (16..32).contains(&b) => Class::Local,
        [100, b, ..] if (64..128).contains(&b) => Class::Local,
        [0, ..] | [192, 0, 0, _] | [192, 0, 2, _] | [192, 88, 99, _] | [198, 51, 100, _] | [203, 0, 113, _] => Class::Never,
        [198, b, ..] if (18..20).contains(&b) => Class::Never,
        [a, ..] if a >= 224 => Class::Never,
        _ => Class::Global,
    }
}

fn classify_v6(a: Ipv6Addr) -> Class {
    if a.is_loopback() {
        return Class::Local;
    }
    let s = a.segments();
    // fc00::/7 unique-local, fe80::/10 link-local: this network.
    if s[0] & 0xfe00 == 0xfc00 || s[0] & 0xffc0 == 0xfe80 {
        return Class::Local;
    }
    // An IPv4 address in an IPv6 one (mapped, compatible): it is the IPv4 address's class.
    if let Some(v4) = a.to_ipv4_mapped() {
        return classify_v4(v4);
    }
    // Only 2000::/3 is global unicast; and not the protocol, documentation and 6to4 parts of it.
    let global = s[0] & 0xe000 == 0x2000 && !(s[0] == 0x2001 && s[1] < 0x200) && !(s[0] == 0x2001 && s[1] == 0xdb8) && s[0] != 0x2002 && !(s[0] == 0x3fff && s[1] < 0x1000);
    if global { Class::Global } else { Class::Never }
}

// ---- turning a plugin's request into one the host sends ------------------------------------------

/// Why a request was turned down at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reject {
    /// Something wrong with the request (E_BAD).
    Bad(String),
    /// The host is not on the list (E_NOT_ALLOWED).
    NotAllowed(String),
}

/// A request ready to send: the secrets are in it. Never print it (`Debug` does not).
#[derive(Clone, PartialEq, Eq)]
pub struct Fetch {
    pub ticket: u64,
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// `scheme://host[:port]`, for the log and the listing.
    pub shown: String,
}

impl std::fmt::Debug for Fetch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Fetch(#{} {} {}, {} headers, {} body bytes)", self.ticket, self.method, self.shown, self.headers.len(), self.body.len())
    }
}

const METHODS: [&str; 6] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"];
/// Headers the host (or the HTTP library) writes itself.
const FORBIDDEN_HEADERS: [&str; 12] = ["host", "content-length", "transfer-encoding", "connection", "upgrade", "te", "trailer", "expect", "keep-alive", "proxy-authorization", "proxy-connection", "http2-settings"];

fn header_ok(name: &str, value: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(format!("header name {name:?}: letters, digits and hyphens, 64 at most"));
    }
    if FORBIDDEN_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
        return Err(format!("header {name}: the host sets it"));
    }
    if value.len() > limits::MAX_HTTP_HEADER_BYTES || value.bytes().any(|b| (b < b' ' && b != b'\t') || b == 0x7f) {
        return Err(format!("header {name}: a value of {} bytes at most, with no control character", limits::MAX_HTTP_HEADER_BYTES));
    }
    Ok(())
}

/// `${config.KEY}` replaced by the admin's value; an unknown or unset key is an error.
fn expand(text: &str, config: &BTreeMap<String, String>) -> Result<String, Reject> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("${") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        let end = after.find('}').ok_or_else(|| Reject::Bad("a ${ without its }".into()))?;
        let key = after[..end].strip_prefix("config.").ok_or_else(|| Reject::Bad("only ${config.KEY} can be put in a request".into()))?;
        let value = config.get(key).ok_or_else(|| Reject::Bad(format!("config {key} is not declared in the manifest, or the admin has not set it (pcds plugin config set ID {key})")))?;
        out.push_str(value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

impl Policy {
    /// The request as it will be sent, or why not. `config`: the values the admin set for the
    /// keys the manifest declares.
    pub fn build(&self, request: &HttpRequest<'_>, config: &BTreeMap<String, String>) -> Result<Fetch, Reject> {
        let bad = |s: String| Reject::Bad(s);
        let method = request.method.to_ascii_uppercase();
        if !METHODS.contains(&method.as_str()) {
            return Err(bad(format!("method {:?}: one of {}", request.method, METHODS.join(", "))));
        }
        if request.headers.len() > pcds_plugin_api::http::MAX_HEADERS {
            return Err(bad(format!("at most {} headers", pcds_plugin_api::http::MAX_HEADERS)));
        }
        if request.body.len() > limits::MAX_HTTP_BODY {
            return Err(bad(format!("a body of {} bytes at most", limits::MAX_HTTP_BODY)));
        }
        if !request.body.is_empty() && (method == "GET" || method == "HEAD") {
            return Err(bad(format!("a {method} has no body")));
        }
        // The host part of a URL is the plugin's own text: a config value can sit in the path, the
        // query and the header values, never decide where the request goes (nor be shown by the log).
        let template_authority = request.url.split_once("://").map(|(_, r)| r.split(['/', '?']).next().unwrap_or(""));
        if template_authority.is_some_and(|a| a.contains("${")) {
            return Err(bad("a ${config.KEY} can be in the path, the query and header values, not in the host".into()));
        }
        let url = expand(request.url, config)?;
        let (_, target) = parse_url(&url).map_err(bad)?;
        if !self.permits(&target) {
            return Err(Reject::NotAllowed(format!("{} is not on this plugin's allow list", target.shown())));
        }
        let mut headers = Vec::with_capacity(request.headers.len());
        let mut total = 0;
        for (name, value) in &request.headers {
            let value = expand(value, config)?;
            header_ok(name, &value).map_err(bad)?;
            total += name.len() + value.len();
            headers.push((name.to_string(), value));
        }
        if total > limits::MAX_HTTP_HEADER_BYTES * 2 {
            return Err(bad(format!("the headers are over {} bytes", limits::MAX_HTTP_HEADER_BYTES * 2)));
        }
        Ok(Fetch { ticket: 0, method, url, headers, body: request.body.to_vec(), shown: target.shown() })
    }
}

/// Tickets are numbered across the whole host, so one that outlives its plugin's instance (a
/// restart) is never taken for a new one's.
pub fn next_ticket() -> u64 {
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

// ---- DNS and the connection ----------------------------------------------------------------------

/// How names become addresses; the host's is the system's resolver, a test's is its own.
pub trait Dns: Send + Sync {
    fn lookup(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>>;
}

pub struct System;

impl Dns for System {
    fn lookup(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
        Ok((host, port).to_socket_addrs()?.collect())
    }
}

/// An error the guard raised, carried through ureq as `Error::Other`.
#[derive(Debug)]
struct Blocked(&'static str);

impl std::fmt::Display for Blocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Blocked {}

fn blocked(why: &'static str) -> ureq::Error {
    ureq::Error::Other(Box::new(Blocked(why)))
}

const DISALLOWED_ADDRESS: &str = "the name resolves to an address the plugin may not reach (local, private or reserved)";

/// Resolves a name the way the plugin's policy allows and no other way.
struct Guard {
    policy: Arc<Policy>,
    dns: Arc<dyn Dns>,
    deadline: Instant,
}

impl std::fmt::Debug for Guard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Guard")
    }
}

impl Resolver for Guard {
    fn resolve(&self, uri: &Uri, _: &ureq::config::Config, _: NextTimeout) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let (_, target) = parse_url(&uri.to_string()).map_err(|_| blocked("not a URL"))?;
        let addrs: Vec<SocketAddr> = match target.host.parse::<IpAddr>() {
            Ok(ip) => vec![SocketAddr::new(ip, target.port)],
            Err(_) => {
                // The system's lookup cannot be interrupted: it runs on a thread of its own that the
                // request stops waiting for at its deadline.
                let left = self.deadline.saturating_duration_since(Instant::now());
                let (tx, rx) = mpsc::sync_channel(1);
                let (dns, host, port) = (self.dns.clone(), target.host.clone(), target.port);
                std::thread::Builder::new().name("pcds-plugin-dns".into()).spawn(move || drop(tx.send(dns.lookup(&host, port)))).map_err(ureq::Error::Io)?;
                match rx.recv_timeout(left) {
                    Ok(Ok(a)) => a,
                    Ok(Err(_)) => return Err(ureq::Error::HostNotFound),
                    Err(_) => return Err(ureq::Error::Timeout(ureq::Timeout::Resolve)),
                }
            }
        };
        if addrs.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        // One address that may not be reached and the name is refused whole: which of them the
        // connection would try first is not something to bet on.
        if addrs.iter().any(|a| !self.policy.ip_ok(a.ip())) {
            return Err(blocked(DISALLOWED_ADDRESS));
        }
        let mut out: ResolvedSocketAddrs = ArrayVec::from_fn(|_| SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0));
        for a in addrs.into_iter().take(8) {
            out.push(a);
        }
        Ok(out)
    }
}

/// Looks at the addresses ureq is about to connect to, once more, in front of the TCP connector:
/// whatever happened between the resolver and here, nothing but allowed addresses is dialled.
#[derive(Debug)]
struct Recheck(Arc<Policy>);

impl Connector<()> for Recheck {
    type Out = ();

    fn connect(&self, details: &ConnectionDetails, chained: Option<()>) -> Result<Option<()>, ureq::Error> {
        if details.addrs.iter().any(|a| !self.0.ip_ok(a.ip())) {
            return Err(blocked(DISALLOWED_ADDRESS));
        }
        Ok(chained)
    }
}

fn agent(policy: &Arc<Policy>, dns: &Arc<dyn Dns>, deadline: Instant, plugin: &str) -> ureq::Agent {
    agent_with(policy, Guard { policy: policy.clone(), dns: dns.clone(), deadline }, deadline, plugin)
}

/// The agent for one request: no proxy, no redirects of its own, no connection kept, and the
/// connector's re-check in front of the connection; `resolver` is the guard (a test puts its own).
pub(super) fn agent_with(policy: &Arc<Policy>, resolver: impl Resolver, deadline: Instant, plugin: &str) -> ureq::Agent {
    let left = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(left))
        .http_status_as_error(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .proxy(None)
        .https_only(false)
        .max_idle_connections(0)
        .user_agent(format!("pcds/{} (plugin {plugin})", env!("CARGO_PKG_VERSION")))
        .build();
    ureq::Agent::with_parts(config, Recheck(policy.clone()).chain(DefaultConnector::default()), resolver)
}

// ---- running a request ---------------------------------------------------------------------------

/// What came of a request, to go to the plugin as `HttpDone`.
#[derive(Debug, Clone, Default)]
pub struct Done {
    pub plugin: String,
    pub ticket: u64,
    pub method: String,
    pub shown: String,
    /// 0: no HTTP answer.
    pub status: u32,
    /// Empty when there was an answer.
    pub error: String,
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
    pub millis: u64,
}

/// The one line of the log for a request: the plugin, the method, `scheme://host[:port]`, what came
/// of it. Never the path, the query, a header or the body.
pub fn log_line(done: &Done) -> String {
    let result = if done.error.is_empty() { format!("HTTP {}", done.status) } else { done.error.clone() };
    format!("plugin {}: http {} {} -> {result} ({} ms, {} bytes)", done.plugin, done.method, done.shown, done.millis, done.body.len())
}

fn describe(e: &ureq::Error) -> String {
    use std::io::ErrorKind as K;
    match e {
        ureq::Error::Timeout(_) => "timed out".into(),
        ureq::Error::HostNotFound => "host not found".into(),
        ureq::Error::BodyExceedsLimit(_) => "the answer is larger than the limit".into(),
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) | ureq::Error::Pem(_) => "TLS failed (the certificate or the handshake)".into(),
        ureq::Error::Io(io) => match io.kind() {
            K::ConnectionRefused => "connection refused".into(),
            K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe => "connection reset".into(),
            K::TimedOut | K::WouldBlock => "timed out".into(),
            K::UnexpectedEof => "the connection closed too soon".into(),
            _ => "connection failed".into(),
        },
        ureq::Error::Protocol(_) => "the server did not speak HTTP".into(),
        ureq::Error::Other(o) => o.downcast_ref::<Blocked>().map_or("request failed", |b| b.0).into(),
        ureq::Error::BadUri(_) | ureq::Error::Http(_) => "bad request".into(),
        ureq::Error::ConnectionFailed => "connection failed".into(),
        _ => "request failed".into(),
    }
}

/// The URL a `Location` header names, from the URL it answered.
fn join(base: &str, location: &str) -> Option<String> {
    if location.starts_with("http://") || location.starts_with("https://") {
        return Some(location.to_string());
    }
    let (scheme, rest) = base.split_once("://")?;
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(authority_end);
    if let Some(r) = location.strip_prefix("//") {
        return Some(format!("{scheme}://{r}"));
    }
    if location.starts_with('/') {
        return Some(format!("{scheme}://{authority}{location}"));
    }
    let dir = path.split(['?', '#']).next().unwrap_or("");
    let dir = &dir[..dir.rfind('/').map_or(0, |i| i + 1)];
    Some(format!("{scheme}://{authority}{}{location}", if dir.is_empty() { "/" } else { dir }))
}

fn same_origin(a: &Target, b: &Target) -> bool {
    a == b
}

/// Sends the request, follows up to `HTTP_REDIRECTS` redirects that stay on the allow list, reads
/// the answer up to the policy's size. Blocking: this is a worker's job.
pub fn run(job: &Job, dns: &Arc<dyn Dns>) -> Done {
    let started = Instant::now();
    let f = &job.fetch;
    let mut done = Done { plugin: job.plugin.clone(), ticket: f.ticket, method: f.method.clone(), shown: f.shown.clone(), ..Default::default() };
    let outcome = send(job, dns, started + job.policy.timeout, &mut done);
    done.millis = started.elapsed().as_millis() as u64;
    if let Err(e) = outcome {
        (done.status, done.body, done.headers, done.error) = (0, Vec::new(), Vec::new(), e);
    }
    done
}

fn send(job: &Job, dns: &Arc<dyn Dns>, deadline: Instant, done: &mut Done) -> Result<(), String> {
    let policy = &job.policy;
    let f = &job.fetch;
    let (mut method, mut url, mut headers, mut body) = (f.method.clone(), f.url.clone(), f.headers.clone(), f.body.clone());
    let (_, mut origin) = parse_url(&url)?;
    for hop in 0..=limits::HTTP_REDIRECTS {
        let (uri, target) = parse_url(&url)?;
        if !policy.permits(&target) {
            return Err(if hop == 0 { "the host is not on the allow list".into() } else { "redirected to a host that is not on the allow list".into() });
        }
        // The credentials of one place are not sent to another.
        if !same_origin(&origin, &target) {
            headers.retain(|(k, _)| !["authorization", "cookie"].contains(&k.to_ascii_lowercase().as_str()));
            origin = target.clone();
        }
        let mut request = ureq::http::Request::builder().method(method.as_str()).uri(uri);
        for (k, v) in &headers {
            request = request.header(k.as_str(), v.as_str());
        }
        let request = request.body(body.clone()).map_err(|_| "bad request".to_string())?;
        let agent = agent(policy, dns, deadline, &job.plugin);
        let mut response = agent.run(request).map_err(|e| describe(&e))?;
        let status = response.status().as_u16();
        if matches!(status, 301 | 302 | 303 | 307 | 308)
            && let Some(location) = response.headers().get("location").and_then(|v| v.to_str().ok())
        {
            if hop == limits::HTTP_REDIRECTS {
                return Err("too many redirects".into());
            }
            url = join(&url, location).ok_or("a redirect to a URL that is not one")?;
            if (301..=303).contains(&status) && method != "HEAD" {
                method = "GET".into();
                body.clear();
                headers.retain(|(k, _)| !k.to_ascii_lowercase().starts_with("content-"));
            }
            continue;
        }
        let mut kept = 0;
        for (name, value) in response.headers().iter() {
            let (n, v) = (name.as_str().to_string(), String::from_utf8_lossy(value.as_bytes()).into_owned());
            kept += n.len() + v.len();
            if done.headers.len() < pcds_plugin_api::http::MAX_HEADERS && kept <= limits::MAX_HTTP_HEADER_BYTES {
                done.headers.push((n, v));
            }
        }
        done.status = status as u32;
        if method != "HEAD" {
            // One byte more than the limit is read, to tell an answer that fills it from one that passes it.
            done.body = response.body_mut().with_config().limit(policy.max_response as u64 + 1).read_to_vec().map_err(|e| describe(&e))?;
            if done.body.len() > policy.max_response {
                return Err("the answer is larger than the limit".into());
            }
        }
        return Ok(());
    }
    Err("too many redirects".into())
}

// ---- the pool ------------------------------------------------------------------------------------

/// Counts one request of a plugin in flight until it is dropped (finished, refused or never run).
pub struct InFlight(Arc<AtomicU32>);

impl InFlight {
    fn new(counter: Arc<AtomicU32>) -> InFlight {
        counter.fetch_add(1, Ordering::SeqCst);
        InFlight(counter)
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        // Only InFlight::new counts up, once per value, so this never goes below zero.
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct Job {
    pub plugin: String,
    pub fetch: Fetch,
    pub policy: Arc<Policy>,
    _flight: InFlight,
}

/// The workers, the queue before them and the answers after them. Threads start with the first
/// request and end when the `Net` is dropped.
pub struct Net {
    dns: Arc<dyn Dns>,
    queue: Option<mpsc::SyncSender<Job>>,
    done: Arc<Mutex<VecDeque<Done>>>,
    /// Requests submitted whose answer has not been taken.
    pub outstanding: usize,
}

impl Default for Net {
    fn default() -> Net {
        Net::with_dns(Arc::new(System))
    }
}

impl Net {
    pub fn with_dns(dns: Arc<dyn Dns>) -> Net {
        Net { dns, queue: None, done: Arc::default(), outstanding: 0 }
    }

    /// Hands a request to the workers; its answer comes out of `take`. `in_flight`: the plugin's counter.
    pub fn submit(&mut self, plugin: &str, fetch: Fetch, policy: Arc<Policy>, in_flight: Arc<AtomicU32>) {
        self.outstanding += 1;
        let job = Job { plugin: plugin.to_string(), fetch, policy, _flight: InFlight::new(in_flight) };
        if self.queue.is_none() {
            self.queue = self.start_workers();
        }
        let refused = match &self.queue {
            Some(q) => q.try_send(job).err().map(|e| match e {
                mpsc::TrySendError::Full(j) | mpsc::TrySendError::Disconnected(j) => j,
            }),
            None => Some(job),
        };
        if let Some(job) = refused {
            let f = &job.fetch;
            let done = Done { plugin: job.plugin.clone(), ticket: f.ticket, method: f.method.clone(), shown: f.shown.clone(), error: "the server is busy with other requests".into(), ..Default::default() };
            self.done.lock().unwrap_or_else(|e| e.into_inner()).push_back(done);
        }
    }

    fn start_workers(&self) -> Option<mpsc::SyncSender<Job>> {
        let (tx, rx) = mpsc::sync_channel::<Job>(limits::HTTP_QUEUE);
        let rx = Arc::new(Mutex::new(rx));
        for n in 0..limits::HTTP_WORKERS {
            let (rx, done, dns) = (rx.clone(), self.done.clone(), self.dns.clone());
            let spawned = std::thread::Builder::new().name(format!("pcds-plugin-http-{n}")).spawn(move || {
                loop {
                    let job = match rx.lock().unwrap_or_else(|e| e.into_inner()).recv() {
                        Ok(job) => job,
                        Err(_) => return,
                    };
                    let answer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&job, &dns))).unwrap_or_else(|_| Done { plugin: job.plugin.clone(), ticket: job.fetch.ticket, method: job.fetch.method.clone(), shown: job.fetch.shown.clone(), error: "the request failed inside the server".into(), ..Default::default() });
                    drop(job);
                    done.lock().unwrap_or_else(|e| e.into_inner()).push_back(answer);
                }
            });
            if let Err(e) = spawned {
                eprintln!("plugins: no thread for an HTTP worker ({e})");
                if n == 0 {
                    return None;
                }
            }
        }
        Some(tx)
    }

    /// Up to `max` finished requests.
    pub fn take(&mut self, max: usize) -> Vec<Done> {
        let mut queue = self.done.lock().unwrap_or_else(|e| e.into_inner());
        let n = queue.len().min(max);
        let taken: Vec<Done> = queue.drain(..n).collect();
        self.outstanding = self.outstanding.saturating_sub(taken.len());
        taken
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(allow: &[&str], local: bool) -> Policy {
        Policy { patterns: allow.iter().map(|p| Pattern::parse(p, local).unwrap()).collect(), local, timeout: Duration::from_secs(5), max_response: 1 << 16 }
    }

    fn target(url: &str) -> Target {
        parse_url(url).unwrap().1
    }

    #[test]
    fn patterns_name_a_scheme_a_host_and_a_port_and_nothing_wide() {
        for ok in ["https://discord.com", "https://discord.com/", "http://hooks.example.org:8080", "https://*.example.com", "https://a-b.example.co.uk:8443"] {
            assert!(Pattern::parse(ok, false).is_ok(), "{ok}");
        }
        for (bad, why) in [
            ("discord.com", "begins with http"),
            ("ftp://discord.com", "begins with http"),
            ("https://discord.com/api", "no path"),
            ("https://user@discord.com", "no path"),
            ("https://discord.com:0", "port"),
            ("https://discord.com:99999", "port"),
            ("https://Discord.com", "lowercase"),
            ("https://*", "lowercase DNS name"),
            ("https://*.com", "public suffix"),
            ("https://*.co.uk", "public suffix"),
            ("https://*.github.io", "public suffix"),
            ("https://*.s3.amazonaws.com", "public suffix"),
            ("https://a.*.example.com", "lowercase DNS name"),
            ("https://**.example.com", "lowercase DNS name"),
            ("https://127.0.0.1", "net.http.local"),
            ("https://[::1]:8080", "net.http.local"),
            ("https://127.1", "lowercase DNS name"),
            ("https://0x7f.0.0.1", "lowercase DNS name"),
            ("https://2130706433", "lowercase DNS name"),
            ("https://exa mple.com", "no path"),
            ("https://bücher.example", "no path"),
        ] {
            let e = Pattern::parse(bad, false).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
        // IP addresses only for a plugin that also asks for local access.
        assert!(Pattern::parse("http://127.0.0.1:8080", true).is_ok() && Pattern::parse("http://[::1]:9", true).is_ok());
    }

    #[test]
    fn a_url_must_match_scheme_host_and_port_and_a_wildcard_covers_names_below_only() {
        let p = policy(&["https://discord.com", "http://relay.example.org:8080", "https://*.example.com"], false);
        for yes in ["https://discord.com/api/webhooks/1/x", "https://DISCORD.com:443/", "http://relay.example.org:8080/p?q=1", "https://a.example.com/", "https://a.b.example.com/x"] {
            assert!(p.permits(&target(yes)), "{yes}");
        }
        for no in ["http://discord.com/", "https://discord.com:8443/", "https://evildiscord.com/", "https://discord.com.evil.net/", "https://example.com/", "https://notexample.com/", "https://xexample.com/", "http://relay.example.org/", "https://relay.example.org:8080/", "https://127.0.0.1/"] {
            assert!(!p.permits(&target(no)), "{no}");
        }
        // The things a URL parser can be fooled by.
        for bad in ["https://127.1/", "https://0x7f.0.0.1/", "https://2130706433/", "https://0177.0.0.1/", "https://discord.com./", "https://disc%6frd.com/", "https://discord%2ecom/", "https://[fe80::1%25eth0]/", "https://discord.com@evil.net/", "https://discord.com:443@evil.net/", "https://discord.com#@evil.net/", "https://discord.com\\@evil.net/", "https:/discord.com", "//discord.com", "file:///etc/passwd", "https://disc\tord.com/", "https://discord.com/a b", "https://discord.com/\u{e9}"] {
            assert!(parse_url(bad).is_err(), "{bad}");
        }
        assert_eq!(target("https://[2001:db8::1]:8443/x").shown(), "https://[2001:db8::1]:8443");
        assert_eq!(target("http://Example.com:80/x?y=1").shown(), "http://example.com");
    }

    #[test]
    fn addresses_a_plugin_may_not_reach_are_every_local_private_and_reserved_range() {
        let public = policy(&[], false);
        let local = policy(&[], true);
        for ip in ["8.8.8.8", "1.1.1.1", "93.184.216.34", "172.15.0.1", "172.32.0.1", "100.63.0.1", "100.128.0.1", "2606:4700:4700::1111", "2a00:1450:4001::1"] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(public.ip_ok(ip) && local.ip_ok(ip), "{ip}");
        }
        for ip in ["127.0.0.1", "127.255.255.254", "10.0.0.1", "172.16.0.1", "172.31.255.255", "192.168.1.1", "169.254.169.254", "100.64.0.1", "100.127.255.255", "::1", "fc00::1", "fd12:3456::1", "fe80::1", "::ffff:127.0.0.1", "::ffff:10.1.2.3", "::ffff:169.254.169.254"] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(!public.ip_ok(ip), "{ip} is local and must be refused");
            assert!(local.ip_ok(ip), "{ip} is local and net.http.local allows it");
        }
        for ip in ["0.0.0.0", "0.1.2.3", "224.0.0.1", "239.255.255.255", "255.255.255.255", "240.0.0.1", "192.0.2.1", "198.51.100.7", "203.0.113.9", "198.18.0.1", "192.88.99.1", "::", "ff02::1", "2001:db8::1", "2001::1", "2002:7f00:1::1", "64:ff9b::7f00:1", "::ffff:224.0.0.1", "3fff::1", "1::1"] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(!public.ip_ok(ip) && !local.ip_ok(ip), "{ip} is never a peer");
        }
    }

    #[test]
    fn requests_are_checked_before_they_are_queued() {
        let p = policy(&["https://discord.com", "https://api.example.com"], false);
        let config: BTreeMap<String, String> = [("hook".to_string(), "1234/abcd".to_string()), ("token".to_string(), "s3cret".to_string())].into();
        let req = |method: &'static str, url: &'static str, headers: Vec<(&'static str, &'static str)>, body: &'static [u8]| HttpRequest { method, url, headers, body };
        let ok = p.build(&req("post", "https://discord.com/api/webhooks/${config.hook}?wait=true", vec![("Authorization", "Bot ${config.token}"), ("Content-Type", "application/json")], b"{}"), &config).unwrap();
        assert_eq!((ok.method.as_str(), ok.url.as_str(), ok.shown.as_str()), ("POST", "https://discord.com/api/webhooks/1234/abcd?wait=true", "https://discord.com"));
        assert_eq!(ok.headers[0], ("Authorization".to_string(), "Bot s3cret".to_string()));
        // Debug shows neither the URL nor a header nor the body.
        let shown = format!("{ok:?}");
        assert!(!shown.contains("1234") && !shown.contains("s3cret") && shown.contains("https://discord.com"), "{shown}");
        let not_allowed = |url| matches!(p.build(&req("GET", url, vec![], b""), &config), Err(Reject::NotAllowed(_)));
        assert!(not_allowed("https://evil.example/") && not_allowed("http://discord.com/") && not_allowed("https://127.0.0.1/") && not_allowed("https://discord.com:444/"));
        let bad = |r: HttpRequest<'_>| matches!(p.build(&r, &config), Err(Reject::Bad(_)));
        assert!(bad(req("CONNECT", "https://discord.com/", vec![], b"")));
        assert!(bad(req("GET", "https://discord.com/", vec![], b"x")), "a GET has no body");
        assert!(bad(req("GET", "https://discord.com/${config.nothing}", vec![], b"")), "a key nobody set");
        assert!(bad(req("GET", "https://discord.com/${env.HOME}", vec![], b"")), "only config values");
        assert!(bad(req("GET", "https://discord.com/${config.hook", vec![], b"")));
        assert!(bad(req("GET", "https://${config.hook}/", vec![], b"")), "a value cannot make the host");
        for h in ["Host", "content-length", "Transfer-Encoding", "Connection", "Upgrade", "Expect", "Proxy-Authorization"] {
            assert!(bad(req("GET", "https://discord.com/", vec![(h, "x")], b"")), "{h}");
        }
        assert!(bad(req("GET", "https://discord.com/", vec![("X-A\r\nB", "x")], b"")) && bad(req("GET", "https://discord.com/", vec![("X-A", "x\r\nHost: evil")], b"")));
        assert!(bad(req("GET", "https://discord.com/", vec![("X-A", "x"); 33], b"")), "at most 32 headers");
        let long = "x".repeat(limits::MAX_HTTP_URL);
        assert!(p.build(&HttpRequest { method: "GET", url: &format!("https://discord.com/{long}"), headers: vec![], body: b"" }, &config).is_err());
        let big = vec![0u8; limits::MAX_HTTP_BODY + 1];
        assert!(p.build(&HttpRequest { method: "POST", url: "https://discord.com/", headers: vec![], body: &big }, &config).is_err());
    }

    #[test]
    fn a_relative_redirect_is_joined_to_the_url_it_answered() {
        assert_eq!(join("https://a.example/x/y?z=1", "https://b.example/q").as_deref(), Some("https://b.example/q"));
        assert_eq!(join("https://a.example/x/y?z=1", "/q").as_deref(), Some("https://a.example/q"));
        assert_eq!(join("https://a.example/x/y?z=1", "q").as_deref(), Some("https://a.example/x/q"));
        assert_eq!(join("https://a.example", "q").as_deref(), Some("https://a.example/q"));
        assert_eq!(join("https://a.example:8443/x", "//b.example/q").as_deref(), Some("https://b.example/q"));
    }
}
