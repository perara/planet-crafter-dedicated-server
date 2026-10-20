# Plugin API reference

The exact interface between the server and a plugin. The design and the reasons are in
docs/plugins.md; a first plugin, step by step, is in docs/plugins-guide.md. Parts of this page are
checked against the code by a test (`plugins::tests::the_api_document_matches_the_code`): the
event kinds, capabilities, imports, exports, limits, endpoints and the version below cannot
drift from what the server does.

Versions in this release:

| What | Version |
|---|---|
| plugin ABI | 1.3 (`ABI 1.3`; `pcds_plugin_api::ABI`, `0x0001_0003`) |
| `pcds-plugin-api` crate | 0.2.0 |
| manifest (`plugin.toml`) | the keys below; unknown keys are refused |
| snapshot | `handoff::FORMAT` 7, with the `ext.plugins` part (docs/plugins.md, "The compatibility rule") |

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
server's (`ABI 1.3` today). A module built for 1.0, 1.1 or 1.2 runs unchanged: a new server only adds to
what it sends and answers. It refuses a module that imports anything but the twelve functions below,
with the name of the import in the message, and a module built for a newer minor with both versions
in the message ("built for plugin ABI 1.4; this server speaks 1.3").

The module is translated when it is read, all of it, not function by function when each is first
called: the first call of an instance costs the fuel the next ones do. Before ABI 1.3 translation
was charged to the call, and a budget that held the steady state could fault the first call.

## The module's imports (module `pcds`)

Pointers are `i32` offsets in the module's memory, lengths `i32` byte counts, strings UTF-8.
A pointer range outside the memory **traps** the call (a fault): the host never reads outside
the module, and a module that asks it to has a bug. Every function returns `0` for done, or a
negative code (below).

| Import | Signature | Capability | What |
|---|---|---|---|
| `pcds.log` | `(level: i32, ptr: i32, len: i32)` | none | a line in the server's log, prefixed with `plugin ID:`. Levels 0 error, 1 warn, 2 info, 3 debug. 20 lines per second, 240 characters, control characters become spaces |
| `pcds.query` | `(kind: i32, arg: i32, arg_len: i32, out: i32, cap: i32) -> i32` | `world.read` (not for kind 7) | writes the answer into `out` (at most `cap` bytes) and returns its full length; a length over `cap` means it did not fit, call again with a bigger buffer (the host keeps the answer for the length of the call, so asking again costs the copy). `arg` is `arg_len` bytes (at most `MAX_QUERY_ARG`) that some kinds take (ABI 1.3); kinds 1 to 3 take none (pass 0, 0) |
| `pcds.say` | `(ptr: i32, len: i32) -> i32` | `chat.say` | a chat message from the server to everyone (500 characters at most) |
| `pcds.tell` | `(client: i64, ptr: i32, len: i32) -> i32` | `chat.say` | the same, to one player |
| `pcds.kick` | `(client: i64, ptr: i32, len: i32) -> i32` | `player.kick` | kicks a client with the reason as the player's message (128 characters) |
| `pcds.teleport` | `(client: i64, x: f32, y: f32, z: f32) -> i32` | `player.teleport` | moves a player; finite numbers within 100,000 of the origin |
| `pcds.rules_patch` | `(ptr: i32, len: i32) -> i32` | `rules.write` | a JSON merge patch of the rules, as `pcds rules set` sends it, limited to the gameplay sections; two a second |
| `pcds.give` | `(client: i64, ptr: i32, len: i32, count: i32) -> i32` | `player.give` | ABI 1.3. Puts `count` (1 to `MAX_GIVE_COUNT`) of the item whose group id is the text (letters, digits, `_`, `-`, at most 64 bytes) into the backpack of a player. Queued; at apply time the player must be online, the item a real item of the game that can be given (not a key or a quest item), and the backpack must have room: the ones that fit are put in. `GIVES_PER_SECOND` a second |
| `pcds.tokens` | `(amount: i32) -> i32` | `world.tokens` | ABI 1.3. Adds Terra tokens to the world's count, or takes them away (negative; the count stops at 0). `1` to `MAX_TOKENS_PER_ACTION` either way. Queued. `TOKEN_CHANGES_PER_SECOND` a second |
| `pcds.timer_set` | `(id: u32, delay_ms: u32, repeat_ms: u32) -> i32` | `event.timer` | ABI 1.3. Calls the plugin with the event `timer` after `delay_ms` of game time, then every `repeat_ms` if that is not 0 (at least `MIN_TIMER_REPEAT_MS`; delay and repeat at most `MAX_TIMER_DELAY_MS`). Setting an id again replaces the timer. `MAX_TIMERS` per plugin. Takes effect at once. A timer set in `pcds_load` starts counting at the first game tick after it |
| `pcds.timer_cancel` | `(id: u32) -> i32` | `event.timer` | ABI 1.3. Cancels a timer; one that is not set is no error |
| `pcds.http_request` | `(ptr: i32, len: i32) -> i64` | `net.http` | ABI 1.2. Starts an HTTP request (an encoded `HttpRequest`, at most `MAX_HTTP_REQUEST` bytes) and returns its ticket, a positive number; the answer comes later as the event `http_done`. Returns a code (below) when the request is turned down at once. See "`net.http`" |

Codes: `0` done, `-1` denied (the capability is not granted), `-2` bad arguments, `-3` the action
queue is full (16 per call, 200 per second; for `http_request`: too many requests alive, or in this
second), `-4` unsupported (the host does not have it, or there is no world to read now: inside
`pcds_save` and `pcds_load`), `-5` not allowed (ABI 1.2; `http_request`: the host is not on the
manifest's `[net] allow` list), `-6` not found (ABI 1.3; the player or object asked about is not there).

Actions (`say`, `tell`, `kick`, `teleport`, `rules_patch`, `give`, `tokens`, `http_request`) are **queued**: the host applies them
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
| 1 | `tick` | `event.tick` | `game_ms: i64`, `tick: i32`, `players: u32`, `since_ms: u32` (game time since this plugin's last tick), `unix_ms: i64` (ABI 1.3: the wall clock) |
| 2 | `player_join` | `event.player` | `client: u64`, `name: str`, `steam_id: str` (empty unless verified), `player_id: u64` (ABI 1.3) |
| 3 | `player_leave` | `event.player` | `client: u64`, `name: str`, `player_id: u64` (ABI 1.3) |
| 4 | `chat` | `event.chat` | `client: u64`, `name: str`, `text: str`, `player_id: u64` (ABI 1.3) |
| 5 | `command` | `event.chat` | `client: u64`, `name: str`, `command: str` (lower case), `argc: u32`, `argc` times `arg: str`, `text: str`, `player_id: u64` (ABI 1.3) |
| 6 | `rpc_before` | `event.rpc` (`rpc.veto` to refuse) | `client: u64`, `rpc: str`, `object: u64`, `args_json: str` |
| 7 | `rpc_after` | `event.rpc` | `client: u64`, `rpc: str`, `object: u64`, `args_json: str` |
| 8 | `object_placed` | `event.object` | `id: i32`, `group: str`, `x: f32`, `y: f32`, `z: f32`, `client: u64` (ABI 1.3: who built or dropped it; 0: nobody) |
| 9 | `object_destroyed` | `event.object` | `id: i32`, `group: str`, `x: f32`, `y: f32`, `z: f32`, `client: u64` (ABI 1.3: who took it apart; 0: it went some other way) |
| 10 | `world_saved` | `event.world` | `game_ms: i64`, `ok: bool` |
| 11 | `rule_change` | `event.world` | `changes_json: str` (an object of dotted rule names to values, the ones that differ from the defaults, gameplay sections only) |
| 12 | `http_done` | `net.http` | ABI 1.2. `ticket: u64`, `status: u32` (the HTTP status; 0 when there was no answer), `error: str` (empty when there was an answer; else a short fixed phrase), `body: bytes` (a `u32` length and the bytes), `headers`: `count: u32`, then `count` times `name: str`, `value: str` (names in lowercase) |
| 13 | `timer` | `event.timer` | ABI 1.3. `id: u32`, `game_ms: i64`, `late_ms: u32` (how long after its time it is delivered) |
| 14 | `flip` | `event.world` | ABI 1.3. `game_ms: i64`, `server_version: str` |

- `player_id` is the player's id in the world's save: the same player has the same id at every visit,
  whatever connection number (`client`) the session has. It is the verified SteamID64 for a player who joined through
  Steam, else a number made from the name. Keep what you remember about a player under it. A chat message from a
  client that is not in the game yet has an empty `name` and `player_id` 0.
- `timer` is not in `events.subscribe`: it is delivered to the plugin that set the timer, whatever it subscribed to, and
  to nobody else. `flip` is: subscribe to it. It is delivered once, to the plugins that were running when a hot update
  handed the world over, after their `pcds_load` and before the world goes on; not at a restart and not at a reload.
- `unix_ms` and `player_id` and the others added in 1.3 come at the end of the event: a module built for an older minor
  ignores them.
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
  most once per game tick, and not while the world stands still. The same goes for timers: they count game
  time, so a world that stands still (paused, or empty with `world.pauseWhenEmpty`) delivers none; use `unix_ms`
  of the next tick for anything that is about the wall clock.
- A `command` reaches the plugin only after the host has checked it against the manifest: a command with
  `permission = "operator"` from a player who is not one is answered by the host (`!name is for the server's
  operators.`), and one with fewer arguments than `min_args` or more than `max_args` is answered with `Use: !name USAGE`.
  The plugin is not called then. `command` is given in lower case, whatever the player typed.
- `object_placed` is not sent for the objects of a loaded world or for a restored one.
- Events caused by a plugin's own actions (a `rule_change` from its `rules_patch`) are not
  delivered to plugins during that call, so plugins cannot loop on themselves.

## Queries

All of them need `world.read` except kind 7. Answers are read-only copies taken at the moment of the call;
asked twice in one call (kinds 1 to 3, no argument) they are the same copy. A query is available in
`pcds_load` too: the host answers it from the world as it is when the plugin starts (not inside `pcds_save`, where
there is no world, and a query there returns `-4`).

| Kind | Const | Argument | Answer |
|---|---|---|---|
| 1 | `Q_WORLD` | none | `game_ms: i64`, `planet: str`, `players: u32`, `world_objects: u32`, `network_objects: u32`, `terraformation: f64`, `standing_still: bool`, `unix_ms: i64` (1.3), `command_prefix: str` (1.3) |
| 2 | `Q_PLAYERS` | none | `count: u32`, then for each: `client: u64`, `name: str`, `steam_id: str`, `x: f32`, `y: f32`, `z: f32`, `oxygen: f32`, `water: f32`, `health: f32`, `toxic: f32`, `player_id: u64` (1.3), `operator: bool` (1.3) |
| 3 | `Q_RULES` | none | the rules that differ from the defaults, as JSON text (no length; the gameplay sections only) |
| 4 | `Q_INVENTORY` | the client, `u64` | ABI 1.3. `slots_used: u32`, `slots_total: u32`, `count: u32`, then `group: str`, `count: u32` for each kind of item, sorted by group id. `-6` if the player is not online |
| 5 | `Q_NEARBY` | `x, y, z, radius: f32`, `max: u32` | ABI 1.3. `count: u32` and for each, the closest first: `id: i32`, `net: u64`, `group: str`, `x, y, z: f32`, `distance: f32`. At most `MAX_NEARBY` objects within `MAX_NEARBY_RADIUS`; only what stands in the world (not what is in a backpack). `-2` for a number that is not finite. The host looks at every object in the world: that costs `FUEL_PER_SCANNED_OBJECT` each |
| 6 | `Q_OBJECT` | a byte, 0 for a world object id (`i32` as `u64`) or 1 for a network object id (`u64`) | ABI 1.3. `found: bool` and, if found, `id: i32`, `net: u64`, `group: str`, `x, y, z: f32`. An RPC's `object` is a network object id |
| 7 | `Q_CONFIG` | none | ABI 1.3. No capability. `count: u32` and `key: str`, `value: str` for each setting the plugin declares that is not a secret, as the admin set it or as its default says (checked and normalised: `true`/`false`, whole numbers) |
| 8 | `Q_PLAYER` | the client, `u64` | ABI 1.3. One `PlayerInfo`, as in kind 2. `-6` if the player is not online. Much cheaper than kind 2 when you know whom you want |

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
| `rules.write` | change the server rules (gameplay sections only: `meteo`, `gauges`, `speed`, `death`, `loot`, `limits`, `world`; not `world.dlcPlanets`, which is the operator's statement of what they own) |
| `net.http` | send HTTP requests to the hosts its manifest lists |
| `net.http.local` | let those requests reach this machine and the local network |
| `event.timer` | set timers and be called when they are due |
| `player.give` | put items into players' backpacks |
| `world.tokens` | add Terra tokens to the world, or take them away |

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
subscribe = ["tick", "player_join"]       # kinds from the table, except command, http_done and timer
rpc = ["ChatHandler.SendChatMessageServerRpc"]   # with rpc_before / rpc_after

[[command]]                    # wasm only; a data plugin's commands are in its rules file
name = "stats"
help = "the world's numbers"
usage = "[player]"             # optional: the arguments, one line; the host adds "!stats" when it tells a player how to type it
min_args = 0                   # optional: fewer or more and the host answers the player and does not call the plugin
max_args = 1
permission = "everyone"        # or "operator": only the players in access.operators (verified SteamIDs)

[net]                          # only with the capability net.http (see "net.http")
allow = ["https://discord.com", "https://*.example.org:8443"]   # scheme://host[:port]; the only places it may go
timeout_ms = 10000             # optional: the wall clock of a request (30000 at most)
max_response_bytes = 65536     # optional: the most of an answer it takes (1048576 at most)

[[config]]                     # a setting the admin gives (pcds plugin config set ID KEY, or the dashboard)
key = "webhook"
description = "the path of the Discord webhook"
secret = true                  # the default for a string: never shown, never logged, never in the plugin's memory;
                               # only put into a request as ${config.KEY}. A secret has no default.

[[config]]                     # a setting the plugin reads: host.config("afk_minutes")
key = "afk_minutes"
description = "minutes without moving before a player is warned"
type = "int"                   # "string" (the default), "int", "float" or "bool"; a number or a yes/no is never a secret
default = 15                   # what the plugin sees when the admin set nothing
min = 1                        # int and float: bounds, both included
max = 240

[[config]]
key = "mode"
type = "string"
secret = false                 # a string setting the plugin may read says so
default = "warn"
choices = ["warn", "kick"]     # the only values; max_len = 200 is the default length limit of a string (2048 for a secret)
```

Unknown keys are an error, with the key that was probably meant (`unknown key capabilites (did you mean
capabilities?)`); so are a capability, an event or a setting's type that does not exist. The id must be the folder's name. `abi` must equal the server's major;
`min_server` newer than the server makes the plugin `incompatible`. `events.rpc` and a subscription
to `rpc_before` or `rpc_after` go together.

**Settings.** The admin's value is checked against the declaration when it is set (the command line, the admin API and
the dashboard all do) and again when the plugin starts: a stored value the declaration no longer accepts (the plugin
was updated to a narrower range) makes the plugin `invalid` with the message, and is not quietly replaced by the default.
The plugin reads its settings with `Host::settings()` / `config(key)` (kind 7 of the queries) in `pcds_load` and in events; a
change of a value restarts the plugin the way a reload does (its state is handed over, its timers are set again by its `load`).
A setting that is a secret is never in that list.

**Operators.** `access.operators` (`pcds rules set access.operators=SteamID,SteamID`, or the dashboard's rules) lists
SteamID64s. Only a player who joined through Steam, where the id is verified, can be an operator; `PlayerInfo.operator` and
`permission = "operator"` follow it. A name cannot be one: a name is anyone's.

### `approved.json`

Written by `pcds plugin enable` and the admin API; the server only reads it.

```json
{"hello-stats": {"enabled": true, "grant": ["event.chat", "chat.say"], "digest": "9f2c...",
                 "limits": {"fuelPerEvent": 250000}, "approvedAt": 1760781234}}
```

`signer` (optional) is the id of the trusted key that had signed the plugin when it was approved (see
"Signing"). `limits` (optional) lowers the plugin's budget (`fuelPerEvent`, `fuelPerSave`, `memoryPages`) or
lengthens its tick (`tickEveryMs`); it can not raise anything.

## Signing

Digest pinning is the default: an approval holds for the files whose digest it recorded. A
publisher can sign a plugin so that an admin who trusts the publisher's key does not approve each
file. A signature only ever adds trust; a plugin whose signature is missing or does not hold is
judged by its pinned digest as before.

- **`plugin.sig`**, in the plugin's folder (`pcds plugin sign --key NAME.key DIR` writes it):
  ```json
  {"publicKey": "9f2c... (64 hex)", "signature": "ab01... (128 hex)", "digest": "5e7d... (the plugin's digest)"}
  ```
  The signature is Ed25519 over the text `pcds-plugin-v1:` and the digest, the sha256 of the manifest,
  a zero byte and the entry file (the `digest` of `approved.json` and of the listing). It is checked
  against the files as they are when they are read, at every start and every reload, so a changed
  manifest (another capability, another allow list) or module is a signature that does not hold.
  At most 4096 bytes are read.
- **`trusted-keys.json`**, beside `approved.json` (`pcds plugin trust`, `untrust`, `keys`, the
  admin API; the server only reads it):
  ```json
  {"keys": [{"id": "ab12...", "publicKey": "9f2c...", "name": "Acme", "grant": ["event.chat", "chat.say"], "addedAt": 1760781234}],
   "revoked": [{"id": "cd34...", "name": "Old key", "at": 1760781300}]}
  ```
  `id` is the first 16 bytes of sha256 of the public key, in hex. `grant` is the key's **trust
  grant**: the most it vouches for. A file that does not read trusts nobody (`keysError`).
- **The rule** (`approvals::standing`). The admin's own `disable` beats everything. A plugin whose
  signature holds and whose key is trusted needs no approval for the capabilities in the key's grant:
  if it asks for nothing else, it runs with no entry in `approved.json` at all, and every new
  version the key signs runs as soon as it is read. What it asks for beyond the grant waits for the
  admin as for any plugin (`pending`); `enable` then records the signer, and from then on a version
  the same key signs runs without a new approval while it asks for nothing beyond the approved
  grant and the key's. A different key, no signature, a signature that does not hold, a key
  untrusted or revoked: the digest is all there is, and the running version of a plugin whose new
  files do not hold goes on with a notice, or stops (a key taken away) at the next look at the folder.
  A key's trust grant is a floor: the admin can switch the plugin off, but not grant it less than the
  key vouches for.
- **Revocation.** `pcds plugin untrust KEY` (or the admin API) removes the key and records it as
  revoked; the plugins that ran only because of it stop within seconds (two looks at the folder), with
  their state saved. Trusting a revoked key again takes `--force`.
- **Keys.** `pcds plugin keygen NAME` writes `NAME.key` (0600; the publisher keeps it) and `NAME.pub`.
  `trust` takes the `.pub` file or its 64 hex digits and shows the key's id.

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
| `MAX_GIVE_COUNT` | 50 | items in one `give` |
| `GIVES_PER_SECOND` | 10 | `give` calls one plugin may queue in a second |
| `MAX_TOKENS_PER_ACTION` | 10000 | Terra tokens one `tokens` call adds or takes |
| `TOKEN_CHANGES_PER_SECOND` | 2 | `tokens` calls one plugin may queue in a second |
| `MAX_TIMERS` | 16 | timers one plugin has |
| `MIN_TIMER_REPEAT_MS` | 100 | the shortest repeat |
| `MAX_TIMER_DELAY_MS` | 604800000 | the longest delay or repeat (seven days) |
| `TIMER_EVENTS_PER_TICK` | 4 | timer events one plugin gets in one game tick |
| `FUEL_PER_SCANNED_OBJECT` | 8 | fuel for each world object `Q_NEARBY` looks at |
| `MAX_QUERY_ARG` | 64 | bytes of a query's argument |
| `MAX_ITEM_NAME_BYTES` | 64 | bytes of an item's group id in `give` |
| `LOG_KEPT` | 200 | log lines kept for each plugin (`pcds plugin logs`) |
| `FAULTS_KEPT` | 20 | faults kept for each plugin |
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
| `GET /api/plugins` | | `{"plugins": [...], "folder", "approvalsError", "capabilities": [{"name","allows"}], "abi": {"major","minor"}, "stream": {"epoch"}}`. Each plugin: `id name version description authors license kind status detail requested granted notGranted commands events warnings limits stats digest net config signature`; `stats`: `calls faults restarts slow shed denied refused actions vetoes fuel averageMicros maxMicros lastError lastFaultAt byEvent faultLog http`, where `byEvent` is a list of `{event, calls, fuel, averageFuel, maxFuel, averageMicros, maxMicros}` for each kind of event the plugin was called for, and `faultLog` the last `FAULTS_KEPT` faults `{at, during, text}`, oldest first; `deniedCapabilities`: the capabilities the plugin used without being granted them (the log says each once); `changes`: null, or what the files on disk ask for beyond what the admin approved, `{version, digest, requested, allow, added, removed, addedHosts, removedHosts}` (capabilities and `[net] allow` hosts), which the dashboard and `pcds plugin enable` show before the admin decides; `commands` each `{name, help, usage, minArgs, maxArgs, permission}`; `stats.http`: `requests answered failed bytesOut bytesIn inFlight`; `net` (null without `[net]`): `allow wildcards local localGranted timeoutMs maxResponseBytes`; `signature`: `state` (`none`, `invalid`, `untrusted`, `revoked`, `trusted`), `keyId`, `keyName`, `vouches` (what the key covers of what the plugin asks) and `detail` (why an invalid one is); the listing also has `trustedKeys`, `revokedKeys` and `keysError`; `config`: the declared keys, `key description secret set type default min max maxLen choices`, and the `value` of a key that is not a secret; the listing also has `commandPrefix` |
| `POST /api/plugins/logs` | `{"id", "limit"}` (limit: 1 to `LOG_KEPT`, default 100) | `{"id", "lines": [{"seq","at" (Unix seconds),"level" ("error","warn","info","debug"),"text"}]}`, oldest first: the plugin's own lines and the host's about it (faults, unloading, slow calls), the last `LOG_KEPT`. 404 for a plugin that is not installed. `seq` counts the plugin's lines since it was loaded, so a reader that follows asks again and keeps the lines above the last `seq` it saw |
| `POST /api/plugins/reload` | | `{"ok": true, "message"}`: the folder is read again; a plugin whose files and approval did not change keeps running |
| `POST /api/plugins/enable` | `{"id", "grant": [...]}` (grant optional: everything asked) | `{"ok": true, "plugin": {...}}`; 400 for an unknown plugin, a grant above what is asked, or a plugin that does not hold up |
| `POST /api/plugins/disable` | `{"id"}` | `{"ok": true, "plugin": {...}}` |
| `POST /api/plugins/config` | `{"id", "key", "value"}` (`value` null or left out removes it) | `{"ok": true, "plugin": {...}}`; 400 for a plugin or key that does not exist, or a value that is not one line. The plugin is restarted with it. A secret's value is in no answer |
| `POST /api/plugins/trust` | `{"publicKey": "(64 hex or a .pub file's text)", "name", "grant": [...], "force"}` | `{"ok": true, "key": {...}}`; 400 for a key that is none, a grant that is no capability, or a revoked key without `force`. The plugins are read again |
| `POST /api/plugins/untrust` | `{"id"}` (the id, or its first 8 characters or more) | `{"ok": true, "revoked": {...}}`; 400 for a key that is not trusted |
| `GET /api/plugins/tokens` | | `{"tokens": [{"id","name","scopes","ratePerMinute","createdAt"}], "scopes": [{"name","allows"}], "defaultRatePerMinute", "maxRatePerMinute"}`: never a token or its hash |
| `POST /api/plugins/tokens` | `{"name", "scopes": [...], "ratePerMinute"}` | `{"ok": true, "token": "pcdst_...", "info": {...}}`: **the only time the token is shown**; 400 for a name used, an unknown scope, a rate out of range |
| `POST /api/plugins/tokens/revoke` | `{"id"}` (the id or the name) | `{"ok": true, "revoked": {...}}`; the token is refused from its next request |
| `POST /api/plugins/events` | `{"consumer": "relay", "after": 0, "types": ["chat","player_join"], "limit": 200, "wait": 25}` or `{"consumer": "relay", "unregister": true}` | `{"epoch", "events": [...], "last", "dropped"}`, or `{"ok", "registered": false, "was"}`. With `wait` (seconds, 25 at most) the front holds the request until there is an event or the wait is over (see "The event stream") |

Status values: `running`, `pending` (discovered, never approved), `disabled`, `changed` (files differ
from the approved ones, and nothing is running), `unloading` (its last call and save are running;
seen in the log, a request never finds it so), `invalid`, `incompatible`, `failed` (did not start,
or switched off for faults). Each change of state is one log line, `plugin ID: STATE`.
A `running` plugin may have a `notice`: its files changed and wait for approval, or a reload of
them failed and the old version goes on. `runtime` is `{"instances", "memoryPages"}`: what the
running modules hold; it is zero once they are all unloaded. `scanEverySeconds` is how often the
bank looks at the folder by itself (2); a change is acted on when two looks in a row see it.

`GET /api/status` has a `plugins` object: `loaded`, `running`, `faults`.

## Delivery and ordering

What a plugin may rely on:

- **One thing at a time.** The host calls plugins on the game thread, between the game's own steps; a call is never
  interrupted by the game, by another plugin or by an admin request. Whatever a plugin reads in a call (`world`,
  `players`, ...) is the world as it stood when the call began, and stays so until it returns.
- **Order of plugins.** Plugins are called in the order of their ids (the names of their folders, sorted). That order is the
  same at every start and does not depend on when a plugin was installed or approved. A `rpc_before` that one plugin vetoes is
  not shown to the plugins after it. Ticks are the exception: when the plugins' 2 ms for a tick run out, the next tick
  starts with the plugin where the last one stopped, so they take turns.
- **Order of events.** Events reach a plugin in the order they happened in the game. A chat message that is a command
  is delivered as `chat` first (to the plugins that subscribed), then as `command` (to its owner). The actions a call
  queued are applied right after that call, before the next plugin is called, in the order they were queued, so the
  next plugin sees their effect. Timers are delivered after the tick's own events, at most `TIMER_EVENTS_PER_TICK` per plugin.
- **A hot update.** At the hand-over each plugin is asked for its state (`pcds_save`), the new server starts the plugins from it
  (`pcds_load`), and `flip` is delivered to those that subscribed. No other event happens in between. Timers do not
  survive: set them in `pcds_load`. A request in flight (`http_request`) is lost without an event.
- **Not guaranteed.** That every event is delivered: a plugin that faults misses the event that faulted it and is started
  again; a plugin that is switched off for faults misses everything until it is started. That two events arrive at the same
  wall-clock time. That `tick` arrives at the interval asked for (it never arrives while the world stands still, and
  not more often than once per game tick of 5 ms).

## What each host function costs

Every call costs `HOST_CALL_FUEL` (200) fuel besides the instructions that make it; a byte the host copies for the
module (a text, an answer) costs `FUEL_PER_BYTE` (1) and a byte it parses (a rules patch, an HTTP request)
`FUEL_PER_PARSED_BYTE` (16). A million fuel is about a third of a millisecond.

| Function | Fuel | Other limits |
|---|---|---|
| `log` | 200 + 1 per byte (at most 4 times `LOG_LINE_CHARS` bytes looked at) | `LOG_LINES_PER_SECOND` a second, `LOG_LINE_CHARS` characters; the rest is counted, not kept |
| `query` kinds 1, 2, 3, 7, 8 | 200 + 1 per byte of the answer copied | the answer is built once per call (kinds 1 to 3, 7) |
| `query` 4, 6 | the same | `-6` when not found |
| `query` 5 (`nearby`) | the same + `FUEL_PER_SCANNED_OBJECT` per object in the world | at most `MAX_NEARBY` objects, radius `MAX_NEARBY_RADIUS`; a world of 100,000 objects costs 800,000 of the 1,000,000 default fuel |
| `say`, `tell`, `kick` | 200 + 1 per byte (at most `MAX_TEXT_BYTES`) | `ACTIONS_PER_CALL`, `ACTIONS_PER_SECOND`; `SAY_CHARS` per chat line; a kick's reason is cut to 128 characters |
| `teleport` | 200 | finite, within 100,000 of the origin |
| `rules_patch` | 200 + 17 per byte | `MAX_PATCH_BYTES`; `RULE_CHANGES_PER_SECOND`; gameplay sections only |
| `give` | 200 + 1 per byte of the name | `MAX_GIVE_COUNT`, `GIVES_PER_SECOND`, and the actions limits |
| `tokens` | 200 | `MAX_TOKENS_PER_ACTION`, `TOKEN_CHANGES_PER_SECOND`, and the actions limits |
| `timer_set`, `timer_cancel` | 200 | `MAX_TIMERS`, `MIN_TIMER_REPEAT_MS`, `MAX_TIMER_DELAY_MS` |
| `http_request` | 200 + 16 per byte of the request | `HTTP_IN_FLIGHT`, `HTTP_PER_SECOND`, and the rest under `net.http` |

A pointer or a length outside the module's memory traps the call, whatever the function.

### Scoped tokens

A program that reads the stream or says something in the chat should not hold the admin token. A
**scoped token** (`pcds plugin token create NAME --scope events.read,chat.send`, or the dashboard) is
sent as `Authorization: Bearer pcdst_...` and opens the requests of its scopes and nothing else:

| Scope | Allows |
|---|---|
| `events.read` | read the plugin event stream (`POST /api/plugins/events`) |
| `chat.read` | read the chat (`GET /api/chat/events`) |
| `chat.send` | write to the chat as the server (`POST /api/chat`) |
| `player.kick` | kick a player (`POST /api/kick`) |
| `player.teleport` | move a player (`POST /api/teleport`) |

Every other request, the endpoints that manage tokens, keys and approvals among them, takes the
admin token only; a token asking for one is answered `403` and the scope it lacks. `401`: no such
token (or revoked, or the list does not read: then none is valid). `429` with `Retry-After`: the
token is over its rate (`ratePerMinute`, 600 by default, 6000 at most; a window of a minute). The
server keeps only the sha256 of a token in `tokens.json` (0600, beside `approved.json`), compares
it in constant time, and never writes a token to the log, a listing or an answer. A revocation is
read from the file at the token's next request. The admin thread of the front checks tokens, so a
front that was started before this change does not know them: they work after `pcds upgrade
--restart` (docs/hot-update.md).

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

**Waiting for events (long poll).** Add `"wait": SECONDS` (a number up to 25) to the request and it
is answered when the ring holds an event after `after` of your kinds, or, empty, when the wait is
over: loop on it with the `last` of each answer as the next `after`, and events reach you in the
time the bank takes to say so (a few milliseconds), without asking every second. The waiting is
done by the front (a thread and a connection for each, at most 8 at once: more are answered `503`
at once, poll without `wait` or try again), not by the game loop. The bank rings a bell (`ipc::STREAM`)
when the ring has a newer id; the front then asks the bank again. When no bell comes (a bank older than
this feature) the front asks every second, so the latency is at most one. A consumer is registered by each
ask, so a long poll keeps it listed. Without `wait`, as before: answered at once, empty or not. This is in the
front, which a hot update does not replace: it works after `pcds upgrade --restart`; an older front
ignores `wait` and answers at once, which a loop written as above handles (add a pause when the answer is
empty and came at once).

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
