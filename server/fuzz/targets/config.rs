//! config::check_env on a hostile server.env: every setting's value is text from the file.
use super::*;

pub fn run(data: &[u8]) {
    // KEY=VALUE lines, as server.env has them (invalid UTF-8 is lossy-converted).
    let text = String::from_utf8_lossy(data);
    let get = |key: &str| text.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('=').map(str::to_string));
    let _ = check_env(get);
    // And with every known key present and the input as its value.
    let all = |_: &str| Some(text.to_string());
    let _ = check_env(all);
}
