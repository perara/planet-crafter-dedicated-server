//! Applies what a plugin queued, after its call returned. Each action goes through the code the
//! admin API and the game's own handlers use (`chathooks::say`, `Game::kick`, `status::teleport`,
//! `rules::set`), checked again here for what has changed since the plugin asked (the player may
//! be gone). A plugin never writes the world: it asks, and the server decides.

use serde_json::{Value, json};

use super::ctx::Action;
use crate::game::Game;
use crate::net::Target;

/// The rule sections a plugin with `rules.write` may change. Not `chat` (webhooks and the exec
/// program run code outside the sandbox), `backup` (keys and endpoints), `access` (who may play) or
/// `players` (it forgets saved players). Inside `world`, not `dlcPlanets`: which DLC planets are
/// owned is the operator's statement, and no plugin makes it.
pub const PLUGIN_RULES: [&str; 7] = ["meteo", "gauges", "speed", "death", "loot", "limits", "world"];

pub fn rules_patch_allowed(patch: &Value) -> Result<(), String> {
    for section in patch.as_object().into_iter().flat_map(|o| o.keys()) {
        if !PLUGIN_RULES.contains(&section.as_str()) {
            return Err(format!("{section}: plugins may change only the {} rules", PLUGIN_RULES.join(", ")));
        }
    }
    if patch["world"].as_object().is_some_and(|w| w.contains_key("dlcPlanets")) {
        return Err("world.dlcPlanets: only the operator states which DLC planets they own (`pcds rules set world.dlcPlanets=...`), not a plugin".into());
    }
    Ok(())
}

/// What happened to the actions: how many were done, and how many the world no longer allowed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    pub done: u32,
    pub refused: u32,
}

pub fn apply(game: &mut Game, plugin: &str, actions: Vec<Action>) -> Applied {
    let mut out = Applied::default();
    for action in actions {
        let ok = match action {
            Action::Say(text) => {
                crate::chathooks::say(game, &text);
                true
            }
            Action::Tell(client, text) => {
                let there = game.clients.get(&client).is_some_and(|c| c.ready.is_some());
                if there {
                    game.net.rpc_scene(Target::Only(client), "ChatHandler.ReceiveChatMessageClientRpc", &[json!(0), json!(text)]);
                }
                there
            }
            Action::Kick(client, reason) => {
                let there = game.clients.contains_key(&client) && !game.kicks.iter().any(|(c, _)| *c == client);
                if there {
                    println!("plugin {plugin}: kicks client {client} ({reason})");
                    game.kick(client, &reason);
                }
                there
            }
            Action::Teleport(client, position) => crate::status::teleport(game, client, position),
            Action::RulesPatch(patch) => match game.rules.patched(&patch).and_then(|new| if new == game.rules { Ok(false) } else { crate::rules::set(game, new).map(|()| true) }) {
                // A patch that changes nothing (a plugin that puts a rule back to a value it has) writes no file.
                Ok(changed) => {
                    if changed {
                        // Only the gameplay sections can be in it: no secret.
                        println!("plugin {plugin}: changed the rules: {patch}");
                    }
                    true
                }
                Err(e) => {
                    eprintln!("plugin {plugin}: its rules change was refused: {e}");
                    false
                }
            },
            // The host takes the HTTP requests out before it applies the rest (Host::run).
            Action::Http(_) => false,
        };
        if ok {
            out.done += 1;
        } else {
            out.refused += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plugin_can_not_reach_the_rules_that_run_code_or_choose_players() {
        assert!(rules_patch_allowed(&json!({"speed": {"growth": 2}, "gauges": {"noDeath": true}})).is_ok());
        for bad in [json!({"chat": {"exec": ["/bin/sh"]}}), json!({"backup": {"s3Bucket": "x"}}), json!({"access": {"whitelist": []}}), json!({"speed": {}, "chat": null}), json!({"players": {"forgetAfterDays": 1}})] {
            assert!(rules_patch_allowed(&bad).is_err(), "{bad}");
        }
        // The operator's statement of which DLC planets they own is not a plugin's to make, though
        // the rest of `world` is.
        assert!(rules_patch_allowed(&json!({"world": {"pauseWhenEmpty": true}})).is_ok());
        for bad in [json!({"world": {"dlcPlanets": ["Humble"]}}), json!({"world": {"dlcPlanets": null}}), json!({"world": {"pauseWhenEmpty": true, "dlcPlanets": []}})] {
            let e = rules_patch_allowed(&bad).unwrap_err();
            assert!(e.contains("world.dlcPlanets") && e.contains("operator"), "{bad}: {e}");
        }
    }
}
