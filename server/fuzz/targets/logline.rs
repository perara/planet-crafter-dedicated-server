//! The log: whatever a message holds (control characters, secrets, bearer tokens, quotes), a line is
//! one JSON object per line or lines a terminal shows as text, and no registered secret survives.
//! Also util::utc against days-from-civil, its inverse.
use super::*;

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

pub fn run(data: &[u8]) {
    let Some((&flags, rest)) = data.split_first() else { return };
    let mut parts = rest.split(|b| *b == 0xff).map(|p| String::from_utf8_lossy(p).into_owned());
    let (message, a, b) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
    // The secrets the registry holds are at least MIN_SECRET characters; the property is for the ones
    // that cannot be formed again by the replacement itself (no `*`).
    let secrets: Vec<&str> = [a.as_str(), b.as_str()].into_iter().filter(|s| s.len() >= MIN_SECRET && s.bytes().all(|c| c.is_ascii_alphanumeric())).collect();
    let redacted = redact_with(&message, secrets.iter().copied());
    for s in &secrets {
        assert!(!redacted.contains(s), "{s:?} survives in {redacted:?}");
    }
    assert_eq!(redact_with(&redacted, secrets.iter().copied()), redacted, "masking twice is masking once");
    let mut at = 0;
    while let Some(i) = redacted[at..].find("Bearer ") {
        let after = &redacted[at + i + 7..];
        assert!(after.is_empty() || after.starts_with(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '*')), "a bearer token is left in {redacted:?}");
        at += i + 7;
    }
    let level = [Level::Error, Level::Warn, Level::Info, Level::Debug][(flags >> 2) as usize % 4];
    let fields = [("name", json!(a)), ("n", json!(flags))];
    let line = render(flags & 1 == 1, flags & 2 == 2, level, "front", &redacted, &fields, SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(u64::from(flags) * 1_234_567_891), 7, "front");
    if flags & 1 == 1 {
        let body = line.strip_prefix(&format!("<{}>", level.priority())).filter(|_| flags & 2 == 2).unwrap_or(&line);
        assert!(!body.contains('\n') && !body.contains('\r'), "one line: {line:?}");
        let v: Value = serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {line:?}"));
        assert_eq!(v["message"], redacted.as_str());
        assert_eq!(v["level"], level.name());
        assert_eq!(v["fields"]["name"], a.as_str());
    } else {
        assert!(line.chars().all(|c| !c.is_control() || c == '\n' || c == '\t'), "a terminal is steered by {line:?}");
        let expect = redacted.replace("\r\n", "\n").lines().count().max(1);
        assert_eq!(line.lines().count(), expect, "{line:?}");
        let prefix = if flags & 2 == 2 { format!("<{}>", level.priority()) } else { String::new() };
        assert!(line.lines().all(|l| l.starts_with(&prefix)), "{line:?}");
    }
    let _ = component(&message);
    // util::utc against the inverse algorithm, for any time up to the year 2^40 s is.
    let secs = u64::from_le_bytes([data[0], data.get(1).copied().unwrap_or(0), data.get(2).copied().unwrap_or(0), data.get(3).copied().unwrap_or(0), data.get(4).copied().unwrap_or(0), data.get(5).copied().unwrap_or(0), 0, 0]);
    let (y, m, d, h, mi, s) = crate::util::utc(secs);
    assert!((1..=12).contains(&m) && (1..=31).contains(&d) && h < 24 && mi < 60 && s < 60);
    assert_eq!(days_from_civil(y, m, d) * 86_400 + h * 3600 + mi * 60 + s, secs as i64);
    let _ = timestamp(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs));
}
