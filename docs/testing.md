# Testing

Every kind of test and check this project has, what it needs, and how to run it. The short
version: `make test` runs all the automatic ones; the rest are tools you run by hand when you
change the part they watch. Where a layer needs the game's tables (which cannot be in the
repository), it says so, and CI skips it.

Commands are run from the repository root unless a `cd` is shown. On a shared machine put
`agent-run` or `nice` in front of the heavy ones (`make` does when `agent-run` exists).

| Layer | Needs tables | In CI | Run |
|---|---|---|---|
| [Unit tests](#unit-tests) | most of them | yes, without the tables | `make test` or `cargo test --release` in `server/` |
| [End-to-end tests](#end-to-end-tests-cli-hot-update-admin-golden) (`cli`, `hot_update`, `admin_golden`) | all but `cli` | yes, without the tables | with the unit tests |
| [Structure and docs tests](#structure-and-documentation-tests) | no | yes | with the unit tests |
| [Plugin tests](#plugins) | no | yes | see below |
| [Fuzzing](#fuzzing) | some targets | no | `cargo fuzz` |
| [Sanitizers and Miri](#sanitizers-and-miri) | no | no | nightly toolchain |
| [Parity sweep](#parity-with-the-game-the-sweep) | yes, and the game | no | `server/tests/parity/sweep.sh` |
| [Completeness map](#completeness-map) | `check`: no | `check` only | `map.py check` |
| [Benchmarks](#benchmarks-and-fingerprints) | yes, and a save | no | `make bench` and `cargo test ... -- --ignored` |
| [Efficiency budgets](#efficiency-budgets) | most of them | yes, without the tables (the transport's) | with the unit tests |
| [Load, chaos, soak and churn](#load-chaos-soak-and-churn) | yes, and a built server | no | `tools/chaos`, `tools/perf` |
| [Docker](#docker) | part 2 | no | `make docker-test` |
| [The flip gate](#the-flip-gate-and-the-release-checklist) | yes | no | `make gate` |

## Unit tests

`cargo test --release` in `server/` (the `release` profile is the one the server ships with, and the
timing-sensitive tests assume it; a debug build is much slower). `make test` builds first and runs the
tests under a 6 GB address-space limit. About 650 tests live next to the code they test, in the
`#[cfg(test)]` part of each file or in a `tests.rs` beside it. `src/testkit.rs` and `src/scenario.rs`
build a game and play it with the messages a client would send.

**With the game tables.** Most of the game logic tests need the tables the server reads from the
game (`netdata.json`, `gamedata.json`, `rpcs.json`, `scenes/`...). They are extracted from your own copy
of the game (`pcds read-game`, which `pcds setup` runs) into `run/data/<game version>/`, and the tests
look there by default. Another folder: `PCDS_TEST_DATA=/path/to/tables`. Tables that are not found fail
the test run, with a message saying so, rather than skipping silently.

**Without them.** `PCDS_TEST_DATA=none` skips every test that needs tables (they return at once and
count as passed) and runs the rest: the wire, transport, save, config, rules, access, plugin host,
updater, CLI and structure tests. CI runs this way:

```bash
cd server
PCDS_TEST_DATA=none PCDS_GAME_DIR=none cargo test --release --locked
```

`PCDS_GAME_DIR=none` points every lookup of the game's folder at a folder that does not exist, so
no test can find a Steam installation on the machine it runs on.

A few more conventions:

- `PCDS_RNG_SEED=N` fixes the game's random numbers (test builds only). Tests that depend on
  random ids seed themselves; the benchmarks set it.
- `src/coverage.txt` names the tests of every server RPC and every module. A test fails when an
  RPC or a module is missing from it, or when it names a test that no longer exists. Add your new
  test's name to the line of its module.
- Tests that print timings (`#[ignore]`d ones) are not tests of correctness: run them as shown in
  [Benchmarks](#benchmarks-and-fingerprints) and the later sections.
- One test or one module: `cargo test --release units::` or `cargo test --release --bin pcds bank::`.
- The tests set up their own folders under the system temp directory and use their own ports.

### What the test environment variables are

Variables that only tests and tools read are listed in [env.md](env.md) under "Tests and
tools". The program itself never reads them (a test checks that every `PCDS_` name in the source
is either a setting, a process variable or in that list, and that `env.md` is the generated text of
the tables in `src/config.rs`: change a table, then run `cargo test --release config::tests::write_env_doc -- --ignored`).

## End-to-end tests: cli, hot_update, admin_golden

These are in `server/tests/` and start the `pcds` program that cargo just built, as a user would.

- `tests/cli.rs`: the command line with a folder of its own (`PCDS_HOME`) and a stand-in `systemctl` first
  on the `PATH`: setup, install, start, stop, upgrade and rollback with a local release feed, worlds,
  rules, plugin commands. It needs no tables and no systemd, so it runs in CI.
- `tests/hot_update.rs`: a front and two banks on a new world, with scripted players playing through
  flips; a flip whose new bank dies, a flip refused at its first message, a bank killed outside a
  flip, the updater against a release server on 127.0.0.1 (a good release, a wrong checksum, another
  protocol) and a rollback. Needs tables. One test, `a_hot_update_from_the_release_before_works`, is
  ignored: it needs the last release's download, and is what `make gate` runs.
- `tests/admin_golden.rs`: the admin API route by route against `tests/golden/admin-api.txt` (the status and
  the shape of each answer). If an answer changes on purpose, delete the file and run the test: it
  writes the new one and fails once; commit the new file with the change. Needs tables.

`cargo test --release --test hot_update` runs one of them. They take ports from the operating
system, so they can run beside each other and beside a real server.

## Structure and documentation tests

These need no tables and keep the repository honest:

- `tests/layers.rs` (also `scripts/check-layers`): every module is in a layer in `server/layers.txt` and
  uses only what its layer may ([architecture.md](architecture.md)). A forbidden use fails; a debt in the
  list that no longer exists fails too, so the list only shrinks.
- `coverage::every_server_rpc_and_module_is_in_the_coverage_map`: see `src/coverage.txt` above.
- `config::tests::*`: the settings and rules schema (`KNOBS`) matches `rules.rs`, `server.env`
  checks, the README's tables of settings and rules are the schema's, the operator-facing variable
  budget (`OPERATOR_ENV_BUDGET`) holds, every `PCDS_` name anywhere in the source, scripts and images
  is known, and `docs/env.md` equals what the tables generate.
- `front::admin_routes::tests::docs_admin_api_is_written_from_the_table`: `docs/admin-api.md` is the route table.
- `python3 tests/parity/completeness/map.py check` (in `server/`): the completeness files agree with
  `coverage.txt`, the sweep matrix and `docs/COMPLETENESS.md`. CI runs it.
- `tests/dotnet_sort/`: the game's sort order for equal keys, from `reference.py` and checked against a
  real Mono (`Check.cs`). `python3 server/tests/dotnet_sort/reference.py check` fails when `vectors.txt` is stale.

Other checks that run without the tables: `make lint` (clippy with no warnings allowed, and shellcheck of
the installer and Docker scripts). Clippy must stay at zero warnings.

## Plugins

- `cd plugin-api && cargo test --locked`: the library plugins are built with (codec, mock host).
- `cd examples/plugins/hello-stats && cargo test --locked`: the Rust example, on the PC, against a mock host.
- `plugins::*` unit tests in the server: the host (limits, fuel, memory, approvals, state through a
  hot update), signing (`plugins::sign_tests`, including `the_signed_example_is_current`, which fails when a
  `plugin.sig` does not match its `plugin.wasm`), the network capability (`plugins::net_tests`), the
  admin routes and the game events (`plugins::game_tests`).
- `scripts/build-plugin-examples --check`: the committed `plugin.wasm` files are what their source builds. The
  bytes do not depend on where the checkout is. They do depend on the compiler's version, so run it with
  the toolchain the files were built with (CI only builds them). After a change to a plugin or to
  `plugin-api`: `scripts/build-plugin-examples`, then `scripts/sign-plugin-examples server/target/release/pcds`
  for the signed one.
- `python3 examples/plugins/webhook-relay/relay.py --self-test`: the relay example.
- `make plugin-bench` (ignored tests in `plugins::cost`): what a plugin call, an event and a tick cost.
  Needs tables; `PCDS_BENCH_SAVE` adds the cost on a real world. The engine comparison behind
  [plugins.md](plugins.md) is in `tools/plugin-engine-bench/` (its README).

## Fuzzing

`server/fuzz/` fuzzes the parsers and the game with libFuzzer. The server is one binary crate, so the
fuzz binary is the same `main.rs` built with `--cfg fuzzing`; `PCDS_FUZZ_TARGET` picks the target. Nothing
changes in a normal build.

```bash
cargo install cargo-fuzz                      # once; needs a nightly toolchain
cd server
python3 fuzz/seeds.py                         # (re)writes the small seed corpus (no game data)
PCDS_FUZZ_TARGET=wire cargo fuzz run pcds_fuzz fuzz/corpus/wire
```

Targets: `wire utp rpcspec transform ipc config chathooks updater access rules save handoff game front`.
The targets `rpcspec`, `handoff` and `game` read the game tables from `PCDS_FUZZ_DATA` (default:
`run/data/2.103` of the checkout). `handoffsweep` and `savesweep` are not fuzzers: each puts awkward
numbers and strings, one at a time, into a real snapshot or save (`PCDS_FUZZ_SWEEP_FILE`, `_STEP`, `_OFFSET`;
pass `-timeout=0`). The `game` target runs 40 inputs on one game, takes a snapshot and restores it every 20,
and on a panic writes the inputs to a history file (`PCDS_FUZZ_REPLAY` runs it again, `PCDS_FUZZ_TRACE`
prints each input). Build for what production runs, as well as the default, with
`RUSTFLAGS="-C overflow-checks=off -C debug-assertions=off"`: the default turns checks on, which finds
wrapping arithmetic too.

**Replaying a corpus without libFuzzer** (a regression run, and the way to put it under Miri):

```bash
cd server
RUSTFLAGS="--cfg pcds_corpus" PCDS_FUZZ_TARGET=wire PCDS_CORPUS=fuzz/corpus/wire \
  cargo test replay_corpus
```

`PCDS_CORPUS_LIMIT=N` takes the first N files. A crash that fuzzing finds becomes a unit test of its
own; the corpus only replays seeds. The RPC fuzz test that runs with the unit tests
(`PCDS_FUZZ_ROUNDS`, `PCDS_FUZZ_SEED`) throws hostile client messages at a game; `tools/dev/hostile-fuzz.sh`
runs it over many seeds. `tools/chaos/fuzz.py` fuzzes a running server over the network.

## Sanitizers and Miri

None of these are in CI; they were run for the audits ([audits.md](audits.md)) and are worth repeating after a
change to the transport, the save code or anything `unsafe`. All need a nightly toolchain.

- **Miri** (`cargo +nightly miri test`) on the pure unit tests (wire, geometry, transform, protocol, model,
  rpcspec parse, save text, rules, config, access) and on corpus replays (above, a few dozen files per
  target: a directory of thousands is slow under Miri). It cannot run what uses sockets (utp, ipc, front),
  `ring`, fsync, mmap or memfd. Result at v0.6.1: no undefined behaviour.
- **AddressSanitizer** (`RUSTFLAGS=-Zsanitizer=address` with `--target x86_64-unknown-linux-gnu`) on the whole unit
  suite and on `cli` and `hot_update`. Setting `RUSTFLAGS` drops the `-rpath $ORIGIN` that
  `.cargo/config.toml` adds, so a bank started with a clean environment cannot find `libsteam_api.so`:
  add `-C link-arg=-Wl,-rpath,$ORIGIN` to your flags. `hot_update` has timing assertions that a
  sanitizer's slowdown can break (a failed craft round trip, not a memory error).
- **ThreadSanitizer** (`-Zsanitizer=thread -Zbuild-std`) on the unit suite: the admin threads, the chat-hook
  worker, S3 uploads and concurrent save writes. `hot_update` was not run under it.

## Parity with the game: the sweep

`server/tests/parity/` checks the game logic against the real game host (the game itself, headless
under Wine and Xvfb, with the extractor plugin) from the same save. The README's "Parity with the
game" explains how to build the reference host. Nothing here is in CI or in `cargo test`: it needs the
game and Wine.

- `run.sh REF_ADDR OURS_ADDR DATA_DIR START_SAVE SCRIPT.json OUT_DIR` runs one probe script against
  both hosts. `parity.py` decodes every network variable with the tables and prints what differs;
  `savediff.py START REF OURS` compares the saves field by field.
- `scripts/*.json` are the hand-written scripts, one per mechanic family. `EXPECTED.md` lists what
  still differs on purpose (the host's own player, time, the game's randomness, the safety rules).
  Anything else is a bug.
- `sweep.py gen` writes the exhaustive scripts from the tables; `sweep.sh both|ours|ref2 OUT_DIR SCRIPT...`
  runs them:
    - `both`: the reference and this server at once; one transcript, one diff, one save diff per script.
    - `ours`: only this server, against the transcripts and saves already in `OUT_DIR`. This is the mode
      after a fix: no game is started.
    - `ref2`: the reference a second time, and the difference between the two runs of the game itself
      (its randomness and timing: the `.noise` files), so that a difference that is noise is not a bug.
  `sweep.py report` classifies every difference (`sweep_classes.json`) and writes `SWEEP.matrix.md`;
  `SWEEP.md` has the result and the bugs found. Environment: `REF_DIR`, `DATA_DIR`, `START` (from
  `mkstart.py`, never a different one), `PCDS`, and the ports `REF_PORT`, `REF_API`, `OURS_PORT`.
- `tools/parity/` has the drivers around it: replaying many scripts in parallel workers, re-comparing
  stored runs, re-running the reference only where a difference needs a noise baseline, and comparing
  bench dumps across commits (see [tools/README.md](../tools/README.md)).

The saves a sweep starts from are derived from a real player's save; keep them out of the repository.

## Completeness map

`server/tests/parity/completeness/` maps every group, every server RPC and every component type of the
game against this server's code, its unit tests, the sweep and a late-game save. The result is
`docs/COMPLETENESS.md`, `MATRIX.md` and the CSV files. `map.py check` (in `server/`) needs no game data and
fails when the files disagree with each other or with the code, for instance when a test named in
`coverage.txt` was removed; CI runs it. To measure again with the game's data:

```bash
cd server
python3 tests/parity/completeness/map.py gen DATA_DIR [--cs DECOMPILED_CS_DIR] [--census census.json]
python3 tests/parity/completeness/map.py numbers       # the headline numbers COMPLETENESS.md quotes
```

`census.py` counts the groups of a save (names and counts only) into `census.json`; `loaddiff.py` and
`aggdiff.py` compare what a load and a save change, and what two runs of a save aggregate to.

## Benchmarks and fingerprints

`src/bench.rs` has the benchmarks. They are `#[ignore]`d tests (they need tables and a real save, and a
timing is no test). All are run as

```bash
PCDS_BENCH_SAVE=run/worlds/world.json PCDS_RNG_SEED=1 \
  cargo test --release bench::NAME -- --ignored --nocapture
```

| Test | What it does | Variables |
|---|---|---|
| `bench::bench` (`make bench SAVE=... PLAYERS=4`) | The tick on a real save with a fixed clock and seed, 4 scripted players by default: microseconds per tick (mean, p50, p99), user instructions per tick (where the kernel lets the process count them), allocations per tick, and a **fingerprint** of every message the server sends and every byte it saves. | `PCDS_BENCH_SECONDS` (300), `PCDS_BENCH_PLAYERS`, `PCDS_BENCH_ACTION_MS` (players act that often; default they only walk), `PCDS_BENCH_START_DAYS` (the game's clock starts that many days in: the save hash must not depend on it), `PCDS_BENCH_DUMP` (every message and the final save, to a file, to compare two runs), `PCDS_BENCH_SECTIONS=1` (time per part of the tick, and the sections of the slowest ticks), `PCDS_BENCH_ALLOC_SITES=N` (the places in the code that allocate in the tick) |
| `bench::handoff` | The same run straight, and with the game snapshotted and rebuilt from the snapshot every N seconds (what a hot update does). The two fingerprints must be `EQUAL`. | the same, and `PCDS_BENCH_HANDOFF` |
| `bench::restart` | An idle world run straight and with the process restarted from its save at half time (the clock starts at 0 again); prints the units' levels of both. What differs is what the save does not hold. | the same |
| `bench::scale` | Load, save and memory of a world of any size: the time of each step of a start and of an autosave, and the byte accounting of docs/performance.md: the heap by part of the game (world objects, network objects, machines, inventories...) and of the tables, how many world objects hold only an id and a group, the machines' census, the process's peak, the anonymous part of its RSS after a trim and what malloc holds free. | `PCDS_BENCH_SAVE` |
| `bench::tables_memory` | The memory each of the game tables takes. | `PCDS_TEST_DATA` |
| `bench::flipbench` | The stand of a hot-update flip on worlds of several sizes, phase by phase (p50, p95, max). | `PCDS_FLIPBENCH_SAVES=a.json,b.json`, `PCDS_FLIPBENCH_N`, `PCDS_FLIPBENCH_PLAYERS`. `tools/grow-world.py SAVE OUT N` grows a save to about N world objects. |
| `bench::dirtybench` | How much of a big world changes while a pre-copy would run (entries and 2048-entry chunks that differ after 0.25, 1 and 5 s). | `PCDS_FLIPBENCH_*` |

**Fingerprints.** A change that should not alter what the game does must leave the fingerprint as it
was, and the handoff run must stay `EQUAL`. They depend on the save, so compare with a run of the
build before your change on the same save. The maintainer's bench world (a mid-game save, not in the
repository) gives, with `PCDS_RNG_SEED=1`, 4 players and 300 s:

| Players | Fingerprint |
|---|---|
| only walk (`PCDS_BENCH_ACTION_MS` unset) | `15c299165b6b3eb0` |
| act every 200 ms (`PCDS_BENCH_ACTION_MS=200`) | `4e119fd2aacba604` |

A change that alters the game on purpose changes the fingerprint; say so in the commit and record the
new one in the places that quote it. The ages (`PCDS_BENCH_START_DAYS`) give a fingerprint of their
own each, but the printed save hash must be the same for ages that only differ in the clock's
resolution ([clock.md](clock.md)).

`pcds load` and `pcds probe` (the scripted clients) and `GET /api/status` (`busyPercent`,
`longestLoopMicros`) are the live equivalents; see [Load, chaos, soak and churn](#load-chaos-soak-and-churn).

Times on a shared machine are not the code's: compare **instructions** (the bench prints them;
`perf stat -e instructions:u` for a process) and allocations. docs/performance.md has the numbers of
a build, how to profile and the budgets below.

Ignored soak tests in the same spirit: `soak_growth` (`PCDS_SOAK_SECONDS`, `PCDS_SOAK_STEP`,
`PCDS_SOAK_REPORT`: what grows over a long game), `drone_farm_runs_for_hours` (`PCDS_FARM_STATIONS`,
`_DRONES`, `_PAIRS`, `_SECONDS`: stations, drones and a supply and demand that never end; prints per game
minute the worst tick, the sizes of what could grow and the memory) and `perf_logistics_and_destroy`
(`PCDS_REVIEW_STATIONS`, `PCDS_REVIEW_SECONDS`: the cost of the logistics search in a world with tens of
thousands of stored items). Run each with `cargo test --release NAME -- --ignored --nocapture`.

## Efficiency budgets

Tests that fail when the server gets dearer (docs/performance.md has the table of budgets and what
they were when set). They count what does not depend on the machine's load: allocations, bytes and
user instructions (the kernel's counter, `perf_event_open`; where the process may not count, the
instruction budgets say so and are skipped, the allocation and byte budgets still run).

- `bench::budget::*`: a world of about 0, 400 and 1,600 buildings that anyone can build (containers
  that supply and demand, water collectors, autocrafters, a drone station) with 0 and 4 players:
  allocations and instructions per tick, the growth of the tick with the world, the bytes per world
  object of a hot update's snapshot, the save and a join, the instructions of a snapshot and its
  restore, and the memory the world's parts keep (a world object, a network object, a machine, an
  inventory: the bytes malloc holds for each, counted by the test allocator). `cargo test --release bench::budget::numbers -- --ignored --nocapture` prints the
  measured numbers to set a budget from.
- `utp::tests::polling_idle_sessions_and_sending_a_batch_allocate_a_bounded_number_of_times`:
  allocations of the transport; needs no tables, CI runs it.
- `tests/hot_update.rs`, `an_idle_server_wakes_up_within_its_budget`: the wake-ups a second of an idle
  front and bank (the kernel's count of voluntary context switches).

A budget is about one and a half times what the build measured. A failure says which and by how much;
if the cost is the point of the change, raise the budget in the test and in docs/performance.md in
the same commit.

## Load, chaos, soak and churn

Against a running server (a scratch one on a loopback address, never your real one), with the scripted
clients the program has:

- `pcds load HOST:PORT DATA_DIR [PLAYERS] [SECONDS] [ACTION_MS] [NAME]`: players that walk, craft, drop and
  pick up, build, eat, drink, mine, chat and sort; prints the round trip of every call the server
  answers, as percentiles, and what the players received. Each player connects from its own 127.0.0.N
  address, so HOST must be a loopback address.
- `pcds probe HOST:PORT DATA_DIR SCRIPT.json`: one scripted player, prints everything the server sends.
  `tools/probe_diff.py` compares two such runs.
- Two ignored tests drive the same bots through behaviour a real network has and the load test does not:
  `load::tests::churn` (sessions that play and leave cleanly, play and vanish, are approved and never
  become players, only handshake, or join and leave at once) and `load::tests::rejoin` (players that join
  again when dropped, as the game client does). They print one JSON line per session.

  ```bash
  PCDS_LOAD_SERVER=127.0.0.1:8421 PCDS_TEST_DATA=run/data/2.103 PCDS_LOAD_SECONDS=60 \
    cargo test --release load::tests::churn -- --ignored --nocapture
  ```

  Variables: `PCDS_LOAD_SERVER` (required), `PCDS_LOAD_PLAYERS`, `PCDS_LOAD_SECONDS`, `PCDS_LOAD_SEED`,
  `PCDS_LOAD_REUSE_NAMES`, `PCDS_LOAD_CHAT_BYTES`. The bots receive worlds of any size (a client takes
  batches up to 256 MiB; the server's own limit on the batches it receives is 1 MiB), so `pcds load` joins a 200,000-object world.

The scripts that start a scratch server, run these and check the server afterwards are in
`tools/chaos/` (chaos, soak, churn, stress, stall, limits, disk full) and `tools/perf/` (load runs
with CPU and memory sampling, profiles, A/B runs): see [tools/README.md](../tools/README.md) for what
each does and what a pass looks like. None of them is in CI.

## Docker

`make docker-test` (`docker/test.sh [IMAGE]`) runs the hot update inside the container, end to end:
part 1 needs no game (the image's updater installs a release on a read-only root, `pcds status`, the start
without a game says what to do); part 2 (with `PCDS_TEST_DATA=DIR`) runs a server on a made-up game
folder, three scripted players, a hot update from a local release feed, `docker restart` and a rollback
in place. It works on rootless Docker too. Details and measurements: [docker.md](docker.md).

The container in part 2 runs with the hardening of [hardening.md](hardening.md) (read-only root, `no-new-privileges`, all
capabilities dropped but the five the entrypoint needs, a process limit), and part 1 checks the image's health check is
`pcds health`.

## Operations checks by hand

What the operator documents rely on, and the tools that checked them (none of them is in CI, because each needs Docker or a
live server; the unit tests keep the files and the output in step):

* **Metrics**: `curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:7782/api/metrics | docker run --rm -i --entrypoint promtool
  prom/prometheus check metrics`; the alert rules and the scrape configuration of docs/monitoring.md with `promtool check rules` and
  `check config`; a Prometheus container scraping a real server, and every `expr` of docs/grafana/pcds.json run against it with the
  query API. `front::prometheus::tests` parses what `render` writes with a strict reader of the format and keeps the metric table of
  docs/monitoring.md, the alert rules and the dashboard to metrics that exist.
* **The sandbox**: `systemd-analyze --user security pcds.service` (the score) and `grep -E 'NoNewPrivs|Seccomp|CapBnd'
  /proc/PID/status`, then a hot update, a chat hook of each language you use and `steamcmd` under the unit
  ([hardening.md](hardening.md) lists what was tried).
* **Proxies**: `docker run caddy caddy validate` and `docker run nginx nginx -t` on the configurations of
  docs/reverse-proxy.md, then the curl checks listed there against a running server.
* **Kubernetes**: `docker run ghcr.io/yannh/kubeconform -strict -kubernetes-version 1.31.0 docs/kubernetes.yaml`.
* **Disaster recovery**: the rehearsal at the end of [disaster-recovery.md](disaster-recovery.md).

## The flip gate and the release checklist

`make gate` (`tools/flip-gate.sh`) hot-updates the last published release into this build on a world with
one of every building and six players. It is by hand because it needs the tables and a download. The
release checklist, with the gate's meaning, is in
[hot-update.md](hot-update.md) (under "As built": the paragraphs "The flip gate" and "Release checklist"): follow that list before tagging; this file does not repeat it.

The last full run of all of this on a build, with the numbers and what it found, is [soak-report.md](soak-report.md).
