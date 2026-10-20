//! What a plugin calls: the `Host` trait, and the WebAssembly implementation that calls the
//! server's imports. Write a plugin against `Host` and test it on a PC with your own mock.

use alloc::string::String;
use alloc::vec::Vec;

use crate::codec::DecodeError;
#[cfg(target_arch = "wasm32")]
use crate::codec::Writer;
use crate::http::HttpRequest;
#[cfg(target_arch = "wasm32")]
use crate::query::{self, NearbyArgs};
use crate::query::{InventoryInfo, Nearby, ObjectInfo, ObjectRef, PlayerInfo, WorldInfo};

/// Return codes of the host functions (negative: it did not happen).
pub const OK: i32 = 0;
/// The capability is not granted.
pub const E_DENIED: i32 = -1;
/// The arguments are no good (a player that is not online is not an error here: actions are
/// checked when they are applied, after the plugin has returned).
pub const E_BAD: i32 = -2;
/// Too many actions in this call, or too many this second.
pub const E_FULL: i32 = -3;
/// The host does not have this function (a server older than the plugin).
pub const E_UNSUPPORTED: i32 = -4;
/// (ABI 1.2) The request's host is not one the manifest lists, or the URL is no URL the host takes.
pub const E_NOT_ALLOWED: i32 = -5;
/// (ABI 1.3) The player or object asked about is not there (a player left; an object was destroyed).
pub const E_NOT_FOUND: i32 = -6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Denied,
    Bad,
    Full,
    Unsupported,
    NotAllowed,
    NotFound,
    Decode(DecodeError),
}

impl Error {
    pub fn from_code(code: i32) -> Error {
        match code {
            E_DENIED => Error::Denied,
            E_FULL => Error::Full,
            E_UNSUPPORTED => Error::Unsupported,
            E_NOT_ALLOWED => Error::NotAllowed,
            E_NOT_FOUND => Error::NotFound,
            _ => Error::Bad,
        }
    }
}

impl From<DecodeError> for Error {
    fn from(e: DecodeError) -> Error {
        Error::Decode(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

pub trait Host {
    /// A line in the server's log, prefixed with the plugin's id. Rate limited.
    fn log(&self, level: Level, message: &str);
    /// `world.read`
    fn world(&self) -> Result<WorldInfo, Error>;
    /// `world.read`
    fn players(&self) -> Result<Vec<PlayerInfo>, Error>;
    /// `world.read`: the rules that differ from the defaults, as JSON text.
    fn rules(&self) -> Result<String, Error>;
    /// `chat.say`: a message from the server to everyone.
    fn say(&self, text: &str) -> Result<(), Error>;
    /// `chat.say`: a message from the server to one player.
    fn tell(&self, client: u64, text: &str) -> Result<(), Error>;
    /// `player.kick`
    fn kick(&self, client: u64, reason: &str) -> Result<(), Error>;
    /// `player.teleport`
    fn teleport(&self, client: u64, position: [f32; 3]) -> Result<(), Error>;
    /// `rules.write`: a JSON merge patch, as `pcds rules set` sends it.
    fn rules_patch(&self, json: &str) -> Result<(), Error>;
    /// `net.http` (ABI 1.2): starts an HTTP request and returns its ticket. The answer arrives
    /// later, in a later call, as `Event::HttpDone` with the same ticket. Errors that can be told at
    /// once are returned (`NotAllowed`: the host is not on the manifest's list; `Full`: too many
    /// requests in flight or this second); the rest (DNS, connection, timeout, size) come in the event.
    fn http_request(&self, request: &HttpRequest<'_>) -> Result<u64, Error>;

    // ---- ABI 1.3. A host older than the plugin answers `Unsupported`; so do these defaults, which
    // ---- keep a `Host` written for 1.2 (a test double) compiling.

    /// `world.read` (ABI 1.3): what a player carries. `NotFound` if they are not online.
    fn inventory(&self, _client: u64) -> Result<InventoryInfo, Error> {
        Err(Error::Unsupported)
    }
    /// `world.read` (ABI 1.3): the world objects within `radius` of `center`, closest first, at most
    /// `max` (and `query::MAX_NEARBY`). The scan is paid for in fuel by the number of objects in the world.
    fn nearby(&self, _center: [f32; 3], _radius: f32, _max: u32) -> Result<Vec<Nearby>, Error> {
        Err(Error::Unsupported)
    }
    /// `world.read` (ABI 1.3): one world object, or `None` if there is no such object (any more).
    fn object(&self, _which: ObjectRef) -> Result<Option<ObjectInfo>, Error> {
        Err(Error::Unsupported)
    }
    /// ABI 1.3, no capability: this plugin's own settings, the `[[config]]` keys of its manifest that
    /// are not secrets, as the admin set them or as their defaults say. A key nobody set and with no
    /// default is not in the list.
    fn settings(&self) -> Result<Vec<(String, String)>, Error> {
        Err(Error::Unsupported)
    }
    /// One setting (`settings` and a search): `None` if it is not set and has no default.
    fn config(&self, key: &str) -> Option<String> {
        self.settings().ok()?.into_iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    /// `player.give` (ABI 1.3): puts `count` of the item `group` into a player's backpack. Queued;
    /// done after the call if the player is still there and the backpack has room.
    fn give(&self, _client: u64, _group: &str, _count: u32) -> Result<(), Error> {
        Err(Error::Unsupported)
    }
    /// `world.tokens` (ABI 1.3): adds Terra tokens to the world's count, or takes them away (a
    /// negative amount; the count does not go below zero). Queued.
    fn tokens(&self, _amount: i32) -> Result<(), Error> {
        Err(Error::Unsupported)
    }
    /// `event.timer` (ABI 1.3): calls this plugin with `Event::Timer { id, .. }` after `delay_ms` of
    /// game time, and then every `repeat_ms` if that is not 0. Setting an id again replaces it. Timers
    /// belong to the running instance: set them in `load`, they are gone after a reload or a restart.
    fn set_timer(&self, _id: u32, _delay_ms: u32, _repeat_ms: u32) -> Result<(), Error> {
        Err(Error::Unsupported)
    }
    /// `event.timer` (ABI 1.3): cancels a timer. Cancelling one that is not set is not an error.
    fn cancel_timer(&self, _id: u32) -> Result<(), Error> {
        Err(Error::Unsupported)
    }
}

#[cfg(target_arch = "wasm32")]
mod sys {
    #[link(wasm_import_module = "pcds")]
    unsafe extern "C" {
        pub fn log(level: i32, ptr: *const u8, len: usize);
        pub fn query(kind: u32, arg: *const u8, arg_len: usize, out: *mut u8, out_cap: usize) -> i32;
        pub fn say(ptr: *const u8, len: usize) -> i32;
        pub fn tell(client: u64, ptr: *const u8, len: usize) -> i32;
        pub fn kick(client: u64, ptr: *const u8, len: usize) -> i32;
        pub fn teleport(client: u64, x: f32, y: f32, z: f32) -> i32;
        pub fn rules_patch(ptr: *const u8, len: usize) -> i32;
        pub fn http_request(ptr: *const u8, len: usize) -> i64;
        pub fn give(client: u64, group: *const u8, group_len: usize, count: u32) -> i32;
        pub fn tokens(amount: i32) -> i32;
        pub fn timer_set(id: u32, delay_ms: u32, repeat_ms: u32) -> i32;
        pub fn timer_cancel(id: u32) -> i32;
    }
}

/// The server's host functions, as a plugin module imports them.
#[cfg(target_arch = "wasm32")]
pub struct Wasm;

#[cfg(target_arch = "wasm32")]
impl Wasm {
    fn ask(kind: u32) -> Result<Vec<u8>, Error> {
        Wasm::ask_with(kind, &[])
    }
    fn ask_with(kind: u32, arg: &[u8]) -> Result<Vec<u8>, Error> {
        let mut out = alloc::vec![0u8; 512];
        loop {
            // SAFETY: the pointers are valid for the lengths given; the host writes at most out_cap bytes.
            let n = unsafe { sys::query(kind, arg.as_ptr(), arg.len(), out.as_mut_ptr(), out.len()) };
            if n < 0 {
                return Err(Error::from_code(n));
            }
            if n as usize <= out.len() {
                out.truncate(n as usize);
                return Ok(out);
            }
            out.resize(n as usize, 0);
        }
    }
    fn code(c: i32) -> Result<(), Error> {
        if c == OK { Ok(()) } else { Err(Error::from_code(c)) }
    }
}

#[cfg(target_arch = "wasm32")]
impl Host for Wasm {
    fn log(&self, level: Level, message: &str) {
        // SAFETY: a valid string slice.
        unsafe { sys::log(level as i32, message.as_ptr(), message.len()) }
    }
    fn world(&self) -> Result<WorldInfo, Error> {
        Ok(WorldInfo::decode(&Wasm::ask(crate::query::Q_WORLD)?)?)
    }
    fn players(&self) -> Result<Vec<PlayerInfo>, Error> {
        Ok(crate::query::decode_players(&Wasm::ask(crate::query::Q_PLAYERS)?)?)
    }
    fn rules(&self) -> Result<String, Error> {
        String::from_utf8(Wasm::ask(crate::query::Q_RULES)?).map_err(|_| Error::Decode(DecodeError::Utf8))
    }
    fn say(&self, text: &str) -> Result<(), Error> {
        // SAFETY: a valid string slice.
        Wasm::code(unsafe { sys::say(text.as_ptr(), text.len()) })
    }
    fn tell(&self, client: u64, text: &str) -> Result<(), Error> {
        // SAFETY: a valid string slice.
        Wasm::code(unsafe { sys::tell(client, text.as_ptr(), text.len()) })
    }
    fn kick(&self, client: u64, reason: &str) -> Result<(), Error> {
        // SAFETY: a valid string slice.
        Wasm::code(unsafe { sys::kick(client, reason.as_ptr(), reason.len()) })
    }
    fn teleport(&self, client: u64, p: [f32; 3]) -> Result<(), Error> {
        // SAFETY: plain numbers.
        Wasm::code(unsafe { sys::teleport(client, p[0], p[1], p[2]) })
    }
    fn rules_patch(&self, json: &str) -> Result<(), Error> {
        // SAFETY: a valid string slice.
        Wasm::code(unsafe { sys::rules_patch(json.as_ptr(), json.len()) })
    }
    fn http_request(&self, request: &HttpRequest<'_>) -> Result<u64, Error> {
        let mut w = Writer::new();
        request.encode(&mut w);
        // SAFETY: a valid byte slice.
        let ticket = unsafe { sys::http_request(w.0.as_ptr(), w.0.len()) };
        if ticket > 0 { Ok(ticket as u64) } else { Err(Error::from_code(ticket as i32)) }
    }
    fn inventory(&self, client: u64) -> Result<InventoryInfo, Error> {
        Ok(InventoryInfo::decode(&Wasm::ask_with(query::Q_INVENTORY, &client.to_le_bytes())?)?)
    }
    fn nearby(&self, center: [f32; 3], radius: f32, max: u32) -> Result<Vec<Nearby>, Error> {
        let mut w = Writer::new();
        NearbyArgs { center, radius, max }.encode(&mut w);
        Ok(query::decode_nearby(&Wasm::ask_with(query::Q_NEARBY, &w.0)?)?)
    }
    fn object(&self, which: ObjectRef) -> Result<Option<ObjectInfo>, Error> {
        let mut w = Writer::new();
        which.encode(&mut w);
        let answer = Wasm::ask_with(query::Q_OBJECT, &w.0)?;
        let mut r = crate::codec::Reader::new(&answer);
        Ok(if r.bool()? { Some(ObjectInfo::decode(&mut r)?) } else { None })
    }
    fn settings(&self) -> Result<Vec<(String, String)>, Error> {
        Ok(query::decode_settings(&Wasm::ask(query::Q_CONFIG)?)?)
    }
    fn give(&self, client: u64, group: &str, count: u32) -> Result<(), Error> {
        // SAFETY: a valid string slice.
        Wasm::code(unsafe { sys::give(client, group.as_ptr(), group.len(), count) })
    }
    fn tokens(&self, amount: i32) -> Result<(), Error> {
        // SAFETY: a plain number.
        Wasm::code(unsafe { sys::tokens(amount) })
    }
    fn set_timer(&self, id: u32, delay_ms: u32, repeat_ms: u32) -> Result<(), Error> {
        // SAFETY: plain numbers.
        Wasm::code(unsafe { sys::timer_set(id, delay_ms, repeat_ms) })
    }
    fn cancel_timer(&self, id: u32) -> Result<(), Error> {
        // SAFETY: a plain number.
        Wasm::code(unsafe { sys::timer_cancel(id) })
    }
}
