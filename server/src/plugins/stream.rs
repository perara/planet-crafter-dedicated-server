//! The event stream for out-of-process plugins (docs/plugins.md, "Out-of-process integrations"):
//! a ring of the last events the integrations asked for, read with POST /api/plugins/events.
//! It is in the snapshot, so ids go on across a hot update, and the `epoch` (when the ring was
//! made) tells a reader that the server restarted and the ids began again.
//!
//! Nothing is recorded until someone asks. A consumer registers by polling (it names itself and the
//! kinds it wants), is recorded for the minute after each poll, and leaves by saying so
//! (`unregister`) or by being silent for a minute; each arrival and each departure is logged once.
//! The ring records the union of what the registered consumers want, so with none it costs nothing.

use std::collections::{BTreeMap, VecDeque};
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
/// Consumers registered at once (a real setup has a handful of bridges).
pub const MAX_CONSUMERS: usize = 16;
/// What a poll that names no kinds gets.
pub const DEFAULT_KINDS: u32 = Kind::PlayerJoin.bit() | Kind::PlayerLeave.bit() | Kind::Chat.bit() | Kind::WorldSaved.bit() | Kind::RuleChange.bit();

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ring {
    /// Unix milliseconds when the ring began.
    pub epoch: u64,
    next: u64,
    events: VecDeque<Value>,
    dropped: u64,
    /// Who is registered (not in the snapshot: a consumer registers again with its next poll).
    #[serde(skip)]
    consumers: BTreeMap<String, Consumer>,
}

#[derive(Debug, Clone)]
struct Consumer {
    kinds: u32,
    last: Instant,
    polls: u64,
    after: u64,
}

pub fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl Default for Ring {
    fn default() -> Self {
        Ring { epoch: unix_ms(), next: 1, events: VecDeque::new(), dropped: 0, consumers: BTreeMap::new() }
    }
}

impl Ring {
    /// The kinds being recorded now: what the registered consumers want. A consumer that has not
    /// polled for a minute is gone (said once in the log).
    pub fn wanted(&mut self) -> u32 {
        if !self.consumers.is_empty() {
            let gone: Vec<String> = self.consumers.iter().filter(|(_, c)| c.last.elapsed() > KEEP_ASKING).map(|(n, _)| n.clone()).collect();
            for name in gone {
                self.consumers.remove(&name);
                println!("stream consumer {name}: gone (no poll for {} s)", KEEP_ASKING.as_secs());
            }
        }
        self.consumers.values().fold(0, |m, c| m | c.kinds)
    }

    /// A poll by `name` for `kinds`, having read up to `after`: it is registered (or its wishes
    /// updated). False when `name` is new and `MAX_CONSUMERS` others are registered: a token
    /// that names itself anew at every poll made a log line and an entry each.
    pub fn ask(&mut self, name: &str, kinds: u32, after: u64) -> bool {
        self.wanted();
        if !self.consumers.contains_key(name) && self.consumers.len() >= MAX_CONSUMERS {
            return false;
        }
        let kinds = kinds & STREAMABLE;
        let c = self.consumers.entry(name.to_string()).or_insert_with(|| {
            println!("stream consumer {name}: registered");
            Consumer { kinds, last: Instant::now(), polls: 0, after }
        });
        (c.kinds, c.last, c.after) = (kinds, Instant::now(), after);
        c.polls += 1;
        true
    }

    /// `name` leaves: what only it wanted is no longer recorded. False if it was not there.
    pub fn unregister(&mut self, name: &str) -> bool {
        let there = self.consumers.remove(name).is_some();
        if there {
            println!("stream consumer {name}: deregistered");
        }
        there
    }

    /// The registered consumers, for the listings.
    pub fn consumers_json(&self) -> Vec<Value> {
        self.consumers.iter().map(|(name, c)| json!({"name": name, "kinds": Kind::ALL.iter().filter(|k| c.kinds & k.bit() != 0).map(|k| k.name()).collect::<Vec<_>>(), "lastPollSecondsAgo": c.last.elapsed().as_secs(), "polls": c.polls, "after": c.after})).collect()
    }

    /// The id of the newest event (0: none yet).
    pub fn last(&self) -> u64 {
        self.next - 1
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
        // Ids as text: a SteamID64 is more than a JavaScript number holds exactly.
        Event::Chat { client, name, text, player_id } => json!({"client": client, "name": name, "text": text, "playerId": player_id.to_string()}),
        Event::PlayerJoin { client, name, steam_id, player_id } => json!({"client": client, "name": name, "steamId": id(steam_id), "playerId": player_id.to_string()}),
        Event::PlayerLeave { client, name, player_id } => json!({"client": client, "name": name, "playerId": player_id.to_string()}),
        Event::ObjectPlaced { id, group, position, client } | Event::ObjectDestroyed { id, group, position, client } => json!({"object": id, "group": group, "position": position, "client": (*client != 0).then_some(client)}),
        Event::WorldSaved { game_ms, ok } => json!({"gameSeconds": *game_ms as f64 / 1000.0, "ok": ok}),
        Event::RuleChange { changes_json } => json!({"changes": serde_json::from_str::<Value>(changes_json).unwrap_or(Value::Null)}),
        Event::Tick { .. } | Event::Command { .. } | Event::RpcBefore { .. } | Event::RpcAfter { .. } | Event::HttpDone { .. } | Event::Timer { .. } | Event::Flip { .. } => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_numbers_filters_and_forgets() {
        let mut ring = Ring::default();
        assert_eq!(ring.wanted(), 0);
        ring.ask("a", Kind::Chat.bit() | Kind::Tick.bit(), 0);
        assert_eq!(ring.wanted(), Kind::Chat.bit(), "per-tick events cannot be streamed");
        ring.push(&Event::Chat { client: 3, name: "Ann", text: "hi", player_id: 9 });
        ring.push(&Event::PlayerJoin { client: 4, name: "Bo", steam_id: "", player_id: 10 });
        ring.push(&Event::Tick { game_ms: 0, tick: 0, players: 0, since_ms: 0, unix_ms: 0 });
        let all = ring.read(0, DEFAULT_KINDS, 100);
        assert_eq!((all["events"].as_array().unwrap().len(), all["last"].as_u64()), (2, Some(2)));
        assert_eq!(all["events"][1]["steamId"], Value::Null);
        let chat = ring.read(0, Kind::Chat.bit(), 100);
        assert_eq!(chat["events"].as_array().unwrap().len(), 1);
        assert_eq!(chat["events"][0]["text"], "hi");
        assert_eq!(ring.read(1, DEFAULT_KINDS, 100)["events"][0]["type"], "player_join");
        for _ in 0..KEPT + 5 {
            ring.push(&Event::Chat { client: 1, name: "x", text: "y", player_id: 1 });
        }
        let r = ring.read(0, DEFAULT_KINDS, usize::MAX);
        assert_eq!((r["events"].as_array().unwrap().len(), r["dropped"].as_u64()), (KEPT, Some(7)));
        // Ids and the epoch survive the snapshot; the consumers do not (they register again).
        let back: Ring = serde_json::from_str(&serde_json::to_string(&ring).unwrap()).unwrap();
        assert_eq!((back.epoch, back.read(0, DEFAULT_KINDS, 1)["last"].as_u64()), (ring.epoch, r["last"].as_u64()));
        assert!(back.consumers_json().is_empty());
    }

    #[test]
    fn consumers_register_leave_and_are_forgotten_when_silent() {
        let mut ring = Ring::default();
        ring.ask("relay", Kind::Chat.bit(), 0);
        ring.ask("metrics", Kind::PlayerJoin.bit() | Kind::PlayerLeave.bit(), 5);
        assert_eq!(ring.wanted(), Kind::Chat.bit() | Kind::PlayerJoin.bit() | Kind::PlayerLeave.bit());
        assert_eq!(ring.consumers_json().len(), 2);
        // One leaves: what only it wanted stops being recorded; leaving twice is told.
        assert!(ring.unregister("relay") && !ring.unregister("relay"));
        assert_eq!(ring.wanted(), Kind::PlayerJoin.bit() | Kind::PlayerLeave.bit());
        // A minute of silence and the other is gone too.
        ring.consumers.get_mut("metrics").unwrap().last = Instant::now() - KEEP_ASKING - Duration::from_secs(1);
        assert_eq!(ring.wanted(), 0);
        assert!(ring.consumers_json().is_empty());
        ring.push(&Event::Chat { client: 1, name: "x", text: "y", player_id: 1 });
        // The ring still numbers what was pushed (it is cheap), but nothing asks for it.
        assert_eq!(ring.read(0, DEFAULT_KINDS, 10)["last"], 1);
    }

    /// A token that names itself anew at every poll would make an entry and a log line each time.
    #[test]
    fn the_consumers_are_few() {
        let mut ring = Ring::default();
        for i in 0..MAX_CONSUMERS {
            assert!(ring.ask(&format!("c{i}"), Kind::Chat.bit(), 0));
        }
        assert!(!ring.ask("one-more", Kind::Chat.bit(), 0), "a new name over the limit is refused");
        assert!(ring.ask("c3", Kind::Chat.bit(), 7), "one that is registered goes on");
        assert_eq!(ring.consumers_json().len(), MAX_CONSUMERS);
        // A silent one gives its place up.
        ring.consumers.get_mut("c0").unwrap().last = Instant::now() - KEEP_ASKING - Duration::from_secs(1);
        assert!(ring.ask("one-more", Kind::Chat.bit(), 0));
    }
}
