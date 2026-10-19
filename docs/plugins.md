# Plugins: the design

Third parties can add commands, rules, reactions and integrations to the server without forking
it. This document says what was chosen and why, with the measurements it rests on. How to write a
plugin is docs/plugins-guide.md; the exact interface is docs/plugins-api.md; where the code lives
is docs/architecture.md.

## The decision in ten lines

1. **Logic that runs inside the world is a WebAssembly module**, a plain core module (no WASI, no
   component model) with seven imports and seven exports, run by **wasmi**, an interpreter in
   pure Rust. Rust, C, C++, Zig, AssemblyScript and TinyGo can all make one.
2. **Small jobs are data**: a `rules.toml` of chat commands, welcome messages, announcements and
   rule switches that the host runs itself. No code, nothing to sandbox, nothing that can loop.
3. **Integrations are programs of their own** that read the event stream
   (`POST /api/plugins/events`) and use the admin API, in any language, on any machine. A crash
   of one never touches the game.
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
    world stops and only starts them after the restore. `handoff::FORMAT` is unchanged.

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
host for an interface of seven functions; and WASI itself is moving (0.2 to 0.3). A core module
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
a hot update, and the `epoch` tells a reader that the server was restarted. It is polled today:
a long-poll in the front (like `/api/chat/events`) needs a new frame between front and bank, which
an old front cannot be taught by a hot update, so it waits for a release that also restarts the
front (see "Open questions"). Out-of-process plugins have no manifest, no capabilities and no
budget: they hold the admin token and can do what it can. That is stated in the dashboard and in
`examples/plugins/webhook-relay`.

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

## Capabilities and approval

A manifest (`plugin.toml`) asks for capabilities by name; the table is in docs/plugins-api.md.
Asking is not having: the admin grants (`pcds plugin enable ID`, or the dashboard), shown each
capability in plain words, and may grant less than was asked (`--grant event.chat,chat.say`).
`approved.json` records the grant and a **digest** (sha256 of the manifest and the entry file).
At every start and reload the host recomputes it; a plugin whose files changed since the approval
is `changed` and runs nothing until approved again, so an update or a swapped file cannot slip
in. A file that cannot be read, or an `approved.json` that does not parse, means nothing is
approved (never "everything is allowed").

The checks are at two places, so a missing capability shows up where it is used: **events** are
only delivered if the capability for their kind is granted (a warning in the listing tells which
are not), and every **host function** checks its capability when called (a refused call returns
an error code to the plugin, counts in `denied`, and does nothing). `pcds plugin check` compares
the host functions a module imports with the capabilities its manifest asks for, so most
mismatches are found before installing.

## Budgets and faults

| What | Limit | When it is over |
|---|---|---|
| work per event | fuel: 1,000,000 by default (about 0.3 ms of plain arithmetic), 20,000,000 at most; the manifest may ask for less or more up to the maximum, the admin may lower it | the call stops with a trap: a fault |
| work for `pcds_save` | 5,000,000 by default, 50,000,000 at most | a fault; the plugin keeps its older saved state |
| starting a plugin | 100,000,000 | the plugin is `failed` |
| memory | 64 pages (4 MiB) by default, 1024 (64 MiB) at most; one memory, one table | growth fails (the module sees -1); a module that needs more at its start does not start |
| all plugins' ticks together | 2 ms per game tick | the rest wait for the next tick and take turns (`shed` counts) |
| all plugins' saves in a hot update | 25 ms | a plugin not reached keeps the state of its last save |
| actions | 16 per call, 200 per second per plugin | refused, counted |
| log lines | 20 per second per plugin, 240 characters | dropped, counted and said once a second |
| state | 1 MiB | a fault |
| module | 8 MiB | not loaded |
| faults | 5 in 60 seconds | the plugin is switched off (`failed`) until `pcds plugin reload` or the dashboard starts it |

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

### The exact compatibility rule

- `handoff::FORMAT` is **not** changed (it is 3, as before). The plugin part is a new field of
  `Ext`: `plugins` (`#[serde(default)]`), holding `{"saved": {"states": {ID: {"version": N, "hex":
  "..."}}, "stream": {"epoch", "next", "events", "dropped"}}}`. Every field of it has a default.
- **A new bank reading an older snapshot** (no `plugins` part): the default is used; plugins
  start from their files on disk (the last world save), or empty.
- **An older bank reading a newer snapshot**: the unknown `plugins` field is ignored (no type of
  the snapshot denies unknown fields); such a bank has no plugin host, so nothing was lost that it
  could have used. A hot update *to* an older release (rollback) therefore keeps the world and
  drops plugin state from the snapshot; the file on disk still holds the last saved state.
- **Inside the part**: adding a field needs `#[serde(default)]`; renaming or removing one needs a
  `FORMAT` bump and a step in `handoff::migrate`, as for any state. The bytes of a plugin's state
  are the plugin's business and its `load`'s.
- The check `handoff::verify` (PCDS_FLIP_VERIFY) compares the restored state with the snapshot's,
  including this part, and the tests restore into a second game and verify
  (`plugins::game_tests`, and `tests/hot_update.rs` through real flips).

The change to the files another agent is rewriting is small and isolated in its own commit:
`ext.rs` (one field) and `bank.rs` (the call to `plugins::checkpoint` before the snapshot, two
lines). If the snapshot becomes a binary format, `Saved` is `Serialize`/`Deserialize` and works in
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
| read the folder again; start one that failed | `pcds plugin reload` | `POST /api/plugins/reload`, dashboard |

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

Today: a plugin is a folder, installed by `pcds plugin install PATH` from wherever the admin got
it. Trust is the admin's, built on three things: the capabilities shown in plain words at approval,
the digest pinned at approval (a changed file waits again), and the sandbox. Nothing is downloaded
by the server itself.

Planned, and designed so that none of it changes a plugin: a `plugin.sig` next to the manifest
holding an Ed25519 signature of the digest, checked against keys the admin put in
`plugins/trusted-keys` (the approval screen would say "signed by KEY" or "unsigned"); `pcds plugin
install URL` for a release asset with a published sha256 (as `pcds upgrade` does); and a list of
plugins somebody curates. A registry is a social problem before it is a technical one and is not
started.

Plugins that are programs have no permissions at all beyond the admin token. Scoped tokens (a
token that may read the stream and `say`, nothing else) are the missing piece; they are a
change to the admin API and are listed in "Open questions".

## Licensing

The server is GPL-3.0-or-later. What that means for a plugin, honestly:

- **A WebAssembly module or a program that talks to the server through the documented
  interface** is, in the project's view, a separate work, and may be under any licence, closed
  or open. The module is run in an isolated memory and exchanges bytes through seven functions
  and seven exports; the program is another process. The Free Software Foundation's own FAQ
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
| Chat hooks (webhooks, exec, `GET /api/chat/events`) | `chathooks.rs`: the bank queues a chat event, the front's worker delivers | the front-side half is an integration that reads the stream; the bank-side half is `events::chat`'s first subscriber | the bank side is already the first subscriber of `events`. Replacing the front's worker by the stream needs the front-side long-poll, and the file split (docs/architecture.md, step 6): not done |
| Rules (`speed.*`, `gauges.*`...) | `rules.rs`, read by the game's code | stays core: it changes how the game computes, and parity depends on it | plugins change rules with `rules.write`; `rules::set` emits `rule_change` |
| Backups to S3 | `s3.rs` called from `bank.rs`: network I/O and a thread in the game process, with process-wide statics that a flip resets | the first user of a future `net.http` capability, on `world_saved`, its status kept as plugin state | the event exists; the capability and the thread model do not. Until then it stays where it is |
| `world.pauseWhenEmpty`, the pause | the clock | stays core | |

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
- **Starting a plugin**: 1.0 ms to read, validate and compile a 44 KB module (done before the
  world stops, in the new bank), 25 µs to make an instance (inside the stop). Ten plugins add
  about 0.3 ms to the stop.

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
3. **Long-polling the stream in the front.** Needs a frame between front and bank (`EVENTS`), so
   it takes effect only after a restart of the front, and the front's protocol number may stay
   the same (old banks never send it). Worth doing before the first release with plugins?
4. **Signed plugins and a list of them.** Not started; wanted by whom?
5. **Scoped tokens** for out-of-process plugins.
6. **The component model.** Trigger: wasmi implements it, or a plugin appears that needs
   wasmtime's speed. Then a WIT file and `Runtime` for components.
7. **The `net.http` capability** (a host function that posts to hosts the manifest lists and the
   admin approved, run on a thread of the host) would let S3 and Discord run inside the bank. The
   risks (requests to the local network) need a design of their own.
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
