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
| base | `wire` `idmap` `geometry` `world` `save` `oldsave` `data` `protocol` `rpcspec` `transform` `gamedata` `model` `ipc` `utp` | base | Bytes, ids, the game's tables as data, the framing of the transports. No game, no process, no sockets of their own. |
| world | `net` | base | The game's network state: objects, variables, RPC messages. |
| core | `game` `handlers` `hooks` `ext` `scene` `status` `gauges` `units` `energy` `water` `machines` `logistics` `meteo` `rockets` `planets` `animals` `genetics` `players` `inventories` `instances` `objects` `panels` `deconstruct` `equipment` `toxic` `unlocks` `access` `rules` `config` `chathooks` `handoff` | base, world, core, extension | The game logic, ported class by class, with no I/O: files, sockets and threads are not in it. A subsystem is free functions over `&mut Game`, reached through `hooks.rs`. |
| extension | `events` `plugins` | base, world, core, extension | What the world tells the outside (`events`) and the host of the plugins that listen (`plugins`). |
| reader | `native` | base | `pcds read-game`: reads the game's own files into the tables. A program of its own. |
| tools | `probe` `load` | base, world, tools | Scripted players for tests and load: clients of a server. |
| bank | `bank` `dashboard` `s3` `newworld` | base, world, core, extension, reader, bank | The bank's loop and what it serves: admin requests, the world listing, off-site backups, new worlds. |
| front | `front` `admin` `updater` | base, front | Sockets, Steam, supervising the banks, the admin HTTP threads, the updater. |
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
| `admin → chathooks`, `front → chathooks` | `chathooks.rs` holds the bank's chat state and the front's feed, worker and HTTP in one file | 6 |
| `bank → cli`, `native → cli`, `native → net` | the tables folder's contract (build id, finding tables, an encoder) is defined in `cli` and `net` | 7 |
| `front → cli`, `updater → cli` | file primitives (`hold`, `replace_file`, `set_env_file`) live in `cli` | 13 |
| `front → bank`, `bank → front` | the kick batch is built in `bank.rs`; the snapshot's path in `front.rs` | 4 |
| `config → bank` | `bank::enforce` (an access policy) is called from the settings code | 4 |
| `front → config`, `front → rules`, `front → dashboard` | settings read by name; the chat rules' struct; process metrics | 8, 6 |

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
5. **An admin endpoint**: answered by the bank when it needs the world (`bank::admin_request`,
   or a module's own `admin::request` as `plugins` has), by the front when it is about the
   process (updates, stopping the world, the banks). A new endpoint needs a line in the README
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
| 5 | typed messages between front and bank (`Hello`, `Ready`, `BankInfo`) | later | `tests/hot_update.rs` |
| 6 | split `front.rs`, `bank.rs`, `chathooks.rs` by concern, moving code only | later | `cargo test`; removes `admin → chathooks`, `front → chathooks`, `front → dashboard` |
| 7 | a `tables` module that owns the tables folder's contract | later | `pcds read-game` then `pcds tables-diff`; removes `bank → cli`, `native → cli`, `native → net` |
| 8 | typed settings accessor | later | removes `front → config`, `front → rules` |
| 13 | one place for the file primitives | later | removes `front → cli`, `updater → cli` |
| 14 | a workspace, with the crates above, one at a time, base first | after 4, 7, 13 | each crate: the build, the tests and the flip |
| 15 | the flip and recovery as a state machine with its own tests | last, and the owner's call | `tests/hot_update.rs` first extended |

Everything that cuts an edge of the debt list deletes a line from `layers.txt` in the same
change, or the test says so.

## What the plugin system changed here, and what it did not

It added two modules (`events`, `plugins`), a field in `Ext`, a handful of one-line calls to
`events::*` (in `hooks.rs`, `game.rs`, `players.rs`, `handlers.rs`, `rules.rs`) and a few lines in
`bank.rs`. It did not touch the front, the transports, the snapshot's format number or the
files the other refactors are changing (`config.rs`, `handoff.rs`), and it adds no edge to the
debt list. The splits of `front.rs`, `bank.rs` and `chathooks.rs` are described above and not
started.
