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

### Fixed

* Scene inventories retain their fixed scene IDs, so keycard doors (including
  Selenea's three-card entry) stay open after revisiting a sector or reloading a
  save. Existing inventories created under an incorrect ID need save-specific
  recovery; the server cannot infer which door they originally belonged to.

## [0.8.5] - 2026-10-22

### Breaking changes

* None. Front protocol 3, snapshot format 7 and tables format 4 are unchanged.

### Fixed

* Remote players are announced once after their names are ready. This preserves map
  labels without destroying/recreating avatars, which could unregister their movement
  updates in the stock client and leave players invisible at each other's positions.
* Players still loading are kept private until ready. Their departure does not tear down
  avatars on observers who never received them.

### Upgrade

From 0.8.4 use the dashboard's server update or `pcds upgrade v0.8.5`. The world and
settings are preserved, and the server can hot-update without a restart. Reconnect
players who were already affected once after updating, to replace the frozen proxies
in their clients. No client mod is required.


## [0.8.4] - 2026-10-22

### Breaking changes

* Existing `access.operators` entries also authorize the new built-in server controls. Review
  the operator list before upgrading if those users previously had only plugin permissions.
* The chat command `!admin` (or `admin` with the configured prefix) is reserved for built-in
  server administration. Rename any plugin command using that name.
* Front protocol 3, snapshot format 7 and tables format 4 are unchanged; no restart is required.

### Added

* Player cards in the dashboard can grant and revoke admin access, show the current role,
  and explain when a player lacks a verified Steam identity. The existing `access.operators`
  list stores the roles; changes apply immediately and persist across reconnects and hot updates.
* Private `!admin` commands: `help`, `rule SECTION.RULE=VALUE`, `save`, `kick PLAYER`,
  `ban PLAYER`, `unban ENTRY`, and `tp PLAYER X Y Z`. Names with spaces and explicit
  `#client-id` targets are supported. Commands check the caller's current verified SteamID;
  a direct-IP name, claimed id or local trust cannot impersonate an admin.
* Chat rule changes use the existing validation and persistence. Host programs, secrets,
  backup credentials, DLC ownership and admin grants remain dashboard/console-only settings.

### Upgrade

From 0.8.3 use `pcds upgrade v0.8.4` to hot-update without disconnecting players, then refresh
an open dashboard page. In **Players**, click **Make admin** for a player who joined through a
Steam invite code, then use `!admin help` in game. Existing operator roles continue working.
Use **Remove admin** to revoke access immediately. The ordinary dashboard settings still
manage offline SteamIDs through `access.operators`.

Rollback to 0.8.3 retains the saved operator list; its plugin permissions still apply, while
built-in admin chat commands and the player-card controls are unavailable.

## [0.8.3] - 2026-10-22

### Breaking changes

None. Front protocol 3, snapshot format 7 and game tables format 4 are unchanged.
New behavior remains opt-in; existing rocket launches are unchanged with the rule off.

### Added

* **Live rule `rockets.serverFlight` (off).** All 21 orbital launch-platform rocket types
  can use server-controlled, collision-free ascent without a client mod, keeping their
  normal bonuses and space events. Travel and trading rockets retain their existing behavior.
  Toggle the rule while the server runs; it affects the next launch, and active flights
  finish normally after disabling it.
* The optional rocket ascent includes six seconds of orange flare smoke/light at the pad
  and the game's Crafting sound near the console (within 10 m). These approximate launch
  effects; the original engine flame and roar do not follow the rocket.

### Fixed

* With `drones.nearestFirst` enabled, parked drones now choose jobs by demand priority and
  shortest available pickup distance. Previously only airborne idle drones chose the nearest
  job; parked drones took jobs in registration order. Distances are reconsidered when a
  station releases its last drone. The rule remains off by default.


### Upgrade

From 0.8.2, use `pcds upgrade v0.8.3` for a hot update without disconnecting players.
Enable `rockets.serverFlight` through `pcds rules` or `POST /api/rules`; no restart is needed.
Existing drone rules and navigation maps continue to work.

Before a binary rollback to 0.8.2, disable `rockets.serverFlight` and let any active
server-controlled flights finish (40 game seconds). Disabling omits the new rocket setting
from persisted rules and handoff headers so 0.8.2 can read them. A rollback while the new
setting is enabled is refused safely and the current bank continues serving.

Validation includes all 21 rocket types, a real Unity client observing collision crossing,
smoke/light and audio playback, and a cross-release handoff with six scripted players.
Human visual/listening acceptance and a long real multiplayer soak remain open.

## [0.8.2] - 2026-10-21

### Breaking changes

* **Where this server now differs from the game, on purpose.** A launch or a destruction switched on
  while the rocket or destructor is already full, a building that is built after an optimizer's fuse
  was put in, a deconstruction that something was put into meanwhile, a joiner's growth boost and a few
  counts behave as described under **Fixed**; the game itself waits for the next item and the like.
  What drones do differently (their order, their flights, their lost cargo) is behind rules that are
  off. [docs/codex-integration.md](docs/codex-integration.md) gives each one with the game's own code.

* The published 0.8.1 snapshot format 8 is not accepted. A server already on 0.8.1 needs a
  restart install; servers still on 0.8.0 can hot-update directly to this release.

### Added

* **Rule `drones.keepCargo` (off).** What a drone carries when its delivery fails (the demand filled up
  or was taken down) is dropped on the ground, on the drone's own planet, where it can be picked up
  again, instead of being destroyed as the game does.
* **Rule `drones.nearestFirst` (off).** Drones choose their work by distance and priority instead of in
  the game's task order: an idle drone takes the job of the highest priority and, among those, the
  nearest pickup; a demand takes from the supply nearest to it; the nearest station with a parked
  drone sends it; a drone arrives by its own distance; a drone with nothing to do on one planet no
  longer stops the other drones getting jobs.
* **Rule `drones.avoidObstacles` (off) and `pcds drone-map GAME_DIR TABLES_DIR`.** The command reads
  the colliders of the terrain (including its heightmaps) and of the buildings from your own game into
  `drone-navigation.json` beside the tables; with the rule on, drones climb, cross above everything on
  their route and descend to dock instead of flying through terrain and walls. The rule is refused
  while the map is missing, malformed or of another game build, and the file is read only when the
  rule is on. Planning is limited to 16 drones a tick. See the README, "Differences from hosting in the game".

### Changed

* Reuse network-delta and poll buffers, avoid repeated energy-list allocation, and use compact drone clearance indexes. Existing timer deadlines and packet bytes stay unchanged.

### Fixed

* Short admin error responses are buffered before closing the socket, keeping the overload response body with its headers.
* Enabling automatic launch on an already-full trading or interplanetary exchange rocket launches it
  at once, without having to take an item out and put it back.
* Switching a destructor to destroy when full, while it is already full, empties it at once.
* Deconstruction checks the building's contents again when the animation ends: a chest that something
  was put in meanwhile is not taken down (the player gets the "not empty" message, no materials are
  refunded and the neighbouring panels stay as they were) instead of destroying what was put in.
* A rocket platform, generator or grower built after an optimizer's fuse was put in gets the fuse's
  boost without the fuse being taken out and put back; a joining player is told every grower's current
  growth boost.
* Picking up, moving or dropping a saved object no longer counts it again in the totals of objects of
  its group; replacing an object, saving it differently and deleting it keep the totals right.
* A joining player sees the other players' names and map markers: the other players' proxies are
  restarted for the joiner once the joiner's name is known (and the joiner's for the others).
* Steam shutdown keeps its server login alive while relay initialization completes, after closing connections and the listen socket. Cleanup is bounded to 15 seconds; stalled upstream initialization can still warn after that deadline. The fix requires a front restart.

* **Hot update from 0.8.0 works again.** The binary world state retains snapshot format 7;
  the added logging, backup verification and drone rules travel by name in an
  optional snapshot header. Operator permissions use the same extension while existing bans,
  whitelists and IP restrictions retain their old binary layout. The new bank accepts old snapshots with their original defaults
  and preserves the new settings through later handoffs. Invalid extensions, conflicting
  old rules and damaged snapshots are refused before the new bank becomes ready.
* The cross-release flip test keeps its players connected through the update and rollback.
  The flip gate fails on an incompatible format instead of exiting successfully without testing.

### Upgrade

From a running 0.8.0 server, skip 0.8.1 and use `pcds upgrade v0.8.2` once this release is
published. This updates the game bank while the existing front retains its sockets and players.
The front's Steam shutdown fix, monitoring routes, logging and service hardening require `pcds upgrade v0.8.2 --restart` to take effect; that restart disconnects players once.
An unmodified 0.8.1 front requires a restart to cross from its snapshot format 8; this release
refuses format 8 bodies rather than misreading them as format 7. Do not replace published
0.8.1 assets with this build.

A rollback to 0.8.0 can read the world state, but refuses rules.json if it contains the new
settings (or access.json contains `operators`). That failed handoff resumes the current bank.
Remove unsupported entries from rules.json and access.json before retrying; existing game rules and the world need no conversion.

Validation exercises scripted direct-IP players and native Steam login/shutdown; a real Steam-relay game-client session remains an acceptance gap.

## [0.8.1] - 2026-10-21

The operator's release. One command checks the setup (`pcds doctor`), the server has numbers and a
health route for monitoring, the log can be filtered and shipped, the systemd service and the Docker
container are sandboxed, backups are checked and restored from the command line, and the admin port
has limits. The server uses about half to a third of the memory and the late-game world costs a
fraction of the CPU it did. Plugins can do much more, and ten new example plugins show how. A third
round of fuzzing and review (13 findings), an acceptance run of every feature on a late-game world and
a 12-hour soak found and fixed the bugs listed under **Fixed** and **Security**. New documents:
[monitoring.md](docs/monitoring.md), [hardening.md](docs/hardening.md),
[disaster-recovery.md](docs/disaster-recovery.md), [reverse-proxy.md](docs/reverse-proxy.md),
[performance.md](docs/performance.md), [acceptance.md](docs/acceptance.md) and the Grafana dashboard
[docs/grafana/pcds.json](docs/grafana/pcds.json).

### Breaking changes

* **Snapshot format 8.** The rules are part of the state a hot update hands over, and three rules were
  added (`log.format`, `log.level`, `save.verifyBackups`), so 0.8.0 cannot hand a running world to this
  release, and this release cannot hand one back. `pcds upgrade` refuses the hot update and names
  `pcds upgrade VERSION --restart`; every player joins again once. The game tables (format 4) and the
  front protocol (3) are unchanged, so nothing is read from the game again.
* **Log lines start with a time, a level and a component** (`2026-10-20T18:07:37.354Z INFO  front: ...`).
  A program that matched the start of a line has to skip the prefix, or read `log.format=json`.
* **The systemd unit is sandboxed, and only the command line writes it.** `pcds restart`, `start`,
  `install-service` and `upgrade --restart` write the unit when a release changes it; the server no
  longer rewrites its own unit at start (its home is read-only to it, which is the point). A chat hook
  program that writes outside `run/`, or a Node or Java hook that you started with `MemoryDenyWriteExecute`
  set yourself, is affected: [docs/hardening.md](docs/hardening.md), "What a hook program or a plugin sees".
* **Docker and Compose are hardened by default.** `compose.yaml` sets `read_only: true` (with a `tmpfs` for
  `/tmp`), `no-new-privileges`, `cap_drop: [ALL]` with the six capabilities the image needs (the five that
  give `/data` to your user, and `KILL`, without which `docker stop` loses the world; see **Fixed**) and
  `pids_limit: 1024`. The entrypoint runs the server with `setpriv --no-new-privs`. If you copied the
  old compose file, take the new options or at least add `KILL`.
* **New routes.** `GET /api/health` answers without a token (`{"ok":true}`, nothing else) and
  `GET /api/metrics` needs a token; a proxy that exposes the admin port now exposes the health route too.
* **Plugin ABI 1.3, crate `pcds-plugin-api` 0.2.0.** A module built for 1.0, 1.1 or 1.2 runs unchanged.
  Rust source that names every field of `Event::Tick`, `PlayerJoin`, `PlayerLeave`, `Chat`, `Command`,
  `ObjectPlaced`, `ObjectDestroyed`, or builds a `WorldInfo` or `PlayerInfo`, must add the new fields (or
  use `..`). `Event::Command::command` is lower case, whatever the player typed. A setting in `[[config]]`
  of type `int`, `float` or `bool`, or with `choices`, is checked when it is set (a stored value that no
  longer fits makes the plugin `invalid`), and a number or a yes/no is never a secret, so the plugin can
  read it; a text is a secret unless it says `secret = false`. `[[every]]` in a rules file takes a text
  or a list, and `[[when_players]]` has `at_most`; files that worked still work.

### Added

* **`pcds doctor`** checks everything an operator needs in one command and prints PASS, WARN or FAIL
  with the line to type that fixes each problem: the game folder and its tables, the world file,
  `server.env`, `rules.json` and `access.json`, both ports (and whether the admin port is open to other
  machines), the modes of the files that hold secrets, free disk against the world and its backups,
  whether backups are on and leave the machine, the clock, the systemd unit and its sandbox, Steam
  (login token, `steamclient.so`, network), both banks, the release feed, plugins that wait or fail, the
  memory limit against what a hot update needs, and the open-files limit. With the server running it also
  asks the admin API (invite code, last save, backups, S3, crashes). It changes nothing; the exit code
  is 1 when a check FAILs; `--json` is for scripts.
* **Metrics and a health route.** `GET /api/metrics` answers in the Prometheus text format: players,
  tick p50, p99 and max, how busy the loop is, memory, objects, the last save (when, whether it worked,
  how long), backups and S3 copies, hand-overs and how long the world stood, crashes, watchdog kills,
  the queue of player traffic, plugins by state and cost, and version labels.
  `GET /api/health` needs no token and answers `{"ok":true}` while a world is served, and `503` while it
  is stopped, loading or starting again after a crash (a hot update does not turn it red). `pcds health`
  is the command form. [docs/monitoring.md](docs/monitoring.md) lists the metrics (a test keeps the list
  equal to what the server writes), a scrape configuration, 18 alert rules and a Grafana dashboard
  ([docs/grafana/pcds.json](docs/grafana/pcds.json)).
* **A scoped token for the scraper.** The scope `metrics.read` opens `GET /api/metrics` and nothing
  else (`pcds plugin token create prometheus --scope metrics.read`), so a monitor does not hold the
  admin token.
* **A log that can be filtered and shipped.** The rules `log.format` (`text` or `json`) and `log.level`
  (`error`, `warn`, `info`, `debug`) apply to the front and the game process at once and change while
  the server runs. A JSON line has `timestamp`, `level`, `component`, `message` and `fields`. Under
  systemd each line carries its priority, so `journalctl -p warning` works. `pcds logs` takes
  `-n LINES`, `--since TIME`, `--grep PATTERN` and `--follow`. [compose.yaml](compose.yaml) now rotates
  the container's log; docs/monitoring.md says how long journald, Docker and Kubernetes keep it.
* **Backups from the command line** ([docs/disaster-recovery.md](docs/disaster-recovery.md)).
  `pcds backup list`; `pcds backup verify` loads the newest backups with the loader the server uses and
  checks the S3 copy of the newest (exit 1 when one does not load); `pcds backup restore NAME` loads the
  backup first, keeps the world it replaces and refuses while the server runs (`--as WORLD` makes a new
  world instead); `pcds backup fetch [NAME]` copies one from the bucket. The new rule
  `save.verifyBackups` (off by default) reads each new backup back and reports it in the log and in
  the metrics `pcds_backup_verify_last_ok` and `pcds_backup_verify_last_timestamp_seconds`.
* **Three guides.** [docs/disaster-recovery.md](docs/disaster-recovery.md) (what to keep and how to restore
  on the same machine, on a new one, from the bucket and from a Docker volume; rehearsed on a scratch
  installation), [docs/hardening.md](docs/hardening.md) (the unit's sandbox, the Docker flags, Kubernetes,
  what a hook program or a plugin sees) and [docs/reverse-proxy.md](docs/reverse-proxy.md) (Caddy and nginx
  in front of the admin port, tried against a real server).
* **Plugins can do more** (ABI 1.3): read the world in `load`; know a player by a stable `player_id`; read
  the wall clock; see who built or took apart an object; look up one player, a backpack, the objects near a
  point, or one object; read their own settings; give items (`player.give`) and Terra tokens
  (`world.tokens`); set timers (`event.timer`); hear that a hot update handed the world over (`flip`);
  declare commands with a usage line, an argument count and a permission (`permission = "operator"`, the
  players in the new rule `access.operators`).
* **Plugin tooling.** `pcds plugin new` (Rust and C templates, a scenario), `test` (a scenario file drives
  the plugin in the real host against a made-up world, with no game tables), `check` (fuel of each event
  in a world of 16 players, secret settings it cannot read), `watch` (installs each new build), `logs`,
  `stats`, `pack` (one `.pcdsplugin` file), `search` and `install` (from a file, an https address with
  `--sha256`, or an index entry). `pcds-plugin-api` has a `testing` feature (`MockHost`, `Harness`) for an
  ordinary `cargo test`.
* **The dashboard's Plugins page** shows each plugin's log, faults, cost by event and typed settings, a
  per-row reload, and what a new version asks for beyond what you approved. `POST /api/plugins/logs`
  and richer `GET /api/plugins` back it.
* **Ten example plugins** in `examples/plugins/`: `afk-guard`, `playtime-board`, `quest-rewards`,
  `claims`, `restart-notice`, `chat-bridge`, `prometheus-exporter`, `hello-c`, `server-motd` and
  `scaling-rules`, each with a README and scenarios, and tested against the real game. New plugin
  documents: [docs/plugins-guide.md](docs/plugins-guide.md) (tutorial and cookbook), and the host
  functions with their costs in [docs/plugins-api.md](docs/plugins-api.md).
* **Documents and tests for efficiency.** [docs/performance.md](docs/performance.md) says what the server
  costs, how to profile it and how big a machine a world needs. Allocations, instructions per tick, bytes
  per world object and the wake-ups of an idle server are held by tests (docs/testing.md).
  `GET /api/status` has `traffic.wire`. [docs/acceptance.md](docs/acceptance.md) has every feature of the
  server tested on the late-game world, with the result of each row.

### Changed

* **The late-game world costs a fraction of the CPU.** On a save of 33,000 world objects with hundreds of
  machines, the idle bank uses about 8 times fewer user instructions (196 M to 24 M a second simulated; live
  380 M to 45 M), 4 players 203 to 32 M, 100 players 329 to 108 M, with the same messages and the same
  save, byte for byte. The logistics search for tasks and the scans of every world object were the cost.
  On smaller and grown worlds the gain is 10 to 50%.
* **The hot update stands for about 42% less work** (385 to 223 M instructions at 33k objects, 2,130 to
  1,239 M at 200k), and a start is faster (the game from the save 341 to 197 ms at 33k).
* **The server uses 50 to 67% less memory.** The game process on the same worlds: the bench world 43.8 to
  21.5 MiB, the late-game save 77 to 33, 100,000 objects 122 to 43, 200,000 objects 199 to 66 MiB. A hot
  update's peak is 52 to 59% lower on those worlds (the late-game save 154 to 74 MiB, 200,000 objects 437
  to 179). The tick costs the same and the snapshot is the same bytes. The front costs a third less at 64
  players and wakes up 20 times a second idle, not 50. [docs/performance.md](docs/performance.md) has the
  numbers, the byte accounting and a sizing guide (a container for a hot update: three times the bank).
* **Three limits a real player could meet are wider** (found by the acceptance run, from the traffic of a
  late-game join): RPCs a client may have waiting for later ticks 65,536 (was 8,192; the 4 MiB bound
  stays; a late-game world sends about 7,000 while it loads), players of one address joining at once 4
  (was 2), and new sessions per address in 10 seconds 6 (was 4).
* **The game's protocol noise is `debug`**: an RPC to an object without the class, a variable an object does
  not carry. It is no longer in the log unless `log.level=debug`.
* **The Docker health check and the Kubernetes probes use the health route.** The image's `HEALTHCHECK`
  runs `pcds health`, and [docs/kubernetes.yaml](docs/kubernetes.yaml) probes `GET /api/health` over HTTP
  (the liveness probe waits five minutes for a world). The admin port waits 2 seconds, not 3, for a
  request head.
* **`pcds load` and `pcds probe` take a batch of any size** (up to 256 MiB), as the game's client does, so
  a world of 200,000 objects can be joined by them. The server's own limit on a batch it receives is
  still 1 MiB.
* **Plugin faults say what happened and what to do** (the event, the budget used and the knob that raises
  it), and a module is translated when it is read, so the first call no longer costs several times the
  next ones and faults a budget that holds the steady state.

### Fixed

* **`docker stop` and `docker restart` lost the world under the hardened Docker flags.** With every
  capability dropped but five, tini (PID 1, root) could not pass SIGTERM on to the server (another user):
  exit 1 and no save. `KILL` is kept now, and `docker/test.sh` stops the container and wants exit 0 and a
  saved world. If you copied the flags into your own compose file or Kubernetes manifest, add `KILL`.
* **The game's own way to another planet did nothing.** A departure platform with the interplanetary
  rocket on its craft spawn started the launch (the clients saw it) but never switched the planet: the
  server looked for the rocket 22 m from where the game puts it. It looks at the craft spawn now.
* **A pick-up that no inventory takes left a world object behind.** When the inventory was full or refused
  the item, the object stayed in the world, held by nobody and never saved or freed (a long run with two
  players grew the snapshot from 0.43 MB to 2.5 MB). It is taken away again when the request fails.
* **A front that had been stopped killed the healthy game process when it ran again.** A front stopped
  for longer than `limits.bankWatchdogSeconds` (SIGSTOP, a paused virtual machine, a starved host) took
  its own absence for the game's silence and restarted the world from the last save. The watchdog starts
  the game's clock again after a gap of more than 5 seconds.
* **The `discord-notify` example switched itself off during a Discord outage.** A flush of the piled-up
  lines cost more than the default fuel of one event, so the plugin faulted until it was switched off. Its
  manifest now asks for 4,000,000 fuel an event.
* **`PCDS_DEV=flip-verify` failed the flips of a big world** (its check took more than the 5 s a new game
  process has to be ready). It is a test hook; production flips were not affected.
* **A `server.env` that lists only what differs works with the commands.** `pcds health`, `selftest`,
  `doctor`, `players`, `status` and `api` read the admin address, token and port as the server does. The
  container's health check stayed "starting" for ever with a hand-written `server.env`.
* `pcds rules reset` on a world standing on a DLC planet resets every rule but `world.dlcPlanets` and says
  so (it was refused as a whole).
* `pcds upgrade --restart` no longer says "the server is back" when the front went back to the other bank.
* `POST /api/update` with a version that is not text is a 400 (it started an update to the latest release).
* The dashboard's **Roll back** no longer says every rollback needs a restart.
* After `pcds plugin untrust`, a running plugin lists its signature as revoked at once.
* `world.tokens` accepted an amount of `i32::MIN`; `pcds plugin check` reported an error for a plugin that
  reads its settings without `world.read`; a rules file's command with `cooldown_seconds` silently had no
  cooldown without `world.read` (the check now says so).
* A panic in the server's own code inside a plugin host function would have ended the whole process. Every
  host function runs under `catch_unwind` now: the panic is that plugin's fault, the plugin restarts, the
  game goes on.
* The fuzz crate (`server/fuzz`) compiles again and CI checks it (`make fuzz-check`); fifteen new fuzz
  targets exist (docs/testing.md).

### Security

* **The systemd unit is sandboxed** ([docs/hardening.md](docs/hardening.md)): no new privileges, no
  capabilities, everything but `run/` read-only, a private `/tmp`, no devices, a system call filter, files
  private to your user, and limits on open files and tasks. `systemd-analyze --user security` scores the
  unit 2.1 (OK) where the 0.8.0 unit scored 9.4 (UNSAFE). Each option was tried with the real server (Steam
  login, players, saves, a hot update, S3, hooks in shell, Python, Node and Java, a plugin with `net.http`,
  `steamcmd`). Left out because they break something: `MemoryDenyWriteExecute` and
  `SystemCallArchitectures=native`; the document says how to add them. A machine without user mount
  namespaces gets the options that need none, and `pcds install-service` and `pcds doctor` say so.
* **Docker and Kubernetes:** the base images are pinned by digest, `docker/test.sh` runs its hot update
  with the hardened flags, and the Kubernetes example mounts no service account token.
* **The admin port, when it is open past the loopback:** other machines get 48 of the 64 connection slots
  and one address 16, so the command line and a proxy on the machine always find one; a connection over its
  share is answered `503` with `Retry-After` at once; a request head that does not come within 2 seconds
  ends the connection. The start-up warning says that the token can run programs on the machine, and
  `pcds doctor` warns.
* **No secret in the log.** The admin token, Steam token, chat secret, webhook URLs and S3 keys are replaced
  by `***` wherever they appear in a line, and so is anything that looks like a bearer or scoped token.
* **Round 3 of the audit** ([docs/audits.md](docs/audits.md): 503 million fuzzing executions of 34 targets,
  property tests against references, sanitizers) found 13 things (R1 to R13); all are fixed with a test:
  * `net.http` never reaches more of the cloud services that hand out credentials: Azure's WireServer
    (168.63.129.16, a *public* address), AWS ECS and EKS container credentials, Tencent's metadata and
    Google's IPv6 metadata server (R1).
  * TLS: rustls-webpki 0.103.16 and webpki-roots 1.0.10 (RUSTSEC-2026-0382 and RUSTSEC-2026-0381): the
    chain check accepted intermediates the root programs distrust. The updater, S3 and `net.http` use them (R13).
  * A deconstructed Farm or VegetableGrower takes the network objects of its destroyed plants along; they
    stayed in every client's scene, join and snapshot for good (R2).
  * A save whose lists hold an entry that is no record (a string, a number) is refused at the start; it
    loaded and then panicked at every autosave, so the world could never save (R3).
  * Numbers in a save and in the messages between the front and the game read back as written (serde_json's
    float parsing was an ulp off for one f64 in ten); a rules number past f32 is refused (R4).
  * A rules file or change that is a JSON array is refused; it was read by position (R5).
  * A toxic area's key can no longer overflow (R6).
  * The admin HTTP reader: a line of blanks no longer ends the head, and a `Content-Length` with a sign is
    refused (R7).
  * A Steam library file nested without end is refused; it ended the process with a stack overflow (R8).
  * The log cannot steer a terminal: control characters other than line breaks and tabs are written as
    `\u{..}`, and the error text of an S3 request is plain text (R11, R12).
  * Also: the fuzz crate builds again (R9), and the snapshot reader sets lengths to the count read (R10).

### Upgrade

Back up first: `pcds save`, then `pcds backup verify` finds a backup that loads, and `tar czf
pcds-settings.tar.gz -C run server.env rules.json access.json plugins` keeps the settings
([disaster-recovery.md](docs/disaster-recovery.md)). This release cannot be hot-updated from any
earlier one: the world is saved as the server stops and every player joins again.

**From 0.8.0:**

1. Back up (above).
2. `pcds upgrade v0.8.1 --restart`. It installs the release, writes the new unit (the sandbox of
   [hardening.md](docs/hardening.md)) and restarts the server. Your `server.env` needs no change.
3. `pcds doctor` checks the setup; `pcds status` shows 0.8.1.
4. If a hook program writes outside `run/`, see the sandbox note under **Breaking changes**.
5. Back to 0.8.0: `pcds upgrade v0.8.0 --restart` (it writes the unit of 0.8.0 again).

**From 0.7.0 or 0.6.x:** the old release cannot check what the new one refuses, so remove the retired
`PCDS_` lines from `run/server.env` **before** `pcds upgrade --restart` (the list and what replaced each
is in [docs/upgrading.md](docs/upgrading.md), step 2), then follow that runbook; use `v0.8.1` where it
names the tag. The game has to be available to the server (it reads the tables from it), and a world on
Humble or Toxicity needs `pcds rules set world.dlcPlanets=...` as in the 0.8.0 notes below.

**Docker and Compose:**

1. Stop the container and copy the folder you mount at `/data`; keep the game mounted at `/game`.
2. Take the new [compose.yaml](compose.yaml)'s security options (`read_only` with the `tmpfs` for `/tmp`,
   `no-new-privileges`, `cap_drop: [ALL]` with `cap_add: [CHOWN, SETUID, SETGID, DAC_OVERRIDE, FOWNER,
   KILL]`, `pids_limit`). If you keep your own flags, **add `KILL`**: without it `docker stop` loses the
   world. A Kubernetes manifest needs the same capability.
3. `docker compose pull && docker compose up -d` (or `docker compose exec pcds pcds upgrade v0.8.1
   --restart`; a hot update inside the container is refused for the reason above). The health check is
   `pcds health` now; if you wrote your own, use that or `GET /api/health`.
4. Check with `docker compose exec pcds pcds doctor`. Back: put the old image tag in the compose file.

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

[Unreleased]: https://github.com/perara/planet-crafter-dedicated-server/compare/v0.8.5...HEAD
[0.8.5]: https://github.com/perara/planet-crafter-dedicated-server/compare/v0.8.4...v0.8.5
[0.8.4]: https://github.com/perara/planet-crafter-dedicated-server/compare/v0.8.3...v0.8.4
[0.8.3]: https://github.com/perara/planet-crafter-dedicated-server/compare/v0.8.2...v0.8.3
[0.8.2]: https://github.com/perara/planet-crafter-dedicated-server/compare/v0.8.1...v0.8.2
[0.8.1]: https://github.com/perara/planet-crafter-dedicated-server/compare/v0.8.0...v0.8.1
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
