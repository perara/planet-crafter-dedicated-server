//! The plugin API of the Planet Crafter dedicated server (docs/plugins-api.md).
//!
//! A plugin is a WebAssembly module the server's bank loads and calls once per event it asked
//! for. This crate is both sides of that contract:
//!
//! - the plugin's side: the `Plugin` trait, the `export!` macro that turns it into the module's
//!   exports, and the `Host` trait (what a plugin calls: queries and actions);
//! - the shared bytes: `Event`, the query answers and the codec, which the server uses to write
//!   what the plugin reads.
//!
//! It is `no_std` with `alloc`. A plugin built with it needs a global allocator: `std` gives one
//! (a `cdylib` for `wasm32-unknown-unknown` with `panic = "abort"` is a module of a few tens of
//! kilobytes), or bring your own in a `no_std` plugin.
//!
//! Also here: [`json`], a small JSON reader and writer for RPC arguments, rules and HTTP bodies,
//! and (with the `testing` feature) `testing`, a mock host and a harness that run a plugin in
//! plain `cargo test`, with no server and no WebAssembly.
//!
//! # Features
//!
//! - `std`: links the standard library. Nothing in the API needs it.
//! - `testing`: the `testing` module. Turns `std` on. Use it as a dev-dependency only.
//!
//! ```
//! use pcds_plugin_api::*;
//!
//! struct Hello;
//! impl Plugin for Hello {
//!     fn load(_: &dyn Host, _: Option<State<'_>>) -> Self { Hello }
//!     fn event(&mut self, host: &dyn Host, event: &Event<'_>) -> Verdict {
//!         if let Event::Command { client, command: "hello", .. } = event {
//!             let _ = host.tell(*client, "Hello from a plugin");
//!         }
//!         Verdict::Continue
//!     }
//! }
//! export!(Hello);
//! ```

#![no_std]
#![warn(missing_docs)]

extern crate alloc;

// `allow(missing_docs)` marks the modules whose public items still lack doc comments. Remove the
// attribute from a module when it is documented.
#[allow(missing_docs)]
pub mod caps;
#[allow(missing_docs)]
pub mod codec;
#[allow(missing_docs)]
pub mod event;
#[allow(missing_docs)]
pub mod host;
#[allow(missing_docs)]
pub mod http;
pub mod json;
#[allow(missing_docs)]
pub mod query;
#[cfg(feature = "testing")]
pub mod testing;

pub use codec::{DecodeError, Reader, Writer};
pub use event::{Event, Kind, Verdict};
pub use host::{Error, Host, Level};
pub use http::HttpRequest;
pub use query::{InventoryInfo, Nearby, ObjectInfo, ObjectRef, PlayerInfo, WorldInfo};

#[cfg(target_arch = "wasm32")]
pub use host::Wasm;

/// The ABI this crate speaks: `major << 16 | minor`. A module exports the value it was built
/// for as `pcds_abi`; the server runs it when the major matches and the minor is not newer than
/// the server's own. Additive changes (a new event kind, a new host function, a field at the end
/// of an event) raise the minor; anything else raises the major.
pub const ABI_MAJOR: u32 = 1;
/// The minor part of `ABI`.
pub const ABI_MINOR: u32 = 3;
/// The ABI as one number: `major << 16 | minor`.
pub const ABI: u32 = ABI_MAJOR << 16 | ABI_MINOR;

/// What a plugin keeps across a restart, a reload and a hot update: opaque bytes the server
/// stores with the world, and the version the plugin wrote them with (its own migrate hook is
/// `load`: it gets the version of the bytes it is given).
pub struct State<'a> {
    /// The version the bytes were written with (`SavedState::version` of the call that saved them).
    pub version: u32,
    /// The bytes `save` returned.
    pub bytes: &'a [u8],
}

/// The state a plugin hands back from `Plugin::save`.
pub struct SavedState {
    /// A number of the plugin's own choosing. `load` gets it back with the bytes, so a new
    /// version of the plugin can tell old bytes from new ones and migrate them.
    pub version: u32,
    /// The state itself. At most 1 MiB; empty means nothing to keep.
    pub bytes: alloc::vec::Vec<u8>,
}

/// A plugin. Implement it and call `export!` with the type.
pub trait Plugin: Sized {
    /// Once when the plugin starts (a server start, a reload, or after a hot update): `state` is
    /// what `save` returned last time, if anything.
    fn load(host: &dyn Host, state: Option<State<'_>>) -> Self;

    /// An event the manifest subscribed to. Keep it short: it has a fuel budget. Answer
    /// `Verdict::Continue` to every event you do not care about.
    fn event(&mut self, host: &dyn Host, event: &Event<'_>) -> Verdict;

    /// The state to keep: asked at every world save, before a hot update flips and when the
    /// plugin is reloaded. `None`: nothing to keep.
    fn save(&mut self) -> Option<SavedState> {
        None
    }

    /// Once when the plugin is switched off, removed, or switched off for faults: the last call,
    /// with a budget (as `save`'s), after which the instance and its memory are dropped and `save`
    /// is asked one more time. Nothing queued during it is applied. A reload (a new version taking
    /// over) does not call it: `save` and the new version's `load` hand the state over. (ABI 1.1.)
    fn unload(&mut self) {}
}

#[doc(hidden)]
pub struct Slot<T>(core::cell::UnsafeCell<Option<T>>);

// SAFETY: WebAssembly modules without threads run one call at a time.
unsafe impl<T> Sync for Slot<T> {}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Slot<T> {
    pub const fn new() -> Self {
        Slot(core::cell::UnsafeCell::new(None))
    }
    #[allow(clippy::mut_from_ref)]
    pub fn get(&self) -> &mut Option<T> {
        // SAFETY: one call at a time (see the Sync impl), and the references never overlap
        // because the exports below take the slot, use it and put it back.
        unsafe { &mut *self.0.get() }
    }
}

#[doc(hidden)]
pub mod rt {
    use alloc::alloc::{Layout, alloc, dealloc};

    pub fn alloc_bytes(len: usize) -> *mut u8 {
        match Layout::from_size_align(len.max(1), 8) {
            // SAFETY: a non-zero size and a valid layout.
            Ok(l) => unsafe { alloc(l) },
            Err(_) => core::ptr::null_mut(),
        }
    }
    /// # Safety
    /// `ptr` came from `alloc_bytes(len)`.
    pub unsafe fn free_bytes(ptr: *mut u8, len: usize) {
        if let Ok(l) = Layout::from_size_align(len.max(1), 8) {
            // SAFETY: as the caller promises.
            unsafe { dealloc(ptr, l) }
        }
    }
}

/// The module's exports for a `Plugin`: `pcds_abi`, `pcds_alloc`, `pcds_free`, `pcds_event`,
/// `pcds_load`, `pcds_save`, `pcds_state_version` and `pcds_unload`.
#[macro_export]
macro_rules! export {
    ($plugin:ty) => {
        static __PCDS_PLUGIN: $crate::Slot<$plugin> = $crate::Slot::new();
        static __PCDS_SAVED_VERSION: $crate::Slot<u32> = $crate::Slot::new();

        #[unsafe(no_mangle)]
        pub extern "C" fn pcds_abi() -> u32 {
            $crate::ABI
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn pcds_alloc(len: usize) -> *mut u8 {
            $crate::rt::alloc_bytes(len)
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn pcds_free(ptr: *mut u8, len: usize) {
            // SAFETY: the host gives back what pcds_alloc or pcds_save handed out.
            unsafe { $crate::rt::free_bytes(ptr, len) }
        }

        /// 0: continue, 1: veto, negative: the plugin failed.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn pcds_event(kind: u32, ptr: *const u8, len: usize) -> i32 {
            let Some(plugin) = __PCDS_PLUGIN.get().as_mut() else { return -2 };
            // SAFETY: the host wrote len bytes at ptr (from pcds_alloc).
            let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
            match $crate::Event::decode(kind, bytes) {
                Ok(event) => $crate::Plugin::event(plugin, &$crate::__host(), &event).code(),
                // An event of a newer API than this plugin: nothing to do.
                Err($crate::DecodeError::UnknownKind(_)) => 0,
                Err(_) => -1,
            }
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn pcds_load(ptr: *const u8, len: usize, version: u32) -> i32 {
            let state = if ptr.is_null() {
                None
            } else {
                // SAFETY: the host wrote len bytes at ptr (from pcds_alloc).
                Some($crate::State { version, bytes: unsafe { core::slice::from_raw_parts(ptr, len) } })
            };
            *__PCDS_PLUGIN.get() = Some(<$plugin as $crate::Plugin>::load(&$crate::__host(), state));
            0
        }

        /// The state as pointer << 32 | length (0: none). The host copies it and calls pcds_free.
        #[unsafe(no_mangle)]
        pub extern "C" fn pcds_save() -> u64 {
            let Some(plugin) = __PCDS_PLUGIN.get().as_mut() else { return 0 };
            match $crate::Plugin::save(plugin) {
                Some(saved) if !saved.bytes.is_empty() => {
                    let len = saved.bytes.len();
                    let ptr = $crate::rt::alloc_bytes(len);
                    if ptr.is_null() {
                        return 0;
                    }
                    // SAFETY: ptr holds len bytes.
                    unsafe { core::ptr::copy_nonoverlapping(saved.bytes.as_ptr(), ptr, len) };
                    *__PCDS_SAVED_VERSION.get() = Some(saved.version);
                    ((ptr as usize as u64) << 32) | len as u64
                }
                _ => 0,
            }
        }

        /// The last call before the instance is dropped (ABI 1.1). 0 or more: fine.
        #[unsafe(no_mangle)]
        pub extern "C" fn pcds_unload() -> i32 {
            if let Some(plugin) = __PCDS_PLUGIN.get().as_mut() {
                $crate::Plugin::unload(plugin);
            }
            0
        }

        /// The version of the state the last pcds_save wrote.
        #[unsafe(no_mangle)]
        pub extern "C" fn pcds_state_version() -> u32 {
            (*__PCDS_SAVED_VERSION.get()).unwrap_or(0)
        }
    };
}

#[doc(hidden)]
#[cfg(target_arch = "wasm32")]
pub fn __host() -> Wasm {
    Wasm
}

/// On a PC there are no host functions: a plugin's own tests call `Plugin::event` with a mock.
#[doc(hidden)]
#[cfg(not(target_arch = "wasm32"))]
pub fn __host() -> host_unavailable::Unavailable {
    host_unavailable::Unavailable
}

#[cfg(not(target_arch = "wasm32"))]
mod host_unavailable {
    use alloc::string::String;
    use alloc::vec::Vec;

    use crate::host::{Error, Host, Level};
    use crate::http::HttpRequest;
    use crate::query::{PlayerInfo, WorldInfo};

    /// What `export!` hands a plugin on a PC: every call is refused.
    pub struct Unavailable;

    impl Host for Unavailable {
        fn log(&self, _: Level, _: &str) {}
        fn world(&self) -> Result<WorldInfo, Error> {
            Err(Error::Unsupported)
        }
        fn players(&self) -> Result<Vec<PlayerInfo>, Error> {
            Err(Error::Unsupported)
        }
        fn rules(&self) -> Result<String, Error> {
            Err(Error::Unsupported)
        }
        fn say(&self, _: &str) -> Result<(), Error> {
            Err(Error::Unsupported)
        }
        fn tell(&self, _: u64, _: &str) -> Result<(), Error> {
            Err(Error::Unsupported)
        }
        fn kick(&self, _: u64, _: &str) -> Result<(), Error> {
            Err(Error::Unsupported)
        }
        fn teleport(&self, _: u64, _: [f32; 3]) -> Result<(), Error> {
            Err(Error::Unsupported)
        }
        fn rules_patch(&self, _: &str) -> Result<(), Error> {
            Err(Error::Unsupported)
        }
        fn http_request(&self, _: &HttpRequest<'_>) -> Result<u64, Error> {
            Err(Error::Unsupported)
        }
    }
}

#[cfg(any(feature = "std", test))]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::{String, ToString};
    use alloc::vec;

    fn samples() -> alloc::vec::Vec<Event<'static>> {
        vec![
            Event::Tick { game_ms: 123_456, tick: 7, players: 3, since_ms: 1000, unix_ms: 1_760_000_000_000 },
            Event::PlayerJoin { client: 4, name: "Ann", steam_id: "76561198000000000", player_id: 76561198000000000 },
            Event::PlayerLeave { client: 4, name: "Ann", player_id: 76561198000000000 },
            Event::Chat { client: 9, name: "Bo", text: "hej på deg \u{1F680}", player_id: 5 },
            Event::Command { client: 9, name: "Bo", command: "tp", args: vec!["home", "now"], text: "!tp home now", player_id: 5 },
            Event::Command { client: 9, name: "Bo", command: "stats", args: vec![], text: "!stats", player_id: 5 },
            Event::RpcBefore { client: 1, rpc: "InventoriesHandler.Move", object: 77, args_json: "[1,2]" },
            Event::RpcAfter { client: 1, rpc: "InventoriesHandler.Move", object: 77, args_json: "[1,2]" },
            Event::ObjectPlaced { id: -5, group: "Drill1", position: [1.0, -2.5, 3.25], client: 3 },
            Event::ObjectDestroyed { id: 5, group: "Drill1", position: [0.0, 0.0, 0.0], client: 0 },
            Event::WorldSaved { game_ms: 99, ok: true },
            Event::RuleChange { changes_json: "{\"speed.growth\":2}" },
            Event::HttpDone { ticket: 41, status: 204, error: "", body: &[], headers: vec![] },
            Event::HttpDone { ticket: 42, status: 200, error: "", body: b"{\"ok\":true}", headers: vec![("content-type", "application/json"), ("etag", "\"x\"")] },
            Event::HttpDone { ticket: 43, status: 0, error: "timed out", body: &[0, 255, 7], headers: vec![] },
            Event::Timer { id: 7, game_ms: 99_000, late_ms: 12 },
            Event::Flip { game_ms: 99_000, server_version: "0.9.0" },
        ]
    }

    #[test]
    fn every_event_round_trips_and_names_its_kind() {
        for e in samples() {
            let mut w = Writer::new();
            e.encode(&mut w);
            assert_eq!(Event::decode(e.kind() as u32, &w.0).unwrap(), e);
            assert_eq!(Kind::from_name(e.kind().name()), Some(e.kind()));
            assert_eq!(Kind::from_u32(e.kind() as u32), Some(e.kind()));
            // A newer server may add fields at the end: this reader ignores them.
            let mut longer = w.0.clone();
            longer.extend_from_slice(&[1, 2, 3, 4, 5]);
            assert_eq!(Event::decode(e.kind() as u32, &longer).unwrap(), e);
        }
        // The interest bits are distinct.
        let all = Kind::ALL.iter().fold(0u32, |m, k| m | k.bit());
        assert_eq!(all.count_ones() as usize, Kind::ALL.len());
    }

    #[test]
    fn a_truncated_or_garbage_event_is_an_error_never_a_panic() {
        for e in samples() {
            let mut w = Writer::new();
            e.encode(&mut w);
            for cut in 0..w.0.len() {
                let _ = Event::decode(e.kind() as u32, &w.0[..cut]);
            }
        }
        assert_eq!(Event::decode(99, &[]), Err(DecodeError::UnknownKind(99)));
        assert_eq!(Event::decode(Kind::PlayerLeave as u32, &[1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0xff, 0xfe]), Err(DecodeError::Utf8));
        // A string that claims more bytes than there are, an argument count that cannot fit.
        assert_eq!(Event::decode(Kind::Chat as u32, &[0; 8].iter().chain(&[255, 255, 255, 255]).copied().collect::<alloc::vec::Vec<u8>>()), Err(DecodeError::Short));
        let mut w = Writer::new();
        w.u64(1).str("a").str("b").u32(u32::MAX);
        assert_eq!(Event::decode(Kind::Command as u32, &w.0), Err(DecodeError::Short));
        // A cheap xorshift fuzz: random bytes for every kind.
        let mut x = 0x9E3779B97F4A7C15u64;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = (x % 48) as usize;
            let bytes: alloc::vec::Vec<u8> = (0..len).map(|i| (x >> (i % 8 * 8)) as u8).collect();
            for k in 0..20 {
                let _ = Event::decode(k, &bytes);
            }
        }
    }

    #[test]
    fn query_answers_round_trip() {
        let world = WorldInfo { game_ms: 5, planet: "Prime".to_string(), players: 2, world_objects: 9, network_objects: 11, terraformation: 1234.5, standing_still: true, unix_ms: 1_760_000_000_000 };
        let mut w = Writer::new();
        world.encode(&mut w);
        assert_eq!(WorldInfo::decode(&w.0).unwrap(), world);
        let players = vec![PlayerInfo { client: 3, name: "Ann".to_string(), steam_id: String::new(), position: [1.0, 2.0, 3.0], gauges: [100.0, 90.0, 80.0, 0.0], player_id: 77, operator: true }];
        let mut w = Writer::new();
        query::encode_players(&players, &mut w);
        assert_eq!(query::decode_players(&w.0).unwrap(), players);
        assert!(query::decode_players(&[255, 255, 255, 255]).is_err());
    }

    #[test]
    fn the_abi_1_3_queries_round_trip_and_short_answers_are_errors() {
        use query::*;
        let inventory = InventoryInfo { slots_used: 3, slots_total: 30, items: vec![ItemCount { group: "Iron".to_string(), count: 5 }, ItemCount { group: "ice".to_string(), count: 1 }] };
        let mut w = Writer::new();
        inventory.encode(&mut w);
        assert_eq!(InventoryInfo::decode(&w.0).unwrap(), inventory);
        assert_eq!(inventory.count("Iron"), 5);
        assert_eq!(inventory.count("gold"), 0);
        for cut in 0..w.0.len() {
            assert!(InventoryInfo::decode(&w.0[..cut]).is_err());
        }
        let found = vec![Nearby { object: ObjectInfo { id: -4, net: 9, group: "Drill1".to_string(), position: [1.0, 2.0, 3.0] }, distance: 4.5 }];
        let mut w = Writer::new();
        encode_nearby(&found, &mut w);
        assert_eq!(decode_nearby(&w.0).unwrap(), found);
        assert!(decode_nearby(&[255, 255, 255, 255]).is_err());
        let args = NearbyArgs { center: [1.0, 2.0, 3.0], radius: 50.0, max: 8 };
        let mut w = Writer::new();
        args.encode(&mut w);
        assert_eq!(NearbyArgs::decode(&w.0).unwrap(), args);
        for which in [ObjectRef::World(-12), ObjectRef::World(7), ObjectRef::Net(u64::MAX)] {
            let mut w = Writer::new();
            which.encode(&mut w);
            assert_eq!(ObjectRef::decode(&w.0).unwrap(), which);
        }
        let settings = vec![("afk_minutes".to_string(), "10".to_string())];
        let mut w = Writer::new();
        encode_settings(&settings, &mut w);
        assert_eq!(decode_settings(&w.0).unwrap(), settings);
        assert!(decode_settings(&[255, 255, 255, 255]).is_err());
    }

    #[test]
    fn a_request_round_trips_and_a_bad_one_is_an_error_never_a_panic() {
        let request = HttpRequest { method: "POST", url: "https://discord.com/api/webhooks/${secret.hook}", headers: vec![("Content-Type", "application/json"), ("X-Empty", "")], body: b"{\"content\":\"hi\"}" };
        let mut w = Writer::new();
        request.encode(&mut w);
        assert_eq!(HttpRequest::decode(&w.0).unwrap(), request);
        for cut in 0..w.0.len() {
            assert!(HttpRequest::decode(&w.0[..cut]).is_err(), "a request cut at {cut} is no request");
        }
        // A header count the bytes cannot hold.
        let mut w = Writer::new();
        w.str("GET").str("https://a.example/").u32(u32::MAX);
        assert_eq!(HttpRequest::decode(&w.0), Err(DecodeError::Short));
        let mut x = 0x2545F4914F6CDD1Du64;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let bytes: alloc::vec::Vec<u8> = (0..(x % 64) as usize).map(|i| (x >> (i % 8 * 8)) as u8).collect();
            let _ = HttpRequest::decode(&bytes);
        }
    }

    #[test]
    fn the_abi_number_is_major_and_minor() {
        assert_eq!((ABI >> 16, ABI & 0xffff), (ABI_MAJOR, ABI_MINOR));
    }
}
