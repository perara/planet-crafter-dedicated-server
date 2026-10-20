//! Chat hooks, the game's side: every chat message players send becomes an event for tools outside
//! the game, and what those tools reply goes into the chat. This is the pure half (no thread, no
//! socket, no file): the command prefix parsing, the typed event (msg.rs `ChatEvent`), the bank's
//! `State` (the events not sent yet, the replies waiting for the tick) and the subscription to the
//! event bus.
//!
//! One path from a game event to the hooks: the game calls `events::chat` (events.rs, as for every
//! event), which hands it to its subscribers in turn: this one first (`wants`, `on_event`: the chat
//! message becomes an `Event` in `State::outbox`), then the plugins. The bank sends the outbox to the
//! front every loop (bank/forward.rs) and the front's hooks (front/chat_hooks.rs) take it from there:
//! the feed, the worker that delivers to the webhooks and the exec program, and the HTTP endpoints.
//! The hooks live in the front (docs/hot-update.md), so they outlive a flip and their event ids go
//! on. Replies (a webhook's {"reply": "..."}, the exec program's output, POST /api/chat) come back
//! to the bank as SAY and go to the chat at the next tick, from the server (client 0).

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use pcds_plugin_api::Kind;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::game::Game;
use crate::net::Target;

/// A chat message as an event (msg.rs: it crosses from the bank to the front).
pub use crate::msg::ChatEvent as Event;
use crate::msg::{CHAT_MAX_REPLY as MAX_REPLY, CHAT_REPLIES as REPLIES};

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
        warn!("chat: {REPLIES} replies are waiting for the chat; one more was dropped");
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
        info!("chat: nobody is in the game; {n} message(s) to the chat dropped");
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
mod tests {
    use super::*;
    use crate::game::test_game;
    use crate::game::tests::{join, save};

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
