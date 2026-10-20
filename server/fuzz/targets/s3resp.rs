//! What a bucket (or whoever answers in its place) says: a listing never panics and gives back the
//! keys that were written into it, an error text carries no control character and is short, and
//! the signing helpers keep their shape for any key.
use super::*;

fn escape(k: &str) -> String {
    k.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

pub fn run(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    // Any text as an answer.
    let (keys, next) = listing(&text);
    assert!(next.is_none() || text.contains("<IsTruncated>true</IsTruncated>"));
    drop(keys);
    let said = error_text(403, &text);
    assert!(said.starts_with("HTTP 403") && !said.chars().any(char::is_control), "{said:?}");
    assert!(said.chars().count() <= "HTTP 403: ".len() + 200 + " ()".len() + 200, "{said:?}");
    // Keys put into a well-formed answer come back as they were, in order, with the token.
    let parts: Vec<&str> = text.split('\u{0}').collect();
    let token = parts.last().copied().unwrap_or("");
    let written: Vec<&str> = parts.iter().take(parts.len().saturating_sub(1)).copied().collect();
    let xml = format!(
        "<ListBucketResult><IsTruncated>true</IsTruncated>{}<NextContinuationToken>{}</NextContinuationToken></ListBucketResult>",
        written.iter().map(|k| format!("<Contents><Key>{}</Key><Size>1</Size></Contents>", escape(k))).collect::<String>(),
        escape(token)
    );
    let (got, next) = listing(&xml);
    assert_eq!(got, written, "{xml}");
    assert_eq!(next.as_deref(), Some(token));
    let done = xml.replace("<IsTruncated>true", "<IsTruncated>false");
    assert!(listing(&done).1.is_none());
    // The signed path and query are what the encoder makes of any key: nothing outside the unreserved set.
    for keep in [false, true] {
        let e = encode(&text, keep);
        assert!(e.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.~%".contains(&b) || (keep && b == b'/')), "{e}");
    }
}
