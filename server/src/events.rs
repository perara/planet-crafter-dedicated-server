//! Where the game tells the outside what happened: the one place events are emitted.
//!
//! The game's own subsystems are reached through `hooks.rs` (the entry points the ported game
//! code is fanned out by). This is the other direction: a notification for whoever listens, with
//! the same typed events (`pcds_plugin_api::Event`) for all of them:
//!
//! - the chat hooks (`chathooks`, in the front: webhooks, the exec program, GET /api/chat/events)
//!   take every chat message, always;
//! - the plugins (`plugins`) and the event stream for integrations take the kinds they asked for.
//!
//! **Nobody listening costs one load and one test.** `wants` reads a bit mask in the game's
//! `ext.plugins` slot; every function below returns at once when the kind's bit is clear, before
//! it builds anything. With no plugin loaded and nobody polling the stream the mask is zero.
//!
//! A call site names what happened and passes what it already has; this module builds the event.
//! Nothing here changes what the game does, except `rpc_before`, which lets a plugin that holds
//! the capability refuse a client RPC.

use pcds_plugin_api::{Event, Kind, Verdict};

use crate::game::Game;
use crate::model::WorldObject;

/// Whether anyone wants events of this kind.
#[inline(always)]
pub fn wants(game: &Game, kind: Kind) -> bool {
    game.ext.plugins.interest & kind.bit() != 0
}

fn online(game: &Game) -> u32 {
    game.clients.values().filter(|c| c.ready.is_some()).count() as u32
}

fn emit(game: &mut Game, event: &Event<'_>) -> Verdict {
    crate::plugins::dispatch(game, event)
}

/// Every game tick (the plugins that asked are called every so often, not at each).
#[inline]
pub fn tick(game: &mut Game) {
    if !wants(game, Kind::Tick) {
        return;
    }
    let event = Event::Tick { game_ms: (game.time as f64 * 1000.0) as i64, tick: game.tick_number(), players: online(game), since_ms: 0 };
    emit(game, &event);
}

/// A player is in the game (PlayerReady done).
pub fn player_joined(game: &mut Game, client: u64) {
    if !wants(game, Kind::PlayerJoin) {
        return;
    }
    let Some(c) = game.clients.get(&client) else { return };
    let (name, steam_id) = (c.ready.as_ref().map(|s| s.name.clone()).unwrap_or_default(), c.verified_steam_id.map(|i| i.to_string()).unwrap_or_default());
    emit(game, &Event::PlayerJoin { client, name: &name, steam_id: &steam_id });
}

pub fn player_left(game: &mut Game, client: u64, name: &str) {
    if wants(game, Kind::PlayerLeave) {
        emit(game, &Event::PlayerLeave { client, name });
    }
}

/// ChatHandler.SendChatMessageServerRpc: the chat hooks always get it; the plugins when they want it.
pub fn chat(game: &mut Game, client: u64, text: &str) {
    crate::chathooks::chat(game, client, text);
    if !wants(game, Kind::Chat) {
        return;
    }
    let name = game.clients.get(&client).and_then(|c| c.ready.as_ref()).map(|s| s.name.clone()).unwrap_or_default();
    emit(game, &Event::Chat { client, name: &name, text });
}

fn rpc_event(game: &mut Game, kind: Kind, client: u64, object: u64, name: &str, args: &[serde_json::Value]) -> Verdict {
    if !wants(game, kind) || !crate::plugins::wants_rpc(game, name) {
        return Verdict::Continue;
    }
    let json = serde_json::to_string(args).unwrap_or_default();
    let event = if kind == Kind::RpcBefore { Event::RpcBefore { client, rpc: name, object, args_json: &json } } else { Event::RpcAfter { client, rpc: name, object, args_json: &json } };
    emit(game, &event)
}

/// A client RPC is about to run: true when a plugin that may refuse it did, and it must not run.
pub fn rpc_before(game: &mut Game, client: u64, object: u64, name: &str, args: &[serde_json::Value]) -> bool {
    rpc_event(game, Kind::RpcBefore, client, object, name, args) == Verdict::Veto
}

pub fn rpc_after(game: &mut Game, client: u64, object: u64, name: &str, args: &[serde_json::Value]) {
    rpc_event(game, Kind::RpcAfter, client, object, name, args);
}

/// A world object got its network object (built, dropped; not when the world loads).
pub fn object_placed(game: &mut Game, id: i32) {
    if !wants(game, Kind::ObjectPlaced) {
        return;
    }
    let Some(wo) = game.wos.get(&id) else { return };
    let (group, position) = (game.data.groups[wo.group].id.clone(), wo.position);
    emit(game, &Event::ObjectPlaced { id, group: &group, position });
}

/// A world object was destroyed (already out of `wos`).
pub fn object_destroyed(game: &mut Game, wo: &WorldObject) {
    if wants(game, Kind::ObjectDestroyed) {
        let group = game.data.groups[wo.group].id.clone();
        emit(game, &Event::ObjectDestroyed { id: wo.id, group: &group, position: wo.position });
    }
}

/// The world was saved (or the save failed).
pub fn world_saved(game: &mut Game, ok: bool) {
    if wants(game, Kind::WorldSaved) {
        emit(game, &Event::WorldSaved { game_ms: (game.time as f64 * 1000.0) as i64, ok });
    }
}

/// The server rules changed.
pub fn rules_changed(game: &mut Game) {
    if wants(game, Kind::RuleChange) {
        let json = crate::plugins::changes_json(&game.rules);
        emit(game, &Event::RuleChange { changes_json: &json });
    }
}
