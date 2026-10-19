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
| `flip-verify` | A bank that restores a snapshot checks its state equals the snapshot's (every flip). |
| `test-faults` | POST /api/update takes failForTest, to make the next bank fail on purpose. |
| `trust-loopback` | Believe the SteamID a direct-IP client from 127.0.0.1 claims. Never behind a UDP relay, where every player comes from 127.0.0.1. |
| `bank-fail=restore\|protocol` | The front sets it for a bank: it dies on RESTORE, or says another protocol. |

## Tests and tools

Read by test builds and tools, never by an installation. Set by hand for the benchmark and the tests, or by a test for the process it starts.

| Variable | What |
|---|---|
| `PCDS_TEST_DATA` | The game tables the tests run on (default run/data/2.103); none skips the tests that need them (CI). |
| `PCDS_RNG_SEED` | A fixed seed for the game's random numbers: benchmarks and replays compare runs. Read by test builds only. |
| `PCDS_BENCH_SAVE` | bench: the save the benchmark runs on (required). |
| `PCDS_BENCH_SECONDS` | bench: game seconds to run (default 300). |
| `PCDS_BENCH_PLAYERS` | bench: scripted players (default 0). |
| `PCDS_BENCH_ACTION_MS` | bench: each player acts this often (default: only moves). |
| `PCDS_BENCH_DUMP` | bench: a file for every message and the final save, to compare two runs. |
| `PCDS_BENCH_HANDOFF` | bench handoff: seconds between handoffs (default half the run). |
| `PCDS_FUZZ_ROUNDS` | The RPC fuzz test's rounds (default 1500). |
| `PCDS_FUZZ_SEED` | The RPC fuzz test's seed. |
| `PCDS_IPC_ECHO` | ipc test: the child process's socket file. |
| `PCDS_BACKUP_CUT_DIR` | save test: the child process's folder. |
| `PCDS_SUPERVISE_TEST` | supervise test: the child process's folder. |
| `PCDS_SUPERVISE_WHAT` | supervise test: how the child front ends. |
| `PCDS_ADMIN_TOKEN_EXEC_TEST` | chat hook test: a secret the exec program must not see. |
| `PCDS_TARBALL_URL` | install.sh: a release tarball to install instead of GitHub's latest (with its .sha256 beside it). |

## Gone since 0.6

A server started with one of these set (in `server.env` or its environment) stops, in one line that says what replaces it, and `pcds start` and `pcds restart` say so before asking systemd. `pcds config migrate` shows what it would change in `server.env` and `rules.json`; `--apply` writes it.

| Was | Now |
|---|---|
| `PCDS_AUTOSAVE` | the rule save.autosaveMinutes: edit rules.json or POST /api/config |
| `PCDS_BACKUPS` | the rule save.backupsKept: edit rules.json or POST /api/config |
| `PCDS_BACKUP_MINUTES` | the rule save.backupMinutes: edit rules.json or POST /api/config |
| `PCDS_MAX_PLAYERS` | the rule limits.maxPlayers: edit rules.json or POST /api/config |
| `PCDS_AUTO_UPDATE` | the rule update.auto: edit rules.json or POST /api/config |
| `PCDS_UPDATE_CHECK_HOURS` | the rule update.checkHours: edit rules.json or POST /api/config |
| `PCDS_TRACE` | the rule log.trace: edit rules.json or POST /api/config |
| `PCDS_PORT` | it only chose the port of the first server.env: the port is part of PCDS_LISTEN (HOST:PORT) |
| `PCDS_ADMIN_BIND` | it only chose the admin address of the first server.env: that is PCDS_ADMIN (the Docker image sets it) |
| `PCDS_RULES` | the file is rules.json in PCDS_HOME (move yours there) |
| `PCDS_ACCESS` | the file is access.json in PCDS_HOME (move yours there) |
| `PCDS_ENV_FILE` | pcds run derives it from PCDS_HOME (server.env there) |
| `PCDS_BANKS` | pcds run derives it from PCDS_HOME (bin/ there) |
| `PCDS_BANK` | fronts before 0.7 set it for their banks; nothing reads it |
| `PCDS_NEW_WORLD` | the options of the first world are the container's arguments now: docker run IMAGE --planet Aqualis (Compose: command: ["--planet", "Aqualis"]) |
| `PCDS_WEB` | the page is web/index.html beside the program (a release or bank folder): change that file |
| `PCDS_TRUST_LOOPBACK` | PCDS_DEV=trust-loopback (a developer hook) |
| `PCDS_FLIP_VERIFY` | PCDS_DEV=flip-verify (a developer hook) |
| `PCDS_TEST_FAULTS` | PCDS_DEV=test-faults (a developer hook) |
| `PCDS_BANK_FAIL` | PCDS_DEV=bank-fail=restore or bank-fail=protocol (a developer hook) |
| `PCDS_WRECKS_TRACE` | debug prints of the wreck layout in read-game: deleted |
| `PCDS_REACH_IL` | `pcds native-reach ... --il` |
| `PCDS_DIFF_SHOW` | the fourth argument of `pcds tables-diff` |
| `PCDS_REVIEW_CHESTS`, `PCDS_REVIEW_LOOSE`, `PCDS_REVIEW_DEMANDS`, `PCDS_REVIEW_GROWING` | constants of the one perf test they sized |
| `PCDS_FUZZ_TRACE` | debug prints of the fuzz test: deleted |

## Hot updates from a release before 0.7

A hot update replaces the bank, not the front, so the new bank runs under a front that still has the environment of 0.6: its whole `server.env` and the paths its launcher set. The bank never refuses to start on a retired setting; it ignores them and says so once in one line. The rules decide what they set, so run `pcds config migrate --apply` after the update (the old values are kept as `server.env.before-migrate`), before the next restart. These the old front or launcher sets for its child; this version takes them without a word when they say what `PCDS_HOME` gives anyway and stops on one that says otherwise:

`PCDS_BANK`, `PCDS_BANKS`, `PCDS_ENV_FILE`, `PCDS_RULES`, `PCDS_ACCESS`.

`rules.json` and the snapshot of a bank leave the sections new in 0.7 (`save`, `update`, `log`) out while they hold the defaults, so a 0.6 bank can still read them for a rollback; once one of those rules is set, a rollback to a 0.6 bank needs it removed.
