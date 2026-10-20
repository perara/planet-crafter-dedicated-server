//! Sharing plugins: the `.pcdsplugin` package and the index file a community may host
//! (docs/plugins.md, "Sharing plugins").
//!
//! A package is one plain (POSIX ustar) tar file, not compressed: the manifest, the entry file, the
//! signature, the README, the licence and the scenarios of a plugin, nothing else, in sorted order with
//! no owners and no times, so the same plugin always packs to the same bytes and its sha256 can be
//! published. It is read here, by this program, never by `tar`: only regular files with the names a
//! plugin folder has are accepted, whatever the archive claims.
//!
//! An index (`plugins-index.json`) is a list of packages with their sha256. It is a convenience for
//! finding a package, not a trust anchor: the sha256 it carries only says that the package is the one
//! the index's author listed. What the package may do is decided by the admin at approval, as for any plugin.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::manifest;

pub const EXTENSION: &str = "pcdsplugin";
/// Files in a package, and the most one may hold, and all of them together.
pub const MAX_FILES: usize = 64;
pub const MAX_FILE_BYTES: usize = super::limits::MAX_MODULE_BYTES;
pub const MAX_PACKAGE_BYTES: usize = 12 << 20;
/// What an index may hold.
pub const MAX_INDEX_BYTES: usize = 4 << 20;
pub const MAX_INDEX_ENTRIES: usize = 2000;

/// A name inside a package: relative, flat or one folder deep (`scenarios/x.toml`), plain characters.
pub fn name_ok(name: &str) -> bool {
    let parts: Vec<&str> = name.split('/').collect();
    parts.len() <= 2
        && parts.iter().all(|p| !p.is_empty() && p.len() <= 96 && *p != "." && *p != ".." && !p.starts_with('.') && p.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)))
        && (parts.len() == 1 || parts[0] == "scenarios")
}

/// The files of a plugin folder that a package carries: the manifest, the entry, the signature, the
/// README, the licence files and `scenarios/*.toml`.
pub fn files_of(folder: &Path, entry: &str) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut names = vec![manifest::FILE.to_string(), entry.to_string(), super::sign::FILE.to_string()];
    for e in std::fs::read_dir(folder).map_err(|e| format!("{}: {e}", folder.display()))?.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        let upper = n.to_ascii_uppercase();
        if n == "README.md" || upper.starts_with("LICENSE") || upper.starts_with("NOTICE") {
            names.push(n);
        }
    }
    for e in std::fs::read_dir(folder.join("scenarios")).into_iter().flatten().flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if n.ends_with(".toml") {
            names.push(format!("scenarios/{n}"));
        }
    }
    names.sort();
    names.dedup();
    let mut files = Vec::new();
    for name in names {
        let path = folder.join(&name);
        match super::read_regular(&path, MAX_FILE_BYTES) {
            Ok(bytes) => {
                if !name_ok(&name) && name != entry {
                    return Err(format!("{name}: not a name a package can carry (letters, digits, . _ -, one folder deep)"));
                }
                files.push((name, bytes));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && (name == super::sign::FILE || name.starts_with("README") || name.to_ascii_uppercase().starts_with("LICENSE")) => {}
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    if !files.iter().any(|(n, _)| n == entry) {
        return Err(format!("{entry}: not there"));
    }
    Ok(files)
}

/// The tar file of these files (sorted by name as given).
pub fn tar(files: &[(String, Vec<u8>)]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for (name, bytes) in files {
        if name.len() > 99 {
            return Err(format!("{name}: a name over 99 bytes"));
        }
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..108].copy_from_slice(b"0000644\0");
        h[108..116].copy_from_slice(b"0000000\0");
        h[116..124].copy_from_slice(b"0000000\0");
        h[124..136].copy_from_slice(format!("{:011o}\0", bytes.len()).as_bytes());
        h[136..148].copy_from_slice(b"00000000000\0");
        h[148..156].copy_from_slice(b"        ");
        h[156] = b'0';
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        let sum: u32 = h.iter().map(|b| *b as u32).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        out.extend_from_slice(&h);
        out.extend_from_slice(bytes);
        out.resize(out.len().div_ceil(512) * 512, 0);
    }
    out.resize(out.len() + 1024, 0);
    if out.len() > MAX_PACKAGE_BYTES {
        return Err(format!("the package is {} bytes: at most {MAX_PACKAGE_BYTES}", out.len()));
    }
    Ok(out)
}

fn octal(field: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(field).ok()?.trim_matches(|c: char| c == '\0' || c == ' ');
    if text.is_empty() { Some(0) } else { u64::from_str_radix(text, 8).ok() }
}

/// The files of a package, or why it is none. Nothing but regular files is accepted: a link, a
/// folder entry with a payload, a name with `..` or a leading `/`, a file twice, too many or too large.
pub fn untar(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    if bytes.len() > MAX_PACKAGE_BYTES {
        return Err(format!("{} bytes: a package is at most {MAX_PACKAGE_BYTES}", bytes.len()));
    }
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut at = 0;
    loop {
        let Some(h) = bytes.get(at..at + 512) else { return Err("the package ends in the middle of a header (it is cut short, or is no tar file)".into()) };
        if h.iter().all(|b| *b == 0) {
            break;
        }
        let name_end = h[..100].iter().position(|b| *b == 0).unwrap_or(100);
        let name = std::str::from_utf8(&h[..name_end]).map_err(|_| "a name that is not UTF-8".to_string())?;
        let size = octal(&h[124..136]).ok_or("a size that is no number")? as usize;
        let sum_stored = octal(&h[148..156]).ok_or("a checksum that is no number")?;
        let sum: u64 = h.iter().enumerate().map(|(i, b)| if (148..156).contains(&i) { 32 } else { *b as u64 }).sum();
        if sum != sum_stored {
            return Err(format!("{name}: the header's checksum is wrong (the package is damaged, or is no tar file)"));
        }
        if !matches!(h[156], b'0' | 0) {
            return Err(format!("{name}: only regular files are accepted in a package (this is a link, a folder or something else)"));
        }
        if !name_ok(name) {
            return Err(format!("{name:?}: not a name a package may carry (letters, digits, . _ -, at most one folder deep, `scenarios/`)"));
        }
        if size > MAX_FILE_BYTES {
            return Err(format!("{name}: {size} bytes: at most {MAX_FILE_BYTES}"));
        }
        let body = bytes.get(at + 512..at + 512 + size).ok_or_else(|| format!("{name}: the package ends before the file does"))?;
        if files.iter().any(|(n, _)| n == name) {
            return Err(format!("{name} twice"));
        }
        files.push((name.to_string(), body.to_vec()));
        if files.len() > MAX_FILES {
            return Err(format!("more than {MAX_FILES} files"));
        }
        at += 512 + size.div_ceil(512) * 512;
    }
    if !files.iter().any(|(n, _)| n == manifest::FILE) {
        return Err(format!("no {} in the package", manifest::FILE));
    }
    Ok(files)
}

/// sha256 of bytes, as hex.
pub fn sha256(bytes: &[u8]) -> String {
    crate::util::to_hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

/// `plugin-VERSION.pcdsplugin`.
pub fn file_name(id: &str, version: &str) -> String {
    format!("{id}-{version}.{EXTENSION}")
}

// ---- the index -----------------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Index {
    /// 1.
    pub format: u32,
    #[serde(default)]
    pub name: String,
    pub plugins: Vec<Entry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub license: String,
    /// The package: an https URL, or a path relative to the index's own URL.
    pub url: String,
    /// sha256 of the package, hex. The download must match it.
    pub sha256: String,
    /// The id of the key the plugin is signed with, if it is (informational: the signature in the package decides).
    pub signer: Option<String>,
    /// The capabilities the plugin asks for (informational: the manifest in the package decides, and a package that
    /// asks for more than the index says is refused).
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub homepage: Option<String>,
}

impl Index {
    pub fn parse(bytes: &[u8]) -> Result<Index, String> {
        if bytes.len() > MAX_INDEX_BYTES {
            return Err(format!("the index is {} bytes: at most {MAX_INDEX_BYTES}", bytes.len()));
        }
        let text = std::str::from_utf8(bytes).map_err(|_| "the index is not UTF-8".to_string())?;
        let index: Index = serde_json::from_str(text).map_err(|e| format!("the index does not read: {e}"))?;
        if index.format != 1 {
            return Err(format!("the index is format {}; this program reads format 1", index.format));
        }
        if index.plugins.len() > MAX_INDEX_ENTRIES {
            return Err(format!("{} entries: at most {MAX_INDEX_ENTRIES}", index.plugins.len()));
        }
        for e in &index.plugins {
            let at = |what: &str| format!("index entry {}: {what}", super::clean(&e.id));
            if !manifest::valid_id(&e.id) {
                return Err(at("the id is no plugin id"));
            }
            if manifest::version_triple(&e.version).is_none() {
                return Err(at("the version is not three numbers"));
            }
            if e.sha256.len() != 64 || !e.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(at("the sha256 is 64 hex digits"));
            }
            for (what, text) in [("name", &e.name), ("description", &e.description), ("author", &e.author), ("license", &e.license)] {
                if text.chars().count() > 500 || super::clean(text) != *text {
                    return Err(at(&format!("{what}: one line of plain text, 500 characters at most")));
                }
            }
            if e.url.is_empty() || e.url.len() > 2048 || e.url.bytes().any(|b| b < b' ' || b == 0x7f) {
                return Err(at("the url is empty or not one printable line"));
            }
        }
        Ok(index)
    }

    /// The entries whose id, name or description contain `term` (all of them for none), newest version of an id last.
    pub fn search(&self, term: &str) -> Vec<&Entry> {
        let t = term.to_lowercase();
        let mut found: Vec<&Entry> = self.plugins.iter().filter(|e| t.is_empty() || e.id.contains(&t) || e.name.to_lowercase().contains(&t) || e.description.to_lowercase().contains(&t)).collect();
        found.sort_by(|a, b| a.id.cmp(&b.id).then(manifest::version_triple(&a.version).cmp(&manifest::version_triple(&b.version))));
        found
    }

    /// The newest entry of this id.
    pub fn find(&self, id: &str) -> Option<&Entry> {
        self.plugins.iter().filter(|e| e.id == id).max_by_key(|e| manifest::version_triple(&e.version))
    }
}

/// Where an entry's package is: its url, made absolute against the index's (an index read from a file
/// has only absolute urls).
pub fn resolve(index_location: &str, url: &str) -> Result<String, String> {
    if url.starts_with("https://") || url.starts_with("http://127.0.0.1") {
        return Ok(url.to_string());
    }
    if url.contains("://") {
        return Err(format!("{}: a package comes over https", crate::util::shown_url(url)));
    }
    let base = index_location.rsplit_once('/').map(|(b, _)| b).filter(|_| index_location.contains("://")).ok_or_else(|| format!("the url {url:?} is relative, and the index is a file: its package urls must be https addresses"))?;
    Ok(format!("{base}/{}", url.trim_start_matches("./")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<(String, Vec<u8>)> {
        vec![("README.md".into(), b"hello".to_vec()), ("plugin.toml".into(), vec![b'x'; 700]), ("plugin.wasm".into(), vec![0, 1, 2]), ("scenarios/a.toml".into(), b"name = \"a\"".to_vec())]
    }

    #[test]
    fn a_package_round_trips_and_is_the_same_bytes_every_time() {
        let files = sample();
        let a = tar(&files).unwrap();
        assert_eq!(a, tar(&files).unwrap());
        assert_eq!(untar(&a).unwrap(), files);
        assert_eq!(sha256(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        // The system's tar reads it too (and lists the same names).
        let path = std::env::temp_dir().join(format!("pcds-pack-{}.tar", std::process::id()));
        std::fs::write(&path, &a).unwrap();
        if let Ok(out) = std::process::Command::new("tar").arg("-tf").arg(&path).output() {
            let listed = String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect::<Vec<_>>();
            assert_eq!(listed, vec!["README.md", "plugin.toml", "plugin.wasm", "scenarios/a.toml"]);
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_hostile_package_is_refused() {
        let ok = tar(&sample()).unwrap();
        // Names: parents, absolute, hidden, deep, odd characters, not on the list of folders.
        for bad in ["../x", "/etc/passwd", ".hidden", "a/b/c", "a b", "other/x.toml", "a//b", ""] {
            let forged = tar(&[("plugin.toml".into(), vec![1]), ("x".into(), vec![2])]).unwrap();
            let mut f = forged.clone();
            let name = bad.as_bytes();
            f[512 + 512..512 + 512 + name.len().min(99)].copy_from_slice(&name[..name.len().min(99)]);
            if name.is_empty() {
                f[1024] = 0;
            }
            // Fix the checksum of the forged header.
            let h = &mut f[1024..1536];
            h[148..156].copy_from_slice(b"        ");
            let sum: u32 = h.iter().map(|b| *b as u32).sum();
            h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
            assert!(untar(&f).is_err(), "{bad:?}");
        }
        // A link, a damaged header, a cut file, a file twice, no manifest, too large, no tar at all.
        let mut link = ok.clone();
        link[156] = b'2';
        assert!(untar(&link).unwrap_err().contains("checksum") || untar(&link).unwrap_err().contains("regular"));
        let mut damaged = ok.clone();
        damaged[10] ^= 1;
        assert!(untar(&damaged).unwrap_err().contains("checksum"));
        assert!(untar(&ok[..700]).is_err());
        assert!(untar(&tar(&[("plugin.toml".into(), vec![1]), ("plugin.toml".into(), vec![2])]).unwrap()).unwrap_err().contains("twice"));
        assert!(untar(&tar(&[("plugin.wasm".into(), vec![1])]).unwrap()).unwrap_err().contains("no plugin.toml"));
        assert!(untar(b"not a tar file at all, but not 512 bytes either").is_err());
        assert!(untar(&vec![1u8; 600]).is_err());
        assert!(untar(&vec![0u8; MAX_PACKAGE_BYTES + 1]).unwrap_err().contains("at most"));
        let many: Vec<(String, Vec<u8>)> = (0..MAX_FILES + 1).map(|i| (format!("f{i}"), vec![])).chain([("plugin.toml".to_string(), vec![])]).collect();
        assert!(untar(&tar(&many).unwrap()).unwrap_err().contains("more than"));
    }

    #[test]
    fn an_index_is_read_strictly_and_searched() {
        let text = |extra: &str| format!(r#"{{"format": 1, "name": "Community", "plugins": [{{"id": "afk-kick", "name": "AFK kick", "version": "1.2.0", "description": "Kicks players who stand still", "url": "afk-kick-1.2.0.pcdsplugin", "sha256": "{}"{extra}}}, {{"id": "afk-kick", "name": "AFK kick", "version": "1.10.0", "url": "https://example.org/a.pcdsplugin", "sha256": "{}"}}]}}"#, "a".repeat(64), "b".repeat(64));
        let index = Index::parse(text("").as_bytes()).unwrap();
        assert_eq!(index.find("afk-kick").unwrap().version, "1.10.0", "versions compare as numbers");
        assert_eq!(index.search("stand").len(), 1);
        assert_eq!(index.search("").len(), 2);
        assert!(index.search("nothing").is_empty());
        for (bad, why) in [(r#", "surprise": 1"#, "unknown field"), (r#", "capabilities": 3"#, "invalid type")] {
            assert!(Index::parse(text(bad).as_bytes()).unwrap_err().contains(why), "{bad}");
        }
        assert!(Index::parse(text("").replace("\"format\": 1", "\"format\": 2").as_bytes()).unwrap_err().contains("format 2"));
        assert!(Index::parse(text("").replace(&"a".repeat(64), "abc").as_bytes()).unwrap_err().contains("sha256"));
        assert!(Index::parse(text("").replace("afk-kick\", \"name", "Bad Id\", \"name").as_bytes()).unwrap_err().contains("plugin id"));
        assert_eq!(resolve("https://example.org/index/plugins-index.json", "a.pcdsplugin").unwrap(), "https://example.org/index/a.pcdsplugin");
        assert!(resolve("/tmp/plugins-index.json", "a.pcdsplugin").unwrap_err().contains("file"));
        assert!(resolve("https://example.org/i.json", "http://evil.example/a").unwrap_err().contains("https"));
        assert_eq!(resolve("/tmp/i.json", "https://example.org/a").unwrap(), "https://example.org/a");
    }
}
