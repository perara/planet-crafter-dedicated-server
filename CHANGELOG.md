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

An operator's release: one command that checks the setup (`pcds doctor`), numbers and a health route
for monitoring, a log that can be filtered and shipped, a sandboxed service, backups that are checked
and restored from the command line, and limits on the admin port. Five new documents:
[monitoring.md](docs/monitoring.md), [hardening.md](docs/hardening.md),
[disaster-recovery.md](docs/disaster-recovery.md), [reverse-proxy.md](docs/reverse-proxy.md), and the
dashboard [grafana/pcds.json](docs/grafana/pcds.json).

Plugins grew up by being used (the plugin parts below). Fourteen example plugins were written and tested against the real game, and what
hurt while writing them is fixed: plugin ABI 1.3 (a stable player id, the wall clock, who built or destroyed
something, timers, a backpack and nearby-objects query, giving items and Terra tokens, typed settings, operator
commands), a runner that tests a plugin in the real host without a game, a log and a cost table for each plugin, a way
to share a plugin as one file, and a dashboard page that shows what a plugin did and what a new version asks for.

### Breaking changes

* **Snapshot format 8.** The rules are part of the state a hot update hands over, and three rules
  were added (`log.format`, `log.level`, `save.verifyBackups`), so 0.8.0 cannot hand a running world to
  this release (`make gate` found it: the new game process could not read the old snapshot). `pcds
  upgrade` refuses the hot update and names `pcds upgrade VERSION --restart`; every player joins again
  once. The game tables (format 4) and the front protocol (3) are unchanged: nothing is read again.
* **Log lines start with a time, a level and a component** (see Changed), which breaks a program that
  matched the start of a line. `log.format=json` is the format for programs.
* **`pcds run` and the server no longer rewrite the systemd unit.** `pcds restart`, `start`, `install-service`
  and `upgrade --restart` do.

* **Plugin ABI 1.3, crate `pcds-plugin-api` 0.2.0.** A module built for 1.0, 1.1 or 1.2 runs unchanged. Rust source that
  names every field of `Event::Tick`, `PlayerJoin`, `PlayerLeave`, `Chat`, `Command`, `ObjectPlaced`, `ObjectDestroyed`
  or builds a `WorldInfo`/`PlayerInfo` must add the new fields (or use `..`); the bytes an old module receives are
  unchanged apart from fields at the end that it ignores.
* **`Event::Command::command` is lower case**, whatever the player typed.
* **A string setting in `[[config]]` is a secret unless it says `secret = false`** (it was always the default); a number or a
  yes/no is never a secret. A setting of type `int`, `float`, `bool` or with `choices` is checked when it is set, and a stored value that
  no longer fits makes the plugin `invalid` instead of running with a value nobody meant.
* **`[[every]]` in a rules file** takes a text or a list; **`[[when_players]]`** has `at_most`. Files that worked still work.

### Upgrade

1. Back up: `pcds backup verify` finds a backup that loads, `tar czf pcds-settings.tar.gz -C run server.env
   rules.json access.json plugins` keeps the settings ([disaster-recovery.md](docs/disaster-recovery.md)).
2. `pcds upgrade vX.Y.Z --restart`. It installs the release, writes the new unit (the sandbox of
   [hardening.md](docs/hardening.md)) and restarts the server; players join again. Then `pcds doctor`.
3. In Docker: `docker compose pull && docker compose up -d`, with the new `compose.yaml` if you want
   its hardening (a read-only root needs the `tmpfs` for `/tmp`; docs/hardening.md); a hot update inside the
   container is refused for the same reason as above (`docker exec pcds pcds upgrade vX.Y.Z --restart`).
4. A hook program that writes outside `run/`, or is written in Node or Java and was started with
   `MemoryDenyWriteExecute` set by you, is affected by the sandbox: docs/hardening.md, "What a hook
   program or a plugin sees".
5. Back to 0.8.0: `pcds upgrade v0.8.0 --restart`; the unit of 0.8.0 is written again by it.

Nothing to do for a server that runs no plugin, or plugins built for ABI 1.0 to 1.2: they load as they were. `POST /api/plugins/logs` is
a route of the front (which a hot update does not replace): `pcds plugin logs` and the dashboard's log work after `pcds upgrade --restart`. Rebuild a Rust plugin that
matches on `Event` fields by name. The plugin changes add nothing to the snapshot (format 8 in this release comes from the rules being part of the state).

### Added

* **`pcds doctor`** checks everything an operator needs in one command and prints PASS, WARN or
  FAIL with the line to type that fixes each problem: the game folder and its tables (build,
  tables format, game version, the DLC ground when the world is on a DLC planet and the rule
  `world.dlcPlanets`), the world file, `server.env`, `rules.json` and `access.json`, both ports
  (free, or this server's) and whether the admin port is open to other machines, the modes of the
  files that hold secrets, free disk against the world and its backups, whether the backups are on
  and leave the machine, the clock (backups stamped in the future), the systemd unit, its sandbox
  and lingering, Steam (login token, `steamclient.so`, network), both banks, the release feed,
  plugins that wait or fail, the memory limit against the size a hot update needs, and the
  open-files limit. With the server running it also asks the admin API (invite code, last save,
  backups, S3, crashes). It changes nothing; the exit code is 1 when a check FAILs; `--json` is
  for scripts.
* **Metrics and health for monitoring.** `GET /api/metrics` answers in the Prometheus text format
  (checked with `promtool check metrics`, and scraped by a real Prometheus): players online and
  the limit, tick p50, p99 and max, how busy the loop is, memory of the front and the game,
  objects, inventories, the last save (when, whether it worked, how long it took), backups and the
  S3 copies, hand-overs (total, failed, how long the world stood), crashes, watchdog kills, the
  queue of player traffic and what was dropped, plugins by state and what each one costs, and
  version and format labels. `GET /api/health` needs no token and answers `{"ok":true}` while a
  world is served and `503` while it is stopped, loading or starting again after a crash; a hot
  update does not turn it red. `pcds health` is the command form. [docs/monitoring.md](docs/monitoring.md)
  has the list of metrics (a test keeps it equal to what the server writes), a scrape
  configuration, 18 alert rules (checked with `promtool check rules`) and the Grafana dashboard
  [docs/grafana/pcds.json](docs/grafana/pcds.json) (all its queries run on a real Prometheus).
* **A structured, filterable log.** The rule `log.format` (`text` or `json`) and `log.level`
  (`error`, `warn`, `info`, `debug`) apply to the front and the game process at once and change
  while the server runs. A JSON line has `timestamp`, `level`, `component`, `message` and
  `fields` (the process, its pid, and the data of the event: client, name, path, ...); a text
  line starts with its UTC time, level and component. Under systemd each line carries its priority,
  so `journalctl -p warning` works. `pcds logs` takes `-n LINES`, `--since TIME`, `--grep PATTERN`
  and `--follow`. [docs/monitoring.md](docs/monitoring.md) says how long journald, Docker and
  Kubernetes keep the log and how to set that; [compose.yaml](compose.yaml) now rotates the
  container's log.
* **Backups can be checked and restored from the command line**
  ([docs/disaster-recovery.md](docs/disaster-recovery.md)). `pcds backup list`; `pcds backup
  verify` loads the newest backups (3 by default, `--count N`, `--all`) with the loader the server
  uses, reports what each holds and whether the server would start it, checks the S3 copy of the
  newest, and exits 1 when one does not load; `pcds backup restore NAME` loads the backup first,
  asks, keeps the world it replaces and refuses while the server runs (`--as WORLD` makes a new
  world instead); `pcds backup fetch [NAME]` copies one from the bucket (HEAD, GET and listing are
  new in the S3 client, signed like the upload; the signer is checked against AWS's own examples,
  and ran against a real S3 server). The new rule `save.verifyBackups` (off by default) reads each
  new backup back on a thread of its own and says so in the log and in the metrics
  `pcds_backup_verify_last_ok` and `pcds_backup_verify_last_timestamp_seconds`. The guide covers
  what to keep, restoring on the same machine, on a new one, from the bucket and from a Docker
  volume, and was rehearsed on a scratch installation: a late-game world restored from the bucket
  into an empty installation on other ports, which a scripted player and two load players then
  joined.
* **A scoped token for the scraper.** The scope `metrics.read` opens `GET /api/metrics` and nothing
  else (`pcds plugin token create prometheus --scope metrics.read`), so a monitor does not hold the
  admin token.
* **docs/performance.md**: what the server costs (instructions, allocations, bytes, wake-ups), the
  budgets a test holds the build to, how to profile it and read the tick histogram of `/api/status`, how
  big a machine a world and its players need, the limits of the transport, and what was tried and left out.
* **Efficiency budgets as tests** (docs/testing.md): allocations and user instructions per tick on a
  world anyone can build (0, 400 and 1,600 buildings, 0 and 4 players), their growth with the world, the
  bytes per world object of a snapshot, a save and a join, the instructions of a hot update's stand,
  the transport's allocations (needs no game tables: CI runs it) and the wake-ups of an idle server.
* `GET /api/status` `traffic.wire`: datagrams and bytes in and out, how many datagrams only
  acknowledged, heartbeats and resends. `bench::bench` and `bench::flipbench` print user instructions
  (where the kernel lets the process count them); `PCDS_BENCH_ALLOC_SITES` names where a tick allocates.
  `tools/perf/loadrun.py` prints the wire; `PCDS_TOOLS_RULES` gives the tools' servers rules (a DLC planet).

* **Plugins can** (ABI 1.3): read the world in `load`; know a player by a stable `player_id`; read the wall clock (`unix_ms`);
  see who built or took apart an object (`client` on `object_placed` and `object_destroyed`); look up one player, a
  player's backpack, the objects near a point, one object (a network object id from an RPC resolves to its position),
  and their own settings; give items (`player.give`) and Terra tokens (`world.tokens`); set timers (`event.timer`);
  hear that the world was handed over by a hot update (`flip`); declare commands with a usage line, an argument count and
  a permission (`permission = "operator"`, the players in the new rule `access.operators`).
* **`pcds plugin test`**: a scenario file (TOML) drives a plugin in the real host against a made-up world, with no game tables;
  exit code 1 when an expectation fails. `scenario.rs` describes the format.
* **`pcds plugin check`** shows the fuel of each event as a share of the budget, in a world of 16 players, and warns about a
  secret setting the plugin cannot read.
* **`pcds plugin watch`** (install each new build, approve it again if it asks for nothing new, state kept), **`logs`**, **`stats`**,
  **`pack`**, **`search`**, **`install`** from a `.pcdsplugin` file, an https address (with `--sha256`) or an index entry; `plugin new`
  has a C template, a Makefile and a justfile, and a scenario.
* **`pcds-plugin-api`**: a `testing` feature (`MockHost`, `Harness`: ordinary `cargo test` for a plugin), a small `json` module, docs for every
  item, package metadata and a README. It is not published.
* **The dashboard's Plugins page** shows each plugin's log, faults, cost by event and settings (typed controls), a per-row
  reload, and what a new version asks for beyond what you approved.
* **`POST /api/plugins/logs`**; `GET /api/plugins` has `byEvent`, `faultLog`, `deniedCapabilities`, `changes`, typed settings and
  `commandPrefix`; the event stream's chat and player events have `playerId`.
* **Examples** (`examples/plugins/`): `afk-guard`, `playtime-board`, `quest-rewards`, `claims`, `restart-notice`, `chat-bridge`,
  `prometheus-exporter`, `hello-c`, `server-motd`, `scaling-rules`, each with a README, scenarios and a test in the real game
  (`server/src/plugins/cookbook_tests.rs`).
* **Documentation**: docs/plugins-guide.md is a tutorial and a cookbook; docs/plugins-api.md has the host functions with their costs and
  the delivery and ordering rules; docs/plugins.md has sharing, the licence, the versions, the measurements.

### Changed

* **Every line of the log starts with its time, level and component** (`2026-10-20T18:07:37.354Z
  INFO  front: client 1: connected from ...`), also in `pcds logs`, where journald's own time is
  not shown. Warnings and errors still go to standard error and the rest to standard output.
  A program that parsed the old lines has to skip the prefix, or read `log.format=json`. Valve's
  Steam library still writes a few plain lines of its own while the server logs in.
* **The game's protocol noise is `debug`**: an RPC to an object without the class, a variable an
  object does not carry. It is no longer in the log unless `log.level=debug`.
* **The unit is written by the command line only.** The server can no longer rewrite its own unit
  at start (its home is read-only to it, which is the point); `pcds start`, `pcds restart`,
  `pcds install-service` and `pcds upgrade --restart` write it when a release changes it. After
  the update that brings the sandbox, run `pcds restart` once (`pcds upgrade --restart` does it).
* **The Docker health check and the Kubernetes probes use the health route.** The image's
  `HEALTHCHECK` runs `pcds health` (it needed the admin token and a status request before), and
  [docs/kubernetes.yaml](docs/kubernetes.yaml) probes `GET /api/health` over HTTP. The liveness
  probe now waits five minutes for a world, since a crash is healed in seconds.
* **The admin port waits 2 seconds, not 3,** for a request head.
* **The world's tick costs a fraction on a big world.** The late-game save (33,000 world objects,
  hundreds of machines, drones and logistics inventories), user instructions per simulated second:
  idle 196 -> 24 M (8x), 4 players 203 -> 32 M, 32 players 226 -> 54 M, 100 players 329 -> 108 M, with
  the same messages and the same save, byte for byte (the bench's fingerprints, every case). The
  logistics search for tasks went through every supply for every demand and every dropped item; it
  goes through the items left. The machines' walk reads a flat array of due times instead of every
  machine. Scans of every world object (the machines' range, the autocrafters' items) look at the
  few that have a network object, and de-duplicate by set. A message to one client is no longer looked
  for by every client, and the drones' hiding is sets, not lists (100 players and the drones). Idle,
  the live bank on that save used 380 M instructions a second and uses 45 M. On the bench world
  and the grown 100k and 200k worlds the gain is 10 to 50% (they have little logistics).
* **The hot update stands for 40% less work** (user instructions of a snapshot and its restore, all
  threads): 385 -> 223 M at 33k objects, 1,080 -> 625 M at 100k, 2,130 -> 1,239 M at 200k. The
  restore read the tables of every machine again, six lookups each, which was the largest part of it
  (52 / 172 / 342 ms of the stand); it reads them once for each group. In time, in process on a loaded
  host: 102 -> 32 ms at 33k, 276 -> 76 at 100k, 513 -> 162 at 200k (p50 of the better of two runs); with
  the real front and two banks and no players: 105 to 118 -> 62 to 67 ms at 100k, 277 to 306 -> 95 to
  129 ms at 200k.
* **A start is faster**: the game from the save 341 -> 197 ms (33k), 508 -> 334 (100k), 973 -> 739
  (200k) of CPU (the prefab components are found by an index, a group's authorized items worked out
  once).
* **The front costs a third at 64 players** (89 M of 266 M user instructions a second: a chunk of
  the send queue was copied byte by byte; the sessions' timers are looked at when one can be silent,
  not at every 5 ms frame) and wakes up 20 times a second idle, not 50 (its quiet frame is 50 ms; an
  admin request waits up to that).
* **`pcds load` and `pcds probe` take a batch of any size** (up to 256 MiB), as the game's client does,
  so a world of 200,000 objects (a join of 2.4 MB) can be joined by them. The server's own limit on a
  batch it receives is still 1 MiB. The test-only switch for it is gone.
* **Left out, because the numbers did not support them**: the save text on several threads, a
  two-level array for the machines' walk, delayed acknowledgements (docs/performance.md).

* **A module is translated when it is read**, not when each function is first called. Translation was charged to the call as fuel,
  so the first call of an instance cost up to five times the next ones and a budget that held the steady state could fault it, again and
  again after each restart. Now translation is never charged to a call (the first call can still cost a little more, for the heap it grows), and a reload does its translation before it starts the plugin.
* **A fault says what happened and what to do**: the event, the budget used and the knob that raises it (`chat: out of fuel: the call used all
  500000 fuel...`), a Rust panic is named as such, a capability a plugin used without having it is named once in its log, and a manifest key,
  capability or event that is misspelled names the one meant.
* The first `tick` of an instance says `since_ms` 0 (it was the whole game time).
* **The server uses a third of the memory.** The bank's resident memory, on the same worlds, with the
  same messages, the same saves and the same snapshots, byte for byte (the bench's fingerprints, every
  case; the hot update's snapshot is the same bytes): the bench world 43.8 -> 21.4 MiB, the late-game save
  (33,000 world objects) 77 -> 33, 100,000 objects 122 -> 43, 200,000 objects 199 -> 66 MiB (a world object
  beyond the bench world: 0.81 -> 0.23 KiB). The start peaks at 298 -> 87 MiB on 200,000 objects, and a hot update holds 437 -> 179 MiB
  at its largest (264 -> 114 at 100,000). The tick costs the same (the late save: 32.7 against 32.8 M
  instructions a second with 4 players; 100,000 objects 28.7 against 35.5) and a hot update's stand costs
  no more (9,500 instructions a world object, 9,700 before). Where it went: nine in ten world objects of a big
  world are items in an inventory and are 24 bytes (304 before), a machine without components is 48
  bytes (584), a network object shares the names, types and order of its variables with every other of its
  kind (1.7 KB -> 0.55), and the game's tables, parsed, are a quarter of what they were (26 -> 8 MiB; the
  start also keeps a planet's scene dump as text until a planet needs it). The start reads the world from
  the save's text one entry at a time (a tree of values of the whole save was 100 MB at 200,000 objects),
  checking a backup or a world does the same, and malloc's mmap threshold is fixed at 128 KiB, so that
  the frames of a world joined (2.4 MB) go back to the system (the front with 16 players on the 200,000-object
  world: 21.5 -> 10.5 MiB, its anonymous memory 13.2 -> 1.4). A player costs the bank 6 KB (25 before) and
  the front 35 to 50 KB. docs/performance.md has the byte accounting, the sizing
  guide (a container for a hot update: three times the bank and the front still holds) and what was tried
  and left out; `bench::budget` has a test of the memory of a world object, a network object, a machine
  and an inventory.

### Fixed

* **The `discord-notify` example switched itself off for good during an outage.** With Discord down its lines
  pile up, and a flush builds a message of up to 1,900 characters from them at every tick, which costs more
  than the default fuel of one event: the plugin ran out of fuel, was switched off after five faults in a
  minute, and started again at a reload only to fault again, so the lines were never sent once Discord came
  back. The example's manifest now asks for 4,000,000 fuel an event (and is signed again). Found by the
  long soak's `net.http` plugin, a copy of it, against a webhook that was not there.
* **A front that had been stopped killed the healthy game process when it ran again.** The watchdog
  (`limits.bankWatchdogSeconds`) counted the time since the game process last spoke; a front stopped for
  longer than the rule (SIGSTOP, a paused virtual machine, a suspended or starved host) took its own
  absence for the game's silence, killed the game at its first look and started the world again from the
  last save with every player gone. The watchdog now notes when it last looked and, after a gap of more than
  5 seconds, starts the game's clock again.
* **A pick-up that no inventory takes left a world object behind.** `AddWorldObjectToInventoryServerRpc`
  makes the world object when the world does not have the id (an item that had despawned, or an id a
  client made up); when the inventory was full or refused it, the object stayed in the world, held by
  nobody, placed nowhere, never saved and never freed. A long-uptime run of the virtual-clock soak
  (two scripted players on a mid-game world, 70 days of game time) grew the snapshot from 0.43 MB to
  2.5 MB and the process's memory to 380 MB; a client that asks again and again with a full inventory
  could do the same. The object is taken away again when the request fails.

### Security

* **The systemd unit is sandboxed** ([docs/hardening.md](docs/hardening.md)): no new privileges,
  no capabilities, everything but `run/` read-only (your home readable, not writable), a private
  `/tmp`, no devices, no writing of `/proc/sys`, `/sys` or the cgroups, only the socket families a
  server uses, a system call filter, files private to your user, and limits on open files and
  tasks. `systemd-analyze --user security` scores the unit 2.1 (OK) where the 0.8 unit scored 9.4
  (UNSAFE) (audit item I2). Each option was tried with the real server: Steam login and invite
  code, players, saves and backups, a hot update, S3 copies, a signed webhook, hook programs
  (shell with curl, Python, Node, Java), a plugin with `net.http`, and `steamcmd`. Left out because
  they break something the server starts: `MemoryDenyWriteExecute` (Node and Java hooks,
  `steamcmd`) and `SystemCallArchitectures=native` (`steamcmd` is 32-bit); the document says how to
  add them. A machine that does not let a user service have a mount namespace gets the options that
  need none, and `pcds install-service` and `pcds doctor` say so. The update check's unit gets the
  options that need no mount.
* **Docker**: the entrypoint runs the server with `setpriv --no-new-privs`; `compose.yaml` turns on
  `read_only`, `no-new-privileges`, dropped capabilities (with the five the entrypoint needs to
  give `/data` away) and a process limit; `docker/test.sh` runs its hot update with the same flags.
  The base images are pinned by digest (audit item I1; Dependabot's `docker` entry proposes new
  ones). The Kubernetes example mounts no service account token.
* **The admin port, when it is open past the loopback** (audit item I9): other machines get 48 of
  the 64 connection slots and one address 16, so the command line and a proxy on the machine always
  find one; a connection over its share is answered `503` with `Retry-After` at once; a request head
  that does not come within 2 seconds ends the connection. The start-up warning says that the token
  can run programs on the machine (the rule `chat.exec`), and `pcds doctor` warns.
  [docs/reverse-proxy.md](docs/reverse-proxy.md) has a Caddy and an nginx configuration, tried
  against a real server.
* **No secret in the log.** The admin token, the Steam token, the chat secret, webhook URLs and S3
  keys are replaced by `***` wherever they appear in a line, and so is anything that looks like a
  bearer token or a scoped token. The code did not print them; this is the net under it, with
  tests in both formats and a check on a real server's log.

* **`net.http` never reaches more of the cloud services that hand out credentials.** Besides
  169.254.169.254, 100.100.100.200 and fd00:ec2::254 it now never reaches Azure's WireServer
  (168.63.129.16: a *public* address, so a plugin with `net.http` and no `net.http.local` could
  reach it through any name that resolved to it), the container credentials of AWS ECS and EKS
  (169.254.170.2, 169.254.170.23, fd00:ec2::23), Tencent's metadata (169.254.0.23) and Google's
  IPv6 metadata server (fd20:ce::254).
* **TLS: rustls-webpki 0.103.16 and webpki-roots 1.0.10** (RUSTSEC-2026-0382, RUSTSEC-2026-0381, of
  19 and 20 October): the chain check accepted intermediates that the root programs distrust (emSign).
  The updater, the S3 copies and the plugins' `net.http` use them. `cargo audit` is clean again but
  for the known `paste` warning.
* **The admin API's HTTP reader is stricter about the head.** A line of blanks no longer ends the
  head (a proxy in front reads it as a folded line), and a `Content-Length` with a sign
  (`+5`) is refused.

### Fixed

* `world.tokens` accepted an amount of `i32::MIN`: `abs()` wraps in a release build, so the bound let it through. It is checked with `unsigned_abs`.
* `pcds plugin check` no longer reports an error for a plugin that reads its settings without `world.read`.
* A panic in the server's own code inside a plugin host function would have ended the whole process (the interpreter's frame cannot unwind).
  Every host function now runs under `catch_unwind` and a panic is that plugin's fault: counted, the plugin restarted, the game goes on. No such
  panic is known; 300 generated hostile modules in the tests found none.
* A rules file's command with `cooldown_seconds` silently had no cooldown without `world.read` (the clock is the world's); the check now says the
  rules need `world.read`, and a command without a cooldown no longer asks the world for the time.
* **The fuzz crate (`server/fuzz`) compiles again.** It had stopped building when the game tables
  became raw JSON (a missing `raw_value` feature) and when two test-only functions were used by
  the targets, so none of the fuzz targets could run. CI now type-checks it (`make fuzz-check`).
  Fifteen targets are new (`docs/testing.md`), and the game targets let game time pass.
* **A toxic area's key can no longer overflow.** The key a client registers an area under is any
  number; `i32::MIN` matched the area at the origin and `i32::MIN + 1` the one at `i32::MAX`
  (a build with overflow checks panicked in the tick).
* **Numbers in a save and in the messages between the front and the banks read back as they were
  written.** serde_json's default float parsing is an ulp off for one f64 in ten, so a load moved
  some of the world units' totals by an ulp and a save was not a fixpoint of load and save
  (`float_roundtrip`). The late-game bench save's fingerprint moves with it; the bench world's
  does not.
* **A deconstructed grower takes the network objects of its destroyed plants along.** The plants
  of a Farm or a VegetableGrower keep their network object when the items of the machine's
  inventory are destroyed (as the game does: they are the machine's children), but nothing
  removed them afterwards: they stayed in every client's scene, in every join and in every
  snapshot for a world object that was gone, for good.
* **A save whose lists hold something that is no record is refused at the start.** A string, a
  number or an array after the first entry of the players, world objects, messages... loaded, and
  the game's code that sets a field of such an entry panicked at the first autosave and every one
  after it: a world that plays and can never save.
* **A rules file or a rules change that is a JSON array is refused.** It was read as the sections'
  fields in order (`[[0.5]]` set `meteo.eventChance`).
* **A Steam library file nested without end** (`"a" {` a quarter of a million times) is refused
  with an error; it ended the process with a stack overflow.
* **The log cannot be used to steer a terminal.** The text format wrote a message as it came: a
  carriage return in it (a name, a bucket's error text, a release feed's message) moved the cursor
  over the line's own time and level, an escape sequence cleared or recoloured the screen of
  `pcds logs`, `journalctl` and `docker logs`. Control characters other than line breaks and tabs are
  now written as `\u{..}`. The error text of an S3 request is plain text too (control characters
  `?`, entities read, 200 characters a field).
* `make fuzz-check` and the CI job for it; `--features portable-dispatch` for the sanitizer
  builds (wasmi's default dispatch needs the optimizer's tail calls, which the sanitizers
  prevent).

## [0.8.0] - 2026-10-20

The DLC planets (Humble, Toxicity) run on their own ground, and you say which of them you own; the
server is harder to knock over, from a plugin, a game client or a bad file; the command line and the
dashboard answer mistakes in words; and the repository is ready for public releases (signed build
provenance, a changelog that is the release text, the policy files a public project needs). A
runbook for the update, [docs/upgrading.md](docs/upgrading.md), was rehearsed on the published 0.6.2
and 0.7.0 releases. This release cannot be hot-updated from any earlier one: see **Upgrade**.

### Breaking changes

* **Tables format 4 and snapshot format 7.** Release 0.7.0 cannot hand a running world over to
  0.8.0 (and 0.8.0 cannot hand one back): `pcds upgrade` refuses the hot update and names the
  restart install. Every player has to join again once. The way back to 0.7.0 is a restart
  install too (`pcds upgrade v0.7.0 --restart`), which reads the tables in format 3 again.
* **The tables are read again, from your game.** Format 4 holds the scenes of the DLC planets and
  the toxic areas of Toxicity, so the tables of 0.7.0 are refused at start. `pcds upgrade
  --restart`, `pcds restart`, `pcds start` and the Docker image read them again on their own
  (under a minute); the game has to be on the machine (in Docker, mounted at `/game`).
* **A world on a DLC planet does not start until you state that you own the planet.** The server
  cannot ask Steam about an account, so a save that lists Humble or Toxicity is no longer enough:
  run `pcds rules set world.dlcPlanets=Humble,Toxicity` (the planets you own). Without it the
  start stops and says so, and players cannot travel to a DLC planet. Such a world also needs the
  new tables (above); with old ones the start names `pcds extract`. Free planets are not affected.
* **Plugin authors:** a manifest text with a control character, a line break or a direction mark is
  refused; a data plugin (a rules file) over the new limits (64 commands, 64 `every`, 32
  `when_players`, 16 each of `on_join` and `on_leave`, 64 variables) is not loaded; host calls
  (`log`, `say`, `query`, `rules_patch`, `http_request`) now cost fuel, so a module that called
  them in a tight loop may run out of fuel; `rules.write` can no longer change `players.*` or
  `world.dlcPlanets` (docs/plugins.md).
* **Scripts that drive `pcds`:** `pcds rules` prints a table, not JSON (`pcds api GET /api/rules`
  gives the JSON); an unknown command, or an argument a command does not take, is an error with a
  non-zero exit instead of the help text (`pcds help`, `pcds --help` and `pcds` alone print the
  help and exit 0); `pcds api` takes GET and POST only; a world name must be letters, digits,
  `.`, `_` and `-` (at most 64, starting with a letter or digit), never a path.
* **Hand-written admin API clients:** a request head that can be read two ways is refused with 400
  (a folded header line, a space before the colon, two different `Content-Length`s) and a head over
  64 KiB with 431. `pcds`, the dashboard and ordinary HTTP libraries send neither.
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
* **The rule `world.dlcPlanets`** (empty by default) lists the DLC planets you own. It is your
  statement, not a check: nothing is faked or patched. A rules change that would take the planet
  from a world standing on it is refused, and `/api/status` shows the list as `dlcPlanets`. `pcds
  rules set` takes a list as plain names (`world.dlcPlanets=Humble,Toxicity`).
* **The rule `players.forgetAfterDays`** (default 0: off, nothing changes). Every new name leaves a
  saved player (about 1.2 KB) in the world for ever; with the rule on, a player who has been away
  that long, never crafted or earned a token, holds no more than the joining kit and is not the
  host or in the game is forgotten with those items. Everyone else is kept.
* **`pcds check-settings`** reads `server.env` the way the next start will and says what would stop
  it. Nothing is restarted.
* **Release engineering.** `CHANGELOG.md` is the release text; the release workflow stops when a
  tag has no section here, and takes the section as the body of the GitHub release (the generated
  notes follow below it). Build provenance is attested for the tarball and the image
  (`gh attestation verify`, see docs/hot-update.md "Verifying a release"), the Docker image carries
  BuildKit provenance and an SBOM, third-party actions are pinned by commit, and
  `THIRD_PARTY_LICENSES` lists the licence of every crate in the program and Valve's terms for
  `libsteam_api.so`.
* **Project files:** `SECURITY.md` (how to report a vulnerability), `CONTRIBUTING.md`, a code of
  conduct, issue and pull request templates, Dependabot for the workflows' actions.
* **Docs:** [docs/upgrading.md](docs/upgrading.md) (the update runbook from 0.6.x and 0.7.0, with
  rollback and Docker), [docs/troubleshooting.md](docs/troubleshooting.md) (from the line you saw
  to the command that fixes it), [docs/testing.md](docs/testing.md) (every test and tool, and how to
  repeat them), [docs/soak-report.md](docs/soak-report.md) (the final soak, chaos and update
  rehearsal, with the numbers) and the security review in [docs/audits.md](docs/audits.md), which
  also holds the four audits of 0.6.1 with the state of every finding.
* **Tools** (in the repository, not in a release): `tools/chaos` (soak, churn, chaos, disk-full,
  limits, stress and datagram-fuzz drivers), `tools/perf`, `tools/parity` (the sweep's replay and
  compare drivers), `tools/dev`.

### Changed

* **`pcds upgrade --restart` asks the new release before it stops anything.** It runs the new
  release's `pcds check-settings`; if the release would refuse `server.env` (a retired setting),
  the upgrade stops there, says what to fix and leaves the old server running. After the restart
  it waits for the world and says so when the server is back, or fails and points at `pcds logs`
  and the rollback when the unit gave up. Going from 0.6.x the old release does the asking, and
  it cannot: see **Upgrade**.
* **`pcds start`, `restart` and `upgrade --restart` clear systemd's start-limit count first.** An
  upgrade, a rollback and an upgrade again used to leave the unit failed with "Start request
  repeated too quickly" and the server down.
* **A retired setting is named with the rule that replaced it** (`PCDS_AUTOSAVE` is the rule
  `save.autosaveMinutes`), and a retired setting in a compose file's `environment:` is named in the
  log instead of being ignored without a word. Ignored it remains: it does not stop the start.
* **The command line and the dashboard answer in words.** `pcds help` lists itself and what `gui`,
  `token`, `config` and `api` do; an admin API failure says what to do (server stopped, wrong
  token, the server's own message); nothing set up says to run `pcds up`; `pcds rules set`
  confirms what it changed; `setup --world` checks the file first; a port that is in use stops the
  start in one sentence (UDP and the admin page); an `access.json` that does not parse says where;
  a failed Steam login says what to try. New worlds start on Prime or Selenea.
* **A stall of the server is a third of a second of game time at most**, as in the game (Unity
  bounds a frame the same way). After a SIGSTOP, a long save or a busy host the world used to take
  the whole gap as game time: the units jumped, the gauges drained about 40 times too fast until
  they caught up. A loop gap of a second or more is logged. docs/clock.md.
* **The explosives' scan costs nothing in a world with no explosive about.** It used to look at
  every world object ten times a second (72 % of an idle tick at 90,000 objects).
* **Plugins** pay fuel for the host's work, not only for their own: a host call costs 200 fuel and
  the bytes it copies or parses, and a bulk memory operation costs a quarter of a fuel a byte
  (wasmi charges 1/64). One chat event of a plugin that asked for nothing but `event.chat` used
  to hold the game thread for 44 s. A plugin's state is no longer written into a folder the admin
  removed. docs/plugins.md, "Limits".
* **Stricter reads.** A `banks/active` file that says neither `a` nor `b` is an error instead of
  bank `a`; a save with no known planet is refused instead of becoming Prime; bad hex is an error;
  a plugin folder's files are read as files only.
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
  of a server with invite codes (the admin page's Stop, `POST /api/stop`, `systemctl stop`) ended
  with status 139, a core dump and a crash report that the library tries to send to Valve, and
  because the unit has `Restart=on-failure` the admin page's Stop brought the server back after 15
  seconds. The front now closes its Steam connections, listen socket and login, in that order,
  before it exits. 0.7.0 and the releases before it have the crash; servers with `PCDS_STEAM=off`
  never had it.
* A plugin the admin removed (its folder deleted while it ran) came back as an empty folder at the
  next save, and the removed plugin kept running.
* A backup stamped in the future (the clock stepped back) no longer stops new backups; it is
  pruned first.
* An install that died between its two renames is made whole at the next start.
* `pcds status | head` ends quietly instead of panicking.
* **Dashboard:** on a phone the overview, map and tools pages no longer scroll sideways; a wrong
  admin token says so and is not kept; with the world stopped the overview, Config and ban list
  show the stopped state instead of `undefined`, an empty page and "nobody is banned"; the green
  badge and the primary buttons are readable in the dark theme and the light theme's colours reach
  4.5:1 contrast; Roll back is off when there is no earlier release; dialog fields have names for
  screen readers.
* The README, Compose file and image notes named Aqualis as the planet of a first world, which is
  refused; and the README matches the commands and the page (docs/troubleshooting.md).
* A connection request file that holds text that is not ASCII reads as zeros instead of panicking
  (only the tools that read `connection-request.hex` call it).
* The count of plugin requests in flight no longer uses `AtomicU32::fetch_update`, which Rust 1.98
  deprecates (a build warning, no change in behaviour).

### Security

An independent review of the code new since 0.6.2 (docs/audits.md, "Security review") found 27
problems, each proved by a failing test or a run against a scratch server and fixed with a test.
One is only bounded, not closed: UDP source addresses can be forged (see below). The open items
that remain are listed in docs/audits.md, "Open".

* **Plugins cannot stall the game.** Host calls cost fuel by the call and by the byte; a bulk
  memory operation costs by the byte; a data plugin's rules file is bounded in what each tick and
  event walks; a plugin folder with a named pipe or a device in place of a file no longer hangs
  the server; at most 16 name lookups wait at once for all plugins.
* **Approval is for what the admin saw.** `pcds plugin enable` and the dashboard keep the digest
  of the files they displayed, and the approval is refused if the files changed meanwhile
  (`digest` in `POST /api/plugins/enable`). A manifest's text cannot hide or forge lines of the
  approval on your terminal (control characters are refused; what the host says of a plugin is
  cleaned). The published example key is flagged wherever it is trusted: its private half is in
  the repository.
* **`net.http.local` never reaches the cloud metadata addresses** (169.254.169.254,
  100.100.100.200, fd00:ec2::254), which hand out the machine's credentials. A plugin cannot state
  which DLC planets you own.
* **Scoped tokens:** at most 16 event-stream consumer names (429 beyond), so a token that names
  itself anew at every poll cannot fill the table and the journal.
* **The admin HTTP head is read strictly** (400 and 431, above), and the admin page has a
  Content-Security-Policy of its own and sends `no-store`, `nosniff` and `no-referrer`.
* **Game clients cannot make the server grow without end.** Capped: scene wrecks a client creates
  (256), animal spawners and their traits, the toxic areas' registrations, the scene moves a
  client starts, and the world instance a client opens by RPC. RPC arrays over 4096 entries are
  not decoded and an RPC costs one more for each KiB. Transform and variable updates are relayed
  to the other clients only when whole and decodable, and an animator message over 8 KiB is not
  relayed. A panic in an RPC that was queued kicks that client and not the world. A long player
  name is cut between characters.
* **Joins from other machines are rate-limited.** A burst of the larger of 16 and `limits.maxPlayers`,
  then one a second; the rest are refused with "Many players are joining at once: try again in a
  moment". This bounds the amplification of a forged handshake (876 bytes in, 2.1 MB out before);
  it does not prove the source address is real.
* **Name bans and the whitelist** fold invisible blanks (no-break and ideographic spaces, U+3164,
  Braille blank), so a space no longer dodges a ban.
* **Files:** world files and kept copies made by the command line are 0600 like the saves the
  server writes; a symbolic link in the way no longer lends its target's mode to a saved file; a
  snapshot that claims more entries than bytes is an error and not an abort.
* **Updates and CI:** a release tarball with a hard-linked file is refused; the descriptors a peer
  sends over the bank socket are checked (a refused seal and a flood of descriptors break the
  link); every workflow action is pinned by commit, the checkout keeps no token and the default
  permission is read-only.

### Upgrade

This release is a restart install from every earlier one: the world is saved as the server stops
and loaded again, every player joins again (about a second of downtime after 10 to 20 seconds of
preparation while the server keeps playing). The full runbook, with the numbers measured, is
[docs/upgrading.md](docs/upgrading.md). Before anything: `pcds save`, and keep a copy of `run/`
(the worlds are in `run/worlds/`, the automatic backups in `run/worlds/backups/`). The game has to
be available to the server (the Steam library on the machine, `--game DIR`, or `/game` in Docker):
it reads the tables from it. Going back: `pcds upgrade --rollback --restart`.

**From 0.7.0:**

1. Back up (above) and have the game available.
2. `pcds upgrade v0.8.0 --restart` (or run the installer again, or unpack the tarball over the
   folder). Your `server.env` is already valid for this release.
3. `pcds status` shows 0.8.0 running and the tables of format 4; `pcds selftest` checks that a
   scripted player gets in.
4. If a world is on Humble or Toxicity, run `pcds rules set world.dlcPlanets=Humble,Toxicity`
   (the planets you own) and `pcds restart`; until then that world stops at start and says so.
   If it names `pcds extract` instead, run that first.

**From 0.6.x:** the old release cannot check what the new one will refuse, so **do the first step
before you run the upgrade**.

1. Back up (above).
2. Open `run/server.env` and remove every `PCDS_` line this release does not know:
   `PCDS_AUTOSAVE`, `PCDS_BACKUPS`, `PCDS_BACKUP_MINUTES`, `PCDS_MAX_PLAYERS`, `PCDS_AUTO_UPDATE`,
   `PCDS_UPDATE_CHECK_HOURS`, `PCDS_TRACE`, `PCDS_TRUST_LOOPBACK`, `PCDS_PORT`, `PCDS_ADMIN_BIND`,
   `PCDS_RULES`, `PCDS_ACCESS`, `PCDS_WEB` and `PCDS_NEW_WORLD` (what replaced each is in
   docs/upgrading.md, step 2). Note the values you had set. If you leave one, the new release
   refuses to start, systemd gives up after five tries and the server stays down until you fix
   the file (nothing is lost; then `pcds start`).
3. `pcds upgrade v0.8.0 --restart`.
4. Put your values back as rules: `pcds rules set save.autosaveMinutes=5 save.backupsKept=24
   limits.maxPlayers=16 update.auto=true` (your values; the defaults equal the old settings').
5. If a world is on Humble or Toxicity, `pcds rules set world.dlcPlanets=Humble,Toxicity`, as above.
6. A game save of the old layouts (2022, one planet), if you have one, goes through `pcds world
   import`. Plugins are new since 0.7.0: nothing runs until you install one.

**Docker and Compose:**

1. Stop the container and copy the folder you mount at `/data`; mount the game at `/game`.
2. Edit `./data/server.env` as in step 2 above when coming from 0.6.x. Delete the same names from
   `environment:` in your compose file: there they are ignored (one log line names each), so your
   old values would silently become the defaults.
3. `docker compose pull && docker compose up -d` (or `docker compose exec pcds pcds upgrade
   v0.8.0 --restart`, which installs the release into `/data/bin` and restarts in place). The new
   container reads the tables again: about 13 seconds until the world runs.
4. `docker compose exec pcds pcds status` and `docker compose logs --tail 30 pcds`; then set the
   rules (steps 4 and 5). Back: `docker compose exec pcds pcds upgrade --rollback --restart`, or
   put the old image tag in the compose file.

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
