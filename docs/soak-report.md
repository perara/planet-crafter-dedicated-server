# Soak report: the build that follows 0.7.0

*Round 2 (a soak with an hourly timetable, years of game time, resource pressure, Docker under a memory limit) is at the end of this page: [Round 2](#round-2-12-h).*

The release candidate (0.7.0 plus the consolidation and the DLC planets) was run hard before it was
called ready: three hours on a real late-game world, 800 chaos actions in both supervision modes,
the abuse tools, 64 and 128 simulated players, and the exact procedure the owner will use to update
a live server, rehearsed on a systemd user unit and in Docker. This page has the numbers, what
failed, what was fixed and what was not covered. The commands to repeat each part are in
[testing.md](testing.md) and [tools/README.md](../tools/README.md); the procedure itself is
[upgrading.md](upgrading.md).

Everything ran on scratch servers (their own folder, ports 8460 to 8479, units named `pcds-qa-*`),
on a copy of the data. The world is a late-game save of 33,358 world objects, 3,160 network
objects, 2,209 inventories on three planets (planet Humble), tables of format 4. The save is not
in the repository and nothing of its players is quoted here.

**The host was not quiet.** It is one 32-core machine shared with other sessions: its load average
was 20 to 60 for the whole run and the CPU pressure ("some") was 20 to 45 %. Counts, sizes, trends and
pass/fail checks are not affected by that; latency tails are, and are marked. The one instrument
that does not depend on the load is the benchmark (virtual clock): its numbers are given beside the
live ones.

## Verdict

| | Result |
|---|---|
| 1. Soak, 3 h, 8 players | No leak, no creep. Memory fell a little; threads and descriptors steady; restart from the final save keeps every container. The tool's own gate failed once on a transient and was corrected (below). |
| 2. Chaos, 400 actions in unit mode and 400 in serve mode | All invariants held in both. Five harness faults and **one server bug** found and fixed (a removed plugin's folder came back). |
| 3. Stress, limits, fuzz, churn, 64 and 128 players, queue cap, silent clients, disk full | All pass. The 256 MiB aggregate cap and drop-the-slowest work as written. |
| 4. Live update procedure | Rehearsed with the real 0.7.0 and 0.6.2 releases, in both directions. One real **bug found and fixed**, one limit that cannot be fixed from this side (stated in the runbook). Downtime about 0.5 s plus 8 to 17 s of preparation while the server keeps playing. |
| 5. Docker | Built from the real Dockerfile; `docker/test.sh` passes; update from the published 0.7.0 image rehearsed. |

Not production blockers found. What was not covered is at the end.

## 1. Soak

Release build, the late-game save, `tools/chaos/soak.py`: 8 players acting every 200 ms
(`pcds load`), autosave every minute, 6 backups kept and one every 5 minutes, S3 off, a status
sample every 30 s (359 samples over 10,829 s), a timed manual save every 10 minutes. A second
poller read the status once a second.

| | first fifth (warm-up) | second fifth | last fifth | trend |
|---|---|---|---|---|
| bank RSS | 49.3 MB | 46.1 MB | 45.8 MB | -0.5 % (slope -0.2 MB/h) |
| front RSS | 7.3 MB | 6.3 MB | 6.0 MB | -4.6 % |
| bank threads / descriptors | 1 / 4 | 1 / 4 | 1 / 4 | flat in every sample |
| front threads / descriptors (median) | 3 / 7 | 3 / 7 | 3 / 7 | flat |
| save file | 2.44 MB | 2.44 MB | 2.44 MB | flat |
| world objects | 33,678 | 33,682 | 33,661 | flat (the players build and mine; no drift) |
| backups on disk | | | 6 | the cap |
| server log | | | 33 KB | |

CPU over the run: bank 6.2 % of a core (6.6 % in the first half, 5.7 % in the second), front 1.6 %
(1.59 and 1.53). Load average of the host in the same time: 20 to 60.

**Tick** (the bank's own histogram of the last 60 s, read 359 times): p50 37 µs (median of the
samples; worst 108 µs). p99 median 3.9 ms; the p99 of those p99s is 52 ms. The worst tick of a window
was 0.36 s in the median sample and up to 5.5 s in the worst: see the causes below.

On the late-game world with the virtual clock (`bench::bench`, 8 players acting every 200 ms,
120 s simulated, so no host noise in it): 133 µs per tick on average, p50 2.4 µs, p99 1.1 ms,
p99.9 18 ms, max 25 ms; the tick at 30 Hz has 33 ms. The long ones are the
once-a-second logistics search (84 µs per tick on average, 64 % of the busy time). With nobody
else on the host this is the "longest loop in a second" the status shows: 23 ms median, 41 ms at
p99 in the seconds when the bank did not wait for a CPU.

**Autosave game-thread cost.** The loop makes the save text and a thread writes the file, the
backup and the S3 copy. `bench::scale` on this world: the text takes 8.5 ms on the game thread; the
write (89 ms), the backup (2.5 ms) are on the thread. In the soak, the longest loop in the three
seconds after an autosave was 35.3 ms at the median, against 34.6 ms in the other seconds (105
against 1,783 samples): the autosave does not show at one-second resolution. Timed manual saves took
26 to 55 ms, and once 5.8 s (at 10,277 s, in the worst stretch of the host's starvation below).

**Loop spikes over 50 ms and their causes.** There was no quiet stretch on this host. The longest
loop of a second was over 50 ms in a fifth to a quarter of the seconds sampled once a second (2,100 s
in two stretches), and in 262 of the 359 thirty-second windows (73 %); the worst was 4.9 s. The best
quarter hour (16:27) had a median worst loop of 45 ms and 13 of 30 windows over 50 ms; the worst
(14:12) a median of 657 ms. The cause is the host, not the server: in the last 37 minutes the bank's
main thread was given 129 s of CPU and waited 377 s for one (`/proc/PID/task/PID/schedstat`: 2.9 times
its CPU time); in 85 % of the seconds with a loop over 50 ms the bank had been waiting for a CPU for at
least half of that loop; in the 326 seconds in which it waited under 5 ms, 3 had a loop over 50 ms
(its p50 was 23 ms, its p99 41 ms). The status of a server on a host like this is honest about it:
`busyPercent` went to 514 (more than a core's worth of wall time spent in loops). A server on a quiet
machine should show the 25 to 40 ms of the once-a-second search and nothing over the frame; that was
measured only by the benchmark above and in the quietest minutes of the soak (one second in 48
over 50 ms, 58 ms at most), not on a quiet machine.

**The soak tool's gate failed, and was wrong.** The last fifth had a mean of 4 front threads
against 3, and 8 descriptors against 7: a stalled game loop piles up admin connections for a while
(maximum 9 threads and 19 descriptors, in the starved stretch), and a mean of small integers read
that as growth. Every sixth of the run has a median of 3 and 7, the bank is at 1 and 4 in every
sample but two. `soak.py` now compares medians for threads and descriptors (commit
"Soak: threads and descriptors are compared by their median"). The virtual size of the front
grew from 260 MB to 477 MB in the same stretch for the same reason (thread stacks, not memory in use).

**Restart from the final save** (`world.json` after the stop, rc 0, copied): the server was running
0.4 to 0.6 s after the start.

| | final save | after one restart | after a second restart |
|---|---|---|---|
| world objects | 33,667 | 33,642 | 33,642 |
| inventories | 2,381 | 2,381 | 2,381 |
| containers | 1,093 | 1,093 (0 lost, 0 new) | 1,093 |
| players, planets, messages | 11, 5, 56 | same | same |

The first load drops 26 loose objects (dropped ore, seeds, eggs) and the second keeps all: that is
the game's own behaviour on loading a save, checked against the real game
([COMPLETENESS.md](COMPLETENESS.md) item 3). The objects present in both saves differ in at most
28 fields after the second restart (drone hunger and similar, which move with time).
The save's one pre-existing flaw (a link to an inventory that is not there) is in the original too.

## 2. Chaos

`tools/chaos/chaos.py`, the late-game save, 6 rejoining players, 400 actions each, seeds 11
and 12, `PCDS_DEV=flip-verify` (every flip also checks that the restored world equals the
snapshot). After every action the tool waits for the server to settle and checks: one front and one
bank, the status answers, the `active` file names a bank, the world file loads and keeps its
invariants (unique ids, every item held once, links resolve), no `.tmp` of a dead writer, every
container of an acknowledged save still there, the players back within 45 s, a flip drops nobody,
invalid settings refused, a read-only folder fails a save visibly, the plugin list agrees with
what the churn did. The two runs used the same actions with different supervision.

| | unit mode | serve mode |
|---|---|---|
| supervision | `pcds run` under a transient user unit `pcds-qa-chaos`: `pcds setup`, `Restart=on-failure` after 15 s, 5 starts in 10 minutes, `KillMode=mixed`, 90 s to stop | the tool restarts the front when it is gone |
| actions, failures | 400, **0** | 400, **0** |
| kill -9 of the bank / the front / both / `pcds run` | 52 / 35 / 6 / 7 | 48 / 48 / 11 / not applicable |
| flips (to the other bank) and real updates (download, install, flip from a local feed) | 24 and 7 | 23 and 12 |
| flips with a kill of the front, of the old bank, of the new bank, with a world stop, with a second flip | 11, 5, 11, 13, 11 | 7, 12, 16, 9, 8 |
| world stop/start, bursts, switches to another world and back | 22, 10, 13 | 15, 6, 9 |
| SIGSTOP of the bank, front, new bank, the players | 11, 11, 12, 10 | 16, 13, 11, 7 |
| watchdog: 125 s stop (replaced?) / 10 s stop (not replaced?) | 2 of 2 replaced after 60 s / 3 of 3 not | 4 of 4 replaced after 60 to 61 s / 4 of 4 not |
| SIGTERM of the front or the bank | 3, 5 | 9, 8 |
| rules, settings, access, chat hooks, S3 on/off against a fake S3, pause, saves, read-only folder, 5000-character chat, status hammering | all drawn, all as expected | all drawn, all as expected |
| plugin churn operations (install, approve, disable, remove, a flipped bit in the module or the manifest, restore, commands, reload) | 690 | 720 |
| plugin states seen at the checks | absent 150, disabled 102, running 103, invalid 20, changed 21, incompatible 5 | absent 184, running 90, disabled 71, invalid 26, pending 9, changed 7, incompatible 14 |
| systemd gave up (5 starts in 10 minutes) and the tool reset and started it | 7 | |
| unit ended cleanly after a SIGTERM of the front (a stop: systemd does not restart it) | 3 | |

What the unit mode showed that the other does not: after the fifth kill within ten minutes systemd
leaves the unit failed and the server stays down until someone starts it (`pcds start` clears
the count first, as of this release). That is the unit's design ("a server that keeps crashing stops")
and it is in the runbook. A SIGTERM of the front is a clean stop, so the unit stays down.

The flip numbers in these runs include the check of `flip-verify` (median 1.25 s in unit mode and 2.2 s
in serve mode on the loaded host, with the verification of the 3.8 MB state on top of the stand). The
production flip, without the check, measured on the same world with 8 players acting every 200 ms,
12 flips: **the world stood 31 ms at the median, 39 ms at most** (snapshot 3.5 ms, restore 27 ms), and
none of the players was dropped.

**A plugin that traps in every event** (the bytes of a module whose `pcds_event` is `unreachable`) was added to the
churn after the two runs above, and run for a third time (section 2b).

### What failed and was fixed

| | What | Root cause | Fix |
|---|---|---|---|
| Server bug | After `rm -rf plugins/ID` of a running plugin, the plugin stayed in the list as `running` (even after a reload). | The state file of a plugin is written at every world save with `create_dir_all`, which made the plugin's folder again, with nothing in it but `data/`. The host read a folder whose manifest is gone as a plugin whose new files do not hold up, and keeps the approved version running (by design for a half-copied update). | The state is written only into an existing plugin folder (`create_dir`). Test `plugins::state::tests::a_state_is_not_written_into_a_plugin_folder_that_was_removed`. Commit "Plugins: a state written after the admin removed a plugin's folder no longer makes the folder again". |
| Harness | "A disabled plugin is not disabled" | the churn expected `disabled` for a plugin it had damaged itself, which stays `invalid` | the expectation is only for an undamaged plugin |
| Harness | "An orphan bank outlived its front" | the tool saw a bank of the unit's next start in a second look at the processes | one look, and only a bank without a front counts |
| Harness | "The unit had given up (Result=success)" | the instant between systemd's restart job and the start looks inactive | three looks a second apart, and the event is named by the unit's real result |
| Harness | a real update failed with "Peer disconnected" once | the tool's fake release server answered HTTP/1.0 without `Connection: close` and the next request met the closed socket | the header; the 19 updates of the finished runs all succeeded |
| Harness | the plugin that traps was invalid | the export entry for `pcds_free` had the wrong length in the hand-written module | fixed; the plugin now faults and is switched off after 5 faults in 60 s as designed |

The first versions of the two chaos runs had these harness faults and were started again from the
start after each fix; the numbers above are of the runs that finished with the fixed tool.

### 2b. Plugin faults during everything

A third run, 200 actions in unit mode (seed 13, the same late-game world, the plugin that traps in every event added to the churn: 48
installs or visits of it, each followed by 1 to 8 chat lines to trap on, between all the other faults and the churn of
`hello-stats`): **0 failures.** The log has 1,193 traps ("wasm `unreachable` instruction executed"), 211 switch-offs
("switched off after 5 faults in 60 s", started again by the next reload), no panic of the host, and the other plugin,
the world, the flips (16), the real updates (6), the kills (bank 12 and during a save 7, both 5, `pcds run` 4) and the
watchdog (2 replaced) carried on: every invariant held. The state of the plugin list matched what the churn
did at every check (361 operations).

## 3. Stress, limits, fuzz, churn, many players

All on the late-game save, one fresh server each.

| Tool | Result |
|---|---|
| `stress.py burst`: 16 players acting every 200, 20, 5 and 1 ms for 30 s each | longest loop 35, 29, 30 and 42 ms; busy 6.3, 7.5, 10.8 and 21.8 % (mean); nobody dropped |
| `bigchat`: 8 players, chat lines of 900,000 characters | the transport refuses a batch over 1 MiB ("a batch over 1 MiB, dropped"); longest loop 32 ms; front 10 MB |
| `full`: 40 rejoining players on a 16-player server | 16 in the game at most, longest loop 34 ms |
| `stop`: the bank stopped for 30 and for 120 s with 16 players | front memory 9 to 14 MB; a 30 s stop: status again after 0.1 s, nobody lost; a 120 s stop: the watchdog replaced the bank at 60 s, the 16 players rejoined |
| `queuecap` (new): 60 players sending 450,000-character chat lines (900 KB on the wire) while the bank stands for 40 s | front memory 37 MB before, **318 MB at the peak**, a plateau of about 300 MB: the cap of 256 MiB holds. 39 clients were dropped, each the one with the most waiting ("client 16: 8 MiB waiting for the game, the most of the 255 MiB all clients have waiting (at most 256 MiB): dropped"). After the CONT the status answered in 0.8 s and the players came back |
| `slowclient` (new): 8 healthy players and 4 whose process stopped for 50 s | the 4 silent sessions were gone after the 30 s timeout; the healthy players' longest loop 28 ms (32 ms before); the front 9 to 10 MB; no healthy player failed |
| `limits.py`: the connection limits with raw datagrams, on a real LAN address | 4 handshakes per address in 10 s, 8 sessions per address, 128 in all; silent sessions gone in 30 s; a real player joined with the table full (after 7 s) and after the flood |
| `fuzz.py`, 3 seeds, 90 s each | 1.57, 1.75 and 2.45 million garbage datagrams: front and bank alive, memory flat (front 7 to 8 MB, bank 42 to 77 MB), no panic |
| `churn.py`, 2 seeds, 120 s, 24 workers | 2,321 and 2,122 sessions that play and leave, vanish, never become players or only handshake, up to 53 players at once; afterwards no player left, a fresh player joins, the save is sound |
| disk full (`diskfull.sh`, a 32 MB private tmpfs) | a save on a full disk answers 500 and says so in `lastSave`; the autosave fails visibly and again later; after freeing space the save works; a stop on a full disk keeps the snapshot beside the world and a restart loses no container (999 of 999); a kill of the bank on a full disk: status 200 in 0.2 s |

**64 and 128 simulated players for 10 minutes** (`tools/perf/loadrun.py`, `pcds load`, every player acts every 200 ms:
walks, crafts, builds, chats, mines, sorts). Host load 20 to 30. 128 players against a server that holds
at most 100 (the rule `limits.maxPlayers` goes up to 100): the 100 slots fill and the 28 others are refused, as designed.

| | 64 players | 128 players (100 slots) |
|---|---|---|
| front CPU (user + system, % of one core) | 2.7 + 4.7 = 7.4 | 3.4 + 5.4 = 8.8 |
| bank CPU | 9.3 + 1.1 = 10.4 | 9.0 + 1.1 = 10.1 |
| idle, for comparison: front, bank | 0.1, 5.1 | 0.1, 2.9 |
| front RSS before / during / after | 8.0 / 7.5 / 9.3 MB | 8.1 / 12.0 / 13.3 MB |
| bank RSS before / during / after | 76 / 46 / 49 MB | 74 / 52 / 56 MB |
| received by a player | 54 KB/s, 2,504 messages/s in 49 batches/s | 62 KB/s, 3,024 messages/s in 40 batches/s |
| the server's output in all | about 3.5 MB/s | about 6.2 MB/s (50 Mbit/s) |
| craft round trip p50 / p95 / p99 / max | 0.59 / 40 / 685 / 2,213 ms | 0.57 / 8.9 / 45 / 343 ms |
| status after the run: busy, tick p99 | 8.2 %, 5.1 ms | 5.7 %, 10.7 ms |

The tails of the 64-player run are the starved host again (its minutes overlapped a build on the host); the
128-player run had a quieter one. What is solid: nothing grows (the bank gives memory back while the
players play), the front costs under a tenth of a core for 100 players, and the bank's cost is about
a tenth of a core more than idle. The bandwidth in is not reported (the status counts bytes in and out
for the whole server; the run did not read them).

## 4. The live update procedure

Done on a systemd user unit named `pcds-qa-rehearsal` (with the real systemd restart rules),
the real published v0.7.0 and v0.6.2 release tarballs downloaded into empty folders and checked
against their sha256, the real game folder for the tables, a `server.env` with a made-up token, 3 scripted
players, and this build as a local release feed (tag `v0.8.0`). The runbook that came out of it is
[upgrading.md](upgrading.md); its page has the steps, the rollback, Docker and the list of what changed.

| Step (late-game world unless noted) | Command total | Server stopped at | Three players back after the disconnect |
|---|---|---|---|
| 0.7.0 to this build, `upgrade v0.8.0 --restart` | 14 to 17 s (19 s on the 2,500-object bench world) | after 14 to 17 s | within 0.5 to 0.6 s |
| back, `upgrade v0.7.0 --restart` | 11.2 s | 10.3 s | 0.51 s |
| back, `upgrade --rollback --restart` | 8.6 s | 7.6 s | 0.52 s |
| 0.6.2 to this build and back (bench world) | 16 s and 23 s | | about 1 s |
| a restart onto the same formats | 1.5 s | 0.6 s | 0.5 s |

Where the time goes: 10 to 16 s of the command are the download and re-reading the game tables for the
new tables format, while the server keeps playing. **Players are disconnected for about half a second to a
second** (that includes the 0.3 s pause the scripted players make before reconnecting). The world kept all its
containers through 8 restarts in both directions; the world written by this build loaded in 0.6.2 and 0.7.0.
A hot update across this jump is refused with a message that names `--restart` (tables format 2/3 to 4,
snapshot format 3/6 to 7).

**Bug found and fixed: the order of a restart that fails.**

* With the retired settings still in `server.env` (`PCDS_AUTOSAVE`, `PCDS_BACKUPS`, `PCDS_BACKUP_MINUTES`:
  what every 0.6.x `pcds setup` writes), `pcds upgrade vX --restart` installed the new release, stopped the
  server, and printed "server restarted" while the new release refused the file ("unknown setting ...").
  systemd tried again every 15 s and gave up after 5 starts: **the server stayed down** (nothing lost;
  recovery: delete the lines, `pcds start`). Both 0.6.2 and 0.7.0 behave so: the command is run by the
  release that runs now, which cannot know what the next one refuses, and **this cannot be fixed from the new
  release for those two**. The runbook says: take the lines out *before* the upgrade (editing the file under
  a running server does nothing).
* From this release on, `pcds upgrade --restart` asks the new release (`pcds check-settings`) before it
  stops anything; when it would refuse the file the old release stays active, the server runs on and the
  message says what to fix (tested with a stand-in release that retired a setting: uptime unchanged, the 3
  players stayed connected). It also waits for the world after the restart and says so loudly if the unit gave up.
* Found on the way: systemd counts manual restarts toward its limit of 5 starts in 10 minutes, so an upgrade, a
  rollback and an upgrade again left the unit failed and the server down. `pcds start`, `restart` and
  `upgrade --restart` now clear the count first (`tests/cli.rs::a_restart_clears_the_units_start_count_first`).
* A retired setting found only in the process environment (a compose file's `environment:`) used to be ignored
  without a word, so 0.6 values silently became defaults. The start now says "ignored in the environment:
  PCDS_AUTOSAVE is the rule save.autosaveMinutes now", and the refusal in `server.env` names the rule that replaced
  each (`config::RETIRED`, tested against the rules that exist).

## 5. Docker

The image was built **from the real Dockerfile** (`docker build -t pcds-qa-soak .`, rootless daemon,
about 45 s); `docker/test.sh` with the tables passes in 40 s: the updater on a read-only root, a hot update
with 3 players kept, `docker restart` coming back on the updated bank, and `--rollback --restart` in place.
The update path of a compose user was rehearsed with the published `ghcr.io/perara/planet-crafter-dedicated-server:0.7.0`
image on a copy of the late-game world, replaced by the image built here on the same `/data`: removing
the old container took 1.0 s and the new one needed 12.2 s to a running world (ten of them reading the game
tables again), so players wait about 13 s there, not one. With a retired line left in `server.env` the container
restart-loops, printing the refusal each time, and starts 0.14 s after the file is fixed. All `pcds-qa-*`
containers and images were removed.

## Checks of the final tree

On the final tree (the commits of branch `final-soak`):

| Check | Result |
|---|---|
| `cargo test --release --no-fail-fast` with the tables (format 4) | 640 unit tests, `cli` 15, `hot_update` 24 (3 ignored), `admin_golden` 4, `layers` 1: all ok |
| the same as CI runs it (`PCDS_TEST_DATA=none PCDS_GAME_DIR=none`) | all ok |
| `cargo clippy --release --locked` | 0 warnings |
| `map.py check` | 732 groups, 208 RPCs, 165 components, 0 problems |
| bench fingerprint on the bench world, 4 players, 300 s | `15c299165b6b3eb0`, and the handoff run `15c299165b6b3eb0`: EQUAL |

(`tools/dev/verify.sh`; the tests that start servers ran beside two chaos runs and a host load of 30 and passed.)

## What was not covered

* A real Steam invite code (Steam was off everywhere), and a real game client rejoining after a restart.
* The actual GitHub download of the new release (a local feed with the same layout stood in; the
  published 0.7.0 and 0.6.2 tarballs were downloaded for real).
* Releases 0.6.0 and 0.6.1, Kubernetes, Windows hosts.
* A quiet machine: the tail latencies above are of a loaded host. The loop budget on a quiet
  one was measured only by the benchmark and the first ten minutes of the soak.
* Bandwidth into the server, and a client on a slow or lossy network (the slow-client test stops
  the client; it does not throttle it).
* Plugins beyond `hello-stats` and the trapping module: the other examples, signed plugins and `net.http`
  are covered by the unit tests only.
* Chaos in unit mode with a disk-full or an S3 failure at the same time as a kill: each fault is drawn on its
  own, on top of the churn; `diskfull.sh` runs by itself.
* `make gate` (a hot update from the published release into this build): skipped on purpose, the formats differ.

## Round 2: 12 h

The second round asked a harder question than the first: not "does it run for three hours" but "does it
degrade, over hours and over simulated years, when the conditions keep changing". It had four parts: a long
soak with an hourly timetable of everything an operator's week holds, the virtual-clock soak for years of game
time, resource pressure (one resource taken away at a time) and the real Docker image under memory limits. It
found **two server bugs, both fixed** (a memory growth that only a long game shows, and a watchdog that killed a
healthy game process after the front itself had been stopped), one bug in a shipped example plugin (fixed), and one memory step in the second soak that is **not explained**
(R2.2).

Everything ran on scratch servers (units `pcds-qa-soak2-*`, ports 8640 to 8659), on copies of the late-game save of
the first round (33,583 world objects, 2,246 inventories, planet Humble, `world.dlcPlanets=Humble`, tables format 4).
Nothing of the save's players is quoted here.

**What actually ran, and the gaps.** The plan was one soak of 12 h on v0.8.0. What exists is: **part A**, the release build `v0.8.0` (commit c5ca29e), 7.6 h (27,300 s,
910 samples, no gap over 45 s); **part B**, a second soak of 8.0 h on the build of `integrate` as it was then (with `pcds doctor`, `/api/metrics`, structured logging; section R2.2), started when
part A was at 4.8 h and run beside it for the next 2.8 h; and **a third soak, R2.6, of the final code** (everything merged) at twice the pace. The agent driving the work was stopped by an API rate limit
twice (once for about 70 minutes after the 4 h 26 min mark of part A, once for about 8 hours while part B finished). The soaks are transient systemd units of their own and went on
unobserved: neither lost a sample (the sample files have no gap over 45 s) or a scheduled event. Part A was ended by hand at 7.6 h, after its fourth kill of the bank (a clean stop by `POST /api/stop`, exit status 0, the unit's
Result=success, then the orchestrator stopped); part B ran its 8 hours to the end. Nothing was restarted from zero because of the interruptions.

**The host was not quiet, again.** One 32-core machine shared with other sessions; load average 25 to 125 while these ran
(the soaks, the growth runs, test suites and builds of this session were part of it). Counts, sizes, trends and checks
are not affected; latency tails are, and are marked "host" below. The best hours of part A (4, 6 and 7) had a median
longest loop of 36 to 76 ms and a tick p99 of 1.7 to 3.8 ms; the worst (2 and 5) 380 and 350 ms. The virtual-clock
numbers do not depend on the load.

### Verdict

| | Result |
|---|---|
| 1. Soak, part A (v0.8.0, 7.6 h, 8 to 32 players, 25 hot flips, 4 kills, a SIGSTOP) | No leak. Memory, threads and descriptors flat; what grows is what the bots build (9,035 containers). One unexplained 16.9 s gap of a game loop in a flip during a 32-player burst on a loaded host. |
| 2. Soak, part B (an early build of `integrate`: doctor, metrics, JSON log; 8.0 h) | The ops features hold: 1,916 scrapes of `/api/metrics` valid, `pcds doctor` ok every hour, every log line JSON, 26 hand-overs, 4 kills, a SIGSTOP. **But from hour 3.2 the game process started each time at 100 to 120 MB instead of 40 to 50 and the front stayed at 27 to 36 MB after a churn: unexplained.** A `net.http` plugin copy of the `discord-notify` example ran out of fuel 158 times (fixed in the example). |
| 3. Virtual-clock soak, 60 and 400 days, and 825 to 835 days (the wrap of the wire tick) | **One server bug found and fixed** (below). After the fix: flat for 400 days. |
| 4. Resource pressure | **One server bug found and fixed** (the watchdog). CPU, disk, memory, descriptors, many worlds, failing hooks and S3 and a frozen front: graceful, with the limits named below. |
| 5. The final code (all branches merged), the timetable at twice the pace, 2.2 h | Flat where part B stepped: front 8.1 to 8.7 MB, bank 19 to 40 MB, 14 hand-overs with the world standing 22 ms at the median (quiet host), metrics and doctor and JSON log clean. Too short to call the part B step gone (R2.6). |
| 6. Docker under `--memory` | Starts and plays down to 112 MB; the flip refuses cleanly under about 160 MB; the restart install works at every size; the kernel killed nothing. |

### R2.1 Part A: the soak on v0.8.0

`tools/chaos/longsoak.py` (new): the server runs as the unit `pcds run` under systemd (`Restart=on-failure`), autosave every minute,
a backup every 10 minutes (24 kept) copied to a fake S3, chat hooks to a fake webhook, the plugins `hello-stats`,
`welcome-rules` and a `net.http` plugin that posts to a local fake (built from `discord-notify` with its URL changed), a
dashboard-like client reading `/api/status` once a second (and plugins, worlds, map, logs less often), a sample
every 30 s. The same timetable runs every hour: 8 players acting every 200 ms (`rejoin` bots, which come back when
dropped); :05 pause 60 s; :07, :22, :37, :52 another set of live rules; :10, :30, :50 a hot flip to the other bank (a copy of the
binary with a changed tag); :15 and :45 plugins reloaded; :22 to :34 16 players, :40 to :52 32 players, :53 to :59 24 workers
of join and leave churn; hours 3 and 7 a real update from a local feed (download, install, flip); kill -9 of the bank at :35 of
the odd hours; one SIGSTOP of the bank for 30 s at hour 6 :12; a 10-minute S3 outage in hours 1 and 5, a 5-minute
webhook outage in hours 2 and 6. Every hour the save is checked for its invariants.

What the timetable did in 7.6 h: 4,191 sessions of bots that got in, 25 hot hand-overs (23 flips, 2 real updates), 4 kills of the bank, 1 SIGSTOP of the bank,
75 backups copied to the fake S3 (110 MB), 179,594 chat-hook posts, 1,770 plugin posts, 15 plugin reloads, 8 pauses, 30 rule changes.

**Incidents and checks.**

| | |
|---|---|
| kill -9 of the bank (4, at hours 1, 3, 5, 7) | the world was back in 0.6, 0.7, 1.2 and 0.8 s; the 8 players were back after 2.8, 2.7, 3.3 and 2.8 s; no acknowledged container lost; invariants held after each |
| SIGSTOP of the bank for 30 s (hour 6) | the same bank (pid unchanged), no crash counted, the status answered again after 0.2 s, 8 players still in. The watchdog did not trigger |
| 25 hot hand-overs | all `ok`; no flip dropped a player. World stood (front's clock): p50 141 ms, p95 1.37 s, max 1.78 s (host; the first round measured 31 ms median on a quieter host, with 8 players). Snapshot p50 45 ms, restore p50 96 ms. The snapshot grew from 4.0 MB to 8.3 MB, as the world did |
| S3 outages, webhook outages | the server went on; the doctor, the log and the status say so; recovered after each |
| hourly checks of the save | 8 of 8 passed (unique ids, items held once, links resolve; the original's one dangling link excepted) |
| the status read once a second | 47 calls of 27,000 were slow (over 1 s) or failed: 8 of them during the SIGSTOP of the bank and the final stop (500 "the game loop did not answer", by design), the rest the host |
| the final stop | rc 0 (a clean stop on `POST /api/stop`), the unit ended with Result=success; the final save loads |
| panics | none |

**Trends** (`tools/perf/trend.py`, Theil-Sen; the first 10 % dropped; the steady 8-player samples only, so the load is the same in each sample;
the "all samples" table is in the soak's folder). Tau is the share of pairs of samples that agree on the direction.

| | median | slope per hour | drift over the run | verdict |
|---|---|---|---|---|
| front RSS | 8.0 MB | -122 KB | -10.6 % (tau -0.32) | **flat** (falling) |
| bank RSS (all samples; a new process every 20 min) | 51 MB | +1.9 MB | +25.6 % (tau +0.43) | follows the players (see below) |
| bank RSS, inside each of the 22 processes of 12 to 19 minutes | | median -4.6 MB/h | median -1.7 % | **flat**: 11 flat, 9 falling, 2 creeping |
| front threads / descriptors | 4 / 9 | 0 | 0 | **flat** |
| bank threads / descriptors | 5 / 4 (1 to 8 as a save, an upload or a churn comes and goes) | 0 | 0 | **flat** (median of the first third equals the last's; in each process too) |
| admin connections open at a sample | 0 | | | **flat** |
| front CPU | 2.05 % of a core | -0.03 %/h | | **flat** (first third 111 s of CPU an hour, last third 108) |
| bank CPU | 6.0 % of a core | -0.08 %/h | | **flat** |
| tick p50 | 41 µs | | first hour 46, last 42 | **flat** |
| tick p99 (host) | 5.1 ms | | first hour 4.4 ms, last 2.4 ms | **flat**, falling as the host calmed |
| busy percent (host) | 8.9 | -0.6 /h | | **flat** |
| log | 13,200 lines, 0.55 MB an hour | | last third 0.91 of the first | **flat**; the lines are the sessions (connected, ready, left: 5,764 "connected" and 5,710 "left the game" in 7.6 h), no spam |
| backups on disk | 24 (the rule) | | reached at hour 3.7 | **capped** (82 MB) |
| home folder | 96 MB median | | 18 to 117 MB while the backups filled | **capped**: it stops at 117 MB |
| messages in the save | 56 | | 56 at the end | **capped** |
| world objects | 39,660 | +1,277/h | 33,583 to 43,778 | **growth, and it is the bots**: 9,035 of the 10,195 new objects are `Container1` (the bots build up to 20 in each session; 4,191 sessions), the rest items they made. The save grew 2.4 to 3.9 MB with them. Not a leak: the same load on a world with no new sessions is flat |
| inventories, network objects | | | 2,246 to 11,368; 3,160 to 12,490 | the same containers |
| saved players | 67 | | 3 to 67 | **capped by the bots' names** (they reuse 64 names). A server with ever new visitors grows by 1.2 KB each until `players.forgetAfterDays` |

The bank's RSS in all samples follows the players: the same hour with 8 players has 44 MB, with 32 players 58 to 79 MB, and a bank
that has had a 24-worker churn is 60 to 80 MB for the minutes after it. Each new process starts at 38 to 48 MB. The two creeping
processes (+17 and +22 MB in 19 minutes) are the ones the SIGSTOP of the bank and the 32-player burst fell in: a stopped bank queues what the players send.

**The 16.9 s gap.** One gap of a game loop: at hour 6 :50, in a flip during the 32-player burst, after "the running bank is told to hand the world over"
(the front's own clock says the world stood 1.37 s; the old bank took 1.18 s to answer, 1,521 client frames were held; the log of the game says
"the game loop was away 16.9 s", and the status that second says longest loop 16.9 s). The other 30 such lines of the run are 1.3 to 5.7 s
(all in churn or burst phases of the busy hours; one is the 30 s SIGSTOP). It was not seen again and could not be reproduced; the
cause is unknown. Treat it as the host (a 17 s starvation of a process on a machine with a load of 60 to 120) until a quiet host
says otherwise. Nothing was lost: the world's time went on by a third of a second only (docs/clock.md), the save is sound.

### R2.2 Part B: the same timetable on the build of `integrate`

Built from the branch `integrate` as it was after its first ops-ready commits (`pcds doctor`, `/api/metrics`, structured logging, snapshot
format 8) with this round's two fixes, **before** the memory diet and the other work that `integrate` has now; `--ops` of `longsoak.py` turns on what that release
adds: `log.format=json`, `save.verifyBackups=true`, a scrape of `/api/metrics` every 15 s checked as Prometheus text (every value finite, no series twice, only `pcds_` names,
no counter that goes back) and `pcds doctor` in the third minute of each hour. The timetable is part A's, from a fresh start. It ran 8.0 hours
(28,813 s; 959 samples, no gap over 31 s) while the first soak was still running (2.8 h of overlap on one host).

| | Result |
|---|---|
| kill -9 of the bank (hours 1, 3, 5, 7) | world back after 0.6, 0.5, 0.5, 0.4 s; 8 players back after 2.6, 2.5, 2.5, 2.4 s |
| SIGSTOP of the bank 30 s (hour 6) | the same pid, no crash counted, status again after 0.0 s |
| 26 hot hand-overs (24 flips, 2 real updates) | all ok, none dropped a player; the world stood p50 49 ms, p95 237 ms, max 369 ms (the later hours were quiet: tick p99 under 1 ms); snapshot p50 6.9 ms, restore p50 42 ms |
| `/api/metrics` | 1,916 scrapes (every 15 s): 79 to 87 series, 8.8 to 10.5 KB, median 3 ms (max 5.1 s, in the SIGSTOP); every one parsed, every value finite, no duplicate series, no counter went back. Two scrapes got HTTP 500 ("the game loop did not answer") while the bank was stopped: the harness counts them as failures, they are the stopped bank. The memory the metrics report (`pcds_memory_resident_bytes`) is the process's own RSS to the kilobyte |
| `pcds doctor` | 8 runs, exit status 0 each time; the warnings are the harness's (the rules file is 644, the open-files limit 1024, the fake update feed answers nothing, the unit is transient), the S3 failure of the outage and "the release in bank a was rolled back" (the soak flips with rollback) |
| log | 21 MB, 113,677 lines, **all JSON** (0 lines that are not); 2.35 MB an hour with JSON (0.55 MB as text) at the same 12,700 lines an hour |
| `save.verifyBackups` | on for the whole run; the backups verified, nothing in the log |
| hourly checks of the save | 8 of 8 passed |
| panics, final stop | none; rc 0 |

**Trend** (steady 8-player samples): thread and descriptor counts flat; per-hour medians of the front's and the bank's resident memory:

| hour | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 |
|---|---|---|---|---|---|---|---|---|
| front RSS (MB) | 7.7 | 7.6 | 7.4 | 8.1 | 27.6 | 28.8 | 35.3 | 36.2 |
| bank RSS (MB) | 45.8 | 44.9 | 45.7 | 74.4 | 111 | 115 | 119 | 123 |
| world objects | 33,702 | 35,728 | 37,074 | 38,192 | 39,564 | 40,752 | 42,085 | 43,242 |
| busy percent (host), median | 20 | 7 | 6.7 | 3.8 | 3.1 | 2.8 | 2.8 | 2.8 |

**This is a finding that is not explained.** In the first three hours the game processes behaved as part A's: a new bank (every 20 minutes) starts at 38 to 50 MB and stays
within about 10 MB of that. **From the flip at 3 h 10 min every new bank started at 100 to 120 MB and stayed there** (the per-process table: 0.4 to 3 h:
first sample 40 to 65 MB; 3 h 10 min on: 99 to 127 MB, flat within each process, "thr 1"), and the front, which came back to 7 MB after each of the first three churns,
stayed at 27 MB after the third (3 h 54 min) and reached 36 MB at hour 7. The world had grown by 13 % in that time, not by a factor of 2.5, and part A's bank of the same size
stayed at 52 to 58 MB. Nothing in the log marks the step; in the minutes before it the plugin `net-soak` was switched off for faults (below). Metrics
and doctor agree with the process numbers, and nothing else grew: threads, descriptors, backups, the hand-overs' times, the tick are flat.
What was checked afterwards: **on the build `integrate` has now (with the memory diet), a server on the same late-game world with `save.verifyBackups` on and off, a backup every minute and 8 players for 10 minutes holds
33 MB in the bank and 9 to 10 MB in the front, flat, identical with the rule on and off**, so the rule is not the cause there; a 90-second churn with the plugin failing leaves the bank at 13 MB
on the bench world. A soak of the final build at twice the pace is in R2.6 (flat, but only 2.2 h). Until it says otherwise: **creeping, step-wise, cause not found, not reproduced on the final build in the short probes.**

**Found on the way: the `discord-notify` example switched itself off during an outage.** The soak's `net.http` plugin is a copy of that example with its URL pointed at the
fake. The fake was not listening on the port the plugin names (the soak ran on other ports than the one compiled into it), so every post was refused, the lines piled up,
and each tick, which builds a message of up to 1,900 characters from them, used all the fuel an event may use: 158 faults and 31 switch-offs in 8 hours ("tick: out of fuel: the call used all 1000000
fuel it is allowed"), the host contained every one of them (the game never noticed: tick p99 and the status are those of part A) and the plugin never sent what it held. The manifest of the
shipped example now asks for 4,000,000 fuel an event and is signed again; the same plugin against the same absent webhook for 130 s: 5 faults before, 0 after.


### R2.3 The virtual clock: years of game time

`bench::soak_growth` (the ignored test of docs/testing.md) plays scripted players that act every game second (craft, drop, pick up, build, eat,
drink, mine, chat, sort) in steps of a third of a second of game time (the longest frame, docs/clock.md), starting at any age of the
server (`PCDS_BENCH_START_DAYS`), and at each report prints the age, the heap, the resident memory, the size of the save and of the
hand-over snapshot, the wire tick, and the sizes of every part of the state that grew; it also loads and saves twice (the save must
reach a fixpoint). New in this round: `PCDS_SOAK_PLAYERS`, `PCDS_SOAK_ONLY` (which actions), `PCDS_SOAK_PARTS` (the heap each part holds), and the report
prints after its own temporaries are gone (they made the first numbers look like a leak of 1 GB: the report held the serialised world, a
second and a third copy of the game).

The speed decides what can be run: on the 2,565-object bench world with 2 players 400 days in 1 h 43 min (about 230 days an hour) and 60 days with 8 players in 23 minutes; on the 33,000-object
late-game world a game day takes 1.3 to 1.5 hours (the host loaded). So the years ran on the small world and the late world ran for days.

| Run (all with the fix below unless it says) | Game time | Players | Heap / RSS at the end | Save / snapshot at the end | Verdict |
|---|---|---|---|---|---|
| bench world, **before the fix** | 70 days | 2 | 376 MB RSS (start 129) | 169 KB / **2.5 MB** (start 433 KB) | **leak**: about 650 world objects a day that nothing holds |
| bench world, **before the fix** | 28 days | 8 | 549 MB RSS | 191 KB / **3.77 MB** | the same, four times as fast |
| bench world | 400 days | 2 | 59 MiB heap, 101 MB RSS (start 59, 129) | 169 KB / 452 KB (start 433) | **flat** from day 1; wire tick 1,036,800,007 |
| bench world | 60 days | 8 | 63 MiB, 103 MB RSS | 191 KB / 523 KB (start 447) | **flat** after the first 8 days (the players build their 20 containers each) |
| bench world, **clock started at 825 days** | 7 days (to day 832) | 2 | 59 MiB, 100 MB | 169 KB / 453 KB | **flat**; the wire tick wraps to -2,139,719,291 at 828 days and nothing notices |
| bench world, **clock started at 826 days** | 9 days (to day 835) | 8 | 63 MiB, 104 MB | 191 KB / 525 KB | **flat** across the wrap |
| late-game world | 2.75 days | 4 | 370 MiB, 551 MB RSS (start 356, 571) | 2.43 MB / 3.95 MB (start 2.40 / 3.74) | **flat** from the first half day (the logistics tasks settle at 380 KB of state) |
| late-game world, **clock started at 400 days** | 0.75 day | 4 | 370 MiB, 554 MB | 2.43 / 3.95 MB | **flat**, the same numbers as day 0 |

In every run the save reached its fixpoint (a save loaded and saved again is the same text), the network objects without a world object stayed at
what the save started with (0 on the bench world, 9 on the late one), and no run panicked. Timers, counters and the clock are tested by `timetravel`
(docs/clock.md: the same steps at 0, 1, 3, 7, 25, 60, 200, 400 and 900 days); the runs here add that a 400-day-old world with players, saves, loads and the
growth of the world behaves like a young one, and that the wrap of the wire tick at 828 days is not noticed by a server whose scripted players are connected through it
(a real game client's resync was not tried, as in docs/clock.md).

**Bug found and fixed: a pick-up that no inventory takes left a world object behind.** `AddWorldObjectToInventoryServerRpc` makes the world
object when the world does not have the id (a dropped item that had despawned, or an id a client made up). When the inventory was full the object stayed: held by
nobody, placed nowhere, never saved, never freed. The scripted players do it about 650 times a day each pair; a hostile client could do it as fast as its
RPC budget allows, and the world's memory grows with it until the server restarts. Found by `PCDS_SOAK_ONLY` (only item, drop, pick grew) and `PCDS_SOAK_PARTS`;
fixed in `inventories.rs` (the object made for the request is taken away when the request fails); test
`inventories::tests::a_pick_up_that_fails_leaves_no_world_object_behind` (it fails without the fix).
Not a bug: what the soak's first reports looked like (a chat outbox that grew by 6 MiB a day) was the test harness, which did not empty
what the bank's loop sends on to the front; the harness does it now.


### R2.4 Resource pressure

`tools/chaos/pressure.py` (new): one resource taken away at a time with the unit's cgroup, on the late-game world, each scenario
on a server of its own, sampled every 5 s, one JSON line each. Windows are 6 to 15 minutes. Latency numbers are "host" numbers (load 25 to 125).

| Scenario | What was done | What the server did | Result |
|---|---|---|---|
| CPU free / `CPUQuota=100%` / 2 cores (`AllowedCPUs`) | 32 players acting every 200 ms, 10 min each | all 32 stayed; the bank uses 8 % of a core (front 4 %), so the quotas did not bind. Longest loop p95 435 / 333 / 345 ms (host), status never unanswered | pass |
| `CPUQuota=12 %` | the same, 6 min: below what 32 players need | busy 52 % at the median, longest loop p50 423 ms, p95 644 ms, tick p99 73 ms; 32 players stayed, status 27 ms | pass: slow, not broken |
| `CPUQuota=6 %` | half of what is needed | busy 119 %, longest loop p50 0.97 s, p95 1.7 s, max 2.3 s, tick p99 0.9 s; the 32 players stayed, the status answered (p50 162 ms, max 1.7 s); 17 "the game loop was away" lines, the world's time goes on by a third of a second only per gap | pass: degrades by getting slow, drops nobody |
| Slow disk, writes (`IOWriteBandwidthMax=512K`) | 8 players, autosave and backup every minute, 8 min | longest loop p95 779 ms against 510 ms for the same server unthrottled (host); saves and backups never reach the tick (the writer is a thread) | pass |
| Slow disk, reads as well (`IOReadBandwidthMax=512K`) | the same, 8 more minutes | longest loop p95 3.1 s, max 3.5 s; one status unanswered for 10 s; the game thread waits in `folio_wait_bit_common` for pages of its own program (found by sampling `/proc/PID/wchan`: up to 8.8 s at a time) | **limit, not fixed**: a disk that reads 512 KB/s makes every first touch of a rarely used page of the binary a stall. Writes are harmless. Pinning the program in memory needs `RLIMIT_MEMLOCK`, which the unit does not have by default |
| `MemoryHigh=128M` / `96M`, no swap (the server holds 90 to 105 MB with its cache) | 8 players, 8 min each | 128M: nothing (0 events). 96M: 8,292 `high` events, status max 4.2 s, longest loop p95 587 ms, max 2.6 s; 8 players stayed; no OOM kill. At 64M the start did not finish in 90 s (the kernel makes every allocation wait) | pass above 96M; below the working set it is the kernel's throttle, not the server |
| `LimitNOFILE=256` | 100 players, then 300 idle connections to the admin port for 60 s | all 100 stayed; the front never used more than 7 descriptors (one UDP socket serves all players); the idle connections are capped at 64 by the admin server (the others are refused: 5 status calls in the flood got no answer), no spin (front CPU 9.9 % before and during), 1 log line, normal again 10 s after they closed | pass |
| 20 worlds, 200 switches | `POST /api/world/switch` at random | 200 of 200 ended in the world asked for; 0.27 s (first third) and 0.30 s (last third) a switch, max 0.72 s; the front 8.8 to 10.9 MB, threads 3, descriptors 7 before and after; no `.tmp`; 5 log lines a switch | pass |
| Chat hook that answers 500, hangs 8 s, drops the connection (15 min each), 16 players with 1,500-character chat lines | 75 min | nothing stopped: the status never unanswered, loop p95 252 / 813 / 554 ms (host), front 8.8 to 11 MB, 4 to 5 threads; the hook is skipped "for 60 s" after its failures and gets events again; 231 log lines in all; the save's messages stay at 56 | pass |
| S3 returns 500, hangs 8 s, drops the connection (10 min each), backups every minute | 50 min | one attempt per backup (8 to 10 in 10 minutes), never a queue; the bank 52 MB throughout; the log has the failures (10 + 10 lines); S3 worked again at the first backup after | pass |
| Front frozen (SIGSTOP) 60 s with 8 players | | **before the fix**: on SIGCONT the front found the bank silent for 60 s and killed it ("said nothing for 60 s ... it is hung; killed"), the world restarted from the save. **After**: the bank is the same process, the 8 players are back 0.1 s after the CONT | **bug fixed** (below) |
| Backups stamped in the future | 5 files dated 2099 in `backups/`, rule keeps 3, backup every minute | the future ones were pruned first, 3 real backups remain | pass |

**Bug found and fixed: a front that was stopped killed the healthy bank.** The watchdog counts the time since the bank's last frame. The front stopped
for longer than `limits.bankWatchdogSeconds` (SIGSTOP, a paused virtual machine, a suspended or starved host) took its own absence for the
bank's silence, and its first look afterwards killed the bank and restarted the world from the last save, every player gone and up to a minute of
play lost. The watchdog now notes when it last looked, and after a gap of more than 5 s (it looks many times a second) starts the bank's clock again.
Test `front::supervise::tests::a_front_that_was_away_does_not_blame_the_bank_for_the_time`; verified on the late world with the front
stopped for 60 s.

### R2.5 Docker under a memory limit

The image was built from the real Dockerfile (`docker build -t pcds-qa-soak2 .`, rootless daemon); `tools/chaos/dockermem.py` ran it with
`--memory M --memory-swap M` on the late-game world and a made-up game folder (as `docker/test.sh`), 8 players on the host, 5 minutes of play, then
`pcds upgrade vA` (hot) and `pcds upgrade vB --restart` inside the container. The cgroup's own `memory.peak` and `oom_kill` were read.

| `--memory` | start (peak) | hot update | restart update | OOM kills |
|---|---|---|---|---|
| 2 GB / 1 GB / 512 MB | 2.4 to 5.9 s (119 to 138 MB) | flipped, 8 players kept, world stood 53 to 103 ms; peak 173 to 202 MB | worked in 1.9 to 2.4 s | 0 |
| 256 MB / 224 MB / 192 MB | 2.1 to 2.6 s (122 to 131 MB) | flipped (peaks 179, 191, 180 MB) | worked | 0 |
| 160 MB | 3.2 s | **refused**: "... Raise the memory limit to at least 225 MB (docker run --memory, Compose mem_limit, a Kubernetes memory limit), or restart instead (pcds upgrade --restart VERSION), which needs no extra memory and disconnects every player"; server and players untouched. (An earlier run of the same size flipped, squeezing against the limit: the check depends on what the cache holds at that moment) | worked in 3.9 s | 0 |
| 144 MB | 4.2 s | flipped at the limit (174 `memory.max` events: the page cache was reclaimed) | worked | 0 |
| 128 MB / 112 MB | 4.8 / 8.1 s | refused, the same message (232 MB for 112 MB) | worked (4.5 s at 112 MB) | 0 |

The server needs about 125 MB to start on this world and works down to a container of 112 MB, with the file cache given up. The check that refuses
a hand-over which would not fit works as built (`front/metrics.rs`), and the restart install, the way out it names, works at every size.
One harness fault on the way: the fake release feed answered without `Connection: close` and the updater met a closed socket once ("Peer disconnected");
fixed in the tool, as in the first round.

### R2.6 The final code, at twice the pace

After the work of the other branches was merged (`integrate`: ops-ready, plugin-eco, perf-final, bughunt2, mem-diet) the soak was run again on that build, with the timetable at half
an hour to the hour (`--scale 0.5`: flips every 10 minutes, a burst or a churn every half hour, the real updates after 1.5 and 3.5 hours, the SIGSTOP after 3 hours), so that the third hour, where part B changed,
was passed in 1.5 hours of wall time: 2.2 hours (8,000 s; 266 samples) of wall time, which is 4.4 timetable hours, passing the 3.2-hour mark where part B changed after 1.6 hours.

| | Result |
|---|---|
| front RSS | 8.1, 8.4, 8.7 MB in the three clock hours; Theil-Sen +0.3 MB/h, tau +0.14: **flat** (part B: 7.7 then 27 to 36) |
| bank RSS | a new process every 10 minutes starts at 19 to 40 MB and stays within 2 to 15 MB of it; per clock hour 22.9, 29.2, 33.1 MB as the world grows from 33.6k to 38.6k objects (part A's banks of the same size: 43 to 53 MB; this build's compact objects hold about half). No step to 100 MB |
| threads, descriptors | front 4 and 9, bank 5 and 4: flat |
| hand-overs | 14 (13 flips, 1 real update), all ok, no player dropped; the world stood **p50 22 ms, p95 49 ms, max 49 ms** (snapshot p50 8 ms, restore p50 15 ms): the host was quiet (busy 2.1 %, tick p99 0.77 ms, longest loop median 27 ms). The first round's 31 ms median is the same figure |
| kill -9 of the bank (2) | world back after 0.2 and 0.5 s, 8 players back after 2.2 and 2.6 s |
| `/api/metrics` | 532 scrapes, 0 bad, 114 series (13 KB: the final build has more), counters never go back |
| `pcds doctor` | 5 runs, exit 0 each |
| log | all JSON lines; 5.5 MB in 2.2 h (2.5 MB an hour), 13,700 lines an hour |
| plugins | the `net.http` plugin delivered 466 posts and was never switched off (0 fuel faults; part B: 158) |
| "the game loop was away" | 0 lines |
| incidents | two false alarms of the harness ("the unit was not running", `Result` none: systemd's moment between two states while a kill was handled; no restart counted) |
| final stop | rc 0 |

Not tested here: the SIGSTOP of the bank (hour 6 of the timetable, past the end), the second kill pair, 3.2 clock hours. A 10-minute probe of the same build with a backup every minute and `save.verifyBackups` on and off holds
the bank at 33 MB in both. What this run shows is that the final code does not step at the 3.2-hour mark of the *timetable*; it does not show that the part B step was something an old commit had and the merge removed, and
the cause of the step is **open** until a soak of the final build with a quiet hour-by-hour timetable passes 4 real hours.

### Checks of the final tree

On the merge of the second round with `integrate` (the commits of branch `soak-long`):

| Check | Result |
|---|---|
| `cargo test --release --locked --no-fail-fast` with the tables (format 4) | 800 unit tests, `layers` 1, `cli` 23, `hot_update` 25 (3 ignored), `admin_golden` 4: all ok |
| the same as CI runs it (`PCDS_TEST_DATA=none PCDS_GAME_DIR=none`) | all ok |
| `cargo clippy --release --locked` | 0 warnings |
| `map.py check` | 732 groups, 208 RPCs, 165 components, 0 problems |
| bench fingerprint, bench world, 4 players, 300 s | walking `15c299165b6b3eb0` as before, handoff `EQUAL`; acting every 200 ms **`eab3e8e67a9290d9`** (was `4e119fd2aacba604`: it changes on purpose with the pick-up fix, the scripted players' failed pick-ups left objects whose ids pushed the next ones up by one), handoff `EQUAL` |
| the watchdog test and the plugin tests | `front::supervise` 3 ok; `plugins::` 143 ok (the signed example is current) |

### What was not covered in round 2

* **One 12 h soak on one build.** What ran is part A (v0.8.0, 7.6 h), part B (an early build of `integrate`, 8.0 h) and the final build at twice the pace (2.2 h of the 3.5 h planned: it was ended to stay near the two hours the owner gave for finishing); no single 12-hour stretch. The rate limits
  of the session that drove them cost no run (the soaks are units of their own), but they cost the 12 h plan: the first soak was ended at 7.6 h to make room for part B.
* **A quiet host.** Part A's first hours and every tail latency are from a machine with a load of 25 to 125. The virtual-clock numbers are not.
* **The late-game world for 60 days.** It plays a day in 1.3 to 1.5 hours; 2.75 days ran. The 60 and 400 days ran on the 2,565-object bench world (which has machines, plants, animals, logistics and a day and night cycle, but 13 times fewer objects).
* **A real game client** through the wrap of the wire tick at 828 days, and through a front that was frozen.
* **S3 and the webhook "for hours".** The outages were 10 and 5 minutes in the soaks and 10 minutes a phase (50 and 75 minutes) in the pressure runs; the S3 uploader makes one attempt per backup and keeps no queue, which is why the length does not matter, but a failure that only shows after hours of retries was not looked for.
* **A network blackhole of the UDP port** (iptables needs root): the frozen front stood in for it. A slow or lossy client network.
* **Chaos in unit mode with these faults at the same time.** The round-1 chaos tool was not run again.
* **The cause of part B's step in memory** (R2.2).
