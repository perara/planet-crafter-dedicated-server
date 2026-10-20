//! Off-site copies of the save backups: after each backup the newest one of the running world
//! is PUT to an S3-compatible bucket (rules backup.*), signed with AWS Signature Version 4, on a
//! thread of its own so the game never waits for the network. The outcome is kept for
//! /api/status; the keys are never logged.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::rules::Backup;
use crate::util::to_hex;

/// The last upload's outcome, and the backup file it was.
static LAST: Mutex<Option<LastUpload>> = Mutex::new(None);
static UPLOADED: Mutex<Option<String>> = Mutex::new(None);
/// One upload at a time: a slow bucket must not pile up threads. A backup made meanwhile is
/// PENDING: the uploading thread sends the newest one when it is done.
static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The upload under way: BUSY is freed when its thread ends, panic or not (it stayed set for ever
/// after a panic, and no backup was copied off-site again).
struct Upload;

impl Drop for Upload {
    fn drop(&mut self) {
        BUSY.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The last upload's outcome: whether it worked, the backup file it was, why not.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LastUpload {
    ok: bool,
    file: String,
    error: Option<String>,
    at: u64,
}

/// What /api/status shows: off, or the bucket and the last upload.
#[derive(Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Status<'a> {
    Off { enabled: bool },
    On { enabled: bool, bucket: &'a str, prefix: &'a str, missing: Vec<&'static str>, uploading: bool, last: Option<LastUpload> },
}

pub fn status(b: &Backup) -> Status<'_> {
    let last = LAST.lock().unwrap_or_else(|e| e.into_inner()).clone();
    status_of(b, last, BUSY.load(std::sync::atomic::Ordering::SeqCst))
}

fn status_of(b: &Backup, last: Option<LastUpload>, uploading: bool) -> Status<'_> {
    match &b.s3_bucket {
        None => Status::Off { enabled: false },
        Some(bucket) => Status::On { enabled: true, bucket, prefix: &b.s3_prefix, missing: missing(b), uploading, last },
    }
}

/// The settings an upload still needs.
fn missing(b: &Backup) -> Vec<&'static str> {
    [("s3Endpoint", b.s3_endpoint.is_none()), ("s3AccessKey", b.s3_access_key.is_none()), ("s3SecretKey", b.s3_secret_key.is_none())].into_iter().filter(|(_, m)| *m).map(|(k, _)| k).collect()
}

/// The newest backup of the save at `save_path` (backups/<world>-<time>.json), by
/// `save::backup_stamps`: a stamp from the future (the clock stepped back) is not the newest.
pub fn newest_backup(save_path: &str) -> Option<PathBuf> {
    let path = Path::new(save_path);
    let stem = path.file_stem()?.to_string_lossy().to_string();
    let dir = path.with_file_name("backups");
    let stamps = crate::save::backup_stamps(&dir, &stem, &crate::save::stamp(std::time::SystemTime::now())).ok()?;
    stamps.last().map(|t| dir.join(format!("{stem}-{t}.json")))
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
    let spawned = std::thread::Builder::new().name("pcds-s3".into()).spawn(move || {
        let _busy = Upload;
        let mut file = file;
        loop {
            let name = file.file_name().unwrap_or_default().to_string_lossy().to_string();
            let key = format!("{}{name}", b.s3_prefix);
            let result = std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display())).and_then(|body| put(&b, &key, &body, SystemTime::now()));
            match &result {
                Ok(()) => {
                    info!("file" = name, "key" = key; "backup: {name} copied to s3://{}/{key}", b.s3_bucket.as_deref().unwrap_or(""));
                    *UPLOADED.lock().unwrap_or_else(|e| e.into_inner()) = Some(name.clone());
                }
                Err(e) => warn!("backup: {name} not copied to S3: {e}"),
            }
            record(result.is_ok(), &name, result.as_ref().err().map(String::as_str).unwrap_or(""));
            // A backup made during this upload: the newest goes next.
            match newest_backup(&save_path) {
                Some(next) if PENDING.swap(false, std::sync::atomic::Ordering::SeqCst) && next != file => file = next,
                _ => break,
            }
        }
    });
    if let Err(e) = spawned {
        // Never ran, so nothing frees BUSY: and this runs on the game loop, which must not panic on it.
        BUSY.store(false, std::sync::atomic::Ordering::SeqCst);
        return Err(format!("no thread for the upload: {e}"));
    }
    Ok(Some(answer))
}

fn record(ok: bool, file: &str, error: &str) {
    let at = crate::util::now_secs();
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = Some(LastUpload { ok, file: file.to_string(), error: (!ok).then(|| error.to_string()), at });
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

/// The Authorization header of a request (AWS Signature Version 4, service s3). `query`: the
/// canonical query string (names sorted, names and values encoded), "" for none. `headers`:
/// lower-case names with their values, host and x-amz-date among them.
fn authorization(method: &str, path: &str, query: &str, headers: &[(&str, String)], payload_hash: &str, region: &str, access: &str, secret: &str, amz_date: &str) -> String {
    let mut headers: Vec<&(&str, String)> = headers.iter().collect();
    headers.sort_by_key(|(k, _)| *k);
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
    let signed: Vec<&str> = headers.iter().map(|(k, _)| *k).collect();
    let signed = signed.join(";");
    let request = format!("{method}\n{}\n{query}\n{canonical_headers}\n{signed}\n{payload_hash}", encode(path, true));
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/s3/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256(request.as_bytes()));
    let key = ["s3", "aws4_request"].iter().fold(hmac(&hmac(format!("AWS4{secret}").as_bytes(), date), region), |k, part| hmac(&k, part));
    format!("AWS4-HMAC-SHA256 Credential={access}/{scope}, SignedHeaders={signed}, Signature={}", to_hex(&hmac(&key, &to_sign)))
}

/// The Host header the request goes out with, which the signature must cover as sent: the host
/// and the port unless it is the scheme's own (ureq leaves that one out), never a user:password@.
fn host_header(uri: &ureq::http::Uri) -> Option<String> {
    let host = uri.host()?;
    let default = if uri.scheme_str() == Some("http") { 80 } else { 443 };
    Some(match uri.port_u16() {
        Some(port) if port != default => format!("{host}:{port}"),
        _ => host.to_string(),
    })
}

/// One signed request to the bucket (path-style: <endpoint>/<bucket>/<key>, which every S3-compatible
/// service takes). `query`: (name, value) pairs. The answer is returned whatever its status.
fn send(b: &Backup, method: &str, key: &str, query: &[(&str, &str)], body: &[u8], now: SystemTime) -> Result<ureq::http::Response<ureq::Body>, String> {
    let (Some(endpoint), Some(bucket), Some(access), Some(secret)) = (&b.s3_endpoint, &b.s3_bucket, &b.s3_access_key, &b.s3_secret_key) else { return Err("not configured".into()) };
    let uri: ureq::http::Uri = endpoint.parse().map_err(|_| "backup.s3Endpoint is not a URL".to_string())?;
    let scheme = uri.scheme_str().unwrap_or("https");
    let host = host_header(&uri).ok_or("backup.s3Endpoint has no host")?;
    let path = if key.is_empty() { format!("/{bucket}") } else { format!("/{bucket}/{key}") };
    let stamp = crate::save::stamp(now);
    let amz_date = format!("{}T{}Z", &stamp[..8], &stamp[9..]);
    let hash = sha256(body);
    let headers = [("host", host.clone()), ("x-amz-content-sha256", hash.clone()), ("x-amz-date", amz_date.clone())];
    let mut pairs: Vec<(String, String)> = query.iter().map(|(k, v)| (encode(k, false), encode(v, false))).collect();
    pairs.sort();
    let query = pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    let auth = authorization(method, &path, &query, &headers, &hash, &b.s3_region, access, secret, &amz_date);
    let config = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(300))).http_status_as_error(false).max_redirects(0).build();
    let agent = ureq::Agent::new_with_config(config);
    let url = format!("{scheme}://{host}{}{}", encode(&path, true), if query.is_empty() { String::new() } else { format!("?{query}") });
    if method == "PUT" {
        return agent.put(&url).header("x-amz-content-sha256", &hash).header("x-amz-date", &amz_date).header("Authorization", &auth).send(body).map_err(|e| e.to_string());
    }
    let request = if method == "HEAD" { agent.head(&url) } else { agent.get(&url) };
    request.header("x-amz-content-sha256", &hash).header("x-amz-date", &amz_date).header("Authorization", &auth).call().map_err(|e| e.to_string())
}

/// What an error answer says: "HTTP 403: AccessDenied (message)". S3 errors are XML:
/// <Code>AccessDenied</Code><Message>...</Message>. Never echoes the keys.
fn failure(mut answer: ureq::http::Response<ureq::Body>) -> String {
    let status = answer.status().as_u16();
    let text = answer.body_mut().with_config().limit(16 << 10).read_to_string().unwrap_or_default();
    error_text(status, &text)
}

/// "HTTP 403: AccessDenied (message)" from the status and the XML body of an error. The body is the
/// bucket's: what it says goes to a terminal and the log as text, so control characters and
/// direction marks are written as `?` and a field is cut at 200 characters.
fn error_text(status: u16, text: &str) -> String {
    let field = |tag: &str| text.split(&format!("<{tag}>")).nth(1).and_then(|r| r.split(&format!("</{tag}>")).next()).map(|v| crate::plugins::clean(&unescape(v)).chars().take(200).collect::<String>());
    format!("HTTP {status}{}", field("Code").map(|c| format!(": {c}{}", field("Message").map(|m| format!(" ({m})")).unwrap_or_default())).unwrap_or_default())
}

/// PUT one object.
fn put(b: &Backup, key: &str, body: &[u8], now: SystemTime) -> Result<(), String> {
    let answer = send(b, "PUT", key, &[], body, now)?;
    if answer.status().is_success() { Ok(()) } else { Err(failure(answer)) }
}

/// The size of an object (HEAD), None when the bucket has none by that key.
pub fn head(b: &Backup, key: &str) -> Result<Option<u64>, String> {
    let answer = send(b, "HEAD", key, &[], &[], SystemTime::now())?;
    match answer.status().as_u16() {
        404 => Ok(None),
        200..=299 => Ok(answer.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok())),
        _ => Err(failure(answer)),
    }
}

/// An object's bytes (at most 256 MiB).
pub fn get(b: &Backup, key: &str) -> Result<Vec<u8>, String> {
    let mut answer = send(b, "GET", key, &[], &[], SystemTime::now())?;
    if !answer.status().is_success() {
        return Err(failure(answer));
    }
    answer.body_mut().with_config().limit(256 << 20).read_to_vec().map_err(|e| e.to_string())
}

/// The keys that begin with `prefix` (ListObjectsV2), all pages (at most 10).
pub fn list(b: &Backup, prefix: &str) -> Result<Vec<String>, String> {
    let (mut keys, mut token) = (Vec::new(), None::<String>);
    for _ in 0..10 {
        let mut query = vec![("list-type", "2"), ("prefix", prefix)];
        if let Some(t) = &token {
            query.push(("continuation-token", t));
        }
        let mut answer = send(b, "GET", "", &query, &[], SystemTime::now())?;
        if !answer.status().is_success() {
            return Err(failure(answer));
        }
        let xml = answer.body_mut().with_config().limit(16 << 20).read_to_string().map_err(|e| e.to_string())?;
        let (page, next) = listing(&xml);
        keys.extend(page);
        token = next;
        if token.is_none() {
            return Ok(keys);
        }
    }
    Err("the bucket lists more than 10,000 backups under that prefix: use a longer prefix or clear old ones".into())
}

/// One page of a ListObjectsV2 answer: the keys, and the token of the next page when the answer
/// says it is cut.
fn listing(xml: &str) -> (Vec<String>, Option<String>) {
    let each = |tag: &str| -> Vec<String> { xml.split(&format!("<{tag}>")).skip(1).filter_map(|r| r.split(&format!("</{tag}>")).next()).map(unescape).collect() };
    (each("Key"), each("NextContinuationToken").pop().filter(|_| xml.contains("<IsTruncated>true</IsTruncated>")))
}

/// The XML entities S3 uses in a key.
fn unescape(text: &str) -> String {
    text.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    /// The status before it had types: the JSON built by hand. The typed status prints the same bytes.
    #[test]
    fn the_status_prints_what_the_json_macro_did() {
        let old = |b: &Backup, last: Option<Value>, uploading: bool| match &b.s3_bucket {
            None => json!({"enabled": false}),
            Some(bucket) => json!({"enabled": true, "bucket": bucket, "prefix": b.s3_prefix, "missing": missing(b), "uploading": uploading, "last": last}),
        };
        let off = Backup::default();
        let configured = Backup { s3_bucket: Some("my-bucket".into()), s3_prefix: "pcds/\"x\"/".into(), s3_endpoint: Some("https://s3.example".into()), ..Backup::default() };
        let complete = Backup { s3_access_key: Some("a".into()), s3_secret_key: Some("b".into()), ..configured.clone() };
        for b in [&off, &configured, &complete] {
            for uploading in [false, true] {
                for (typed, hand) in [
                    (None, None),
                    (Some(LastUpload { ok: true, file: "w-20260101-000000".into(), error: None, at: 1_760_000_000 }), Some(json!({"ok": true, "file": "w-20260101-000000", "error": null, "at": 1_760_000_000u64}))),
                    (Some(LastUpload { ok: false, file: "w".into(), error: Some("HTTP 403: AccessDenied".into()), at: 5 }), Some(json!({"ok": false, "file": "w", "error": "HTTP 403: AccessDenied", "at": 5}))),
                ] {
                    assert_eq!(serde_json::to_string(&status_of(b, typed, uploading)).unwrap(), old(b, hand, uploading).to_string());
                }
            }
        }
        assert_eq!(serde_json::to_string(&status_of(&off, None, false)).unwrap(), r#"{"enabled":false}"#);
        assert_eq!(serde_json::to_string(&status_of(&configured, None, false)).unwrap(), r#"{"enabled":true,"bucket":"my-bucket","prefix":"pcds/\"x\"/","missing":["s3AccessKey","s3SecretKey"],"uploading":false,"last":null}"#);
    }

    /// AWS's own example (Signature Version 4, "GET Object"): the same key, date, request and
    /// signature as in the S3 API reference.
    #[test]
    fn signs_like_aws() {
        let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let headers = [("host", "examplebucket.s3.amazonaws.com".to_string()), ("range", "bytes=0-9".to_string()), ("x-amz-content-sha256", empty.to_string()), ("x-amz-date", "20130524T000000Z".to_string())];
        let auth = authorization("GET", "/test.txt", "", &headers, empty, "us-east-1", "AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY", "20130524T000000Z");
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        assert_eq!(encode("pcds/world 1+2.json", true), "pcds/world%201%2B2.json");
    }

    /// The signature covers the Host header that goes out: not the default port ureq leaves off,
    /// and not a user:password@ in the endpoint.
    #[test]
    fn the_signed_host_is_the_host_sent() {
        let host = |url: &str| host_header(&url.parse().unwrap());
        assert_eq!(host("https://s3.example.com:443").as_deref(), Some("s3.example.com"));
        assert_eq!(host("http://minio:80/").as_deref(), Some("minio"));
        assert_eq!(host("http://minio:9000").as_deref(), Some("minio:9000"));
        assert_eq!(host("https://u:p@s3.example.com:8443").as_deref(), Some("s3.example.com:8443"));
        assert_eq!(host("https://s3.example.com").as_deref(), Some("s3.example.com"));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let seen = crate::testkit::read_request(&mut stream);
            std::io::Write::write_all(&mut stream, b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            seen
        });
        let b = Backup { s3_endpoint: Some(format!("http://u:p@127.0.0.1:{port}")), s3_bucket: Some("saves".into()), s3_access_key: Some("AKIAEXAMPLE".into()), s3_secret_key: Some("secret".into()), ..Backup::default() };
        put(&b, "pcds/world.json", b"{}", SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)).unwrap();
        let (headers, _) = server.join().unwrap();
        let get = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap();
        let sent = [("host", get("host")), ("x-amz-content-sha256", get("x-amz-content-sha256")), ("x-amz-date", get("x-amz-date"))];
        let again = authorization("PUT", "/saves/pcds/world.json", "", &sent, &get("x-amz-content-sha256"), "us-east-1", "AKIAEXAMPLE", "secret", &get("x-amz-date"));
        assert_eq!(get("authorization"), again);
    }

    /// What a bucket says goes to a terminal and the log: its error text has no control character
    /// (an escape sequence would rewrite the screen), reads its XML entities and is cut short.
    #[test]
    fn an_error_body_is_plain_text_and_a_listing_reads_back_its_keys() {
        assert_eq!(error_text(403, "<Error><Code>Access&amp;Denied</Code><Message>no\x1b[31m &lt;x&gt;\r\n</Message></Error>"), "HTTP 403: Access&Denied (no?[31m <x>??)");
        assert_eq!(error_text(500, "not xml"), "HTTP 500");
        assert_eq!(error_text(400, &format!("<Error><Code>{}</Code></Error>", "x".repeat(900))).chars().count(), "HTTP 400: ".len() + 200);
        let (keys, next) = listing("<ListBucketResult><IsTruncated>true</IsTruncated><Contents><Key>a&amp;lt;b</Key></Contents><Contents><Key></Key></Contents><NextContinuationToken>t&amp;1</NextContinuationToken></ListBucketResult>");
        assert_eq!((keys, next.as_deref()), (vec!["a&lt;b".to_string(), String::new()], Some("t&1")));
        assert_eq!(listing("<ListBucketResult><IsTruncated>false</IsTruncated><NextContinuationToken>t</NextContinuationToken></ListBucketResult>"), (vec![], None));
    }

    /// AWS's own example for a request with a query ("GET Bucket (List Objects)").
    #[test]
    fn signs_a_query_like_aws() {
        let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let headers = [("host", "examplebucket.s3.amazonaws.com".to_string()), ("x-amz-content-sha256", empty.to_string()), ("x-amz-date", "20130524T000000Z".to_string())];
        let auth = authorization("GET", "/", "max-keys=2&prefix=J", &headers, empty, "us-east-1", "AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY", "20130524T000000Z");
        assert!(auth.ends_with("SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"), "{auth}");
    }

    /// A bucket's answers to HEAD, GET and the listing (two pages), on a stand-in server that records
    /// each request line: sizes, a missing key, bytes, an error with its S3 code, and keys with the
    /// characters XML escapes.
    #[test]
    fn the_bucket_is_asked_for_a_size_a_file_and_a_listing() {
        use std::io::{BufRead, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let answers: Vec<Vec<u8>> = vec![
            b"HTTP/1.1 200 OK\r\nContent-Length: 2458107\r\nConnection: close\r\n\r\n".to_vec(),
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nsave".to_vec(),
            {
                let xml = "<ListBucketResult><IsTruncated>true</IsTruncated><Contents><Key>pcds/world-20261020-170930.json</Key></Contents><Contents><Key>pcds/a&amp;b.json</Key></Contents><NextContinuationToken>tok/en+1</NextContinuationToken></ListBucketResult>";
                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{xml}", xml.len()).into_bytes()
            },
            {
                let xml = "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>pcds/world-20261021-010000.json</Key></Contents></ListBucketResult>";
                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{xml}", xml.len()).into_bytes()
            },
            {
                let xml = "<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>";
                format!("HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{xml}", xml.len()).into_bytes()
            },
        ];
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for answer in answers {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let (mut auth, mut length, mut line) = (false, 0usize, String::new());
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap() == 0 || line.trim().is_empty() {
                        break;
                    }
                    auth |= line.to_ascii_lowercase().starts_with("authorization: aws4-hmac-sha256 credential=akiaexample/");
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap();
                    }
                }
                reader.read_exact(&mut vec![0; length]).unwrap();
                seen.push(format!("{} signed={auth}", first.trim()));
                stream.write_all(&answer).unwrap();
            }
            seen
        });
        let b = Backup { s3_endpoint: Some(format!("http://127.0.0.1:{port}")), s3_bucket: Some("saves".into()), s3_access_key: Some("AKIAEXAMPLE".into()), s3_secret_key: Some("secret".into()), ..Backup::default() };
        assert_eq!(head(&b, "pcds/world-1.json"), Ok(Some(2_458_107)));
        assert_eq!(head(&b, "pcds/none.json"), Ok(None));
        assert_eq!(head(&b, "pcds/denied.json").unwrap_err(), "HTTP 403");
        assert_eq!(get(&b, "pcds/world-1.json").unwrap(), b"save");
        assert_eq!(list(&b, "pcds/world-").unwrap(), ["pcds/world-20261020-170930.json", "pcds/a&b.json", "pcds/world-20261021-010000.json"]);
        assert_eq!(list(&b, "pcds/").unwrap_err(), "HTTP 403: AccessDenied (Access Denied)");
        assert_eq!(
            server.join().unwrap(),
            [
                "HEAD /saves/pcds/world-1.json HTTP/1.1 signed=true",
                "HEAD /saves/pcds/none.json HTTP/1.1 signed=true",
                "HEAD /saves/pcds/denied.json HTTP/1.1 signed=true",
                "GET /saves/pcds/world-1.json HTTP/1.1 signed=true",
                "GET /saves?list-type=2&prefix=pcds%2Fworld- HTTP/1.1 signed=true",
                "GET /saves?continuation-token=tok%2Fen%2B1&list-type=2&prefix=pcds%2Fworld- HTTP/1.1 signed=true",
                "GET /saves?list-type=2&prefix=pcds%2F HTTP/1.1 signed=true",
            ]
        );
        let off = Backup::default();
        assert_eq!(head(&off, "x"), Err("not configured".into()));
    }

    #[test]
    fn a_panicking_upload_does_not_keep_the_next_ones_out() {
        BUSY.store(true, std::sync::atomic::Ordering::SeqCst);
        let done = std::thread::spawn(|| {
            let _busy = Upload;
            panic!("the upload thread died");
        })
        .join();
        assert!(done.is_err());
        assert!(!BUSY.load(std::sync::atomic::Ordering::SeqCst));
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
