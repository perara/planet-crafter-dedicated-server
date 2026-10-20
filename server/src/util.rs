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

/// Bytes from hex text: an even number of hex digits, upper or lower case, and nothing else.
pub fn from_hex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err("an odd number of hex digits".into());
    }
    let digit = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    text.as_bytes().chunks(2).map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?)).collect::<Option<Vec<u8>>>().ok_or_else(|| "not hex".to_string())
}

/// When a file was last written; the epoch for one that cannot be read.
pub fn modified(path: &Path) -> SystemTime {
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
        assert_eq!(from_hex("000aFf").unwrap(), [0, 0x0a, 0xff]);
        assert_eq!(from_hex("").unwrap(), Vec::<u8>::new());
        // Bad hex is an error, never a zero byte, a sign or a panic on a text that is not ASCII.
        for bad in ["abc", "zz", "+f", "-1", "0x", "a\u{e9}01", "\u{e9}\u{e9}", " 0"] {
            assert!(from_hex(bad).is_err(), "{bad:?}");
        }
        assert_eq!(parent_dir(Path::new("world.json")), Path::new("."));
        assert_eq!(parent_dir(Path::new("worlds/world.json")), Path::new("worlds"));
        assert_eq!(modified(Path::new("/no/such/file")), UNIX_EPOCH);
        assert!(modified(Path::new(env!("CARGO_MANIFEST_DIR"))) > UNIX_EPOCH);
    }
}
