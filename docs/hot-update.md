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
that only the release that wrote it reads (no migration: a release that changes the format is
installed with `pcds upgrade --restart`, which disconnects every player).

## As built

```
 run/bin/pcds      what the systemd units run: it hands every command to the active bank's
                   program (exec), so `pcds run` is the active bank's: it starts that
                   bank's `pcds serve`, the front, with PCDS_HOME=run (its bin/ holds the banks), as a child, and
                   supervises it (signals passed on, orphaned banks waited for, the
                   other bank's front when this one cannot start the world)
 run/bin/fallback  the last start that fell back to the other bank, and why (pcds status)
 run/bin/a/, b/    the banks: pcds, libsteam_api.so, web/, and bank.json (version, front
                   protocol, snapshot formats, the release tag and its sha256)
 run/bin/active    "a" or "b": the bank the front starts (written beside, renamed over)
```

**Modules.** `front.rs` (the front), `bank.rs` (the game process), `ipc.rs` (frames, the
non-blocking queue, SCM_RIGHTS, the memfd), `updater.rs` (releases into a bank),
`cli/upgrade.rs` (`pcds upgrade`, the banks in `pcds status`). `split.rs` is gone. The state
module of (e) is not split out: the state types stay where they are, and `handoff.rs` holds the
snapshot format and `verify`; `bincodec.rs` is the codec. `main.rs` dispatches.

**The front** owns the UDP socket and the utp.rs sessions, the Steam game server login, its
listen socket, every `NetConnection` with its `Split<n>` reassembly, the invite code, the client
ids, the 300 ms delayed close of kicked clients, the admin HTTP listener (it serves the live
bank's `web/index.html`), the chat hooks (`chathooks::Hooks`: the event ring, the webhook and
exec worker, `/api/chat`), and the autosave timer (it sends SAVE; the bank saves only if the
world advanced). Admin requests other than `/api/stop`, `/api/update` and `/api/world/*` (stop,
start, switch: the front owns the world's lifetime) go to the bank as ADMIN frames with the
autosave setting; while the world is stopped the front answers `GET /api/status` and `GET
/api/worlds` itself and refuses the rest. The answer comes back as ANSWER, and the front adds its part to
`/api/status` (invite code, network, uptime, traffic, CPU and memory of front and banks,
`banks`, `update`). Writes to a bank never block: frames wait in a queue per bank, and a client
with more than 16 MiB waiting for the game (held during a flip, or a stalled bank) is dropped.
The bank writes blocking: a front that stopped reading stalls the world, which loses nothing.
The frames queued together go out in one write (writev; a frame with a descriptor alone). The
front loops every 5 ms; with nobody connected and no flip under way it loops every 20 ms (a
datagram or a bank frame wakes it at once; an admin request, a chat reply, a stop signal and
Steam's callbacks wait for the frame).

**The bank** is the server loop's game half: it builds and parses batches, decides what goes
out when (the immediate answers, the 16 ms movement batches), kicks (the reason as DATA, then
KICK; the front closes the connection 300 ms later and says GONE), saves, and keeps the panic
guard. It ignores SIGINT and SIGTERM: the front tells it to save and stop (STOP), and a bank
whose front is gone saves and ends. The autosave (SAVE) makes its snapshot in the loop and writes the file, the backup and the S3 copy on a thread of its own (the fsyncs are hundreds of milliseconds to seconds on a busy disk); every other save, and the stop, wait for that write first. Chat events go to the front as CHAT frames; the chat rules
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
checked (front protocol, game tables, and that it is in the running world's snapshot format;
a refusal names `pcds upgrade --restart VERSION` and says a restart disconnects every player,
Steam players with a new invite code unless PCDS_STEAM_TOKEN is set); HANDOFF; the old bank
sends what is queued, snapshots and waits; RESTORE; the new bank restores and says READY within
5 s (the health check); the front sends EXIT, gives the new bank the held frames and writes
`run/bin/active`. A refusal at the HELLO (protocol, tables, snapshot format) kills the new
bank and sends nothing: the old bank never stopped. Any failure after HANDOFF and before
READY kills the new bank and sends RESUME to the old one. With
`PCDS_DEV=flip-verify` the new bank also checks that its restored state, written and read back, is
the snapshot (without the two clocks and the order of sets); the tests run with it.

**A crash** (the live bank ends outside a flip): the front starts the same bank again from the
last snapshot when it is newer than the save, else from the save (either way `access.json` and
`rules.json` are read again from disk, so a ban after the last flip stays), tells its players "The server
restarted after an error: join again" and closes their connections; the Steam login and the
invite code stay. A snapshot restored this way drops every client in it. More than 5 crashes in
10 minutes and the front exits (systemd's restart and start limit take over). At the first
start, a bank that cannot load the world makes the front try the other bank once (a broken
update falls back to the release that ran before), and writes `run/bin/fallback`. Every HELLO,
at boot, flip or recovery, is checked the same way; a recovering bank of another protocol stops
the front (exit 1). When no bank can start the world (the other one is of another protocol,
after `pcds upgrade --restart`), the front exits 75; `pcds run` then starts the other bank's own
front, makes it the active one and writes `run/bin/fallback`, unless that bank is empty or the
one the owner rolled back from. When the
front stops while no bank holds the world (a stop during a recovery, the crash limit), the last
snapshot, if newer than the save, is written to `world.json.snapshot` and the next start
restores from it; a start whose save is newer deletes it. One the bank can never run (another
game's tables after a game update, a snapshot format it does not read, not a snapshot) is moved
to `world.json.snapshot.unusable-<UTC time>` with a line in the log, and the world starts from
the save. If a start cannot restore it for another reason, the start fails: move it away. A save that fails is never silent: `POST /api/save` answers 500 with the error, `GET
/api/status` has `lastSave` (`ok`, `at` in Unix seconds, `error`), and a failed autosave is tried
again at the next one. An autosave makes its text on the game thread (about 1 ms for a world of 2500 objects, 36 ms for 100k) and a thread of the bank backs up and writes the file, so the disk never holds the world still; `lastSave` is set when the write ends. One autosave is written at a time, and the saves that wait for the result (`POST /api/save`, the one at a stop, a flip's exit) wait for it first, so a save never overtakes an older one. When the last save (at a stop, or when the front is gone) fails, the bank
writes its snapshot to `world.json.snapshot` (else to the temporary folder, and says where) and
ends with 1; the front says so and ends with 1 too.

**The updater** (`updater.rs`, in a thread of the front, or in the CLI when the server is
stopped): the release from `PCDS_RELEASES` (default GitHub's API for
perara/planet-crafter-dedicated-server), `pcds-<tag>-linux-x86_64.tar.gz` and its `.sha256` as
the release workflow publishes them; the tarball's SHA-256 (ring) against the published one;
`tar -xzf --strip-components=1` into `run/bin/.<bank>.new.<pid>`; `pcds bank-info` of the
unpacked program (front protocol, snapshot format and tables format; a program that does not
answer is refused); the compatibility check; `bank.json`; then the folder is renamed over the bank. Any
failure leaves the bank as it was (a failed rename puts the old bank back). Downloads are https
only (http only for a `PCDS_RELEASES` on 127.0.0.1, which the tests use); versions and tags
must match `v?[0-9A-Za-z._-]+`; the unpacked tree may hold only plain files and folders;
`bank-info` runs with only PATH, HOME and LANG in its environment. The sha256 guards against a
damaged download, not a forged release: releases are not signed. Renames into the banks and
writes of `active` take a lock on `run/bin/.lock`. A later release is later than the live bank's
release: its tag (bank.json) when the bank came from a release, else the program's version (a
build, or a release folder's program), so a release whose program forgot to bump its version is
installed once, not at every check. The rule `update.auto` checks every `update.checkHours`
(default 24, the first check 10 minutes after the start; a change of either rule counts at
once; a check that failed is tried again after an hour, one that found the world busy or
stopped after 10 minutes) and installs and flips to a release later than the live bank's; it is off by default, and with it off the
server never contacts GitHub on its own. A rollback writes `run/bin/rolled-back` (the bank it
left and that bank's tag); the automatic check skips that tag, and a later release, an explicit
update or the next rollback clears or replaces the note.

**Rollback.** The other bank's release takes the world only if it passes the same check as an
update (front protocol, tables format, and that it is in the running world's snapshot format).
Across a change of any of them the way back is `pcds upgrade --rollback --restart` (the server
saves as it stops and the other release loads that save; every player joins again): `--rollback`
alone answers with that, changes nothing, and the dashboard says so in the Updates section and
when Roll back is clicked. While the world is stopped (`POST /api/world/stop`) no bank runs it:
an update or a rollback is answered 409 "the world is stopped" (start it first, or use
`--restart`), and the dashboard's buttons are off.

**State and the next release.** A snapshot is the whole serialized `Game`. The release that
restores it is another program than the one that wrote it. There is no legacy support: a
snapshot is read in full by the format it was written in, or refused. The rules:
- Any change to what is serialized (a field added, removed, renamed or retyped, a different
  encoding) bumps `handoff::FORMAT`. There are no `#[serde(default)]`s to keep an older snapshot
  readable, no `migrate` and no list of older formats: `format` in `pcds bank-info` (and
  bank.json) is the one format a release writes and reads, an exact match. The front checks it before a flip
  (`updater::compatible`), so a release of another format is a restart install
  (`pcds upgrade --restart`; the world saves as the server stops and the new release loads the
  save), and so is the way back across it.
- What the tables give is not state. Do not store it, or store it and set it again after a
  restore: `machines::refresh_from_tables` (called by `handoff::restore_bytes`) sets again every
  table value a machine part keeps beside its state (Harvestable, Vegetation, LinkedGrower,
  AutoCrafter, Disintegrator, Optimizer). A part that mixes both and is not refreshed
  (Generator: spawn_every, speed, groups) must be added there when its table values are read
  differently. State (timers, what is growing, a random sequence, what the rays found) stays
  the snapshot's.
- `rules` are in the snapshot and are `deny_unknown_fields`: a new rule is a change of state,
  so it bumps `FORMAT` like any other.

**The flip gate.** Every flip in `tests/hot_update.rs` is between banks of this build. The one
thing they cannot show is a flip from the program of the last release, which is another binary.
`make gate` (`tools/flip-gate.sh`) fetches the last published release that is not this
version and compares its `bank-info` with this build's (`format`, `protocol`, `tablesFormat`).
When they differ it prints "format changed: ... is not hot-update compatible" between rows of
`#` and exits 0 without testing: that release is a restart install by design (the release notes
say so). When they are equal that release runs a front and a bank on a world with one of every
building the game has and 6 players, and this build's bank takes the world over live
(`hot_update::a_hot_update_from_the_release_before_works`, ignored by default because it needs
the download). It fails when the flip fails after the front accepted it, a player drops, a
building is lost, the world does not save from the new bank, or the way back (a rollback of the
update just made) does not work live. It needs the game's tables (`PCDS_TEST_DATA`), which no
CI has: it is part of the release checklist below, not a CI job.

**Release checklist** (by hand, before tagging):
1. `server/Cargo.toml`'s version is the tag's (the release workflow checks).
2. If the serialized state, the front protocol (`ipc::PROTOCOL`) or the tables format changed,
   `FORMAT`, `PROTOCOL` or `TABLES_FORMAT` is bumped, and the release notes say the update is
   `pcds upgrade --restart`.
3. `make test` with game tables, and `make docker-test`.
4. `make gate`. Read its output: "SKIPPED - format changed" is expected after a bump in step 2
   and nothing else; anything else must pass.

**The CLI.** `pcds upgrade [VERSION] [--restart] | --rollback | --check`. Running: through the
admin API (`POST /api/update`, then `GET /api/update` until its outcome). `--restart` installs
into the standby bank without the protocol check, makes it active and restarts the unit (the
path for a release that changes the front protocol); in a container, which has no unit, the
supervisor (`pcds container`) restarts the server in place on SIGUSR1 (docs/docker.md).
Before it downloads or flips, the front refuses an update that does not fit in the container's
cgroup v2 memory limit (`dashboard::flip_memory`). Stopped: the release becomes the active
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
Roll back; the Config tab sets `update.auto` and
`update.checkHours` (rules; the front gets them from the bank in an `OPS` frame).

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

With `PCDS_DEV=flip-verify` on the same world: all 10 restored states equal to their snapshots; the
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

### Flip speed

The numbers above are the spike's. Since then the stand was measured on grown worlds and cut
(branch `flip-speed`). Worlds: `tools/grow-world.py SAVE OUT N` copies the bench world's world
objects, inventories and links (ids renumbered) up to N objects; the four used here have 2.6k,
10.3k, 30.7k and 99.5k world objects (the 2.6k one is the bench world itself, 410 network
objects; the 99.5k one has 14.9k network objects, 10.3k inventories). 8 players. This host is
shared (load average 85 to 120 during all of these), so p50 moves by a factor of 2 between runs;
the min column and the single-thread runs are the steadier ones.

**Harnesses.** `bench::flipbench` (in-process: the old bank snapshots into a memfd, the
descriptor crosses a socket pair as the front passes it on, the new bank maps it and restores;
per phase p50 / p95 / max / min, plus the CPU time of each kind of part):
`PCDS_FLIPBENCH_SAVES=a.json,b.json [PCDS_FLIPBENCH_N=15] [PCDS_DEV=flip-threads=1] cargo test
--release --bin pcds bench::flipbench -- --ignored --nocapture`. `hot_update::flipbench` (the
real thing: front, two banks, 8 `pcds load` players, sockets; the front's timeline of the flip on
one monotonic clock: HANDOFF to the old bank, its snapshot, the SNAPSHOT to the front, the relay,
the RESTORE to the new bank, its restore, the READY, the held frames out): `cargo test --release
--test hot_update flipbench -- --ignored --nocapture`, same variables. `bench::dirtybench` counts what changes while a
pre-copy would run. The state check is off in all of them.

**The stand before and after**, ms (v0.6.2 is the code this branch was cut from):

| world objects | in-process before (p50) | in-process after (p50 / min) | live before (p50 / min) | live after (p50 / min) |
|---|---|---|---|---|
| 2.6k | 12.9 | 4.6 / 2.8 | 180 / 61 | 8.3 / 6.0 |
| 10.3k | 39.7 | 10.6 / 6.8 | 91 / 79 | 17.3 / 12.9 |
| 30.7k | 115 | 20.1 / 15.2 | 312 / 225 | 47.4 / 18.2 |
| 99.5k | 350 | 52 / 34 | 841 / 573 | 130 / 79 (90 / 69 in a calmer run; 40.5 for one flip with no players) |

In-process one thread, 99.5k: 72.7 (min 68.5); 2.6k: 3.8. Live at 99.5k by threads
(`PCDS_DEV=flip-threads=N`) under load: 1: 190, 2: 155, 4: 89, 8: 96. Snapshot 2.18 / 8.0 / 23.5 / 75.9 MB
(JSON) now 0.46 / 1.4 / 4.1 / 12.9 MB.

**What cost what.** (1) The live stand was dominated by something that was not the snapshot:
`promote` wrote `run/bin/active` (an fsync) after queueing the held frames and before the loop's
flush, so on a busy disk the held frames waited about 100 ms for a file write, and the front
served no client meanwhile (2.6k world: 126 ms stand, 18 ms of it snapshot and restore). The held
frames now go first and the banks-folder writes run in a thread (joined at shutdown; the flip is
recorded once they are on disk). (2) The profile of the JSON snapshot at 30k objects (perf, frame
pointers): 61% of the write and 68% of the read were world objects, 28% / 20% network objects,
6% machines; the time was serde_json itself, names written and matched for every struct, numbers
as digits, `Vec<u8>` as arrays of numbers; nothing in the state was slow by itself
(`rebuild_holders` 0.8 ms and `change_generation_for_groups` 0.01 ms at 99.5k). (3) The binary
codec, in parts on threads (format 5, above): CPU ms at 99.5k, write / read: world objects 7.2 /
15.9, network objects 7.4 / 15.2, machines 1.7 / 4.1, inventories 0.9 / 1.8, rest 0.4 / 1.4;
then the memfd write 4.1, the slot indexes 4.0 and the network index 0.9 in the read, all on
one thread. What is left is the network objects' strings (class, variable name and type, three
allocations per variable; a layout-based restore or interned names would halve their read), the
first touch of the heap in a new process (16k page faults at 99.5k, about 8 ms of CPU spread over
the threads) and 5 wake-ups (0.02 to 0.1 ms each when the host is not saturated).

**Not done, and why.** A parallel memfd write through a shared mapping was tried and left out: no
gain over write(2) that the noise could show. Moving the held-frame release and the READY off the
critical path: the release is 0.02 ms and the READY to the front 0.05 ms in the timeline; there
is nothing to take. A persistent thread pool and a pre-faulted heap in the new bank: not built
(the restore takes 347 page faults at 2.6k and 8k to 15k at 99.5k; not measured what pre-faulting
saves).

**Pre-copy** (the old bank streams a full snapshot while it runs, only the changes follow at the
stand) was evaluated, not built. `bench::dirtybench`, 8 players acting every 200 ms: in 0.25 s 1
of 99549 world objects, 3 of 10312 inventories and 11 of 14852 network objects differ (1 of 49,
1 of 6, 2 of 8 chunks); in 5 s on the 2.6k world 25 of 2640 world objects, 8 of 291 inventories,
14 of 427 network objects (every one of them in one chunk of 2048). So the world objects, the
inventories and the network objects (92% of the bytes and 94% of the CPU) would leave the stand
almost entirely: what is left in the stand at 99.5k is the machines, the rest and the water (CPU
2.6 ms to write and 6.3 ms to read), the dirty chunks and the wake-ups. It needs dirty tracking in `Slots` (its mutators are get_mut, values_mut,
insert, remove, retain: feasible) and in the network objects' map, two new frames, an assembly of
the new bank's state from a base and a delta that must give the same slot order and free lists,
twice the memory for the transfer, and a failure path (old bank resumes while the new one holds
half a world). Not worth it at 99.5k: the stand is 40 to 90 ms; it would be at several hundred
thousand objects, or if the stand must stay under 20 ms at 100k.

**Around the stand** (not part of it): downloading v0.6.2 from GitHub 1.2 s (4.6 MB), its sha256
0.01 s, unpacking 0.1 s, `bank-info` 0.01 s. The new bank is up and says HELLO 69 ms after the
front spawns it (tables and scenes: 66 MiB resident; the live bank with the 99.5k world holds
181 MiB). The 120 s load deadline is not near. A standby bank kept ready would save those 70 ms
of an update that takes seconds and cost 66 MiB all the time: not worth it.

### Not verified

- **Steam.** Steam connections, their `Split<n>` handling and the invite code live in the
  front, by the same code the single process had (moved, not changed), and pass through the
  same paths as direct-IP clients (CONNECTED with the verified SteamID, DATA, GONE). No test
  runs Steam: every run above had `PCDS_STEAM=off`. That a Steam player stays connected through a
  flip, and that the invite code survives a bank crash, follows from the design and is not
  measured.
- **A real GitHub release**: run since (0.6.0 to 0.6.1 and 0.6.2 through `pcds upgrade`, the
  admin API and the rule `update.auto`, with load players; GitHub's redirect to the asset storage
  works). What is not driven: GitHub's rate limit (a 403 or 429 is an error with its status, tried
  again within the hour) and a mirror that serves a release without `.sha256`.
- **Across releases.** The first real update (0.6.0 to 0.6.1) failed on a snapshot field the
  release before did not write. Since then there is no cross-release reading at all: a flip is
  between releases of one snapshot format (anything else is a restart install), and `make gate`
  flips the last release of the same format into the build before every tag (see "The flip
  gate" above).
- **Front restarts.** Updating the front is a restart (`pcds upgrade --restart`; in a container
  also in place, see docs/docker.md): every player reconnects. Keeping the UDP socket and the
  sessions across it is not built; the design note below ("Updating the front without dropping
  players") lists what it would take.
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
 pcds upgrade, updater ─┘  health check, flip back          └── game process from bank B (during a flip)

 run/bin/a/  run/bin/b/   each a whole unpacked release, checked against its sha256
 run/bin/active           which bank the front starts (a file holding "a" or "b", renamed over)
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

**Banks and the updater.** `updater.rs` (a thread of the front, or `pcds upgrade` when the server
is stopped; see "The updater" under As built): reads the latest release from the GitHub API,
downloads `pcds-<tag>-linux-x86_64.tar.gz` and its `.sha256`, checks the tarball's SHA-256,
unpacks it into the inactive bank, and the front is asked to flip (`POST /api/update`). Only
after the new bank has confirmed does the front write `run/bin/active` (a new file renamed
over the old one), so a restart of the whole unit starts the bank that last ran well. The old
bank stays installed: flipping back needs no download. The release stays one tarball with one
binary (`pcds serve`, the front; `pcds bank`, the game process; the tools).

**Supervisor.** The front, the systemd unit's process; the game process is its child (same
cgroup, so stopping the unit stops both). A game process that dies outside a flip cannot be
resumed (the clients have seen newer state than any snapshot), so the front starts the active
bank from the save and tells the clients to reconnect: today's crash behaviour, except that
the Steam login and the invite code survive it.

## (b) The boundary between front and game

One unix stream socket per game process, carrying frames: `u32 length` (of what follows),
`u8 kind`, `u64 n` (the client id, or a request or flip number), body. The spike's frames; the
built ones, with what each carries, are listed in `ipc.rs`:

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
above (built in `ipc.rs`).

**Latency budget.** Today a client's own packet is answered at once (the loop wakes on the
socket). With the split the path gains two hops: the front wakes and writes a frame, the game
wakes and answers, the front wakes again. Measured between two processes
(the spike's `split::tests::ipc`, 120-byte frames, two runs; now `ipc::tests::ipc`):

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
`PCDS_HOME` (where `rules.json` and `access.json` are), `PCDS_GAME_DIR` (the status) and
`PCDS_SESSION`; the front passes its environment to every bank. Everything else a bank
does is a rule, read from `game.rules`. The server loop in `main.rs` holds the batches
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

A snapshot is a head line `{"format":5,"game_version":"2.103"}` and the state in the binary
codec of `bincodec.rs` (serde's data model with no names and no tags: a struct is its fields back
to back; integers of two bytes and more, lengths and variants are LEB128, zigzag when signed;
floats and bytes are fixed width; a float array that is all zeros is one byte; a `Value` is its
JSON text; the types are read as the types they were written as, so it is exact, and NaN and
infinity keep their bits, which JSON could not carry). The body is a directory (`u32 n`, then
`n * (u8 kind, u32 entries, u64 length)`) and the parts back to back: REST (the game with its big
collections left empty), MACHINES, WATER, and chunks of 2048 entries of the world objects, the
inventories and the network objects, in order. The parts are independent: they are encoded and
decoded on up to 8 threads (`PCDS_DEV=flip-threads=N`), and the chunks of the world objects and
inventories are read straight into their final `Vec`. Rust layouts are not stable across builds,
so state crosses a flip only serialized, never as shared memory. `restore` (`handoff.rs`):

- refuses a snapshot made on other game tables (state holds indices into the tables);
- reads `FORMAT` and nothing else: there is no `migrate` and there are no older formats. A
  release that changes the state's shape or the encoding bumps `FORMAT`; its bank-info says
  `format: FORMAT` (it reads that one only), so the front refuses a hot flip between releases of different formats and
  says to use `pcds upgrade --restart VERSION` (the world then loads from the save); a rollback
  across a format bump is refused the same way. A snapshot kept on disk by an older release is
  moved aside (`world.json.snapshot.unusable-*`) and the world starts from the save;
- needs a field added to the state to be added in the format as well: there are no defaults, a
  snapshot is read in full or refused. `tests::a_snapshot_restores_only_in_its_format_and_on_its_tables`
  checks the refusals.

`verify` (`PCDS_DEV=flip-verify`, tests) reads the snapshot again as a game, writes it and the restored
game as JSON and compares them without the clocks and the order of sets.

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
(`PCDS_SPLIT_FAIL` then, `PCDS_DEV=bank-fail=...` now) resumed the old bank after 8 to 14 ms. No player
was dropped. Of the live pause, about 12 ms is the snapshot crossing the socket twice (4 MiB there and back measured
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
- **Snapshot format and tables.** As above: `FORMAT` in the head, bank-info's `format` an exact match,
  refusal otherwise; the game version in HELLO and in the head (the refactor should add
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

## Updating the front without dropping players (a design note)

Status: analysis only, nothing of it is built. Today the front is updated by a restart
(`pcds upgrade --restart`, in a container also in place: the server stops, saves and starts
again as the same process), and every player reconnects. The idea is nginx's binary upgrade: the
running front starts the new program and gives it what a client notices, so direct-IP players see
a pause of a few hundred milliseconds. The owner decides whether it is worth it; what follows
is what it would take, read from `front.rs` and `utp.rs`.

**What the front holds, and what happens to each at the handover**

| held by the front | handover |
|---|---|
| the UDP socket (`Utp.socket`) | inherited as a file descriptor (CLOEXEC cleared, its number given to the new program). The kernel keeps the datagrams that arrive in the gap in the socket's buffer, and clients resend every 200 ms, so a gap of a few hundred milliseconds loses nothing |
| the admin TCP listener (`admin::listen`) | inherited the same way; requests in flight are answered first, or fail with 503 |
| the bank's unix socket (`Bank.link`, one per bank; two during a flip) and what is queued on it either way | the front stops reading at a frame boundary, flushes what it has queued, and passes the socket; the new front tells the bank to announce itself again (a new ATTACH frame answered by HELLO). A flip, a recovery and an update under way (`Phase` not `Idle`, `updates.busy`) refuse the handover |
| the bank processes themselves | children of the front. With an exec in place the PID stays, so they stay its children and the supervisor (`pcds container` or `pcds run`, which waits for the front's PID) does not notice. With fork and exec the new front is a grandchild that the supervisor, already the subreaper, inherits, but it then waits for a front PID that has exited: it would have to learn the new PID |
| the Unity Transport sessions (`utp::Connection`: address, handshake time, `received`/`mask`/`next`, early packets, the reassembled stream, the send sequence, `in_flight` with send times, queued bytes, the two fragment buffers, window, backoff, closing) and `Utp.handshakes`, `outbox`, `dropped` | serialized as the game state is: plain data, `Instant`s written as ages, a versioned format with `migrate`. `outbox` is flushed first. A session is only as exact as the clock: the in-flight packets' resend times are re-based, so some resend at once, which is harmless |
| the front's tables: `conns` (client id to token), `ip_clients`, `next_client`, `pending_drop` (the 300 ms kicks), `kicked`, `held` (empty outside a flip), `crashes`, `invite_code` | serialized in the same record |
| `last_snapshot` and `recover_from` (memfds), the world's `flock` (`world_lock`) | the descriptors are inherited, so a crash right after the handover still recovers from the last snapshot and the world stays locked to this server |
| the chat hooks (`Hooks`: the event ring, the webhook and exec workers), the update history, the web page cache | the ring and history serialized or dropped (they are for the admin page); the workers restarted by the new front |
| **the Steam connections** (`Conn::Steam`: the game server login, the listen socket, every `NetConnection` with its `Split<n>` buffer) | **cannot survive**: they live inside the Steam API in the process that logged in and end with it. Steam players are told why and disconnected; the new front logs in again, which gives anonymous logins a new SteamID and invite code (`PCDS_STEAM_TOKEN` keeps it). Only a third process that owns Steam and never updates would keep them, which is a different design |

**Parts of the work**

1. Re-exec plumbing in the supervisor and the front: which descriptors are passed (a list, with
   CLOEXEC cleared on exactly those), how the new program is told it is an inheritor, a
   handover refused during a flip or recovery, and what the supervisor does about the PID (an
   exec in place keeps it; a fork needs the supervisor to follow). Also decides the failure
   story: with an exec in place there is nothing to fall back to if the new program cannot
   start, so the new program must be started first (fork), prove it is ready (it holds the
   sockets, restored its tables, re-attached the bank), and only then does the old one exit; the
   old one keeps serving until then.
2. A snapshot and restore of `utp.rs` (`Utp`, `Connection`, `Fragments`) with a format version
   and a test that runs a conversation across a handover with packet loss on both sides and
   compares the stream the client sees with one without a handover.
3. A snapshot and restore of the front's own tables (the record above), in the same format.
4. Taking over a live bank: quiescing the link at a frame boundary, the ATTACH/HELLO exchange,
   the bank's side of it (a bank that is running must answer a new front, which no bank does
   today), and the protocol rule for a new front that speaks a different front protocol than the
   running bank (today it cannot take over the world and says so). The new front would have to
   speak the previous protocol for one flip, as snapshots are read in the previous format, or a
   protocol change keeps needing the restart.
5. Steam: refuse the handover while Steam players are connected, or kick them with a reason,
   then log in again in the new front; a message to the admin saying what it did to them.
6. The handover command and its place in the CLI and admin page (`pcds upgrade --front`?), the
   container's and the systemd unit's stop budgets (a failed handover must still end in
   `docker stop` saving the world), and the documentation.
7. Tests in `tests/hot_update.rs` style: players through two front handovers, a new front that
   dies at start (the old keeps the players), a handover in the middle of a bank crash
   (refused), the admin API through a handover, and a Steam-off run of everything.

Not part of it: updating the bank (done), the Steam player's connection (impossible without
the third process), and anything that makes the front bigger: the smaller its state, the
shorter the list above, which is the reason to keep new features out of it.

## (e) What moves where in the refactor

Front (`front.rs`, new, from `main.rs`):
- `utp.rs` as it is. Its only crate dependency is `idmap::FxHashMap`; the spike's front uses it
  unchanged (one accessor for its socket was added, to wait on it with other descriptors).
- From `main.rs`: `start_steam`, `APP_ID`, `Link`, `reassemble`, `send` (Steam `Split<n>`),
  `CHUNK`, `MAX_SPLIT`, the poll part of the loop (Steam events and messages, `utp.poll`,
  client ids, `links`, `ip_clients`), `pending_drop`, the signal handler, the shutdown.
- `admin.rs` (the HTTP thread), forwarding requests to the game; the front fills
  `invite_code`, `network`, `port` and the per-player link type of `/api/status`.
- The supervisor (also in `front.rs`): banks, HELLO checks, HANDOFF / SNAPSHOT / RESTORE / READY /
  EXIT / RESUME, holding frames, the health check, the active pointer, restarting a crashed
  bank from the save.

Shared (`ipc.rs`, new): the frame format and kinds, `Link`, `wait`, `PROTOCOL`.

State module (planned as `state/`, not built: the types stay where they are and `handoff.rs` holds the format): the types in the table under (c), moved out of
the logic modules with their derives; `handoff.rs` (the head, `FORMAT`, `migrate`, `snapshot`,
`restore`, the encodings for clocks, infinities, tuple-keyed maps and static names); the
autosave countdown and setting from `main.rs`. The logic keeps `impl` blocks and functions
over these types, so moving a type is a change of its path, not of its code.

Logic (the game process; built as `bank.rs`, from `main.rs`, not `gameproc.rs`):
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

`main.rs` keeps only the subcommand dispatch. `pcds run` starts the active bank's `pcds serve`
(the front), which starts `pcds bank`.

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
cd server && cargo test --release --no-run
# the test binaries find libsteam_api.so in the steamworks crate's build output:
export LD_LIBRARY_PATH=$(dirname "$(ls -t target/release/build/steamworks-sys-*/out/libsteam_api.so | head -1)")
PCDS_BENCH_SAVE=<copy of world.json> PCDS_RNG_SEED=1 PCDS_BENCH_SECONDS=300 PCDS_BENCH_PLAYERS=4 \
  [PCDS_BENCH_ACTION_MS=200] [PCDS_BENCH_HANDOFF=<seconds between handoffs>] \
  target/release/deps/pcds-* bench::handoff --ignored --nocapture
target/release/deps/pcds-* ipc::tests::ipc --ignored --exact --nocapture
PCDS_TEST_DATA=<tables> target/release/deps/hot_update-* --nocapture   # the end-to-end test

# a server on a scratch save, flipped by hand (both banks are this build without PCDS_HOME):
PCDS_LISTEN=127.0.0.1:47777 PCDS_ADMIN=127.0.0.1:47778 PCDS_ADMIN_TOKEN=t PCDS_STEAM=off \
  target/release/pcds serve DATA_DIR scratch-world.json &
target/release/pcds load 127.0.0.1:47777 DATA_DIR 8 60 &
curl -X POST -H 'Authorization: Bearer t' -d '{"rollback": true}' http://127.0.0.1:47778/api/update
```
