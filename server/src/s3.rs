//! Off-site copies of the save backups: after each backup the newest one of the running world
//! is PUT to an S3-compatible bucket (rules backup.*), signed with AWS Signature Version 4, on a
//! thread of its own so the game never waits for the network. The outcome is kept for
//! /api/status; the keys are never logged.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};

use crate::rules::Backup;
use crate::util::to_hex;

/// The last upload's outcome, and the backup file it was.
static LAST: Mutex<Option<Value>> = Mutex::new(None);
static UPLOADED: Mutex<Option<String>> = Mutex::new(None);
/// One upload at a time: a slow bucket must not pile up threads. A backup made meanwhile is
/// PENDING: the uploading thread sends the newest one when it is done.
static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What /api/status shows: off, or the bucket and the last upload.
pub fn status(b: &Backup) -> Value {
    let last = LAST.lock().unwrap_or_else(|e| e.into_inner()).clone();
    match &b.s3_bucket {
        None => json!({"enabled": false}),
        Some(bucket) => json!({"enabled": true, "bucket": bucket, "prefix": b.s3_prefix, "missing": missing(b), "uploading": BUSY.load(std::sync::atomic::Ordering::SeqCst), "last": last}),
    }
}

/// The settings an upload still needs.
fn missing(b: &Backup) -> Vec<&'static str> {
    [("s3Endpoint", b.s3_endpoint.is_none()), ("s3AccessKey", b.s3_access_key.is_none()), ("s3SecretKey", b.s3_secret_key.is_none())].into_iter().filter(|(_, m)| *m).map(|(k, _)| k).collect()
}

/// The newest backup of the save at `save_path` (backups/<world>-<time>.json).
pub fn newest_backup(save_path: &str) -> Option<PathBuf> {
    let path = Path::new(save_path);
    let stem = path.file_stem()?.to_string_lossy().to_string();
    let dir = path.with_file_name("backups");
    std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| {
            let name = e.ok()?.file_name().to_string_lossy().to_string();
            let time = name.strip_prefix(&format!("{stem}-"))?.strip_suffix(".json")?;
            (time.len() == 15 && time.bytes().enumerate().all(|(i, b)| if i == 8 { b == b'-' } else { b.is_ascii_digit() })).then_some(name)
        })
        .max()
        .map(|name| dir.join(name))
}

/// After a backup: the newest one uploaded, unless it already was (`force`: again anyway); with
/// an upload under way, it follows that one. Err only for what is wrong (off, settings missing,
/// no backup).
pub fn after_backup(b: &Backup, save_path: &str, force: bool) -> Result<Option<String>, String> {
    if b.s3_bucket.is_none() {
        return if force { Err("S3 backups are off: set backup.s3Bucket and the rest under Config".into()) } else { Ok(None) };
    }
    let missing = missing(b);
    if !missing.is_empty() {
        let e = format!("S3 backups: set backup.{} under Config", missing.join(", backup."));
        record(false, "", &e);
        return Err(e);
    }
    let Some(file) = newest_backup(save_path) else { return Err("no backup of this world yet".into()) };
    let name = file.file_name().unwrap_or_default().to_string_lossy().to_string();
    if !force && UPLOADED.lock().unwrap_or_else(|e| e.into_inner()).as_deref() == Some(name.as_str()) {
        return Ok(None);
    }
    let answer = format!("{}{name}", b.s3_prefix);
    if BUSY.swap(true, std::sync::atomic::Ordering::SeqCst) {
        PENDING.store(true, std::sync::atomic::Ordering::SeqCst);
        return Ok(Some(answer));
    }
    let (b, save_path) = (b.clone(), save_path.to_string());
    std::thread::spawn(move || {
        let mut file = file;
        loop {
            let name = file.file_name().unwrap_or_default().to_string_lossy().to_string();
            let key = format!("{}{name}", b.s3_prefix);
            let result = std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display())).and_then(|body| put(&b, &key, &body, SystemTime::now()));
            match &result {
                Ok(()) => {
                    println!("backup: {name} copied to s3://{}/{key}", b.s3_bucket.as_deref().unwrap_or(""));
                    *UPLOADED.lock().unwrap_or_else(|e| e.into_inner()) = Some(name.clone());
                }
                Err(e) => eprintln!("backup: {name} not copied to S3: {e}"),
            }
            record(result.is_ok(), &name, result.as_ref().err().map(String::as_str).unwrap_or(""));
            // A backup made during this upload: the newest goes next.
            match newest_backup(&save_path) {
                Some(next) if PENDING.swap(false, std::sync::atomic::Ordering::SeqCst) && next != file => file = next,
                _ => break,
            }
        }
        BUSY.store(false, std::sync::atomic::Ordering::SeqCst);
    });
    Ok(Some(answer))
}

fn record(ok: bool, file: &str, error: &str) {
    let at = crate::util::now_secs();
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = Some(json!({"ok": ok, "file": file, "error": (!ok).then_some(error), "at": at}));
}

/// RFC 3986 encoding, as SigV4 wants it: unreserved characters kept, '/' kept in paths.
fn encode(s: &str, keep_slash: bool) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) || (keep_slash && b == b'/') { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key), data.as_bytes()).as_ref().to_vec()
}

fn sha256(data: &[u8]) -> String {
    to_hex(ring::digest::digest(&ring::digest::SHA256, data).as_ref())
}

/// The Authorization header of a request (AWS Signature Version 4, service s3). `headers`:
/// lower-case names with their values, host and x-amz-date among them.
fn authorization(method: &str, path: &str, headers: &[(&str, String)], payload_hash: &str, region: &str, access: &str, secret: &str, amz_date: &str) -> String {
    let mut headers: Vec<&(&str, String)> = headers.iter().collect();
    headers.sort_by_key(|(k, _)| *k);
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
    let signed: Vec<&str> = headers.iter().map(|(k, _)| *k).collect();
    let signed = signed.join(";");
    let request = format!("{method}\n{}\n\n{canonical_headers}\n{signed}\n{payload_hash}", encode(path, true));
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/s3/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256(request.as_bytes()));
    let key = ["s3", "aws4_request"].iter().fold(hmac(&hmac(format!("AWS4{secret}").as_bytes(), date), region), |k, part| hmac(&k, part));
    format!("AWS4-HMAC-SHA256 Credential={access}/{scope}, SignedHeaders={signed}, Signature={}", to_hex(&hmac(&key, &to_sign)))
}

/// PUT one object (path-style: <endpoint>/<bucket>/<key>, which every S3-compatible service takes).
fn put(b: &Backup, key: &str, body: &[u8], now: SystemTime) -> Result<(), String> {
    let (Some(endpoint), Some(bucket), Some(access), Some(secret)) = (&b.s3_endpoint, &b.s3_bucket, &b.s3_access_key, &b.s3_secret_key) else { return Err("not configured".into()) };
    let uri: ureq::http::Uri = endpoint.parse().map_err(|_| "backup.s3Endpoint is not a URL".to_string())?;
    let host = uri.authority().map(|a| a.as_str().to_string()).ok_or("backup.s3Endpoint has no host")?;
    let path = format!("/{bucket}/{key}");
    let stamp = crate::save::stamp(now);
    let amz_date = format!("{}T{}Z", &stamp[..8], &stamp[9..]);
    let hash = sha256(body);
    let headers = [("host", host.clone()), ("x-amz-content-sha256", hash.clone()), ("x-amz-date", amz_date.clone())];
    let auth = authorization("PUT", &path, &headers, &hash, &b.s3_region, access, secret, &amz_date);
    let config = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(300))).http_status_as_error(false).max_redirects(0).build();
    let agent = ureq::Agent::new_with_config(config);
    let url = format!("{}://{host}{}", uri.scheme_str().unwrap_or("https"), encode(&path, true));
    let mut answer = agent
        .put(&url)
        .header("x-amz-content-sha256", &hash)
        .header("x-amz-date", &amz_date)
        .header("Authorization", &auth)
        .send(body)
        .map_err(|e| e.to_string())?;
    if answer.status().is_success() {
        return Ok(());
    }
    // S3 errors are XML: <Code>AccessDenied</Code><Message>...</Message>. Never echoes the keys.
    let text = answer.body_mut().with_config().limit(16 << 10).read_to_string().unwrap_or_default();
    let field = |tag: &str| text.split(&format!("<{tag}>")).nth(1).and_then(|r| r.split(&format!("</{tag}>")).next()).map(str::to_string);
    Err(format!("HTTP {}{}", answer.status().as_u16(), field("Code").map(|c| format!(": {c}{}", field("Message").map(|m| format!(" ({m})")).unwrap_or_default())).unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AWS's own example (Signature Version 4, "GET Object"): the same key, date, request and
    /// signature as in the S3 API reference.
    #[test]
    fn signs_like_aws() {
        let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let headers = [("host", "examplebucket.s3.amazonaws.com".to_string()), ("range", "bytes=0-9".to_string()), ("x-amz-content-sha256", empty.to_string()), ("x-amz-date", "20130524T000000Z".to_string())];
        let auth = authorization("GET", "/test.txt", &headers, empty, "us-east-1", "AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY", "20130524T000000Z");
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        assert_eq!(encode("pcds/world 1+2.json", true), "pcds/world%201%2B2.json");
    }

    #[test]
    fn the_newest_backup_of_this_world() {
        let dir = std::env::temp_dir().join(format!("pcds-s3-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("backups")).unwrap();
        for f in ["world-20260101-000000.json", "world-20261018-120000.json", "world-2-20271231-000000.json", "world-removed-20280101-000000.json"] {
            std::fs::write(dir.join("backups").join(f), "{}").unwrap();
        }
        let newest = newest_backup(dir.join("world.json").to_str().unwrap()).unwrap();
        assert_eq!(newest.file_name().unwrap(), "world-20261018-120000.json");
        let off = Backup::default();
        assert!(after_backup(&off, dir.join("world.json").to_str().unwrap(), false).unwrap().is_none());
        let half = Backup { s3_bucket: Some("saves".into()), ..Backup::default() };
        assert!(after_backup(&half, dir.join("world.json").to_str().unwrap(), false).unwrap_err().contains("s3Endpoint"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
