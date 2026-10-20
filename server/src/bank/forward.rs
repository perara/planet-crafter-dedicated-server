//! What the bank tells the front besides the players' bytes: the chat events and the chat rules the
//! front's hooks act on (chat hooks deliver from the front, so they outlive a flip), and the rules
//! the front acts on itself (autosave, updates, the watchdog).

use serde_json::{json, Value};

use crate::game::Game;
use crate::ipc::{self, Link};

/// The chat events for the front's hooks, and the hooks' settings when they changed.
pub(super) fn forward_chat(game: &mut Game, link: &mut Link) {
    for event in game.ext.chat.outbox.drain(..) {
        link.send(ipc::CHAT, 0, serde_json::to_string(&event).unwrap_or_default().as_bytes());
    }
    if std::mem::take(&mut game.ext.chat.changed) {
        link.send(ipc::CHATCFG, 0, serde_json::to_string(&game.rules.chat).unwrap_or_default().as_bytes());
    }
}

/// The rules the front acts on (autosave, updates): sent at the start and whenever they change.
pub(super) fn forward_ops(game: &Game, link: &mut Link, last: &mut Value) {
    let (save, update) = (&game.rules.save, &game.rules.update);
    let ops = json!({"autosaveMinutes": save.autosave_minutes, "autoUpdate": update.auto, "updateCheckHours": update.check_hours, "bankWatchdogSeconds": game.rules.limits.bank_watchdog_seconds});
    if *last != ops {
        link.send(ipc::OPS, 0, ops.to_string().as_bytes());
        *last = ops;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game;
    use std::os::unix::net::UnixStream;

    /// The rules the front acts on go to it once, and again only when they change.
    #[test]
    fn the_rules_the_front_acts_on_are_sent_once_and_when_they_change() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let (a, b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        let (mut front, mut bank) = (Link::new(a), Link::new(b));
        let mut last = Value::Null;
        forward_ops(&game, &mut bank, &mut last);
        forward_ops(&game, &mut bank, &mut last);
        game.rules.save.autosave_minutes = 7;
        game.rules.update.auto = true;
        forward_ops(&game, &mut bank, &mut last);
        bank.send(ipc::GONE, 0, &[]);
        let mut bodies = Vec::new();
        while bodies.last() != Some(&(ipc::GONE, String::new())) {
            assert!(front.fill(usize::MAX));
            while let Some(f) = front.next() {
                bodies.push((f.kind, String::from_utf8(f.body).unwrap()));
            }
        }
        assert_eq!(
            bodies,
            [
                (ipc::OPS, r#"{"autosaveMinutes":5,"autoUpdate":false,"updateCheckHours":24,"bankWatchdogSeconds":60}"#.to_string()),
                (ipc::OPS, r#"{"autosaveMinutes":7,"autoUpdate":true,"updateCheckHours":24,"bankWatchdogSeconds":60}"#.to_string()),
                (ipc::GONE, String::new())
            ]
        );
    }
}
