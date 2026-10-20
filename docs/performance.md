# Performance: what the server costs, how to measure it, how to size it

What a server costs at rest and under players, in numbers that do not depend on how busy the machine
was when they were taken; the budgets a test holds the build to; how to profile; how to read the
tick histogram of the status page; and how big a machine a world and its players need.

Everything here was measured on one host (an Intel i9-14900K, Linux 7, a shared machine with a load
average of 60 to 130 during every run). A busy host stretches times, not **instructions**, so the
tables count user instructions (`perf stat -e instructions:u`, or the counter the bench reads
itself) and allocations; times are given where a person feels them (a start, a join, the world
standing still for a hot update) and are the middle of several runs on that loaded host, so an idle
host is faster.

The numbers were taken on the build before the operations work (logging, metrics, hardening) was merged in; that work adds nothing to a loop. The worlds: **bench** is the maintainer's mid-game save (2,700 world objects, 520 network objects);
**late** is a late-game save (33,400 world objects, 3,200 network objects, 2,300 inventories, drones,
hundreds of machines and logistics inventories; on the DLC planet Humble); **100k** and **200k** are the
bench world grown with `tools/grow-world.py` to 99,500 and 199,000 world objects (14,800 and 29,700
network objects).

## What it costs

### The world's tick (the bank)

User instructions per simulated second of the bank's game loop (`bench::bench`: the same clock, seed
and scripted players for every build; 200 ticks a second; players walk and act every 200 ms: craft,
drop, build, eat, mine, chat, sort). Build 0.8.0, then this one.

| World | Players | 0.8.0 | now | | allocations a tick (0.8.0 -> now) |
|---|---|---|---|---|---|
| bench (2.7k objects) | 0 | 1.3 M | 1.0 M | 1.2x | 1 -> 1 |
| bench (2.7k objects) | 4 | 3.1 M | 2.7 M | 1.1x | 10 -> 10 |
| bench (2.7k objects) | 8 | 4.7 M | 4.1 M | 1.1x | 19 -> 18 |
| bench (2.7k objects) | 32 | 13.7 M | 12.6 M | 1.1x | 76 -> 75 |
| bench (2.7k objects) | 100 | 45.3 M | 42.8 M | 1.1x | 279 -> 278 |
| late (33k) | 0 | 195.6 M | 24.2 M | 8.1x | 141 -> 77 |
| late (33k) | 4 | 202.8 M | 31.8 M | 6.4x | 154 -> 91 |
| late (33k) | 8 | 207.0 M | 36.2 M | 5.7x | 163 -> 100 |
| late (33k) | 32 | 226.2 M | 54.1 M | 4.2x | 222 -> 162 |
| late (33k) | 100 | 329.0 M | 107.8 M | 3.1x | 427 -> 366 |
| 100k | 0 | 16.0 M | 10.8 M | 1.5x | 1 -> 1 |
| 100k | 4 | 43.1 M | 35.5 M | 1.2x | 10 -> 10 |
| 100k | 8 | 56.3 M | 47.0 M | 1.2x | 19 -> 18 |
| 100k | 32 | 73.2 M | 61.1 M | 1.2x | 76 -> 76 |
| 100k | 100 | 112.0 M | 98.4 M | 1.1x | 280 -> 279 |
| 200k | 0 | 31.1 M | 20.7 M | 1.5x | 1 -> 1 |
| 200k | 4 | 84.2 M | 69.1 M | 1.2x | 10 -> 10 |
| 200k | 8 | 109.3 M | 91.1 M | 1.2x | 19 -> 18 |
| 200k | 32 | 134.4 M | 111.0 M | 1.2x | 76 -> 76 |

(0 players: idle; the others walk and act every 200 ms. The fingerprints of every case, messages and save, are those of 0.8.0.)

An instruction is about 0.2 ns on this host when the caches are warm: 100 M instructions a second
is 2% of a core. The real server adds what a bench does not have: the wake-up of a sleeping
processor every 5 ms (see Idle below).

### The front

The front only moves bytes: UDP sockets, the 5 ms frame, the link to the bank. User instructions
per second on the late save with `pcds load` players acting every 200 ms (the kernel's part, the
UDP system calls, is larger than the program's):

| Players on the late save | front 0.8.0 | front now | bank 0.8.0 | bank now |
|---|---|---|---|---|
| 8 | 15.0 M | 8.6 M | 405 M | 72 M |
| 32 | 89 M | 36.6 M | 439 M | 128 M |
| 64 | 266 M | 90 M | 589 M | 227 M |

(The bank's figure is a real bank, so it includes its idle floor: 380 M a second before, 45 M now. The front's kernel part, the system calls of UDP, is 64 M, 240 M and 490 M a second at 8, 32 and 64 players and did not change. Three times fewer instructions in the front is a smaller share of its time than of its count: the byte-by-byte loop it lost ran at a high rate of instructions per cycle; by `/proc` the front at 64 players is 2.4% user and 5.7% system of a core, against 2.6% and 5.3%.)

On the wire (`GET /api/status`, `traffic.wire`; this host's loopback, the bench world, players
acting): | Players | out, KB/s (per player) | datagrams out/s | of them only acknowledging | batches inside |
|---|---|---|---|---|
| 8 | 79 (9.9) | 698 | 277 | 60 KB/s |
| 32 | 876 (27.4) | 2,788 | 1,089 | 798 KB/s |
| 64 | 3,294 (51.5) | 6,216 | 2,177 | 3,121 KB/s |

On the late save, 64 players: 3,820 KB/s out (59.7 each).

The headers of UTP are 26 bytes a datagram. A datagram that only acknowledges one the client
sent is a third of the datagrams and a few percent of the bytes: acknowledging later, with the next
batch, would save about a third of the datagrams, but the client times its resends from the
acknowledgements and nothing here could be tried against the game's own client, so the server
acknowledges at the next frame as it did. The batches themselves are the game's bytes (the
transform of every other player is the O(n squared) part of the traffic, as in the game).

### Idle

Nobody connected, nothing under way. The bank's loop wakes up every 5 ms, as the game's does, and
runs one tick; the front wakes up every 50 ms (a datagram or a frame from the bank wakes it at
once).

| | 0.8.0 | now |
|---|---|---|
| bank, late save, user instructions a second | 380 M (491 M cycles) | 45 M (155 M cycles) |
| bank, 200k, in the bench's own counter | 31.1 M | 20.7 M |
| bank, bench world | 2.9 M (49 M cycles) | 2.7 M |
| front, user instructions a second | 0.29 M | 0.12 M |
| front wake-ups a second | 49 to 54 | 20 to 22 |
| bank wake-ups a second | 206 to 217 | 204 to 248 |
| CPU of a core, bench world (user + system) | 0.9% | 0.8% |

On a small world the bank's instructions are few (a tick is 1 to 3 microseconds warm) but each wake-up
finds the caches cold, so a tick costs far more cycles than its instructions say (2.7 M instructions
and 49 M cycles a second on the bench world): that, and the 200 wake-ups a second, make the bank's
floor **0.8 to 0.9% of a core**, above the 0.3% that would be wished for. The front is at 0.1 to 0.2%.
The floor cannot be cut without changing what the world does between two ticks (every timer is set
from the tick that fires it, so a slower idle tick moves production by milliseconds a cycle); the
rule `world.pauseWhenEmpty` makes a world with nobody in it cost a wake-up and nothing else.

### Start, save, join, hot update

| | 0.8.0 | now |
|---|---|---|
| Game::new (CPU) on late / 100k / 200k | 341 / 508 / 973 ms | 197 / 334 / 739 ms |
| a new bank's tables and scenes | 76 + 23 ms | the same |
| cold start of the front and bank to "running", late save | 0.6 to 0.7 s | 0.7 to 0.9 s (a host with a load of 100: the two processes and the tables are most of it) |
| the save's text on the game thread, late / 100k / 200k | 9.8 / 28 / 53 ms | the same (an attempt to do better is below) |
| a join on the game thread, late / 100k / 200k (bytes sent) | 2.2 / 6.7 / 12 ms (266 KiB / 1.2 MiB / 2.4 MiB) | the same |
| hot update, the world standing, in user instructions (snapshot, restore, all threads), late / 100k / 200k | 385 / 1,080 / 2,130 M | 223 / 625 / 1,239 M (-42%) |
| the same in time, p50 of the better of two runs (min), in process | 102 (44) / 276 (154) / 513 (226) ms | 32 (16) / 76 (43) / 162 (115) ms |
| the same with the real front and two banks, no players, 100k / 200k | 105 to 118 / 277 to 306 ms | 62 to 67 / 95 to 129 ms |

The stand is one number that does not move with the host: 6,700 instructions a world object (11,500 before). Its largest cost was
reading each machine's tables again after the restore: 52 / 172 / 342 ms at 33k / 100k / 200k objects; it is read once for each group of a machine now.

### Memory

The resident memory of the bank (from `/proc/PID/smaps_rollup`, five seconds after it says running, on the
same four worlds), the peak of its start (`VmHWM`), and what a hot update holds at its largest (the old
bank at rest, the new bank at its peak, the snapshot, the front). 0.8.0 is the build this work started from.

| World | Bank at rest, 0.8.0 -> now | Beyond the bench world, per world object | Start peak | Hot update's peak (old bank + new bank's peak + snapshot + front) |
|---|---|---|---|---|
| bench world (2.7k objects) | 43.8 -> 21.5 MiB (-51%) | | 44 -> 25 MiB | 96 -> 57 MiB |
| late (33k) | 77.2 -> 32.6 MiB (-58%) | 1.11 -> 0.37 KiB | 79 -> 35 MiB | 154 -> 74 MiB |
| 100k | 122.0 -> 43.1 MiB (-65%) | 0.83 -> 0.23 KiB | 166 -> 52 MiB | 264 -> 114 MiB |
| 200k | 198.8 -> 66.3 MiB (-67%) | 0.81 -> 0.23 KiB | 298 -> 87 MiB | 437 -> 179 MiB |

(Sum for the hot update, 200k: old bank 65.8, new bank 80.5, snapshot 23.3, front 9.6 MiB; the new bank's
peak is where it has the snapshot mapped and its world half built.) The front is 9 MiB, of which 8.3 are
the program's own file pages, shared with the bank and reclaimable; its anonymous memory is 0.7 MiB.

Where the bytes are, the bank's heap by part (`bench::scale`, MiB), and what each part became:

| | late (33k), 0.8.0 -> now | 200k, 0.8.0 -> now | How |
|---|---|---|---|
| world objects | 20.6 -> 3.7 | 82.5 -> 16.1 | 24 bytes for the nine in ten that hold only an id and a group (a placed one boxes its fields, 304 bytes before for each); 32-bit slot indexes |
| network objects | 5.5 -> 1.3 | 50.0 -> 15.5 | words (class, variable name, type) interned and the shape of an object shared; values 24 bytes inline |
| machines | 2.3 -> 0.6 | 34.1 -> 5.5 | 48 bytes for the nine in ten that have no component (584 before) |
| inventories | 1.2 -> 1.2 | 6.1 -> 5.9 | |
| holders, constructed, pickables, network index | 0.5 -> 0.5 | 2.7 -> 2.4 | |
| rockets, water, the rest of the game | 1.0 -> 0.8 | 2.0 -> 1.5 | |
| tables: layouts | 3.0 -> 1.8 | 3.0 -> 1.7 | words interned |
| tables: assets, groups, panels | 9.6 -> 1.8 | 9.7 -> 1.8 | `Doc`, a quarter of a `Value` |
| tables: prefabs (text, and parsed as the world has buildings) | 17.4 -> 4.6 | 8.2 -> 2.7 | compact text, `Doc` |
| tables: scene dumps | 8.8 -> 1.9 | 8.8 -> 2.1 | compact text, parsed when a planet needs it |
| **heap in use** (the parts above and what is not counted in them) | **71.6 -> 20.3** | **213.6 -> 62.2** | |
| RSS after the load and a trim | 79 -> 36 | 200 -> 74 | the program's own pages (10) and malloc's free space are the rest |

**Per player** (`pcds load` players on a live server, RSS 20 seconds after they joined): on the late save
the bank keeps 6 KB (an inventory, an equipment and a player object; 25 to 30 KB before) and the front 35 to 50 KB
(55 to 60). On the 200k world a join is 2.4 MB: the first 16 players leave the bank 4 to 6 MiB bigger (9 before) and the
next 32 nothing; the front is 10.5 MiB with 16 players, 1.4 of it anonymous (21.5 and 13.2 before). The front's room
for a world sent (a session's send queue, the link's inbox) goes back to the system when it is empty, and so do
the frames, which are mappings of their own past 128 KiB (`util::fix_malloc_threshold`).

The memory is held by budgets (next section): `bench::budget::the_memory_of_the_world_stays_within_its_budget`.

## The budgets

A change that makes the server dearer should fail a test, not wait for a player to notice. These tests
hold the build to budgets of about one and a half times what it measures (a third above it for the memory ones), in things that do not
depend on the machine's load (allocations, bytes, user instructions); they need the game tables, like
every game test, so CI (which has none) skips them, except the first, which needs none.

| Test | What it counts | Measured | Budget |
|---|---|---|---|
| `utp::tests::polling_idle_sessions_and_sending_a_batch_allocate_a_bounded_number_of_times` | allocations in 100 polls of 64 idle sessions; in a flush of 4 datagrams | 0; 13 | 0; 24 |
| `bench::budget::the_tick_stays_within_its_budget` | allocations and user instructions per tick on the guard world (a test base of 0, 400 and 1,600 buildings: containers that supply and demand, water collectors, autocrafters, a drone station), with 0 and 4 players | see below | see below |
| `bench::budget::a_tick_costs_about_the_same_per_building_at_four_times_the_size` | instructions per tick and world object at 1,600 buildings against 400 | 0.84 (10.6 against 12.7) | at most 2 |
| `bench::budget::the_snapshot_the_save_and_the_join_stay_within_their_budget` | bytes per world object: a hot update's snapshot, the save text, the join | 230, 75, 17.6 | 300, 100, 24 |
| `bench::budget::the_stand_of_a_hot_update_stays_within_its_budget` | user instructions per world object of a snapshot and its restore, all threads | 9,500 | 14,000 |
| `bench::budget::the_memory_of_the_world_stays_within_its_budget` | bytes malloc holds for a world object, a network object, a machine and an inventory (what each frees when it is dropped, counted by the test allocator), on the guard world with 1,600 buildings | 134, 839, 391, 262 | 175, 1,100, 510, 340 |
| `an_idle_server_wakes_up_within_its_budget` (`tests/hot_update.rs`) | wake-ups a second of an idle front and bank | 20 and 190 | 40 and 260 |

The tick budgets, per tick (the guard world has 7, 1,607 and 6,407 world objects):

| Case | Allocations a tick | Instructions a tick |
|---|---|---|
| nobody, no buildings | 0.5 (budget 2) | 2,900 (6,000) |
| 400 buildings (1,607 world objects) | 19.7 (30) | 20,300 (32,000) |
| 400 buildings, 4 players walking | 24.9 (38) | 24,500 (38,000) |
| 400 buildings, 4 players acting every 200 ms | 28.5 (45) | 28,900 (45,000) |
| 1,600 buildings (6,407 world objects), 4 players acting | 77.3 (120) | 83,000 (130,000) |

Before the logistics search went through the items left, the base of 1,600 buildings cost 332,000
instructions a tick with nobody in it (9 times the 400-building base for 4 times the buildings); it costs
68,000 (3.4 times).

When a budget fails the message says which and by how much. If the cost is the point of the change
(a feature that has to do more), raise the number in the test and here, in the same commit, and say
why in the message; if it is not, find it: the next section.

`make bench` and the tools below measure the real worlds; the budgets are for a world anyone can
build in a second.

## How to profile

Against numbers, not against the clock: a loaded host makes every time wrong by a factor of two or
more, and no instruction count wrong at all.

* **The tick on a save, in instructions.** `bench::bench` prints the user instructions per tick
  (when the kernel lets the process count: `perf_event_paranoid` of 2 or less) with the fingerprint
  and allocations per tick; `tools/perf/bench.py --instructions` runs several cases and compares two
  builds. `PCDS_BENCH_SECTIONS=1` adds the time per subsystem and, for the slowest ticks, which
  section they were in (the 1 Hz searches show there first). `PCDS_BENCH_ALLOC_SITES=N` names, with
  call paths, the places in the server's code that allocate in the tick (every Nth allocation):
  the way to take a tick's allocations from 140 to 70.
* **Where the time goes.** Build with symbols and frame pointers in a target folder of your own,
  `CARGO_PROFILE_RELEASE_DEBUG=1 RUSTFLAGS="-C force-frame-pointers=yes" cargo test --release --no-run`
  (and the same for `cargo build --release` when you want the real processes), then
  `perf record --call-graph fp -e cycles:u` on the bench, or `tools/perf/profile.py --during
  load|idle|save --instructions` on a running server (the front and the bank are recorded apart);
  read them with `tools/perf/pstack.py DATA --top 30` (self, inclusive, callers, callees).
  Measure on the world that has the problem: the late save made a profile that the bench world
  never would (the logistics search, the machines' walk and the scans of every world object were
  invisible on 2,700 objects).
* **A running server.** `tools/perf/loadrun.py --perf` (idle and load: CPU, instructions, context
  switches, memory, round trips, the wire); `tools/perf/memory.py`, `tools/perf/scale.py` (start,
  save and memory on growing worlds), `tools/perf/flipbench.sh [--live]`, `tools/perf/startbench.sh`.
  A save on a DLC planet needs the rule: `PCDS_TOOLS_RULES='{"world":{"dlcPlanets":["Humble"]}}'`.
* **Allocations.** The bench counts them per tick; `bench::scale` prints the heap by part.

## Reading the tick histogram

`GET /api/status` (the dashboard shows it):

| Field | Meaning |
|---|---|
| `tick.p50Micros`, `p99Micros`, `maxMicros` | the game's tick over the last 60 seconds (`windowSeconds`, `samples` ticks: one every 5 ms), in microseconds. The tick is the simulation: machines, logistics, units, the RPCs that arrived, the messages built. |
| `busyPercent` | the share of the last second the bank's loop was working (the tick, and delivering what it made), not waiting for the next frame. |
| `longestLoopMicros` | the longest single pass of the loop in the last second: what a player could feel as a hitch. Above 50,000 (10 frames) is worth a look. |
| `fps` | loops in the last second; 190 to 200 is a bank that keeps its 5 ms frame. Less means the host (or a save, or a flip) held it. |

How to read them. **p50 is the cost of an ordinary tick** (a few microseconds on a small world, tens
on a late one) and says little. **p99 and max are the searches that run once a second** (the drones'
tasks, the energy check every two seconds) **and the autosave's text** (milliseconds on a big
world): on the late save they were 1.2 ms and 29 ms before this release and are 1.0 ms and 7 ms
now. Compare p99 with `busyPercent`: a p99 of 3 ms with a busy percent of 2 is a 1 Hz spike, harmless;
a rising p50 with a rising busy percent is a world that has grown into its machine. `longestLoopMicros`
beside a save or a flip is expected (the flip is the world standing still, and is reported by the
update itself: `stoodMs`).

## Sizing

### Cores

The bank is one thread that does the world; the front is another that moves bytes (with a few
threads of the admin page, the chat hooks and the plugin workers that are asleep). A flip uses
up to 8 threads for a few tens of milliseconds. So **two cores** run any world and any number of
players this server allows (100); a third makes a flip faster. The costs, per core, from the tables:

| Players (late save, 33k objects) | front | bank |
|---|---|---|
| 0 | 0.05% | 2% |
| 8 | 2.2% | 3.1% |
| 32 | 4.9% | 5.4% |
| 64 | 8.1% | 7.9% |

(user plus system, as a share of one core, a loaded host's; 0.8.0 had a bank of 6.4%, 8.6% and 11.8% at 8, 32 and 64 players, and 6% idle.)

### Memory

The bank holds the game's tables (8 MiB) and the world: **about 0.2 to 0.4 KiB per world object** (the smaller
figure on the bigger worlds, where the items in inventories are most of the objects) and a few KB a player. The
front is 9 MiB, nearly all of it the program's own file pages. A hot update holds two banks and the snapshot at
once. **Size a container at three times the bank plus the front** (the front refuses a hot update that would not
fit and says how much to give):

| World | Bank at rest | The start's peak | A hot update's peak (old bank, new bank at its peak, snapshot, front: measured) | Container to give | Without hot updates |
|---|---|---|---|---|---|
| 2,700 objects | 21 MiB | 25 MiB | 57 MiB | 128 MiB | 64 MiB |
| 33,000 | 33 MiB | 35 MiB | 74 MiB | 256 MiB | 64 MiB |
| 100,000 | 43 MiB | 52 MiB | 114 MiB | 256 MiB | 128 MiB |
| 200,000 | 66 MiB | 87 MiB | 179 MiB | 512 MiB | 128 MiB |

A flip holds the old bank, the new bank (which peaks above where it ends by about the snapshot, which it has mapped while
it restores) and the snapshot (12 MiB at 100,000 objects, 23 at 200,000): 2.3 to 2.7 times the bank, with the front.
The snapshot did not shrink with the bank (it is the same bytes as in 0.8.0), so the factor is no smaller than it was
(2.2 times a bank that was three times as big). The start peaks 1.2 to 1.3 times above the bank at rest. Page cache
(the program, the game's tables, the save) counts against a container's limit but is given back when it is needed.
The server refuses a hot update that would not fit and says how much to give (docs/docker.md); a restart needs
no more than the start.

### Bandwidth

What a server sends is the other players' movement to every player, so it grows with the square of the
players: per player and second, **10 KB at 8 players, 28 at 32, 52 at 64** on the wire (the game
does the same; the batches inside are 7.5, 25 and 49 KB). A whole server at 64 players sends 3.3 MB/s
(26 Mbit/s) and receives 0.2 MB/s. The join sends the world once, **about 12 bytes a world object**: 0.27 MB
for the late save, 1.2 MB for 100,000 objects, 2.4 MB for 200,000.

### Recommended machines

| Players | World | Machine |
|---|---|---|
| up to 8 | any up to 50,000 objects | 2 vCPU, 1 GiB (2 GiB with hot updates on the late save) |
| up to 32 | up to 100,000 objects | 2 vCPU, 2 GiB, 100 Mbit/s |
| up to 64 | up to 100,000 objects | 2 to 4 vCPU, 2 GiB, 100 Mbit/s |
| up to 100 | up to 200,000 objects | 4 vCPU, 4 GiB, 1 Gbit/s |

The cores are for the bank's tick and the kernel's UDP work, which together stay under half of one
core at 64 players on the late save; the rest is headroom for a save, a flip and the plugins.

## Limits of the transport

The game's UnityTransport is configured (read from the game's files by `pcds read-game`'s reader) with a
maximum payload of 6,144 bytes (`m_MaxPayloadSize`), a packet queue of 128, a heartbeat of 500 ms, 10
connection attempts and a disconnect timeout of 30 s. As Unity Transport documents the first, it limits one
send on its unreliable pipelines. The server sends everything on the **reliable pipeline**, a byte
stream with a length in front of every batch (utp.rs); the join is
one batch of about 12 bytes a world object and the server fragments nothing of it. Nothing in the
transport's code bounds it on the sending side; whether the real client reads a 2.4 MB batch was not
tried (no game client could be run), the 33k-object world's 266 KiB is what players have joined with.
What bounds a join in this server:

* the server's own cap on a batch it **receives** is 1 MiB (a hostile client), and the `pcds load`
  and `pcds probe` bots, which are clients, take up to 256 MiB;
* the pipe between the front and the bank carries a frame of up to 64 MiB: a join of more than
  5 million world objects is refused and the client told so;
* a client that is not yet a player may be sent 8 MiB more than its world;
* over **Steam** the game's transport takes a message of up to 5 chunks of 512 KiB; the server
  splits a join into several batches between messages, which holds until one message (the
  ConnectionApproved with every object) is over 2.5 MiB, about 215,000 world objects. A larger
  world says in the log that it can be joined over direct IP only.

A 200,000-object world (a join of 2.4 MB) was joined by `pcds load` players on a live server and
played for 25 seconds; the real client could not be run.

## Tried and not kept

Each was measured before it was left out.

* **The save text built on several threads.** Same bytes, and the game thread stood 6.2 ms against 6.6 ms
  on the late save (33,000 objects), 33 against 20 ms at 100,000, and on a loaded host 53 against 231
  ms at 200,000 (the threads wait to be scheduled while the game thread waits for them), with half
  again the CPU. A save happens every five minutes; a thread that is not scheduled holds the world.
* **Acknowledging with the next batch.** A third of the datagrams out and 2 to 3% of the bytes are
  acknowledgements of the client's batches; folding them into the movement batches 16 ms later would
  save a fifth of the datagrams the server sends. The client times its resends from them and no real
  game client could be run against it, so the server keeps acknowledging at the next frame.
* **Larger datagrams.** A datagram carries 1,200 bytes of the stream; 1,400 would save 1% of the
  datagrams of a join and none of a movement batch (they are 1.2 to 2 KB), against the game's own limit
  of 1,400.
* **A two-level array of due times for the machines' walk** (a minimum for each run of 128): the
  instructions did not change (28.2 and 28.1 M a second on the late save), so it was taken out.
* **Fewer lookups of a unit's multipliers** (a table per group in `units::from_world_objects`): 107.7
  against 107.2 M a second with 100 players building on the late save. Taken out.
* **Slower ticks while nobody is in the world.** The world's timers are set from the tick that fires
  them: a 20 ms tick instead of 5 changes what the world is a minute later. `world.pauseWhenEmpty` is the
  switch for a world that should wait.
* **A different allocator.** glibc's malloc against jemalloc and tcmalloc (`LD_PRELOAD`, the same build, 200,000
  objects, RSS five seconds after the start and the bank's peak): glibc 72 / 97 MiB, glibc with the mmap threshold
  fixed (what the server does now) 66 / 86, jemalloc 92 / 99 (the front 23), tcmalloc 95 / 95 (the front 15). None is in the tree, they are C libraries, and
  none beats the setting of one number. `MALLOC_ARENA_MAX=1` changed nothing the threshold had not.
* **Madvise on the program's own pages.** 8.3 of the front's 9 MiB (and 10 of the bank's) are the program's file
  pages, which the kernel maps around every fault and keeps in the page cache for both processes; dropping them
  from the process (`MADV_DONTNEED` on the text) would lower the number `ps` shows and not what the container
  holds. Not done.
* **Tables shared by the two banks of a hot update** through a mapped, packed file: 8 MiB a bank now (26 before),
  3% of a flip at 200,000 objects. Not built.
* **Another encoding of the snapshot** (the names of each network object once, 24 -> 17 MiB at 200,000 objects):
  a new snapshot format, for 4% of a hot update's peak. The snapshot is the same bytes as in 0.8.0.
* **Compact inventories** (the four lists and the logistics fields only where an inventory has them: 144 -> 48
  bytes, 2 MiB at 200,000 objects, 3%) and **24-byte variable values** (1 MiB): not done; they are the next
  memory after the 16 MiB of world objects and the 15.5 of network objects below.
* **Pre-copy for the hot update, a persistent thread pool and a pre-faulted heap** for the new bank:
  evaluated in docs/hot-update.md ("Flip speed"); with the stand now 30 to 160 ms the case is weaker
  and none was built.
