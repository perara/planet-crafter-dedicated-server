# Contributing

Thank you for helping. This page is what a change needs to be merged.

## What does not belong in the repository

* **Nothing from the game.** No tables read from an installation (`netdata.json`, `gamedata.json`,
  scenes), no extracted assets, no game binaries, no save files, no screenshots of them. The
  tests that need the tables skip without them (`PCDS_TEST_DATA=none`); a fixture is written by
  hand or made by a test.
* **Nothing personal.** No Steam ids, player names, IP addresses, invite codes, tokens, keys or
  e-mail addresses in code, tests, docs, logs or commit messages. Look at your diff before you push.
* No generated output (`out/`, `run/`, `dist/`, `target/`), no files over 1 MB, no copies of source
  and no version-suffixed names (`_v2`, `_final`): history is git, and a frozen version is a tag.

## Build and test

You need Rust 1.88 or newer (`rustup`), `make`, Python 3 and, for the scripts, `shellcheck`.

```bash
make build                       # server/target/release/pcds with libsteam_api.so beside it
cd server
PCDS_TEST_DATA=none PCDS_GAME_DIR=none cargo test --release --locked    # what CI runs
cargo clippy --release --locked  # no warnings
python3 tests/parity/completeness/map.py check
cd .. && make lint               # clippy and shellcheck
```

With your own copy of the game (`pcds extract` reads the tables into `run/data/`), `make test`
runs the rest. [docs/testing.md](docs/testing.md) says what each kind of test needs and how to run it;
CI cannot run the ones that need the game, so run them yourself when you touch game logic.

If you change the plugin API or an example plugin: `cargo test --locked` in `plugin-api/`, and
`scripts/build-plugin-examples` (docs/testing.md, "Plugins").

## How the code is arranged

* The modules are in layers and a lower layer must not use a higher one
  ([docs/architecture.md](docs/architecture.md), `server/layers.txt`, `scripts/check-layers`).
  A new module goes in one layer in that file; the test fails when it is in none.
* The game logic is a port of the game's own: say in a comment where the game decides a value,
  and keep the game's order and rounding. Tests send the messages a client would send
  (`src/testkit.rs`). A new RPC or module is named in `src/coverage.txt` with its tests.
* No legacy support and no fallbacks: a format is read by the one release that wrote it, a setting
  that is not known stops the start, an input that cannot be read is refused. Do not add `#[serde(default)]`
  to keep an older snapshot readable; bump the format instead (docs/hot-update.md, "State and the
  next release").
* `docs/env.md`, `docs/admin-api.md` and the completeness map are generated or checked against the
  code: run the test that names the file it wants regenerated.

## Commits and pull requests

* One change per pull request, rebased on `main`, with its tests and docs in it.
* The commit message says what changed and why, in plain words, in the first line (as the log
  does: "Refuse a save with no game settings instead of running it as an empty world"). The body
  gives the reason or the measurement. No prefixes, no ticket numbers.
* A change a user can see goes in [CHANGELOG.md](CHANGELOG.md) under `[Unreleased]`, in plain
  language. If it breaks something (a format, a setting, an API route), it goes under **Breaking
  changes** and the **Upgrade** part says what to do.
* A change to what is serialized in a snapshot bumps `handoff::FORMAT`; to the front protocol,
  `ipc::PROTOCOL`; to what the tables hold, `TABLES_FORMAT`. The release is then a restart
  install and says so.

## Releases

Maintainers only: the release checklist is in [docs/hot-update.md](docs/hot-update.md).

## Licence

Your contribution is licensed as the part of the project it changes: GPL-3.0-or-later for the
server and `plugin-api/` under MIT OR Apache-2.0 (see README, "License"). Be sure you wrote it
or may pass it on.
