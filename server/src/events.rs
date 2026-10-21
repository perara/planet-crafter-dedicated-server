//! Where the game tells the outside what happened: the one place events are emitted.
//!
//! The game's own subsystems are reached through `hooks.rs` (the entry points the ported game
//! code is fanned out by). This is the other direction: a notification for whoever listens, with
//! the same typed events (`pcds_plugin_api::Event`) for all of them:
//!
//! - the chat hooks (`chathooks`: queued for the front, which delivers them to webhooks, the exec
//!   program and GET /api/chat/events) take every chat message, always;
//! - the plugins (`plugins`) and the event stream for integrations take the kinds they asked for.
//!
//! Those are the **subscribers**, in this order, and `emit` is the only place that calls them: each
//! has a `wants(kind)` and takes the event. A new listener is a third line in `emit` and in `wants`,
//! not a call from the game.
//!
//! **Nobody listening costs one load and one test.** `wants` reads a bit mask in the game's
//! `ext.plugins` slot (and asks the chat hooks, which is a constant for every kind but chat); every
//! function below returns at once when the kind's bit is clear, before it builds anything. With no
//! plugin loaded and nobody polling the stream the mask is zero.
//!
//! A call site names what happened and passes what it already has; this module builds the event.
//! Nothing here changes what the game does, except `rpc_before`, which lets a plugin that holds
//! the capability refuse a client RPC.

use pcds_plugin_api::{Event, Kind, Verdict};

use crate::game::Game;
use crate::model::WorldObject;

/// Whether anyone wants events of this kind: the chat hooks or a plugin.
#[inline(always)]
pub fn wants(game: &Game, kind: Kind) -> bool {
    crate::chathooks::wants(kind) || plugins_want(game, kind)
}

/// Whether a plugin (or the event stream) asked for events of this kind.
#[inline(always)]
fn plugins_want(game: &Game, kind: Kind) -> bool {
    game.ext.plugins.interest & kind.bit() != 0
}

fn online(game: &Game) -> u32 {
    game.clients.values().filter(|c| c.ready.is_some()).count() as u32
}

/// The event to its subscribers, each only for the kinds it wants; the verdict is the plugins'.
fn emit(game: &mut Game, event: &Event<'_>) -> Verdict {
    let kind = event.kind();
    if crate::chathooks::wants(kind) {
        crate::chathooks::on_event(game, event);
    }
    if plugins_want(game, kind) {
        return crate::plugins::dispatch(game, event);
    }
    Verdict::Continue
}

/// Every game tick (the plugins that asked are called every so often, not at each).
#[inline]
pub fn tick(game: &mut Game) {
    if !wants(game, Kind::Tick) {
        return;
    }
    let event = Event::Tick { game_ms: (game.time.secs() * 1000.0) as i64, tick: game.tick_number(), players: online(game), since_ms: 0, unix_ms: crate::plugins::stream::unix_ms() as i64 };
    emit(game, &event);
}

/// A player is in the game (PlayerReady done).
pub fn player_joined(game: &mut Game, client: u64) {
    if !wants(game, Kind::PlayerJoin) {
        return;
    }
    let Some(c) = game.clients.get(&client) else { return };
    let (name, steam_id, player_id) = (c.ready.as_ref().map(|s| s.name.clone()).unwrap_or_default(), c.verified_steam_id.map(|i| i.to_string()).unwrap_or_default(), c.ready.as_ref().map_or(0, |s| s.id));
    emit(game, &Event::PlayerJoin { client, name: &name, steam_id: &steam_id, player_id });
}

pub fn player_left(game: &mut Game, client: u64, name: &str, player_id: u64) {
    if wants(game, Kind::PlayerLeave) {
        emit(game, &Event::PlayerLeave { client, name, player_id });
    }
}

/// ChatHandler.SendChatMessageServerRpc: the chat hooks always get it; the plugins when they want it.
pub fn chat(game: &mut Game, client: u64, text: &str) {
    let (name, player_id) = game.clients.get(&client).and_then(|c| c.ready.as_ref()).map(|s| (s.name.clone(), s.id)).unwrap_or_default();
    emit(game, &Event::Chat { client, name: &name, text, player_id });
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
    let client = game.ext.plugins.actor;
    emit(game, &Event::ObjectPlaced { id, group: &group, position, client });
}

/// A world object was destroyed (already out of `wos`).
pub fn object_destroyed(game: &mut Game, wo: &WorldObject) {
    if wants(game, Kind::ObjectDestroyed) {
        let group = game.data.groups[wo.group].id.clone();
        let client = game.ext.plugins.actor;
        emit(game, &Event::ObjectDestroyed { id: wo.id, group: &group, position: wo.position, client });
    }
}

/// The world was saved (or the save failed).
pub fn world_saved(game: &mut Game, ok: bool) {
    if wants(game, Kind::WorldSaved) {
        emit(game, &Event::WorldSaved { game_ms: (game.time.secs() * 1000.0) as i64, ok });
    }
}

/// The server rules changed.
pub fn rules_changed(game: &mut Game) {
    if wants(game, Kind::RuleChange) {
        let json = crate::plugins::changes_json(&game.rules);
        emit(game, &Event::RuleChange { changes_json: &json });
    }
}
