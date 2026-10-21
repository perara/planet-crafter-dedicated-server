# Upgrading to 0.8 (from 0.6.x or 0.7.0)

A step-by-step runbook for moving a running server to this release, and for going back. (Written and
rehearsed with 0.8.0; the steps are the same for 0.8.1, which is what the commands name.) It was
rehearsed for real: the published 0.6.2 and 0.7.0 releases were installed, played on by three
scripted players, and upgraded and rolled back with `pcds upgrade` on a systemd user unit and in
Docker (numbers below). The general rules of upgrading (banks, hot updates, `update.auto`) are in
the README's [Upgrading](../README.md#upgrading) and [Updating in Docker](../README.md#updating-in-docker);
how a hot update works inside is in [hot-update.md](hot-update.md).

**The short version.** Back up. Take the old `PCDS_*` lines out of `server.env` (this release
refuses to start with them). Run `pcds upgrade v0.8.1 --restart`. Check `pcds status`. Players are
disconnected for about a second and join again. If anything is wrong, `pcds upgrade --rollback --restart`.

## What players see

- A **hot update** (no `--restart`) keeps everybody connected; the world pauses for a few
  milliseconds. It is **not possible** for this jump: the world's snapshot format and the game
  tables' format changed (see "What changed"), so `pcds upgrade` without `--restart` refuses and
  says so, changing nothing.
- With `--restart` every player is disconnected when the server stops and has to join again. The
  server is back in about a second (measured: world of 33,000 objects). Steam players get a **new invite
  code** unless `PCDS_STEAM_TOKEN` is set in `server.env`; direct-IP players just join again.
- The world is saved when the server stops, and its state is what comes back. The old server's
  autosave interval is the most you can lose if the server is killed instead of stopped.

## 1. Before you start

1. See what runs: `pcds status` (the release and the bank), `pcds players`.
2. Pick the time: nobody is in the game, or you have told them. The server keeps playing while the
   release downloads and the tables are read (10 to 20 seconds); only the restart at the end disconnects.
3. **Back up.** The world is saved on stop, but keep a copy of everything you can lose:

   ```bash
   pcds save                                           # the world as it is now
   cp -a run run-backup-$(date +%F)                    # worlds, server.env, rules.json, access.json (also the banks and tables: a few hundred MB)
   # smaller: cp -a run/worlds run/server.env run/rules.json run/access.json /somewhere/safe/
   ```

   In Docker the same files are in the folder you mounted at `/data`: copy it with the container stopped.
4. Note the release you run now (`pcds status`, first lines). That is where `--rollback` goes back to.

## 2. Edit `server.env` (this is the step that matters)

This release **refuses to start** when `server.env` has a setting it does not know, and says which
ones. Releases before 0.7 wrote three of them into every `server.env`. Look:

```bash
grep -nE '^PCDS_(AUTOSAVE|BACKUPS|BACKUP_MINUTES|MAX_PLAYERS|AUTO_UPDATE|UPDATE_CHECK_HOURS|TRACE|TRUST_LOOPBACK|PORT|ADMIN_BIND|RULES|ACCESS|WEB|NEW_WORLD)=' run/server.env
```

Delete every line it prints (`pcds config` opens the file in your editor). What each one is now:

| Old line in `server.env` | Now | Default (what you get with the line gone) |
|---|---|---|
| `PCDS_AUTOSAVE=N` | rule `save.autosaveMinutes` | 5 |
| `PCDS_BACKUPS=N` | rule `save.backupsKept` | 24 |
| `PCDS_BACKUP_MINUTES=N` | rule `save.backupMinutes` | 60 |
| `PCDS_MAX_PLAYERS=N` | rule `limits.maxPlayers` | 16 |
| `PCDS_AUTO_UPDATE=on` | rule `update.auto` | false |
| `PCDS_UPDATE_CHECK_HOURS=N` | rule `update.checkHours` | 24 |
| `PCDS_TRACE=1` | rule `log.trace` | off |
| `PCDS_PORT`, `PCDS_ADMIN_BIND` | the port in `PCDS_LISTEN`, and `PCDS_ADMIN` | |
| `PCDS_RULES`, `PCDS_ACCESS`, `PCDS_WEB`, `PCDS_TRUST_LOOPBACK` | gone: `rules.json` and `access.json` are always in the installation folder | |
| `PCDS_NEW_WORLD` | the options of `pcds container` (Docker `command:`) | |

If a line had the default value (5, 24, 60 are what a 0.6 `setup` wrote), you are done with it.
If you had set another value, set it as a rule **after** the upgrade, when the new server runs:

```bash
pcds rules set save.autosaveMinutes=10 save.backupsKept=48     # takes effect at once, kept in run/rules.json
```

(Do not try before: the 0.6 release does not have these rules.) The other lines (`PCDS_LISTEN`,
`PCDS_STEAM`, `PCDS_STEAM_TOKEN`, `PCDS_SESSION`, `PCDS_ADMIN`, `PCDS_ADMIN_TOKEN`, `PCDS_WORLD`,
`PCDS_RELEASES`) stay as they are. Editing the file while the old server runs does nothing to it:
it is read at the start. Do the edit **before** the upgrade, not after, for the reason in the next
box.

> **Why before.** `pcds upgrade --restart` is run by the release that is installed now (it cannot
> know what the new one will refuse). It installs the new release, stops the server and starts the
> new release, which reads `server.env`. If the file still has an old line, the start fails:
> systemd tries again every 15 seconds, gives up after 5 starts, and **the server stays down**
> until you fix the file (rehearsed: 0.6.2 and 0.7.0 both, about a minute until systemd gave up).
> Nothing is lost; the world was saved. Recovery: delete the lines, then `pcds start`
> (`systemctl --user reset-failed pcds` first if the unit says "failed").
>
> From this release on, `pcds upgrade --restart` **asks the new release about `server.env` before
> it stops anything**: if the new release would refuse the file, the upgrade stops there, says what
> to fix, leaves the old release active and the server running (tested, and rehearsed with a stand-in
> release that retired a setting). `pcds check-settings` does the same check by hand for the release
> that runs.

Check the file once more: nothing it prints should be left.

## 3. Upgrade

```bash
pcds upgrade v0.8.1 --restart       # name the tag; without it you get the latest release
```

What it prints, and when the server is down:

1. `downloading v0.8.1 into bank b` ... `installed in bank b (sha256 checked)`: the server keeps playing.
2. `reading the game tables ...`: this release reads another tables format, so they are read again
   from your game folder (it must be installed on this machine; Docker: mounted at `/game`). The
   server keeps playing; this took 10 to 16 seconds.
3. `restarting the server: every player is disconnected and reconnects`: **the downtime**, about half a second to a second.
4. From this release on it then waits for the world and prints `the server is back after N s`; or, if the
   unit gave up, says so and points at `pcds logs` and the rollback. A release before 0.8 prints
   `server restarted` and leaves the checking to you (step 4).

Measured (systemd user unit, real game folder, 3 scripted players, world of 33,000 objects on 3
planets; bench world of 2,500 objects in brackets):

| Step | Command run by | Command total | Server stopped at | Back with players |
|---|---|---|---|---|
| 0.7.0 to 0.8 | 0.7.0 | 14 to 17 s (19 s) | after 14 to 17 s | within 0.5 s |
| 0.8 back to 0.7.0 (`--rollback --restart`) | 0.8 | 8.6 s (10 s) | after 7.6 s | within 0.5 s |
| 0.8 to 0.7.0 by tag, to 0.8, `--rollback`, to 0.8 again | | 11, 17, 9, 14 s | | within 0.5 to 0.6 s |
| 0.6.2 to 0.8 and back (bench world) | 0.6.2 and 0.8 | 16 s and 23 s | | server answers again 1 s after the command |
| a restart onto a release of the same formats | | 1.5 s | after 0.6 s | within 0.5 s |

"Back with players" is the time from the disconnect until three bots had joined again; it includes
the 0.3 s pause the bots make before they reconnect, so the server itself is back sooner. How long a real
game client takes to notice and rejoin was not measured. A start with the world of 33,000 objects: the
bank had the world running 0.3 s after it started.

## 4. Check

```bash
pcds status           # both banks listed; the new one marked [active, running]; "world ... on <planet>"
pcds players          # who is back
pcds code             # the new invite code (Steam), if you use one
pcds logs             # nothing red; "bank b (pcds 0.8.1) running the world"
```

Look at the world too: join the game, or `pcds api GET /api/status` and compare `objects` with what
you saw before. In the rehearsal a copy of the late-game world kept all its containers through eight
restarts in both directions.

If you set your own values for the old settings, set the rules now (step 2).

## 5. Go back

```bash
pcds upgrade --rollback --restart
```

The release you had is still in the other bank, so nothing is downloaded. The server restarts on
it (the same one second of downtime) after reading the game tables once more in the old format.
`--rollback` without `--restart` works only when both releases share the snapshot format, which
0.6 and 0.7 do not share with 0.8, so use `--restart`. The world file written by 0.8 loaded in
0.6.2 (the bench world) and in 0.7.0 (the late-game world with its three planets).

To try again after a rollback, `pcds upgrade v0.8.1 --restart` installs it again (no need to remove anything).

**If the server does not come back**, whichever way you went:

```bash
pcds logs                                    # what it said: usually a line about server.env or the game tables
systemctl --user reset-failed pcds           # clears systemd's "5 starts in 10 minutes" count (pcds start/restart do this themselves from 0.8)
pcds upgrade --rollback --restart            # or fix the file and: pcds start
```

## Docker and Compose

The same two steps apply, in the folder you mount at `/data`:

1. **Back up `./data`** (stop the container, copy the folder).
2. **Edit `./data/server.env`** like step 2 above. If your compose file sets any of the old names
   under `environment:` (`PCDS_AUTOSAVE: "10"`), delete them there too and set the rules after the update:
   in the environment they do not stop the start, they are **ignored**, with one line in the container log
   (`ignored in the environment: PCDS_AUTOSAVE is the rule save.autosaveMinutes now`). Your 0.6 values would
   silently be the defaults.
3. `docker compose pull && docker compose up -d` (or `docker compose exec pcds pcds upgrade v0.8.1 --restart`
   to install the release into `/data/bin` and restart in place: the container is not recreated).
   A hot update without `--restart` is refused for this jump, as above.
4. Check: `docker compose exec pcds pcds status` and `docker compose logs --tail 30 pcds`.
5. Back: `docker compose exec pcds pcds upgrade --rollback --restart`, or put the old image tag in the
   compose file and `docker compose up -d`. A pinned image tag does not pin the server after a hot update
   (docs/docker.md, "Which release runs after a new image").

What was measured (rootless Docker, the published 0.7.0 image and one built from this
repository's Dockerfile, the same `/data`, world of 33,000 objects): `docker stop` and removing the
old container 1.0 s; the new container to a running world 12.2 s, **ten of them reading the game tables
again** (the tables format changed), so players wait about 13 s in all, not one. Start the new
image on a copy first if that matters. With an old line left in `server.env` the new container did not
run: its restart policy started it again and again, each time printing
`/data/server.env: unknown setting PCDS_AUTOSAVE, PCDS_BACKUPS, PCDS_BACKUP_MINUTES (PCDS_AUTOSAVE is the rule ...)`;
deleting the lines from a second container (`docker run --rm --user 1000:1000 --entrypoint sed -v ./data:/data IMAGE -i '/^PCDS_AUTOSAVE/d' /data/server.env`)
or by `docker exec` lets the next try start the server (0.14 s after the file was fixed, in the rehearsal).
`docker/test.sh` (hot update, restart in place, rollback in the container) passes on the image built from the Dockerfile.

## What changed since 0.6.x

Things that can break a working setup, in the order you will meet them.

**Settings.** `server.env` keeps eight settings (the listen address, Steam, the Steam token, the
session digits, the admin address and token, the world name, the release feed). Everything else is a rule:
`rules.json` (changes live: `pcds rules set`, `POST /api/config`, the admin page). The table in step 2 is
the whole list. `server.env` with any other `PCDS_` name stops the start. Defaults of the rules equal
the defaults of the old settings. `docs/env.md` is the full list of what is left.

**Admin API.** `/api/settings` and `/api/access` are gone: use `GET/POST /api/config` and
`/api/rules` (access lists are the `access.*` entries of `/api/config`, and `/api/ban`, `/api/unban`).
Added: `/api/ping`, `/api/update`, `/api/world/start|stop|switch`, `/api/backup/s3`, `/api/plugins*`, `/api/chat*`.
`GET /api/status` has no constant `"standalone"` field any more, and has `banks`, `update`, `plugins`, `tick`,
`backup` and `lastSave`. [admin-api.md](admin-api.md) is the list; scripts that call the old routes get 404.

**Formats.**

| | 0.6.2 | 0.7.0 | 0.8.0 | 0.8.1 | the next release |
|---|---|---|---|---|---|
| world snapshot ("snapshot format" in the bank lines of `pcds status`) | 3 | 6 | 7 | 8 | 9 |
| game tables | 2 | 3 | 4 | 4 | 4 |
| front protocol | 3 | 3 | 3 | 3 | 3 |

A hot update needs the same snapshot format and tables format; across them `pcds upgrade` refuses and names
`pcds upgrade --restart VERSION`. Tables of another format are not used: the restart reads them again from the
game folder (the DLC planets' scenes are in the new ones). The game must be installed where the server finds
it (`pcds status` shows the folder); a server that cannot read its tables stays on the old release and says so
(the upgrade puts the old release back as the active bank). World saves (`worlds/*.json`) did not change format;
a save in the game's old layout (2022, one planet) is refused with a message (`pcds world import FILE` converts it).

**Hot update or restart.**

| You are on | Go to 0.8 with | Players |
|---|---|---|
| 0.6.x, 0.7.0 | `pcds upgrade v0.8.1 --restart` | disconnected, about a second |
| 0.8 and later, same formats | `pcds upgrade` (or `update.auto`) | stay connected |
| 0.8 back to 0.6.x or 0.7.0 | `pcds upgrade --rollback --restart` | disconnected, about a second |

`update.auto` (the old `PCDS_AUTO_UPDATE`) cannot do a jump that needs a restart: the update is refused and
the message names `pcds upgrade --restart`.

**New in 0.7 and 0.8 that you do not have to touch:** plugins (`pcds plugin`), `pcds rules`, chat hooks'
rules, S3 copies, the bank watchdog (`limits.bankWatchdogSeconds`: a game process silent for 60 s is
replaced), DLC planets (Humble, Toxicity) in the tables, and `pcds check-settings`.

**Not covered by the rehearsal.** The invite code of a Steam server (Steam was off in all runs: the code only
exists when Steam is on, and it changes at every start without `PCDS_STEAM_TOKEN`); a real game client rejoining;
the published GitHub download of 0.8 (a local release feed with the same layout stood in); 0.6.0 and 0.6.1
(0.6.2 was run); Kubernetes.
