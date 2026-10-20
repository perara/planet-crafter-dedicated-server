//! config.json: the values the admin set for the `[[config]]` keys a manifest declares (a webhook
//! URL, a token). A key marked `secret` (the default) is put into a request by the host and shown
//! nowhere: not in the listing, the log, the event stream or a plugin's own memory. The file is
//! 0600 and is the admin's: `pcds plugin config set`, never an environment variable.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub const FILE: &str = "config.json";

/// A plugin's values: key to value.
pub type Values = BTreeMap<String, String>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Configs(pub BTreeMap<String, Values>);

/// What a value may be: one line of printable text, so it can sit in a URL or a header.
pub fn value_ok(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 2048 || value.bytes().any(|b| b < b' ' || b == 0x7f) {
        return Err("a value is 1 to 2048 bytes on one line, with no control character".into());
    }
    Ok(())
}

impl Configs {
    /// The file in `dir`; none there is no values. One that is there and does not read is an error:
    /// every plugin then runs without its values (a request that needs one is refused).
    pub fn load(dir: &Path) -> Result<Configs, String> {
        let path = dir.join(FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) if text.trim().is_empty() => Ok(Configs::default()),
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Configs::default()),
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

/// Sets (`Some`) or removes (`None`) one value of a plugin.
pub fn set(dir: &Path, plugin: &str, key: &str, value: Option<&str>) -> Result<(), String> {
    let mut all = Configs::load(dir)?;
    match value {
        Some(v) => {
            value_ok(v)?;
            all.0.entry(plugin.to_string()).or_default().insert(key.to_string(), v.to_string());
        }
        None => {
            if let Some(values) = all.0.get_mut(plugin) {
                values.remove(key);
                if values.is_empty() {
                    all.0.remove(plugin);
                }
            }
        }
    }
    all.save(dir)
}
