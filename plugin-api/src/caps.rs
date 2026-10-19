//! The capabilities a manifest may ask for and an admin grants. Each event kind and each host
//! function needs exactly one of them (the table in docs/plugins-api.md).

use crate::event::Kind;

pub const EVENT_TICK: &str = "event.tick";
pub const EVENT_PLAYER: &str = "event.player";
pub const EVENT_CHAT: &str = "event.chat";
pub const EVENT_RPC: &str = "event.rpc";
pub const EVENT_OBJECT: &str = "event.object";
pub const EVENT_WORLD: &str = "event.world";
pub const RPC_VETO: &str = "rpc.veto";
pub const WORLD_READ: &str = "world.read";
pub const CHAT_SAY: &str = "chat.say";
pub const PLAYER_KICK: &str = "player.kick";
pub const PLAYER_TELEPORT: &str = "player.teleport";
pub const RULES_WRITE: &str = "rules.write";

/// Every capability, with what it allows, for `pcds plugin check` and the dashboard.
pub const ALL: [(&str, &str); 12] = [
    (EVENT_TICK, "be called every few seconds of game time"),
    (EVENT_PLAYER, "hear when players join and leave"),
    (EVENT_CHAT, "read chat messages and chat commands"),
    (EVENT_RPC, "observe the client RPCs it names"),
    (EVENT_OBJECT, "hear when buildings and objects are placed and destroyed"),
    (EVENT_WORLD, "hear about saves and rule changes"),
    (RPC_VETO, "refuse the client RPCs it names before they run"),
    (WORLD_READ, "read the world: time, players, positions, counts, rules"),
    (CHAT_SAY, "write to the chat, to everyone or to one player"),
    (PLAYER_KICK, "kick players"),
    (PLAYER_TELEPORT, "move players"),
    (RULES_WRITE, "change the server rules"),
];

pub fn known(name: &str) -> bool {
    ALL.iter().any(|(n, _)| *n == name)
}

/// The capability an event kind needs.
pub fn for_event(kind: Kind) -> &'static str {
    match kind {
        Kind::Tick => EVENT_TICK,
        Kind::PlayerJoin | Kind::PlayerLeave => EVENT_PLAYER,
        Kind::Chat | Kind::Command => EVENT_CHAT,
        Kind::RpcBefore | Kind::RpcAfter => EVENT_RPC,
        Kind::ObjectPlaced | Kind::ObjectDestroyed => EVENT_OBJECT,
        Kind::WorldSaved | Kind::RuleChange => EVENT_WORLD,
    }
}
