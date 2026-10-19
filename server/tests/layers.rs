//! The module layering of the server, checked from the source (docs/architecture.md): every module
//! is in one layer (layers.txt), a module may only use modules of its own layer and the layers
//! below it, and what breaks that today is a named debt. A new `use crate::x` that crosses a
//! layer the wrong way fails here; a debt that is paid (the edge is gone) fails too, so the list
//! only ever gets shorter. `scripts/check-layers` runs just this test.
//!
//! Code of `#[cfg(test)]` items is not looked at (tests may use anything), nor comments or strings,
//! except a string that is a path into the crate (`#[serde(with = "crate::handoff::instant")]`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
const LAYERS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/layers.txt");

/// The source with comments, strings (but crate paths) and `#[cfg(test)]` items blanked out.
fn blank(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = vec![b' '; b.len()];
    let mut i = 0;
    let keep = |out: &mut Vec<u8>, from: usize, to: usize| out[from..to].copy_from_slice(&b[from..to]);
    while i < b.len() {
        let c = b[i];
        if c == b'\n' {
            out[i] = b'\n';
            i += 1;
        } else if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let mut depth = 0;
            while i < b.len() {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    if b[i] == b'\n' {
                        out[i] = b'\n';
                    }
                    i += 1;
                }
            }
        } else if c == b'r' && matches!(b.get(i + 1), Some(b'"') | Some(b'#')) && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')) {
            // A raw string r"..." or r#"..."#.
            let mut j = i + 1;
            let mut hashes = 0;
            while b.get(j) == Some(&b'#') {
                hashes += 1;
                j += 1;
            }
            if b.get(j) != Some(&b'"') {
                out[i] = c;
                i += 1;
                continue;
            }
            j += 1;
            let close: Vec<u8> = std::iter::once(b'"').chain(std::iter::repeat_n(b'#', hashes)).collect();
            while j < b.len() && !b[j..].starts_with(&close) {
                if b[j] == b'\n' {
                    out[j] = b'\n';
                }
                j += 1;
            }
            i = (j + close.len()).min(b.len());
        } else if c == b'"' {
            let start = i + 1;
            i += 1;
            while i < b.len() && b[i] != b'"' {
                i += if b[i] == b'\\' { 2 } else { 1 };
                if i <= b.len() && b.get(i - 1) == Some(&b'\n') {
                    out[i - 1] = b'\n';
                }
            }
            // A path into the crate that serde reads: #[serde(with = "crate::handoff::instant")].
            if b[start..i.min(b.len())].starts_with(b"crate::") && b[..start].ends_with(b"with = \"") {
                keep(&mut out, start, i.min(b.len()));
            }
            i += 1;
        } else if c == b'\'' {
            // A char literal ('x', '\n', '{'), else a lifetime.
            if b.get(i + 1) == Some(&b'\\') {
                i += 2;
                while i < b.len() && b[i] != b'\'' {
                    i += 1;
                }
                i += 1;
            } else if b.get(i + 2) == Some(&b'\'') {
                i += 3;
            } else {
                out[i] = c;
                i += 1;
            }
        } else {
            out[i] = c;
            i += 1;
        }
    }
    let text = String::from_utf8_lossy(&out).into_owned();
    drop_cfg_test(&text)
}

/// Removes every item that follows `#[cfg(test)]`: to its closing brace, or to its `;`.
fn drop_cfg_test(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = text.as_bytes().to_vec();
    let mut from = 0;
    while let Some(at) = text[from..].find("#[cfg(test)]") {
        let start = from + at;
        let mut i = start + "#[cfg(test)]".len();
        // More attributes may sit between it and the item.
        loop {
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if b[i..].starts_with(b"#[") {
                let mut depth = 0;
                while i < b.len() {
                    match b[i] {
                        b'[' => depth += 1,
                        b']' => {
                            depth -= 1;
                            if depth == 0 {
                                i += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
            } else {
                break;
            }
        }
        let mut depth = 0;
        while i < b.len() {
            match b[i] {
                b';' if depth == 0 => {
                    i += 1;
                    break;
                }
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        for byte in &mut out[start..i.min(b.len())] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
        from = i.min(b.len());
        if from >= b.len() {
            break;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The first name of each path in `crate::...`, including `crate::{a, b::c}`.
fn crate_uses(code: &str) -> BTreeSet<String> {
    let b = code.as_bytes();
    let ident = |mut i: usize| {
        let s = i;
        while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
            i += 1;
        }
        (&code[s..i], i)
    };
    let mut found = BTreeSet::new();
    let mut from = 0;
    while let Some(at) = code[from..].find("crate::") {
        let start = from + at;
        from = start + 7;
        if start > 0 && (b[start - 1].is_ascii_alphanumeric() || b[start - 1] == b'_' || b[start - 1] == b':') {
            continue;
        }
        let mut i = from;
        if b.get(i) == Some(&b'{') {
            // The top-level names of a use list.
            let mut depth = 0;
            let mut expect_name = true;
            while i < b.len() {
                match b[i] {
                    b'{' => {
                        depth += 1;
                        expect_name = depth == 1;
                    }
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    b',' => expect_name = depth == 1,
                    c if expect_name && depth == 1 && (c.is_ascii_alphabetic() || c == b'_') => {
                        let (name, end) = ident(i);
                        found.insert(name.to_string());
                        expect_name = false;
                        i = end;
                        continue;
                    }
                    _ => {}
                }
                i += 1;
            }
        } else {
            let (name, _) = ident(i);
            if !name.is_empty() {
                found.insert(name.to_string());
            }
        }
    }
    found
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

/// The modules named in main.rs, and which of them exist only for tests.
fn modules() -> (BTreeSet<String>, BTreeSet<String>) {
    let main = std::fs::read_to_string(format!("{SRC}/main.rs")).unwrap();
    let (mut all, mut test_only) = (BTreeSet::new(), BTreeSet::new());
    let mut prev_cfg_test = false;
    for line in main.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix("mod ").or_else(|| line.strip_prefix("pub mod ")).and_then(|r| r.strip_suffix(';')) {
            all.insert(name.to_string());
            if prev_cfg_test {
                test_only.insert(name.to_string());
            }
        }
        prev_cfg_test = line == "#[cfg(test)]";
    }
    (all, test_only)
}

/// The file names (without `.rs`) of the submodules `top` declares as `#[cfg(test)] mod x;`.
fn test_submodules(top: &str) -> BTreeSet<String> {
    let text = std::fs::read_to_string(format!("{SRC}/{top}.rs")).unwrap_or_default();
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    lines.windows(2).filter(|w| w[0] == "#[cfg(test)]").filter_map(|w| w[1].strip_prefix("mod ").and_then(|r| r.strip_suffix(';'))).map(str::to_string).collect()
}

/// The edges between top-level modules that non-test code has: module -> modules it uses.
fn edges(modules: &BTreeSet<String>) -> BTreeMap<String, BTreeSet<String>> {
    let mut files = Vec::new();
    rs_files(Path::new(SRC), &mut files);
    let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for f in files {
        let rel = f.strip_prefix(SRC).unwrap().to_string_lossy().to_string();
        let top = rel.split('/').next().unwrap().trim_end_matches(".rs").to_string();
        if !modules.contains(&top) || test_submodules(&top).contains(f.file_stem().unwrap().to_str().unwrap()) {
            continue;
        }
        let used = crate_uses(&blank(&std::fs::read_to_string(&f).unwrap()));
        let e = edges.entry(top.clone()).or_default();
        e.extend(used.into_iter().filter(|u| u != &top && modules.contains(u)));
    }
    edges
}

struct Rules {
    layer_of: BTreeMap<String, String>,
    may: BTreeMap<String, BTreeSet<String>>,
    only: Vec<(String, BTreeSet<String>)>,
    debts: BTreeSet<(String, String)>,
}

fn rules() -> Rules {
    let text = std::fs::read_to_string(LAYERS).unwrap();
    let mut r = Rules { layer_of: BTreeMap::new(), may: BTreeMap::new(), only: Vec::new(), debts: BTreeSet::new() };
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap().trim();
        if line.is_empty() {
            continue;
        }
        let bad = || -> ! { panic!("layers.txt line {}: {line:?}", n + 1) };
        let (word, rest) = line.split_once(' ').unwrap_or_else(|| bad());
        match word {
            "layer" => {
                let (name, members) = rest.split_once(':').unwrap_or_else(|| bad());
                for m in members.split_whitespace() {
                    assert!(r.layer_of.insert(m.to_string(), name.trim().to_string()).is_none(), "{m} is in two layers");
                }
            }
            "may" => {
                let (name, below) = rest.split_once("->").unwrap_or_else(|| bad());
                r.may.insert(name.trim().to_string(), below.split_whitespace().map(str::to_string).collect());
            }
            "only" => {
                let (name, who) = rest.split_once("<-").unwrap_or_else(|| bad());
                r.only.push((name.trim().to_string(), who.split_whitespace().map(str::to_string).collect()));
            }
            "debt" => {
                let (a, b) = rest.split_once("->").unwrap_or_else(|| bad());
                r.debts.insert((a.trim().to_string(), b.trim().to_string()));
            }
            _ => bad(),
        }
    }
    r
}

/// What is wrong with these edges under these rules (empty: nothing).
fn problems(modules: &BTreeSet<String>, test_only: &BTreeSet<String>, edges: &BTreeMap<String, BTreeSet<String>>, r: &Rules) -> Vec<String> {
    let mut problems = Vec::new();
    for m in modules.iter().filter(|m| !test_only.contains(*m)) {
        if !r.layer_of.contains_key(m) {
            problems.push(format!("module {m} is in no layer: add it to layers.txt (docs/architecture.md says where code goes)"));
        }
    }
    for m in r.layer_of.keys() {
        if !modules.contains(m) {
            problems.push(format!("layers.txt names {m}, which is no module (any more): remove it"));
        }
    }
    let mut used_debts = BTreeSet::new();
    for (from, tos) in edges {
        if test_only.contains(from) {
            continue;
        }
        let Some(layer) = r.layer_of.get(from) else { continue };
        let allowed = &r.may[layer];
        for to in tos.iter().filter(|t| !test_only.contains(*t)) {
            let Some(to_layer) = r.layer_of.get(to) else { continue };
            let mut broken = (!allowed.contains(to_layer)).then(|| format!("{from} ({layer}) uses {to} ({to_layer}): a {layer} module may use {} only", allowed.iter().cloned().collect::<Vec<_>>().join(", ")));
            if let Some((_, who)) = r.only.iter().find(|(m, _)| m == to) {
                if !who.contains(from) {
                    broken = Some(format!("{from} uses {to}, which only {} may use (docs/architecture.md)", who.iter().cloned().collect::<Vec<_>>().join(", ")));
                }
            }
            if let Some(why) = broken {
                if r.debts.contains(&(from.clone(), to.clone())) {
                    used_debts.insert((from.clone(), to.clone()));
                } else {
                    problems.push(format!("{why}. Move the code, or if it cannot move yet, add `debt {from} -> {to}` to layers.txt with a reason."));
                }
            }
        }
    }
    for (a, b) in r.debts.difference(&used_debts) {
        problems.push(format!("the debt {a} -> {b} in layers.txt is paid (or never was one): remove the line"));
    }
    problems
}

#[test]
fn every_module_is_in_a_layer_and_uses_only_what_its_layer_may() {
    let (modules, test_only) = modules();
    let found = problems(&modules, &test_only, &edges(&modules), &rules());
    assert!(found.is_empty(), "\n{}\n", found.join("\n"));
}

#[test]
fn a_forbidden_edge_a_stale_debt_and_an_unplaced_module_each_fail() {
    let set = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
    let mut r = Rules { layer_of: BTreeMap::new(), may: BTreeMap::new(), only: vec![("host".into(), set(&["bank"]))], debts: BTreeSet::new() };
    for (m, l) in [("wire", "base"), ("game", "core"), ("host", "core"), ("bank", "shell"), ("front", "shell")] {
        r.layer_of.insert(m.into(), l.into());
    }
    r.may.insert("base".into(), set(&["base"]));
    r.may.insert("core".into(), set(&["base", "core"]));
    r.may.insert("shell".into(), set(&["base", "core", "shell"]));
    let modules = set(&["wire", "game", "host", "bank", "front", "new"]);
    let edge = |a: &str, b: &str| (a.to_string(), set(&[b]));
    let ok: BTreeMap<_, _> = [edge("game", "wire"), edge("bank", "game"), edge("bank", "host")].into();
    assert_eq!(problems(&modules, &BTreeSet::new(), &ok, &r), vec!["module new is in no layer: add it to layers.txt (docs/architecture.md says where code goes)"]);
    let modules = set(&["wire", "game", "host", "bank", "front"]);
    assert!(problems(&modules, &BTreeSet::new(), &ok, &r).is_empty());
    // The game uses the bank: a core module using a shell one.
    let bad: BTreeMap<_, _> = [edge("game", "bank")].into();
    let found = problems(&modules, &BTreeSet::new(), &bad, &r);
    assert!(found.len() == 1 && found[0].contains("game (core) uses bank (shell)") && found[0].contains("add `debt game -> bank`"), "{found:?}");
    // A module that is not the bank uses the host.
    let found = problems(&modules, &BTreeSet::new(), &[edge("front", "host")].into(), &r);
    assert!(found.iter().any(|f| f.contains("front uses host, which only bank may use")), "{found:?}");
    // The debt, once named, lets the edge be; once the edge is gone the debt must go.
    r.debts.insert(("game".into(), "bank".into()));
    assert!(problems(&modules, &BTreeSet::new(), &bad, &r).is_empty());
    assert_eq!(problems(&modules, &BTreeSet::new(), &ok, &r), vec!["the debt game -> bank in layers.txt is paid (or never was one): remove the line"]);
}

#[test]
fn the_scanner_sees_crate_paths_and_skips_tests_comments_and_strings() {
    let code = blank(
        r##"
use crate::{access, chathooks::{x, y}, config as c};
use crate::game::Game; // crate::comment
fn f() { let s = "crate::in_a_string"; let r = r#"crate::raw"#; let c = '{'; crate::units::value(); }
#[serde(with = "crate::handoff::instant")]
struct S;
#[cfg(test)]
mod tests { use crate::only_in_tests::x; fn g() { let s = "}"; } }
#[cfg(test)]
use crate::test_import::Y;
fn after() { crate::after_tests::z(); }
"##,
    );
    let found = crate_uses(&code);
    let want: BTreeSet<String> = ["access", "chathooks", "config", "game", "units", "handoff", "after_tests"].iter().map(|s| s.to_string()).collect();
    assert_eq!(found, want, "{code}");
}

#[test]
fn the_plugin_host_stays_on_the_bank_side_of_the_process_split() {
    // Spelled out as well as in layers.txt, because it is the rule the plugin host depends on.
    let (modules, _) = modules();
    let edges = edges(&modules);
    for shell in ["front", "bank", "cli", "admin", "dashboard", "updater", "utp", "native", "config", "s3"] {
        assert!(!edges["plugins"].contains(shell), "plugins uses {shell}: the host lives in the bank, next to the game, and nowhere else");
    }
    for m in edges.keys().filter(|m| !["events", "bank", "ext", "cli", "plugins"].contains(&m.as_str())) {
        assert!(!edges[m].contains("plugins"), "{m} uses plugins: the game reaches plugins only through events");
    }
}
