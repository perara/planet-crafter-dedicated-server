//! A plugin's state: opaque bytes the plugin writes (`pcds_save`) and reads back (`pcds_load`),
//! with the version it wrote them with. The host never looks inside. They travel three ways:
//!
//! - in the hot-update snapshot (`Saved`, the `plugins` part of `ext`), so the next bank's plugin
//!   goes on where this one stopped;
//! - on disk beside the plugin (`<plugin>/data/<world>.state`), at every world save, so a restart
//!   or a crash keeps it too;
//! - in memory (`Loaded::checkpoint`), the last bytes the plugin gave, to restart a plugin that
//!   crashed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const MAGIC: &[u8; 8] = b"PCDSPLG1";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blob {
    pub version: u32,
    pub bytes: Vec<u8>,
}

/// What the snapshot of a flip carries for plugins. Additive: an older bank reading a snapshot
/// that has it ignores it (the field is unknown to it), and a bank reading an older snapshot gets
/// the default (no part named `plugins`, or one without `states` or `stream`). Docs/plugins.md,
/// "Plugin state in a flip".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Saved {
    /// Per plugin id: its state (hex in the JSON, so the snapshot stays text and small).
    #[serde(default, with = "hex_blobs")]
    pub states: BTreeMap<String, Blob>,
    /// The events kept for the out-of-process stream (POST /api/plugins/events): ids go on across a flip.
    #[serde(default)]
    pub stream: super::stream::Ring,
}

mod hex_blobs {
    use super::Blob;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    #[derive(Serialize, Deserialize)]
    struct Wire {
        version: u32,
        hex: String,
    }

    pub fn serialize<S: Serializer>(m: &BTreeMap<String, Blob>, s: S) -> Result<S::Ok, S::Error> {
        s.collect_map(m.iter().map(|(k, b)| (k, Wire { version: b.version, hex: super::hex(&b.bytes) })))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<String, Blob>, D::Error> {
        let wire: BTreeMap<String, Wire> = Deserialize::deserialize(d)?;
        wire.into_iter().map(|(k, w)| super::unhex(&w.hex).map(|bytes| (k, Blob { version: w.version, bytes })).map_err(serde::de::Error::custom)).collect()
    }
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

pub fn unhex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err("state: an odd number of hex digits".into());
    }
    (0..text.len() / 2).map(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).map_err(|_| "state: not hex".to_string())).collect()
}

/// Where a plugin's state for a world lives: the plugin's own folder, so removing the plugin
/// removes it.
pub fn path(plugin_dir: &Path, world: &str) -> PathBuf {
    plugin_dir.join("data").join(format!("{world}.state"))
}

pub fn read(file: &Path) -> Result<Option<Blob>, String> {
    let bytes = match std::fs::read(file) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", file.display())),
    };
    if bytes.len() < 12 || &bytes[..8] != MAGIC {
        return Err(format!("{}: not a plugin state file", file.display()));
    }
    Ok(Some(Blob { version: u32::from_le_bytes(bytes[8..12].try_into().expect("4 bytes")), bytes: bytes[12..].to_vec() }))
}

pub fn write(file: &Path, blob: &Blob) -> Result<(), String> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let mut out = Vec::with_capacity(12 + blob.bytes.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&blob.version.to_le_bytes());
    out.extend_from_slice(&blob.bytes);
    crate::save::write_atomic(&file.to_string_lossy(), &out).map_err(|e| format!("{}: {e}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_and_refuses_garbage() {
        assert_eq!(unhex(&hex(&[0, 1, 254, 255])).unwrap(), vec![0, 1, 254, 255]);
        assert!(unhex("abc").is_err() && unhex("zz").is_err());
    }

    #[test]
    fn saved_state_is_text_and_older_documents_still_read() {
        let mut saved = Saved::default();
        saved.states.insert("hello".into(), Blob { version: 2, bytes: vec![9, 8, 7] });
        let text = serde_json::to_string(&saved).unwrap();
        assert!(text.contains("\"hex\":\"090807\""), "{text}");
        let back: Saved = serde_json::from_str(&text).unwrap();
        assert_eq!(back.states["hello"], Blob { version: 2, bytes: vec![9, 8, 7] });
        // A document from before plugins, or with fields a later build adds, reads.
        assert!(serde_json::from_str::<Saved>("{}").unwrap().states.is_empty());
        assert!(serde_json::from_str::<Saved>("{\"states\":{},\"future\":1}").unwrap().states.is_empty());
    }

    #[test]
    fn a_state_file_keeps_its_version_and_refuses_other_files() {
        let dir = std::env::temp_dir().join(format!("pcds-state-{}", std::process::id()));
        let file = path(&dir, "world");
        assert_eq!(read(&file).unwrap(), None);
        let blob = Blob { version: 7, bytes: b"counter=3".to_vec() };
        write(&file, &blob).unwrap();
        assert_eq!(read(&file).unwrap(), Some(blob));
        std::fs::write(&file, b"nonsense, longer than a header").unwrap();
        assert!(read(&file).unwrap_err().contains("not a plugin state file"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
