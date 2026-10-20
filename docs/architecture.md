# Architecture

How the server is built, where new code goes, and the order in which the rough parts get
tidied. The plugin system (docs/plugins.md) stands on the first two sections; the rest is the
map for whoever changes the code next.

The audit this is based on found the game logic well organised and the edges (settings, files,
the front's size) not. The layering below is checked by a test, so it cannot slip back.

## The picture

One program, `pcds`, runs as two processes of the same file (docs/hot-update.md):

```
 players ──UDP/Steam──►  FRONT  ◄──────── admin HTTP, web page, chat hooks, updater
                           │  one unix socket per bank (frames, docs/hot-update.md)
                           ▼
                         BANK   the world: tables, game state, the tick, plugins
```

- The **front** (`pcds serve`) owns the sockets, the invite code, the admin HTTP threads, the
  chat hooks and the updater. It moves bytes for the game and knows nothing of the game's rules.
  It lives as long as the server and is replaced only by a restart.
- The **bank** (`pcds bank`, started by the front) holds the world and runs the tick. It is
  replaced at a hot update: the new bank loads its tables, takes over a snapshot of the old
  one's state, and the front switches the players over. The world stands only for the length of
  that switch (tens of milliseconds).
- The **command line** (`pcds up`, `pcds rules`, `pcds plugin`, ...) installs and tends both.

Anything that must survive a hot update either lives in the front, or is in the snapshot.
Anything the game's rules depend on lives in the bank.

## Layers

`server/layers.txt` puts every module in a layer, says what each layer may use, and lists the
edges that break the rules today (the debts). `server/tests/layers.rs` reads the source and
fails on a new edge that breaks a rule, on a module in no layer, and on a debt that has been
paid (so the list only gets shorter). `scripts/check-layers` runs that test alone.

| Layer | Modules | May use | What it is |
|---|---|---|---|
| base | `wire` `idmap` `geometry` `world` `save` `oldsave` `data` `protocol` `rpcspec` `transform` `gamedata` `model` `ipc` `msg` `worlds` `utp` `routes` | base | Bytes, ids, the game's tables as data, the framing of the transports, the typed bodies of the frames between the front and the banks (`msg`), the world files' names and places (`worlds`), the lookup of a route table (`routes`: the bank's admin requests and the front's use one type). No game, no process, no sockets of their own. |
| world | `net` | base | The game's network state: objects, variables, RPC messages. |
| core | `game` `handlers` `hooks` `ext` `scene` `status` `gauges` `units` `energy` `water` `machines` `logistics` `meteo` `rockets` `planets` `animals` `genetics` `players` `inventories` `instances` `objects` `panels` `deconstruct` `equipment` `toxic` `unlocks` `access` `rules` `config` `chathooks` `handoff` | base, world, core, extension | The game logic, ported class by class, with no I/O: files, sockets and threads are not in it. A subsystem is free functions over `&mut Game`, reached through `hooks.rs`. |
| extension | `events` `plugins` | base, world, core, extension | What the world tells the outside (`events`) and the host of the plugins that listen (`plugins`). |
| reader | `native` | base | `pcds read-game`: reads the game's own files into the tables. A program of its own. |
| tools | `probe` `load` | base, world, tools | Scripted players for tests and load: clients of a server. |
| bank | `bank` `dashboard` `s3` `newworld` | base, world, core, extension, reader, bank | The bank's loop and what it serves: admin requests, the world listing, off-site backups, new worlds. |
| front | `front` `admin` `updater` | base, front | Sockets, Steam, supervising the banks, the admin HTTP threads, the updater. `front` is a directory of small modules: see "The front" below. |
| cli | `cli` | everything | The command line. Nothing may use it. |

Two more rules in `layers.txt` that carry the plugin system: `plugins` may be used by `events`,
`bank`, `ext` and `cli` only (the game reaches plugins through `events`), and `plugins` may not
use the front, the command line, the transports or the reader: **the host lives in the bank,
next to the game, and nowhere else.** `tests/layers.rs` also spells that one out in a test of
its own.

### The debts today

Each is an edge that is there and should not be. The number is the step in "The order of
the work" that removes it.

| Edge | Why it is there | Step |
|---|---|---|
| `bank → cli`, `native → cli`, `native → net` | the tables folder's contract (build id, finding tables, an encoder) is defined in `cli` and `net`; the bank asks it in `bank/status.rs` (the installed game) | 7 |
| `front → cli`, `updater → cli` | file primitives (`hold`, `replace_file`, `set_env_file`) live in `cli` | 13 |
| `front → bank`, `bank → front` | the kick batch is built in `bank/mail.rs` (`reason_batch`); the snapshot's path (`snapshot_path`, used by `bank/persist.rs`) is in `front/snapshot.rs` | 4 |
| `config → bank` | `bank::enforce` (an access policy, in `bank/start.rs`) is called from the settings code | 4 |
| `front → config`, `front → rules` | settings read by name; the chat rules' struct | 8 |

## The front

`server/src/front/` is a directory of small modules, each with one job. The front moves bytes
for the game and keeps the world in a bank; it does not know the game's rules.

| Module | Lines | What it is |
|---|---|---|
| `mod.rs` | 370 | the `Front` struct (one field per part below), the loop (`serve`), the shutdown, `pcds serve` |
| `transport.rs`, `transport/steam.rs` | 260, 370 | the clients' connections over direct IP and Steam: one table of client ids, send, kick, close, the Steam login, the caps and the game's message framing. Knows no bank |
| `clients.rs` | 115 | relays between the transport and the live bank (CONNECTED, DATA, GONE) |
| `queue.rs` | 310 | the frames held for a bank while the world stands, and the budgets that drop a client, not the world |
| `bank_link.rs` | 370 | the banks as processes on sockets: start, boot, wait for the first frames, read, and route each frame kind (the table in its header) |
| `flip.rs` | 450 | `Phase` as a state machine (the picture is in its header); every change goes through `Front::enter`, which the tests hold to the table of transitions; the flip's checks, hand-over and failure |
| `supervise.rs` | 320 | the watchdog, the crash window, restarts, giving a release up |
| `snapshot.rs` | 85 | the last flip's snapshot, kept for a world that is in no bank |
| `updates.rs`, `history.rs` | 310, 130 | looking up, downloading and flipping to a release; the typed history, which `Front::record` is the one place to add to |
| `world_control.rs` | 190 | stop, start and switch the world |
| `ops.rs` | 110 | the rules the bank sends (OPS): the autosave timer, the update schedule, the watchdog's limit |
| `status.rs`, `metrics.rs` | 65, 110 | the front's part of `GET /api/status`; the process's CPU and memory, the traffic, the cgroup's memory |
| `admin_routes.rs` | 420 | the table of the admin API and the front's answers (below) |
| `chat_hooks.rs` | 610 | the chat feed, its worker (webhooks, exec) and the endpoints; the game's half is `chathooks/` |

The rules inside the front: a part with state is a struct with methods that take what they need
and touch no other part (`Transport`, `Queue`, `Updates`, `CrashWindow`, `Watchdog`, `Autosave`,
`Admin`); the `impl Front` blocks in the other files are where the parts meet. A frame of a bank is
a type (`msg.rs`), read in `bank_link.rs` and handled by the module the table in its header names.

**The admin API has one table**: `ROUTES` in `front/admin_routes.rs` says for each route who
answers it (the HTTP thread of `admin.rs`, the front's loop, or the bank), what it takes and what
it does (a `routes::Route<Entry>`: `routes.rs` is the one table type, the bank's `ROUTES` holds functions where this one holds an `Entry`). The front's loop and `admin.rs` route by it; `docs/admin-api.md` is written from it (a
test keeps them equal); a test holds the bank's and the plugins' `("POST", "/api/...")` arms to
the table, so a new route that is not in it fails the build. `server/tests/admin_golden.rs` pins
the status and the shape (keys, their order, types) of every route's answer in both phases against
`server/tests/golden/admin-api.txt`.

**Typed messages**: `msg.rs` (base layer) has one struct for each body that crosses between the
front and a bank, or between the programs and the banks' folder: `BankInfo` (also `pcds
bank-info` and bank.json), `Hello`, `Restore`, `Ready`, `SnapshotNote`, `SnapshotHead`,
`Connected`, `AdminCall`, `AdminAnswer`, `Ops`, `ChatEvent`, and the notes of the banks' folder
(`RolledBack`, `Fallback`). Both ends use the same definition; the
bytes are the JSON they always were (a test pins each), and a message of another release is read
leniently (keys missing are `None`, keys not known are kept when `BankInfo` is written back). Not
typed: the bodies of the admin API's requests and answers (the table says what they take, the
golden test pins the answers' shapes).

**World files** (`worlds.rs`, base layer): the name rule, where a world's save, backups and
kept copies are, and the listing of a folder, for `pcds world`, the admin API and the front.
Left as they were on purpose: how the command line and the API sort and number a removal in
the same second; `save.rs`' backup naming and `s3.rs`' own recognition of a backup's name.

## Where new code goes

Ask what the code is, in this order.

1. **A rule of the game** (what a building, a machine, a drone or a player does): the *core*.
   If it is a new class of the game, a subsystem module with the six entry points of `hooks.rs`
   (`init`, `tick`, `rpc`, `on_instantiated`, `on_destroyed`, `inventory_changed`) and its
   state in `ext.rs`. It does no I/O. It must behave as the game's own code does: parity with
   the game is the reason the server exists, and `server/tests/parity` and the README's "Parity with the game" are how it is kept.
2. **A fact about the game's files** (a table, a hash, a layout): the tables. Read once at
   start, never in the tick. A fact that is only known when the game updates belongs in what
   `pcds read-game` writes, so a game update fails at the start, not in the middle of a game.
3. **Something that happened that the outside may want to know** (a player joined, a message
   was said, a building was placed): call a function of `events.rs` from where it happens. Never
   call a plugin, a webhook or a stream from the core; `events.rs` is the one place that does,
   it checks first whether anyone listens, and it costs nothing when nobody does.
4. **Something that should be doable by third parties** (a command, a reaction, a bridge): not
   code in the server. A plugin (docs/plugins-guide.md), or a program that reads the event stream
   and uses the admin API. If the plugin API cannot express it, widen the API (docs/plugins-api.md,
   "Versioning"), do not add a special case to the core.
5. **An admin endpoint**: answered by the bank when it needs the world (a line in `ROUTES` of
   `bank/admin/mod.rs` and a handler in the file of its family, or a module's own
   `admin::request` as `plugins` has), by the front when it is about the process (updates,
   stopping the world, the banks). A new endpoint needs a line in the README
   and the web page or the command line, or it does not exist.
6. **A setting**: one entry in `config::KNOBS` (the schema; the admin page, `pcds config` and
   the checks are drawn from it). Never read an environment variable in the core.
7. **A file the server writes**: through `save::write_atomic` (a crash mid-write leaves the
   previous file). Never from the tick.
8. **A command of `pcds`**: `cli/`, one module for a family of commands (`cli/plugin.rs`);
   what it does to a running server goes through the admin API, so it works the same on a
   machine and in a container.
9. **A new module**: add it to a layer in `layers.txt`, and to `coverage.txt` with the tests
   that check it. Both are tests; neither can be forgotten.

What does not go in the core, ever: a wall clock outside `Game::clock` (the tick must be
repeatable: the benchmark's fingerprint depends on it), a thread, a socket, a file, `println!`
for anything but the log lines the server already prints, an environment variable.

## The plugin host in this picture

```
 core ──events::chat(...) / events::tick(...) ...──►  events  ──►  plugins::dispatch ──► Host::run
                                                         │                                  │
                                         chat hooks ◄────┘            each plugin: capabilities, fuel,
                                    (first subscriber, always)        a read-only view, queued actions
                                                                                │
 bank ◄── prepare (before HELLO) / attach (after restore) / checkpoint (at HANDOFF, at saves)
```

- `events.rs` is the only caller of the host from the game. Its functions test one bit of
  `game.ext.plugins.interest` first.
- `plugins/` reaches the game through one trait, `Sink` (a world to read, a place to apply
  actions), so the host is tested without game tables and could be a crate of its own.
- The bank prepares the plugins (reads, validates, compiles) while it loads its tables and
  before it says HELLO, so a hot update spends its time in the world's stop only on starting
  them, not on compiling them.
- `ext.plugins` is the one thing the snapshot carries: the plugins' saved state and the event
  stream's ring. How it stays compatible is in docs/plugins.md ("Plugin state in a flip").

## The bank, module by module

`bank/` is a directory module; each file has one job, and nothing outside it is reached except through
`mod.rs` (`run`, `info`, `reason_batch`, `contained`, `enforce`).

| File | What it owns |
|---|---|
| `run.rs` | the process: signals, the link (`PCDS_IPC_FD`), the tables, HELLO, START or RESTORE, READY, then the loop. The `PCDS_DEV=bank-fail=` test switches |
| `serve.rs` | the loop, one turn per frame: the front's frames (`handle`, `flip`), the tick, then what goes out (`deliver`) |
| `start.rs` | the world from the save (`load`) or the snapshot (`restore`), the rules and access files beside it, `enforce` |
| `admin/` | the admin requests: a route table (`ROUTES`, the type is `routes.rs`) and handlers by family (`world`, `players`, `settings`, `worlds`); typed answers |
| `persist.rs` | how a world gets to disk: the autosave thread, backups, `.before-panic`, the S3 hand-off, the last save's outcome, the final save |
| `status.rs` | the status of the bank as typed structs: tick times, backups, tables and the installed game, S3 |
| `mail.rs` | the queued messages sorted into each client's batch, the frame limit, a loading client's bound, the kick message |
| `forward.rs` | what the bank tells the front besides players' bytes: chat events and settings, OPS |
| `plugins_glue.rs` | the only calls into the plugin host |
| `panic.rs` | the containment policy: `guarded` (one client's work), `contained` (admin, hooks, plugins), the tick, `taint` |

`routes.rs` (base layer) is the shared table type: a `Route { method, path, handler }` line, a path
ending in `*` is a prefix, the method `*` any, the first match wins. The handler type is the table's
own. The bank's table is `bank::admin::ROUTES` (`fn(&mut Game, &Request) -> Reply`); the front's is
`front::admin_routes::ROUTES`, the same `Route` with an `Entry` (who answers, what it takes, what it
does) for its handler (see "The front" above). Both have a test that no route is shadowed.

The JSON the bank answers with is typed where a struct is easy: the status (`status::Status`, the
tick, backup and S3 parts), the answers of save, pause, backup and the rules, and the messages to the
front (READY, the snapshot's note, OPS: the structs of `msg.rs`). Golden tests (`the_*_prints_what_the_json_macro_did`) keep
the old hand-built JSON in the test and compare bytes: the API's names, their order, and numbers
(the game's f32 values go out as the f64 they widen to, as `json!` made them). The HELLO (`info()`) is `msg::BankInfo`, which `pcds bank-info` and `pcds up` extend.

## Chat hooks and the event bus

```
 game ──events::chat──► events::emit ──► chathooks::on_event  (wants: chat, always)
                                   │              │ State::outbox (the typed Event)
                                   │              ▼ bank/forward.rs: CHAT frame, CHATCFG when the chat rules change
                                   └──► plugins::dispatch (the kinds a plugin asked for)
                                                  │
                    FRONT: chathooks::Feed (ring) ──► the worker: webhooks, exec ──► replies ──► SAY frame ──► chathooks::say
```

There is one path from a game event to the hooks: the game calls `events::*`, and `events::emit`
hands the event to its subscribers in turn, each only for the kinds it `wants`: the chat hooks first
(`chathooks::wants` is true for chat messages only; `chathooks::on_event` queues the typed `Event`
for the front), then the plugins. A third listener is a line in `emit` and in `wants`.

`chathooks/mod.rs` is the bank's half: the command prefix parsing, `State` (outbox, replies,
changed), `say`, `tick`, and the subscription (`wants`, `on_event`). The chat event is `msg::ChatEvent`
(`chathooks::Event` is the same type), the message of the CHAT frame and the JSON of `GET
/api/chat/events`. The front's half is `front/chat_hooks.rs`: `Feed`, `Hooks` (`configure`,
`replies`), the worker, the signed webhook POST, the exec program, `api`. The two meet in the CHAT,
CHATCFG and SAY frames only (`ipc.rs`), and `admin.rs` reaches the feed through `front::chat_api`.
There is one delivery worker; the debts `admin → chathooks`, `front → chathooks` and `front →
dashboard` are paid.

## Target: crates instead of modules

Modules can be kept apart by a test, which is what is done now. Crates are kept apart by the
compiler, which is better and costs a workspace. The target:

| Crate | Holds | Depends on |
|---|---|---|
| `pcds-wire` | the base layer: `wire` `idmap` `geometry` `protocol` `rpcspec` `transform` `utp` `ipc` | nothing |
| `pcds-tables` | `data` `gamedata` `model` `world`, the tables folder's contract (one owner), and `native` behind a feature | `pcds-wire` |
| `pcds-core` | `net`, the game, its subsystems, `rules` `access` `handoff` `events`; the `Sink` impl | `pcds-wire`, `pcds-tables` |
| `pcds-plugin-api` | the events, queries and codec (exists: `plugin-api/`) | nothing |
| `pcds-plugin-host` | `plugins/` without the glue: manifests, approvals, runtimes, budgets, `Sink` | `pcds-plugin-api`, wasmi, toml |
| `pcds-front` | `front` `admin` `updater` and the chat feed | `pcds-wire` |
| `pcds-bank` | `bank` `dashboard` `s3` `newworld` | `pcds-core`, `pcds-plugin-host` |
| `pcds-cli` | `cli` | everything it installs and tends |
| `pcds` | `main.rs`: picks a subcommand | all |

The front depending on no game crate is the point: a change to the game's rules would no longer
recompile the front, and the front could not grow a dependency on the game by accident. The
price is a workspace root: `server/Cargo.toml` becomes members, the Makefile, the Dockerfile
and the two workflows move from `server/target` to `target`, `Cargo.lock` moves, and
`CARGO_MANIFEST_DIR` uses in tests change. None of it is hard; all of it touches every build
script at once, which is why it comes after the debts are paid, not before, and why it was not
done in the same change as the plugin host.

`plugin-api/` is a plain path dependency of the server today (not a workspace member) for that
reason; the Dockerfile copies it next to `server/`.

## The order of the work

Small steps, each safe on its own, each with the check that proves it. The numbers are those of
the audit's list (the ones not done here are another agent's or later).

| Step | What | State | Proof |
|---|---|---|---|
| 0 | `layers.txt` and its test; the `events` module; the plugin host | done with the plugin system | `scripts/check-layers`; zero behaviour change: the bench fingerprint and the handoff comparison are equal |
| 1-3 | single source of truth for settings and rules, delete dead code | the refactor1 branch | its own |
| 4 | small relocations: `DISCONNECT_REASON` and `reason_batch` into `protocol.rs`; `enforce` into `access.rs`; the serde helpers of `handoff` into a neutral module; `now_secs`, `hex` and friends once | next | removes the debts `front → bank`, `bank → front`, `config → bank` |
| 5 | typed messages between front and bank (`Hello`, `Ready`, `BankInfo`, ...) | done for the frames, bank.json and the banks' folder's notes (`msg.rs`); the API's requests and answers are not typed | `tests/hot_update.rs`, `msg::tests`, `tests/admin_golden.rs` |
| 6 | split `front.rs`, `bank.rs`, `chathooks.rs` by concern, moving code only | `front/`, `bank/` and `chathooks/` done (the modules above and in "The bank, module by module") | `cargo test`; removed `admin → chathooks`, `front → chathooks`, `front → dashboard` |
| 7 | a `tables` module that owns the tables folder's contract | later | `pcds read-game` then `pcds tables-diff`; removes `bank → cli`, `native → cli`, `native → net` |
| 8 | typed settings accessor | later | removes `front → config`, `front → rules` |
| 9 | one worlds module | done for the names, the paths and the listing (`worlds.rs`) | `worlds::tests`, `tests/cli.rs`, `tests/admin_golden.rs` |
| 11 | one admin route table | done: the table, `docs/admin-api.md`, the test against the source, the golden test; not done: typed requests and answers, one timeout table, dropping routes | `front::admin_routes::tests`, `tests/admin_golden.rs` |
| 13 | one place for the file primitives | later | removes `front → cli`, `updater → cli` |
| 14 | a workspace, with the crates above, one at a time, base first | after 4, 7, 13 | each crate: the build, the tests and the flip |
| 15 | the flip and recovery as a state machine with its own tests | last, and the owner's call | `tests/hot_update.rs` first extended |

Everything that cuts an edge of the debt list deletes a line from `layers.txt` in the same
change, or the test says so.

## What the plugin system changed here, and what it did not

It added two modules (`events`, `plugins`), a field in `Ext`, a handful of one-line calls to
`events::*` (in `hooks.rs`, `game.rs`, `players.rs`, `handlers.rs`, `rules.rs`) and a few lines in
the bank (now `bank/plugins_glue.rs`). It did not touch the front, the transports, the snapshot's format number or the
files the other refactors are changing (`config.rs`, `handoff.rs`), and it adds no edge to the
debt list. The splits of `front.rs`, `bank.rs` and `chathooks.rs` are done ("The front" above).
