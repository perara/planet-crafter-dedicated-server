# Plugins: the design

Third parties can add commands, rules, reactions and integrations to the server without forking
it. This document says what was chosen and why, with the measurements it rests on. How to write a
plugin is docs/plugins-guide.md; the exact interface is docs/plugins-api.md; where the code lives
is docs/architecture.md.

## The decision in ten lines

1. **Logic that runs inside the world is a WebAssembly module**, a plain core module (no WASI, no
   component model) with eight imports and nine exports, run by **wasmi**, an interpreter in
   pure Rust. Rust, C, C++, Zig, AssemblyScript and TinyGo can all make one.
2. **Small jobs are data**: a `rules.toml` of chat commands, welcome messages, announcements and
   rule switches that the host runs itself. No code, nothing to sandbox, nothing that can loop.
3. **Integrations are programs of their own** that read the event stream
   (`POST /api/plugins/events`, with a long poll) and use the admin API with a scoped token, in any
   language, on any machine. A crash of one never touches the game. A module may also reach a few
   listed hosts itself with `net.http`, which the server sends from its own threads and aims for it.
4. **No native libraries** for third parties: no memory isolation, no budget, a segfault takes the
   world with it, and they would make the plugin part of the GPL program. First-party code that
   must be native stays a Cargo feature or a built-in, behind the same `Runtime` trait.
5. **No scripting language in the server**: Rhai measured 120 times slower than wasmi; the data
   kind covers the small rules, WebAssembly covers the rest (a scripting language compiled to
   WebAssembly can be a plugin).
6. **wasmi, not wasmtime**: +2.2 MB on the program instead of +11.6 MB, a module ready in about a
   millisecond instead of 70, no machine code generated at run time, fuel metering that is
   deterministic. It is 3 times slower than a JIT on pure computation and 5 times slower per
   call; an event costs 0.4 µs. The price of the choice is no component model (wasmi does not
   have it) and no wall-clock interruption (fuel is the limit).
7. **A plugin cannot write the world.** It receives events, may read a copy of the world, and
   queues *actions* the host applies afterwards through the same validated paths the admin API
   uses. The one exception to "reacts only" is refusing a client RPC, which is its own
   capability with the RPC names listed in the manifest.
8. **The admin approves capabilities**, not plugins: a manifest asks, `approved.json` grants,
   with the digest of the files at that moment; a changed plugin waits for the admin again.
9. **Budgets and faults**: fuel per call, a memory limit, 2 ms for all plugins together in a
   tick, 25 ms for all of them in a hot update. A fault restarts the plugin from its last saved
   state; five in a minute switch it off. The cost of having no plugin is one load and one test
   per event (0.35 ns).
10. **State survives a hot update**: each plugin hands over opaque bytes with a version, they ride
    in the snapshot in an additive part of `ext`, the new bank prepares its plugins before the
    world stops and only starts them after the restore. They are part of the snapshot's state, written by the same binary codec (bincodec.rs) as the rest.

## What a plugin can and cannot be

The game's clients are unmodified copies of The Planet Crafter. They have their own tables, their
own recipes, their own idea of what exists. So a server-side plugin can do what the *server*
decides:

- **Can**: add chat commands and replies; react to joins, leaves, chat, builds, saves and rule
  changes; read the world (time, players, positions, gauges, terraformation, object counts);
  message, kick and move players; change the gameplay rules (`speed.*`, `gauges.*`, `limits.*`...);
  refuse a client request; keep its own state; talk to the outside world through the event stream.
- **Cannot**: add an item, a building or a recipe the client does not know (the client would
  craft it by its own tables and the server's answer would not match: that is what the parity rule
  forbids); draw a user interface in the game (chat and kick messages are the whole channel);
  change what the game's files say.
- **Tweaks of the world** that need no client knowledge (machine speed, growth speed, gauge
  drain, building limits, loot amount) are rules already (README, "Server rules"); a plugin sets
  them with `rules.write`. "Machines, recipes and items as data" therefore means: rule values, and
  per-world settings at creation (`pcds new --set KEY=VALUE`). A data overlay on the server's
  tables is possible for values the server alone uses and is not built: every such value needs its
  own check that the client does not depend on it.
- **World generation tweaks** are the settings of a new world today. A hook at world creation
  (an event with the settings, answered with a patch) fits the same model; it is not built.
- **Admin panels**: a plugin that is a program serves its own page (the event stream and the
  admin API are all it needs); a panel inside the server's web page would be an iframe with a
  sandbox and no access to the admin token. Neither the host nor the page has it yet; the
  dashboard lists plugins, their state, their capabilities and their cost, and approves them.

## The options, measured

Everything below was measured on the development machine (32 cores, load above 80 for the whole
session, so the figures are the best of several rounds and every real number is probably better).
The engine comparison is described under "Reproducing"; the host's own costs are `make plugin-bench`.

A workload of each kind, the same everywhere: an empty call; an event with a 64-byte payload
whose handler adds up the bytes (and answers one time in a hundred); 10,000 steps of integer
arithmetic.

| Mechanism | empty call | event, 64 B | 10,000 steps | start of a 44 KB module | notes |
|---|---|---|---|---|---|
| (a) native library through a C ABI (`libloading`) | 1.5 ns | 34 ns | 8.1 µs | n/a | no isolation, no budget |
| (b1) WebAssembly, wasmtime (Cranelift JIT), fuel on | 15 ns | 67 ns | 8.5 µs | 70 ms to compile | JIT, component model |
| (b2) WebAssembly, **wasmi**, fuel on | 61 ns | 270 ns | 27 µs | 1.0 ms to compile, 25 µs per instance | **chosen** |
| (c) another process, a unix socket, round trip | n/a | 3.5 to 4.7 µs | n/a | a process | answer must be awaited; measured with an echo thread, so a real program adds its own parsing |
| (c) the same, one way (an event sent, no answer) | n/a | 0.6 to 0.7 µs | n/a | | |
| (d) Rhai script | 0.1 µs | 17 µs | 2.7 ms | 12 µs to compile a small script | 120 times slower than wasmi |
| (e) data (rules file) | n/a | 0.7 µs | n/a | read once | host code, not a language |

The numbers of the host as a whole (`make plugin-bench`, wasmi, one thread, host only):

| One event delivered to | time | per second |
|---|---|---|
| 1 module that does nothing | 0.40 µs | 2.5 million |
| 1 module that says a line (an action) | 0.52 µs | 1.9 million |
| 1 module that reads the world | 0.57 µs | 1.8 million |
| 10 modules, all wanting the event | 2.5 µs | 0.4 million |
| 1 command, 10 modules with a command each (only the owner is called) | 0.46 µs | 2.2 million |
| 1 rules file, a command that replies | 0.68 µs | 1.5 million |

The game ticks every 5 ms. On the benchmark world with four players (the tick itself costs 1.7
µs there, with the players standing still), plugins called at **every** tick, which is the most
a plugin can ask for, add:

| Plugins called at every tick | added per tick | share of the 5 ms tick |
|---|---|---|
| 0 | 0.00 µs (the reference) | 0 |
| 1 that does nothing | 0.7 µs | 0.015 % |
| 10 that do nothing | 5.1 µs | 0.10 % |
| 1 that reads the world each tick | 2.1 µs | 0.04 % |
| 10 that read the world each tick | 6.8 µs | 0.14 % |

And with **no plugin at all**: `events::tick` measured 0.48 ns per call against an empty loop's
0.13 ns, and `events::rpc_before` 1.8 ns per call (an RPC is several microseconds of work). That
is a load and a test of one mask. The bench fingerprint and the handoff comparison are equal with
and without the plugin code (see "Measurements of the whole" below).

### Why not each of the others

**(a) Native libraries.** The fastest by far, and everything else is against them. There is no
Rust ABI; `abi_stable` or hand-written `#[repr(C)]` tables give a stable surface, but only to code
built by a matching compiler, and they do nothing about the real problems. A native plugin reads
the whole process: the admin token, the Steam token and the chat secret are in the environment of
the program the bank is a child of. A bug is a segfault of the bank, which is the world. A loop
in `tick` stalls every player, and nothing can stop it from outside. Reloading a library
(`dlclose`) is unsafe for Rust code with thread-locals or destructors. A panic that crosses the
boundary is undefined behaviour. There is a binary for each machine kind, and an admin cannot
review it. And a library loaded into the process is, in the view of the Free Software Foundation,
one program with it: it would have to be GPL (see "Licensing"). None of this matters for code the
project itself ships, which can be a built-in; for third parties the answer is no.

**(b) WebAssembly** is the one mechanism that gives, together: memory isolation (a module can
only reach its own linear memory), no ambient authority (it has no file, no clock, no network
unless the host lends it a function), a hard limit on work (fuel) and on memory, a crash that is a
trap and not a signal, the same module on every machine, and many languages. *Wasmtime, wasmi or
wasmer* is the real decision:

| | wasmtime 49 (Cranelift) | wasmi 2.0 | wasmer 7.5 |
|---|---|---|---|
| program size, stripped, added (alone: a hello program) | 11.6 MB | **2.0 MB** | not built or measured |
| compressed in the release tarball | +3.7 MB | **+0.7 MB** | not measured |
| first build of the engine on 4 loaded cores | 3 min 40 s | **25 s** | not measured |
| module of 44 KB, ready | 70 ms | **1.0 ms** | not measured |
| a new instance | 5 µs | 25 µs | not measured |
| an empty call, an event | 15 ns, 67 ns | 61 ns, 270 ns | not measured |
| 10,000 steps of arithmetic | 8.5 µs | 27 µs | not measured |
| machine code generated at run time | yes | **no** | yes (Singlepass, Cranelift or LLVM) |
| fuel | yes | yes | yes (a metering middleware) |
| wall-clock interruption | **yes (epochs)** | no | not checked |
| component model, WIT | **yes** | no | not checked |
| memory limit, table limit | yes | yes | not checked |
| licence | Apache-2.0 with LLVM exception | MIT or Apache-2.0 | MIT |

On the server, the program (stripped) went from 9.9 MB to 12.1 MB with the plugin host, wasmi and
the TOML reader together (4.0 MB to 4.8 MB compressed): the figures above are for the engines
alone. Wasmtime would be 11 MB on top of that, more than doubling the program and the Docker
layer, for a plugin that adds a command. A plugin does 0.3 microseconds of work per chat message;
a JIT makes that 0.2. What a JIT is worth is *heavy* computation in a plugin (pathfinding,
generating terrain each tick), and no plugin in sight does that; if one comes, `wasm.rs` is the one
file that names the engine (about 250 lines), and the interface is engine-neutral core
WebAssembly, so wasmtime can replace wasmi behind a Cargo feature without a plugin changing.
Wasmer was not built or measured: it carries a compiler back end (Singlepass, Cranelift or LLVM)
as wasmtime does, so it would sit with wasmtime on size and start-up and with a smaller following;
that is a judgement from what it is, not a figure, and the column above says so.

Two things wasmi cannot do and the design lives without: **wall-clock interruption** (fuel is
the limit, and fuel is an exact count of work, so a loop is stopped after a fixed amount of it
on any machine, whereas a time limit would stop the same plugin at different places on a busy
host; the host also measures each call's time, counts calls over 5 ms as *slow* and logs the first of
each ten seconds), and the **component model**.

*The component model.* The plan was to describe the interface in WIT and run components. It was
set aside: components need wasmtime (the 11 MB); an author needs `cargo-component` or
`wasm-tools component new` and the `wasm32-wasip2` target, which brings a set of WASI imports the
sandbox then has to stub one by one; the canonical ABI's lifting and lowering would be code in the
host for an interface of eight functions; and WASI itself is moving (0.2 to 0.3). A core module
with a hand-written codec builds with `cargo build --target wasm32-unknown-unknown`, which any Rust
author has. The event kinds and their fields map one to one onto a WIT `variant`, so the day wasmi
has components, or wasmtime is wanted anyway, a WIT file is a mechanical translation and old core
modules can still be run. The trigger is written down in "Open questions".

**(c) Another process.** For what can wait a millisecond, this is the best mechanism there is:
any language, any library (Discord's, a database, an HTTP client), full crash isolation, no
budget to manage, can run on another machine, can be written by someone who has never seen Rust.
It cannot be in the tick (a veto of an RPC must be answered before the RPC runs) and it cannot be
trusted with less than the admin token. So it is the second mechanism, for integrations:
the server writes events into a ring (`stream.rs`) and any program reads them (`POST
/api/plugins/events`), then acts through the endpoints that already exist (`/api/chat`,
`/api/kick`, `/api/teleport`, `/api/rules`). The ring is part of the snapshot, so ids go on across
a hot update, and the `epoch` tells a reader that the server was restarted. It can be waited on:
`POST /api/plugins/events` with `"wait"` is held in the front (like `/api/chat/events`) and
answered when the bank rings the bell (`ipc::STREAM`, a frame an older front skips and an older
bank never sends), with a look every second as the fallback; the ring and the registration stay in
the bank, the front only waits (section "Long-polling the stream"). Out-of-process plugins have no manifest, no capabilities and no
budget: they can do what their token can, and a **scoped token** (section "Scoped tokens") makes that the
stream and a few actions instead of everything the admin token can. That is stated in the dashboard and in
`examples/plugins/webhook-relay`. A small integration that only calls a few hosts (a Discord notice) is better a
WebAssembly module with `net.http` (`examples/plugins/discord-notify`): no process to run, a budget, and no token at all.

**(d) Scripting.** Rhai is a good embedded language, and its operation limits would serve as a
budget. It measured 17 µs for the event that wasmi handles in 0.27 µs, and 2.7 ms for the 10,000
steps that take wasmi 27 µs: acceptable for a command, not for a tick. More important: a script
engine in the server is a third language to specify, secure and keep stable, and its authors would be
the only ones who write plugins in it. Lua (`mlua`) needs a C compiler in the build and is not
measured. The small jobs that scripts are wanted for are what the data kind does without a
language; the larger ones are WebAssembly; and a script engine compiled to WebAssembly (Rhai and
Lua both can be) is a plugin like any other, for whoever wants it.

**(e) Data.** Already the answer for the most common wish ("when someone says !rules, say this").
It cannot misbehave because it cannot loop: every construct is one step. Its limits are the
language's, and the language is small on purpose (commands, joins, leaves, announcements, rule
switches by player count). When a file needs more, it becomes a module; `pcds plugin check`
runs both kinds the same way.

**(f) Cargo features** are for the first party: code the project ships and builds with the server
(`s3` could be one). They may be native. The seam is the `Runtime` trait of `plugins/runtime.rs`,
which `data.rs` and `wasm.rs` implement: a native first-party extension is a third implementation,
compiled in, with no sandbox, subject to the same events, capabilities and state hand-over. A
Cargo feature `plugins` that leaves the host out of the program is not offered: the host costs
2.2 MB and 20 s of build, and a server that has no plugin pays one test per event; the numbers do
not justify two builds to test.

## Architecture

```
  third-party WebAssembly module ─┐         rules.toml ─┐
                                   ▼                     ▼
  game ── events::* ──► plugins::dispatch ──► Host::run ──► Runtime (wasm.rs | data.rs)
   ▲  (one place)            │                   │ per call: Ctx = capabilities + a read-only view
   │                         │                   │ + a queue of actions + fuel
   └── actions.rs ◄──────────┴───────────────────┘ applied after the call, through the admin API's paths

  program of its own ──► POST /api/plugins/events ──► ring (in the snapshot)
                    └──► /api/chat /api/kick /api/teleport /api/rules ...
```

The layers, from the bottom (docs/architecture.md has the module map and the check):

| Layer | What | Where |
|---|---|---|
| core | the game; knows nothing of plugins | `server/src/` |
| bus | `events.rs`: the typed events, emitted from where they happen; zero cost when nobody listens | `server/src/events.rs` |
| API crate | `pcds-plugin-api`: the events, the queries, the codec, the `Plugin` and `Host` traits and the `export!` macro; `no_std`, MIT or Apache-2.0 | `plugin-api/` |
| host | manifests, approvals, capabilities, budgets, runtimes, state, the admin API's part, the command line's check | `server/src/plugins/` |
| plugins | modules, rules files, programs | the admin's `plugins/` folder, `examples/plugins/` |

*Where the host lives.* In the **bank**, with the game it serves. A plugin that reads the world
and refuses RPCs must run on the game thread, between two ticks, with the world held still: that
is what makes a read consistent without copying the world. The price is that the bank is replaced
at a hot update, so plugins are reloaded and their state handed over; the section "Plugin state in
a flip" is that. The front could host the event stream (it survives updates) but not the logic
(it knows no world); only a long-poll would move there.

*Why one thread and no concurrency.* A plugin call is a function call on the game thread, with
a fuel budget. Plugins never run at the same time as the tick or each other, so they need no
locks and cannot race the game. Fuel makes the cost bounded and the same on every machine.

## Events, queries, actions

Every event is a typed value of `pcds_plugin_api::Event`; the bytes a module receives are its
`encode`, and a module reads them with `Event::decode`. Fields are only ever added at the end; a
reader ignores bytes after the fields it knows.

| Event | When | Needs | Cost notes |
|---|---|---|---|
| `tick` | every `tick_every_ms` of game time the plugin asked for (default 1000, least 5), never while the world stands still | `event.tick` | all plugins together get 2 ms per game tick; the ones left over wait for the next tick and take turns |
| `player_join` | a player is in the game (PlayerReady done) | `event.player` | |
| `player_leave` | a player's object went away | `event.player` | |
| `chat` | a chat message (after the game's own rate limit) | `event.chat` | |
| `command` | a chat message that starts with the command prefix (`chat.commandPrefix`, else `!`) and names one of the plugin's commands | `event.chat` | only the plugin that owns the command is called |
| `rpc_before` | a client RPC is about to run, for the RPC names in the manifest | `event.rpc`; `rpc.veto` to refuse | the event is built only when a plugin names this RPC |
| `rpc_after` | the same, after the server handled it | `event.rpc` | |
| `object_placed` | a world object got its network object (built, dropped; not at load) | `event.object` | |
| `object_destroyed` | a world object was destroyed | `event.object` | |
| `world_saved` | after a save | `event.world` | |
| `rule_change` | the server rules changed | `event.world` | |

Not built, described in the API's versioning as additive: `machine_cycle` (a machine finished a
production), `flip` (the plugin was reloaded after a hot update: `Plugin::load` covers it today),
`world_created`.

**Queries** (capability `world.read`; read-only copies, taken at the moment of the call): the
world (time, planet, players online, object counts, terraformation, whether it stands still), the
players (name, SteamID if verified, position, gauges), the rules that differ from the defaults
(without the chat, backup and access sections: those hold webhook URLs, keys and ban lists).

**Actions** (queued, applied after the call, by the host): `say` and `tell` (`chat.say`), `kick`
(`player.kick`), `teleport` (`player.teleport`), `rules_patch` (`rules.write`; only the `meteo`,
`gauges`, `speed`, `death`, `loot`, `limits` and `world` rules: never `chat`, whose exec program
would run code outside the sandbox, nor `backup`, nor `access`). Each is checked again when
applied (the player may have gone), counted, and bounded (16 per call, 200 per second per
plugin).

**Why actions are queued.** A plugin that could change the world while it runs could change it
halfway through the game's own work; it would make the order of effects depend on plugin code,
and it would not be a server the game's clients see as the same server. A queue applied after
the call keeps the plugin's part pure, and puts every effect through code that was written for
the admin API and is tested. This is the answer to "alters game state must go through the same
validated handlers": there is no other way.

**The one veto.** `rpc_before` may be answered `Veto`, which drops the client's RPC as if it had
never arrived (the server does not log it as unhandled). The client, unmodified, has already
done its half: it may show an item moved that did not move. A veto is therefore for requests the
game would refuse itself in some situations (chat, a build over a limit), or for a message that
tells the player why (`tell`). The capability `rpc.veto` is listed with the RPC names in the
dashboard's approval, so the admin sees which requests the plugin may stop.

## Loading and unloading while the world runs

Plugins come, change and go with the server running: no restart, no hot update. Everything below
happens on the bank's game thread **between two events**, never inside a plugin's call, so nothing
is torn down while it runs.

**States.** One word for each, in `pcds plugin list`, `GET /api/plugins` and the dashboard, and
each change of state is one line in the log (`plugin ID: running`):

| State | Meaning |
|---|---|
| `pending` | discovered (its folder is there) and awaiting approval; nothing of it runs |
| `running` | approved and loaded; may carry a `notice` (below) |
| `disabled` | approved once, switched off; its state is kept |
| `changed` | its files differ from the approved ones and nothing is running (after a start) |
| `unloading` | its last call and last save are running (a moment; seen in the log) |
| `failed` | switched off for faults, or it did not start (the faulted state) |
| `invalid`, `incompatible` | its manifest, module or rules do not hold up; built for another ABI major |

**Discovery.** The bank looks at the plugins folder every 2 seconds: the folder, `approved.json`,
and each plugin's manifest and entry file, by size and modification time (about 45 µs for ten
plugins: a few dozen `stat`s, every 2 s, which is 0.002 % of one core). A change is acted on when two looks in a row see the same new state,
so a folder still being copied in is not read half written; so a plugin dropped in is `pending`
within about 4 seconds, and starts within about 4 seconds of its approval being written, whoever
wrote it (`pcds plugin enable` with the server stopped or running, the admin API, the dashboard, an
editor). The admin API, the command line and the dashboard act at once (they ask for a reload).
A plugin that is a rules file is loaded and unloaded exactly like a module. A program that reads
the event stream registers by asking and is listed (name, kinds, last poll); it leaves by saying
so (`"unregister": true`) or after a minute of silence, and what only it wanted stops being recorded.

**Loading.** A new folder is read and, once approved, compiled, started with its state (from the
snapshot's part, else the file beside it) and its commands and event kinds are registered, all in
one step between events (about 1 ms for a 44 KB module). A new version of an approved plugin is
started first; the old one is replaced only when the new one has started.

**Unloading** (switched off, folder removed, or switched off for faults), in this order:

1. no event reaches the plugin from now on (the state is `unloading`);
2. its optional `pcds_unload` (ABI 1.1: `Plugin::unload`) is called, and then `pcds_save`, each with
   the fuel of a save (at most 50,000,000, so a loop in it is stopped within milliseconds; a trap
   in it is a line in the log, not a fault of the plugin; the state of the last good save is then
   kept instead);
3. the state goes into the snapshot's part and, unless the folder is gone, beside the plugin;
4. the instance is dropped: its memory, its capabilities and everything it had queued are gone
   (what it asked for in its unload call is never applied), its timers with it;
5. its commands, its event kinds and its limits leave the host's tables.

A plugin switched off for faults is held (its instance, for the last call only) until the
current event is done, then goes through the same steps. A plugin whose folder was removed is
unloaded the same way, but nothing is written back, so removing it removes its state too.

**Reload** is unloading and loading of the changed files with the state handed over: the old
instance is asked for its state (`pcds_save`, not `pcds_unload`: it is not going away, a new
version takes over), the new one starts with it and `pcds_load(state, version)` is the
plugin's own migration. If the new version cannot start (its `load` traps on the old state, its
module is invalid), **the old version keeps running** and the plugin carries a `notice` saying why
(`a reload failed: ...; the old version keeps running`), in the log, the listing and the dashboard.
Files that changed on disk and are not approved yet do the same: the approved version, in
memory, goes on with the notice `its files changed and wait for approval`, until the admin approves
(it is replaced) or disables it. A plugin switched off for faults is started again by an explicit
reload, not by the folder merely changing elsewhere.

**While the world runs.**

- *During a tick*: the folder is looked at after the tick, never in it; a plugin is never loaded
  or unloaded during an event, because both happen on the one thread that runs the events.
- *During a flip*: the old bank, from its `HANDOFF` until the new bank has taken over, does not
  answer requests or look at the folder, so its plugin set cannot change under the snapshot: **the
  snapshot holds the set that was running at the `HANDOFF`.** The new bank read and compiled the
  folder earlier, before its hello; **when it attaches it looks at the folder once more and, if
  anything changed since (an approval or a disable during the flip), reads it again at once.** So
  the plugin set the new bank runs is the folder's at the moment it takes over, never the compile's.
  A plugin switched off during a flip is started with its state from the snapshot and unloaded
  straight after, its state written; nothing is lost.
- *While players chat commands*: a command is delivered to the plugin that owns it when the message
  is handled; after the unload nobody owns it, and it is only a chat message.
- *Repeatedly*: a test loads and unloads a plugin 100 times and finds the same instances, memory
  pages, registered commands and subscribed kinds as before, and the same tick cost (the host counts
  its instances and pages; `GET /api/plugins` shows `runtime`).
- *A hostile plugin*: a trap or an endless loop in `pcds_unload` cannot stop the unloading (above).

The cost of a reload or load is paid on the game thread (compiling: about 1 ms for 44 KB, up to
0.2 s for the 8 MiB limit); a hot update's is not (the new bank compiles before the world stops).

## Capabilities and approval

A manifest (`plugin.toml`) asks for capabilities by name; the table is in docs/plugins-api.md.
Asking is not having: the admin grants (`pcds plugin enable ID`, or the dashboard), shown each
capability in plain words, and may grant less than was asked (`--grant event.chat,chat.say`).
`approved.json` records the grant and a **digest** (sha256 of the manifest and the entry file).
At every start and reload the host recomputes it; a plugin whose files changed since the approval
is `changed` and runs nothing until approved again, so an update or a swapped file cannot slip
in. The approval is for the files that were shown: `pcds plugin enable` and the dashboard hold the
digest they displayed, and the approval is refused if the files changed meanwhile (`digest` in
`POST /api/plugins/enable`). A file that cannot be read, or an `approved.json` that does not parse,
means nothing is approved (never "everything is allowed").

The checks are at two places, so a missing capability shows up where it is used: **events** are
only delivered if the capability for their kind is granted (a warning in the listing tells which
are not), and every **host function** checks its capability when called (a refused call returns
an error code to the plugin, counts in `denied`, and does nothing). `pcds plugin check` compares
the host functions a module imports with the capabilities its manifest asks for, so most
mismatches are found before installing.

## `net.http`: a plugin's way out

A module has no network. Integrations that must reach the Internet were programs of their own
(a process, the admin token); a Discord notice, a web hook, a metrics push is a small call to a few
hosts, and a program for it is a service to run, watch and give the admin token. `net.http` lets a
module make those calls itself, **from inside the bank and without being able to aim them**. The
reference is docs/plugins-api.md ("`net.http`"); this is the design.

**Asynchronous, and never on the game thread.** `pcds.http_request` checks the request (capability,
form, allow list, rate), queues it like an action and returns a **ticket**. Four worker threads,
started with the first request and shared by every plugin, send it. The answer is an event,
`http_done`, delivered by the bank between two ticks (`plugins::poll`, the same safe point as the
folder scan; at most eight a frame) to the plugin that asked, and the call that handles it is an
ordinary call: fuel, faults, actions applied after it. A synchronous call would hold the tick for
a network round trip; a callback would run plugin code on a thread. A plugin that wants to wait
for an answer remembers its ticket in its state.

**The allow list is the authority.** The manifest's `[net] allow` is what the admin approves
(shown in the approval, pinned by the digest), so a plugin cannot widen it later. A pattern is a
scheme, a lowercase host and a port; one `*.` in front of a name covers the names below it, and the
manifest is refused if the name is a public suffix (`*.com`, `*.co.uk`, `*.github.io`). The host
carries a short list of those instead of the Public Suffix List (thousands of lines, a new
dependency, a release to keep it fresh): the rule that a wildcard has two labels at least, plus
the two-label national suffixes and the hosting platforms a plugin would plausibly name. It is a
net, not a proof; the approval screen flags a wildcard and the admin decides.

**The address is checked where it is used.** Allowing a *name* proves nothing about the *machine*
(a name can point at 127.0.0.1, at the cloud's metadata address, at the router). The guard is in the
connection itself: ureq is given a resolver of the host's that does the lookup (on a thread it stops
waiting for at the deadline), refuses every address in a class a plugin may not reach, and hands
ureq exactly what it checked, and a connector in front of the TCP connector looks at the addresses
ureq is about to dial once more. A name with one disallowed address among its answers is refused
whole. The lookup is repeated for every connection and every redirect, no connection is kept, so
there is no moment at which a second answer of the DNS could be used without being looked at (the
rebinding attack): the test that does it gives the same name a loopback answer the first time and
an unspecified one the second, and the redirect to the same name is stopped. IPv4 spellings that a
resolver reads as addresses (`127.1`, `0x7f.1`, `2130706433`) are not host names here (a host's last
label begins with a letter), and one that reaches the resolver anyway is judged by the address it
becomes. `net.http.local` (asked for in the manifest, granted by the admin as its own line in the
approval) opens loopback, the private ranges, link-local and shared address space, for a plugin
whose job is the local network (a bridge on the same machine); unspecified, multicast and the
reserved ranges stay closed.

**Limits that need no goodwill.** One wall clock from the lookup to the last byte (so a server
that trickles one byte a second is let go: `timeout_ms`, 10 s by default, 30 s at most); the answer
is read one byte past `max_response_bytes` and an answer over it is an error, not a short body that
looks like a whole one; a request's size, URL and headers are bounded; two requests alive per
plugin and ten started a second; four workers behind a queue of 32, and a request that finds the
queue full is answered at once with "the server is busy" (nothing waits without bound). Four workers
and two alive per plugin mean that it takes two plugins that never answer to hold half the
workers, and four to hold all of them: a hostile plugin the admin approved can still slow the
others' requests down to the timeout; it cannot touch the game.

**What a plugin learns.** Fixed phrases, never a URL and never an address: a plugin must not be able
to map the server's network by the errors it gets back. The host's log has one line per request
(plugin, method, `scheme://host[:port]`, result, milliseconds, bytes): no path, no query, no
header.

**Values the admin sets.** A webhook URL or a token should not be in the plugin (the manifest is
public, the module is pinned and shared) and the plugin should not have it in its memory either, where
a bug or a hostile module could copy it into a request to a listed host. The manifest declares
`[[config]]` keys; the admin sets them (`pcds plugin config set`, in `config.json`, mode 0600); a
request names them as `${config.KEY}` in the path, the query or a header value and the host puts
the value in *after* the plugin's call. A key is a secret unless the manifest says otherwise, and a
secret is in no listing, log line or event; the host part of a URL cannot hold one, so the log line
(which shows the host) cannot either. A plugin whose values change is restarted with its state
handed over, like one whose files changed.

**What is not here.** No streaming of a body (a request is at most 1 MiB and an answer at most 1
MiB, so the S3 backup, whose world files are larger, stays in the bank for now: the section "Where
the built-ins stand"); no WebSocket or server-sent events; HTTP/1.1 only; no proxy and no custom
root certificates (a private CA is a change to the host, not to a manifest); no client
certificates; the system's DNS lookup cannot be interrupted, so a lookup that hangs occupies a thread
(of its own, not a worker) until the system gives up. A request in flight when the bank is
replaced by a hot update is lost: the plugin's own timeout (in `tick`) is the answer; the state of
tickets is not in the snapshot and `handoff::FORMAT` did not change.

**What it costs.** With no plugin that uses it: nothing (`poll` tests whether any request is
out, one load of a counter that is zero, once a frame; the first request starts the threads). A
request costs the plugin's call, a queue push, and on a worker a lookup, a connection and a TLS
handshake (a fresh agent each time: no pooling, by design). `make plugin-bench` and the bench are
unchanged by it (see "Measurements of the whole").

## Long-polling the stream

An integration that polls the stream every second costs a request a second and is up to a second
late. With `"wait"` the request is held and answered when there is something. Where the waiting
is: not in the bank (the game loop answers admin requests between ticks and must never sit in
one), so in the front, as for `/api/chat/events`, in the admin thread that already holds the
connection. What the front does not have is the ring (it is in the bank, in the snapshot, with
the consumers' registration), so it does not wait for events but for a **bell**: the bank sends one
small frame (`ipc::STREAM`) when the ring has a newer id than it last told, once a loop at most,
nothing when nobody listens (nothing is recorded then, so no id is new); the front counts rings
and an admin thread that is waiting asks the bank the same question again. Three properties came
out of that: the bank's side is unchanged (one question, one answer, the same code as the plain poll
and its tests); a ring between the question and the wait is not lost (the count is read before the
question); and a front and a bank of different releases still work: an older front skips the
frame (`on_live` ignores kinds it does not know) and answers a request with `wait` at once; an
older bank never rings and the front asks again every second. `ipc::PROTOCOL` did not change
(nothing a bank must send or understand was added), the snapshot did not change. Limits: 25 s a
wait, 8 waiting at once, a thread and a connection each (the admin listener's 64 are shared).
The change is in `front/` and `admin.rs`, which a hot update does not replace: it takes effect after
`pcds upgrade --restart` (docs/hot-update.md lists it).

## Budgets and faults

| What | Limit | When it is over |
|---|---|---|
| work per event | fuel: 1,000,000 by default (about 0.3 ms of plain arithmetic; a byte moved by `memory.fill` or `memory.copy` costs a quarter of a fuel, wasmi's own price being 1/64), 20,000,000 at most; the manifest may ask for less or more up to the maximum, the admin may lower it | the call stops with a trap: a fault |
| the host's work for a call | each host call costs 200 fuel besides the call itself, and each byte the host copies for it (a text, a `query` answer) 1, each byte it parses (a rules patch, an HTTP request) 16; a `log`, `say`, `tell` or `kick` text is cut at 4096 bytes, a rules patch over 64 KiB is refused (`-1`); a `query` is built once per call | the call runs out of fuel: a fault |
| work for `pcds_save` | 5,000,000 by default, 50,000,000 at most | a fault; the plugin keeps its older saved state |
| starting a plugin | 100,000,000 | the plugin is `failed` |
| memory | 64 pages (4 MiB) by default, 1024 (64 MiB) at most; one memory, one table | growth fails (the module sees -1); a module that needs more at its start does not start |
| all plugins' ticks together | 2 ms per game tick | the rest wait for the next tick and take turns (`shed` counts) |
| all plugins' saves in a hot update | 25 ms | a plugin not reached keeps the state of its last save |
| actions | 16 per call, 200 per second per plugin | refused, counted |
| log lines | 20 per second per plugin, 240 characters | dropped, counted and said once a second |
| a data plugin's rules file | 64 commands, 16 each of on_join and on_leave, 64 every, 32 when_players, 64 vars; a text 2000 bytes, a variable 1000 | not loaded (`invalid`, with the count) |
| state | 1 MiB | a fault |
| module | 8 MiB | not loaded |
| faults | 5 in 60 seconds | the plugin is switched off (`failed`) until `pcds plugin reload` or the dashboard starts it |
| HTTP requests | 2 alive and 10 a second per plugin, 4 workers and a queue of 32 for all, 16 name lookups waiting at once for all, 10 s (30 s at most) each, answers of 64 KiB (1 MiB at most) | refused (`-3`), or answered with an error |

A **fault** is a trap (including `unreachable`, which is what a Rust panic becomes with
`panic = "abort"`), running out of fuel, a pointer outside the module's memory, an answer that is
no answer (a code other than 0 and 1; a negative code the plugin reports itself), or state over the
limit. It is counted, written to the log as `plugin ID: EVENT: WHAT`, kept as `lastError`, and the
plugin is restarted from its last saved state (a new instance takes 25 µs). What a call had asked
for before it faulted is not done. The bank also catches a panic of its own host code around the
host's calls that run outside the tick, so a bug in the host is not a dead world; inside the tick
the existing rule stands (a panic of the tick saves the world and stops the bank for the front
to start again).

*A slow call* is one over 5 ms of wall time. It is counted and logged (once per ten seconds per
plugin), not a fault: fuel is what stops runaways, and a call can take long for reasons that are
not the plugin's (the machine is busy).

*What the log shows, for each offence:* the plugin's id, the event, the kind of fault. Never the
event's payload, never the chat text, never a token: the host does not log what it is given.

## Plugin state in a flip

A hot update replaces the bank. The old one is stopped at a tick boundary, writes its snapshot,
and the new one restores it and takes over. For plugins:

```
 new bank starts ─ loads tables ─ plugins::prepare (read, validate, compile; nothing runs) ─ HELLO
 ...                      the old bank still plays; this is why the compile is not in the stop
 front: HANDOFF ──► old bank: sends what is queued, plugins::checkpoint (each plugin's pcds_save,
                    25 ms for all), snapshot (ext.plugins.saved.states holds the bytes) ──► front
 front: RESTORE ──► new bank: restore the snapshot ─ plugins::attach (an instance for each plugin,
                    pcds_load(bytes, version)) ─ READY          ◄── the world stands from HANDOFF to here
```

A plugin's state is **opaque bytes and a version number the plugin chose** (`SavedState`).
The host never looks inside. A plugin changes its format by raising the version and reading the
old one in `load`, which gets the version the bytes were written with; `examples/plugins/hello-stats`
does exactly that (version 1 had one counter, version 2 has two). A server cannot know how a plugin
stores a map, so it cannot migrate it; this is the only design that does not need it to.

The state is also written to `plugins/<id>/data/<world>.state` at every world save, so a restart
or a crash keeps it. The snapshot's state wins over the file when both exist (it is newer), and
removing a plugin's folder removes its state with it.

### The compatibility rule

The project has no legacy support (docs/plugins-api.md, "Versioning and compatibility"): the
plugin part is a normal part of the current snapshot format, not a second format kept alive.

- The plugin part is `ext.plugins` of the state in `handoff::FORMAT` 7 (the binary codec of
  bincodec.rs carries it in the REST part with the rest of `ext`): `{saved: {states: {ID:
  {version, bytes}}, stream: {epoch, next, events, dropped}}}`. Every field is written; nothing is
  skipped or defaulted on reading, and a snapshot of another format is not read at all.
- A later change to the part's shape that an older reader cannot read **bumps `FORMAT`**; the front
  then refuses the hot update and the server is restarted (`pcds upgrade --restart`). Nothing
  keeps the old shape readable.
- The bytes of a plugin's state are the plugin's business: it raises its state version and reads
  the old one in `load`.
- `handoff::verify` (`PCDS_DEV=flip-verify`) compares the restored state with the snapshot's, including
  this part, and the tests restore into a second game and verify (`plugins::game_tests`, and
  `tests/hot_update.rs` through real flips).

The change to the files another agent is rewriting is small and isolated in its own commit:
`ext.rs` (one field) and the bank (the call to `plugins::checkpoint` before the snapshot, two
lines; now all the bank's calls into the host are in `bank/plugins_glue.rs`). If the snapshot becomes a binary format, `Saved` is `Serialize`/`Deserialize` and works in
any serde format; only the hex text of the states (`state.rs`, `hex_blobs`) should become raw bytes.

## Parity and fairness

- **Nothing the game does changes, with no plugin.** The events module adds calls that return at
  once; the bench fingerprint (`15c299165b6b3eb0`) and the handoff comparison were checked equal.
- **With plugins, the server is one server.** Every action goes through the code the admin API
  uses, so a plugin cannot do anything an admin could not, and nothing no player could notice. All
  players are subject to the same plugins; `tell` is the only per-player effect and is the same for
  anyone who asks the same thing.
- **No plugin sees the world while it changes**, and none changes it while it reads: a call is
  atomic with respect to the game thread.
- **Determinism**: wasmi with fuel is deterministic for a module that uses only the imports (no
  clock, no randomness, no threads exist for it). Which plugin runs first is the order of the
  folders' names. A plugin that needs time or randomness gets them from the event it is given.
- **Refusing an RPC** can leave a client out of step (above); it is the one capability that can
  make a client's view differ, and the admin sees the RPC list when granting it.

## Operations

| Task | Command | Also |
|---|---|---|
| list, with state, capabilities, cost | `pcds plugin list` | `GET /api/plugins`, dashboard "Plugins" |
| make a plugin | `pcds plugin new ID [--kind wasm\|data]` | docs/plugins-guide.md |
| check, with a test run on sample events | `pcds plugin check PATH_OR_ID` | |
| install (copies the manifest and the entry; waits) | `pcds plugin install PATH [--force]` | |
| approve | `pcds plugin enable ID [--grant ...] [--yes]` | `POST /api/plugins/enable`, dashboard |
| switch off | `pcds plugin disable ID` | `POST /api/plugins/disable`, dashboard |
| read the folder again now; start one that failed | `pcds plugin reload` | `POST /api/plugins/reload`, dashboard (the bank also looks every 2 s by itself) |

For each plugin the listing shows: its state (`running`, `pending`, `disabled`, `changed`,
`invalid`, `incompatible`, `failed`), the capabilities asked, granted and not granted, its limits,
and counters: calls, average and slowest call in microseconds, fuel, faults, restarts, slow calls,
ticks shed, refused calls, actions, vetoes, the last error and when. `/api/status` carries a
summary (loaded, running, faults). Metrics for Prometheus or the like come from a program that
reads `/api/plugins` (an out-of-process plugin); the server does not speak a metrics format.

Logs: every line a plugin or the host writes about one starts with `plugin ID:`. The plugin's
own lines (`Host::log`) are bounded and cleaned of control characters, so a plugin cannot forge a
line of the server's log.

## Distribution, signing, permissions

A plugin is a folder, installed by `pcds plugin install PATH` from wherever the admin got it. Nothing
is downloaded by the server itself. Trust is the admin's, built on the capabilities shown in plain
words at approval, the sandbox, and one of two ways of recognising the files:

- **The digest** (the default, unchanged): the approval pins the sha256 of the manifest and the
  entry; a changed file waits for the admin again.
- **A signature** (optional): the admin trusts a *publisher* instead of each file.

*Signing.* The publisher makes a key (`pcds plugin keygen acme`: `acme.key`, kept secret, and
`acme.pub`) and signs the plugin (`pcds plugin sign --key acme.key DIR`: a `plugin.sig` with an
Ed25519 signature over the plugin's digest, so over the manifest as well as the module). The admin
trusts the key (`pcds plugin trust acme.pub --name Acme --grant event.chat,chat.say`): the
**trust grant** is the most the key vouches for. What the plugin asks for within the grant needs
no approval, and every version the key signs runs as soon as it is read; what it asks for beyond
waits for the admin like any plugin, and once approved follows the publisher's updates while it
asks for nothing more. Verification is of the files as they are, at every load and reload, with ring
(already in the program for HMAC and TLS: no new dependency). Choices and reasons:

- *A signature only adds trust.* One that is missing or broken does not make a plugin worse than
  unsigned: the digest the admin pinned still decides. So a half-copied file, or a plugin that was
  signed and then edited, is a warning and a plugin that waits, never a plugin that runs unchecked,
  and never a plugin an admin approved by digest that stops for a signature.
- *The grant is part of the key, not of the plugin.* A publisher cannot widen what it is trusted for
  by changing its manifest: the grant is in the admin's file, and a manifest that asks for more makes
  the plugin wait. The default grant is nothing: trusting a key says who, not what.
- *The signer is recorded at approval*, so a plugin that needed the admin's approval for some
  capability does not need it again for the next version of the same publisher, but does for another
  signer and for a request for more than was approved.
- *Revocation is immediate and remembered.* `untrust` removes the key and lists it as revoked, and
  the plugins that ran only by that key stop at the next look at the folder (two looks, a few
  seconds), saving their state. A key listed as revoked is not trusted again by accident
  (`--force`). There is no online check: a revocation is the admin's file, and a publisher's lost key
  is announced the way any lost key is.
- *No chain of keys, no expiry, no registry.* A key is trusted or it is not; rotating is trusting
  the new key and untrusting the old. A list of plugins somebody curates is a social problem before
  a technical one and is not started; `pcds plugin install URL` for a release asset with a
  published sha256 (as `pcds upgrade` does) is not either.

Plugins that are programs have no manifest and no capabilities; what they may do is what their token
may do. The admin token may do everything, so a program should hold a **scoped token** instead
(next section).

## Scoped tokens

A program that reads the event stream and says something in the chat (the Discord bridge of
`examples/plugins/webhook-relay`) used to need the admin token: with it, a bug or a leak of that
program is the whole server (stop, ban, rules, the Steam settings). A scoped token is the same
bearer header with less in it.

- **What a token is.** `pcds plugin token create NAME --scope events.read,chat.send [--rate 600]`
  makes 24 random bytes (`pcdst_` and 48 hex digits), prints them once, and stores only their sha256 in
  `run/plugins/tokens.json` (0600) with the name, the scopes and the rate. The dashboard lists them
  and makes and revokes them (`GET/POST /api/plugins/tokens`, admin token only); `pcds plugin tokens`
  lists. A token is high-entropy, so a fast hash is the right one, as for an API key.
- **What a token opens.** Five scopes, each a (method, path) of the existing API: the stream
  (`events.read`), the chat feed (`chat.read`), saying in the chat (`chat.send`), `player.kick`,
  `player.teleport`. They are the endpoints a bridge needs and the ones that act on one player;
  the settings, the rules, the world controls, the logs, the status (which holds the access lists),
  the plugins' approvals, keys and tokens are not scoped to anything: **admin token only**,
  and a test asks each of them with a token and expects 403 (no scope covers them, so whatever scopes the token has).
- **Checks.** The front's admin thread looks at `tokens.json` for each request (one `stat`; the file is
  read again when it changed), so a revocation is effective at the token's next request, from the
  command line, the dashboard or an editor. A file that does not read makes no token valid. The
  hash is compared in constant time with every stored hash; the token is never logged, echoed,
  listed or kept by the server; a refused request is `401` (no such token), `403` (not its
  scope; the answer names the scope that is missing) or `429` (its rate, with `Retry-After`).
- **Rates.** A token has its own requests per minute (default 600, at most 6000), counted in the
  front, in a window of a minute, only for requests its scopes allow: a flood from one program
  does not use another's budget, and a long poll is one request.
- **What it does not do.** No expiry (revoke), no per-address restriction, no counters of use in
  the listing (the front keeps its counts in memory and the bank does not see them), and it does not
  make the admin API safe on a network: it is still plain HTTP on 127.0.0.1 unless the admin puts a
  tunnel or an HTTPS proxy in front (README, "Administration").
- **Hot update.** The check is in the front, which a hot update does not replace: the tokens work
  after `pcds upgrade --restart`; a front that was started before them answers `401` to a token
  (the admin token keeps working). The bank's endpoints (`/api/plugins/tokens`) work at once.

## Licensing

The server is GPL-3.0-or-later. What that means for a plugin, honestly:

- **A WebAssembly module or a program that talks to the server through the documented
  interface** is, in the project's view, a separate work, and may be under any licence, closed
  or open. The module is run in an isolated memory and exchanges bytes through eight functions
  and nine exports; the program is another process. The Free Software Foundation's own FAQ
  treats a pipe, a socket or command-line arguments as normally between separate programs, and
  says that exchanging complex internal data structures weighs towards one program. Here nothing
  internal is exchanged: the interface is small, public and versioned. **This is not legal
  advice, and no court has decided how it applies to WebAssembly.** The interface is designed to
  keep the argument on the side of "separate".
- **To remove doubt**, the copyright holder can grant an additional permission under section 7 of
  the GPL: "You may run, and may write and distribute under any licence, programs and
  WebAssembly modules that communicate with this program only through the plugin interface
  documented in docs/plugins-api.md and the admin API." That is a decision for the owner and it
  is not made here; the wording is a proposal, and the `LICENSE` file is unchanged.
- **The API crate `pcds-plugin-api` is licensed MIT or Apache-2.0**, not GPL, so that including it
  in a closed plugin does not make the plugin GPL. This is also a decision for the owner (it is
  new code in this change, so there is nothing to relicense); the `Cargo.toml` says so.
- **A native library loaded into the bank** shares its memory and its symbols with a GPL program:
  by any reading it is one program with the server, and would have to be GPL-compatible. That is
  one reason native plugins are not offered.
- **A plugin that copies server code** is a derivative of it and GPL. The API crate is the part
  meant to be copied.
- **Whoever runs a modified server** has the GPL's duties for that server; a plugin they run
  next to it, under the reading above, is not part of what they must publish.
- **Game data.** Plugins must not carry or publish the game's files or tables; the interface
  does not expose them, and the README's rule (never publish game data) applies to plugins
  as to the server.

## Where the built-ins stand

| Built-in | Today | As a user of the same API | Done here |
|---|---|---|---|
| Chat hooks (webhooks, exec, `GET /api/chat/events`) | `chathooks/`: the bank queues a chat event (`mod.rs`, a subscriber of the bus); `front/chat_hooks.rs`: the front's worker delivers | the front-side half is an integration that reads the stream; the bank-side half is `events::chat`'s first subscriber | the bank side is the first subscriber of `events` (`chathooks::wants` and `on_event`, called by `events::emit`). Replacing the front's worker by the stream needs the front-side long-poll; the file split is done (`front/chat_hooks.rs` is the front's half, `chathooks/` the bank's) |
| Rules (`speed.*`, `gauges.*`...) | `rules.rs`, read by the game's code | stays core: it changes how the game computes, and parity depends on it | plugins change rules with `rules.write`; `rules::set` emits `rule_change` |
| Backups to S3 | `s3.rs` called from `bank/persist.rs` and `bank/admin/worlds.rs`: network I/O and a thread in the game process, with process-wide statics that a flip resets | the first user of `net.http`, on `world_saved`, its status kept as plugin state | the event and the capability exist; what is missing is a body bigger than 1 MiB (a world file is tens of MB) and a signing of requests with the keys of `backup.*`. Until a streaming request body exists it stays where it is |
| `world.pauseWhenEmpty`, the pause | the clock | stays core | |

Chat has one path (`events::chat`, with the chat hooks as its first subscriber); no parallel one was
kept. When the front's worker moves onto the stream, the old worker is deleted, not kept beside it.

Things that change how the game computes are never plugins. Things that tell the outside, or add
behaviour next to the game, are.

## Measurements of the whole

Made on the benchmark world (`PCDS_BENCH_SAVE`, 300 s of game time, 4 players, `PCDS_RNG_SEED=1`),
with the build of the commit before this work (`bb6ae80`) against this one, on a host with a load
above 100, so every figure has noise of about 20 %:

- **The fingerprint is `15c299165b6b3eb0` in both, and the handoff run is EQUAL in both**: the
  same 45,558 messages and the same final save, with the plugin code compiled in and no plugin.
- **The tick, six alternating runs of each** (best, median): before 2.88 µs and 3.05 µs per tick,
  after 2.44 µs and 2.68 µs. No difference can be told from the noise, which is what 0.35
  nanoseconds per game tick should look like.
- **The snapshot** grows by 99 bytes in 2,147,378 (the empty plugin part). The stand of a flip
  (serialize and restore together, p50) was 13.6 ms before and 10.9 ms after: noise, and the
  other agent's work on that number is not touched.
- **The program**: 9.9 MB to 12.1 MB stripped (+2.2 MB, +23 %); 4.0 MB to 4.8 MB in the release
  tarball (+0.8 MB, +19 %), of which wasmi is 2.0 MB and 0.7 MB. Build time of the new
  dependencies: wasmi 25 s, the TOML reader a few seconds, on 4 loaded cores. Docker: the image is
  the program, so +0.8 MB compressed; the build copies `plugin-api/` into the build stage.
- **Loading and unloading**: the idle scan of a folder with 10 plugins takes 46 µs every 2 s. 100
  load/unload cycles of a plugin on a host with a second, steady plugin: the same instances, pages,
  commands and subscribed kinds at the end, and the tick cost 824 ns before, 782 ns after (best of
  three rounds of 20,000 ticks; the noise on this host is larger than the difference).
- **Starting a plugin**: 1.0 ms to read, validate and compile a 44 KB module (done before the
  world stops, in the new bank), 25 µs to make an instance (inside the stop). Ten plugins add
  about 0.3 ms to the stop.

**After `net.http`, signing, scoped tokens and the stream's long poll** (the branch these four came in,
cut from the integration branch, on a host with a load of about 20, so figures have noise of 20 % or
more): the bench's fingerprint is still `15c299165b6b3eb0` and the handoff comparison EQUAL (45,558
messages); `events::tick` with no plugin 0.46 ns a call against an empty loop's 0.31 ns, `events::rpc_before`
1.85 ns (before: 0.48 and 1.8); ten plugins that do nothing at every tick +3.1 µs, ten that read the world
+4.4 µs. The snapshot did not change (`handoff::FORMAT` is as it was: tickets, secrets, keys and tokens are
not in it), and no new crate came in (`Cargo.lock` is untouched): the program grew by 0.58 MB unstripped
(14.71 to 15.28 MB, +3.9 %), which is the guard, the pool, the signature code and the token store. What a
program that nobody asks for any of it pays: once a frame, `plugins::poll` tests whether a request is
out (one load of a counter when a host exists, one test of `None` when none does), and once a loop the
bank compares two numbers for the stream's bell.

**Reproducing.** `make plugin-bench` prints the host's numbers (`PCDS_BENCH_SAVE=...` adds the tick
cost on a world). The fingerprint and the handoff comparison are `make bench`. The engine
comparison is `tools/plugin-engine-bench/` (two small programs; not part of the server's build,
because it pulls wasmtime: 4 minutes of compiling): the same three workloads through wasmi,
wasmtime, a native library, Rhai and a unix-socket echo.

## Open questions

These are for the owner.

1. **The licence statements.** The API crate's licence (MIT or Apache-2.0 proposed) and the
   additional permission for plugins (the wording above). Both are the copyright holder's call.
2. **A workspace.** `plugin-api` is a path dependency, not a workspace member, because a workspace
   root moves `target/` and `Cargo.lock` and touches the Makefile, the Dockerfile and both
   workflows at once (docs/architecture.md, "Target: crates"). Do it with the refactors, or now?
3. **Long-polling the stream in the front**: built (section "Long-polling the stream"), as a bell
   frame (`ipc::STREAM`) rather than the events themselves; it takes effect with a restart of the front.
4. **Signed plugins**: built (section "Distribution, signing, permissions"). A list of them
   somebody curates, and `pcds plugin install URL`, are not.
5. **Scoped tokens** for out-of-process plugins: built (section "Scoped tokens").
6. **The component model.** Trigger: wasmi implements it, or a plugin appears that needs
   wasmtime's speed. Then a WIT file and `Runtime` for components.
7. **The `net.http` capability**: built (section "`net.http`: a plugin's way out"). Left: a request
   body and an answer that stream (the S3 backup), and the Public Suffix List instead of the short
   list, if the wildcard turns out to be wanted in practice.
8. **A panel for plugins in the web page** (a sandboxed iframe served from the plugin's files).
9. **The snapshot becoming binary** (the flip-speed work): `Saved`'s hex strings should become
   bytes then.
10. **A reload compiles on the game thread**: `POST /api/plugins/reload` and the dashboard's buttons
    read and compile the approved plugins inside the admin request, which the game loop answers
    between ticks: about 1 ms for a 44 KB module, up to 0.2 s for the biggest allowed (8 MiB).
    A hot update does not have this (the new bank compiles before the world stops). Moving the
    compile to a thread is a small change; it was left out for its extra state.
11. **`opt-level = "s"` for wasmi** (`[profile.release.package.wasmi]`) would save some of the
    2.2 MB for some speed; not measured.
