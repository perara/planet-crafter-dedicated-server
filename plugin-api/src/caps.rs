//! The capabilities a manifest may ask for and an admin grants. Each event kind and each host
//! function needs exactly one of them (the table in docs/plugins-api.md).

use crate::event::Kind;

/// `Event::Tick`.
pub const EVENT_TICK: &str = "event.tick";
/// `Event::PlayerJoin` and `Event::PlayerLeave`.
pub const EVENT_PLAYER: &str = "event.player";
/// `Event::Chat` and `Event::Command`.
pub const EVENT_CHAT: &str = "event.chat";
/// `Event::RpcBefore` and `Event::RpcAfter`, for the RPCs the manifest names.
pub const EVENT_RPC: &str = "event.rpc";
/// `Event::ObjectPlaced` and `Event::ObjectDestroyed`.
pub const EVENT_OBJECT: &str = "event.object";
/// `Event::WorldSaved`, `Event::RuleChange` and `Event::Flip`.
pub const EVENT_WORLD: &str = "event.world";
/// Answer `Verdict::Veto` to an `Event::RpcBefore`.
pub const RPC_VETO: &str = "rpc.veto";
/// The queries: `Host::world`, `players`, `player`, `rules`, `inventory`, `nearby` and `object`.
pub const WORLD_READ: &str = "world.read";
/// `Host::say` and `Host::tell`.
pub const CHAT_SAY: &str = "chat.say";
/// `Host::kick`.
pub const PLAYER_KICK: &str = "player.kick";
/// `Host::teleport`.
pub const PLAYER_TELEPORT: &str = "player.teleport";
/// `Host::rules_patch`.
pub const RULES_WRITE: &str = "rules.write";
/// `Host::http_request` and `Event::HttpDone`.
pub const NET_HTTP: &str = "net.http";
/// Lets `net.http` requests reach this machine and the local network.
pub const NET_HTTP_LOCAL: &str = "net.http.local";
/// ABI 1.3. `Host::set_timer`, `Host::cancel_timer` and `Event::Timer`.
pub const EVENT_TIMER: &str = "event.timer";
/// `Host::give`.
pub const PLAYER_GIVE: &str = "player.give";
/// `Host::tokens`.
pub const WORLD_TOKENS: &str = "world.tokens";

/// Every capability, with what it allows, for `pcds plugin check` and the dashboard.
pub const ALL: [(&str, &str); 17] = [
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
    (NET_HTTP, "send HTTP requests to the hosts its manifest lists"),
    (NET_HTTP_LOCAL, "let those requests reach this machine and the local network"),
    (EVENT_TIMER, "set timers and be called when they are due"),
    (PLAYER_GIVE, "put items into players' backpacks"),
    (WORLD_TOKENS, "add Terra tokens to the world, or take them away"),
];

/// True if `name` is a capability this API knows.
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
        Kind::WorldSaved | Kind::RuleChange | Kind::Flip => EVENT_WORLD,
        Kind::HttpDone => NET_HTTP,
        Kind::Timer => EVENT_TIMER,
    }
}
