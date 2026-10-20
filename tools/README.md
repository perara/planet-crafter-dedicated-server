# Tools

Scripts for testing, measuring and checking the server. None of them is part of the program
or of the release, and none of them runs in CI (CI only checks that they parse). They take their
folders, ports and programs as arguments; `--help` (or the comment at the top of each file) says
what it needs, how to run it, and what a pass looks like. Test servers use ports 8420 to 8439 by
default and never the default 7777 and 7782, and a tool stops only the processes it started.

How the layers fit together, and which command runs what, is in
[docs/testing.md](../docs/testing.md). Nothing here contains game data or a save: tools that
need a save or the game's tables take them as arguments (the tables come from your own copy of the
game, `pcds read-game`).

## Older tools

| Tool | What |
|---|---|
| `flip-gate.sh` | The flip gate (`make gate`): the last release hot-updated into this build. |
| `grow-world.py` | Grows a save to about N world objects, for `bench::flipbench`. |
| `probe_diff.py` | Compares two `pcds probe` transcripts (the real game's and this server's). |
| `plugin-engine-bench/` | The engine comparison behind docs/plugins.md (not part of the build; its README). |

## chaos/: breaking a running server on purpose

Each starts a private server on a copy of a save (never the real one), plays scripted players
against it, does the nasty thing, and checks afterwards that the server is alive, the save is
sound and memory did not run away. Exit status 0 is a pass; the last line is JSON with the checks.
`lib.py` is their shared code (start a server, talk to the admin API, read `/proc`, check a save).
Python 3.9+, standard library only.

| Tool | What |
|---|---|
| `chaos.py` | Random faults (kill -9 and SIGSTOP of the bank, front or `pcds run`, the bank watchdog (a 125 s stop must replace the bank, a 10 s one must not), flips and real updates from a local release feed, world stop, start and switch, save storms, a read-only folder, bad settings, chat hooks and S3 against a fake endpoint, and plugin churn all the time: install, approve, disable, remove, damaged files, a plugin that traps in every event) with the invariants checked after each. `--unit pcds-qa-NAME` runs the server as `pcds run` under a transient systemd user unit (the service file's restart rules) instead of supervising it itself. |
| `soak.py` | Players for hours; a sample every 30 s of CPU, memory, tick times and file sizes, to find leaks and drift. |
| `churn.py` | Sessions that join and leave badly (vanish, never become a player, only handshake), driven by the ignored test `load::tests::churn`. |
| `stress.py` | Six abuse scenarios on a fresh server each: `burst` (16 players acting every 200 down to 1 ms), `bigchat` (900 KB chat lines), `full` (40 rejoining players on a 16-player server), `stop` (the bank stopped for 30 and 120 s), `queuecap` (64 players sending 900 KB lines while the bank stands: the front drops the client with the most waiting at 256 MiB, and its memory stays bounded) and `slowclient` (four players stopped for 40 s: their sessions time out, the others do not notice). |
| `limits.py` | The front's connection limits, with raw datagrams. |
| `fuzz.py` | Garbage datagrams at the front's UDP port. |
| `diskfull.py`, `diskfull.sh` | What the server does when the world's disk fills (the shell wrapper makes a small private tmpfs with `unshare`, no root). |

## perf/: what the server costs

The numbers of a build, how to read them and the budgets that hold them are in [docs/performance.md](../docs/performance.md).

| Tool | What |
|---|---|
| `bench.py` | The game loop's cost per tick for several player counts, best of N (drives the ignored `bench::bench`). |
| `loadrun.py` | What a real server (front and bank) costs idle and under `pcds load` players: CPU, instructions, context switches, memory, round trips, and what went over the wire (datagrams, bytes, acknowledgements). A save on a DLC planet: `PCDS_TOOLS_RULES='{"world":{"dlcPlanets":["Humble"]}}'` (merged over the rules of every tool). |
| `profile.py`, `pstack.py` | `perf record` of the front and bank under load, a save or idle; and the call graph read as plain tables. |
| `memory.py` | The front's and bank's RSS, PSS and anonymous memory over time, with saves. |
| `scale.py` | How the server behaves on worlds of growing size (start, save, memory). |
| `ab.py`, `abtest.sh` | Two builds (or two disks) side by side under the same load; one test of two builds alternately, N times each. |
| `adminpoll.py` | Watch or hammer the admin API of a running server (for stall hunting). |
| `flipbench.sh` | How long the world stands still when a hot update hands it over (`bench::flipbench`, or the whole flip on a real server). |
| `startbench.sh` | The CPU cost of a start on a world, in user instructions (they do not depend on how busy the host is). |

## parity/: the sweep against the real game, and comparing runs

The sweep itself is `server/tests/parity/sweep.sh` (docs/testing.md, "Parity with the game"). These
run around it. Those that start the reference need the game, Wine and the extractor plugin.

| Tool | What |
|---|---|
| `ref-host.sh start\|status\|save\|stop` | Start, check, save and stop the Wine reference host. |
| `ours-host.sh start\|stop` | This server as the sweep runs it, on a test port, with autosave off. |
| `replay-ours.sh` | Replays scripts on this build in parallel workers against a stored reference run (after a fix). |
| `rediff.sh` | Compares stored transcripts and saves again, with no host; with a script folder it also classifies the differences. A pass: no `unclassified`, no class `a`. |
| `noise-rerun.sh` | Runs the reference a second time only for the scripts that differ, to tell the game's own randomness from a bug. |
| `bench-compare.sh` | For each commit: build, run the bench with a fixed seed, dump every message and the save; compare neighbouring dumps (bisecting a change in behaviour). |
| `dumpcmp.py`, `diffcmp.py` | Compare two bench dumps; compare the differences of two sweep runs. |
| `transcript.py` | Look into a probe transcript: steps, history of one object, deconstructs, a series. |
| `runs-compare.py` | Who holds what across end saves, and whether this server falls inside the game's own spread. |
| `mk-demand-save.py`, `gen-watch-script.py` | Build a start save for the drone service-order test; write a probe script that re-reads inventories. |

## dev/: for working on the server

| Tool | What |
|---|---|
| `verify.sh` | The integrator's summary: the full suite with tables, the CI-style run, clippy, `map.py check`, and optionally the bench fingerprint and handoff equality. |
| `probe-smoke.sh` | A new world, a server on a test port, the world probe, stop. A pass: probe and server exit 0. |
| `hostile-fuzz.sh` | The hostile-RPC test over many seeds and rounds. |
| `tables-check.sh` | `pcds read-game` on a game folder, then `pcds tables-diff` against a folder you trust. |
| `make-recovery-snapshot.py` | A snapshot fixture from an older release (flip, kill the live bank, stop the front). |
| `merge-coverage.py` | Resolves merge conflicts in `server/src/coverage.txt` by uniting the test lists. |
| `stable-hash.py` | The game's string hash (`Prime` is -1140328421). |
| `unity-header.py`, `dotnet-strings.py`, `il-dump.py`, `clean-cs.py`, `client-rpcs.py` | Reverse-engineering helpers over the game's files (Unity header, .NET string heap, IL of a type, decompiled C# without the plumbing, client RPCs). `il-dump.py` needs `dnfile` and `dncil` (pip). |

## What is not here

Scripts that only made sense for one night's work (wrappers around one commit, built-in paths of one
machine, the old bash installer, save surgery on one player's save) were not kept. Probe scripts
generated from a player's late-game save are not kept either: the save has no licence and carries
a real account number. The sweep generates its scripts from the tables (`sweep.py gen`).
