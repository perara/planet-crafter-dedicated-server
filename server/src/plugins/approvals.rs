//! approved.json: what the admin has allowed, kept beside the plugins. A plugin runs only when it
//! is listed here as enabled, with the capabilities granted, and its files still hash to the digest
//! that was approved: a plugin that changes after approval (an update, a swapped file) waits for
//! the admin again. The server only reads this file; `pcds plugin enable` and the admin API write it.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub const FILE: &str = "approved.json";

/// The admin's lowering of a plugin's budget (never above what the manifest asks, or the host's maxima).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Overrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fuel_per_event: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fuel_per_save: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_pages: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tick_every_ms: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Approval {
    pub enabled: bool,
    pub grant: Vec<String>,
    /// sha256 (hex) of the manifest and the entry file as they were approved.
    pub digest: String,
    #[serde(default)]
    pub limits: Overrides,
    /// Unix seconds.
    #[serde(default)]
    pub approved_at: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Approvals(pub BTreeMap<String, Approval>);

impl Approvals {
    /// The file in `dir`; none there is an empty list. A file that is there and does not read is an
    /// error: every plugin then waits (an unreadable list never means everything is allowed).
    pub fn load(dir: &Path) -> Result<Approvals, String> {
        let path = dir.join(FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) if text.trim().is_empty() => Ok(Approvals::default()),
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Approvals::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn save(&self, dir: &Path) -> Result<(), String> {
        let path = dir.join(FILE);
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        crate::save::write_atomic(&path.to_string_lossy(), format!("{text}\n").as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// sha256 of the manifest's bytes, a zero byte and the entry's bytes, as hex.
pub fn digest(manifest: &[u8], entry: &[u8]) -> String {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    ctx.update(manifest);
    ctx.update(&[0]);
    ctx.update(entry);
    super::state::hex(ctx.finish().as_ref())
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
