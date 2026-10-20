# Plugin API reference

The exact interface between the server and a plugin. The design and the reasons are in
docs/plugins.md; a first plugin, step by step, is in docs/plugins-guide.md. Parts of this page are
checked against the code by a test (`plugins::tests::the_api_document_matches_the_code`): the
event kinds, capabilities, imports, exports, limits, endpoints and the version below cannot
drift from what the server does.

Versions in this release:

| What | Version |
|---|---|
| plugin ABI | 1.2 (`ABI 1.2`; `pcds_plugin_api::ABI`, `0x0001_0002`) |
| `pcds-plugin-api` crate | 0.1.0 |
| manifest (`plugin.toml`) | the keys below; unknown keys are refused |
| snapshot | `handoff::FORMAT` 5, with the `ext.plugins` part (docs/plugins.md, "The compatibility rule") |

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
| `pcds_unload` | `() -> i32` | no | ABI 1.1: the last call before the instance is dropped (the plugin is switched off, its folder removed, or it is switched off for faults); then `pcds_save` is asked once more. Not called at a reload (a new version takes over with the state from `pcds_save`). It has the fuel of a save; a trap or a loop in it is logged and the state of the last save is kept. What it queues is never applied |

The `export!` macro of `pcds-plugin-api` makes all of these from a type that implements `Plugin`.
A module for another language implements them by hand; the table is the whole contract.

The host accepts a module whose `pcds_abi` has the same major and a minor no newer than the
server's (`ABI 1.2` today). It refuses a module that imports anything but the eight functions
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
| `pcds.http_request` | `(ptr: i32, len: i32) -> i64` | `net.http` | ABI 1.2. Starts an HTTP request (an encoded `HttpRequest`, at most `MAX_HTTP_REQUEST` bytes) and returns its ticket, a positive number; the answer comes later as the event `http_done`. Returns a code (below) when the request is turned down at once. See "`net.http`" |

Codes: `0` done, `-1` denied (the capability is not granted), `-2` bad arguments, `-3` the action
queue is full (16 per call, 200 per second; for `http_request`: too many requests alive, or in this
second), `-4` unsupported (the host does not have it, or there is no world to read now: inside
`pcds_save` and `pcds_load`), `-5` not allowed (ABI 1.2; `http_request`: the host is not on the
manifest's `[net] allow` list).

Actions (`say`, `tell`, `kick`, `teleport`, `rules_patch`, `http_request`) are **queued**: the host applies them
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
| 12 | `http_done` | `net.http` | ABI 1.2. `ticket: u64`, `status: u32` (the HTTP status; 0 when there was no answer), `error: str` (empty when there was an answer; else a short fixed phrase), `body: bytes` (a `u32` length and the bytes), `headers`: `count: u32`, then `count` times `name: str`, `value: str` (names in lowercase) |

- `http_done` is not in `events.subscribe` either: it is delivered to the plugin that made the request, whatever it subscribed to, and to nobody else.
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
| `net.http` | send HTTP requests to the hosts its manifest lists |
| `net.http.local` | let those requests reach this machine and the local network |

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

[net]                          # only with the capability net.http (see "net.http")
allow = ["https://discord.com", "https://*.example.org:8443"]   # scheme://host[:port]; the only places it may go
timeout_ms = 10000             # optional: the wall clock of a request (30000 at most)
max_response_bytes = 65536     # optional: the most of an answer it takes (1048576 at most)

[[config]]                     # values the admin sets (pcds plugin config set ID KEY), put into requests as ${config.KEY}
key = "webhook"
description = "the path of the Discord webhook"
secret = true                  # the default: never shown, never logged, never in the plugin's memory
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

## `net.http`

A plugin that asks for `net.http` may send HTTP and HTTPS requests to the places its manifest lists,
and only those. It does it with `pcds.http_request` (`Host::http_request` in the crate), which takes
an encoded `HttpRequest` (`method`, `url`, `headers`, `body`: the codec's `str`, `str`, a header
count and pairs of `str`, and `bytes`) and returns a **ticket**. Nothing is sent from inside the
call: the host's worker threads send it, and the answer is delivered some milliseconds to seconds
later as the event `http_done` (kind 12) to the same plugin, in a later call, between two game ticks.
A request in flight when the server restarts or is hot-updated is lost without an event (keep your
own deadline in `tick`).

*Where.* `[net] allow` is a list of patterns `scheme://host[:port]` (the port is the scheme's own if
left out). A host is a lowercase DNS name; `*.` in front covers every name below it but not the
name itself, and never a public suffix (`*.com`, `*.co.uk`, `*.github.io` are refused when the
manifest is read; the host knows a short list of those, and the approval screen shows every
pattern). An IP address in a pattern is refused unless the plugin also asks for `net.http.local`.
The list is part of the manifest, so the approval's digest pins it: a plugin that changes its list
waits for the admin again. The URL of every request, and of every redirect, is matched against it; a
URL with a user name, a fragment or a character outside ASCII is refused. A redirect is followed (up
to `HTTP_REDIRECTS`) only if its target matches too, and then without `Authorization` and `Cookie`
when it leaves the first host.

*Which machine.* The host resolves the name itself and connects to what it resolved. Loopback,
private (RFC 1918 and unique-local), link-local (the cloud metadata address among them), shared
(`100.64.0.0/10`), documentation, multicast, reserved and unspecified addresses, the IPv6 ranges
outside `2000::/3`, and 6to4 are refused, unless the admin granted `net.http.local` as well (and
unspecified, multicast and the reserved ranges even then). A name with one such address among its
answers is refused whole; the lookup is done again for every connection and every redirect, and no
connection is kept for the next request. There is no proxy, and TLS is always verified against the
Mozilla roots.

*How much.* A request is at most `MAX_HTTP_REQUEST` bytes (a URL of `MAX_HTTP_URL`, 32 headers,
a header value of `MAX_HTTP_HEADER_BYTES`, a body of `MAX_HTTP_BODY`). `Host`, `Content-Length`,
`Connection`, `Transfer-Encoding`, `Upgrade`, `Expect`, `TE`, `Trailer` and `Proxy-Authorization`
are the host's: a request that sets one is refused. The methods are GET, HEAD, POST, PUT, PATCH and
DELETE. The answer's body is cut at `max_response_bytes` (an answer over it is an error, never a short
body). One wall clock (`timeout_ms`, `HTTP_TIMEOUT_MS` by default) covers the lookup, the connection,
TLS, sending and reading, so a server that trickles bytes is let go at the timeout. A plugin has
`HTTP_IN_FLIGHT` requests alive and starts `HTTP_PER_SECOND` in a second; `HTTP_WORKERS` threads serve
all plugins behind a queue of `HTTP_QUEUE`; a request the queue has no room for is answered at
once with the error "the server is busy with other requests".

*What the plugin is told.* `http_request` returns at once for what can be known without the network:
`-1` (not granted), `-2` (the request is no good: method, URL, headers, size, a `${config.KEY}`
that is not set), `-5` (not on the list), `-3` (too many alive, or this second). The rest arrives in
`http_done`: `status` is the server's, or 0 and `error` one of a few fixed phrases (`timed out`,
`host not found`, `connection refused`, `connection reset`, `TLS failed ...`, `the answer is larger
than the limit`, `redirected to a host that is not on the allow list`, `too many redirects`, `the
name resolves to an address the plugin may not reach ...`). Never the URL, never an address.

*Values and secrets.* The manifest declares `[[config]]` keys; the admin sets them with `pcds plugin
config set ID KEY` (stored 0600 in `config.json` beside `approved.json`). A request may hold
`${config.KEY}` in the path, the query and header values (not the host); the host puts the value in
after the plugin's call. The plugin never has the value, and a secret (the default) is in no log
line, listing, event or stream. One line of the log for each request: `plugin ID: http METHOD
scheme://host[:port] -> HTTP 204 (12 ms, 0 bytes)`, never the path or query.

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
| `HTTP_IN_FLIGHT` | 2 | `net.http` requests of one plugin alive (queued or running) |
| `HTTP_PER_SECOND` | 10 | requests one plugin starts in a second |
| `HTTP_WORKERS` | 4 | threads that send the requests of all plugins |
| `HTTP_QUEUE` | 32 | requests waiting for a thread, all plugins |
| `HTTP_REDIRECTS` | 3 | redirects followed |
| `HTTP_DELIVER` | 8 | answers handed to plugins between two ticks |
| `HTTP_TIMEOUT_MS` | 10000 | wall clock of a request by default |
| `MAX_HTTP_TIMEOUT_MS` | 30000 | the most a manifest may ask for |
| `HTTP_RESPONSE_BYTES` | 65536 | the answer's body a plugin takes by default |
| `MAX_HTTP_RESPONSE` | 1048576 | the most a manifest may ask for |
| `MAX_HTTP_URL` | 2048 | bytes of a URL |
| `MAX_HTTP_HEADER_BYTES` | 8192 | bytes of a header value (and of all the answer's headers) |
| `MAX_HTTP_BODY` | 1048576 | bytes of a request's body |
| `MAX_HTTP_REQUEST` | 1179648 | bytes of an encoded request |

Times: all plugins together may use `TICK_BUDGET` (2 ms) of a game tick and `FLIP_BUDGET` (25 ms)
in a hot update; a call over `SLOW_CALL` (5 ms) is counted slow; `FAULT_WINDOW` is 60 seconds.

## The admin API

All answer JSON; the bearer token is the admin token (docs/hot-update.md, README "Administration").

| Request | Body | Answer |
|---|---|---|
| `GET /api/plugins` | | `{"plugins": [...], "folder", "approvalsError", "capabilities": [{"name","allows"}], "abi": {"major","minor"}, "stream": {"epoch"}}`. Each plugin: `id name version description authors license kind status detail requested granted notGranted commands events warnings limits stats digest net config`; `stats`: `calls faults restarts slow shed denied refused actions vetoes fuel averageMicros maxMicros lastError lastFaultAt http`; `stats.http`: `requests answered failed bytesOut bytesIn inFlight`; `net` (null without `[net]`): `allow wildcards local localGranted timeoutMs maxResponseBytes`; `config`: the declared keys, `key description secret set`, and the `value` of a key that is not a secret |
| `POST /api/plugins/reload` | | `{"ok": true, "message"}`: the folder is read again; a plugin whose files and approval did not change keeps running |
| `POST /api/plugins/enable` | `{"id", "grant": [...]}` (grant optional: everything asked) | `{"ok": true, "plugin": {...}}`; 400 for an unknown plugin, a grant above what is asked, or a plugin that does not hold up |
| `POST /api/plugins/disable` | `{"id"}` | `{"ok": true, "plugin": {...}}` |
| `POST /api/plugins/events` | `{"consumer": "relay", "after": 0, "types": ["chat","player_join"], "limit": 200}` or `{"consumer": "relay", "unregister": true}` | `{"epoch", "events": [...], "last", "dropped"}`, or `{"ok", "registered": false, "was"}` |

Status values: `running`, `pending` (discovered, never approved), `disabled`, `changed` (files differ
from the approved ones, and nothing is running), `unloading` (its last call and save are running;
seen in the log, a request never finds it so), `invalid`, `incompatible`, `failed` (did not start,
or switched off for faults). Each change of state is one log line, `plugin ID: STATE`.
A `running` plugin may have a `notice`: its files changed and wait for approval, or a reload of
them failed and the old version goes on. `runtime` is `{"instances", "memoryPages"}`: what the
running modules hold; it is zero once they are all unloaded. `scanEverySeconds` is how often the
bank looks at the folder by itself (2); a change is acted on when two looks in a row see it.

`GET /api/status` has a `plugins` object: `loaded`, `running`, `faults`.

### The event stream

`POST /api/plugins/events` registers you as a *consumer* (`consumer`: 1 to 32 letters, digits, `.`
`_` or `-`; default `anonymous`) and records the kinds you ask for from the first call on, for the
minute after each call; you leave with `unregister`, or by not polling for a minute. The listing
(`stream.consumers`) shows each consumer, its kinds and its last poll; arrival and departure are one
log line each. With nobody registered, nothing is recorded. Streamable kinds: `player_join`,
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

**The policy: semver-style, and a major bump breaks plugins.** There is no legacy support, no
shim for an old API, no deprecation period. A plugin is built for one ABI major; the server checks
that one number and refuses a plugin of another major with a plain message ("built for plugin ABI
2; this server speaks 1"). The author rebuilds.

**The ABI** (`major.minor`, in `pcds_abi` and `abi` in the manifest). *Minor* rises for additions
that cost an old module nothing: a new event kind, a new field at the end of an event, a new query
kind, a new import, a new capability. *Major* rises for anything else: a changed or removed import
or export, a changed meaning, a reordered field, and whenever the project finds the old shape in
the way. The server runs a module of the same major and a minor not newer than its own: a module
built for 1.3 is refused cleanly by a 1.1 server ("built for plugin ABI 1.3; this server speaks
1.1"). Modules ignore bytes after the fields they know and answer 0 to kinds they do not know,
so a module built for 1.0 runs on every 1.x server.

**The `pcds-plugin-api` crate** follows the ABI: `0.x` while the ABI is 1.x and the API is young
(a `0.x` minor may change Rust signatures), then its major follows the ABI's major.

**The server's release** (README, `pcds upgrade`) is separate. A plugin states the oldest server
it runs on (`min_server`). Changes to the API are listed in the release notes under "Plugins";
`pcds plugin check` on the new server tells an admin which installed plugins still hold up. A
release with another ABI major starts its plugins that do not fit as `incompatible` or `failed`;
the world and the other plugins go on.

**Loading and unloading** are not versioned: a plugin of the right ABI major is loaded and unloaded
at any time, with the folder scanned every 2 seconds (docs/plugins.md, "Loading and unloading while
the world runs"). `pcds_unload` is ABI 1.1 and `net.http` (`pcds.http_request`, the event `http_done`, the code `-5`) is ABI 1.2; a module built for 1.0 or 1.1 loses nothing.

**The snapshot.** The plugins' state is part of the current snapshot format (`ext.plugins`). A
change to its shape that an older reader cannot read bumps `handoff::FORMAT`, which means the
front refuses a hot update across it and the server is restarted (`pcds upgrade --restart`); there
is no second reader kept for the old shape. A plugin's own state bytes are its author's: it raises
its state version and reads the old one in `load`, or starts empty.

**What does not change within a major**: the names and numbers of the kinds, the order of
existing fields, the meaning of return codes, the capability names, the manifest keys that exist,
the `approved.json` format, the endpoints above and their fields (new fields are added).

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
