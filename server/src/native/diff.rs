//! `pcds-server tables-diff EXPECTED_DIR ACTUAL_DIR [SECTION]`: compares two game table folders
//! (data/<version>/) section by section, floats within 1e-5 (relative above 1, absolute below), and prints how much of
//! each section matches with the first differences by JSON path. The native reader is checked
//! against the tables the game itself produced this way.

use serde_json::Value;

fn floats_equal(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() <= 1e-5 * a.abs().max(b.abs()).max(1.0)
}

/// Differences under `path`, at most `limit` of them pushed to `out`; returns their count.
fn walk(path: &str, a: &Value, b: &Value, out: &mut Vec<String>, limit: usize) -> usize {
    fn note(out: &mut Vec<String>, limit: usize, msg: String) -> usize {
        if out.len() < limit {
            out.push(msg);
        }
        1
    }
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let mut n = 0;
            for (k, v) in x {
                n += match y.get(k) {
                    Some(w) => walk(&format!("{path}.{k}"), v, w, out, limit),
                    None => note(out, limit, format!("{path}.{k}: missing")),
                };
            }
            for k in y.keys().filter(|k| !x.contains_key(*k)) {
                n += note(out, limit, format!("{path}.{k}: extra"));
            }
            n
        }
        (Value::Array(x), Value::Array(y)) => {
            let mut n = 0;
            if x.len() != y.len() {
                n += note(out, limit, format!("{path}: {} items, not {}", y.len(), x.len()));
            }
            for (i, (v, w)) in x.iter().zip(y).enumerate() {
                n += walk(&format!("{path}[{i}]"), v, w, out, limit);
            }
            n
        }
        (Value::Number(x), Value::Number(y)) if floats_equal(x.as_f64().unwrap_or(f64::NAN), y.as_f64().unwrap_or(f64::NAN)) => 0,
        _ if a == b => 0,
        _ => note(out, limit, format!("{path}: {} != {}", short(b), short(a))),
    }
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.len() > 120 {
        format!("{}...", &s[..117])
    } else {
        s
    }
}

/// One section: how many of its entries (keys or items) are equal, and the first differences.
fn section(name: &str, a: &Value, b: &Value, show: usize) {
    let (total, equal) = match (a, b) {
        (Value::Object(x), Value::Object(y)) => (x.len(), x.iter().filter(|(k, v)| y.get(*k).is_some_and(|w| walk("", v, w, &mut Vec::new(), 0) == 0)).count()),
        (Value::Array(x), Value::Array(y)) => (x.len(), x.iter().zip(y).filter(|(v, w)| walk("", v, w, &mut Vec::new(), 0) == 0).count()),
        _ => (1, usize::from(walk("", a, b, &mut Vec::new(), 0) == 0)),
    };
    let mut out = Vec::new();
    let differences = walk(name, a, b, &mut out, show);
    println!("{name}: {equal}/{total} equal, {differences} differences");
    for d in out {
        println!("    {d}");
    }
}

fn load(dir: &str, file: &str) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(format!("{dir}/{file}")).ok()?).ok()
}

pub fn run(args: &[String]) {
    let (Some(expected), Some(actual)) = (args.first(), args.get(1)) else {
        eprintln!("usage: pcds-server tables-diff EXPECTED_DIR ACTUAL_DIR [SECTION]");
        std::process::exit(2);
    };
    let only = args.get(2).map(String::as_str);
    let show = std::env::var("PCDS_DIFF_SHOW").ok().and_then(|v| v.parse().ok()).unwrap_or(8);
    let mut files = vec!["gamedata.json".to_string(), "netdata.json".to_string(), "rpcs.json".to_string()];
    let mut scenes: Vec<String> = std::fs::read_dir(format!("{expected}/scenes")).into_iter().flatten().flatten().map(|e| format!("scenes/{}", e.file_name().to_string_lossy())).collect();
    scenes.sort();
    files.extend(scenes);
    for file in files {
        let (Some(a), b) = (load(expected, &file), load(actual, &file)) else { continue };
        let Some(b) = b else {
            println!("{file}: missing in {actual}");
            continue;
        };
        println!("== {file}");
        match (&a, &b) {
            // rpcs.json is one table keyed by hash; the others are objects of tables.
            (Value::Object(x), Value::Object(y)) if file != "rpcs.json" => {
                for (k, v) in x {
                    let name = format!("{}.{k}", file.trim_end_matches(".json"));
                    if only.is_some_and(|o| !name.contains(o)) {
                        continue;
                    }
                    match y.get(k) {
                        Some(w) => section(&name, v, w, show),
                        None => println!("{name}: missing"),
                    }
                }
            }
            _ => section(&file, &a, &b, show),
        }
    }
}
