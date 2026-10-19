# Plugin API reference

The exact interface between the server and a plugin. The design and the reasons are in
docs/plugins.md; a first plugin, step by step, is in docs/plugins-guide.md. Parts of this page are
checked against the code by a test (`plugins::tests::the_api_document_matches_the_code`): the
event kinds, capabilities, imports, exports, limits, endpoints and the version below cannot
drift from what the server does.

Versions in this release:

| What | Version |
|---|---|
| plugin ABI | 1.0 (`ABI 1.0`; `pcds_plugin_api::ABI`, `0x0001_0000`) |
| `pcds-plugin-api` crate | 0.1.0 |
| manifest (`plugin.toml`) | the keys below; unknown keys are refused |
| snapshot | `handoff::FORMAT` 3, with an optional `ext.plugins` part (docs/plugins.md, "The exact compatibility rule") |

## A plugin is

- **a WebAssembly module** (`kind = "wasm"`): a core module (MVP plus what current compilers emit
  for `wasm32-unknown-unknown`: sign extension, multi-value, bulk memory, mutable globals), no WASI,
  that imports from the module name `pcds` only and exports what is listed below;
- **a rules file** (`kind = "data"`): TOML the host runs itself (see "Rules files");
- or **a program** that reads the event stream and uses the admin API. It has no manifest, no
  capabilities and no budget, and holds the admin token.

The first two live in a folder `plugins/<id>/` beside the rules file (`run/plugins/` of an
installation). The folder's name is the plugin's id. It holds `plugin.toml`, the entry file, and a
`data/` folder the host keeps the plugin's state in.

## The module's exports

| Export | Signature (wasm types) | Required | When |
|---|---|---|---|
| `memory` | a linear memory | yes | the host reads and writes event bytes, state and strings in it |
| `pcds_abi` | `() -> i32` | yes | at the start: `major << 16 | minor` the module was built for |
| `pcds_alloc` | `(len: i32) -> i32` | yes | before each event and each state hand-over: a buffer of `len` bytes (0 only if `len` is 0) |
| `pcds_free` | `(ptr: i32, len: i32)` | no | after: gives the buffer back |
| `pcds_event` | `(kind: i32, ptr: i32, len: i32) -> i32` | yes | for each event: the payload is at `ptr`, `len` bytes. Returns 0 (continue) or 1 (veto, only for `rpc_before`); any other value is a fault, a negative one meaning the plugin reports an error itself |
| `pcds_load` | `(ptr: i32, len: i32, version: i32) -> i32` | no | once, after the start; `ptr` is 0 when there is no state. Returns 0, or a negative code: a fault, the plugin does not start |
| `pcds_save` | `() -> i64` | no | at every world save and before a hot update: the state as `ptr << 32 | len` in memory (0: none). The host copies it, then calls `pcds_free` |
| `pcds_state_version` | `() -> i32` | no | after `pcds_save`: the version of the bytes it gave |

The `export!` macro of `pcds-plugin-api` makes all of these from a type that implements `Plugin`.
A module for another language implements them by hand; the table is the whole contract.

The host accepts a module whose `pcds_abi` has the same major and a minor no newer than the
server's (`ABI 1.0` today). It refuses a module that imports anything but the seven functions
below, with the name of the import in the message.

## The module's imports (module `pcds`)

Pointers are `i32` offsets in the module's memory, lengths `i32` byte counts, strings UTF-8.
A pointer range outside the memory **traps** the call (a fault): the host never reads outside
the module, and a module that asks it to has a bug. Every function returns `0` for done, or a
negative code (below).

| Import | Signature | Capability | What |
|---|---|---|---|
| `pcds.log` | `(level: i32, ptr: i32, len: i32)` | none | a line in the server's log, prefixed with `plugin ID:`. Levels 0 error, 1 warn, 2 info, 3 debug. 20 lines per second, 240 characters, control characters become spaces |
| `pcds.query` | `(kind: i32, arg: i32, arg_len: i32, out: i32, cap: i32) -> i32` | `world.read` | writes the answer into `out` (at most `cap` bytes) and returns its full length; a length over `cap` means it did not fit, call again with a bigger buffer. `arg` is unused in ABI 1.0 (pass 0, 0) |
| `pcds.say` | `(ptr: i32, len: i32) -> i32` | `chat.say` | a chat message from the server to everyone (500 characters at most) |
| `pcds.tell` | `(client: i64, ptr: i32, len: i32) -> i32` | `chat.say` | the same, to one player |
| `pcds.kick` | `(client: i64, ptr: i32, len: i32) -> i32` | `player.kick` | kicks a client with the reason as the player's message (128 characters) |
| `pcds.teleport` | `(client: i64, x: f32, y: f32, z: f32) -> i32` | `player.teleport` | moves a player; finite numbers within 100,000 of the origin |
| `pcds.rules_patch` | `(ptr: i32, len: i32) -> i32` | `rules.write` | a JSON merge patch of the rules, as `pcds rules set` sends it, limited to the gameplay sections; two a second |

Codes: `0` done, `-1` denied (the capability is not granted), `-2` bad arguments, `-3` the action
queue is full (16 per call, 200 per second), `-4` unsupported (the host does not have it, or there
is no world to read now: inside `pcds_save` and `pcds_load`).

Actions (`say`, `tell`, `kick`, `teleport`, `rules_patch`) are **queued**: the host applies them
after the call, through the admin API's code, and checks again then (the player may have left). A
call that faults has its queued actions dropped.

## Events

`pcds_event` gets the kind and the payload. The kind numbers are never reused. A payload is the
fields in order, in the encoding of `pcds_plugin_api::codec`: integers little-endian, `str` a `u32`
byte length and UTF-8, `bool` one byte, `f32` and `f64` IEEE. **A newer server may add fields at the
end; a module ignores bytes after the fields it knows**, and must answer 0 to a kind it does not
know.

| Kind | Name in the manifest | Needs | Fields, in order |
|---|---|---|---|
| 1 | `tick` | `event.tick` | `game_ms: i64`, `tick: i32`, `players: u32`, `since_ms: u32` (game time since this plugin's last tick) |
| 2 | `player_join` | `event.player` | `client: u64`, `name: str`, `steam_id: str` (empty unless verified) |
| 3 | `player_leave` | `event.player` | `client: u64`, `name: str` |
| 4 | `chat` | `event.chat` | `client: u64`, `name: str`, `text: str` |
| 5 | `command` | `event.chat` | `client: u64`, `name: str`, `command: str`, `argc: u32`, `argc` times `arg: str`, `text: str` |
| 6 | `rpc_before` | `event.rpc` (`rpc.veto` to refuse) | `client: u64`, `rpc: str`, `object: u64`, `args_json: str` |
| 7 | `rpc_after` | `event.rpc` | `client: u64`, `rpc: str`, `object: u64`, `args_json: str` |
| 8 | `object_placed` | `event.object` | `id: i32`, `group: str`, `x: f32`, `y: f32`, `z: f32` |
| 9 | `object_destroyed` | `event.object` | `id: i32`, `group: str`, `x: f32`, `y: f32`, `z: f32` |
| 10 | `world_saved` | `event.world` | `game_ms: i64`, `ok: bool` |
| 11 | `rule_change` | `event.world` | `changes_json: str` (an object of dotted rule names to values, the ones that differ from the defaults, gameplay sections only) |

- `command` is not in `events.subscribe`: the manifest's `[[command]]` entries declare it, and
  only the plugin that owns a command is called for it. A chat message is a command when it
  begins with the command prefix (the `chat.commandPrefix` rule, else `!`) followed at once by
  the name; `name` is matched without regard to case. Two plugins declaring one command: the first by
  id gets it and the other is told in its warnings.
- `rpc_before` and `rpc_after` are delivered for the RPC names in `events.rpc` only, e.g.
  `ChatHandler.SendChatMessageServerRpc`; `args_json` is the decoded arguments as a JSON array. An
  RPC refused by a veto does not run, and gets no `rpc_after`.
- `tick` is called when `tick_every_ms` of game time have passed since the plugin's last one, at
  most once per game tick, and not while the world stands still.
- `object_placed` is not sent for the objects of a loaded world or for a restored one.
- Events caused by a plugin's own actions (a `rule_change` from its `rules_patch`) are not
  delivered to plugins during that call, so plugins cannot loop on themselves.

## Queries (capability `world.read`)

| Kind | Const | Answer |
|---|---|---|
| 1 | `Q_WORLD` | `game_ms: i64`, `planet: str`, `players: u32`, `world_objects: u32`, `network_objects: u32`, `terraformation: f64`, `standing_still: bool` |
| 2 | `Q_PLAYERS` | `count: u32`, then for each: `client: u64`, `name: str`, `steam_id: str`, `x: f32`, `y: f32`, `z: f32`, `oxygen: f32`, `water: f32`, `health: f32`, `toxic: f32` |
| 3 | `Q_RULES` | the rules that differ from the defaults, as JSON text (no length; the gameplay sections only) |

## Capabilities

| Capability | Allows |
|---|---|
| `event.tick` | be called every few seconds of game time |
| `event.player` | hear when players join and leave |
| `event.chat` | read chat messages and chat commands |
| `event.rpc` | observe the client RPCs it names |
| `event.object` | hear when buildings and objects are placed and destroyed |
| `event.world` | hear about saves and rule changes |
| `rpc.veto` | refuse the client RPCs it names before they run |
| `world.read` | read the world: time, players, positions, counts, rules |
| `chat.say` | write to the chat, to everyone or to one player |
| `player.kick` | kick players |
| `player.teleport` | move players |
| `rules.write` | change the server rules (gameplay sections only: `meteo`, `gauges`, `speed`, `death`, `loot`, `limits`, `world`) |

A subscription needs the capability of its event kind; the manifest is refused if it subscribes
without asking. The admin may grant less than was asked; events whose capability is not granted
are not delivered, and calls whose capability is not granted return `-1`.

## The manifest: `plugin.toml`

```toml
[plugin]
id = "hello-stats"             # the folder's name; a-z, digits, dashes, 32 at most
name = "Hello stats"           # 64 characters at most
version = "0.1.0"              # three numbers
description = "..."
authors = ["..."]
license = "MIT"
kind = "wasm"                  # "wasm" or "data"
entry = "plugin.wasm"          # the module, or the rules file; a path inside the folder
abi = 1                        # the ABI major the plugin was built for
min_server = "0.7.0"           # optional: the oldest server it runs on
capabilities = ["event.chat", "world.read", "chat.say"]

[budget]                       # all optional: what it asks for
fuel_per_event = 500000
fuel_per_save = 2000000
memory_pages = 64
tick_every_ms = 1000

[events]                       # wasm only
subscribe = ["tick", "player_join"]       # kinds from the table, except command
rpc = ["ChatHandler.SendChatMessageServerRpc"]   # with rpc_before / rpc_after

[[command]]                    # wasm only; a data plugin's commands are in its rules file
name = "stats"
help = "the world's numbers"
```

Unknown keys are an error. The id must be the folder's name. `abi` must equal the server's major;
`min_server` newer than the server makes the plugin `incompatible`. `events.rpc` and a subscription
to `rpc_before` or `rpc_after` go together.

### `approved.json`

Written by `pcds plugin enable` and the admin API; the server only reads it.

```json
{"hello-stats": {"enabled": true, "grant": ["event.chat", "chat.say"], "digest": "9f2c...",
                 "limits": {"fuelPerEvent": 250000}, "approvedAt": 1760781234}}
```

`limits` (optional) lowers the plugin's budget (`fuelPerEvent`, `fuelPerSave`, `memoryPages`) or
lengthens its tick (`tickEveryMs`); it can not raise anything.

## Rules files (`kind = "data"`)

```toml
[vars]                          # your own names for {braces}
discord = "https://discord.gg/example"

[[command]]                     # !rules
name = "rules"
reply = "Be kind. More at {discord}"     # to the player who asked
say = "..."                              # and/or to everyone
cooldown_seconds = 5                      # per player

[[on_join]]                     # [[on_leave]] is the same with say only
say = "Welcome, {name}! {players} online."
tell = "Type !rules"

[[every]]                       # announcements; the first comes after one interval
seconds = 900
say = "Join {discord}"

[[when_players]]                # rule changes by the number of players
at_least = 6
set_rules = { speed = { growth = 2.0 } }
otherwise_rules = { speed = { growth = 1.0 } }
```

Variables: `{name}` `{client}` `{players}` `{planet}` `{time}` `{args}` `{arg1}` `{arg2}` `{arg3}`
and your `[vars]`; `{{` and `}}` are braces. `{players}`, `{planet}` and `{time}` need
`world.read`; a file that uses a variable that does not exist is refused. The capabilities a file
needs (events, `chat.say` for any saying, `rules.write` for `when_players`) are worked out from its
content, and the manifest must ask for all of them.

## Limits

The host's numbers; a manifest may ask for less (or more, up to the maximum), the admin may lower
them (`approved.json`).

| Limit | Value | Meaning |
|---|---|---|
| `FUEL_PER_EVENT` | 1000000 | fuel for one event by default; about 0.3 ms of plain arithmetic |
| `MAX_FUEL_PER_EVENT` | 20000000 | the most a manifest may ask for |
| `FUEL_PER_SAVE` | 5000000 | fuel for `pcds_save` |
| `MAX_FUEL_PER_SAVE` | 50000000 | |
| `FUEL_LOAD` | 100000000 | for the module's start and `pcds_load` |
| `MEMORY_PAGES` | 64 | pages of 64 KiB (4 MiB) by default |
| `MAX_MEMORY_PAGES` | 1024 | 64 MiB |
| `MAX_MODULE_BYTES` | 8388608 | a bigger module is not loaded |
| `TICK_EVERY_MS` | 1000 | default tick interval |
| `MIN_TICK_MS` | 5 | the least: every game tick |
| `ACTIONS_PER_CALL` | 16 | queued actions in one call |
| `ACTIONS_PER_SECOND` | 200 | per plugin |
| `RULE_CHANGES_PER_SECOND` | 2 | rule changes per plugin: each writes `rules.json` |
| `LOG_LINES_PER_SECOND` | 20 | per plugin |
| `LOG_LINE_CHARS` | 240 | |
| `SAY_CHARS` | 500 | one chat line |
| `MAX_STATE_BYTES` | 1048576 | a plugin's saved state |
| `MAX_PAYLOAD` | 1048576 | an event payload or a query answer |
| `MAX_FAULTS` | 5 | faults within the window that switch a plugin off |
| `MAX_PLUGINS` | 64 | plugins loaded |

Times: all plugins together may use `TICK_BUDGET` (2 ms) of a game tick and `FLIP_BUDGET` (25 ms)
in a hot update; a call over `SLOW_CALL` (5 ms) is counted slow; `FAULT_WINDOW` is 60 seconds.

## The admin API

All answer JSON; the bearer token is the admin token (docs/hot-update.md, README "Administration").

| Request | Body | Answer |
|---|---|---|
| `GET /api/plugins` | | `{"plugins": [...], "folder", "approvalsError", "capabilities": [{"name","allows"}], "abi": {"major","minor"}, "stream": {"epoch"}}`. Each plugin: `id name version description authors license kind status detail requested granted notGranted commands events warnings limits stats digest`; `stats`: `calls faults restarts slow shed denied refused actions vetoes fuel averageMicros maxMicros lastError lastFaultAt` |
| `POST /api/plugins/reload` | | `{"ok": true, "message"}`: the folder is read again; a plugin whose files and approval did not change keeps running |
| `POST /api/plugins/enable` | `{"id", "grant": [...]}` (grant optional: everything asked) | `{"ok": true, "plugin": {...}}`; 400 for an unknown plugin, a grant above what is asked, or a plugin that does not hold up |
| `POST /api/plugins/disable` | `{"id"}` | `{"ok": true, "plugin": {...}}` |
| `POST /api/plugins/events` | `{"after": 0, "types": ["chat","player_join"], "limit": 200}` | `{"epoch", "events": [...], "last", "dropped"}` |

Status values: `running`, `pending` (never approved), `disabled`, `changed` (files differ from the
approved ones), `invalid`, `incompatible`, `failed` (did not start, or switched off for faults).

`GET /api/status` has a `plugins` object: `loaded`, `running`, `faults`.

### The event stream

`POST /api/plugins/events` records the kinds you ask for from the first call on, for the minute
after each call; with nobody asking, nothing is recorded. Streamable kinds: `player_join`,
`player_leave`, `chat`, `object_placed`, `object_destroyed`, `world_saved`, `rule_change` (default:
all but the objects). The ring keeps the last 512 events; `dropped` counts the ones lost. Each
event is `{"id", "time" (Unix ms), "type", ...}`:

| `type` | fields |
|---|---|
| `chat` | `client`, `name`, `text` |
| `player_join` | `client`, `name`, `steamId` (null unless verified) |
| `player_leave` | `client`, `name` |
| `object_placed`, `object_destroyed` | `object`, `group`, `position` |
| `world_saved` | `gameSeconds`, `ok` |
| `rule_change` | `changes` (dotted rule names to values) |

Ids go on across hot updates; `epoch` (when the ring began, Unix ms) changes when the server
restarts and the ids begin again: start from `after: 0` when it does.

## Versioning and compatibility

Three numbers, each with its own rule.

**The ABI** (`major.minor`, in `pcds_abi`). *Minor* rises for additions that do not break a
module built for the earlier minor: a new event kind, a new field at the end of an event, a new
query kind, a new import, a new capability. *Major* rises for anything else: a changed or removed
import or export, a changed meaning, a reordered field. The server runs a module of the same major
and a minor not newer than its own. Because modules ignore bytes after the fields they know and
answer 0 to kinds they do not know, a module built for 1.0 runs on every 1.x server; a module
built for 1.3 refuses cleanly on a 1.1 server ("built for plugin ABI 1.3; this server speaks
1.1"). A new import a module uses that an old server lacks is also named in the message.
A new *major* is run by a server that carries both for one release before the old one is
dropped; the release notes say so.

**The `pcds-plugin-api` crate** follows the ABI: `0.x` while the ABI is 1.x and the API is
young (a `0.x` minor may change Rust signatures but never the ABI's meaning), then the crate's
major follows the ABI's major.

**The server's release** (README, `pcds upgrade`) is separate. A plugin states the oldest server
it runs on (`min_server`), and the newest it was tested on is the plugin's to say. Changes to the
API are listed in the release notes under "Plugins". `pcds plugin check` on the new server tells an
admin which installed plugins still hold up before a hot update. A hot update to a release with
another ABI major starts the new bank with plugins that do not fit as `incompatible`; the world and
the other plugins go on.

**Deprecation**: a function or event kind is marked deprecated for at least one minor release
before a major removes it; a deprecated one keeps working and logs once per start per plugin that
uses it.

**What does not change within a major**: the names and numbers of the kinds, the order of existing
fields, the meaning of return codes, the capability names, the manifest keys that exist, the
`approved.json` format, the endpoints above and their fields (new fields are added).

## Faults, in the log

All lines start `plugin ID:`. The ones the host writes:

| Line | Meaning |
|---|---|
| `plugin ID: EVENT: out of fuel (the call did more than its budget)` | fuel ran out in the event; the plugin restarts from its last saved state |
| `plugin ID: EVENT: trap: ...` | the module trapped |
| `plugin ID: EVENT: bad answer: ...` | the answer was no answer |
| `plugin ID: switched off after 5 faults in 60 s; the last, in EVENT: ...` | the breaker; `pcds plugin reload` starts it |
| `plugin ID: start failed: ...` | the module did not start |
| `plugin ID: EVENT took N ms (slow calls are counted...)` | a call over 5 ms |
| `plugin ID: no time to save its state for the hand-over; ...` | the flip's budget was spent first |
| `plugin ID: rules patch refused: ...` | `rules_patch` outside the gameplay sections |
