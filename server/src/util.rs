//! Small helpers more than one module needs.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// UTC date and time of a Unix time: (year, month, day, hour, minute, second). Days to civil date:
/// Howard Hinnant's algorithm.
pub fn utc(unix: u64) -> (i64, i64, i64, i64, i64, i64) {
    let s = unix as i64;
    let (z, secs) = (s.div_euclid(86_400) + 719_468, s.rem_euclid(86_400));
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, m) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    (yoe + era * 400 + (m <= 2) as i64, m, d, secs / 3600, secs / 60 % 60, secs % 60)
}

/// Text another party chose (a plugin's author, a bucket), made safe to put on a terminal and in a log: a control character (an escape
/// sequence that hides, redraws or sets the clipboard; a line break that forges a line), a mark that
/// turns the text around (U+202E) and the zero-width ones become `?`. Plain text is returned as it is.
pub fn clean(s: &str) -> String {
    s.chars().map(|c| if c.is_control() || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{feff}') { '?' } else { c }).collect()
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

/// An exclusive lock on `path` (flock), held while the file lives: Err with `busy` and the
/// holder's pid when another process has it. The file holds the holder's pid.
pub fn hold(path: &Path, busy: &str) -> Result<std::fs::File, String> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let mut file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    // SAFETY: flock on a descriptor this function owns; the lock lasts as long as the file.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let holder = std::fs::read_to_string(path).unwrap_or_default();
        return Err(format!("{busy} (process {}, which holds {}); stop it or wait for it", holder.trim(), path.display()));
    }
    // The holder, for the message above.
    let _ = file.set_len(0).and_then(|_| write!(file, "{}", std::process::id()));
    Ok(file)
}

/// A file written whole or not at all: beside it, flushed to disk, then moved over it (and the
/// move flushed), so a crash or a full disk leaves the old file or the new one, never part.
pub fn replace_file(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let new = path.with_file_name(format!(".{name}.new"));
    let _ = std::fs::remove_file(&new);
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&new).map_err(err)?;
    let written = f.write_all(bytes).and_then(|_| f.sync_all()).and_then(|_| std::fs::rename(&new, path));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&new);
        return Err(err(e));
    }
    let dir = parent_dir(path);
    std::fs::File::open(dir).and_then(|d| d.sync_all()).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(())
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
    fn a_lock_is_held_by_one_holder_at_a_time() {
        let dir = std::env::temp_dir().join(format!("pcds-util-hold-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("world.json.lock");
        let first = hold(&path, "another server runs this world").unwrap();
        let e = hold(&path, "another server runs this world").unwrap_err();
        assert!(e.contains("another server runs this world") && e.contains(&std::process::id().to_string()), "{e}");
        drop(first);
        hold(&path, "another server runs this world").unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A file replaced whole: the new bytes with the mode asked for, no file of the writer's left
    /// beside it, and the old content gone only once the new is complete.
    #[test]
    fn a_file_is_replaced_whole_with_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pcds-util-replace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("active");
        replace_file(&path, b"a\n", 0o600).unwrap();
        replace_file(&path, b"b\n", 0o600).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"b\n");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "nothing left beside it");
        // A folder in the way: an error naming the file, the folder untouched, no leftover.
        let blocked = dir.join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("x"), "x").unwrap();
        assert!(replace_file(&blocked, b"no", 0o644).unwrap_err().contains("blocked"));
        assert!(blocked.join("x").is_file() && !dir.join(".blocked.new").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
