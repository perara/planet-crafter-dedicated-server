//! The events a plugin receives. The server writes each as `kind` and a payload (the fields in
//! the order below, with `Writer`); `Event::decode` reads them back. Fields are only ever added
//! at the end of an event, so an old plugin reads the fields it knows and ignores the rest.

use alloc::vec::Vec;

use crate::codec::{DecodeError, Reader, Writer};

/// The kinds, as the numbers on the wire and in `pcds_event`'s first argument. Never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
pub enum Kind {
    /// `Event::Tick`.
    Tick = 1,
    /// `Event::PlayerJoin`.
    PlayerJoin = 2,
    /// `Event::PlayerLeave`.
    PlayerLeave = 3,
    /// `Event::Chat`.
    Chat = 4,
    /// `Event::Command`.
    Command = 5,
    /// `Event::RpcBefore`.
    RpcBefore = 6,
    /// `Event::RpcAfter`.
    RpcAfter = 7,
    /// `Event::ObjectPlaced`.
    ObjectPlaced = 8,
    /// `Event::ObjectDestroyed`.
    ObjectDestroyed = 9,
    /// `Event::WorldSaved`.
    WorldSaved = 10,
    /// `Event::RuleChange`.
    RuleChange = 11,
    /// `Event::HttpDone`.
    HttpDone = 12,
    /// `Event::Timer`.
    Timer = 13,
    /// `Event::Flip`.
    Flip = 14,
}

impl Kind {
    /// Every kind, in wire order.
    pub const ALL: [Kind; 14] = [
        Kind::Tick,
        Kind::PlayerJoin,
        Kind::PlayerLeave,
        Kind::Chat,
        Kind::Command,
        Kind::RpcBefore,
        Kind::RpcAfter,
        Kind::ObjectPlaced,
        Kind::ObjectDestroyed,
        Kind::WorldSaved,
        Kind::RuleChange,
        Kind::HttpDone,
        Kind::Timer,
        Kind::Flip,
    ];

    /// The kind with this wire number, if there is one.
    pub fn from_u32(n: u32) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| *k as u32 == n)
    }

    /// The name a manifest's `events` list and the event stream use.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Tick => "tick",
            Kind::PlayerJoin => "player_join",
            Kind::PlayerLeave => "player_leave",
            Kind::Chat => "chat",
            Kind::Command => "command",
            Kind::RpcBefore => "rpc_before",
            Kind::RpcAfter => "rpc_after",
            Kind::ObjectPlaced => "object_placed",
            Kind::ObjectDestroyed => "object_destroyed",
            Kind::WorldSaved => "world_saved",
            Kind::RuleChange => "rule_change",
            Kind::HttpDone => "http_done",
            Kind::Timer => "timer",
            Kind::Flip => "flip",
        }
    }

    /// The kind with this name (see `name`), if there is one.
    pub fn from_name(name: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.name() == name)
    }

    /// The bit of this kind in an interest mask.
    pub const fn bit(self) -> u32 {
        1 << (self as u32)
    }
}

/// An event a plugin receives. Strings and slices borrow from the bytes of the call.
#[derive(Debug, Clone, PartialEq)]
pub enum Event<'a> {
    /// Every `tick_every_ms` of game time the plugin asked for (never while the world stands
    /// still). `since_ms`: game time since this plugin's last tick. `unix_ms` (ABI 1.3): the
    /// wall clock, Unix milliseconds: the only clock a plugin has besides the game's.
    Tick {
        /// Game time in milliseconds.
        game_ms: i64,
        /// The game's tick number (the game ticks every 5 ms of game time).
        tick: i32,
        /// Players online.
        players: u32,
        /// Game time since this plugin's last tick.
        since_ms: u32,
        /// The wall clock, Unix milliseconds.
        unix_ms: i64,
    },
    /// A player is in the game (past PlayerReady). `steam_id` is empty unless verified.
    /// `player_id` (ABI 1.3) is the player's id in this world's save: the same player has the same
    /// id on every visit, whatever the connection number (`client`) of the session. It is the
    /// verified SteamID64 when there is one, else a number made from the name.
    PlayerJoin {
        /// The connection number of this session.
        client: u64,
        /// The player's name.
        name: &'a str,
        /// The verified SteamID64 as text, else empty.
        steam_id: &'a str,
        /// The player's id in this world's save.
        player_id: u64,
    },
    /// A player left the game.
    PlayerLeave {
        /// The connection number of the session that ended.
        client: u64,
        /// The player's name.
        name: &'a str,
        /// The player's id in this world's save.
        player_id: u64,
    },
    /// A chat message. A client that is not ready yet (it has not joined) has an empty `name`
    /// and a `player_id` of 0.
    Chat {
        /// The connection number of the sender.
        client: u64,
        /// The sender's name; empty if the client is not ready.
        name: &'a str,
        /// The message.
        text: &'a str,
        /// The sender's id in this world's save; 0 if the client is not ready.
        player_id: u64,
    },
    /// A chat message that starts with the command prefix (`WorldInfo::command_prefix`) and names
    /// one of the plugin's commands. The host has already checked who may use it and how many
    /// arguments it takes, as the manifest says (when it refuses, it answers the player with
    /// `Use: !name` and the manifest's `usage`, which is only the arguments, such as `<player> [reason]`).
    /// `command` has the case the player typed (the
    /// host matches it without regard to case): compare with `eq_ignore_ascii_case`, or lower-case it.
    Command {
        /// The connection number of the player.
        client: u64,
        /// The player's name.
        name: &'a str,
        /// The command's name, without the prefix, as typed.
        command: &'a str,
        /// The words after the command.
        args: Vec<&'a str>,
        /// The whole message as typed, prefix included.
        text: &'a str,
        /// The player's id in this world's save.
        player_id: u64,
    },
    /// A client RPC about to run; answering `Verdict::Veto` drops it (needs `rpc.veto`).
    RpcBefore {
        /// The connection that sent it.
        client: u64,
        /// The RPC's name, such as `InventoriesHandler.Move`.
        rpc: &'a str,
        /// The network object it is for.
        object: u64,
        /// The RPC's arguments as a JSON array (see the `json` module).
        args_json: &'a str,
    },
    /// A client RPC that has run.
    RpcAfter {
        /// The connection that sent it.
        client: u64,
        /// The RPC's name, such as `InventoriesHandler.Move`.
        rpc: &'a str,
        /// The network object it was for.
        object: u64,
        /// The RPC's arguments as a JSON array (see the `json` module).
        args_json: &'a str,
    },
    /// An object was placed. `client` (ABI 1.3): the connection that built or dropped it; 0 when
    /// none did (the server itself, a machine, a plugin's action).
    ObjectPlaced {
        /// The world object's id.
        id: i32,
        /// The object's group id (`Drill1`).
        group: &'a str,
        /// Where it is.
        position: [f32; 3],
        /// The connection that placed it; 0 for none.
        client: u64,
    },
    /// An object was destroyed. `client` (ABI 1.3): the connection that deconstructed it; 0 when
    /// it went some other way (a meteor, a machine, an item used up).
    ObjectDestroyed {
        /// The world object's id.
        id: i32,
        /// The object's group id (`Drill1`).
        group: &'a str,
        /// Where it was.
        position: [f32; 3],
        /// The connection that deconstructed it; 0 for none.
        client: u64,
    },
    /// The world was saved.
    WorldSaved {
        /// Game time in milliseconds.
        game_ms: i64,
        /// False if the save failed.
        ok: bool,
    },
    /// The server rules changed; `changes_json` holds every rule that differs from the default.
    RuleChange {
        /// A JSON object of dotted rule names to values (see the `json` module).
        changes_json: &'a str,
    },
    /// An HTTP request the plugin made (`Host::http_request`, ABI 1.2) is over. Delivered to that
    /// plugin only, whatever it subscribed to. `status` is the HTTP status, or 0 when there was no
    /// answer: `error` then says why (it is empty when there is one). `body` is at most the
    /// manifest's `max_response_bytes`.
    HttpDone {
        /// The ticket `http_request` returned.
        ticket: u64,
        /// The HTTP status, or 0 for no answer.
        status: u32,
        /// Why there was no answer; empty when there was one.
        error: &'a str,
        /// The answer's body.
        body: &'a [u8],
        /// The answer's headers.
        headers: Vec<(&'a str, &'a str)>,
    },
    /// A timer the plugin set (`Host::set_timer`, ABI 1.3) is due. Delivered to that plugin only,
    /// whatever it subscribed to. `late_ms`: how long after its time it is delivered (game time).
    Timer {
        /// The id the plugin gave the timer.
        id: u32,
        /// Game time in milliseconds.
        game_ms: i64,
        /// How long after its time it is delivered.
        late_ms: u32,
    },
    /// The plugin was started by a hot update of the server (ABI 1.3): the world was handed over to
    /// a new program and this is the first event after `Plugin::load`. `server_version` is the new
    /// program's version.
    Flip {
        /// Game time in milliseconds.
        game_ms: i64,
        /// The new program's version.
        server_version: &'a str,
    },
}

impl<'a> Event<'a> {
    /// The kind of this event.
    pub fn kind(&self) -> Kind {
        match self {
            Event::Tick { .. } => Kind::Tick,
            Event::PlayerJoin { .. } => Kind::PlayerJoin,
            Event::PlayerLeave { .. } => Kind::PlayerLeave,
            Event::Chat { .. } => Kind::Chat,
            Event::Command { .. } => Kind::Command,
            Event::RpcBefore { .. } => Kind::RpcBefore,
            Event::RpcAfter { .. } => Kind::RpcAfter,
            Event::ObjectPlaced { .. } => Kind::ObjectPlaced,
            Event::ObjectDestroyed { .. } => Kind::ObjectDestroyed,
            Event::WorldSaved { .. } => Kind::WorldSaved,
            Event::RuleChange { .. } => Kind::RuleChange,
            Event::HttpDone { .. } => Kind::HttpDone,
            Event::Timer { .. } => Kind::Timer,
            Event::Flip { .. } => Kind::Flip,
        }
    }

    /// The payload bytes (what `decode` takes).
    pub fn encode(&self, w: &mut Writer) {
        match self {
            Event::Tick { game_ms, tick, players, since_ms, unix_ms } => {
                w.i64(*game_ms).i32(*tick).u32(*players).u32(*since_ms).i64(*unix_ms);
            }
            Event::PlayerJoin { client, name, steam_id, player_id } => {
                w.u64(*client).str(name).str(steam_id).u64(*player_id);
            }
            Event::PlayerLeave { client, name, player_id } => {
                w.u64(*client).str(name).u64(*player_id);
            }
            Event::Chat { client, name, text, player_id } => {
                w.u64(*client).str(name).str(text).u64(*player_id);
            }
            Event::Command { client, name, command, args, text, player_id } => {
                w.u64(*client).str(name).str(command).u32(args.len() as u32);
                args.iter().for_each(|a| {
                    w.str(a);
                });
                w.str(text).u64(*player_id);
            }
            Event::RpcBefore { client, rpc, object, args_json } | Event::RpcAfter { client, rpc, object, args_json } => {
                w.u64(*client).str(rpc).u64(*object).str(args_json);
            }
            Event::ObjectPlaced { id, group, position, client } | Event::ObjectDestroyed { id, group, position, client } => {
                w.i32(*id).str(group).f32(position[0]).f32(position[1]).f32(position[2]).u64(*client);
            }
            Event::WorldSaved { game_ms, ok } => {
                w.i64(*game_ms).bool(*ok);
            }
            Event::RuleChange { changes_json } => {
                w.str(changes_json);
            }
            Event::HttpDone { ticket, status, error, body, headers } => {
                w.u64(*ticket).u32(*status).str(error).bytes(body);
                crate::http::encode_pairs(headers, w);
            }
            Event::Timer { id, game_ms, late_ms } => {
                w.u32(*id).i64(*game_ms).u32(*late_ms);
            }
            Event::Flip { game_ms, server_version } => {
                w.i64(*game_ms).str(server_version);
            }
        }
    }

    /// Reads an event from the `kind` number and payload bytes the server wrote. Fields a newer
    /// server added at the end are ignored; a kind this version does not know is an error.
    pub fn decode(kind: u32, bytes: &'a [u8]) -> Result<Event<'a>, DecodeError> {
        let mut r = Reader::new(bytes);
        let kind = Kind::from_u32(kind).ok_or(DecodeError::UnknownKind(kind))?;
        Ok(match kind {
            Kind::Tick => Event::Tick { game_ms: r.i64()?, tick: r.i32()?, players: r.u32()?, since_ms: r.u32()?, unix_ms: r.i64()? },
            Kind::PlayerJoin => Event::PlayerJoin { client: r.u64()?, name: r.str()?, steam_id: r.str()?, player_id: r.u64()? },
            Kind::PlayerLeave => Event::PlayerLeave { client: r.u64()?, name: r.str()?, player_id: r.u64()? },
            Kind::Chat => Event::Chat { client: r.u64()?, name: r.str()?, text: r.str()?, player_id: r.u64()? },
            Kind::Command => {
                let (client, name, command) = (r.u64()?, r.str()?, r.str()?);
                let n = r.u32()? as usize;
                // Each argument takes at least 4 bytes: a count the bytes cannot hold is short.
                if n > r.rest().len() / 4 {
                    return Err(DecodeError::Short);
                }
                let mut args = Vec::with_capacity(n);
                for _ in 0..n {
                    args.push(r.str()?);
                }
                let text = r.str()?;
                Event::Command { client, name, command, args, text, player_id: r.u64()? }
            }
            Kind::RpcBefore => Event::RpcBefore { client: r.u64()?, rpc: r.str()?, object: r.u64()?, args_json: r.str()? },
            Kind::RpcAfter => Event::RpcAfter { client: r.u64()?, rpc: r.str()?, object: r.u64()?, args_json: r.str()? },
            Kind::ObjectPlaced => Event::ObjectPlaced { id: r.i32()?, group: r.str()?, position: [r.f32()?, r.f32()?, r.f32()?], client: r.u64()? },
            Kind::ObjectDestroyed => Event::ObjectDestroyed { id: r.i32()?, group: r.str()?, position: [r.f32()?, r.f32()?, r.f32()?], client: r.u64()? },
            Kind::WorldSaved => Event::WorldSaved { game_ms: r.i64()?, ok: r.bool()? },
            Kind::RuleChange => Event::RuleChange { changes_json: r.str()? },
            Kind::HttpDone => Event::HttpDone { ticket: r.u64()?, status: r.u32()?, error: r.str()?, body: r.bytes()?, headers: crate::http::decode_pairs(&mut r)? },
            Kind::Timer => Event::Timer { id: r.u32()?, game_ms: r.i64()?, late_ms: r.u32()? },
            Kind::Flip => Event::Flip { game_ms: r.i64()?, server_version: r.str()? },
        })
    }
}

/// What a plugin answers to an event: the return value of `pcds_event`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to say (also the answer to every event the plugin does not know).
    Continue,
    /// For `RpcBefore`: do not run this RPC. Any other event: same as `Continue`.
    Veto,
}

impl Verdict {
    /// The number `pcds_event` returns: 0 to continue, 1 to veto.
    pub const fn code(self) -> i32 {
        match self {
            Verdict::Continue => 0,
            Verdict::Veto => 1,
        }
    }
}
