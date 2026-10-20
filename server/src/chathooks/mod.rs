//! Chat hooks, the game's side: every chat message players send becomes an event for tools outside
//! the game, and what those tools reply goes into the chat. This is the pure half (no thread, no
//! socket, no file): the command prefix parsing, the typed `Event`, the bank's `State` (the events
//! not sent yet, the replies waiting for the tick) and the subscription to the event bus.
//!
//! One path from a game event to the hooks: the game calls `events::chat` (events.rs, as for every
//! event), which hands it to its subscribers in turn: this one first (`wants`, `on_event`: the chat
//! message becomes an `Event` in `State::outbox`), then the plugins. The bank sends the outbox to the
//! front every loop (bank/forward.rs) and the front's hooks (delivery.rs) take it from there.
//!
//! The hooks live in the front (docs/hot-update.md), so they outlive a flip and their event ids go
//! on; their delivery is `delivery`: the front adds each event to a ring of the last KEPT events
//! (`Feed`), one worker thread POSTs it to the rules' chat.webhooks (signed with chat.secret) and
//! runs chat.exec with it, and the admin API's threads read the ring (GET /api/chat/events, a long
//! poll). Replies (a webhook's {"reply": "..."}, the exec program's output, POST /api/chat) come
//! back through a bounded channel to the front, which sends them to the bank; they go to the chat
//! at the next tick, from the server (client 0). Chat text is never a command line: the exec
//! program gets it only as JSON on stdin, with an environment of PATH, HOME and LANG only.

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use pcds_plugin_api::Kind;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::game::Game;
use crate::net::Target;

mod delivery;
// The front's half, until the front owns it (docs/architecture.md, "Chat hooks and the event bus").
pub use delivery::{api, Feed, Hooks};
#[cfg(any(fuzzing, pcds_corpus))]
use delivery::sign;
#[cfg(any(fuzzing, pcds_corpus))]
use serde_json::Value;

/// Replies waiting for the next tick; more are refused.
pub(crate) const REPLIES: usize = 64;
/// The longest reply, in characters.
pub(crate) const MAX_REPLY: usize = 500;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
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

/// The game's side, in the bank: events for the front, and replies from it for the chat.
#[derive(Default, Serialize, Deserialize)]
pub struct State {
    /// Events not sent to the front yet (the bank sends them every loop, and before a snapshot).
    #[serde(skip)]
    pub outbox: Vec<Event>,
    /// Replies for the chat at the next tick.
    pub replies: VecDeque<String>,
    /// The chat rules changed: the bank sends them to the front.
    #[serde(skip)]
    pub changed: bool,
}

/// A reply from the front, into the chat at the next tick (at most REPLIES wait).
pub fn say(game: &mut Game, text: &str) {
    let text: String = text.trim().chars().take(MAX_REPLY).collect();
    if text.is_empty() {
        return;
    }
    if game.ext.chat.replies.len() >= REPLIES {
        eprintln!("chat: {REPLIES} replies are waiting for the chat; one more was dropped");
        return;
    }
    game.ext.chat.replies.push_back(text);
}

/// While the world stands still (world.pauseWhenEmpty) nobody is in the game to read a reply:
/// they are dropped, not kept for whoever joins next.
pub fn drop_replies(game: &mut Game) {
    let n = game.ext.chat.replies.len();
    if n > 0 {
        game.ext.chat.replies.clear();
        println!("chat: nobody is in the game; {n} message(s) to the chat dropped");
    }
}

/// "!tp home" with prefix "!": ("tp", ["home"]). None: no prefix, or not a command.
pub(crate) fn command(prefix: Option<&str>, text: &str) -> Option<(String, Vec<String>)> {
    let rest = text.trim_start().strip_prefix(prefix.filter(|p| !p.is_empty())?)?;
    if rest.starts_with(char::is_whitespace) {
        return None;
    }
    let mut words = rest.split_whitespace();
    let name = words.next()?.to_string();
    Some((name, words.map(str::to_string).collect()))
}

/// ChatHandler.SendChatMessageServerRpc: the message as an event. Never waits on a hook.
fn chat(game: &mut Game, client: u64, text: &str) {
    let c = game.clients.get(&client);
    let (command, args) = command(game.rules.chat.command_prefix.as_deref(), text).unzip();
    let event = Event {
        id: 0,
        time: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0),
        client,
        name: c.and_then(|c| c.ready.as_ref()).map(|s| s.name.clone()),
        steam_id: c.and_then(|c| c.verified_steam_id).map(|id| id.to_string()),
        planet: game.planet_id().to_string(),
        text: text.to_string(),
        command,
        args: args.unwrap_or_default(),
    };
    game.ext.chat.outbox.push(event);
}

/// The replies that came in, to everyone, as the server's messages are.
pub fn tick(game: &mut Game) {
    while let Some(text) = game.ext.chat.replies.pop_front() {
        game.net.rpc_scene(Target::All, "ChatHandler.ReceiveChatMessageClientRpc", &[json!(0), json!(text)]);
    }
}

/// The rules' chat section changed: the bank tells the front's hooks.
pub fn configure(game: &mut Game) {
    game.ext.chat.changed = true;
}

/// Whether the chat hooks take events of this kind from the event bus (events.rs): chat messages,
/// always, whether a plugin listens or not.
pub fn wants(kind: Kind) -> bool {
    kind == Kind::Chat
}

/// The chat hooks as a subscriber of the event bus: events.rs hands an event of a kind `wants`
/// accepts. Never waits on a hook: the event is queued for the front.
pub fn on_event(game: &mut Game, event: &pcds_plugin_api::Event<'_>) {
    if let pcds_plugin_api::Event::Chat { client, text, .. } = event {
        chat(game, *client, text);
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::game::tests::{join, save};
    use crate::game::{test_game, Args};
    use crate::rules::Chat;
    use serde_json::json;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpStream;
    use std::time::{Duration, Instant};

    pub fn send_chat(g: &mut Game, client: u64, text: &str) {
        assert!(crate::handlers::rpc(g, client, 0, 0, "ChatHandler.SendChatMessageServerRpc", &Args(vec![json!(text)])));
    }

    /// The chat messages queued for the clients: (from, text).
    pub fn chat_sent(g: &Game) -> Vec<(u64, String)> {
        g.net.out.iter().filter(|(_, kind, _)| *kind == crate::protocol::CLIENT_RPC).filter_map(|(_, _, body)| {
            let mut r = crate::wire::Reader::new(body);
            r.packed_u64()?;
            r.packed_u16()?;
            let method = r.packed_u32()?;
            (g.net.spec.get(method)?.name == "ChatHandler.ReceiveChatMessageClientRpc").then_some(())?;
            let args = g.net.spec.decode(method, r.rest())?;
            Some((args[0].as_u64()?, args[1].as_str()?.to_string()))
        }).collect()
    }

    /// What the bank and the front do with the chat each loop: events to the feed, a change of
    /// the chat rules to the hooks, the replies back into the game.
    pub fn pump(g: &mut Game, h: &mut Hooks) {
        g.ext.chat.outbox.drain(..).for_each(|e| h.feed.push(e));
        if std::mem::take(&mut g.ext.chat.changed) {
            h.configure(g.rules.chat.clone());
        }
        for reply in h.replies() {
            say(g, &reply);
        }
    }

    /// Ticks until a chat message from the server is queued (5 s at most).
    pub fn wait_for_reply(g: &mut Game, h: &mut Hooks) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            pump(g, h);
            tick(g);
            let replies: Vec<String> = chat_sent(g).into_iter().filter(|(from, _)| *from == 0).map(|(_, t)| t).collect();
            if !replies.is_empty() {
                return replies;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Vec::new()
    }

    /// One HTTP request read off a test listener: (headers, lowercase names; body).
    pub fn read_request(stream: &mut TcpStream) -> (Vec<(String, String)>, String) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let (mut headers, mut line) = (Vec::new(), String::new());
        reader.read_line(&mut line).unwrap();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap() == 0 || line.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        let length = headers.iter().find(|(k, _)| k == "content-length").map_or(0, |(_, v)| v.parse().unwrap());
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        (headers, String::from_utf8(body).unwrap())
    }

    pub fn answer(stream: &mut TcpStream, body: &str) {
        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    }

    /// As the admin API changes them: checked, then live in the front's hooks.
    pub fn hooked(g: &mut Game, chat: Chat) -> Hooks {
        let rules = crate::rules::Rules { chat, ..g.rules.clone() };
        crate::rules::set(g, rules).unwrap();
        let mut h = Hooks::default();
        pump(g, &mut h);
        h
    }

    #[test]
    fn a_chat_rpc_becomes_an_event_and_still_reaches_everyone() {
        let Some(mut g) = test_game(save()) else { return };
        join(&mut g, 1, "Ann");
        g.rules.chat.command_prefix = Some("!".into());
        g.net.out.clear();
        send_chat(&mut g, 1, "hello there");
        send_chat(&mut g, 1, "!tp home base");
        let mut h = Hooks::default();
        pump(&mut g, &mut h);
        let (events, last) = h.feed.after(0, Duration::ZERO);
        assert_eq!(last, 2);
        let e = &events[0];
        assert_eq!((e.id, e.client, e.name.as_deref(), e.steam_id.as_deref(), e.planet.as_str(), e.text.as_str()), (1, 1, Some("Ann"), None, "Prime", "hello there"));
        assert!(e.command.is_none() && e.time > 1_700_000_000_000);
        assert_eq!((events[1].command.as_deref(), events[1].args.clone()), (Some("tp"), vec!["home".to_string(), "base".into()]));
        let json = serde_json::to_value(e).unwrap();
        assert_eq!(json.as_object().unwrap().keys().collect::<Vec<_>>(), ["id", "time", "client", "name", "steamId", "planet", "text", "command", "args"]);
        assert_eq!(chat_sent(&g), [(1, "hello there".to_string()), (1, "!tp home base".into())], "the game's broadcast is unchanged");
    }

    /// The chat hooks are a subscriber of the event bus like the plugins: they want chat messages and
    /// nothing else, whether a plugin listens or not, and an event that goes through the bus
    /// (events.rs) is queued for the front, once, with the same fields as the RPC's.
    #[test]
    fn chat_hooks_are_a_subscriber_of_the_event_bus() {
        for kind in Kind::ALL {
            assert_eq!(wants(kind), kind == Kind::Chat, "{kind:?}");
        }
        let Some(mut g) = test_game(save()) else { return };
        join(&mut g, 1, "Ann");
        g.rules.chat.command_prefix = Some("!".into());
        assert_eq!(g.ext.plugins.interest, 0, "no plugin listens");
        assert!(crate::events::wants(&g, Kind::Chat) && !crate::events::wants(&g, Kind::Tick));
        crate::events::chat(&mut g, 1, "!kit iron");
        crate::events::chat(&mut g, 9, "from nobody");
        let queued: Vec<_> = g.ext.chat.outbox.iter().map(|e| (e.client, e.name.clone(), e.text.clone(), e.command.clone(), e.args.clone())).collect();
        assert_eq!(queued, [(1, Some("Ann".to_string()), "!kit iron".to_string(), Some("kit".to_string()), vec!["iron".to_string()]), (9, None, "from nobody".to_string(), None, vec![])]);
        // Other kinds of event never reach the hooks.
        let before = g.ext.chat.outbox.len();
        crate::events::player_left(&mut g, 1, "Ann");
        crate::events::world_saved(&mut g, true);
        assert_eq!(g.ext.chat.outbox.len(), before);
    }

    #[test]
    fn command_prefixes_parse() {
        let words = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(command(Some("!"), "!tp  home  base "), Some(("tp".into(), words(&["home", "base"]))));
        assert_eq!(command(Some("!"), "  !help"), Some(("help".into(), vec![])));
        assert_eq!(command(Some("//"), "//kit iron"), Some(("kit".into(), words(&["iron"]))));
        for (prefix, text) in [(Some("!"), "hello !tp"), (Some("!"), "!"), (Some("!"), "! tp"), (None, "!tp"), (Some(""), "tp")] {
            assert_eq!(command(prefix, text), None, "{prefix:?} {text:?}");
        }
    }

    /// While nobody is in the game (world.pauseWhenEmpty), replies are dropped, not kept for
    /// whoever joins next.
    #[test]
    fn replies_are_dropped_while_the_world_stands_still() {
        let Some(mut g) = test_game(save()) else { return };
        g.rules.world.pause_when_empty = true;
        for n in 0..(REPLIES + 10) {
            say(&mut g, &format!("reply {n}"));
        }
        assert_eq!(g.ext.chat.replies.len(), REPLIES, "bounded");
        g.tick();
        assert!(g.ext.chat.replies.is_empty());
    }
}

#[cfg(any(fuzzing, pcds_corpus))]
#[path = "../../fuzz/targets/chathooks.rs"]
pub mod fuzz;
