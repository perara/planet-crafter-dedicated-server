//! The update history: one entry for each update, check, flip, rollback or crash rollback the
//! front has dealt with (GET /api/update "history" and "last"; `pcds upgrade` waits for its
//! number). The last 20 are kept in memory; `Updates::record` is the one place that adds to them.
//! The keys of the JSON are the fields below, in the order declared.

use serde::Serialize;

/// One entry. What it says besides the outcome depends on what happened: a flip has its
/// numbers (`flip`), a check says `check`, a rollback the front made for crashes in an earlier
/// life of the server says `rolledBack`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct History {
    #[serde(flatten)]
    pub flip: Option<FlipNote>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check: Option<bool>,
    /// The request's number, 0 for one that was not asked for.
    pub seq: u64,
    pub ok: bool,
    pub message: String,
    /// Unix seconds.
    pub at: Option<u64>,
    #[serde(flatten)]
    pub rolled: Option<RolledBack>,
}

impl History {
    /// An outcome, now.
    pub fn new(seq: u64, ok: bool, message: impl Into<String>) -> History {
        History { flip: None, check: None, seq, ok, message: message.into(), at: Some(super::now_secs()), rolled: None }
    }

    /// The outcome of a flip, with its numbers.
    pub fn flipped(seq: u64, message: String, note: FlipNote) -> History {
        History { flip: Some(note), ..History::new(seq, true, message) }
    }

    /// The outcome of a check only.
    pub fn checked(seq: u64, message: String) -> History {
        History { check: Some(true), ..History::new(seq, true, message) }
    }

    /// A rollback the front made by itself, noted in the banks' folder (at: when it was made).
    pub fn rolled_back(message: String, at: Option<u64>, bank: Option<String>, tag: Option<String>) -> History {
        History { at, rolled: Some(RolledBack { rolled_back: true, bank, tag }), ..History::new(0, false, message) }
    }
}

/// What a flip came to, for the history.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FlipNote {
    pub from: String,
    pub to: String,
    /// How long the world stood, ms.
    pub stood_ms: f64,
    pub snapshot_bytes: Option<u64>,
    pub snapshot_ms: Option<f64>,
    pub restore_ms: Option<f64>,
    /// Client frames held meanwhile.
    pub held: usize,
    pub verified: Option<bool>,
    /// The new bank's version.
    pub version: Option<String>,
    pub timeline: Timeline,
    pub restore_faults: Option<u64>,
}

/// Where the stand went, in ms on the monotonic clock all three processes read: the HANDOFF to
/// the old bank, its snapshot, the SNAPSHOT to the front, the front's relay of the memfd, the
/// RESTORE to the new bank, its restore, the READY to the front, the held frames out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Timeline {
    pub handoff_ms: f64,
    pub snapshot_ms: f64,
    pub snapshot_to_front_ms: f64,
    pub relay_ms: f64,
    pub restore_to_bank_ms: f64,
    pub restore_ms: f64,
    pub ready_to_front_ms: f64,
    pub release_ms: f64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RolledBack {
    pub rolled_back: bool,
    pub bank: Option<String>,
    pub tag: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(h: &History) -> String {
        let mut v = serde_json::to_value(h).unwrap();
        // The time is the one thing that is not fixed.
        v["at"] = 0.into();
        v.to_string()
    }

    /// Each kind of entry has the keys, in the order, the front always wrote them in.
    #[test]
    fn each_entry_has_the_keys_the_history_always_had() {
        assert_eq!(text(&History::new(3, false, "no")), r#"{"seq":3,"ok":false,"message":"no","at":0}"#);
        assert_eq!(text(&History::checked(4, "up".into())), r#"{"check":true,"seq":4,"ok":true,"message":"up","at":0}"#);
        let note = FlipNote {
            from: "a".into(),
            to: "b".into(),
            stood_ms: 12.5,
            snapshot_bytes: Some(10),
            snapshot_ms: None,
            restore_ms: Some(2.0),
            held: 3,
            verified: None,
            version: Some("0.6.2".into()),
            timeline: Timeline::default(),
            restore_faults: Some(7),
        };
        assert_eq!(
            text(&History::flipped(5, "flip".into(), note)),
            r#"{"from":"a","to":"b","stoodMs":12.5,"snapshotBytes":10,"snapshotMs":null,"restoreMs":2.0,"held":3,"verified":null,"version":"0.6.2","timeline":{"handoffMs":0.0,"snapshotMs":0.0,"snapshotToFrontMs":0.0,"relayMs":0.0,"restoreToBankMs":0.0,"restoreMs":0.0,"readyToFrontMs":0.0,"releaseMs":0.0},"restoreFaults":7,"seq":5,"ok":true,"message":"flip","at":0}"#
        );
        let rolled = History::rolled_back("gave up".into(), Some(9), Some("b".into()), None);
        let v = serde_json::to_value(&rolled).unwrap().to_string();
        assert_eq!(v, r#"{"seq":0,"ok":false,"message":"gave up","at":9,"rolledBack":true,"bank":"b","tag":null}"#);
    }
}
