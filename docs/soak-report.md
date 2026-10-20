# Soak report: the build that follows 0.7.0

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

PLUGIN_FAULT_RESULT

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

CHECKS_RESULT

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
