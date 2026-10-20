//! The server's log: the one place every line of the front and the bank goes through (docs/monitoring.md).
//!
//! A line has a time (UTC, to the millisecond), a level (`error`, `warn`, `info`, `debug`), a
//! component (the part of the server it comes from: `front`, `bank`, `game`, `utp`, `plugins`...,
//! taken from the module that wrote it), a message, and fields (the process and its pid, and
//! whatever the call adds). Two formats, the rule `log.format`:
//!
//! - `text` (the default): `2026-10-20T17:09:30.123Z INFO  front: admin: http://127.0.0.1:7782/` (the
//!   fields are left out: the message says it in words)
//! - `json`, one object per line: `{"timestamp":"2026-10-20T17:09:30.123Z","level":"info","component":"front","message":"...","fields":{"process":"front","pid":1234}}`
//!
//! and the rule `log.level` says which lines are written (everything at that level and above:
//! `error` only the failures, `debug` all). Both apply to the front and to the bank, and change
//! while the server runs. Warnings and errors go to stderr, the rest to stdout, as before.
//!
//! Under systemd every line starts with the `<N>` priority tag that journald reads and strips, so
//! `journalctl -p warning` works; Docker and a terminal never see it.
//!
//! Nothing secret is written: the admin token, the Steam login token, the chat secret, webhook URLs
//! and the S3 keys are registered as they are read (`secret`, `rule_secrets`) and replaced by `***`
//! wherever they appear in a line, and so is anything that looks like a bearer token, a scoped
//! token (`pcdst_...`) or a signature. This is a net under the code that already never prints them.

use std::fmt::Arguments;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use serde_json::{json, Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

/// The values of the rule `log.level`, most important first.
pub const LEVELS: [&str; 4] = ["error", "warn", "info", "debug"];
/// The values of the rule `log.format`.
pub const FORMATS: [&str; 2] = ["text", "json"];

impl Level {
    pub fn name(self) -> &'static str {
        LEVELS[self as usize]
    }

    pub fn parse(name: &str) -> Option<Level> {
        [Level::Error, Level::Warn, Level::Info, Level::Debug].into_iter().find(|l| l.name() == name)
    }

    /// The syslog priority journald files the line under.
    fn priority(self) -> u8 {
        [3, 4, 6, 7][self as usize]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
}

impl Format {
    pub fn parse(name: &str) -> Option<Format> {
        match name {
            "text" => Some(Format::Text),
            "json" => Some(Format::Json),
            _ => None,
        }
    }
}

static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);
static JSON: AtomicU8 = AtomicU8::new(0);
static PROCESS: OnceLock<&'static str> = OnceLock::new();
/// Whether this process is part of a running server (the front, a bank, `pcds run`), not a command.
static SERVER: AtomicBool = AtomicBool::new(false);
/// Secrets that never change in this process (tokens from the environment) and those the rules
/// hold (replaced whenever the rules change).
static FIXED: Mutex<Vec<String>> = Mutex::new(Vec::new());
static FROM_RULES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Which process this is, for the fields of every line: "front", "bank", "pcds" (the supervisor).
pub fn process(name: &'static str) {
    let _ = PROCESS.set(name);
}

/// A process of a running server begins its log: which process it is ("front", "bank", "run" for
/// `pcds run` and the container's start), the format and level the rules file names, and the
/// tokens the environment holds, which are never written.
pub fn start(name: &'static str, rules_file: &str) {
    process(name);
    SERVER.store(true, Ordering::Relaxed);
    init_from_file(rules_file);
    for key in ["PCDS_ADMIN_TOKEN", "PCDS_STEAM_TOKEN"] {
        if let Some(token) = crate::env(key) {
            secret(&token);
        }
    }
}

/// Whether this process is part of a running server: what a command would print is then a log line.
pub fn is_server() -> bool {
    SERVER.load(Ordering::Relaxed)
}

/// The format and level from now on.
pub fn set(format: Format, level: Level) {
    JSON.store(u8::from(format == Format::Json), Ordering::Relaxed);
    LEVEL.store(level as u8, Ordering::Relaxed);
}

/// The format and level the rules file at `path` names, for a process that starts before its rules
/// are loaded (the front, the bank): anything that does not read is left at the defaults, because
/// the bank refuses such a file with its own message a moment later.
pub fn init_from_file(path: &str) {
    let (format, level) = settings_in(path);
    set(format, level);
}

fn settings_in(path: &str) -> (Format, Level) {
    let rules: Value = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    (rules["log"]["format"].as_str().and_then(Format::parse).unwrap_or(Format::Text), rules["log"]["level"].as_str().and_then(Level::parse).unwrap_or(Level::Info))
}

/// A value that must never be written. Short ones are not kept: replacing "1234" everywhere would
/// hide more than it protects.
pub fn secret(value: &str) {
    let mut fixed = FIXED.lock().unwrap_or_else(|e| e.into_inner());
    if value.len() >= MIN_SECRET && !fixed.iter().any(|v| v == value) {
        fixed.push(value.to_string());
    }
}

/// The secrets the rules hold now (the chat secret, the webhook URLs, the S3 keys): they replace
/// the earlier ones.
pub fn rule_secrets(values: impl IntoIterator<Item = String>) {
    *FROM_RULES.lock().unwrap_or_else(|e| e.into_inner()) = values.into_iter().filter(|v| v.len() >= MIN_SECRET).collect();
}

const MIN_SECRET: usize = 6;

/// `text` with every secret replaced by `***`.
pub fn redact(text: &str) -> String {
    let fixed = FIXED.lock().unwrap_or_else(|e| e.into_inner());
    let from_rules = FROM_RULES.lock().unwrap_or_else(|e| e.into_inner());
    redact_with(text, fixed.iter().chain(from_rules.iter()).map(String::as_str))
}

/// redact() with the secrets given.
fn redact_with<'a>(text: &str, secrets: impl Iterator<Item = &'a str>) -> String {
    let mut out = text.to_string();
    for secret in secrets {
        if out.contains(secret) {
            out = out.replace(secret, "***");
        }
    }
    // What looks like one even when it was never registered: a scoped token, a bearer token.
    let out = mask_after(&out, "pcdst_", |c| !c.is_ascii_alphanumeric());
    mask_after(&out, "Bearer ", |c| c.is_whitespace() || c == '"' || c == '\'')
}

/// `text` with what follows each `prefix`, up to the first character `stop` takes, replaced.
fn mask_after(text: &str, prefix: &str, stop: impl Fn(char) -> bool) -> String {
    let (mut out, mut rest) = (String::new(), text);
    while let Some(at) = rest.find(prefix) {
        let start = at + prefix.len();
        let len = rest[start..].find(&stop).unwrap_or(rest.len() - start);
        out += &rest[..start];
        if len > 0 {
            out += "***";
        }
        rest = &rest[start + len..];
    }
    out + rest
}

/// The component of the module that wrote a line: "pcds::front::flip" is "front". The command
/// line's own (`pcds run`, which supervises the front) is "run".
fn component(module: &str) -> &str {
    match module.split("::").nth(1).unwrap_or(module) {
        "cli" => "run",
        other => other,
    }
}

/// UTC "2026-10-20T17:09:30.123Z".
pub fn timestamp(at: SystemTime) -> String {
    let since = at.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let (y, mo, d, h, mi, s) = crate::util::utc(since.as_secs());
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{:03}Z", since.subsec_millis())
}

/// One line as it is written (without the line break of the last line). `fields` is the call's own.
fn render(json: bool, tagged: bool, level: Level, component: &str, message: &str, fields: &[(&str, Value)], at: SystemTime, pid: u32, process: &str) -> String {
    let when = timestamp(at);
    if json {
        let mut f = Map::new();
        f.insert("process".into(), json!(process));
        f.insert("pid".into(), json!(pid));
        fields.iter().for_each(|(k, v)| {
            f.insert((*k).into(), v.clone());
        });
        let line = json!({"timestamp": when, "level": level.name(), "component": component, "message": message, "fields": f}).to_string();
        return if tagged { format!("<{}>{line}", level.priority()) } else { line };
    }
    // The text says it all in words; the fields are for the machine that reads JSON.
    let message = visible(message);
    let mut lines = message.lines();
    let head = format!("{when} {:<5} {component}: {}", level.name().to_uppercase(), lines.next().unwrap_or(""));
    // A message of several lines keeps every line a line of its own, each marked as part of it.
    let mut out = head;
    for line in lines {
        out += &format!("\n{when} {:<5} {component}:   {line}", level.name().to_uppercase());
    }
    if tagged {
        out.lines().map(|l| format!("<{}>{l}", level.priority())).collect::<Vec<_>>().join("\n")
    } else {
        out
    }
}

/// A message for a log read in a terminal: a carriage return, an escape sequence or another
/// control character in it (a name, a reply from a bucket or a release feed, a plugin's text) would
/// rewrite the line before it or the screen, so each is written as `\u{..}`. Line breaks stay
/// (render marks every line as a line of its own) and so do tabs. JSON escapes them itself.
fn visible(message: &str) -> String {
    message.replace("\r\n", "\n").chars().fold(String::new(), |mut out, c| {
        if c.is_control() && !matches!(c, '\n' | '\t') {
            out += &format!("\\u{{{:x}}}", c as u32);
        } else {
            out.push(c);
        }
        out
    })
}

/// The line for a message, or None when the level leaves it out. `forced`: written whatever the level.
fn line(level: Level, module: &str, fields: &[(&str, Value)], args: Arguments, forced: bool) -> Option<String> {
    let json = JSON.load(Ordering::Relaxed) == 1;
    let at_most = [Level::Error, Level::Warn, Level::Info, Level::Debug][LEVEL.load(Ordering::Relaxed) as usize];
    let message = redact(&args.to_string());
    let fields: Vec<(&str, Value)> = fields.iter().map(|(k, v)| (*k, if let Value::String(s) = v { Value::String(redact(s)) } else { v.clone() })).collect();
    build(json, at_most, level, module, &fields, &message, forced)
}

/// The line, with the settings given: None when `level` is below `at_most` (unless `forced`).
fn build(json: bool, at_most: Level, level: Level, module: &str, fields: &[(&str, Value)], message: &str, forced: bool) -> Option<String> {
    if !forced && level > at_most {
        return None;
    }
    let tagged = std::env::var_os("JOURNAL_STREAM").is_some();
    Some(render(json, tagged, level, component(module), message, fields, SystemTime::now(), std::process::id(), PROCESS.get().copied().unwrap_or("pcds")) + "\n")
}

fn write_line(level: Level, module: &str, fields: &[(&str, Value)], args: Arguments, forced: bool) {
    let Some(line) = line(level, module, fields, args, forced) else { return };
    // A log that cannot be written cannot say so.
    let _ = if level <= Level::Warn { std::io::stderr().lock().write_all(line.as_bytes()) } else { std::io::stdout().lock().write_all(line.as_bytes()) };
}

/// `log!(Level; "text {}", x)`, and with fields of its own `log!(Level; "client" = id; "text")`.
pub fn write(level: Level, module: &str, args: Arguments) {
    write_line(level, module, &[], args, false);
}

pub fn write_fields(level: Level, module: &str, fields: &[(&str, Value)], args: Arguments) {
    write_line(level, module, fields, args, false);
}

/// A trace line (the rule log.trace): written whatever the level, as a `debug` line.
pub fn trace(module: &str, args: Arguments) {
    write_line(Level::Debug, module, &[], args, true);
}

#[allow(unused_macros)]
macro_rules! error {
    ($($k:literal = $v:expr),+ ; $($arg:tt)+) => { $crate::logging::write_fields($crate::logging::Level::Error, module_path!(), &[$(($k, ::serde_json::json!($v))),+], format_args!($($arg)+)) };
    ($($arg:tt)+) => { $crate::logging::write($crate::logging::Level::Error, module_path!(), format_args!($($arg)+)) };
}
#[allow(unused_macros)]
macro_rules! warn {
    ($($k:literal = $v:expr),+ ; $($arg:tt)+) => { $crate::logging::write_fields($crate::logging::Level::Warn, module_path!(), &[$(($k, ::serde_json::json!($v))),+], format_args!($($arg)+)) };
    ($($arg:tt)+) => { $crate::logging::write($crate::logging::Level::Warn, module_path!(), format_args!($($arg)+)) };
}
#[allow(unused_macros)]
macro_rules! info {
    ($($k:literal = $v:expr),+ ; $($arg:tt)+) => { $crate::logging::write_fields($crate::logging::Level::Info, module_path!(), &[$(($k, ::serde_json::json!($v))),+], format_args!($($arg)+)) };
    ($($arg:tt)+) => { $crate::logging::write($crate::logging::Level::Info, module_path!(), format_args!($($arg)+)) };
}
#[allow(unused_macros)]
macro_rules! debug {
    ($($k:literal = $v:expr),+ ; $($arg:tt)+) => { $crate::logging::write_fields($crate::logging::Level::Debug, module_path!(), &[$(($k, ::serde_json::json!($v))),+], format_args!($($arg)+)) };
    ($($arg:tt)+) => { $crate::logging::write($crate::logging::Level::Debug, module_path!(), format_args!($($arg)+)) };
}
#[allow(unused_macros)]
macro_rules! trace {
    ($($arg:tt)+) => { $crate::logging::trace(module_path!(), format_args!($($arg)+)) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_millis(1_792_517_846_123)
    }

    #[test]
    fn a_time_is_utc_to_the_millisecond() {
        assert_eq!(timestamp(at()), "2026-10-20T17:37:26.123Z");
        assert_eq!(timestamp(SystemTime::UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(timestamp(SystemTime::UNIX_EPOCH + Duration::from_secs(951_782_400)), "2000-02-29T00:00:00.000Z");
        assert_eq!(timestamp(SystemTime::UNIX_EPOCH + Duration::from_secs(4_102_444_799)), "2099-12-31T23:59:59.000Z");
    }

    #[test]
    fn a_line_has_time_level_component_message_and_fields_in_both_formats() {
        let fields = [("client", json!(7)), ("name", json!("Ann"))];
        assert_eq!(render(false, false, Level::Info, "front", "admin: listening", &[], at(), 42, "front"), "2026-10-20T17:37:26.123Z INFO  front: admin: listening");
        assert_eq!(render(false, false, Level::Warn, "bank", "client 7 dropped", &fields, at(), 42, "bank"), "2026-10-20T17:37:26.123Z WARN  bank: client 7 dropped");
        let json: Value = serde_json::from_str(&render(true, false, Level::Error, "utp", "bad \"packet\"\nsecond line", &fields, at(), 42, "front")).unwrap();
        assert_eq!(
            json,
            json!({"timestamp": "2026-10-20T17:37:26.123Z", "level": "error", "component": "utp", "message": "bad \"packet\"\nsecond line", "fields": {"process": "front", "pid": 42, "client": 7, "name": "Ann"}})
        );
        // One physical line each in JSON; in text a message of several lines marks each as part of it.
        assert!(!render(true, false, Level::Info, "x", "a\nb", &[], at(), 1, "front").contains('\n'));
        let text = render(false, false, Level::Info, "x", "first\nsecond\nthird", &[], at(), 1, "front");
        assert_eq!(text.lines().count(), 3);
        assert!(text.lines().all(|l| l.starts_with("2026-10-20T17:37:26.123Z INFO  x: ")) && text.lines().nth(1).unwrap().ends_with("x:   second"));
        // Under journald each line starts with its priority.
        assert!(render(false, true, Level::Error, "x", "a", &[], at(), 1, "front").starts_with("<3>2026-10-20T"));
        assert!(render(true, true, Level::Warn, "x", "a", &[], at(), 1, "front").starts_with("<4>{\"timestamp\""));
        assert!(render(false, true, Level::Info, "x", "a\nb", &[], at(), 1, "front").lines().all(|l| l.starts_with("<6>")));
    }

    /// A message cannot rewrite the line before it or the terminal: control characters other than
    /// line breaks and tabs are written out, in text; JSON escapes them in its own way.
    #[test]
    fn a_message_cannot_steer_a_terminal_through_the_log() {
        let hostile = "name\r2026-10-20T00:00:00.000Z INFO  front: forged\x1b[2J\u{85}\x07 end\r\nsecond\tline";
        let text = render(false, false, Level::Warn, "front", hostile, &[], at(), 1, "front");
        assert!(text.chars().all(|c| !c.is_control() || c == '\n' || c == '\t'), "{text:?}");
        assert_eq!(text.lines().count(), 2, "a CRLF is one line break, a lone CR is none: {text:?}");
        assert!(text.contains("name\\u{d}2026") && text.contains("\\u{1b}[2J") && text.contains("second\tline"), "{text:?}");
        let json = render(true, false, Level::Warn, "front", hostile, &[], at(), 1, "front");
        assert!(json.chars().all(|c| !c.is_control()), "{json:?}");
        assert_eq!(serde_json::from_str::<Value>(&json).unwrap()["message"], hostile);
    }

    #[test]
    fn the_level_leaves_out_what_is_below_it_and_a_trace_line_is_always_there() {
        let wrote = |at_most: Level, level: Level, forced: bool| build(false, at_most, level, "pcds::game", &[], "x", forced).is_some();
        for (set_to, expect) in [(Level::Error, [true, false, false, false]), (Level::Warn, [true, true, false, false]), (Level::Info, [true, true, true, false]), (Level::Debug, [true, true, true, true])] {
            let got = [Level::Error, Level::Warn, Level::Info, Level::Debug].map(|l| wrote(set_to, l, false));
            assert_eq!(got, expect, "{set_to:?}");
        }
        assert!(wrote(Level::Error, Level::Debug, true), "log.trace is asked for by name: it is written at any level");
        assert_eq!(([Level::Error, Level::Warn, Level::Info, Level::Debug].map(Level::name), Level::parse("warn"), Level::parse("trace")), (LEVELS, Some(Level::Warn), None));
        assert_eq!((Format::parse("json"), Format::parse("text"), Format::parse("xml")), (Some(Format::Json), Some(Format::Text), None));
        // The real writer reads the process's settings; the rules' own tests change them, so this only checks the path is there.
        let _ = line(Level::Info, "pcds::game", &[], format_args!("x"), true).unwrap();
    }

    #[test]
    fn a_rules_file_sets_the_format_and_level_before_the_rules_load() {
        let dir = std::env::temp_dir().join(format!("pcds-log-init-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rules.json").to_string_lossy().to_string();
        std::fs::write(&path, r#"{"log": {"format": "json", "level": "debug"}}"#).unwrap();
        assert_eq!(settings_in(&path), (Format::Json, Level::Debug));
        // A file that does not read, or a value that is no value: the defaults (the bank refuses the file itself).
        std::fs::write(&path, r#"{"log": {"format": "xml", "level": "loud"}}"#).unwrap();
        assert_eq!(settings_in(&path), (Format::Text, Level::Info));
        std::fs::write(&path, "{").unwrap();
        assert_eq!(settings_in(&path), (Format::Text, Level::Info));
        assert_eq!(settings_in(&dir.join("nothing.json").to_string_lossy()), (Format::Text, Level::Info));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn nothing_secret_is_written_in_either_format() {
        let admin = "0123456789abcdef0123456789abcdef";
        let steam = "STEAMTOKEN-1234567890ABCDEF";
        let hook = "https://hooks.example.com/services/T000/B000/XXXXXXXXXXXXXXXX";
        let s3 = "AKIAIOSFODNN7EXAMPLE";
        let secrets = [admin, steam, hook, s3];
        let scoped = "pcdst_0123456789abcdef0123456789abcdef0123456789abcdef";
        let message = format!("admin {admin} steam {steam} webhook {hook} key {s3} scoped {scoped} header Authorization: Bearer abc.def-ghi and short stays");
        for json in [false, true] {
            let fields = [("url", json!(hook)), ("n", json!(3))];
            let fields: Vec<(&str, Value)> = fields.iter().map(|(k, v)| (*k, if let Value::String(s) = v { Value::String(redact_with(s, secrets.iter().copied())) } else { v.clone() })).collect();
            let written = build(json, Level::Info, Level::Warn, "pcds::front::chat_hooks", &fields, &redact_with(&message, secrets.iter().copied()), false).unwrap();
            for leaked in [admin, steam, hook, s3, "0123456789abcdef0123456789abcdef0123456789abcdef", "abc.def-ghi", "XXXXXXXXXXXXXXXX"] {
                assert!(!written.contains(leaked), "json={json} wrote {leaked}: {written}");
            }
            assert!(written.contains("***") && written.contains("short stays"), "{written}");
        }
        // Without the registered secrets only the patterns are masked; a bare word is left alone.
        assert_eq!(redact_with(&format!("webhook {hook}"), std::iter::empty()), format!("webhook {hook}"));
        assert_eq!(redact_with("Bearer", std::iter::empty()), "Bearer");
        assert_eq!(redact_with("a pcdst_ b", std::iter::empty()), "a pcdst_ b");
        // The registry: a short value is never kept, a long one is, and the rules' set is replaced as a whole.
        secret("abc");
        secret("a-secret-long-enough");
        rule_secrets(["ruleSecretValue".to_string(), "x".to_string()]);
        let hidden = redact("abc a-secret-long-enough ruleSecretValue x");
        assert_eq!(hidden, "abc *** *** x");
        rule_secrets([]);
        assert_eq!(redact("ruleSecretValue"), "ruleSecretValue");
    }

    #[test]
    fn the_component_is_the_part_of_the_server_that_wrote() {
        assert_eq!(component("pcds::front::flip"), "front");
        assert_eq!(component("pcds::game"), "game");
        assert_eq!(component("odd"), "odd");
        assert_eq!(component("pcds::cli::setup"), "run");
        // The binary's crate is `pcds`: what every real call site's module_path!() starts with.
        assert!(module_path!().starts_with("pcds::logging"), "{}", module_path!());
    }
}

#[cfg(any(fuzzing, pcds_corpus))]
#[path = "../fuzz/targets/logline.rs"]
pub mod fuzz;
