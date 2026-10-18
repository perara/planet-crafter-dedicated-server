//! The test-coverage map (coverage.txt) checked against the code, so that "is everything
//! tested" is a test, not a judgement:
//!
//! - every server-bound RPC of rpcs.json is either handled and named with the tests that send
//!   it (`rpc`), or not handled and listed with the reason (`unhandled`); which it is, is found
//!   by dispatching each one, not read from the map;
//! - every source module is named with the tests that check it (`module`), with the #[ignore]d
//!   tests run by hand that do (`manual`), or listed with the reason it has none (`untested`);
//! - every test named exists, and an `rpc` line's tests send that RPC.
//!
//! A new RPC, a new module or a renamed test fails here.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::game::tests::{join, save};
use crate::game::{test_game, Args};

const MAP: &str = include_str!("coverage.txt");
const ROOT: &str = env!("CARGO_MANIFEST_DIR");

struct Test {
    ignored: bool,
    body: String,
}

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(rust_files(&p));
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
    out
}

/// "src/machines/growers.rs" → "machines::growers::", "src/main.rs" → "", and an integration
/// test, "tests/hot_update.rs" → "hot_update::" (its own crate).
fn module_path(file: &Path) -> String {
    let src = Path::new(ROOT).join("src");
    let rel = match file.strip_prefix(&src) {
        Ok(rel) => rel.with_extension(""),
        Err(_) => PathBuf::from(file.file_stem().unwrap()),
    };
    rel.iter().map(|p| p.to_string_lossy().into_owned()).filter(|p| p != "mod" && p != "main").map(|p| format!("{p}::")).collect()
}

/// Every #[test] in src/ and tests/, by its cargo test path, with its source.
fn tests() -> BTreeMap<String, Test> {
    let mut out = BTreeMap::new();
    for file in rust_files(&Path::new(ROOT).join("src")).into_iter().chain(rust_files(&Path::new(ROOT).join("tests"))) {
        let module = module_path(&file);
        let text = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let tests_mod = lines.iter().position(|l| l.trim().ends_with("mod tests {"));
        for (i, l) in lines.iter().enumerate() {
            if l.trim() != "#[test]" {
                continue;
            }
            let mut ignored = false;
            let mut j = i + 1;
            while lines[j].trim().starts_with("#[") || lines[j].trim().starts_with("//") {
                ignored |= lines[j].trim().starts_with("#[ignore");
                j += 1;
            }
            let head = lines[j];
            let name = head.trim().strip_prefix("fn ").and_then(|s| s.split('(').next()).unwrap_or_else(|| panic!("{}:{}: #[test] before {head:?}", file.display(), j + 1));
            let indent = &head[..head.len() - head.trim_start().len()];
            let end = (j..lines.len()).find(|&k| lines[k] == format!("{indent}}}")).unwrap();
            let inner = if tests_mod.is_some_and(|t| t < i) { "tests::" } else { "" };
            out.insert(format!("{module}{inner}{name}"), Test { ignored, body: lines[j..=end].join("\n") });
        }
    }
    out
}

/// Every module of the server, as the map names it ("machines::growers"; "main" for the
/// root): the source files, without a module's own tests file (rockets/tests.rs) and without
/// the modules that are test code only (`#[cfg(test)] mod x;` in main.rs).
fn modules() -> BTreeSet<String> {
    let main = std::fs::read_to_string(Path::new(ROOT).join("src/main.rs")).unwrap();
    let lines: Vec<&str> = main.lines().collect();
    let test_only: BTreeSet<String> = lines.windows(2).filter(|w| w[0].trim() == "#[cfg(test)]").filter_map(|w| w[1].trim().strip_prefix("mod ")?.strip_suffix(';').map(|m| format!("{m}::"))).collect();
    rust_files(&Path::new(ROOT).join("src"))
        .iter()
        .map(|f| module_path(f))
        .filter(|m| !m.ends_with("tests::") && !test_only.iter().any(|t| m.starts_with(t.as_str())))
        .map(|m| if m.is_empty() { "main".into() } else { m.trim_end_matches("::").to_string() })
        .collect()
}

struct Entry {
    line: usize,
    kind: String,
    name: String,
    rest: Vec<String>,
}

fn map() -> Vec<Entry> {
    MAP.lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l.split('#').next().unwrap().trim()))
        .filter(|(_, l)| !l.is_empty())
        .map(|(line, l)| {
            let mut words = l.split_whitespace().map(String::from);
            let kind = words.next().unwrap();
            let name = words.next().unwrap_or_else(|| panic!("coverage.txt:{line}: no name"));
            Entry { line, kind, name, rest: words.collect() }
        })
        .collect()
}

/// The server-bound RPCs of rpcs.json, by name, and whether the server handles each: the
/// animator relay or one of the RPC handlers takes it from a player's object (an RPC with an
/// argument the server cannot decode reaches no handler), or a handler takes it from some
/// objects only (a guarded match arm, `"Class.MethodServerRpc" if <condition> =>`).
fn handled_rpcs() -> Option<BTreeMap<String, bool>> {
    let mut g = test_game(save())?;
    join(&mut g, 1, "Ann");
    join(&mut g, 2, "Bob");
    let player = g.player_object(1).unwrap();
    let src = Path::new(ROOT).join("src");
    let handlers: String = rust_files(&src).iter().filter(|f| !f.starts_with(src.join("native")) && !f.ends_with("coverage.rs")).map(|f| std::fs::read_to_string(f).unwrap()).collect();
    let table: Value = serde_json::from_str(&std::fs::read_to_string(format!("{}/rpcs.json", crate::test_data()?)).unwrap()).unwrap();
    let mut out = BTreeMap::new();
    for r in table.as_object().unwrap().values().filter(|r| r["to"] == "server" || r["to"] == "notauthority") {
        let name = r["name"].as_str().unwrap().to_string();
        let hash = g.net.spec.hash(&name).unwrap();
        // Each argument's default (0, false, empty), as encode writes a missing one.
        let defaults = g.net.spec.encode(&name, &[]).1;
        let handled = g.animator_relay(1, player, 0, hash, &[])
            || match g.net.spec.decode(hash, &defaults) {
                Some(args) => g.rpc(1, player, 0, &name, Args(args)),
                None => false,
            }
            || handlers.contains(&format!("\"{name}\" if "));
        out.insert(name, handled);
    }
    Some(out)
}

#[test]
fn every_server_rpc_and_module_is_in_the_coverage_map() {
    let tests = tests();
    let entries = map();
    let mut errors = Vec::new();
    let mut seen = BTreeMap::new();
    let is_module = |kind: &str| matches!(kind, "module" | "manual" | "untested");
    for e in &entries {
        if let Some(first) = seen.insert((is_module(&e.kind), e.name.clone()), e.line) {
            errors.push(format!("coverage.txt:{}: {} is already on line {first}", e.line, e.name));
        }
        match e.kind.as_str() {
            "rpc" | "module" | "manual" => {
                if e.rest.is_empty() {
                    errors.push(format!("coverage.txt:{}: {} names no test", e.line, e.name));
                }
                for t in &e.rest {
                    match tests.get(t) {
                        None => errors.push(format!("coverage.txt:{}: no test {t}", e.line)),
                        Some(test) if test.ignored != (e.kind == "manual") => errors.push(format!("coverage.txt:{}: {t} is {}#[ignore]d, and {} lines name {}", e.line, if test.ignored { "" } else { "not " }, e.kind, if e.kind == "manual" { "tests run by hand" } else { "tests the suite runs" })),
                        Some(test) if e.kind == "rpc" && !test.body.contains(&format!("\"{}\"", e.name)) => errors.push(format!("coverage.txt:{}: {t} does not send {}", e.line, e.name)),
                        _ => {}
                    }
                }
            }
            "unhandled" | "untested" => {
                if e.rest.is_empty() {
                    errors.push(format!("coverage.txt:{}: {} gives no reason", e.line, e.name));
                }
            }
            k => errors.push(format!("coverage.txt:{}: unknown kind {k}", e.line)),
        }
    }
    let modules = modules();
    let listed: BTreeSet<String> = entries.iter().filter(|e| is_module(&e.kind)).map(|e| e.name.clone()).collect();
    for m in modules.difference(&listed) {
        errors.push(format!("module {m} is not in coverage.txt"));
    }
    for m in listed.difference(&modules) {
        errors.push(format!("coverage.txt: no module {m}"));
    }
    // The RPCs need the game tables (rpcs.json is extracted from each user's own game).
    if let Some(rpcs) = handled_rpcs() {
        let kind: BTreeMap<&str, &str> = entries.iter().filter(|e| e.kind == "rpc" || e.kind == "unhandled").map(|e| (e.name.as_str(), e.kind.as_str())).collect();
        for (name, handled) in &rpcs {
            let is = if *handled { "handled" } else { "not handled" };
            match (kind.get(name.as_str()), handled) {
                (Some(&"rpc"), true) | (Some(&"unhandled"), false) => {}
                (Some(k), _) => errors.push(format!("{name} is {is}, coverage.txt says {k}")),
                (None, _) => {
                    let senders: Vec<&String> = tests.iter().filter(|(_, t)| !t.ignored && t.body.contains(&format!("\"{name}\""))).map(|(p, _)| p).collect();
                    errors.push(format!("{name} ({is}) is not in coverage.txt; tests that send it: {senders:?}"));
                }
            }
        }
        for name in kind.keys().filter(|n| !rpcs.contains_key(**n)) {
            errors.push(format!("coverage.txt: {name} is no server RPC of rpcs.json"));
        }
    }
    assert!(errors.is_empty(), "{} problems:\n{}", errors.len(), errors.join("\n"));
}
