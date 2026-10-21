//! What a plugin may cost. The host's numbers are maxima: a manifest may ask for less than the
//! default or for more up to the maximum, and the admin may lower either in approved.json. A
//! plugin that goes over is stopped in that call (fuel, memory) and counts a fault; too many
//! faults in a short time switch it off (docs/plugins.md, "Budgets and faults").
//!
//! Fuel is the interpreter's own count of work: about one unit per simple instruction. Measured
//! on the development host, 1,000,000 fuel is a third of a millisecond of plain arithmetic;
//! `plugins::bench` prints the rate of the machine it runs on.

use std::time::Duration;

/// Fuel for one event, by default and at most. 1,000,000 is 0.3 ms of plain arithmetic.
pub const FUEL_PER_EVENT: u64 = 1_000_000;
pub const MAX_FUEL_PER_EVENT: u64 = 20_000_000;
/// Fuel for `pcds_save` (the state a hot update carries), by default and at most.
pub const FUEL_PER_SAVE: u64 = 5_000_000;
pub const MAX_FUEL_PER_SAVE: u64 = 50_000_000;
/// Fuel for starting a plugin (the module's start function and `pcds_load`).
pub const FUEL_LOAD: u64 = 100_000_000;

/// 64 KiB pages of memory: 64 is 4 MiB; at most 1024 is 64 MiB.
pub const MEMORY_PAGES: u32 = 64;
pub const MAX_MEMORY_PAGES: u32 = 1024;
/// A manifest bigger than this is not read (a real one is a few KiB).
pub const MAX_MANIFEST_BYTES: usize = 256 << 10;
/// A module bigger than this is not loaded.
pub const MAX_MODULE_BYTES: usize = 8 << 20;

/// How often a plugin that subscribed to `tick` is called, in game milliseconds, by default;
/// the least a manifest may ask for is every game tick.
pub const TICK_EVERY_MS: u32 = 1000;
pub const MIN_TICK_MS: u32 = 5;
/// All plugins together may spend this on ticks within one game tick; the plugins left over wait
/// for the next one (they take turns), so a crowd of plugins never stretches a tick.
pub const TICK_BUDGET: Duration = Duration::from_micros(2000);
/// All plugins together may spend this on saving their state when the world is handed over to
/// the next bank; one that is not done keeps the state of its last save.
pub const FLIP_BUDGET: Duration = Duration::from_millis(25);
/// A call that takes longer than this is counted as slow and logged (the fuel limit is what stops
/// it; this is for finding it).
pub const SLOW_CALL: Duration = Duration::from_millis(5);

/// What the host's work for a plugin costs it in fuel (wasm.rs `charge`): a host call, then each
/// byte the host copies for it and each it parses (JSON, a URL, headers: several times a copy).
/// A million fuel is about a third of a millisecond, so this keeps a call that names big buffers
/// to the time its fuel is worth. A `log` or `say` text is cut at `MAX_TEXT_BYTES` (every user of
/// it cuts it shorter), a rules patch is refused over `MAX_PATCH_BYTES`.
pub const HOST_CALL_FUEL: u64 = 200;
pub const FUEL_PER_BYTE: u64 = 1;
pub const FUEL_PER_PARSED_BYTE: u64 = 16;
pub const MAX_TEXT_BYTES: usize = 4096;
pub const MAX_PATCH_BYTES: usize = 64 << 10;
/// A query's argument, and an item's name in `give`.
pub const MAX_QUERY_ARG: usize = 64;
pub const MAX_ITEM_NAME_BYTES: usize = 64;

/// Bytes a module's bulk memory operation (memory.fill, memory.copy) moves for one unit of fuel
/// (wasmi's default is 64; see wasm.rs `engine`).
pub const BYTES_COPIED_PER_FUEL: u32 = 4;

/// Actions a plugin may queue in one call, and in one second.
pub const ACTIONS_PER_CALL: usize = 16;
pub const ACTIONS_PER_SECOND: u32 = 200;
/// Rule changes (each writes rules.json) per second per plugin.
pub const RULE_CHANGES_PER_SECOND: u32 = 2;
/// Log lines per second (the rest are counted), and the longest line.
pub const LOG_LINES_PER_SECOND: u32 = 20;
pub const LOG_LINE_CHARS: usize = 240;
/// A chat line a plugin says, in characters (the chat's own limit).
pub const SAY_CHARS: usize = 500;
/// The state blob, at most (more is not kept; the plugin counts a fault).
pub const MAX_STATE_BYTES: usize = 1 << 20;
/// What a query answer or an event payload may hold.
pub const MAX_PAYLOAD: usize = 1 << 20;

/// `player.give` and `world.tokens`: how many of one item a single call gives, how many gives a
/// plugin may queue in a second, the most Terra tokens one call adds or takes, and how many such
/// calls in a second. A backpack that has no room refuses the rest when the action is applied.
pub const MAX_GIVE_COUNT: u32 = 50;
pub const GIVES_PER_SECOND: u32 = 10;
pub const MAX_TOKENS_PER_ACTION: i32 = 10_000;
pub const TOKEN_CHANGES_PER_SECOND: u32 = 2;

/// `event.timer`: timers one plugin may have, the shortest repeat, the longest delay (seven days),
/// and how many timer events one plugin gets in a game tick (the rest wait for the next).
pub const MAX_TIMERS: usize = 16;
pub const MIN_TIMER_REPEAT_MS: u32 = 100;
pub const MAX_TIMER_DELAY_MS: u32 = 604_800_000;
pub const TIMER_EVENTS_PER_TICK: usize = 4;

/// The scan of `Q_NEARBY` goes through every object in the world; each one it looks at costs this
/// much fuel (about 3 ns), so a world of 50,000 objects costs 400,000 fuel a query.
pub const FUEL_PER_SCANNED_OBJECT: u64 = 8;

/// Log lines and faults kept for each plugin (`pcds plugin logs`, the dashboard).
pub const LOG_KEPT: usize = 200;
pub const FAULTS_KEPT: usize = 20;

/// `net.http` (docs/plugins.md): requests a plugin may have alive (queued or running) and may start
/// in a second; the threads that serve every plugin and the queue before them; redirects followed.
pub const HTTP_IN_FLIGHT: u32 = 2;
pub const HTTP_PER_SECOND: u32 = 10;
pub const HTTP_WORKERS: usize = 4;
pub const HTTP_QUEUE: usize = 32;
pub const HTTP_REDIRECTS: usize = 3;
/// Answers handed to plugins between two game ticks.
pub const HTTP_DELIVER: usize = 8;
/// The wall clock of one request (DNS to the last byte), by default and at most.
pub const HTTP_TIMEOUT_MS: u32 = 10_000;
pub const MAX_HTTP_TIMEOUT_MS: u32 = 30_000;
/// The answer's body a plugin takes, by default and at most (an answer over it is an error).
pub const HTTP_RESPONSE_BYTES: u32 = 65_536;
pub const MAX_HTTP_RESPONSE: u32 = MAX_PAYLOAD as u32;
/// One request: its URL, the most a header's value (and the sum of the answer's headers) may hold,
/// its body, and the encoded request altogether (the body and room for the rest).
pub const MAX_HTTP_URL: usize = 2048;
pub const MAX_HTTP_HEADER_BYTES: usize = 8192;
pub const MAX_HTTP_BODY: usize = 1 << 20;
pub const MAX_HTTP_REQUEST: usize = MAX_HTTP_BODY + (128 << 10);

/// This many faults within FAULT_WINDOW switch the plugin off until it is reloaded or enabled again.
pub const MAX_FAULTS: usize = 5;
pub const FAULT_WINDOW: Duration = Duration::from_secs(60);

/// Plugins loaded at most.
pub const MAX_PLUGINS: usize = 64;

/// What one plugin may use, after the manifest and the admin's overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub fuel_per_event: u64,
    pub fuel_per_save: u64,
    pub memory_pages: u32,
    pub tick_every_ms: u32,
}

impl Limits {
    /// The manifest's request (or the defaults) under the host's maxima, then under the admin's
    /// own lowering.
    pub fn resolve(asked: &super::manifest::Budget, admin: &super::approvals::Overrides) -> Limits {
        let low = |a: Option<u64>, o: Option<u64>, default: u64, max: u64| a.unwrap_or(default).min(max).min(o.unwrap_or(u64::MAX)).max(1);
        Limits {
            fuel_per_event: low(asked.fuel_per_event, admin.fuel_per_event, FUEL_PER_EVENT, MAX_FUEL_PER_EVENT),
            fuel_per_save: low(asked.fuel_per_save, admin.fuel_per_save, FUEL_PER_SAVE, MAX_FUEL_PER_SAVE),
            memory_pages: low(asked.memory_pages.map(u64::from), admin.memory_pages.map(u64::from), MEMORY_PAGES as u64, MAX_MEMORY_PAGES as u64) as u32,
            // A tick interval is the other way round: the admin can only make it longer.
            tick_every_ms: asked.tick_every_ms.unwrap_or(TICK_EVERY_MS).max(admin.tick_every_ms.unwrap_or(0)).max(MIN_TICK_MS),
        }
    }
}
