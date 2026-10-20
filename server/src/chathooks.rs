//! Chat hooks, the game's side: every chat message players send becomes an event for tools outside
//! the game. The game (a bank) only queues the event (`State::outbox`), which the bank sends to
//! the front, and plays the replies that come back; the front keeps the feed, the worker that
//! delivers to webhooks and exec, and the HTTP endpoints (front/chat_hooks.rs, docs/hot-update.md).
//! Replies (a webhook's {"reply": "..."}, the exec program's output, POST /api/chat) come to the
//! bank as SAY and go to the chat at the next tick, from the server (client 0).

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::game::Game;
use crate::msg::{CHAT_MAX_REPLY as MAX_REPLY, CHAT_REPLIES as REPLIES};
use crate::net::Target;

/// A chat message as an event (msg.rs: it crosses from the bank to the front).
pub use crate::msg::ChatEvent as Event;

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
pub fn chat(game: &mut Game, client: u64, text: &str) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::test_game;
    use crate::game::tests::save;

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
#[path = "../fuzz/targets/chathooks.rs"]
pub mod fuzz;
