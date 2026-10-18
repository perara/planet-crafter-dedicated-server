# Hot update: a new server release without disconnecting players

Status: built (branch `hot-update`). The server always runs as a front and a bank; there is no
single-process mode. Sections (a) to (e) are the design and the spike's measurements; "As built"
says what the code does now, what it measured, and what is not verified. Numbers come from this
host (shared, 32 cores) on a copy of the live world (2551 world objects, 402 network objects,
165 KB save).

The model is dual-bank firmware. Two game binaries are installed side by side (bank A and
bank B) with a pointer to the active one. An update writes the inactive bank, checks it, and
flips. The network front plays the bootloader: it stays up through the flip and owns the
sockets and the Steam connections. The old bank keeps running until the new one has
confirmed it is healthy, so a failed flip is a flip back with nobody disconnected. The world
crosses the flip as a serialized snapshot of a separate state module, in a versioned format
that each release reads in its own version and the previous one, as firmware migrates its
data sector on boot.

## As built

```
 run/bin/pcds      what the systemd unit runs (`pcds run`): it execs the active bank's
                   `pcds serve`, the front, with PCDS_BANKS=run/bin
 run/bin/a/, b/    the banks: pcds, libsteam_api.so, web/, and bank.json (version, front
                   protocol, snapshot formats, the release tag and its sha256)
 run/bin/active    "a" or "b": the bank the front starts (written beside, renamed over)
```

**Modules.** `front.rs` (the front), `bank.rs` (the game process), `ipc.rs` (frames, the
non-blocking queue, SCM_RIGHTS, the memfd), `updater.rs` (releases into a bank),
`cli/upgrade.rs` (`pcds upgrade`, the banks in `pcds status`). `split.rs` is gone. The state
module of (e) is not split out: the state types stay where they are, and `handoff.rs` holds the
snapshot format, `migrate` and `verify`. `main.rs` dispatches.

**The front** owns the UDP socket and the utp.rs sessions, the Steam game server login, its
listen socket, every `NetConnection` with its `Split<n>` reassembly, the invite code, the client
ids, the 300 ms delayed close of kicked clients, the admin HTTP listener (it serves the live
bank's `web/index.html`), the chat hooks (`chathooks::Hooks`: the event ring, the webhook and
exec worker, `/api/chat`), and the autosave timer (it sends SAVE; the bank saves only if the
world advanced). Admin requests other than `/api/stop` and `/api/update` go to the bank as ADMIN
frames with the autosave setting; the answer comes back as ANSWER, and the front adds its part to
`/api/status` (invite code, network, uptime, traffic, CPU and memory of front and banks,
`banks`, `update`). Writes to a bank never block: frames wait in a queue per bank, and a client
with more than 16 MiB waiting for the game (held during a flip, or a stalled bank) is dropped.
The bank writes blocking: a front that stopped reading stalls the world, which loses nothing.

**The bank** is the server loop's game half: it builds and parses batches, decides what goes
out when (the immediate answers, the 16 ms movement batches), kicks (the reason as DATA, then
KICK; the front closes the connection 300 ms later and says GONE), saves, and keeps the panic
guard. It ignores SIGINT and SIGTERM: the front tells it to save and stop (STOP), and a bank
whose front is gone saves and ends. Chat events go to the front as CHAT frames; the chat rules
go as CHATCFG; replies come back as SAY. Replies that arrive while the world stands still
(`world.pauseWhenEmpty`) are dropped with a log line.

**Frames** (PROTOCOL 3): HELLO, START, RESTORE, READY, CONNECTED (JSON: ip, steamId, trusted),
DATA, GONE, KICK, HANDOFF, SNAPSHOT, EXIT, RESUME, ADMIN, ANSWER, SAVE, STOP, CHAT, SAY, CHATCFG
(`ipc.rs` lists what each carries). RESTORE and SNAPSHOT carry a memfd: the old bank serializes
straight into it (`handoff::snapshot_to`), seals it, and sends the descriptor; the front passes
the same descriptor on, and keeps one as the last snapshot. The snapshot never crosses the
socket. HANDOFF carries the flip's number and SNAPSHOT echoes it: the front drops a snapshot of
any other flip (a late one from a flip that timed out). A frame shorter than its 9-byte head or
longer than 64 MiB ends the link: the front treats that bank as ended.

**A flip** (`POST /api/update`, `pcds upgrade`): the front starts the other bank; its HELLO is
checked (front protocol, game tables, and that it reads the running world's snapshot format;
a refusal names `pcds upgrade --restart VERSION` and says a restart disconnects every player,
Steam players with a new invite code unless PCDS_STEAM_TOKEN is set); HANDOFF; the old bank
sends what is queued, snapshots and waits; RESTORE; the new bank restores and says READY within
5 s (the health check); the front sends EXIT, gives the new bank the held frames and writes
`run/bin/active`. Any failure before READY kills the new bank and sends RESUME. With
`PCDS_FLIP_VERIFY` the new bank also checks that its restored state, written and read back, is
the snapshot (without the two clocks and the order of sets); the tests run with it.

**A crash** (the live bank ends outside a flip): the front starts the same bank again from the
last snapshot when it is newer than the save, else from the save (either way `access.json` and
`rules.json` are read again from disk, so a ban after the last flip stays), tells its players "The server
restarted after an error: join again" and closes their connections; the Steam login and the
invite code stay. A snapshot restored this way drops every client in it. More than 5 crashes in
10 minutes and the front exits (systemd's restart and start limit take over). At the first
start, a bank that cannot load the world makes the front try the other bank once (a broken
update falls back to the release that ran before). Every HELLO, at boot, flip or recovery, is
checked the same way; a recovering bank of another protocol stops the front (exit 1). When the
front stops while no bank holds the world (a stop during a recovery, the crash limit), the last
snapshot, if newer than the save, is written to `world.json.snapshot` and the next start
restores from it; a start whose save is newer deletes it. If a start cannot restore it, move it
away. A save that fails is never silent: `POST /api/save` answers 500 with the error, `GET
/api/status` has `lastSave` (`ok`, `at` in Unix seconds, `error`), and a failed autosave is tried
again at the next one. When the last save (at a stop, or when the front is gone) fails, the bank
writes its snapshot to `world.json.snapshot` (else to the temporary folder, and says where) and
ends with 1; the front says so and ends with 1 too.

**The updater** (`updater.rs`, in a thread of the front, or in the CLI when the server is
stopped): the release from `PCDS_RELEASES` (default GitHub's API for
perara/planet-crafter-dedicated-server), `pcds-<tag>-linux-x86_64.tar.gz` and its `.sha256` as
the release workflow publishes them; the tarball's SHA-256 (ring) against the published one;
`tar -xzf --strip-components=1` into `run/bin/.<bank>.new`; `pcds bank-info` of the unpacked
program (front protocol and snapshot formats; a release from before hot updates has none and is
refused); the compatibility check; `bank.json`; then the folder is renamed over the bank. Any
failure leaves the bank as it was (a failed rename puts the old bank back). Downloads are https
only (http only for a `PCDS_RELEASES` on 127.0.0.1, which the tests use); versions and tags
must match `v?[0-9A-Za-z._-]+`; the unpacked tree may hold only plain files and folders;
`bank-info` runs with only PATH, HOME and LANG in its environment. The sha256 guards against a
damaged download, not a forged release: releases are not signed. Renames into the banks and
writes of `active` take a lock on `run/bin/.lock`. `PCDS_AUTO_UPDATE=on` checks every
`PCDS_UPDATE_CHECK_HOURS` (default 24, the first check 10 minutes after the start) and installs
and flips to a release later than the live bank's; it is off by default, and with it off the
server never contacts GitHub on its own. A rollback writes `run/bin/rolled-back` (the bank it
left and that bank's tag); the automatic check skips that tag, and a later release, an explicit
update or the next rollback clears or replaces the note.

**The CLI.** `pcds upgrade [VERSION] [--restart] | --rollback | --check`. Running: through the
admin API (`POST /api/update`, then `GET /api/update` until its outcome). `--restart` installs
into the standby bank without the protocol check, makes it active and restarts the unit (the
path for a release that changes the front protocol). Stopped: the release becomes the active
bank for the next start; `--rollback` points `active` at the other bank. `pcds status` lists
both banks (version, tag, front protocol, snapshot format, which is active and which runs).
`setup`, `start`, `restart`, `run` and `container` install this program into a bank by one rule
(`bank_for` in `cli/setup.rs`): nothing when a bank already holds this exact program (compared by
content); into the active bank when both are empty; else into the standby bank (or an empty
active one) only when it is newer than every bank's (a later version, or the same version with a
later program file), and never into the bank the owner rolled back from. `pcds run`, which the
unit starts, runs the front as its child and stays the parent of the banks
(`PR_SET_CHILD_SUBREAPER`): SIGTERM, SIGINT and SIGHUP go to the front; when the front ends,
crashed or not, it waits up to 60 s for the banks to save, then ends what is left. The unit keeps
`KillMode=mixed`; its main process is `pcds run`. The dashboard's
Updates section shows the banks and has Check, Update now (only a newer release), Install and
Roll back; the Config tab shows PCDS_AUTO_UPDATE and
PCDS_UPDATE_CHECK_HOURS (server.env, read-only there).

### Measured

The live world's copy, 8 load-test players (60 s, an action every 200 ms each), a flip every
5 s between two banks of the same build, without the state check: the world stood **12.6 to
16.9 ms** over 10 flips (snapshot 2.2 to 2.6 MB written into the memfd in 4.3 to 6.7 ms,
restored in 8.0 to 9.9 ms, 3 to 5 client frames held). The spike's 10.5 to 29 ms included two
socket copies of the snapshot (about 12 ms); what is left is the write plus the restore, about
0.3 ms besides. No player was dropped; the load test's round trips: craft p50 0.59 / p95 0.73 /
p99 1.44 ms, build 0.59 / 1.13 / 1.19, chat 0.59 / 0.67 / 1.20, the same as without flips (see
(b)).

The memfd's share (`ipc::tests::ipc`, two runs): 4 MiB written into a memfd, its descriptor
passed there and back and the content read, 2.0 to 2.2 ms, against 11.6 to 12.1 ms for the same
bytes through the socket twice in the spike. Frames: a 120-byte round trip p50 4.1 to 4.5 µs,
p99 11 to 14 µs, back to back.

With `PCDS_FLIP_VERIFY` on the same world: all 10 restored states equal to their snapshots; the
check takes 65 to 115 ms and is inside the stand (77 to 128 ms), which is why it is a test
setting.

`bench::handoff` stays EQUAL: 4 players walking, 2 handoffs (`7d6661aa81067690`; serialize
3.6 ms, restore 7.4 ms), and 4 players acting every 200 ms with 42 handoffs
(`8a0f77419ee78444`; serialize p50 2.9 ms max 4.1, restore p50 6.7 max 9.4).

`tests/hot_update.rs` (the full suite runs it): a new world, 8 players for 40 s through 6 flips,
a flip whose new bank dies on its restore (the old one resumed after 2 ms), a flip refused at
the HELLO (another protocol: the old bank never stopped), every restored state checked; a bank
killed with SIGKILL, restarted from the last flip's snapshot, its players dropped, a new player
in; then a release server on 127.0.0.1: a tarball whose sha256 does not match (refused, bank
untouched), a release of front protocol 999 (refused with the restart message), a good release
installed and flipped to under 4 players, and a rollback; `pcds upgrade`, `status` and
`upgrade --rollback` with the server stopped; and `/api/stop` saving the world. On the test's
small world the stand is 6 to 11 ms with the check (1 to 2 ms without it).

### Not verified

- **Steam.** Steam connections, their `Split<n>` handling and the invite code live in the
  front, by the same code the single process had (moved, not changed), and pass through the
  same paths as direct-IP clients (CONNECTED with the verified SteamID, DATA, GONE). No test
  runs Steam: every run above had `PCDS_STEAM=off`. That a Steam player stays connected through a
  flip, and that the invite code survives a bank crash, follows from the design and is not
  measured.
- **A real GitHub release.** The updater ran only against the release server on 127.0.0.1
  (the same JSON fields and asset names as GitHub's API and `make dist`); GitHub was not
  contacted. Its redirects to the asset storage go through ureq's default redirect handling,
  untested here.
- **Across releases.** Every flip ran between two banks of the same build; `migrate` has no
  step yet, and a release from before this one has no `bank-info`, so the first hot update
  possible is from this release to a later one.
- **Front restarts.** Updating the front is a restart (`pcds upgrade --restart`): every player
  reconnects. Keeping the UDP socket and the sessions across it (see (d)) is not built.
- **The systemd unit** with the front and bank (KillMode=mixed, the stop through the front)
  was checked by reading, not by running the unit: the tests start the front directly.

## The answer in short (the spike)

- **Front and banks.** A long-lived front (UDP socket, Unity Transport sessions, Steam API and
  connections, the admin listener, the supervisor) and a game process started from the active
  bank (the world and the tick). They talk over a unix socket.
- **State carries over exactly.** All mutable runtime state is 85 types in 33 files today;
  serialized and restored into a fresh process, the game continues bit-exactly: the tick
  bench's fingerprint is unchanged after 1, 2, 42 and 300 handoffs, with and without the load
  test's actions. The save alone is not enough: 12 parts of the state that the save does not
  rebuild change what clients receive within seconds.
- **The pause players see.** Snapshot plus restore of the live world: p50 7 to 9.5 ms, at most
  15 ms over 300 handoffs. In the live rig, from the old bank stopping to the new one running,
  with the snapshot crossing the socket twice: 10.5 to 29 ms over 19 flips. All 8 load-test
  players stayed connected through every flip and through 4 forced failures, after which the
  old bank resumed in 8 to 14 ms.
- **The socket's cost.** About 25 µs added to a round trip (two wakeups) and about 7 µs of CPU
  per frame. Under the load test the split server answers as the single process does (p50
  0.59 ms in both) and uses 0.15 to 0.8 % of a core more.
- **Updating the front** is a restart of the bootloader. The UDP socket, the transport
  sessions and the running game process can be kept across it, so direct-IP players need not
  notice; Steam connections cannot, so Steam players are disconnected and, with an anonymous
  Steam login, get a new invite code. Keep the front small so that this is rare.

## (a) What runs where

```
 players ──UDP 7777──┐                                   ┌── game process from bank A (active)
 players ──Steam P2P─┤  front = bootloader               │   the logic: handlers, tick, rules,
                     ├─ UDP socket, utp.rs sessions      │   saving, admin answers, batching
 admin GUI ──HTTP────┤  Steam API, NetConnections        │   + the state module it works on
                     │  client ids, delayed drops        │
                     │  holds client frames in a flip ◄─unix socket─►
                     │  supervisor: start a bank, flip,  │
 pcds update-server ─┘  health check, flip back          └── game process from bank B (during a flip)

 run/bin/a/  run/bin/b/   each a whole unpacked release, checked against its sha256
 run/bin/active           which bank the front starts (a symlink, replaced atomically)
```

**Front (the bootloader).** Owns the UDP socket and every Unity Transport session (`utp.rs`:
handshake, reliability window, acks, resends, heartbeats, fragment reassembly, the
length-prefixed batch stream), the Steam game server login, its listen socket and every
`NetConnection` with its `Split<n>` reassembly, the client id counter, and the drop of kicked
clients 300 ms after their reason. It must own these because they are what a client notices:
as long as acks and heartbeats flow, a client waits patiently (resends every 200 ms, gives up
after 30 s). Steam connections cannot leave the process that holds the Steam API, so Steam
lives here. The front also holds the admin HTTP listener, so the web GUI stays up during a
flip, and forwards `/api/*` to the game.

**Game process (a bank).** Everything else: the logic modules and the state module they work
on, the tick, rules and access lists, saving and backups, the panic guard, and the batching
policy (`post`, the 16 ms movement batches, the immediate answers to a client's own packets).
It builds and parses the Netcode message batches (`wire.rs`); the front passes a batch on as
opaque bytes. Netcode framing is split on purpose: the transport framing (length prefixes,
fragments, Steam's `Split<n>`) is fixed by the game's transports and belongs with the
sessions, while batch contents and batching policy change with server releases and must flip
with the bank. The spike works this way; a broadcast then crosses the socket once per
recipient, about 25000 batches a minute at 8 players (measured).

**Banks and the updater.** `pcds update-server` (shell, as `install.sh` is): reads the latest
tag from the GitHub API, downloads `pcds-<tag>-linux-x86_64.tar.gz` and its `.sha256` into the
inactive bank's directory, checks it with `sha256sum -c` (`install.sh` already does both),
unpacks it there, and asks the front to flip (`POST /api/update {"bank": "b"}`). Only after the
new bank has confirmed does the front point `run/bin/active` at it (a new symlink renamed over
the old one), so a restart of the whole unit starts the bank that last ran well. The old bank
stays installed: flipping back needs no download. The release stays one tarball with one
binary (`pcds-server front`, `pcds-server game`, the tools).

**Supervisor.** The front, the systemd unit's process; the game process is its child (same
cgroup, so stopping the unit stops both). A game process that dies outside a flip cannot be
resumed (the clients have seen newer state than any snapshot), so the front starts the active
bank from the save and tells the clients to reconnect: today's crash behaviour, except that
the Steam login and the invite code survive it.

## (b) The boundary between front and game

One unix stream socket per game process, carrying frames: `u32 length, u8 kind, u64 client,
body`. The spike's frames (`server/src/split.rs`):

| frame | direction | body |
|---|---|---|
| HELLO | game → front | the frame protocol version (in the client field), the game version |
| START / RESTORE | front → game | nothing (load the save) / a snapshot |
| READY | game → front | restored and running: the health check |
| CONNECTED | front → game | client id; IP (the real one also: verified SteamID, trusted) |
| DATA | both | client id; one Netcode batch |
| GONE | both | client left (front → game) / drop this client (game → front) |
| HANDOFF, SNAPSHOT, EXIT, RESUME | flip | see (c) |

The refactor adds ADMIN (a request and its answer, JSON) and STATUS (the front's part of
`/api/status`: invite code, network, per-player link type).

**Ordering.** One stream socket gives one total order. A client's batches reach the game in
the order the front read them, as in today's loop; batches going out keep the order the game
wrote them, so each client's order is kept, and so is the order between a client's answer and
the broadcasts around it.

**Back-pressure.** The front never waits for the game: it keeps serving UDP and Steam (acks,
resends, heartbeats) whatever the game does. Frames for a stalled game queue in the front,
bounded per client as `utp.rs` bounds its send queue (a client over the bound is dropped, not
the world). Frames from the game are read every loop; a front that stopped reading would
stall the game's writes, which is the right direction (the world waits, nothing is lost). The
spike uses blocking writes on both sides; the front needs non-blocking writes with the bound
above (marked `ponytail:` in `split.rs`).

**Latency budget.** Today a client's own packet is answered at once (the loop wakes on the
socket). With the split the path gains two hops: the front wakes and writes a frame, the game
wakes and answers, the front wakes again. Measured between two processes
(`split::tests::ipc`, 120-byte frames, two runs):

| | p50 | p99 | CPU |
|---|---|---|---|
| round trip, back to back | 8.9 to 9.8 µs | 44 to 88 µs | 4.4 to 5.4 µs per round trip on the sending side |
| round trip once per ms, both sides asleep in between | 25 to 27 µs | 0.33 to 1.0 ms | 18 to 19 µs per round trip with the timer wakeup |
| echo side, over all 30001 frames | | | 7.8 to 8.0 µs per frame |

End to end, the load test (8 players, 60 s, an action every 200 ms each), two runs each:

| server | craft p50 / p95 / p99 (ms) | build p50 / p95 / p99 | chat p50 / p95 / p99 | server CPU per 60 s |
|---|---|---|---|---|
| one process (today) | 0.59 / 0.81 / 2.53 and 0.59 / 0.67 / 1.24 | 0.59 / 0.69 / 1.58 and 0.59 / 0.63 / 1.19 | 0.59 / 0.86 / 1.59 and 0.59 / 0.80 / 1.29 | 1.43 s and 1.59 s |
| split, no flips | 0.60 / 1.17 / 4.50 and 0.59 / 0.83 / 1.28 | 0.60 / 1.17 / 2.25 and 0.59 / 1.13 / 1.16 | 0.60 / 1.16 / 3.36 and 0.59 / 1.12 / 1.68 | 1.89 s and 1.68 s |
| split, a flip every 10 s | 0.58 / 0.62 / 1.14 and 0.60 / 1.11 / 2.28 | 0.58 / 0.77 / 2.01 and 0.60 / 1.20 / 6.68 | 0.59 / 0.63 / 1.33 and 0.60 / 1.12 / 1.70 | 1.95 s and 2.31 s |

The load test's players poll every 0.5 ms, so its p50 cannot show 25 µs: the median is the
same in all three. The tails vary more between runs of the same server than between servers
on this shared host (one split run without flips had maxima of 17 and 25 ms, the other 2.5 ms
at most). The split costs 0.1 to 0.45 s more CPU per minute (0.15 to 0.8 % of a core, about
7 µs per frame for both sides together) for about 700 frames a second (16830 to the game and
25000 back per minute), in line with the microbenchmark. The flip runs also pay five extra
process starts that load the tables.

## (c) The state module and the flip

### The state module

The owner's requirement: everything mutable lives in one module (later a crate), apart from
the logic, with an explicit, versioned snapshot format; logic modules work on it and keep
nothing of their own. The spike shows what that module contains by doing the work: it derives
`serde::Serialize, Deserialize` on every type the game's state is made of, writes `Game` as
one document (`server/src/handoff.rs`), and adds state until the acceptance test passes (the
bench below: snapshot after N seconds, restore into a fresh game, continue, same fingerprint
as an uninterrupted run).

**Contents.** `Game`'s fields except the tables: `net` (network objects, their behaviours'
variables and pending deltas, the next id, the queue), `save` (the save's sections kept as
JSON), `wos`, `inventories`, `constructed`, `pickables`, `watchers`, `tmp_inventory_id`,
`clients` (client to player, approval, verified SteamID, IP, connected since),
`saved_players`, `total_crafted`, `all_time_tokens`, `planet`, `time`, `started` (the game
clock), `sim_time`, `rng`, `timers`, `crafted_total`, `units`, `ext`, `kicks`, `access`,
`rules`, `rules_path`, `session_number`, `next_time_sync`, `save_requested`. And the types
those are made of, which today live in the logic modules and would move into the state
module:

| defined in | state types |
|---|---|
| `game.rs` | `Game`, `Client`, `PlayerState`, `Rng` |
| `net.rs` | `Net`, `NetObj`, `NetBehaviour`, `NetVar`, `VarValue`, `ListEvent`, `Target` |
| `model.rs` | `WorldObject`, `Inventory`, `Logistic` |
| `save.rs`, `access.rs`, `scene.rs`, `units.rs` | `SaveFile`; `Access`; `Timers`, `Timer`; `Units`, `PlanetUnits`, `Unit`, `Kind` |
| `ext.rs` | `Ext` (one field per subsystem below, plus `creative_mode`, `destroy_when_empty`, `deconstructing`) |
| `gauges.rs`, `status.rs`, `energy.rs`, `water.rs`, `handlers.rs` | `Gauges`, `State`; `Status`; `Energy`; `Water`, `Volume`; `FirstItemLinked`, `DefaultColors` |
| `machines.rs`, `machines/` | `Machines`, `Machine`; `Generator`, `Rays`, `AutoCrafter`, `Disintegrator`, `Optimizer`; `LinkedGrower`, `Vegetation`, `Coroutine`, `Harvestable`; `veins::State`, `Vein` |
| `meteo.rs`, `meteo/` | `Meteo`, `Asteroids`, `AsteroidEvent`, `DayNight`; `Story`; `Explode`, `Explosive`, `Explodable`, `Step` |
| `logistics.rs`, `logistics/motion.rs` | `Logistics`, `Slots`, `Task`, `State`, `Entity`, `Station`, `Drone`, `Access`; `Transform` |
| `rockets.rs`, `rockets/` | `State`, `Departure`; `back_and_forth::Machine`, `Kind`; `interplanetary::State` |
| `animals.rs`, `animals/` | `State`; `spawn::State`, `Spawner`, `Params`, `Animal`, `Pending`; `interpolation::State`, `Interpolation`; `vehicles::State`, `Vehicle`, `Garage`, `Recall` |
| `planets.rs`, `instances.rs`, `instances/generate.rs` | `planets::State`; `instances::State`, `Generator`, `WorldInstance`; `Shape` |
| `geometry.rs` | `MathRandom` (the vegetation's own random sequence) |

Some of these hold data copied from the tables when an object appears (the water volumes'
shapes, the veins' boxes, the machines' rates). They are state in the sense that matters:
the logic reads them from there, not from the tables. The snapshot carries them; the module
can later mark them as rebuildable.

**Not in it, and rebuilt by the new bank.** The tables `data`, `net_data`, `net.spec` (loaded
from `data/<version>/` before the flip, so not in the pause); `data_dir`; `trace` and the "RPC
not handled" log set; the wrecks cache in `instances` (loaded again when first needed). One
catch: `Game::new` scales the groups' generation in the tables by the world's pace and power
settings, so the tables are not quite static; `restore` runs that scaling again. This was
found by reading `Game::new`, not by the bench (the live world has both settings at 1.0).

**State the logic holds outside `Game`.** None in the game modules: they have no `static`,
`thread_local`, `OnceLock` or `Cell` (searched). Environment read while running:
`PCDS_MAX_PLAYERS` (on every join), `PCDS_TRACE`, `PCDS_RNG_SEED` (at a fresh start only); the
front passes its environment to every bank. The server loop in `main.rs` holds the batches
being filled (`mail`, sent before the snapshot, so empty), who got `ConnectionApproved`
(`approved`, rebuilt from `clients[..].approved`), the urgent list (empty at a tick boundary),
and the autosave countdown and the API's autosave setting, which belong in the state module.
Links, client ids and pending drops are the front's and never flip.

**What it took to make the state serializable** (the list of what was missing until the
acceptance test passed):

| state | problem | fix in the spike |
|---|---|---|
| `Game.started`, `Client.since` (`Instant`) | not serializable; `started` is the game clock (`Time.time`, the network tick), `since` drives the 2-minute stall kick and the API's connected time | written as seconds since, read back as that long before now |
| `ext.machines.next_due` | holds ±infinity; JSON has none (serde_json writes `null`, reading fails) | written as the f32's bits. Proven: without it the restore fails with `invalid type: null, expected f32` |
| `ext.rockets.interplanetary.items` | a map keyed by `(i32, i32)`; JSON keys are strings | written as a list of pairs. Proven: without it the snapshot does not serialize |
| the machines' unit names (`&'static str`) | cannot be read back into a static | read as strings, mapped back to the static names |
| the tables inside `Game` and `Net` | not state | skipped (given `Default`), reloaded |
| the scaled generation in the tables | derived from the world's settings at load | `restore` runs `change_generation_for_groups` again |

### The snapshot format and its versions

A snapshot is two lines: a head `{"format":1,"game_version":"2.103"}` and the state. Rust
layouts are not stable across builds, so state crosses a flip only serialized, never as shared
memory. `restore` (spike: `handoff.rs`):

- refuses a snapshot made on other game tables (state holds indices into the tables);
- reads its own `FORMAT` directly;
- reads the previous format by `migrate`, which edits the JSON before it is read (rename a
  field, fill a new one, convert a value): each release carries the step from the one before,
  as firmware migrates its data sector on boot;
- refuses anything else, and the old bank goes on. A unit test checks the refusals.

Rules for releases: a new field gets `#[serde(default)]` and needs no new format; a field
renamed, removed or changed in meaning bumps `FORMAT` and adds its step to `migrate`. CI keeps
a snapshot written by the previous release and restores it in the new build (the acceptance
test, across versions). Flipping back after a confirmed flip (B to A) needs A to read B's
snapshot; with additive changes only, B can write A's format by leaving its new fields out.
When that is not possible, flipping back goes through the save, and players reconnect.

JSON is self-describing, which is what makes `migrate` easy, and the spike's format is exact
(300 handoffs, bit-exact output). Its weak spots are non-finite floats (an `f32` holding one
fails the restore loudly, so the old bank resumes; an `Option<f32>` holding NaN would come back
as `None` silently) and size. A binary self-describing format (CBOR, MessagePack) keeps the
easy migrations, removes both weak spots and shrinks the snapshot, at the cost of a new
dependency.

### Acceptance test results

The bench (`server/src/bench.rs`, test `handoff`) runs the tick loop straight through and
again with handoffs, comparing the fingerprint of every message sent and of the final save:

| players, actions | handoffs | snapshot | serialize | restore | fingerprint |
|---|---|---|---|---|---|
| 4, walking (the brief's setting) | 2 (every 150 s) | 2.09 MB | ≤ 5.3 ms | ≤ 6.3 ms | `2763e72c7b9f5305`, equal |
| 4, walking | 300 (every s) | 2.09 MB | p50 3.9, max 7.7 ms | p50 5.6, max 9.1 ms | equal |
| 4, load-test actions every 200 ms | 2 | 2.82 MB | ≤ 9.0 ms | ≤ 8.1 ms | `3240a35e175fb17d`, equal |
| 4, actions | 300 | 2.82 MB | p50 2.6, max 4.5 ms | p50 4.3, max 8.0 ms | equal |
| 8, actions | 42 (every 7 s) | 3.59 MB | ≤ 3.2 ms | ≤ 6.1 ms | `6a7411173db30ebf`, equal |
| 0 players | 2 | 2.06 MB | ≤ 4.7 ms | ≤ 12 ms | `f45da0e1c0b4d98b`, equal (three runs) |

Snapshot plus restore, the pause players would see from the state alone: p50 9.5 ms (max
14.8) walking and p50 6.9 ms (max 12.3) with actions, over 300 handoffs each. The plain bench's
fingerprint (`2763e72c7b9f5305`) is what it was before the change, so the derives changed no
behaviour; the 144 unit tests pass.

**The save is not enough.** The bench then puts in, one part at a time, what a game freshly
loaded from the save written at that moment has, and checks whether the output changes. Parts
that change what clients receive in the rest of the run (union over the runs above):
`net.objects` (network objects, their ids and variables), `net.next_id`, `wos` (a heater's
energy is 0 live and 1 after loading), `clients`, `rng`, `units.planets`,
`units.next_compute`, `units.next_define`, `ext.gauges`, `ext.status`, `ext.meteo` (the next
weather try at 160 s live, at 60 s after loading), `next_time_sync`.

Parts that differ but did not change the output in these runs: `timers` (despawns and
deconstructions in flight), `constructed`, `save.players`, `save.planets`, `ext.machines`,
`ext.energy`, `ext.water`, `ext.logistics`, `ext.rockets`, `ext.planets`, `ext.animals`,
`units.sys_from`, the players' totals, and `time` and `started` (the bench runs on a fixed
clock; the live server's clock is `started`). That is coverage, not safety: the bench's
players pick their drops up before they despawn, never deconstruct, and the world has no
drones in flight. The design does not depend on this list: the snapshot carries all of it.

Where the bytes go (2.8 MB, actions): `wos` 1.46 MB, `net.objects` 0.73 MB, `ext.machines`
0.16 MB, `ext.water` 0.08 MB, `inventories` 0.07 MB. JSON of byte and float arrays is wordy;
the save of the same world is 165 KB.

### The flip, step by step (as the spike does it)

1. The updater has written and checked the inactive bank and asks the front to flip. The
   front starts a game process from it; it loads its tables and says HELLO with its frame
   protocol and game version. Nothing has stopped yet.
2. The front checks HELLO (see (d)). On a mismatch the new process is killed; the old bank
   never noticed.
3. Quiesce: the front sends HANDOFF to the old bank and from now on holds every client frame
   (it keeps acking, resending and heartbeating, so clients see only a delay).
4. The old bank is at a loop boundary, its tick stopped: it sends what is queued (`post`, all
   mail), writes the snapshot, sends SNAPSHOT, and waits for EXIT or RESUME with its world
   still in memory. If it cannot write a snapshot it says so and simply goes on.
5. The front passes the snapshot to the new bank (RESTORE). The new bank restores and says
   READY: the health check is READY within 5 s.
6. The front switches routing: EXIT to the old bank, the held frames to the new one, the
   active pointer to the new bank. The world stood from step 3 to here.
7. On any failure (the new bank exits, refuses the snapshot, misses the deadline) the front
   sends RESUME to the old bank and gives it the held frames. Nothing was lost: the old world
   never left memory. Players see a delay either way, no disconnect.

Measured live under the load test, 8 players: the world stood 10.5 to 28.9 ms per flip (19
flips in four runs), 0 to 9 client frames were held each time, and the four forced failures
(`PCDS_SPLIT_FAIL`) resumed the old bank after 8 to 14 ms. No player was dropped. Of the live
pause, about 12 ms is the snapshot crossing the socket twice (4 MiB there and back measured
11.6 to 12.1 ms); passing it in a memfd, or straight from the old process to the new one,
brings the stand close to the snapshot-plus-restore time above.

The game clock: `started` is restored as "elapsed before now", so game time stands still
during the pause and then goes on. Linux's monotonic clock is the same in every process, so
the refactor can instead keep the clock running through the pause (the first tick catches up,
as after a long frame); for the pauses measured, clients' network time sees about one tick
(33 ms) of difference at most either way.

## (d) Versions, health check, flip back, and updating the front

- **Frame protocol.** HELLO carries `PROTOCOL`; the front refuses a bank with another one
  before stopping anything. A release that changes the frames also needs a front update.
- **Snapshot format and tables.** As above: `FORMAT` in the head, `migrate` for the previous
  one, refusal otherwise; the game version in HELLO and in the head (the refactor should add
  the tables' build id). A game update is a cold restart in any case: the players' clients must
  update too, and old clients are refused.
- **Health check.** The spike's check is READY within 5 s (restored without error). A stricter
  check (READY plus some seconds of ticks without a panic before EXIT) narrows the window in
  which a confirmed bank can still fail, at the cost of keeping the old bank paused, that is,
  of a longer stand. After EXIT, a failing new bank can only flip back through a snapshot the
  old bank reads (see the format rules) or through the save with reconnects.
- **Updating the front (the bootloader).** The front restarts, ideally by re-executing itself
  in place. What can be kept across that:
  - the UDP socket, passed as a file descriptor (kept open across `exec`, or sent to a new
    process with SCM_RIGHTS over a unix socket);
  - the Unity Transport sessions: `utp.rs` holds plain data (addresses, sequence numbers, ack
    masks, packets in flight, queued bytes, fragments, times), which serialize like the game
    state, so direct-IP players see a pause, not a disconnect;
  - the running game process and its unix socket (a descriptor too), so the world need not
    flip at all; and the admin TCP listener;
  - not the Steam connections: they live inside the Steam API's library in the process that
    logged in, and end with it. Steam players are disconnected and rejoin; with an anonymous
    login the game server gets a new SteamID and so a new invite code (a `PCDS_STEAM_TOKEN`
    keeps it). Moving the Steam glue into its own tiny process that never updates would keep
    Steam players through front updates too, at the cost of a third process.

  None of the front restart was built in the spike. Keeping the front small keeps it rare:
  the module list below gives it only transport and supervision.

## (e) What moves where in the refactor

Front (`front.rs`, new, from `main.rs` and `split.rs`):
- `utp.rs` as it is. Its only crate dependency is `idmap::FxHashMap`; the spike's front uses it
  unchanged (one accessor for its socket was added, to wait on it with other descriptors).
- From `main.rs`: `start_steam`, `APP_ID`, `Link`, `reassemble`, `send` (Steam `Split<n>`),
  `CHUNK`, `MAX_SPLIT`, the poll part of the loop (Steam events and messages, `utp.poll`,
  client ids, `links`, `ip_clients`), `pending_drop`, the signal handler, the shutdown.
- `admin.rs` (the HTTP thread), forwarding requests to the game; the front fills
  `invite_code`, `network`, `port` and the per-player link type of `/api/status`.
- The supervisor from `split.rs`: banks, HELLO checks, HANDOFF / SNAPSHOT / RESTORE / READY /
  EXIT / RESUME, holding frames, the health check, the active pointer, restarting a crashed
  bank from the save.

Shared (`ipc.rs`, new): the frame format and kinds, `Link`, `wait`, `PROTOCOL`.

State module (`state/`, new; later a crate): the types in the table under (c), moved out of
the logic modules with their derives; `handoff.rs` (the head, `FORMAT`, `migrate`, `snapshot`,
`restore`, the encodings for clocks, infinities, tuple-keyed maps and static names); the
autosave countdown and setting from `main.rs`. The logic keeps `impl` blocks and functions
over these types, so moving a type is a change of its path, not of its code.

Logic (the game process, `gameproc.rs`, new, from `main.rs` and `split.rs`):
- From `main.rs`: `tables`, `network_tables`, the save loading, rules and access setup,
  `persist`, `backups_kept`, `guarded`, the tick and its panic guard, `post`, `send_mail` (now
  DATA frames), the movement and urgent policy, kicks (the reason batch, then GONE),
  `admin_request`, `find_client`, `enforce`, `Status`.
- The handlers, unchanged apart from the state types' new paths: `game.rs` (`impl Game`),
  `net.rs` (`impl Net`, `enc`), `wire.rs`, `protocol.rs`, `rpcspec.rs`, `data.rs`,
  `gamedata.rs`, `save.rs` (the file format), `rules.rs`, `access.rs`, `model.rs`, `idmap.rs`,
  `scene.rs`, `hooks.rs`, `handlers.rs`, `objects.rs`, `inventories.rs`, `players.rs`,
  `gauges.rs`, `status.rs`, `units.rs`, `unlocks.rs`, `energy.rs`, `water.rs`, `toxic.rs`,
  `genetics.rs`, `equipment.rs`, `deconstruct.rs`, `panels.rs`, `transform.rs`, `geometry.rs`,
  `world.rs`, and the directories `animals/`, `instances/`, `logistics/`, `machines/`,
  `meteo/`, `planets/`, `rockets/`.

Tools, unchanged, in the same binary: `load.rs`, `probe.rs` (both use `utp.rs` as a client),
`extract.rs`, `newworld.rs`, `bench.rs` (the acceptance test).

`main.rs` keeps only the subcommand dispatch. The `pcds` script's `run` starts `pcds-server
front`, which starts the active bank's `pcds-server game`.

## Not proven by the spike (at the time; see "As built")

- **Steam.** The spike's front speaks direct IP only. That Steam connections and the invite
  code stay up across flips follows from them living in the front; nothing measured it. What a
  front restart does to the invite code is read from the code, not tested.
- **Front restart.** Passing the UDP socket and the sessions to a new front is designed, not
  built or measured.
- **Coverage of the state.** Exactness is proven for this world and the bench's players
  (walking, crafting, dropping, picking up, building containers, eating, drinking, mining,
  chatting, sorting). Drones in flight, meteor events, animals, rockets, world instances and
  deconstructions are serialized by the same derives but were not seen to matter in a run. A
  bench world with them running would close that.
- **Across releases.** Both sides of every flip ran the same build; `migrate` has no step yet
  and the format rules are not tested against a real older release.
- **Banks on disk.** The bank directories, the active pointer and the updater were not built;
  the spike's front starts its own binary. The download and the sha256 check are
  `install.sh`'s, not rerun against GitHub.
- **Big worlds.** Measured: 2 to 3.6 MB snapshots, 2.5 to 9 ms to write and 4 to 12 ms to read.
  A late-game world is larger; the stand grows with the snapshot, and no such save was at hand.
- **The real front's details.** Non-blocking writes with bounded queues, the admin API and
  status through the socket, kicks with their 300 ms delay, restarting a crashed bank: designed
  above, not built.

## Running it

```
cd server && cargo test --release --no-run     # then, with LD_LIBRARY_PATH set as in the README:
PCDS_BENCH_SAVE=<copy of world.json> PCDS_RNG_SEED=1 PCDS_BENCH_SECONDS=300 PCDS_BENCH_PLAYERS=4 \
  [PCDS_BENCH_ACTION_MS=200] [PCDS_BENCH_HANDOFF=<seconds between handoffs>] \
  target/release/deps/pcds-* bench::handoff --ignored --nocapture
target/release/deps/pcds-* ipc::tests::ipc --ignored --exact --nocapture
PCDS_TEST_DATA=<tables> target/release/deps/hot_update-* --nocapture   # the end-to-end test

# a server on a scratch save, flipped by hand (both banks are this build without PCDS_BANKS):
PCDS_LISTEN=127.0.0.1:47777 PCDS_ADMIN=127.0.0.1:47778 PCDS_ADMIN_TOKEN=t PCDS_STEAM=off \
  target/release/pcds serve DATA_DIR scratch-world.json &
target/release/pcds load 127.0.0.1:47777 DATA_DIR 8 60 &
curl -X POST -H 'Authorization: Bearer t' -d '{"rollback": true}' http://127.0.0.1:47778/api/update
```
