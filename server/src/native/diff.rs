//! `pcds tables-diff EXPECTED_DIR ACTUAL_DIR [SECTION]`: compares two game table folders
//! (data/<version>/) section by section, integers exactly and floats within 1e-5 (relative
//! above 1, absolute below), and prints how much of each section matches with the first
//! differences by JSON path; the scenes' water volumes and ore veins, listed in no order, item
//! by item; the text files (connection-request.hex, buildid) as they are. Exits 1 when anything
//! differs, a file in one folder only included. The native reader is checked against the tables
//! the game itself produced this way.

use serde_json::Value;

fn floats_equal(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() <= 1e-5 * a.abs().max(b.abs()).max(1.0)
}

/// Integers exactly (u64 and i64 alike, never through f64); a float on either side within the
/// tolerance.
fn numbers_equal(a: &serde_json::Number, b: &serde_json::Number) -> bool {
    if a.is_f64() || b.is_f64() {
        return a.as_f64().zip(b.as_f64()).is_some_and(|(a, b)| floats_equal(a, b));
    }
    (a.as_i64(), a.as_u64()) == (b.as_i64(), b.as_u64())
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
        (Value::Number(x), Value::Number(y)) if numbers_equal(x, y) => 0,
        _ if a == b => 0,
        _ => note(out, limit, format!("{path}: {} != {}", short(b), short(a))),
    }
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 120 {
        format!("{}...", s.chars().take(117).collect::<String>())
    } else {
        s
    }
}

/// One section: how many of its entries (keys or items) are equal, and the first differences.
fn section(name: &str, a: &Value, b: &Value, show: usize) -> usize {
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
    differences
}

/// A list in no particular order (FindObjectsByType): each expected item paired with an actual
/// one, equal ones first, then the closest; the actual ones left over are extra.
fn unordered(name: &str, a: &[Value], b: &[Value], show: usize) -> usize {
    let mut partner: Vec<Option<usize>> = vec![None; a.len()];
    let mut used = vec![false; b.len()];
    for (i, v) in a.iter().enumerate() {
        if let Some(j) = (0..b.len()).find(|&j| !used[j] && walk("", v, &b[j], &mut Vec::new(), 0) == 0) {
            (partner[i], used[j]) = (Some(j), true);
        }
    }
    let equal = partner.iter().flatten().count();
    let mut out = Vec::new();
    let mut differences = 0;
    for (i, v) in a.iter().enumerate().filter(|(i, _)| partner[*i].is_none()) {
        let Some(j) = (0..b.len()).filter(|&j| !used[j]).min_by_key(|&j| walk("", v, &b[j], &mut Vec::new(), 0)) else {
            differences += 1;
            out.push(format!("{name}[{i}]: no item left in actual"));
            continue;
        };
        used[j] = true;
        differences += walk(&format!("{name}[{i}~{j}]"), v, &b[j], &mut out, show);
    }
    let extra = used.iter().filter(|u| !**u).count();
    println!("{name}: {equal}/{} equal, {differences} differences in the others (paired with the closest), {extra} extra in actual", a.len());
    for d in out.iter().take(show) {
        println!("    {d}");
    }
    differences + extra
}

/// A file of a table folder: None when it is not there; a file there that does not parse is
/// an error.
fn load(dir: &str, file: &str) -> Result<Option<Value>, String> {
    let path = format!("{dir}/{file}");
    match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{path}: {e}")),
        Ok(text) if !file.ends_with(".json") => Ok(Some(Value::String(text))),
        Ok(text) => serde_json::from_str(&text).map(Some).map_err(|e| format!("{path}: {e}")),
    }
}

fn scene_files(dir: &str) -> Vec<String> {
    std::fs::read_dir(format!("{dir}/scenes")).into_iter().flatten().flatten().map(|e| format!("scenes/{}", e.file_name().to_string_lossy())).collect()
}

pub fn run(args: &[String]) {
    let (Some(expected), Some(actual)) = (args.first(), args.get(1)) else {
        eprintln!("usage: pcds tables-diff EXPECTED_DIR ACTUAL_DIR [SECTION [SHOWN_PER_FILE]]");
        std::process::exit(2);
    };
    let only = args.get(2).map(String::as_str);
    let show = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(8);
    match diff(expected, actual, only, show) {
        Ok(0) => println!("no differences"),
        Ok(n) => {
            println!("{n} differences");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("tables-diff: {e}");
            std::process::exit(2);
        }
    }
}

/// Every difference between two table folders, printed; returns their count.
pub(crate) fn diff(expected: &str, actual: &str, only: Option<&str>, show: usize) -> Result<usize, String> {
    let mut files: Vec<String> = ["gamedata.json", "netdata.json", "rpcs.json", "wrecks.json", "connection-request.hex", "buildid"].map(String::from).to_vec();
    let mut scenes = scene_files(expected);
    scenes.extend(scene_files(actual));
    scenes.sort();
    scenes.dedup();
    files.extend(scenes);
    let mut differences = 0;
    for file in files {
        let (a, b) = match (load(expected, &file)?, load(actual, &file)?) {
            (None, None) => continue,
            (Some(_), None) => {
                println!("{file}: missing in {actual}");
                differences += 1;
                continue;
            }
            (None, Some(_)) => {
                println!("{file}: missing in {expected}");
                differences += 1;
                continue;
            }
            (Some(a), Some(b)) => (a, b),
        };
        println!("== {file}");
        match (&a, &b) {
            // rpcs.json is one table keyed by hash; the others are objects of tables.
            (Value::Object(x), Value::Object(y)) if file != "rpcs.json" => {
                let name = |k: &str| format!("{}.{k}", file.trim_end_matches(".json"));
                for (k, v) in x {
                    let name = name(k);
                    if only.is_some_and(|o| !name.contains(o)) {
                        continue;
                    }
                    differences += match (v, y.get(k)) {
                        (Value::Array(x), Some(Value::Array(w))) if file.starts_with("scenes/") && (k == "waterVolumes" || k == "oreVeins") => unordered(&name, x, w, show),
                        (_, Some(w)) => section(&name, v, w, show),
                        (_, None) => {
                            println!("{name}: missing");
                            1
                        }
                    };
                }
                for k in y.keys().filter(|k| !x.contains_key(*k)) {
                    let name = name(k);
                    if only.is_none_or(|o| name.contains(o)) {
                        println!("{name}: extra in actual");
                        differences += 1;
                    }
                }
            }
            _ => differences += section(&file, &a, &b, show),
        }
    }
    Ok(differences)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn integers_compare_exactly_and_floats_within_tolerance() {
        let d = |a: Value, b: Value| walk("", &a, &b, &mut Vec::new(), 0);
        assert_eq!(d(json!(100000000), json!(100000001)), 1);
        assert_eq!(d(json!(u64::MAX), json!(u64::MAX - 1)), 1);
        assert_eq!(d(json!(1.0), json!(1.000001)), 0);
        assert_eq!(d(json!({"a": 1}), json!({"a": 1, "b": 2})), 1);
        assert_eq!(short(&json!("0²".repeat(100))).chars().count(), 120);
    }

    #[test]
    fn a_file_in_one_folder_only_or_a_text_difference_counts() {
        let root = std::env::temp_dir().join(format!("pcds-diff-test-{}", std::process::id()));
        let (a, b) = (root.join("a"), root.join("b"));
        for d in [&a, &b] {
            std::fs::create_dir_all(d.join("scenes")).unwrap();
            std::fs::write(d.join("netdata.json"), r#"{"tickRate": 30}"#).unwrap();
        }
        let (a, b) = (a.to_str().unwrap(), b.to_str().unwrap());
        assert_eq!(diff(a, b, None, 8), Ok(0));
        std::fs::write(format!("{b}/buildid"), "abc").unwrap();
        std::fs::write(format!("{b}/scenes/Prime.json"), "{}").unwrap();
        assert_eq!(diff(a, b, None, 8), Ok(2));
        std::fs::write(format!("{a}/buildid"), "abd").unwrap();
        std::fs::write(format!("{a}/scenes/Prime.json"), "{}").unwrap();
        assert_eq!(diff(a, b, None, 8), Ok(1));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
