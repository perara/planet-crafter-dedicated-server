# Environment variables

Generated from the tables in `server/src/config.rs` by `cargo test config::tests::write_env_doc -- --ignored`; a test fails when it is out of date. Do not edit.

An operator meets 12 variables, no more (12 is the budget, and a test keeps it): what the server must know before it can read any file. Everything else, gameplay and tuning alike, is a rule: `rules.json` and `access.json`, changed live through `pcds rules`, `POST /api/config` and the web page (README, "Server rules").

## Settings (run/server.env)

| Variable | Default | What |
|---|---|---|
| `PCDS_LISTEN` | `0.0.0.0:7777` | Direct-IP address (UDP) players join; off disables it. |
| `PCDS_STEAM` | `on` | Invite codes through Steam; off disables them. |
| `PCDS_STEAM_TOKEN` | unset | A Steam game server login token (app 1284190): keeps the invite code the same across restarts. |
| `PCDS_SESSION` | `4242` | The last four digits of the invite code. |
| `PCDS_ADMIN` | `127.0.0.1:7782` | The admin API and web page's address; off disables them. |
| `PCDS_ADMIN_TOKEN` | unset | The admin API's bearer token (pcds token). |
| `PCDS_WORLD` | `world` | The world pcds starts: worlds/<name>.json (pcds world use NAME). |
| `PCDS_RELEASES` | unset | Where pcds upgrade and update.auto look up releases; unset: GitHub's API for this project. A mirror, or a test's server (plain http only for 127.0.0.1). |

## Where the installation is

Set by the image or the unit that starts the server, or in the shell of every `pcds` command.

| Variable | What |
|---|---|
| `PCDS_HOME` | The folder the installation keeps everything in: worlds/, server.env, rules.json, access.json, data/ (the game tables) and bin/ (the banks). Default: run/ of the release or checkout folder; a second server on the machine gets its own. |
| `PCDS_GAME_DIR` | The game's folder (the one with Planet Crafter.exe). Default: the one used last (run/game-dir), else the Steam library that lists the game; --game DIR on the commands that need it. |
| `PCDS_CONTAINER` | Set by the Docker image: no systemd, the server is the container's own process, the admin address may be public inside it. |
| `PCDS_UNIT` | The systemd unit's name (default pcds): set it, with PCDS_HOME, for a second server on the same machine. The units pcds writes carry it. |

## Between this program's own processes

Never set by hand.

| Variable | What |
|---|---|
| `PCDS_IPC_FD` | The front gives each bank its socket at this descriptor (always 3). Never set by hand. |

## Developer hooks

For the end-to-end tests and the load and probe tools; the hooks are in the next table.

| Variable | What |
|---|---|
| `PCDS_DEV` | Developer hooks, comma-separated (the table below). Tests and tools set it; never an installation. |

`PCDS_DEV` takes these, comma-separated; one that does not exist stops the start.

| Hook | What |
|---|---|
| `server-rocket-flight` | Experimental server-driven rocket visual, temporary flare and craft sound; skips local rocket physics. Test clients only; not an installation setting. |
| `flip-verify` | A bank that restores a snapshot checks its state equals the snapshot's (every flip). |
| `test-faults` | POST /api/update takes failForTest, to make the next bank fail on purpose. |
| `trust-loopback` | Believe the SteamID a direct-IP client from 127.0.0.1 claims. Never behind a UDP relay, where every player comes from 127.0.0.1. |
| `bank-fail=WHAT` | The front sets it for a bank (a test's failForTest): WHAT is restore (dies on RESTORE), protocol (says another protocol), slow-hello (holds its HELLO 4 s), slow-ready (holds its READY 2 s) or die-after-ready (ends 1 s after READY). |
| `flip-threads=N` | A flip writes and reads the snapshot's parts on N threads (default: the cores the process may use, at most 8). |
| `check-seconds=N` | The automatic update check every N seconds, not every update.checkHours hours. |

## Tests and tools

Read by test builds and tools, never by an installation. Set by hand for the benchmark and the tests, or by a test for the process it starts.

| Variable | What |
|---|---|
| `PCDS_TEST_DATA` | The game tables the tests run on (default run/data/2.103); none skips the tests that need them (CI). |
| `PCDS_TEST_PORTS` | docker/test.sh: three ports (UDP, admin, release feed) to use instead of free ones the system picks. |
| `PCDS_RNG_SEED` | A fixed seed for the game's random numbers: benchmarks and replays compare runs. Read by test builds only. |
| `PCDS_BENCH_SAVE` | bench: the save the benchmark runs on (required). |
| `PCDS_BENCH_SECONDS` | bench: game seconds to run (default 300). |
| `PCDS_BENCH_PLAYERS` | bench: scripted players (default 0). |
| `PCDS_BENCH_ACTION_MS` | bench: each player acts this often (default: only moves). |
| `PCDS_BENCH_START_DAYS` | bench: the game's clock starts at this many days, as of a server that has run that long (default 0). |
| `PCDS_BENCH_DUMP` | bench: a file for every message and the final save, to compare two runs. |
| `PCDS_BENCH_HANDOFF` | bench handoff: seconds between handoffs (default half the run). |
| `PCDS_BENCH_SECTIONS` | bench: 1 also prints the time per subsystem of the tick (the section! names) and the sections of the slowest ticks. |
| `PCDS_BENCH_ALLOC_SITES` | bench: N looks at every Nth allocation of the ticks and prints the places in the server's code they come from (per tick). |
| `PCDS_FLIPBENCH_SAVES` | flipbench: the worlds (comma-separated save files) whose flip it times (required). |
| `PCDS_FLIPBENCH_N` | flipbench: flips per world (default 15 in tests/hot_update.rs, 20 in bench::flipbench). |
| `PCDS_FLIPBENCH_PLAYERS` | flipbench: scripted players playing during the flips (default 8). |
| `PCDS_LOAD_SERVER` | load::tests::churn and rejoin (ignored): the running server's HOST:PORT, a loopback address (required). |
| `PCDS_LOAD_PLAYERS` | load::tests: workers or players (default 8). |
| `PCDS_LOAD_SECONDS` | load::tests: seconds to run (default 60). |
| `PCDS_LOAD_SEED` | load::tests::churn: the seed of the random stream the sessions are drawn from (default 1). |
| `PCDS_LOAD_REUSE_NAMES` | load::tests::churn: set: each worker always joins under one name. |
| `PCDS_LOAD_CHAT_BYTES` | load::tests: every chat line of the bots this long (default: short). |
| `PCDS_REVIEW_STATIONS` | perf_logistics_and_destroy (ignored): drone stations (default 1). |
| `PCDS_REVIEW_SECONDS` | perf_logistics_and_destroy (ignored): game seconds to run (default 20). |
| `PCDS_FARM_STATIONS` | drone_farm_runs_for_hours (ignored): drone stations (default 5). |
| `PCDS_FARM_DRONES` | drone_farm_runs_for_hours: drones per station (default 4). |
| `PCDS_FARM_PAIRS` | drone_farm_runs_for_hours: pairs of supply and demand containers (default 20). |
| `PCDS_FARM_SECONDS` | drone_farm_runs_for_hours: game seconds to run (default 3600). |
| `PCDS_FUZZ_TARGET` | fuzz: the target a fuzz binary runs (server/fuzz/entry.rs). |
| `PCDS_FUZZ_DATA` | fuzz: the game tables the targets that need them read. |
| `PCDS_FUZZ_LOG` | fuzz: where the game target catalogues the sites of its panics (default panics.log); for the RPC fuzz test on a real world, a file for every RPC it sends. |
| `PCDS_FUZZ_HISTORY` | fuzz: where the game target dumps the inputs since its last reset when a panic ends the run (default history.bin). |
| `PCDS_FUZZ_REPLAY` | fuzz: a history file the game target runs again. |
| `PCDS_FUZZ_TRACE` | fuzz: set: the game target prints each input it runs. |
| `PCDS_FUZZ_RESET` | fuzz: inputs the game target runs on one game before it starts a fresh one (1 for a replay). |
| `PCDS_FUZZ_SAVE` | fuzz: the save the game, twin and loadsave targets start from, and the one whose snapshot PCDS_FUZZ_SEED_OUT writes (default: the test world). |
| `PCDS_FUZZ_SEED_OUT` | fuzz: a file the handoff target writes a snapshot of a world to, as a seed. |
| `PCDS_FUZZ_SWEEP_FILE` | fuzz: the snapshot or save a sweep puts boundary values into, one at a time. |
| `PCDS_FUZZ_SWEEP_OFFSET` | fuzz: the number a sweep starts at. |
| `PCDS_FUZZ_SWEEP_STEP` | fuzz: a sweep tries every this many numbers. |
| `PCDS_CORPUS` | fuzz: the corpus folder `replay_corpus` runs (cfg pcds_corpus). |
| `PCDS_CORPUS_LIMIT` | fuzz: at most this many corpus files. |
| `PCDS_PREVIOUS_RELEASE` | The flip gate (make gate): the folder of the last release, its pcds in it. |
| `PCDS_SOAK_SECONDS` | soak_growth: game seconds to run (default 3600). |
| `PCDS_SOAK_STEP` | soak_growth: seconds of game time per step (default 0.05). |
| `PCDS_SOAK_REPORT` | soak_growth: seconds of game time between reports (default 900). |
| `PCDS_SOAK_PLAYERS` | soak_growth: players that act every game second (default 2). |
| `PCDS_SOAK_ONLY` | soak_growth: only these actions, comma-separated (item, drop, pick, build, eat, drink, mine, chat, sort); finds which action grows what. |
| `PCDS_SOAK_PARTS` | soak_growth: at the end, the heap each part of the game holds. |
| `PCDS_SOLVER_SEEDS` | solver_time_by_seed (ignored): the seeds tried for each wreck generator (default 300). |
| `PCDS_SOLVER_FROM` | solver_time_by_seed: the first seed (default 0). |
| `PCDS_FUZZ_ROUNDS` | The RPC fuzz test's rounds (default 1500). |
| `PCDS_FUZZ_SEED` | The RPC fuzz test's seed. |
| `PCDS_IPC_ECHO` | ipc test: the child process's socket file. |
| `PCDS_BACKUP_CUT_DIR` | save test: the child process's folder. |
| `PCDS_SUPERVISE_TEST` | supervise test: the child process's folder. |
| `PCDS_SUPERVISE_WHAT` | supervise test: how the child front ends. |
| `PCDS_ADMIN_TOKEN_EXEC_TEST` | chat hook test: a secret the exec program must not see. |
| `PCDS_TARBALL_URL` | install.sh: a release tarball to install instead of GitHub's latest (with its .sha256 beside it). |

## Hot updates

The new bank runs under the environment of the running front, which a hot update does not replace, and reads the files in `PCDS_HOME` itself. A release that changes what the front and the bank share is installed with `pcds upgrade --restart`.
