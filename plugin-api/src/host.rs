//! What a plugin calls: the `Host` trait, and the WebAssembly implementation that calls the
//! server's imports. Write a plugin against `Host` and test it on a PC with your own mock.

use alloc::string::String;
use alloc::vec::Vec;

use crate::codec::DecodeError;
#[cfg(target_arch = "wasm32")]
use crate::codec::Writer;
use crate::http::HttpRequest;
use crate::query::{PlayerInfo, WorldInfo};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Denied,
    Bad,
    Full,
    Unsupported,
    NotAllowed,
    Decode(DecodeError),
}

impl Error {
    pub fn from_code(code: i32) -> Error {
        match code {
            E_DENIED => Error::Denied,
            E_FULL => Error::Full,
            E_UNSUPPORTED => Error::Unsupported,
            E_NOT_ALLOWED => Error::NotAllowed,
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
    }
}

/// The server's host functions, as a plugin module imports them.
#[cfg(target_arch = "wasm32")]
pub struct Wasm;

#[cfg(target_arch = "wasm32")]
impl Wasm {
    fn ask(kind: u32) -> Result<Vec<u8>, Error> {
        let mut out = alloc::vec![0u8; 512];
        loop {
            // SAFETY: the pointers are valid for the lengths given; the host writes at most out_cap bytes.
            let n = unsafe { sys::query(kind, core::ptr::null(), 0, out.as_mut_ptr(), out.len()) };
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
}
