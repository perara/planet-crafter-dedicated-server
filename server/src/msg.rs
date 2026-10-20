//! What the front, the banks and the banks' folder say to each other, as types. Each message is
//! one struct, written by one side and read by the other with the same definition, so a field
//! has one name and one place (docs/hot-update.md has the frames, ipc.rs their kinds). The bytes
//! are JSON, as they always were: a field here is a key there, in the order declared. A message
//! from a bank of another release may lack keys or carry more (a hot update runs two releases
//! at once), so reading is lenient: missing keys are `None`, keys not known are kept where the
//! message is written back.
//!
//! | frame | struct | from -> to |
//! |---|---|---|
//! | HELLO | [`Hello`] | bank -> front |
//! | RESTORE | [`Restore`] | front -> bank |
//! | READY | [`Ready`] | bank -> front |
//! | CONNECTED | [`Connected`] | front -> bank |
//! | SNAPSHOT | [`SnapshotNote`] (and a memfd holding a snapshot, whose first line is a [`SnapshotHead`]) | bank -> front |
//! | ADMIN, ANSWER | [`AdminCall`], [`AdminAnswer`] | front -> bank, bank -> front |
//! | OPS | [`Ops`] | bank -> front |
//! | CHAT | [`ChatEvent`] | bank -> front |
//! | `pcds bank-info`, bank.json | [`BankInfo`] | a bank's program, the banks' folder |

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A message as a frame's body.
pub fn body<T: Serialize>(message: &T) -> Vec<u8> {
    serde_json::to_vec(message).unwrap_or_default()
}

/// A frame's body as a message. Err says what is wrong with it (a value is never quoted: serde's
/// messages do).
pub fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, String> {
    serde_json::from_slice(body).map_err(|e| e.to_string())
}

/// A value as JSON text, for a message that says what a bank reported (`null` when it did not).
pub fn json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// What a bank's program says it is: `pcds bank-info`, the first fields of its HELLO and what
/// bank.json holds. The release adds its `tag`, the download's `sha256` and when it was
/// `installed`; keys not known here (a later release's, a local build's `from`) are kept.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BankInfo {
    /// The program's version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The front protocol it speaks (ipc::PROTOCOL).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<u64>,
    /// The snapshot format it writes and reads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<u64>,
    /// The format of the game tables it loads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tables_format: Option<u64>,
    /// The release's tag (bank.json of an installed release).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Unix seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed: Option<u64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl BankInfo {
    pub fn new(version: &str, protocol: u64, format: u64, tables_format: u64) -> BankInfo {
        BankInfo { version: Some(version.to_string()), protocol: Some(protocol), format: Some(format), tables_format: Some(tables_format), ..Default::default() }
    }

    /// The version, "?" when it does not say.
    pub fn version(&self) -> &str {
        self.version.as_deref().unwrap_or("?")
    }
}

/// A bank's first frame: what it is, and the game version of the tables it loaded.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    #[serde(flatten)]
    pub info: BankInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub game_version: Option<String>,
}

/// The body of RESTORE: whether the bank restarts after a crash (every client in the snapshot
/// is gone) or takes over at a flip.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Restore {
    #[serde(default)]
    pub recover: bool,
}

/// A bank's READY (the health check) after its restore. The times are on the monotonic clock
/// all three processes read (ipc::now_ns, ns); `got_at` is 0 for a bank that loaded the save.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ready {
    pub restore_ms: Option<f64>,
    pub verify_ms: Option<f64>,
    /// Some(true): the restored state was compared with the snapshot (PCDS_DEV=flip-verify).
    pub verified: Option<bool>,
    #[serde(default)]
    pub got_at: u64,
    #[serde(default)]
    pub ready_at: u64,
    /// Minor page faults of the restore.
    pub faults: Option<u64>,
}

/// The body of a SNAPSHOT frame: what the snapshot in its memfd weighs and how long it took.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotNote {
    pub bytes: Option<u64>,
    pub ms: Option<f64>,
    #[serde(default)]
    pub got_at: u64,
    #[serde(default)]
    pub done_at: u64,
}

/// The first line of a snapshot: its format, and the game tables it is for (its indices only
/// hold on the same game version).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SnapshotHead {
    pub format: u64,
    pub game_version: String,
}

/// The body of CONNECTED: a new client, by SteamID (Steam, verified) or IP (direct); `trusted`:
/// the SteamID it reports is believed (PCDS_DEV=trust-loopback, from this machine).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Connected {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steam_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    #[serde(default)]
    pub trusted: bool,
}

/// An admin request the front passes to the bank (ADMIN; the frame's n numbers it).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AdminCall {
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub body: Value,
}

/// The bank's answer to it (ANSWER; n is the request's): the HTTP status and the JSON text.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AdminAnswer {
    pub status: Option<u64>,
    pub body: Option<String>,
}

/// The rules the front acts on, as the live bank says them (OPS): at its start and whenever
/// they change.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ops {
    pub autosave_minutes: Option<u64>,
    pub auto_update: Option<bool>,
    pub update_check_hours: Option<u64>,
    pub bank_watchdog_seconds: Option<u64>,
}

/// A chat message as an event for the hooks and the API (CHAT, GET /api/chat/events).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatEvent {
    /// 1, 2, 3... since the server started: a gap is events dropped before they were read.
    pub id: u64,
    /// Unix time in milliseconds.
    pub time: u64,
    /// The connection's client id.
    pub client: u64,
    pub name: Option<String>,
    /// Verified (invite-code players) only, as a string: JSON numbers do not hold a SteamID64.
    pub steam_id: Option<String>,
    pub planet: String,
    pub text: String,
    /// With chat.commandPrefix "!": "!tp home" is command "tp" with args ["home"].
    pub command: Option<String>,
    pub args: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text<T: Serialize>(m: &T) -> String {
        String::from_utf8(body(m)).unwrap()
    }

    /// The bytes of each message are what the hand-built JSON of the old code was, key for key.
    #[test]
    fn every_message_is_the_json_it_always_was() {
        let info = BankInfo::new("0.6.2", 3, 5, 2);
        assert_eq!(text(&info), r#"{"version":"0.6.2","protocol":3,"format":5,"tablesFormat":2}"#);
        let hello = Hello { info: info.clone(), game_version: Some("2.103".into()) };
        assert_eq!(text(&hello), r#"{"version":"0.6.2","protocol":3,"format":5,"tablesFormat":2,"gameVersion":"2.103"}"#);
        assert_eq!(text(&Restore { recover: true }), r#"{"recover":true}"#);
        assert_eq!(text(&Restore { recover: false }), r#"{"recover":false}"#);
        let ready = Ready { restore_ms: Some(12.5), verify_ms: Some(0.0), verified: None, got_at: 7, ready_at: 9, faults: Some(100) };
        assert_eq!(text(&ready), r#"{"restoreMs":12.5,"verifyMs":0.0,"verified":null,"gotAt":7,"readyAt":9,"faults":100}"#);
        let note = SnapshotNote { bytes: Some(1024), ms: Some(3.25), got_at: 1, done_at: 2 };
        assert_eq!(text(&note), r#"{"bytes":1024,"ms":3.25,"gotAt":1,"doneAt":2}"#);
        assert_eq!(text(&Connected { steam_id: Some(76561198000000001), ip: None, trusted: false }), r#"{"steamId":76561198000000001,"trusted":false}"#);
        assert_eq!(text(&Connected { ip: Some("10.0.0.2".into()), trusted: true, ..Default::default() }), r#"{"ip":"10.0.0.2","trusted":true}"#);
        let call = AdminCall { method: "GET".into(), path: "/api/ping".into(), body: serde_json::json!({}) };
        assert_eq!(text(&call), r#"{"method":"GET","path":"/api/ping","body":{}}"#);
        assert_eq!(text(&AdminAnswer { status: Some(200), body: Some("{}".into()) }), r#"{"status":200,"body":"{}"}"#);
        let ops = Ops { autosave_minutes: Some(5), auto_update: Some(false), update_check_hours: Some(24), bank_watchdog_seconds: Some(60) };
        assert_eq!(text(&ops), r#"{"autosaveMinutes":5,"autoUpdate":false,"updateCheckHours":24,"bankWatchdogSeconds":60}"#);
        assert_eq!(text(&SnapshotHead { format: 5, game_version: "2.103".into() }), r#"{"format":5,"game_version":"2.103"}"#);
    }

    /// A bank of another release: keys missing are None, keys not known are written back, and
    /// the order of an installed release's bank.json is the one the updater always wrote.
    #[test]
    fn a_message_from_another_release_is_read_leniently_and_kept() {
        let old: BankInfo = parse(br#"{"version":"0.5.0","protocol":3,"format":4}"#).unwrap();
        assert_eq!((old.tables_format, old.tag.clone()), (None, None));
        assert_eq!(text(&old), r#"{"version":"0.5.0","protocol":3,"format":4}"#);
        let later: BankInfo = parse(br#"{"version":"0.9.0","protocol":3,"format":5,"tablesFormat":2,"newThing":{"a":1}}"#).unwrap();
        assert_eq!(text(&later), r#"{"version":"0.9.0","protocol":3,"format":5,"tablesFormat":2,"newThing":{"a":1}}"#);
        let mut installed = BankInfo::new("0.6.2", 3, 5, 2);
        installed.tag = Some("v0.6.2".into());
        installed.sha256 = Some("ab".into());
        installed.installed = Some(1700000000);
        installed.extra.insert("from".into(), "/x/pcds".into());
        assert_eq!(text(&installed), r#"{"version":"0.6.2","protocol":3,"format":5,"tablesFormat":2,"tag":"v0.6.2","sha256":"ab","installed":1700000000,"from":"/x/pcds"}"#);
        assert_eq!(parse::<BankInfo>(text(&installed).as_bytes()).unwrap(), installed);
        let hello: Hello = parse(br#"{"version":"0.5.0","protocol":3,"format":4,"gameVersion":"2.100","other":true}"#).unwrap();
        assert_eq!((hello.game_version.as_deref(), hello.info.extra.len()), (Some("2.100"), 1), "the hello's own field is not an unknown key of the info");
        // Nothing at all is a message with nothing in it; a thing that is not an object is an error.
        assert_eq!(parse::<Ready>(b"{}").unwrap(), Ready::default());
        assert_eq!(parse::<Restore>(b"{}").unwrap(), Restore { recover: false });
        assert!(parse::<Ops>(b"no").is_err() && parse::<Ops>(b"[]").is_err());
        assert_eq!(parse::<SnapshotNote>(b"{}").unwrap().bytes, None);
        assert_eq!(json(&parse::<SnapshotNote>(b"{}").unwrap().bytes), "null");
    }
}
