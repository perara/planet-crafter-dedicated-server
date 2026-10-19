//! The event stream for out-of-process plugins (docs/plugins.md, "Out-of-process integrations"):
//! a ring of the last events the integrations asked for, read with POST /api/plugins/events.
//! It is in the snapshot, so ids go on across a hot update, and the `epoch` (when the ring was
//! made) tells a reader that the server restarted and the ids began again.
//!
//! Nothing is recorded until someone asks: the first poll names the kinds it wants, and the ring
//! records them for the next minute after each poll. With nobody polling it costs nothing.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use pcds_plugin_api::{Event, Kind};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Events kept; a reader that falls further behind misses some (`dropped` counts them).
const KEPT: usize = 512;
/// The ring records for this long after the last poll.
const KEEP_ASKING: Duration = Duration::from_secs(60);
/// Kinds an integration may ask for: not the per-tick and per-RPC ones, and not commands (those are
/// a plugin's own: a command's chat message is a `chat` event here).
pub const STREAMABLE: u32 = Kind::PlayerJoin.bit() | Kind::PlayerLeave.bit() | Kind::Chat.bit() | Kind::ObjectPlaced.bit() | Kind::ObjectDestroyed.bit() | Kind::WorldSaved.bit() | Kind::RuleChange.bit();
/// What a poll that names no kinds gets.
pub const DEFAULT_KINDS: u32 = Kind::PlayerJoin.bit() | Kind::PlayerLeave.bit() | Kind::Chat.bit() | Kind::WorldSaved.bit() | Kind::RuleChange.bit();

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ring {
    /// Unix milliseconds when the ring began.
    pub epoch: u64,
    next: u64,
    #[serde(default)]
    events: VecDeque<Value>,
    #[serde(default)]
    dropped: u64,
    #[serde(skip)]
    wanted: u32,
    #[serde(skip)]
    polled: Option<Instant>,
}

pub fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl Default for Ring {
    fn default() -> Self {
        Ring { epoch: unix_ms(), next: 1, events: VecDeque::new(), dropped: 0, wanted: 0, polled: None }
    }
}

impl Ring {
    /// The kinds being recorded now (none once nobody has polled for a minute).
    pub fn wanted(&mut self) -> u32 {
        if self.polled.is_some_and(|t| t.elapsed() > KEEP_ASKING) {
            self.wanted = 0;
            self.polled = None;
        }
        self.wanted
    }

    /// A poll for `kinds`: they are recorded from now on.
    pub fn ask(&mut self, kinds: u32) {
        self.wanted |= kinds & STREAMABLE;
        self.polled = Some(Instant::now());
    }

    pub fn push(&mut self, event: &Event<'_>) {
        let Some(mut v) = event_json(event) else { return };
        v["id"] = self.next.into();
        v["time"] = unix_ms().into();
        v["type"] = event.kind().name().into();
        self.next += 1;
        if self.events.len() == KEPT {
            self.events.pop_front();
            self.dropped += 1;
        }
        self.events.push_back(v);
    }

    /// The events after `after` of these kinds (at most `limit`), and the last id in the ring.
    pub fn read(&self, after: u64, kinds: u32, limit: usize) -> Value {
        let events: Vec<&Value> = self
            .events
            .iter()
            .filter(|e| e["id"].as_u64().is_some_and(|id| id > after))
            .filter(|e| e["type"].as_str().and_then(Kind::from_name).is_some_and(|k| kinds & k.bit() != 0))
            .take(limit)
            .collect();
        json!({"epoch": self.epoch, "events": events, "last": self.next - 1, "dropped": self.dropped})
    }
}

/// The fields of a streamed event as JSON, in camelCase as the chat events are.
pub fn event_json(event: &Event<'_>) -> Option<Value> {
    let id = |s: &str| if s.is_empty() { Value::Null } else { Value::from(s) };
    Some(match event {
        Event::Chat { client, name, text } => json!({"client": client, "name": name, "text": text}),
        Event::PlayerJoin { client, name, steam_id } => json!({"client": client, "name": name, "steamId": id(steam_id)}),
        Event::PlayerLeave { client, name } => json!({"client": client, "name": name}),
        Event::ObjectPlaced { id, group, position } | Event::ObjectDestroyed { id, group, position } => json!({"object": id, "group": group, "position": position}),
        Event::WorldSaved { game_ms, ok } => json!({"gameSeconds": *game_ms as f64 / 1000.0, "ok": ok}),
        Event::RuleChange { changes_json } => json!({"changes": serde_json::from_str::<Value>(changes_json).unwrap_or(Value::Null)}),
        Event::Tick { .. } | Event::Command { .. } | Event::RpcBefore { .. } | Event::RpcAfter { .. } => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_numbers_filters_and_forgets() {
        let mut ring = Ring::default();
        assert_eq!(ring.wanted(), 0);
        ring.ask(Kind::Chat.bit() | Kind::Tick.bit());
        assert_eq!(ring.wanted(), Kind::Chat.bit(), "per-tick events cannot be streamed");
        ring.push(&Event::Chat { client: 3, name: "Ann", text: "hi" });
        ring.push(&Event::PlayerJoin { client: 4, name: "Bo", steam_id: "" });
        ring.push(&Event::Tick { game_ms: 0, tick: 0, players: 0, since_ms: 0 });
        let all = ring.read(0, DEFAULT_KINDS, 100);
        assert_eq!((all["events"].as_array().unwrap().len(), all["last"].as_u64()), (2, Some(2)));
        assert_eq!(all["events"][1]["steamId"], Value::Null);
        let chat = ring.read(0, Kind::Chat.bit(), 100);
        assert_eq!(chat["events"].as_array().unwrap().len(), 1);
        assert_eq!(chat["events"][0]["text"], "hi");
        assert_eq!(ring.read(1, DEFAULT_KINDS, 100)["events"][0]["type"], "player_join");
        for _ in 0..KEPT + 5 {
            ring.push(&Event::Chat { client: 1, name: "x", text: "y" });
        }
        let r = ring.read(0, DEFAULT_KINDS, usize::MAX);
        assert_eq!((r["events"].as_array().unwrap().len(), r["dropped"].as_u64()), (KEPT, Some(7)));
        // Ids and the epoch survive the snapshot.
        let back: Ring = serde_json::from_str(&serde_json::to_string(&ring).unwrap()).unwrap();
        assert_eq!((back.epoch, back.read(0, DEFAULT_KINDS, 1)["last"].as_u64()), (ring.epoch, r["last"].as_u64()));
    }
}
