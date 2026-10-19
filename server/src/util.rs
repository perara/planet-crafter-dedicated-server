//! Small helpers more than one module needs.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Bytes as lower-case hex.
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// When a file was last written; the epoch for one that cannot be read.
pub fn modified(path: impl AsRef<Path>) -> SystemTime {
    std::fs::metadata(path).and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH)
}

/// The folder a file is in: "." for a bare file name.
pub fn parent_dir(path: &Path) -> &Path {
    path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_helpers_do_what_their_names_say() {
        assert!(now_secs() > 1_700_000_000);
        assert_eq!(to_hex(&[0, 0x0a, 0xff]), "000aff");
        assert_eq!(parent_dir(Path::new("world.json")), Path::new("."));
        assert_eq!(parent_dir(Path::new("worlds/world.json")), Path::new("worlds"));
        assert_eq!(modified("/no/such/file"), UNIX_EPOCH);
        assert!(modified(env!("CARGO_MANIFEST_DIR")) > UNIX_EPOCH);
    }
}
