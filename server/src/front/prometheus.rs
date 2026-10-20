//! GET /api/metrics: the server's numbers in the Prometheus text format (version 0.0.4), which
//! Prometheus, VictoriaMetrics, Grafana Agent, Telegraf and the Datadog agent read. `render` is a
//! pure function of one JSON value that the front puts together (admin_routes.rs): the bank's part
//! (the status and the plugins), the front's own counters, and the processes' memory. A test parses
//! what it writes with a strict parser of the format, and docs/monitoring.md lists every name.
//!
//! Naming follows the Prometheus guide: base units (seconds, bytes), `_total` on counters, a
//! timestamp as a gauge of seconds since 1970 (the age is `time() - metric` in the query, so the
//! server keeps no clock of its own for it), one family per name with its HELP and TYPE once and
//! its samples together.

use serde_json::Value;

/// One family: a name, its type, its help and its samples (label pairs and a value).
struct Family<'a> {
    name: &'a str,
    kind: &'static str,
    help: &'a str,
    samples: Vec<(Vec<(&'a str, String)>, f64)>,
}

#[derive(Default)]
struct Out {
    text: String,
}

impl Out {
    fn family(&mut self, f: Family) {
        if f.samples.is_empty() {
            return;
        }
        self.text += &format!("# HELP {} {}\n# TYPE {} {}\n", f.name, f.help.replace('\\', "\\\\").replace('\n', "\\n"), f.name, f.kind);
        for (labels, value) in f.samples {
            self.text += f.name;
            if !labels.is_empty() {
                let pairs: Vec<String> = labels.iter().map(|(k, v)| format!("{k}=\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"))).collect();
                self.text += &format!("{{{}}}", pairs.join(","));
            }
            self.text += &format!(" {}\n", number(value));
        }
    }

    /// One sample without labels, when there is a value.
    fn one(&mut self, name: &str, kind: &'static str, help: &str, value: Option<f64>) {
        if let Some(v) = value {
            self.family(Family { name, kind, help, samples: vec![(vec![], v)] });
        }
    }

    fn gauge(&mut self, name: &str, help: &str, value: Option<f64>) {
        self.one(name, "gauge", help, value);
    }

    fn counter(&mut self, name: &str, help: &str, value: Option<f64>) {
        self.one(name, "counter", help, value);
    }
}

/// A sample value: a whole number without a fraction, else the shortest text that reads back the
/// same. NaN and infinities are never written (callers pass finite numbers; this keeps it so).
fn number(v: f64) -> String {
    if !v.is_finite() {
        return "0".into();
    }
    if v.fract() == 0.0 && v.abs() < 1e15 { format!("{}", v as i64) } else { format!("{v}") }
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64().filter(|f| f.is_finite())
}

fn flag(v: &Value) -> Option<f64> {
    v.as_bool().map(|b| if b { 1.0 } else { 0.0 })
}

/// A label value from JSON text or a number, "" when there is none.
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The states a plugin can be in (plugins/mod.rs Status::name), each always present so that an
/// alert on `pcds_plugins{state="failed"} > 0` has a series to read from the first scrape.
const PLUGIN_STATES: [&str; 8] = ["running", "pending", "disabled", "changed", "invalid", "incompatible", "failed", "unloading"];

/// The text of GET /api/metrics for what the front put together: `status` (GET /api/status as the
/// admin sees it, front part included), `plugins` (GET /api/plugins' list), `front` (counters and
/// the processes' memory) and `bank` (what only the bank knows: the players' limit, how long the
/// last save took).
pub fn render(m: &Value) -> String {
    let (s, front, bank) = (&m["status"], &m["front"], &m["bank"]);
    let running = s["state"] == "running";
    let mut o = Out::default();
    o.family(Family {
        name: "pcds_info",
        kind: "gauge",
        help: "The server's version and formats, always 1.",
        samples: vec![(
            vec![
                ("version", text(&s["serverVersion"])),
                ("bank", text(&s["banks"]["live"]["bank"])),
                ("bank_version", text(&s["banks"]["live"]["version"])),
                ("front_protocol", text(&s["banks"]["frontProtocol"])),
                ("snapshot_format", text(&s["banks"]["live"]["format"])),
                ("game_version", text(&s["gameVersion"])),
            ],
            1.0,
        )],
    });
    o.family(Family { name: "pcds_world_info", kind: "gauge", help: "The world that runs and its planet, always 1 while a world runs.", samples: if running { vec![(vec![("world", text(&s["save"])), ("planet", text(&s["planet"]))], 1.0)] } else { vec![] } });
    o.gauge("pcds_world_running", "1 while the world runs, 0 while it is stopped, starting or recovering.", Some(if running { 1.0 } else { 0.0 }));
    o.gauge("pcds_paused", "1 while the world is paused by the admin.", flag(&s["held"]));
    o.gauge("pcds_uptime_seconds", "Seconds since the front started.", num(&s["uptimeSeconds"]));
    o.gauge("pcds_players_online", "Players in the game.", running.then(|| s["players"].as_array().map_or(0.0, |p| p.len() as f64)));
    o.gauge("pcds_players_max", "The most players at once (the rule limits.maxPlayers).", num(&bank["maxPlayers"]));
    o.family(Family {
        name: "pcds_connections",
        kind: "gauge",
        help: "Connections of players, joining ones included, by transport.",
        samples: ["ip", "steam"].iter().filter_map(|t| Some((vec![("transport", t.to_string())], num(&front["connections"][*t])?))).collect(),
    });
    o.gauge("pcds_tick_p50_seconds", "Median time of a game tick over the last minute.", num(&s["tick"]["p50Micros"]).map(|u| u / 1e6));
    o.gauge("pcds_tick_p99_seconds", "99th percentile time of a game tick over the last minute.", num(&s["tick"]["p99Micros"]).map(|u| u / 1e6));
    o.gauge("pcds_tick_max_seconds", "Longest game tick over the last minute.", num(&s["tick"]["maxMicros"]).map(|u| u / 1e6));
    o.gauge("pcds_tick_samples", "Ticks the tick times above are taken from (the last minute).", num(&s["tick"]["samples"]));
    o.gauge("pcds_loop_busy_ratio", "Share of the last second the game loop worked (0 to 1).", num(&s["busyPercent"]).map(|p| p / 100.0));
    o.gauge("pcds_loop_longest_seconds", "Longest pass of the game loop in the last second.", num(&s["longestLoopMicros"]).map(|u| u / 1e6));
    o.gauge("pcds_loop_rate_hertz", "Passes of the game loop in the last second.", num(&s["fps"]));
    o.family(Family {
        name: "pcds_memory_resident_bytes",
        kind: "gauge",
        help: "Resident memory of the server's processes: the front, the bank that runs the world, and a bank starting up for a hot update.",
        samples: ["front", "bank", "next_bank"].iter().filter_map(|p| Some((vec![("process", p.to_string())], num(&front["memory"][*p])?))).collect(),
    });
    o.counter("pcds_cpu_seconds_total", "CPU time of the front and the banks, ended ones included.", num(&s["process"]["cpuSeconds"]));
    o.gauge("pcds_world_objects", "World objects (buildings, items, plants) in the world.", num(&s["objects"]["world"]));
    o.gauge("pcds_network_objects", "Objects the clients are told about.", num(&s["objects"]["network"]));
    o.gauge("pcds_inventories", "Inventories in the world.", num(&s["objects"]["inventories"]));
    o.counter("pcds_network_receive_bytes_total", "Bytes of game batches received from players.", num(&s["traffic"]["bytesIn"]));
    o.counter("pcds_network_transmit_bytes_total", "Bytes of game batches sent to players.", num(&s["traffic"]["bytesOut"]));

    // Saves and backups. The age of a save is `time() - pcds_save_last_timestamp_seconds`.
    let save = &s["lastSave"];
    o.gauge("pcds_save_last_timestamp_seconds", "When the last save ended, Unix time. Absent until the first save of this start.", num(&save["at"]));
    o.gauge("pcds_save_last_ok", "1 if the last save was written, 0 if it failed.", flag(&save["ok"]));
    o.gauge("pcds_save_last_duration_seconds", "How long the last save took, from the text being made to the file being on disk.", num(&bank["saveMillis"]).map(|ms| ms / 1e3));
    o.gauge("pcds_backup_enabled", "1 if the world is backed up (the rule save.backupsKept is above 0).", flag(&s["backup"]["enabled"]));
    o.gauge("pcds_backup_last_success_timestamp_seconds", "When the last backup was made, Unix time. Absent until one is made after this start.", num(&s["backup"]["lastOkAt"]));
    o.gauge("pcds_backup_last_ok", "1 if the last backup attempt worked, 0 if it failed.", s["backup"]["enabled"].as_bool().map(|_| if s["backup"]["error"].is_null() { 1.0 } else { 0.0 }));
    let s3 = &s["s3"];
    o.gauge("pcds_s3_enabled", "1 if backups are copied to an S3 bucket (the rule backup.s3Bucket is set).", flag(&s3["enabled"]));
    o.gauge("pcds_s3_uploading", "1 while a backup is being copied to the bucket.", flag(&s3["uploading"]));
    o.gauge("pcds_s3_last_upload_ok", "1 if the last copy to the bucket worked, 0 if it failed.", flag(&s3["last"]["ok"]));
    o.gauge("pcds_s3_last_upload_timestamp_seconds", "When the last copy to the bucket ended, Unix time.", num(&s3["last"]["at"]));

    // The processes: flips, crashes, the watchdog, the clients waiting for a bank.
    o.counter("pcds_flips_total", "Hot updates and rollbacks that handed the world to the other bank.", num(&s["banks"]["flips"]));
    o.counter("pcds_flips_failed_total", "Hand-overs that failed and left the old bank running.", num(&front["flipsFailed"]));
    o.gauge("pcds_flip_last_stand_seconds", "How long the world stood in the last hand-over. Absent until one has happened.", num(&front["lastStandMillis"]).map(|ms| ms / 1e3));
    o.counter("pcds_crashes_total", "Times the game process ended outside a hand-over.", num(&front["crashes"]));
    o.gauge("pcds_crashes_recent", "Crashes in the last ten minutes; the front gives up at five.", num(&s["banks"]["crashes"]));
    o.counter("pcds_watchdog_kills_total", "Times the watchdog killed a game process that said nothing (the rule limits.bankWatchdogSeconds).", num(&front["watchdogKills"]));
    o.gauge("pcds_queue_waiting_bytes", "Bytes of player frames waiting for the game process now.", num(&front["queue"]["waitingBytes"]));
    o.counter("pcds_queue_dropped_clients_total", "Players dropped because too much of their traffic waited for the game process.", num(&front["queue"]["droppedClients"]));
    o.counter("pcds_queue_dropped_bytes_total", "Bytes that were waiting for a game process when their player was dropped.", num(&front["queue"]["droppedBytes"]));
    o.gauge("pcds_update_available", "1 if a newer release than the one running is out (as of the last check).", flag(&s["update"]["available"]));

    // Plugins: how many in each state, and what each one costs.
    let plugins = m["plugins"].as_array().map(Vec::as_slice).unwrap_or_default();
    if running {
        o.family(Family {
            name: "pcds_plugins",
            kind: "gauge",
            help: "Plugins by state.",
            samples: PLUGIN_STATES.iter().map(|st| (vec![("state", st.to_string())], plugins.iter().filter(|p| p["status"] == *st).count() as f64)).collect(),
        });
    }
    let per_plugin = |key: &str| -> Vec<(Vec<(&str, String)>, f64)> { plugins.iter().filter_map(|p| Some((vec![("plugin", text(&p["id"]))], num(&p["stats"][key])?))).collect() };
    for (name, key, help) in [
        ("pcds_plugin_calls_total", "calls", "Events and commands handed to the plugin."),
        ("pcds_plugin_faults_total", "faults", "Calls that trapped, ran out of fuel or broke a limit."),
        ("pcds_plugin_restarts_total", "restarts", "Times the plugin was started again after a fault."),
        ("pcds_plugin_slow_total", "slow", "Calls that took longer than the plugin's time budget."),
        ("pcds_plugin_shed_total", "shed", "Events left out because the plugin was too slow to take them."),
        ("pcds_plugin_denied_total", "denied", "Calls refused for a capability the plugin was not granted."),
        ("pcds_plugin_refused_total", "refused", "Calls refused for a limit (size, rate, a bad value)."),
        ("pcds_plugin_actions_total", "actions", "Actions the plugin did in the world (chat, kicks, rule changes)."),
        ("pcds_plugin_vetoes_total", "vetoes", "Events the plugin vetoed."),
        ("pcds_plugin_fuel_total", "fuel", "WebAssembly fuel the plugin has spent."),
    ] {
        o.family(Family { name, kind: "counter", help, samples: per_plugin(key) });
    }
    o.family(Family {
        name: "pcds_plugin_running",
        kind: "gauge",
        help: "1 if the plugin runs, 0 in any other state.",
        samples: plugins.iter().map(|p| (vec![("plugin", text(&p["id"])), ("state", text(&p["status"]))], if p["status"] == "running" { 1.0 } else { 0.0 })).collect(),
    });
    o.text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::{BTreeMap, BTreeSet};

    /// A strict reader of the text format: what a scraper would refuse, this refuses. Returns
    /// (family, labels, value) for every sample.
    pub fn parse(text: &str) -> Result<Vec<(String, BTreeMap<String, String>, f64)>, String> {
        let name_ok = |n: &str, colon: bool| !n.is_empty() && !n.as_bytes()[0].is_ascii_digit() && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || (colon && b == b':'));
        if !text.ends_with('\n') {
            return Err("the text must end with a line break".into());
        }
        let (mut helps, mut types, mut done, mut series) = (BTreeSet::new(), BTreeMap::new(), BTreeSet::new(), BTreeSet::new());
        let (mut open, mut samples) = (None::<String>, Vec::new());
        for (n, line) in text.lines().enumerate() {
            let at = |e: &str| format!("line {}: {e}: {line:?}", n + 1);
            if line.is_empty() {
                return Err(at("empty line"));
            }
            if let Some(rest) = line.strip_prefix("# HELP ") {
                let (name, help) = rest.split_once(' ').ok_or_else(|| at("HELP without text"))?;
                if !name_ok(name, true) || !helps.insert(name.to_string()) {
                    return Err(at("bad or repeated HELP"));
                }
                // Only \\ and \n are escapes in help text.
                let mut chars = help.chars();
                while let Some(c) = chars.next() {
                    if c == '\\' && !matches!(chars.next(), Some('\\' | 'n')) {
                        return Err(at("bad escape in help"));
                    }
                }
            } else if let Some(rest) = line.strip_prefix("# TYPE ") {
                let (name, kind) = rest.split_once(' ').ok_or_else(|| at("TYPE without a type"))?;
                if !name_ok(name, true) || !["counter", "gauge", "histogram", "summary", "untyped"].contains(&kind) || types.insert(name.to_string(), kind.to_string()).is_some() {
                    return Err(at("bad or repeated TYPE"));
                }
                if kind == "counter" && !name.ends_with("_total") {
                    return Err(at("a counter's name ends in _total"));
                }
                if !helps.contains(name) {
                    return Err(at("TYPE before HELP"));
                }
            } else if line.starts_with('#') {
                return Err(at("a comment other than HELP and TYPE"));
            } else {
                let name_end = line.find(['{', ' ']).ok_or_else(|| at("no value"))?;
                let name = &line[..name_end];
                if !name_ok(name, true) || !types.contains_key(name) {
                    return Err(at("a sample of a family with no TYPE"));
                }
                if open.as_deref() != Some(name) {
                    if let Some(prev) = open.replace(name.to_string()) {
                        done.insert(prev);
                    }
                    if done.contains(name) {
                        return Err(at("the samples of a family are not together"));
                    }
                }
                let mut labels = BTreeMap::new();
                let mut rest = &line[name_end..];
                if let Some(inner) = rest.strip_prefix('{') {
                    let (bytes, mut start) = (inner.as_bytes(), 0);
                    loop {
                        let eq = inner[start..].find('=').ok_or_else(|| at("label without ="))? + start;
                        let key = &inner[start..eq];
                        if !name_ok(key, false) || key.starts_with("__") || labels.contains_key(key) {
                            return Err(at("bad or repeated label name"));
                        }
                        if bytes.get(eq + 1) != Some(&b'"') {
                            return Err(at("label value not quoted"));
                        }
                        let (mut value, mut i) = (String::new(), eq + 2);
                        loop {
                            match bytes.get(i).ok_or_else(|| at("label value not closed"))? {
                                b'"' => break,
                                b'\\' => {
                                    value.push(match bytes.get(i + 1) {
                                        Some(b'\\') => '\\',
                                        Some(b'"') => '"',
                                        Some(b'n') => '\n',
                                        _ => return Err(at("bad escape in a label value")),
                                    });
                                    i += 2;
                                }
                                b'\n' => return Err(at("raw line break in a label value")),
                                _ => {
                                    let c = inner[i..].chars().next().unwrap();
                                    value.push(c);
                                    i += c.len_utf8();
                                }
                            }
                        }
                        labels.insert(key.to_string(), value);
                        match bytes.get(i + 1) {
                            Some(b',') => start = i + 2,
                            Some(b'}') => {
                                rest = &inner[i + 2..];
                                break;
                            }
                            _ => return Err(at("expected , or } after a label")),
                        }
                    }
                }
                let value = rest.strip_prefix(' ').ok_or_else(|| at("no space before the value"))?;
                let value: f64 = value.parse().map_err(|_| at("the value is not a number"))?;
                if !value.is_finite() {
                    return Err(at("NaN and infinities are not written"));
                }
                if !series.insert(format!("{name}{labels:?}")) {
                    return Err(at("the same series twice"));
                }
                samples.push((name.to_string(), labels, value));
            }
        }
        Ok(samples)
    }

    fn running() -> Value {
        json!({
            "status": {
                "state": "running", "serverVersion": "0.8.0", "gameVersion": "2.103", "save": "my \"base\"\\x", "planet": "Prime", "held": false,
                "uptimeSeconds": 3600, "players": [{"id": 1}, {"id": 2}],
                "tick": {"samples": 1000, "p50Micros": 150, "p99Micros": 900, "maxMicros": 4000},
                "busyPercent": 12.5, "longestLoopMicros": 5000, "fps": 200,
                "process": {"cpuSeconds": 12.5}, "objects": {"world": 33583, "network": 1200, "inventories": 2246},
                "traffic": {"bytesIn": 100, "bytesOut": 2000},
                "lastSave": {"ok": true, "at": 1_792_000_000, "error": null},
                "backup": {"enabled": true, "lastOkAt": 1_792_000_100, "error": null},
                "s3": {"enabled": true, "uploading": false, "last": {"ok": false, "at": 1_792_000_200, "error": "no"}},
                "banks": {"flips": 3, "crashes": 1, "frontProtocol": 3, "live": {"bank": "a", "version": "0.8.0", "format": 7}},
                "update": {"available": true}
            },
            "plugins": [
                {"id": "welcome", "status": "running", "stats": {"calls": 10, "faults": 0, "restarts": 0, "slow": 1, "shed": 0, "denied": 2, "refused": 0, "actions": 5, "vetoes": 0, "fuel": 123456}},
                {"id": "broken", "status": "failed", "stats": {"calls": 3, "faults": 3, "restarts": 2, "slow": 0, "shed": 0, "denied": 0, "refused": 0, "actions": 0, "vetoes": 0, "fuel": 99}}
            ],
            "front": {"connections": {"ip": 1, "steam": 1}, "memory": {"front": 50_000_000, "bank": 80_000_000, "next_bank": null}, "flipsFailed": 1, "lastStandMillis": 12.5, "crashes": 4, "watchdogKills": 2, "queue": {"waitingBytes": 10, "droppedClients": 1, "droppedBytes": 17_000_000}},
            "bank": {"maxPlayers": 16, "saveMillis": 250.0}
        })
    }

    fn value(samples: &[(String, BTreeMap<String, String>, f64)], name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        samples.iter().find(|(n, l, _)| n == name && labels.iter().all(|(k, v)| l.get(*k).map(String::as_str) == Some(*v))).map(|s| s.2)
    }

    #[test]
    fn what_is_written_is_what_a_strict_scraper_reads() {
        let text = render(&running());
        let samples = parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        let v = |n: &str, l: &[(&str, &str)]| value(&samples, n, l).unwrap_or_else(|| panic!("no {n} {l:?}\n{text}"));
        assert_eq!((v("pcds_players_online", &[]), v("pcds_players_max", &[]), v("pcds_world_running", &[]), v("pcds_paused", &[])), (2.0, 16.0, 1.0, 0.0));
        assert_eq!((v("pcds_tick_p50_seconds", &[]), v("pcds_tick_p99_seconds", &[]), v("pcds_tick_max_seconds", &[])), (0.00015, 0.0009, 0.004));
        assert_eq!((v("pcds_loop_busy_ratio", &[]), v("pcds_memory_resident_bytes", &[("process", "bank")]), v("pcds_world_objects", &[])), (0.125, 80_000_000.0, 33583.0));
        assert!(value(&samples, "pcds_memory_resident_bytes", &[("process", "next_bank")]).is_none(), "a bank that is not there has no series");
        assert_eq!((v("pcds_connections", &[("transport", "steam")]), v("pcds_save_last_ok", &[]), v("pcds_save_last_duration_seconds", &[]), v("pcds_backup_last_ok", &[])), (1.0, 1.0, 0.25, 1.0));
        assert_eq!((v("pcds_s3_last_upload_ok", &[]), v("pcds_flips_total", &[]), v("pcds_flip_last_stand_seconds", &[]), v("pcds_crashes_total", &[])), (0.0, 3.0, 0.0125, 4.0));
        assert_eq!((v("pcds_watchdog_kills_total", &[]), v("pcds_queue_dropped_bytes_total", &[]), v("pcds_update_available", &[])), (2.0, 17_000_000.0, 1.0));
        assert_eq!((v("pcds_plugins", &[("state", "running")]), v("pcds_plugins", &[("state", "failed")]), v("pcds_plugins", &[("state", "pending")])), (1.0, 1.0, 0.0));
        assert_eq!((v("pcds_plugin_fuel_total", &[("plugin", "welcome")]), v("pcds_plugin_faults_total", &[("plugin", "broken")]), v("pcds_plugin_running", &[("plugin", "broken")])), (123456.0, 3.0, 0.0));
        // The labels with quotes and backslashes come back as they went in.
        let world = samples.iter().find(|s| s.0 == "pcds_world_info").unwrap();
        assert_eq!((world.1["world"].as_str(), world.1["planet"].as_str()), ("my \"base\"\\x", "Prime"));
        let info = samples.iter().find(|s| s.0 == "pcds_info").unwrap();
        assert_eq!((info.1["version"].as_str(), info.1["bank"].as_str(), info.1["snapshot_format"].as_str(), info.1["front_protocol"].as_str()), ("0.8.0", "a", "7", "3"));
        // Every counter ends in _total and every family has its help and type once (the parser checks).
        assert!(text.contains("# TYPE pcds_flips_total counter\n") && text.contains("# TYPE pcds_info gauge\n"));
    }

    #[test]
    fn a_stopped_world_has_the_processes_and_no_world() {
        let stopped = json!({"status": {"state": "stopped", "serverVersion": "0.8.0", "uptimeSeconds": 5, "banks": {"flips": 0, "crashes": 0, "frontProtocol": 3, "live": null}, "process": {"cpuSeconds": 0.5}}, "plugins": [], "front": {"connections": {"ip": 0, "steam": 0}, "memory": {"front": 1, "bank": null}, "crashes": 0, "watchdogKills": 0}});
        let text = render(&stopped);
        let samples = parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        assert_eq!(value(&samples, "pcds_world_running", &[]), Some(0.0));
        for gone in ["pcds_players_online", "pcds_world_info", "pcds_world_objects", "pcds_plugins", "pcds_tick_p99_seconds", "pcds_save_last_ok"] {
            assert!(value(&samples, gone, &[]).is_none() && !text.contains(&format!("# TYPE {gone} ")), "{gone} is for a running world");
        }
        assert_eq!(value(&samples, "pcds_uptime_seconds", &[]), Some(5.0));
        // Nothing at all still parses: the text of an empty value is just the always-there gauge.
        parse(&render(&json!({}))).unwrap();
    }

    #[test]
    fn the_strict_parser_refuses_what_a_scraper_refuses() {
        for (bad, why) in [
            ("# TYPE a gauge\na 1\n", "TYPE before HELP"),
            ("# HELP a x\n# TYPE a gauge\na 1", "no final line break"),
            ("# HELP a x\n# TYPE a counter\na 1\n", "counter without _total"),
            ("# HELP a x\n# TYPE a gauge\na NaN\n", "NaN"),
            ("# HELP a x\n# TYPE a gauge\na{b=\"1\"} 1\na{b=\"1\"} 2\n", "the same series twice"),
            ("# HELP a x\n# TYPE a gauge\na{b=\"1\"} 1\n# HELP c x\n# TYPE c gauge\nc 1\na{b=\"2\"} 1\n", "family split in two"),
            ("# HELP a x\n# TYPE a gauge\na{b=1} 1\n", "label value unquoted"),
            ("# HELP a x\n# TYPE a gauge\na{b=\"x\\q\"} 1\n", "bad escape"),
            ("a 1\n", "no TYPE"),
            ("# HELP a x\n# TYPE a gauge\na 1\n\n", "empty line"),
        ] {
            assert!(parse(bad).is_err(), "{why}: {bad:?}");
        }
        assert!(parse("# HELP a_total x\n# TYPE a_total counter\na_total{b=\"1\",c=\"\\\"\"} 1e3\n").is_ok());
    }

    /// The front's own counters reach the text, and a front with no game process writes a
    /// stopped world and still parses.
    #[test]
    fn the_front_writes_its_own_counters() {
        let mut front = super::super::tests::test_front();
        front.totals.crashes = 3;
        front.totals.watchdog_kills = 1;
        front.totals.flips_failed = 2;
        front.totals.dropped_clients = 4;
        front.totals.dropped_bytes = 99;
        front.totals.last_stand_ms = Some(12.5);
        front.net.conns.insert(1, super::super::transport::Conn::Ip([0; 8]));
        let text = front.metrics_text(None);
        let samples = parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        let v = |n: &str, l: &[(&str, &str)]| value(&samples, n, l).unwrap_or_else(|| panic!("no {n}\n{text}"));
        assert_eq!((v("pcds_crashes_total", &[]), v("pcds_watchdog_kills_total", &[]), v("pcds_flips_failed_total", &[]), v("pcds_queue_dropped_clients_total", &[]), v("pcds_queue_dropped_bytes_total", &[])), (3.0, 1.0, 2.0, 4.0, 99.0));
        assert_eq!((v("pcds_flip_last_stand_seconds", &[]), v("pcds_world_running", &[]), v("pcds_connections", &[("transport", "ip")]), v("pcds_connections", &[("transport", "steam")])), (0.0125, 0.0, 1.0, 0.0));
        assert!(v("pcds_memory_resident_bytes", &[("process", "front")]) > 0.0 && value(&samples, "pcds_memory_resident_bytes", &[("process", "bank")]).is_none());
        // What the bank says is the status of a running world: the front adds its part to it.
        let bank = json!({"status": {"state": "running", "serverVersion": "0.8.0", "players": [{"id": 1}]}, "plugins": [], "bank": {"maxPlayers": 16}});
        let text = front.metrics_text(Some(bank));
        let samples = parse(&text).unwrap();
        assert_eq!((value(&samples, "pcds_players_online", &[]), value(&samples, "pcds_players_max", &[]), value(&samples, "pcds_world_running", &[])), (Some(1.0), Some(16.0), Some(1.0)));
    }

    #[test]
    fn numbers_are_written_whole_or_short() {
        for (v, text) in [(0.0, "0"), (42.0, "42"), (0.00015, "0.00015"), (1e-7, "0.0000001"), (12.5, "12.5"), (f64::NAN, "0"), (1e20, "100000000000000000000")] {
            assert_eq!(number(v), text);
            assert_eq!(number(v).parse::<f64>().unwrap(), if v.is_finite() { v } else { 0.0 });
        }
    }

    /// The table of docs/monitoring.md, from what `render` writes for a full sample: name, type,
    /// labels and help.
    fn markdown() -> String {
        let text = render(&running());
        let mut rows: Vec<(String, String, BTreeSet<String>, String)> = Vec::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# HELP ") {
                let (name, help) = rest.split_once(' ').unwrap();
                rows.push((name.to_string(), String::new(), BTreeSet::new(), help.to_string()));
            } else if let Some(rest) = line.strip_prefix("# TYPE ") {
                rows.last_mut().unwrap().1 = rest.split_once(' ').unwrap().1.to_string();
            } else if let Some(labels) = line.split_once('{').and_then(|(_, l)| l.split_once('}')).map(|(l, _)| l) {
                let row = rows.last_mut().unwrap();
                // Label names only: name="value" pairs, whatever the values hold.
                let mut rest = labels;
                while let Some((key, after)) = rest.split_once("=\"") {
                    row.2.insert(key.trim_start_matches(',').to_string());
                    let mut end = 0;
                    let b = after.as_bytes();
                    while b[end] != b'"' {
                        end += if b[end] == b'\\' { 2 } else { 1 };
                    }
                    rest = &after[end + 1..];
                }
            }
        }
        let mut out = String::from("| Name | Type | Labels | What it is |\n|---|---|---|---|\n");
        for (name, kind, labels, help) in rows {
            let labels = if labels.is_empty() { "none".to_string() } else { labels.iter().map(|l| format!("`{l}`")).collect::<Vec<_>>().join(", ") };
            out += &format!("| `{name}` | {kind} | {labels} | {} |\n", help.replace("\\n", " ").replace("\\\\", "\\"));
        }
        out
    }

    fn family_names(text: &str) -> BTreeSet<String> {
        text.lines().filter_map(|l| Some(l.strip_prefix("# TYPE ")?.split_once(' ')?.0.to_string())).collect()
    }

    #[test]
    fn docs_monitoring_lists_every_metric_and_names_no_other() {
        let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/monitoring.md");
        let doc = std::fs::read_to_string(&file).unwrap_or_default();
        let table = doc.split_once("<!-- metrics -->\n").and_then(|(_, rest)| rest.split_once("<!-- /metrics -->")).map(|(t, _)| t.to_string()).unwrap_or_default();
        assert!(table == markdown(), "the table in {} is not what render writes; it should be between <!-- metrics --> and <!-- /metrics -->:\n\n{}", file.display(), markdown());
        // Every metric the page, the alerts and the dashboard name exists.
        let known = family_names(&render(&running()));
        let stopped = family_names(&render(&json!({"status": {"state": "stopped"}})));
        assert!(stopped.is_subset(&known), "a metric only the stopped sample writes: {:?}", stopped.difference(&known));
        let dashboard = std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/grafana/pcds.json")).unwrap_or_default();
        for (what, text) in [("docs/monitoring.md", doc.replace(&table, "")), ("docs/grafana/pcds.json", dashboard.clone())] {
            let mut rest = text.as_str();
            let mut named = BTreeSet::new();
            while let Some(at) = rest.find("pcds_") {
                let word: String = rest[at..].chars().take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_').collect();
                rest = &rest[at + word.len()..];
                named.insert(word);
            }
            for name in named {
                assert!(known.contains(&name) || ["pcds_server", "pcds_home"].contains(&name.as_str()), "{what} names {name}, which is no metric");
            }
        }
        let parsed: Value = serde_json::from_str(&dashboard).expect("docs/grafana/pcds.json is JSON");
        assert!(parsed["panels"].as_array().is_some_and(|p| p.len() > 10) && parsed["uid"] == "pcds-server");
    }
}
