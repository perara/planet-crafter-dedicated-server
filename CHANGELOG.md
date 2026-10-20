# Changelog

All notable changes to the Planet Crafter dedicated server are in this file, newest first. The
format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the versions follow
[Semantic Versioning](https://semver.org/) as far as a server for a game can: a minor version may
change what the server writes to disk (the snapshot format, the tables format) and then says so
under **Breaking changes**.

Every release has two parts a reader can rely on:

* **Breaking changes**: what stops working, or works differently, for someone who updates.
* **Upgrade**: what to do, in order, to get from the release before to this one. A release that
  changes the snapshot format (the state a hot update hands over) or the front protocol cannot be
  hot-updated: `pcds upgrade` says so and `pcds upgrade --restart` installs it (the world is saved
  as the server stops; every player joins again). The release checklist in
  [docs/hot-update.md](docs/hot-update.md) keeps this file and the release in step: the release
  workflow takes the text of a tag's section, from here, as the body of the GitHub release and
  stops when there is none (`scripts/release-notes`).

## [Unreleased]

## [0.8.0] - 2026-10-20

The DLC planets run on their own ground, and the repository is ready for public releases: signed
build provenance, a changelog that is the release text, and the policy files a public project
needs.

### Breaking changes

* **Tables format 4 and snapshot format 7.** Release 0.7.0 cannot hand a running world over to
  0.8.0 (and 0.8.0 cannot hand one back): `pcds upgrade` refuses the hot update and names the
  restart install. Every player has to join again once. The way back to 0.7.0 is a restart
  install too (`pcds upgrade v0.7.0 --restart`), which reads the tables in format 3 again.
* **The tables are read again.** Format 4 holds the scenes of the DLC planets and the toxic areas
  of Toxicity, so the tables of 0.7.0 are refused at start. `pcds upgrade --restart`, `pcds
  restart`, `pcds start` and the Docker image read them again from your game folder on their own
  (under a minute); the game has to be there (in Docker, mounted at `/game`).
* **A world that is on a DLC planet needs the new tables.** Such a world used to run on an empty
  planet. It now stops at start, says so and names `pcds extract` (or `pcds update`) when the
  tables do not hold the planet's ground.
* The install tarball holds two more files: `CHANGELOG.md` and `THIRD_PARTY_LICENSES`.

### Added

* **DLC planets (Humble, Toxicity) run on their own ground.** `pcds read-game` now reads the scene
  of every planet of the game's planet list, DLC planets included, with the reader the free planets
  use (the free planets' files come out byte for byte as before). Ore veins, water and the world
  instances of the portals work there as on a free planet. Nothing of the game is in this
  repository: the ground is read from your own installation.
* **Toxicity's toxic areas.** The 33 areas of goo are read into the tables and the server runs the
  game's `ToxicAreaHandler` and `ToxicArea`: progress and finish messages to the clients that
  registered, the toxic gauge in an area, the gauge's maximum from the area's time.
* **Travel to a DLC planet** stays refused unless the world already has the planet: the server
  cannot ask Steam whether an account owns the DLC, so it never gives a world a planet (README,
  "Differences from hosting in the game").
* **Release engineering.** `CHANGELOG.md` is the release text; the release workflow stops when a
  tag has no section here, and takes the section as the body of the GitHub release (the generated
  notes follow below it). Build provenance is attested for the tarball and the image
  (`gh attestation verify`, see docs/hot-update.md "Verifying a release"), the Docker image carries
  BuildKit provenance and an SBOM, third-party actions are pinned by commit, and `THIRD_PARTY_LICENSES`
  lists the licence of every crate in the program and Valve's terms for `libsteam_api.so`.
* **Project files:** `SECURITY.md` (how to report a vulnerability), `CONTRIBUTING.md`, a code of
  conduct, issue and pull request templates, Dependabot for the workflows' actions.
* **Tools** (in the repository, not in a release): `tools/chaos` (soak, churn, chaos, disk-full,
  limits and datagram-fuzz drivers), `tools/perf`, `tools/parity` (the sweep's replay and compare
  drivers), `tools/dev`, and `docs/audits.md`, the four audits of 0.6.1 with the state of every
  finding at 0.7.0.

### Changed

* The churn, rejoin and drone-farm soaks are ignored tests (`load::tests::*`, driven by test-only
  `PCDS_LOAD_*` variables); the release has no new command or variable for them. The UTP batch
  limit can be lifted in a test build only.
* `docker/test.sh` runs on rootless Docker (a throwaway root container hands the test folders to
  the container's user and takes them back).
* The example plugins are built the same from any checkout path (their `plugin.wasm` bytes do not
  depend on where the checkout is).
* CI also syntax-checks the scripts in `tools/` and the shape of this changelog.

### Fixed

* **Stopping the server with Steam on crashed the process.** Valve's library asserts when it is shut
  down with a socket of its own still open, and the front exited without closing them: every stop
  (the admin page's Stop, `POST /api/stop`, `systemctl stop`) ended with status 139, a core dump and
  a crash report that the library tries to send to Valve, and because the unit has
  `Restart=on-failure` the admin page's Stop brought the server back after 15 seconds. The front
  now closes its Steam connections, listen socket and login before it exits (found by the release
  rehearsal; 0.7.0 has it). Direct-IP-only servers (`PCDS_STEAM=off`) never had it.
* A connection request file that holds text that is not ASCII reads as zeros instead of panicking
  (audit finding S24; only the tools that read `connection-request.hex` call it).
* The count of plugin requests in flight no longer uses `AtomicU32::fetch_update`, which Rust 1.98
  deprecates (a build warning, no change in behaviour).

### Upgrade

From 0.7.0 (a restart install; the world is saved as the server stops and is loaded again):

1. Have the game available to the server (it reads the tables from it): the Steam library on the
   machine, `--game DIR`, or `/game` in Docker.
2. `pcds upgrade --restart` (installs 0.8.0 from GitHub, reads the tables, restarts). Or run the
   installer again, or unpack the tarball over the folder (below); in Docker `docker compose pull
   && docker compose up -d`, or `docker exec pcds pcds upgrade --restart`.
3. `pcds status` shows 0.8.0 running and the tables of format 4. `pcds selftest` checks that a
   scripted player gets in.
4. A world that was on a DLC planet starts only when the tables have its ground; if it does not,
   the start says to run `pcds extract`.

Before the update, `pcds save` and keep a copy of `run/worlds/` (the automatic backups are in
`run/worlds/backups/`). Going back: `pcds upgrade v0.7.0 --restart`.

## [0.7.0] - 2026-10-20

The release that made the server a platform: a smaller, safer configuration, a plugin system, a
binary snapshot for hot updates, and the game's own clock.

### Breaking changes

* **Snapshot format 6 and tables format 3.** No release before this one can hand a world to it or
  take one from it: a restart install (`pcds upgrade --restart`) from any earlier release. The
  tables are read again (`pcds extract` or `pcds update`).
* **The environment is cut to 12 operator variables** (`docs/env.md`, generated from the code and
  held to the number by a test). Autosave, backups, the player limit, the automatic update and
  tracing are rules in `rules.json` now (`save.autosaveMinutes`, `save.backupsKept`,
  `save.backupMinutes`, `limits.maxPlayers`, `update.auto`, `update.checkHours`, `log.trace`); the
  paths are derived from `PCDS_HOME`. A `PCDS_*` name the server does not know in `server.env`
  **stops the start** and is named. There is no migration command and no tolerance of the old names.
* **Gone from the admin API:** `POST /api/settings`, `POST /api/access` and the `{"player"}`
  spelling (use `/api/config`, `/api/rules` and `{"name"}` or `{"id"}`, docs/admin-api.md);
  `POST /api/update` no longer accepts the version `latest` for "no version". The status no longer
  has the constant `standalone` field.
* **Old-format game saves are refused** (the 2022 single-planet layouts), with a pointer to `pcds
  world import`, which converts them as the game does. Before, such a save loaded as an empty
  world. A save with no game settings, with an entry that is not JSON, or with world objects of
  groups the tables do not know is refused too.
* **The snapshot of a flip is one format, read exactly.** No `reads` list, no migration step, no
  tolerance of releases without `bank-info` and no 0.6 `rules.json` compatibility.
* The hot-update gate (`make gate`) compares only releases of one format and is run by hand before a
  tag; the committed cross-release fixtures and the CI job with a secret are gone.

### Added

* **Plugins** (docs/plugins.md, docs/plugins-guide.md, docs/plugins-api.md): WebAssembly modules and
  rules files that add commands, rules and reactions without changing the server. They run in the
  bank on `wasmi` with fuel metering and no JIT; their state goes through a hot update; they are
  loaded, switched off, replaced and unloaded while the server runs. Capabilities are asked for and
  approved by an admin (also on the dashboard); plugins are signed and an admin can trust a
  publisher; `net.http` gives a plugin outbound HTTP(S) with a host policy (no private or
  reserved addresses); scoped tokens (`pcdst_...`) let out-of-process plugins use the admin API for
  their scopes, with a long poll of the event stream. `pcds plugin list | check | install | enable |
  disable | reload | new`. The crate `pcds-plugin-api` (MIT OR Apache-2.0) and four example plugins
  (`examples/plugins`).
* **New modules:** `events.rs` (the one place the game says what happened), `clock.rs` (the game on
  one f64 clock, so a server of any age takes the same steps), `dotnet_sort.rs` (the game's
  introsort where it sorts with `List<T>.Sort`), `oldsave.rs`, `worlds.rs`, `tokens.rs`,
  `bincodec.rs`, `msg.rs` (typed messages between the front, the banks and the banks' folder). The
  front and the bank are folders of modules; the layering is checked by a test
  (`scripts/check-layers`).
* **Hot update:** snapshot in a binary codec, written and read in parts on several threads, so the
  world stands for much less time during a flip; a flip that keeps crashing is given up
  instead of restarted for ever; an update that does not fit the container's memory limit is
  refused with the size to give; a rollback that cannot be live says so and names its command.
* **Bank watchdog:** a bank whose game loop hangs is killed and the world starts again (rule
  `limits.bankWatchdogSeconds`, default 60). The autosave and backups are written on their own
  thread and failures show in `/api/status`, `POST /api/save` and the dashboard. The status carries
  the tick times (p50, p99, longest of the last minute).
* **Docker, Compose, Kubernetes:** how to update, what `stop`, `kill` and a crash do, a pod manifest
  (docs/kubernetes.yaml), `docker/test.sh` (a hot update inside a container, end to end), the
  stop timeout in the image's notes.
* **Docs:** `docs/COMPLETENESS.md` (what the server does and does not, against a late-game save),
  `docs/architecture.md`, `docs/clock.md`, `docs/env.md`, `docs/admin-api.md`, `docs/docker.md`,
  `docs/testing.md`; fuzz targets for the parsers; the parity sweep's classification rules.

### Changed

* Less work per tick and per save on large worlds: save text written in one pass, prefabs parsed
  once, water boxes made once, fast maps, an idle front sleeping, one `writev` for queued frames.
* Memory a client can make the server hold is bounded in all and given back; Steam connections
  have the caps direct-IP sessions have; a panic in a client's RPC no longer saves the half-changed
  world over the last whole save.
* Names (bans, whitelist, the duplicate-name check) compare by Unicode folding, not ASCII case.
* `PCDS_RELEASES` may hold a token: status, update and lookup errors show scheme and host only.
* Re-reading the game tables replaces the folder in one step; a bank is flushed to disk before it
  is moved into place.

### Fixed

* Timers that stopped moving or looped for ever after weeks of uptime (f32 schedules: units hung
  the bank at 24 days, gauges and vehicle checks at 48), the 1e20-second clock panic, NaN objects
  from floats past 1e9 in an RPC, an inventory sort panic from 21 items, a UTP window that could
  send past the oldest unacknowledged packet.
* Owner-only server RPCs are refused from clients that do not own the object; a backpack or
  equipment of another player cannot be taken, decoded or added to; a forged `SetInventorySize` is
  clamped; transform and variable deltas a client may send are bounded.
* Unbanning something that is not banned says so; a ban behind a UDP relay is by name only, never
  by 127.0.0.1.
* The parity sweep's differences from the game (growers, recycling, rovers, wrecks laid out from
  their own seed, rocket trading, removal order), listed in `server/tests/parity/EXPECTED.md`.

### Upgrade

From 0.6.x (a restart install):

1. Before the update, open `run/server.env` and write down any of `PCDS_AUTOSAVE`, `PCDS_BACKUPS`,
   `PCDS_BACKUP_MINUTES`, `PCDS_MAX_PLAYERS`, `PCDS_AUTO_UPDATE`, `PCDS_UPDATE_CHECK_HOURS` and
   `PCDS_TRACE` that you set, then delete every line whose name is not in the table of
   `docs/env.md` (also `PCDS_ACCESS`, `PCDS_RULES`, `PCDS_WEB`, `PCDS_PORT`, `PCDS_NEW_WORLD`,
   `PCDS_ADMIN_BIND` and `PCDS_TRUST_LOOPBACK`); otherwise the start stops and names them.
2. `pcds upgrade --restart` (or the installer again; in Docker pull the image). The tables are
   read again and the server restarts.
3. Put the values from step 1 back as rules: `pcds rules set save.autosaveMinutes=5
   save.backupsKept=24 limits.maxPlayers=16 update.auto=true` (your values).
4. A game save of the old layouts, if you have one, goes through `pcds world import`.
5. Plugins are new: nothing runs until you install one.

## [0.6.2] - 2026-10-19

### Breaking changes

None.

### Fixed

* The hot update from 0.6.0 works again: a snapshot of 0.6.0 restores (0.6.1 could not take over a
  world from 0.6.0).

### Upgrade

`pcds upgrade` from 0.6.0 or 0.6.1; no restart needed.

## [0.6.1] - 2026-10-19

### Breaking changes

* Snapshot format 3: a hot update from 0.6.0 into 0.6.1 does not work (fixed in 0.6.2, which reads
  0.6.0's snapshot): go to 0.6.2 directly.

### Fixed

The parity sweep's fixes, from comparing the server with the real game host on one script per
mechanic family: equipment slots take equipment only, drones, the deconstruction volumes (measured
on the game, all six sides), rover seats and late-join state, drop flags and rotation, the order of
saved inventories, algae generators on land, vegetable crop angles.

### Upgrade

`pcds upgrade v0.6.2` from 0.6.0 (live); skip 0.6.1.

## [0.6.0] - 2026-10-18

### Breaking changes

* Tables format 2: the fields the parity fixes need are required, never guessed; run `pcds update`
  (an install does it) after upgrading from 0.5.
* `server.env` values are checked at start; a bad one stops the server and is named.
* A loopback client's SteamID is trusted only with `PCDS_TRUST_LOOPBACK=1`; chat length and rate
  and the RPC work per tick of a client are capped.

### Added

* **Hot update** (`pcds upgrade`): the server runs as a front that keeps the sockets and a bank that
  holds the world, in two installed releases `run/bin/a` and `run/bin/b`; a new release takes the
  world over live and `--rollback` goes back. The automatic update (`PCDS_AUTO_UPDATE`), the
  dashboard's Updates page, a fall back to the other bank when a release cannot start the world.
* The security and data-safety pass (the audit's H1 to H3, M1 to M5, L1 to L4): hostile clients,
  forged addresses, NaN and infinity from clients, owner-only variables, kicks delivered before the
  close, whitelists checked at the request, one server per world (`<save>.lock`).
* World tools (new, remove, download and restore worlds, pause), backups to S3, Stop/Start/switch
  of the world from the admin page, the Docker image's health check.
* The parity harness (`server/tests/parity`): probe scripts per mechanic family, run against the
  real game host and this server, with the differences accepted on purpose.

### Upgrade

From 0.5.x: run the installer again (or unpack the tarball and `pcds up`); run `pcds update`
(the installer does) to read the tables again. Releases before 0.6.0 cannot be hot-updated; from 0.6.0 on `pcds upgrade` does it.

## [0.5.0] - 2026-10-18

Short history from here down, written from the tags and commit messages. Not every release was
checked again for breaking changes; the tag messages record none.

### Breaking changes

None recorded.

### Added

* One-command install (`install.sh`), the Docker image on GHCR, a `worlds/` folder; `setup` and `up`
  stop a running server to move its world into `worlds/`.

### Upgrade

Run the installer again; `run/` stays.

## [0.4.0] - 2026-10-18

### Breaking changes

None recorded.

### Changed

* Answers within a millisecond, batched networking (fewer datagrams and copies), a load test client
  (`pcds load`), the loop timings in the status.

### Upgrade

Replace the program and restart.

## [0.3.0] - 2026-10-18

### Breaking changes

None recorded.

### Added

* Server rules: `rules.json` and `/api/rules` change meteo, gauges, speeds, death, loot and limits
  while the server runs.

### Changed

* About 4 times less CPU with the same game behaviour (water boxes once per energy check, dirty-only
  flush, fast id maps, idle machines skipped), the tick benchmark (`make bench`).

### Upgrade

Replace the program and restart.

## [0.2.1] - 2026-10-18

### Breaking changes

None recorded.

### Fixed

* Security and data safety: save corruption, hostile clients and the allocations they found, ban
  bypass, character loss, inventory overwrites, safer extraction and setup (modded game folders are
  left alone, stable build ids), crash loops keep the backup history, room for large worlds on join.

### Upgrade

Replace the program and restart.

## [0.2.0] - 2026-10-18

### Breaking changes

None recorded.

### Added

* Corridors between habitat modules, wreck interiors and portal instances generated on the server,
  floating machines that follow the water surface, travellers landing on the departure platform's
  landing spot, new players spawning at the first livable place of the planet, and other host
  leftovers (game version 2.103).

### Upgrade

Replace the program and restart.

## [0.1.0] - 2026-10-17

First public release (game version 2.103).

### Breaking changes

None; the first release.

### Upgrade

Not applicable.

[Unreleased]: https://github.com/perara/planet-crafter-dedicated-server/compare/v0.8.0...HEAD
[0.8.0]: https://github.com/perara/planet-crafter-dedicated-server/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/perara/planet-crafter-dedicated-server/compare/v0.6.2...v0.7.0
[0.6.2]: https://github.com/perara/planet-crafter-dedicated-server/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/perara/planet-crafter-dedicated-server/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/perara/planet-crafter-dedicated-server/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/perara/planet-crafter-dedicated-server/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/perara/planet-crafter-dedicated-server/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/perara/planet-crafter-dedicated-server/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/perara/planet-crafter-dedicated-server/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/perara/planet-crafter-dedicated-server/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/perara/planet-crafter-dedicated-server/releases/tag/v0.1.0
