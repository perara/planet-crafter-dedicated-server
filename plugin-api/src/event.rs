//! The events a plugin receives. The server writes each as `kind` and a payload (the fields in
//! the order below, with `Writer`); `Event::decode` reads them back. Fields are only ever added
//! at the end of an event, so an old plugin reads the fields it knows and ignores the rest.

use alloc::vec::Vec;

use crate::codec::{DecodeError, Reader, Writer};

/// The kinds, as the numbers on the wire and in `pcds_event`'s first argument. Never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
pub enum Kind {
    Tick = 1,
    PlayerJoin = 2,
    PlayerLeave = 3,
    Chat = 4,
    Command = 5,
    RpcBefore = 6,
    RpcAfter = 7,
    ObjectPlaced = 8,
    ObjectDestroyed = 9,
    WorldSaved = 10,
    RuleChange = 11,
    HttpDone = 12,
}

impl Kind {
    pub const ALL: [Kind; 12] = [
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
    ];

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
        }
    }

    pub fn from_name(name: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.name() == name)
    }

    /// The bit of this kind in an interest mask.
    pub const fn bit(self) -> u32 {
        1 << (self as u32)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event<'a> {
    /// Every `tick_every_ms` of game time the plugin asked for (never while the world stands
    /// still). `since_ms`: game time since this plugin's last tick.
    Tick { game_ms: i64, tick: i32, players: u32, since_ms: u32 },
    /// A player is in the game (past PlayerReady). `steam_id` is empty unless verified.
    PlayerJoin { client: u64, name: &'a str, steam_id: &'a str },
    PlayerLeave { client: u64, name: &'a str },
    Chat { client: u64, name: &'a str, text: &'a str },
    /// A chat message that starts with the command prefix and names one of the plugin's commands.
    Command { client: u64, name: &'a str, command: &'a str, args: Vec<&'a str>, text: &'a str },
    /// A client RPC about to run; answering `Verdict::Veto` drops it (needs `rpc.veto`).
    RpcBefore { client: u64, rpc: &'a str, object: u64, args_json: &'a str },
    RpcAfter { client: u64, rpc: &'a str, object: u64, args_json: &'a str },
    ObjectPlaced { id: i32, group: &'a str, position: [f32; 3] },
    ObjectDestroyed { id: i32, group: &'a str, position: [f32; 3] },
    WorldSaved { game_ms: i64, ok: bool },
    /// The server rules changed; `changes_json` holds every rule that differs from the default.
    RuleChange { changes_json: &'a str },
    /// An HTTP request the plugin made (`Host::http_request`, ABI 1.2) is over. Delivered to that
    /// plugin only, whatever it subscribed to. `status` is the HTTP status, or 0 when there was no
    /// answer: `error` then says why (it is empty when there is one). `body` is at most the
    /// manifest's `max_response_bytes`.
    HttpDone { ticket: u64, status: u32, error: &'a str, body: &'a [u8], headers: Vec<(&'a str, &'a str)> },
}

impl<'a> Event<'a> {
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
        }
    }

    /// The payload bytes (what `decode` takes).
    pub fn encode(&self, w: &mut Writer) {
        match self {
            Event::Tick { game_ms, tick, players, since_ms } => {
                w.i64(*game_ms).i32(*tick).u32(*players).u32(*since_ms);
            }
            Event::PlayerJoin { client, name, steam_id } => {
                w.u64(*client).str(name).str(steam_id);
            }
            Event::PlayerLeave { client, name } => {
                w.u64(*client).str(name);
            }
            Event::Chat { client, name, text } => {
                w.u64(*client).str(name).str(text);
            }
            Event::Command { client, name, command, args, text } => {
                w.u64(*client).str(name).str(command).u32(args.len() as u32);
                args.iter().for_each(|a| {
                    w.str(a);
                });
                w.str(text);
            }
            Event::RpcBefore { client, rpc, object, args_json } | Event::RpcAfter { client, rpc, object, args_json } => {
                w.u64(*client).str(rpc).u64(*object).str(args_json);
            }
            Event::ObjectPlaced { id, group, position } | Event::ObjectDestroyed { id, group, position } => {
                w.i32(*id).str(group).f32(position[0]).f32(position[1]).f32(position[2]);
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
        }
    }

    pub fn decode(kind: u32, bytes: &'a [u8]) -> Result<Event<'a>, DecodeError> {
        let mut r = Reader::new(bytes);
        let kind = Kind::from_u32(kind).ok_or(DecodeError::UnknownKind(kind))?;
        Ok(match kind {
            Kind::Tick => Event::Tick { game_ms: r.i64()?, tick: r.i32()?, players: r.u32()?, since_ms: r.u32()? },
            Kind::PlayerJoin => Event::PlayerJoin { client: r.u64()?, name: r.str()?, steam_id: r.str()? },
            Kind::PlayerLeave => Event::PlayerLeave { client: r.u64()?, name: r.str()? },
            Kind::Chat => Event::Chat { client: r.u64()?, name: r.str()?, text: r.str()? },
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
                Event::Command { client, name, command, args, text: r.str()? }
            }
            Kind::RpcBefore => Event::RpcBefore { client: r.u64()?, rpc: r.str()?, object: r.u64()?, args_json: r.str()? },
            Kind::RpcAfter => Event::RpcAfter { client: r.u64()?, rpc: r.str()?, object: r.u64()?, args_json: r.str()? },
            Kind::ObjectPlaced => Event::ObjectPlaced { id: r.i32()?, group: r.str()?, position: [r.f32()?, r.f32()?, r.f32()?] },
            Kind::ObjectDestroyed => Event::ObjectDestroyed { id: r.i32()?, group: r.str()?, position: [r.f32()?, r.f32()?, r.f32()?] },
            Kind::WorldSaved => Event::WorldSaved { game_ms: r.i64()?, ok: r.bool()? },
            Kind::RuleChange => Event::RuleChange { changes_json: r.str()? },
            Kind::HttpDone => Event::HttpDone { ticket: r.u64()?, status: r.u32()?, error: r.str()?, body: r.bytes()?, headers: crate::http::decode_pairs(&mut r)? },
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
    pub const fn code(self) -> i32 {
        match self {
            Verdict::Continue => 0,
            Verdict::Veto => 1,
        }
    }
}
