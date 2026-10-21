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
    /// The trusted key (its id) that signed the plugin when it was approved: while the plugin's
    /// signature holds and that key is still trusted, a new version it signed needs no new approval
    /// as long as it asks for nothing beyond what was granted and what the key vouches for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signer: Option<String>,
    /// What the plugin asked for when it was approved (the grant may be less), and the hosts its
    /// `[net] allow` listed: a later version is compared with these, so the admin is shown what is new.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub asked: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
}

/// What a version asks for that the approved one did not, and what it no longer asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub added_hosts: Vec<String>,
    pub removed_hosts: Vec<String>,
}

impl Approval {
    /// The version asking for `requested` and `allow`, against what was approved. An approval made
    /// before these were recorded compares with its grant.
    pub fn diff(&self, requested: &[String], allow: &[String]) -> Diff {
        let before = if self.asked.is_empty() { &self.grant } else { &self.asked };
        let minus = |a: &[String], b: &[String]| a.iter().filter(|x| !b.contains(x)).cloned().collect::<Vec<_>>();
        Diff { added: minus(requested, before), removed: minus(before, requested), added_hosts: minus(allow, &self.allow), removed_hosts: minus(&self.allow, allow) }
    }
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
    crate::util::to_hex(ctx.finish().as_ref())
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Where a plugin stands: what its approval, its files and the signature on them make of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// Never approved (and not vouched for in full by a trusted key).
    Pending,
    /// The admin switched it off.
    Disabled,
    /// Approved once, but these files are not the ones, and no trusted signer stands for them.
    Changed,
    Running,
}

/// The standing of a plugin and the capabilities it is granted. Digest pinning is the rule: an
/// approval holds for the files whose digest it recorded. A good signature by a trusted key adds
/// to that, never takes away: the capabilities the key vouches for (its trust grant) need no
/// approval at all, and an approval made while the plugin was signed by that key (`signer`) holds
/// for a later version the key signed, as long as the version asks for nothing beyond what was
/// granted and what the key vouches for. The admin's own `disable` beats everything.
pub fn standing(approval: Option<&Approval>, digest: &str, requested: &[String], signed: &super::sign::Signed) -> (Standing, Vec<String>) {
    let key = signed.trusted();
    if approval.is_some_and(|a| !a.enabled) {
        return (Standing::Disabled, Vec::new());
    }
    let vouched: Vec<String> = key.map(|k| requested.iter().filter(|c| k.grant.contains(c)).cloned().collect()).unwrap_or_default();
    let held = approval.filter(|a| a.digest == digest || key.is_some_and(|k| a.signer.as_deref() == Some(k.id.as_str()) && requested.iter().all(|c| a.grant.contains(c) || k.grant.contains(c))));
    if held.is_none() && !(key.is_some() && vouched.len() == requested.len()) {
        return (if approval.is_some() { Standing::Changed } else { Standing::Pending }, Vec::new());
    }
    let mut grant = vouched;
    for g in held.into_iter().flat_map(|a| a.grant.iter()).filter(|g| requested.contains(g)) {
        if !grant.contains(g) {
            grant.push(g.clone());
        }
    }
    (Standing::Running, grant)
}
