# Planet Crafter dedicated server

A dedicated server for [The Planet Crafter](https://store.steampowered.com/app/1284190/The_Planet_Crafter/),
written in Rust. It speaks the game's network protocol and runs the host side of the game
itself: no game client, no Wine and no Steam account while it runs. Players join from the
unmodified game, by invite code or by IP.

The server runs what a host game normally does: terraformation and world units, machines and
growth, energy, logistics drones, meteo and meteor showers, day and night, story events and
messages, rockets, trading and interplanetary exchange, planet travel and world instances,
animals and vehicles, oxygen, thirst and health, death and respawn. It loads and saves worlds in
the game's own save format, so you can move a world between the game and the server either way.

This project is not affiliated with Miju Games. You need your own copy of the game: the server
reads the game tables it needs (items, buildings, scenes, network tables) from your installation
once per game version, and nothing from the game is distributed here.

## Quick start

One command on a Linux machine with the game installed through Steam:

```bash
curl -fsSL https://raw.githubusercontent.com/perara/planet-crafter-dedicated-server/main/install.sh | bash
```

It checks the machine and offers to install what is missing (curl, 32-bit libraries for
steamcmd) with your package manager. Then it downloads the latest release into
`~/planet-crafter-server` and sets everything up. It reads the game tables from your game's
files, which takes under a minute. It asks
whether to play one of your saves or a new world, and starts the server as a service that
survives reboots. It checks that players can get in, and prints the invite code and the address
to join. Run the same command again to upgrade; your worlds and settings stay. The server restarts then, so
players join again and the invite code changes (without a login token); `pcds upgrade` is the way to
update with nobody disconnected.

Options go after `bash -s --`, for example
`curl -fsSL .../install.sh | bash -s -- --world ~/Standard-1.json`:
`--dir DIR`, `--version vX.Y.Z` (a given release instead of the latest), `--world SAVE.json`, `--new [--planet ID]`, `--game DIR` (a game folder outside Steam),
`--steam-user NAME` (no game on this machine: download it with steamcmd and your Steam login),
`--yes` (install packages without asking).

Or with Docker, on any Linux with Docker:

```bash
docker run -d --name pcds --restart unless-stopped --stop-timeout 90 \
  -p 7777:7777/udp -p 127.0.0.1:7782:7782 \
  -v "$PWD/pcds-data:/data" \
  -v "$HOME/.local/share/Steam/steamapps/common/The Planet Crafter:/game:ro" \
  ghcr.io/perara/planet-crafter-dedicated-server
```

See [Docker](#docker) for the data folder, managing worlds and Compose.

## Requirements

- Linux x86_64 with systemd and glibc 2.34 or newer for the release build (Ubuntu 22.04,
  Debian 12, Fedora 35, RHEL 9 and later), or any machine with Docker. Run it as an ordinary
  user: `pcds` refuses to run as root.
- Your copy of The Planet Crafter for Windows: installed through Steam on this machine, the
  game folder copied from a PC (the folder that contains `Planet Crafter.exe`), or downloaded
  with `--steam-user NAME`.
- curl and tar (the installer and the Docker image bring them), for steamcmd.
- For invite codes: Valve's `steamclient.so`, from the Steam client if it is installed, else
  fetched with steamcmd (which needs 32-bit libraries such as `lib32gcc-s1`). Without it the
  server offers direct IP only.
- To build from source: [Rust](https://rustup.rs) and make.

**No Wine.** The server is native Linux code, and so is reading the game: once per game version
`pcds` reads the tables the server needs (network registrations, items, buildings, every
planet's scene, DLC planets included, the wrecks' tile sets) straight from the game's files, in
under a minute.

Tested with game version 2.103.

## Install by hand

From a [release](https://github.com/perara/planet-crafter-dedicated-server/releases), or from
source:

```bash
tar xzf pcds-v*-linux-x86_64.tar.gz && cd pcds-v*-linux-x86_64
./pcds up                                   # what the installer runs: everything below at once
# or: git clone https://github.com/perara/planet-crafter-dedicated-server.git && cd planet-crafter-dedicated-server
#     make install && pcds up               # ~/.local/bin/pcds; the server keeps its files in the checkout's run/
```

`pcds` is one program: the server itself (`pcds serve`) and the commands below. A release keeps
`libsteam_api.so` and `web/` next to it.

**Check the download.** Every release publishes the tarball's sha256 beside it (the installer
and `pcds upgrade` check it themselves) and GitHub's build provenance for it: which commit and which
workflow run built it. With the [GitHub CLI](https://cli.github.com/):

```bash
sha256sum -c pcds-v*-linux-x86_64.tar.gz.sha256
gh attestation verify pcds-v*-linux-x86_64.tar.gz --repo perara/planet-crafter-dedicated-server
gh attestation verify oci://ghcr.io/perara/planet-crafter-dedicated-server:0.8.2 --repo perara/planet-crafter-dedicated-server   # the Docker image
```

`up` takes the same options as `setup`. Step by step:

```bash
./pcds setup --world "/path/to/Standard-1.json"   # or: --new --planet Prime
./pcds install-service    # start now and at login, pick up game updates hourly
loginctl enable-linger $USER   # keep it running while you are logged out
./pcds selftest           # a scripted connection over direct IP is approved and gets the world
```

`setup` takes the game folder from `--game DIR` or `PCDS_GAME_DIR`, else the one it used last
(`run/game-dir`), else the Steam library that lists the game in a `steamapps/libraryfolders.vdf`
of `~/.local/share/Steam`, `~/.steam/steam` or the Flatpak's
`~/.var/app/com.valvesoftware.Steam/.local/share/Steam`. It reads the
game tables from it, puts the world in place, and writes the settings and a systemd user unit. Your
game folder is only read, and a modded one works too. `setup --world`
refuses while the server runs, and when the save is the current world already. The selftest does not become a player, so nothing is saved for it.

Where the game keeps its saves:
- Windows: `%USERPROFILE%\AppData\LocalLow\MijuGames\Planet Crafter\`
- Linux (Proton): `~/.local/share/Steam/steamapps/compatdata/1284190/pfx/drive_c/users/steamuser/AppData/LocalLow/MijuGames/Planet Crafter/`

## Run

```bash
./pcds start              # also: stop, restart, status, logs
```

### Upgrading

**Coming from 0.6.x or 0.7.0?** Read [docs/upgrading.md](docs/upgrading.md) first: this release refuses the autosave, backup and other old settings that earlier releases wrote into `server.env` (they are rules now), and the jump needs a restart, not a hot update. The page has the steps, the rollback and the downtime measured.

A new server release takes over the running world without disconnecting anyone (a hot update):

```bash
./pcds upgrade                # the latest release from GitHub
./pcds upgrade v0.6.0         # a given one
./pcds upgrade --rollback     # back to the release that ran before
./pcds upgrade --check        # the latest release, and what is installed
```

The server runs from one of two installed releases, the banks `run/bin/a` and `run/bin/b`
(`run/bin/active` names the one it starts; `pcds status` lists both). An upgrade downloads the
release and its published sha256 into the bank that is not running, checks it, and hands the
world over: the old release stops for about 15 ms, the new one takes the world and players see a
short pause. If the new release fails, the old one goes on. The release before stays installed,
so `--rollback` needs no download. A rollback is live only when that release reads the snapshot
format the running one writes (a release that changed it does not): when it does not, `--rollback` says
so and changes nothing, and `pcds upgrade --rollback --restart` goes back with a restart (the
server saves the world as it stops; every player has to join again). While the world is stopped
(dashboard: Stop world) there is nothing to hand over: start it first, or use `--restart`. A release that changes how the server's two processes talk
(its "front protocol") cannot take over a running world: `pcds upgrade` says so, and
`pcds upgrade --restart VERSION` installs it with a restart, which disconnects every player
(Steam players get a new invite code unless `PCDS_STEAM_TOKEN` is set). With the server stopped,
`pcds upgrade` makes the release the one the next start runs. The rule `update.auto` does this
by itself for each new release (off by default; `./pcds rules set update.auto=true`, which counts at once). It looks 10 minutes after the start
and then every `update.checkHours`, tries again within the hour when GitHub could not be reached,
and installs a release only when it is later than the one that runs (compared by its tag, or by
the program's version for a build) and not the one you rolled back from. In Docker, see
[Updating in Docker](#updating-in-docker).

Other ways, which all **restart the server** (every player joins again; `pcds upgrade` is the live one).
Read the release's notes first: [CHANGELOG.md](CHANGELOG.md) has a "Breaking changes" and an "Upgrade"
part for every release, and a release that changes the snapshot format or the tables format can
only be installed this way.
- Run the installer again (`install.sh`, the same command as the first install). It unpacks the new
  release over the program files in the install folder and runs `pcds up`; the worlds, settings
  and game tables in `run/` stay, and the new program goes into the standby bank and is started.
- By hand: `tar xzf pcds-vX.Y.Z-linux-x86_64.tar.gz --strip-components=1 -C ~/planet-crafter-server`
  and `./pcds up` there. Or unpack into a new folder, move the old folder's `run/` into it
  (or set `PCDS_HOME` to it) and run `./pcds up`. The tarball has no `run/`, so nothing is overwritten.
- From source: `git pull`, `make build`, then `pcds restart`.
- After a rollback (`pcds upgrade --rollback`) none of these installs the release you rolled back
  from: `pcds status` says which release runs; `pcds upgrade vX.Y.Z --restart` goes there on purpose.

### Uninstalling

```bash
./pcds uninstall-service   # stops the server (the world is saved) and removes its systemd units
rm -rf ~/planet-crafter-server    # the program and run/: worlds, settings, game tables, steamcmd
```

Copy `run/worlds/` (and `run/rules.json`, `run/access.json`) first if you want to keep them: a world
is a save in the game's own format, so the game can open it. A second server with its own `PCDS_HOME` and
`PCDS_UNIT` is removed the same way, with those set. `loginctl disable-linger $USER` undoes the
`enable-linger` of the install, and `rm ~/.local/bin/pcds` the link of `make install`. In Docker:
`docker rm -f pcds`, then delete the data folder (or `docker compose down` and delete `./data`).

The systemd units run `run/bin/pcds`, a copy of the program with `libsteam_api.so` and `web/`
beside it, so `make clean` or a removed release folder does not take the server down. It hands
every command to the active bank's program, so the server, `pcds run` (which starts and watches
it), the hourly game update check and the game tables' reader are always the release that runs,
however it was installed. `setup`, `up`, `install-service`, `start` and `restart` put a newer
build or release into the standby bank (never the one that runs the world) and make it the active
one, unless a bank holds that build already (so a rollback stays rolled back); an older one is
not installed, and says so (`pcds upgrade vX.Y.Z --restart` goes back on purpose). A release
whose server cannot start the world falls back to the other bank, which ran before (unless you
rolled back from it); `pcds status` then says which bank started and why.

## Docker

The image `ghcr.io/perara/planet-crafter-dedicated-server` (tags `latest` and each version)
holds the server and what the one-time table extraction needs. Everything the server keeps is in
`/data`, a folder you mount:

```
data/
  worlds/        one save per world; copy a save from the game here to add it
  worlds/backups/   timestamped copies of the worlds
  server.env     settings (see Settings); PCDS_WORLD names the world that runs
  rules.json     server rules        access.json   access lists
  data/<game version>/   the game tables, extracted on the first start
```

Mount your game folder read-only at `/game`. The first start extracts its tables, which takes
under a minute (`docker logs -f pcds`); later starts only extract again after a game update. Without
the game on the machine, start once interactively and download it with your Steam login:
`docker run -it -e STEAM_USER=yourname -v "$PWD/pcds-data:/data" ... ghcr.io/perara/planet-crafter-dedicated-server`.
From then on the container runs that download (`data/game`, updated at each start). The game
must be there at every start: the server runs only on the tables of the game it finds.

Manage it with `docker exec`:

```bash
docker exec pcds pcds worlds                 # the worlds in data/worlds, * = the one running
docker exec pcds pcds world use mybase && docker restart pcds
docker exec pcds pcds world new second --planet Selenea
docker exec pcds pcds world remove old       # kept in worlds/backups/
docker exec pcds pcds code                   # the invite code
docker exec pcds pcds players | rules | gui
```

The admin page listens on port 7782 inside the container; publish it on 127.0.0.1 only, as
above. The image's health check runs `pcds health`, which asks `GET /api/health` (no token; `docker ps`
shows it healthy once the world runs), so it needs `PCDS_ADMIN` on, as it is by default.
`--stop-timeout 90` gives the server time to save the world at `docker stop` (Docker's default
of 10 s would kill it during the save); Compose has `stop_grace_period: 90s`. `PUID`/`PGID` (default 1000) set who owns `data/` on the host. On the first start without
a world, the newest save in `data/worlds/` runs, or a new world is made (the arguments of the
container, for example `docker run ... IMAGE --planet Selenea` or Compose's `command: ["--planet",
"Selenea"]`, choose its options). [compose.yaml](compose.yaml) does the same with
`docker compose up -d`.

### Updating in Docker

Two ways, and they differ in what players notice:

```bash
docker exec pcds pcds upgrade          # a hot update inside the running container: players stay connected
docker compose pull && docker compose up -d   # a new image: the container is replaced, every player disconnects
```

- **Hot update.** `pcds upgrade` (also the admin page's **Update now**, or the rule `update.auto`
  (`docker exec pcds pcds rules set update.auto=true`), which checks daily and
  does it by itself) downloads the release
  into `data/bin` and hands the running world over to it; the world pauses for the flip. The
  container keeps running, so it needs the network to GitHub (`api.github.com` over https). It
  replaces the game; the part that holds the sockets and Steam keeps running until the next restart
  (`docker restart pcds`, or `docker exec pcds pcds upgrade --restart`, which restarts the server
  inside the container and disconnects everyone). A release that cannot take over a running world
  says so and names that command.
- **New image.** The image's release goes into `data/bin` and runs only if it is newer than what
  `data/bin` holds. So **a pinned image tag does not pin the server** once a hot update has run:
  the newer release from `data/bin` keeps running. The container's log and `docker exec pcds pcds
  status` say which release runs and why; `pcds upgrade vX.Y.Z --restart` goes back to a given one.
- A hot update needs spare memory (about three times the running game, which is 21 MB for a small
  world): with `--memory` set too low it is refused with the size to give, never killed.

[docs/docker.md](docs/docker.md) has the rule in full, what `docker stop`, `kill` and crashes do, running
with a read-only root and without root (`--user`, Kubernetes), and a Kubernetes manifest
([docs/kubernetes.yaml](docs/kubernetes.yaml)). `make docker-test` runs a hot update inside a
container end to end.

## Joining

- **Invite code:** `./pcds code` prints it; players enter it in the game's multiplayer join
  screen. Without a login token (see below) the code changes on every start.
- **Direct IP:** players join `HOST:7777` from the game's IP join screen. Forward UDP port 7777
  on your router for players outside your network. Direct-IP traffic is not encrypted.

## Settings

Settings are of two kinds. What the server must know before it can read any file is an
environment variable, the lines of `run/server.env` (`./pcds config` edits it, `./pcds restart`
applies it). Everything else, gameplay and tuning alike (saves, backups, updates, speeds,
limits, access lists), is a [rule](#server-rules): it lives in `run/rules.json` and
`run/access.json` and changes while the server runs, through `pcds rules`, `POST /api/config`
and the web page's Config tab. [docs/env.md](docs/env.md) lists every variable, including the
ones for tests and tools.

| Setting | Default | |
|---|---|---|
| `PCDS_LISTEN` | `0.0.0.0:7777` | direct IP (UDP); `off` disables it |
| `PCDS_STEAM` | `on` | invite codes through Steam; `off` disables them |
| `PCDS_STEAM_TOKEN` | empty | a [game server login token](https://steamcommunity.com/dev/managegameservers) (app 1284190) keeps the invite code the same across restarts |
| `PCDS_SESSION` | `4242` | the last four digits of the invite code |
| `PCDS_ADMIN` | `127.0.0.1:7782` | admin API and web page; `off` disables them. Plain HTTP: keep it on 127.0.0.1 and reach it from elsewhere through an SSH tunnel (`ssh -L 7782:127.0.0.1:7782 HOST`) or a TLS proxy ([docs/reverse-proxy.md](docs/reverse-proxy.md), with Caddy and nginx configurations); another address logs a warning at every start, `pcds doctor` warns, and other machines get at most 48 of its 64 connections, 16 per address |
| `PCDS_ADMIN_TOKEN` | generated | the admin API's bearer token |
| `PCDS_WORLD` | `world` | the world the server runs: `run/worlds/<name>.json` (`pcds world use NAME`) |
| `PCDS_RELEASES` | GitHub's API for this project | where `pcds upgrade` and the rule `update.auto` look up releases (a mirror) |

A value that is not what the setting takes (`PCDS_SESSION=five`, an address without a port)
stops the server at start, with the setting named in `./pcds logs`.

Where the installation is, set by the image or the unit that starts it, or in your shell for
every `pcds` command:

| Variable | Default | |
|---|---|---|
| `PCDS_HOME` | `run/` of the release or checkout folder | the folder the installation keeps everything in: `worlds/`, `server.env`, `rules.json`, `access.json`, `data/` and `bin/`. A second server on the machine gets its own |
| `PCDS_GAME_DIR` | the one used last, else the Steam library's | the game's folder (also `--game DIR`) |
| `PCDS_UNIT` | `pcds` | the systemd unit's name; with `PCDS_HOME` for a second server on the machine |
| `PCDS_CONTAINER` | unset | set by the Docker image: no systemd, the admin address may be public inside the container |

A name in `server.env` that is not a setting stops the server at start, in one line that names the
file and the name.

`run/server.env` holds the admin token and is readable by your user only.

The systemd unit `pcds setup` writes runs the server in a sandbox (no new privileges, no
capabilities, everything but `run/` read-only, a private `/tmp`, a system call filter), and the
Docker image drops privileges the same way: [docs/hardening.md](docs/hardening.md) says what is
on, what is left off because it breaks `steamcmd` or hook programs written in Node or Java, and how
to change it.

## Administration

```bash
./pcds players                 # who is online
./pcds save
./pcds kick NAME
./pcds ban NAME                # also bans the SteamID (invite code) or the IP (direct IP)
./pcds unban NAME_OR_IP
./pcds gui                     # the admin web page's address, with its token
./pcds token                   # the admin token alone, for curl and scripts
./pcds api GET /api/status     # any admin API route, through the token in server.env
./pcds status --json           # the whole status, for scripts
./pcds doctor                  # checks everything the server needs, with the fix for each problem
./pcds logs                    # the last 100 lines of the log, then new ones (Ctrl+C leaves)
./pcds logs --since "1 hour ago" --grep error   # what was said, then exit (-n LINES, --follow)
```

`./pcds help` lists every command. When a command cannot do what you asked, it says what to do;
[docs/troubleshooting.md](docs/troubleshooting.md) has the usual problems from the symptom to the fix.

`./pcds doctor` is the first thing to run on a new machine, after an update, and when something
is wrong. It prints one line per check, `PASS`, `WARN` or `FAIL`, and under each problem the line
to type that fixes it: the game folder and its tables (and that they are whole, with the DLC
ground when the world is on a DLC planet), the world file, `server.env`, `rules.json` and
`access.json`, the two ports (free, or this server's) and whether the admin port can be reached
from other machines, the file modes of the files that hold secrets, free disk space against the
world and its backups, whether the backups are on and leave this machine, the clock, the systemd unit and its sandbox and lingering, Steam (a login token, the
client library, the network), the two banks, whether the release feed answers, the plugins that
wait for approval or fail, the memory limit against what a hot update needs and the open-files
limit. With the server running it also asks the admin API: the invite code, the last save, the
backups, the off-site copies and the crashes. It changes nothing. The exit code is 1 when a check
FAILs and 0 otherwise (a `WARN` is advice); `--json` prints the same list for scripts and
monitoring. In Docker: `docker exec pcds pcds doctor`.

The admin web page has nine sections (keys 1 to 9 switch between them). **Overview**: players,
CPU, memory, loop load, loop rate and network traffic with live graphs of the last minutes, the
terraformation total with its stages and the time to the next one, every planet unit with its
rate, how to join (invite code, direct IP port), the server version with **Update now**, who is
online and the latest chat. **Players**: a card per player (how they joined, state, oxygen,
water, health and toxicity, position) with show on map, teleport (to coordinates or to another
player), kick and ban. **Map**: the planet from above with the buildings, the players moving with
their trails; drag, wheel or pinch to move and zoom, follow a player, and click a spot to teleport
someone there (the height comes from the nearest building). **Chat**: the chat as it happens, and
messages to everyone. **World**: pause, save now, back up now, stop and start the world (the page and API stay up),
shut the server down, the off-site copies, the worlds beside this one (make, run, download,
remove) and this world's backups (download, restore as a world), and the world settings. **Updates**: the running and latest release, check, update
now, install a given release or roll back (each a hand-over without disconnecting anyone), the
two banks and the update history. **Config**: every setting (rules, access lists; the
`server.env` settings read-only), drawn from the server's configuration schema, with search.
**Tools**: kick, ban and unban by name, the ban list, and the server log, live and filterable
(from the journal when it runs as the `pcds` unit). **Plugins**: the plugins installed, their
state, what each asks for and what it was granted, what it costs, and buttons to approve, switch
off or start one again. The page uses the admin API: `GET /api/config` lists every setting with its type, range,
unit, default, value and whether it applies live; `POST /api/config` with
`{"speed.growth": 2, "access.whitelist": ["Ann"]}` changes any of them (`null`: back to the
default), all or none.

Access lists live in `run/access.json`: `allowedIPs` (addresses or CIDR ranges allowed to
connect; empty means everyone), `whitelist` (names or SteamID64s; empty means everyone),
`bannedNames` and `bannedIPs`. The server reads the file at start and writes it on every ban,
unban or change from the web page: edit it by hand only while the server is stopped. A file
that exists but does not parse stops the start (it is never read as empty lists, which would
unban everyone): fix it or move it away.

- **SteamIDs are verified for invite-code players only.** A SteamID64 entry matches only that
  verified SteamID, never a player name. A banned SteamID is refused as soon as it connects.
- **Direct IP is not authenticated.** The game's reported SteamID cannot be checked there, so
  direct-IP players are identified by their name; anyone can join under any name that is not
  taken or banned. Use `allowedIPs` or turn direct IP off (`PCDS_LISTEN=off`) when that matters.
- `ban NAME` bans the name plus the player's SteamID (when they joined by invite code) or IP
  (direct IP). `unban` removes one entry: unban the name and the SteamID or IP separately
  (`./pcds unban 7656119...`).
- A second connection under a name or SteamID that is already playing is refused.

The admin API listens on 127.0.0.1. To reach the web page from another machine, use an SSH
tunnel (`ssh -L 7782:127.0.0.1:7782 server`) or a TLS reverse proxy
([docs/reverse-proxy.md](docs/reverse-proxy.md)) rather than exposing it: its token can set the
rule `chat.exec`, which runs a program on the machine.
Every route of the API, who answers it and what it takes: docs/admin-api.md.

**Monitoring.** `GET /api/health` needs no token and says only whether the server serves a world
(`200 {"ok":true}`, else `503`; a hot update does not turn it red), for container probes and
uptime monitors (`pcds health` is the same for scripts). `GET /api/metrics` gives players,
tick times, memory, saves, backups, hand-overs, crashes and the plugins in the Prometheus text
format; a token with the scope `metrics.read` (`pcds plugin token create prometheus --scope
metrics.read`) can read that and nothing else. [docs/monitoring.md](docs/monitoring.md) lists every
metric, a scrape configuration, alert rules and the Grafana dashboard
([docs/grafana/pcds.json](docs/grafana/pcds.json)).

To run a second server on the same machine, give it its own folder and unit names:
`PCDS_HOME=/srv/world2 PCDS_UNIT=pcds-world2 PCDS_LISTEN=0.0.0.0:7778 PCDS_ADMIN=127.0.0.1:7783
./pcds setup ...` (the two ports only seed its first `server.env`; `PCDS_HOME` and `PCDS_UNIT`
again for every later `pcds` command).

## Server rules

Rules change how the world plays, on top of the world's own game settings. They live in
`run/rules.json` and change while the server runs:

```bash
./pcds rules                                     # every rule, with the defaults
./pcds rules set speed.growth=2 gauges.thirstDrain=0.5
./pcds rules set limits.buildings.Drill4=10      # at most 10 of that building in the world
./pcds rules set death.consequences=null         # null: back to the default
./pcds rules reset                               # all back to the game's behaviour
```

Every default is the game's own behaviour, so without a `rules.json` nothing changes. A change
through `pcds rules` (or `POST /api/rules`, a JSON merge patch such as
`{"speed": {"growth": 2}}`) is checked first; a value out of range or a misspelled name is
refused. A `rules.json` edited by hand with such a mistake stops the server at start, with the
reason in `./pcds logs`.

| Rule | Default | |
|---|---|---|
| `meteo.eventChance` | `1` | chance of a random weather or meteor event at each try, times this; `0` stops them (events players cause still come) |
| `meteo.materialDespawnSeconds` | game's | seconds before the materials a meteor drops disappear; `0`: they stay. The rocks of an impact are drawn by each game itself and keep their time |
| `gauges.oxygenDrain` | `1` | oxygen use, times this (on top of the world's gauge drain setting) |
| `gauges.thirstDrain` | `1` | water use, times this |
| `gauges.foodDrain` | `1` | food (health) use, times this |
| `gauges.noDeath` | `false` | oxygen, water and health never go below 1 |
| `speed.terraformation` | `1` | what buildings add to every unit but energy, times this |
| `speed.machines` | `1` | extractors, collectors, auto-crafters and recyclers work this many times faster |
| `speed.growth` | `1` | plants in vegetubes, spreaders, farms and vegetable growers grow this many times faster than one with no growth fuse: the speed is added to the fuses' speed, as a growth fuse adds its own (a grower whose fuses are worth F grows (F + this) / F times faster; clients see the same growth). Incubators keep their time |
| `death.consequences` | world's | `NoConsequences`, `DropSomeItems`, `DropAllItems` or `DeleteSaveFile` |
| `death.dropChance` | `50` | with `DropSomeItems`: the percent chance each item drops (with 4 items or fewer, all drop) |
| `loot.amount` | `1` | items in generated containers (wreck safes, crates), times this, up to the container's size |
| `limits.maxPlayers` | `16` | players at once; more are refused with the game's session-full message |
| `limits.bankWatchdogSeconds` | `60` | a game process that says nothing (to a player, an admin request or the front's ping) for this long is killed and the world starts again from the last snapshot or save, as after a crash (it counts toward the crash limit). Not during a flip, a world stop or a save you asked for. `0`: never; else `10` to `3600` |
| `save.autosaveMinutes` | `5` | minutes between saves; `0` saves only on stop |
| `save.backupsKept` | `24` | save backups kept in `run/worlds/backups/`; `0` keeps none |
| `save.backupMinutes` | `60` | minutes between those backups: a save makes one when the last is older than this, so with `save.autosaveMinutes` 0 only the saves you ask for (and the stop) make one |
| `save.verifyBackups` | `false` | read each new backup back after it is written (on a thread of its own) and say in the log and the metrics whether it loads; `./pcds backup verify` does the full check on demand |
| `update.auto` | `false` | install each new server release and hand the world over to it (see [Upgrading](#upgrading)); off: nothing contacts GitHub on its own |
| `update.checkHours` | `24` | hours between those checks |
| `log.trace` | `false` | log every incoming message and RPC (debugging; a lot of output; written whatever `log.level` is) |
| `log.format` | `text` | how a line of the log is written: `text` (time, level, component, message) or `json` (one object per line: `timestamp`, `level`, `component`, `message`, `fields`). The front and the game process change together, at once |
| `log.level` | `info` | the lines written, this level and above: `error` (failures), `warn` (what needs a look), `info` (what happens: joins, saves, hand-overs), `debug` (and the protocol noise) |
| `limits.buildings` | none | building group id to how many the world may have; one more is taken down at once and its materials go back to the builder |
| `players.forgetAfterDays` | `0` | days away after which a saved player is forgotten (about 1.2 KB each). Only a player who never crafted or earned a token and holds no more than a new player is given (the starter items) is forgotten, with those items; anyone with more, the host and anyone in the game is kept for ever. `0`: every player is kept for ever. The days count from the day the rule is first on (players already saved have no time yet) |
| `world.dlcPlanets` | none | the DLC planets (Humble, Toxicity) you own, as names: `./pcds rules set world.dlcPlanets=Humble,Toxicity`. The server cannot ask Steam about an account, so this is your statement of ownership, not a check. A world on a DLC planet does not start unless the planet is listed, and players can travel only to the DLC planets listed. `/api/status` shows them as `dlcPlanets` |
| `world.pauseWhenEmpty` | `false` | with no player in the game the world stands still (time, day and night, growth, production, terraformation, meteo, timers and despawns, drones and vehicles) and goes on from there, with no catching up, when one is. A player counts once loaded in, as the game counts its players; one still loading sees the world as it stopped. An autosave of a world that stood still since the last one is skipped (no write, no backup) |
| `drones.nearestFirst` | `false` | drones choose their work by distance, not in the game's task order: an idle drone takes the job of the highest priority and, among those, the nearest pickup; a demand takes from the supply nearest to it; parked drones also take jobs by priority and shortest pickup distance from an available station, reconsidering distances when a station empties; a drone arrives by its own distance (the game uses the last-added drone's for all); and a drone with nothing to do on one planet no longer stops the others from getting jobs. Off: the game's order (see "Differences from hosting in the game") |
| `drones.avoidObstacles` | `false` | drones fly over the terrain and the buildings in their way (straight up, across, straight down); the game's drones fly straight through them. Needs the collision map that `pcds drone-map` makes from your own game; the rule is refused without it. Off: the game's straight flight |
| `drones.keepCargo` | `false` | what a drone carries when its delivery fails (the demand filled up or was taken down) is dropped on the ground, on the drone's own planet, where it can be picked up again. Off: it is destroyed, as the game does (a late-game world lost 79 items in five minutes to this in a test) |

## Chat hooks

Chat messages can go to tools outside the game (a bot, a Discord bridge, a log). The server
turns each message players send into an event and sends it from a thread of its own, so a slow
or broken hook never holds up the game. Hooks are server rules in the `chat` section, set like
any other rule, so only admins set them:

```bash
./pcds rules set 'chat.webhooks=["https://example.com/pcds-chat"]' chat.secret=CHANGE-ME
./pcds rules set chat.commandPrefix=!            # "!tp home" also comes as command "tp", args ["home"]
./pcds rules set 'chat.exec=["/usr/local/bin/on-chat", "--verbose"]'
./pcds rules set chat.webhooks=null chat.exec=null   # off again
```

| Rule | Default | |
|---|---|---|
| `chat.webhooks` | none | `http://` or `https://` URLs each event is POSTed to as JSON |
| `chat.secret` | none | signs every POST: `X-PCDS-Signature: sha256=<HMAC-SHA256 of the body, hex>` |
| `chat.exec` | none | a program and its arguments, run for each event with the event's JSON on stdin. No shell: chat text is never part of a command line |
| `chat.commandPrefix` | none | messages that start with it also carry `command` and `args` |
| `chat.timeoutSeconds` | `5` | the longest a webhook or the program may take (0.1 to 60) |
| `chat.retries` | `2` | more tries when a webhook gives no answer, 429 or 5xx, 1 s, 2 s... apart (0 to 10) |

An event:

```json
{"id": 12, "time": 1760781234567, "client": 3, "name": "Ann", "steamId": "76561198000000000",
 "planet": "Prime", "text": "!tp home", "command": "tp", "args": ["home"]}
```

`id` counts up from 1 at each start, `time` is Unix time in milliseconds, `client` the
connection, `steamId` the verified SteamID (invite-code players; `null` over direct IP),
`command` `null` and `args` empty when the message is not a command. To check a signature, in
Python: `hmac.compare_digest(header, "sha256=" + hmac.new(secret, body, hashlib.sha256).hexdigest())`.

A webhook that answers with a 2xx and `{"reply": "text"}`, or a program that prints text, puts
that text into the chat for everyone, from the server (500 characters at most). Tools can also
read events and write to the chat through the admin API:

```bash
curl -H "Authorization: Bearer $TOKEN" 'http://127.0.0.1:7782/api/chat/events?after=0&wait=30'
curl -H "Authorization: Bearer $TOKEN" -d '{"text": "Restart in 5 minutes"}' http://127.0.0.1:7782/api/chat
```

`GET /api/chat/events?after=ID&wait=SECONDS` answers `{"events": [...], "last": ID, "dropped": N}`
with the events after `ID`, waiting up to `wait` seconds (30 at most) when there are none yet.
Pass `last` as the next `after`. The server keeps the last 256 events. A gap in the ids means
you missed some. At most 8 requests wait at once (more get 503); a request without `wait` always answers. A webhook that fails (after its retries) is left out for 60 s, so it does not hold the other hooks back. One thread sends each event to every webhook and then to the program, in
order. When they fall more than 256 events behind, the oldest are dropped, counted in `dropped`
and logged. Webhook URLs, the secret and the program's arguments are never logged (the log and
`/api/status` show them as `(set)`). Webhooks get no redirects and no proxy, so events go only
to the URLs you set. Messages the server sends itself are not events.

## Plugins

Plugins add chat commands, reactions to players and the world, rule changes and integrations to
the server without changing it. A plugin is a folder in `run/plugins/`; it is one of three kinds:

- **a rules file** (TOML, no code): `!rules` answers with a text, a welcome message, an
  announcement every 15 minutes, faster growth while six or more play;
- **a WebAssembly module** (Rust, C, Zig...): logic that runs in the game, sandboxed, with a work
  limit per event, a memory limit and its state kept across restarts and hot updates;
- **a program of its own** that reads the server's events and uses the admin API: a database, a
  web page, a bridge to something that is not a web hook. It holds a **scoped token** that opens
  only what it needs (`pcds plugin token create NAME --scope events.read,chat.send`), and can wait for
  events with a long poll instead of asking every second.

A WebAssembly plugin can also reach the Internet itself, with the capability `net.http`: it names the
hosts it may send requests to in its manifest (you see them when you approve it, and they are pinned
with the files), the server sends the requests from its own threads and never lets one reach this
machine or your network (unless you grant `net.http.local` too), and a webhook URL or a token is a
value you set (`pcds plugin config set`), put in the request by the server so that the plugin never
has it. `examples/plugins/discord-notify` posts joins, leaves and `!report` to a Discord webhook that
way.

```bash
pcds plugin new greeter               # a Rust plugin from a template (--lang c, or --kind data: a TOML file)
pcds plugin test .                    # runs it in the real host against a made-up world, from scenarios/*.toml
pcds plugin check .                   # what it asks for, and what each kind of event costs in fuel
pcds plugin install .                 # copies it to run/plugins/ (it does nothing yet)
pcds plugin enable greeter            # shows what it asks for (and what is new in an update); you approve it
pcds plugin watch . --approve         # while writing it: every new build is installed, the state kept
pcds plugin list                      # state, capabilities, calls, faults
pcds plugin logs greeter -f           # the plugin's log
pcds plugin stats greeter             # what each event costs
pcds plugin config set greeter greeting Hi   # a setting a plugin declares (checked; a secret is read from the terminal)
pcds plugin pack .                    # greeter-0.1.0.pcdsplugin and its sha256
pcds plugin install greeter-0.1.0.pcdsplugin --sha256 HEX   # also: an https address, or an id with --index
pcds plugin keygen acme               # a publisher's key: acme.key (secret) and acme.pub
pcds plugin sign --key acme.key greeter   # admins who trust the key need not approve each file
pcds plugin token create bridge --scope events.read   # a token for a program, shown once
```

Plugins are loaded, switched off, replaced and removed with the server running: a folder put in
`run/plugins/` is found within a few seconds, and an approved plugin starts without a restart or a
hot update.

A plugin asks for **capabilities** (read chat, read the world, say in the chat, kick, move
players, give items and Terra tokens, set timers, change gameplay rules, refuse an RPC, send HTTP requests to
listed hosts) and does nothing until you grant them; it is approved for the files as they are, so a changed
plugin waits for you again (and the dashboard and `pcds plugin enable` show what the new version asks for beyond
what you approved), unless you trust its publisher's signature (optional; then what the key vouches for needs no
approval and the publisher's updates follow). Commands can be limited to your **operators**
(`pcds rules set access.operators=SteamID64,...`). The dashboard's Plugins page does all of this and shows each
plugin's log, faults and cost. A plugin can never write the world: it reads a copy and queues actions the server
applies through the same code the admin API uses. A plugin that crashes or runs too long is stopped in that call
and restarted from its last saved state, and switched off after five faults in a minute. With no plugin installed
the server does what it did before, and an event nobody listens to costs a fraction of a nanosecond.

docs/plugins-guide.md makes a first plugin in ten minutes and goes through the cookbook; docs/plugins-api.md is
the reference; docs/plugins.md says why it is built this way (with measurements, sharing, versions and the licence);
docs/architecture.md is the module map. Fourteen examples are in `examples/plugins/`. A change to the
front (scoped tokens, the long poll of the event stream) needs `pcds upgrade --restart` once: a hot
update does not replace the front (docs/hot-update.md).

## Game updates

Players and the server must run the same game version; players with another version are
refused with the game's version-mismatch message. After a game update, `./pcds update` reads the
new tables from the updated game and restarts the server, unless you stopped it; a server that
stopped on the old tables is started (`install-service` runs it every hour; in Docker, restart the
container).
On a machine without Steam, copy the updated game folder over and run
`./pcds update --game DIR`. Every game folder, from Steam or copied, is recognised by a hash of
its code (`Planet Crafter_Data/Managed/Assembly-CSharp.dll`). The server runs only on the tables
that match the installed game: after a game update it does not start until `pcds update` has
read the new ones, and it stops at start when a table file is missing. `pcds status` (and
`/api/status`'s `tables.warning`) says when the installed game has no tables yet. One extraction
runs at a time (it locks `run/.extract.lock`).

The tables are read by the release the server runs (the active bank's `pcds read-game`), and
each folder says which tables format wrote it (`tables-format`). A release that reads the tables
another way has another format: the server refuses tables of another format at start, `pcds
update` (and `pcds start`, `pcds restart` and `pcds upgrade --restart`) read them again, and a
hot update to such a release is refused with what to do instead. A game update that steamcmd
cannot download (`--steam-user` installs) fails `pcds update`, so the hourly unit shows it.

## Worlds and backups

- `run/worlds/` holds the worlds, one save per world in the game's format. `PCDS_WORLD` in
  `server.env` names the one the server runs (default `world`).
- `./pcds worlds` lists them; `./pcds world use NAME` switches (and restarts the server);
  `./pcds world import SAVE.json [NAME]` adds a save, as does copying it into `run/worlds/`
  (a save from the game's old layouts, from 2022 and before, has to be imported: see below);
  `./pcds world new NAME [--planet ID] [--start LOCATION] [--seed N] [--set KEY=VALUE]...` makes a
  new one with the game's Standard settings, on Prime or Selenea (the free starting planets; a
  refused planet or start location names the choices) (`--set` changes any game setting, for example
  `--set modifierGaugeDrain=0.5 --set dyingConsequencesLabel=NoConsequences`);
  `./pcds world remove NAME` moves one into `run/worlds/backups/`.
- `./pcds new [OPTIONS] --force` replaces the running world with a new one; the old one is kept.
- `run/worlds/backups/` keeps timestamped copies of each world (one per `save.backupMinutes`,
  the newest `save.backupsKept`), named by UTC time. Each start also backs up the world as it was
  before the server touched it, at most once per 10 minutes. The name has the second in it: a
  backup you ask for in the same second as the last (`POST /api/backup` twice) replaces it with the
  newer state. `/api/status` (`backup`) and the
  dashboard say when the last one was made and why one failed.
- `./pcds backup list` shows them. `./pcds backup verify` loads the newest three (`--count N`,
  `--all`) the way the server loads a world and says what each holds and whether the server would
  start it (exit 1 when one does not load; with a bucket set it also checks the bucket's copy).
  `./pcds backup restore NAME` puts one back as the world: it loads it first, asks, keeps the world
  it replaces as `<world>-replaced-<time>.json`, and refuses while the server runs;
  `--as WORLD` makes it a new world instead (also while the server runs). The rule
  `save.verifyBackups` reads each new backup back and says so in the log and the metrics.
  [docs/disaster-recovery.md](docs/disaster-recovery.md) has what to keep, restoring on a new
  machine, from the bucket and from a Docker volume.
- When the server catches a panic in a player's action (`./pcds logs` shows it; the player is
  kicked), the world may hold half of what that action did. It is not saved at once: the next
  save first keeps the world's file as it was as `NAME.json.before-panic` beside it, so that you can
  go back to it (stop the server and copy it over the world's file).
- **Old saves.** The game's save layout was another one before it had several planets (2022:
  nine sections, one player, no planets), and the game converts such a save when it opens
  one. This server does not read them as they are: the start, `world use`, the admin world
  switch, `setup --world` and the restore of a backup refuse one, naming
  `pcds world import SAVE.json`. That converts it the way the game does: the single planet
  becomes Prime; its units, the player, world objects, inventories, stats, messages and story
  events are kept; the game settings are made whole as the game makes them (a game mode stands
  for its dying consequences and gauge drain, and a world seed is drawn). A save of the current
  layout is copied as it is, and the old file is left as it was. What the game's own conversion
  loses is lost here too: in the oldest layout the technologies the player had unlocked (the
  game moves them into a record that has no such field; a save from between the layouts keeps
  them) and the colour layers.
- A server that keeps crashing stops after 5 starts in 10 minutes, so the older backups stay.
  `./pcds logs` shows why; `systemctl --user reset-failed pcds` and `./pcds start` try again.
- Access lists (`access.json`) and rules (`rules.json`) belong to the installation and apply to
  whichever world runs.

### Off-site copies (S3)

Every backup of the running world can also be copied to an S3-compatible bucket (AWS S3,
Cloudflare R2, Backblaze B2, MinIO...), so a lost disk does not take the backups with it. It is
off until `backup.s3Bucket` is set; the rules are set like any other (`./pcds rules set
backup.s3Bucket=my-bucket ...`, or the Config tab of the web page):

| Rule | Default | |
|---|---|---|
| `backup.s3Endpoint` | none | the service's address: `https://s3.<region>.amazonaws.com`, `https://<account>.r2.cloudflarestorage.com`, a MinIO address. Required |
| `backup.s3Bucket` | none | the bucket the backups go to. Unset: off |
| `backup.s3Region` | `us-east-1` | the bucket's region (`auto` for Cloudflare R2) |
| `backup.s3Prefix` | `pcds/` | put before each backup's file name in the bucket |
| `backup.s3AccessKey` | none | the access key id of a key that may write to the bucket. Required |
| `backup.s3SecretKey` | none | its secret access key. Required |

After each backup the newest one is uploaded from a thread of its own, so the game never waits
for the network; one upload runs at a time, and a backup made meanwhile follows it. The keys are
never logged, and `/api/rules` and `/api/status` show them as `(set)`. `GET /api/status` has an
`s3` part (whether it is on, what is still to set, whether an upload is running, the last
outcome). `POST /api/backup` saves the world and makes a backup now (answers 400 when
`save.backupsKept` is `0`) and copies it to the bucket; `POST /api/backup/s3` copies the newest
backup again (answers 400 when S3 is off or incomplete, or there is no backup yet). The web
page's World tab has both as buttons. `./pcds backup verify` asks the bucket whether the newest
backup is there with the file's size; `./pcds backup fetch [NAME]` copies a backup from the bucket
into `run/worlds/backups/` (the newest of this world without a name), for a new machine.

## Differences from hosting in the game

- **DLC planets (Humble, Toxicity) are on your word.** The game lets a host take its world to a
  DLC planet when the host's Steam account owns it. This server cannot ask Steam about an
  account (the invite-code server is an anonymous Steam game server), so you state what you
  own: `pcds rules set world.dlcPlanets=Humble,Toxicity`. Nothing is checked, faked or patched;
  the rule is your statement, and by default it is empty. A world saved on a DLC planet that is
  not listed does not start, and the message says which rule to set. Players can travel only to
  the listed DLC planets (a save that lists a planet is no longer enough), and `/api/status`
  shows them as `dlcPlanets`. The planet's ground (ore veins, water, toxic areas, the world
  instances of its portals) is read by `pcds extract` from your own game folder, like the free
  planets', into your tables; nothing of it is in this repository. A world on a DLC planet
  whose tables do not have the ground (read before this release) does not start, and says to
  run `pcds extract` (or `pcds update`): it would run on an empty planet. Toxicity's toxic
  areas (the goo players clean up by picking it up) and the toxic gauge in them work as in the
  game; the host game only knows the areas of the sectors around its own player, this server
  all of the planet's.
- **Old saves are imported, not opened.** The game converts a save of its old layouts (2022 and
  before) when it opens one; this server refuses such a save at every entry and
  `pcds world import SAVE.json` converts it first (see "Worlds and backups").
- **Machines do not switch off when no player is near.** The game switches off a machine with a
  `RequireEnergy` radius (219 objects on the late-game Humble, 25 to 320 m) when every player is
  further away than that; the server keeps every machine running (measured: the same extractors
  produced with one player at the base and with seventeen spread over the planet).
- **No physics on the server.** What a host game simulates itself (items still falling, parked
  vehicles, wild animals) is handed to the nearest player, by the game's own hand-over rules.
  Meteor impacts land where a player's game computes them.
- **Every player sees every animal,** where the game shows them only to players nearby.
- **Endings do not restart the session:** after a player's ending the server keeps running.
- **New worlds start from the Standard settings.** Other modes are a matter of `--set`.
- **Procedural wrecks are laid out a moment after a player enters them.** The game lays a wreck
  out from its seed with a solver (Tessera). The server runs a port of it on the wreck's own seed,
  with the tile sets `pcds extract` read from the game's files, so a seed gives the layout the
  game gives. The solve takes about 0.07 s for a small wreck, 0.1 s for a medium one and about a
  second for a large one (up to four on a busy host). It runs on a thread of its own, and the
  wrecks wait their turn one at a time, as in the game, so the tick is never held. What a wreck
  then holds (safes, containers, locked doors, generators) is picked from its seed as in the
  game; the loot in its containers is rolled at random, as there. A flip of the server in the
  middle of a solve starts that wreck again in the new process. A world from an older version of
  the game's generator loses its wrecks, as in the game.
- **Drones fly, choose and lose cargo as the game's do, unless you ask for better.** The game's
  drones go straight through terrain and buildings, take jobs in the order of a .NET dictionary and
  delete what they carry when a delivery fails. Three rules change that, all off by default.
  `drones.keepCargo` drops that cargo on the ground instead. `drones.nearestFirst` makes dispatch go by distance and priority
  (the rule's row in the table above says how). `drones.avoidObstacles` makes drones climb
  vertically, cross above the colliders on their route and descend vertically to dock. It needs a
  collision map made once from your own game, with the same installation that made the tables:

  ```bash
  pcds drone-map "/path/to/The Planet Crafter" "/path/to/data/2.103"
  ```

  The map is `drone-navigation.json` beside `gamedata.json` (about 55 MB for the six planets, 18 MB
  in memory). The server reads it only when the rule is on, and refuses the rule (when you set it,
  and at start) while the map is missing, malformed or of another game build than the tables; make
  it again after a game update or a fresh `pcds extract`. It holds geometry read from the game:
  keep it with your private tables, never in a repository. The map is conservative bounding
  boxes, including scene colliders that are inactive and may appear as the planet is terraformed, and terrain heightmaps cut into
  patches (holes stay covered), so a mountain can make a drone fly high and long. It is aerial
  clearance, not a shortest-path or doorway navigation: indoor pickup and docking may still cross
  walls, so enclosed storage works. Wreck interiors, moving scenery, vehicles, players and
  meteor debris are not in it. Planning is limited to 16 drones per tick, rotating through the
  fleet; a drone waits for its checked route instead of flying through an obstacle. The server
  indexes the obstacles by place, rebuilds the building index only after a build or a
  demolition, keeps the terrain index, and rebuilds the indexes after a hot update. Nothing of
  this is in the saved world or the snapshot.
- **A few slips of the game are fixed, because they lose items or leave clients stale.** The game
  checks a building's contents only when deconstruction starts; the server checks again
  when the animation ends and cancels (with the game's "not empty" message) if something was put
  in meanwhile. The game's optimizer boosts a building that is built later only if it makes units
  (a rocket platform, for one, does not, nor a grower without units); the server boosts a rocket
  platform, generator or grower built later in its range as well. A joining
  client is told every grower's current growth boost. Switching a destructor, or a trading or
  exchange rocket's automatic launch, on while it is already full acts at once (the game waits for
  the next item to go in or out). A saved object picked up or moved is counted once in the object
  totals (the game counts it again each time). Each of these is a separate commit in the history
  and listed in docs/codex-integration.md with the game's own code.
- **Remote players' map markers are refreshed after they join.** The game's client makes a
  player's marker only when that player's proxy starts with its roster entry present. The server
  restarts the other players' proxies for the joiner (and the joiner's for the others) once the
  joiner's identity is known, never the owner's own player.
- **Sleeping does not skip the night:** the game sends the server no signal for it.

Where the server has to approximate what the game's physics would decide (positions on a
platform, landing spots, seats, machine ranges), the code says so in a `ponytail:` comment.

## Known limits

- **Direct IP can be spoofed.** Unity Transport lets the client pick its connection token and
  the server's packet numbers start at 0, so someone who forges a player's address can start a
  session in their name without seeing the answers, and can acknowledge them blind. What that
  reaches is bounded: a session that acknowledges nothing gets a few KB and ends after 15
  seconds; one that acknowledges blind gets at most its world and 8 MiB more before PlayerReady;
  at most 4 sessions per address may be joining at once and 6 new ones start per 10 seconds.
  Invite codes (Steam networking) do not have this problem.
- **Behind a UDP relay** (playit.gg, frp, `ssh -R`) every player comes from 127.0.0.1: the
  per-address limits do not apply to loopback, and the SteamID a direct-IP client reports is
  believed from 127.0.0.1 only with `PCDS_DEV=trust-loopback` (for tests and the load and probe
  tools; never set it behind a relay).
- **Connection slots can be flooded.** The server caps connections (128 in all, 8 per IP, 10
  seconds to send data) to protect its memory; a flood can still keep new players out while it
  lasts.
- **Large worlds join over direct IP only.** The game's Steam transport takes at most 2.5 MiB
  in one message; a world whose join message is larger is refused over Steam (logged).
- **Clients are trusted with their own inventories**, as the game does: there are no ownership
  checks on inventory changes. Each client gets 32 units of RPC work per 5 ms tick (the rest
  waits), 8000 units a second of movement and variable updates (a message costs 1, and 1 more
  per 64 bytes; the rest is dropped) and 10 chat lines at once, one a second after that, 500
  characters each.

## How it works

- `server/` is the server (Rust). It implements Netcode for GameObjects' messages over Unity
  Transport (direct IP, UDP) and Steam networking sockets (invite codes: the code encodes the
  SteamID of an anonymous Steam game server). The game logic ports the game's host-side code,
  class by class.
- `server/src/native/` reads the game's tables from its files (Unity serialized files, the
  game's .NET metadata and IL) into `run/data/<game version>/`: `pcds read-game GAME_DIR OUT`.
- The server runs as two processes of the same program (docs/hot-update.md): the front (`pcds
  serve DATA_DIR SAVE.json`: the sockets, Steam and the invite code, the admin page, the chat
  hooks) and a bank (`pcds bank`: the world), started from the active release and replaced by a
  new one with the world handed over, while the front keeps every player connected.
- `server/src/plugins/` is the plugin host and `server/src/events.rs` the one place the game says
  what happened; the host lives in the bank, and a plugin's state goes through a hot update in the
  snapshot (docs/plugins.md). `plugin-api/` is the small library plugins are built with.
- `server/src/cli/` is the command line that ties them together, in the same program: `pcds
  serve` is the server, the other commands set it up and run it. It needs no
  Python; curl downloads and tar unpacks steamcmd, systemd runs the service.

## Development

```bash
make build      # server/target/release/pcds (make install links it as ~/.local/bin/pcds)
make test       # also: make lint, make plugin-bench, make gate, make docker, make docker-test, make dist (the release tarball), make clean
```

Before tagging a release: the release checklist in [docs/hot-update.md](docs/hot-update.md) ("Release checklist")
(`make test`, `make docker-test` and `make gate` need the game tables, so CI does not run them).

[docs/testing.md](docs/testing.md) describes every kind of test and check (unit and end-to-end tests,
fuzzing, the parity sweep, the benchmarks and their fingerprints, the efficiency budgets, the load and chaos
tools, Docker, the flip gate) and how to run each; [docs/performance.md](docs/performance.md) has what the
server costs, how to profile it and how big a machine a world needs; [tools/README.md](tools/README.md) indexes the scripts, and
[docs/audits.md](docs/audits.md) holds the findings of the v0.6.1 audits.

Tests that need game tables use `run/data/2.103` (or `PCDS_TEST_DATA`) and fail without
them; `PCDS_TEST_DATA=none` skips them, as CI does (the tables are not in the repository).
`src/coverage.txt` names the tests of every server RPC and every module; a test fails when an
RPC or a module is missing from it. `pcds probe HOST:PORT DATA_DIR SCRIPT.json` is a scripted player
that prints everything a server sends; `tools/probe_diff.py` compares two such runs (against a
real game host and this server, for instance). `pcds tables-diff EXPECTED ACTUAL` compares two
table folders section by section.

### Parity with the game

`server/tests/parity/` checks the game logic against the real game host: the same probe script
(`scripts/*.json`, one per mechanic family) runs against both, from the same save, and what
each sends back is compared.

1. **The reference host** is the game itself, headless under Wine and Xvfb, with BepInEx and
   the extractor plugin that once read the tables (`extractor/` at commit 4992b39; its
   GhostHost.cs hides the host's own player). Build it with the .NET 8 SDK as that commit's
   `pcds build` did, after adding a save endpoint to `Api.cs`:
   `case "POST /api/save": Extractor.OnMain(() => { Managers.GetManager<SavedDataHandler>().SaveWorldData(); return true; });`
   In `BepInEx/config/pcds.extractor.cfg` set `SaveName = Parity-1`, a free `Port` (UDP, the
   probe joins there) and `ApiPort`; put the start save in the Wine prefix as
   `AppData/LocalLow/MijuGames/Planet Crafter/Parity-1.json`; start the game as 4992b39's
   `pcds extract` did (`WINEDLLOVERRIDES="winhttp=n,b" xvfb-run -a -s "-screen 0 1280x720x24"
   wine "Planet Crafter.exe" -batchmode -nographics`) and wait for `PCDS READY` in BepInEx's
   log, then until `GET /api/status` shows the world units' rates standing still. Give the
   save a host record of its own (id 0, name Extractor, `"host": true`, at the escape pod), or
   the hidden host takes the saved host player's place. Without Steam the game owns no DLC.
2. **This server** on a copy of the same save, with `{"save": {"autosaveMinutes": 0}}` as
   `rules.json` beside it: `PCDS_LISTEN=127.0.0.1:PORT PCDS_STEAM=off PCDS_ADMIN=off
   PCDS_DEV=trust-loopback pcds serve DATA_DIR world.json`.
3. **Run a script** against both: `server/tests/parity/run.sh REF_ADDR OURS_ADDR DATA_DIR
   START_SAVE server/tests/parity/scripts/NAME.json OUT_DIR`. `parity.py` decodes every
   network variable with the tables and prints what differs: the approval (objects and their
   variables), the RPCs and spawns of each script step, and the variables' final values (those
   that drift with time, by their rate).
4. **The saves**: `POST /api/save` on the reference, stop ours (it saves), then
   `server/tests/parity/savediff.py START_SAVE REF_SAVE OURS_SAVE`: field by field, order
   included.

`server/tests/parity/SWEEP.md` is the exhaustive version: `sweep.py` writes probe scripts from the
game tables (every item, every constructible with each interaction its prefab carries, the
vehicle, the animals, the farms, two players sharing a container), `sweep.sh` runs them against
both and compares, and the matrix of group x interaction with the result of each row is in
`SWEEP.matrix.md`. The reference there runs with the extractor plugin's `PCDS_KEEP_ACTIVE=1`
(SWEEP.md says what that is). The sweep starts from `mkstart.py`'s start save (`start_full.json`
there: the user's save with a host record and every ore extractor filled), never `start.json`;
after a fix `sweep.sh ours` replays only this server against the stored reference runs.

Start both again from the start save for every script. `server/tests/parity/EXPECTED.md` lists
what still differs on purpose (the reference's host player, time, the game's randomness, the
safety rules); anything else is a bug.

`src/bench.rs` times the server's tick on a real save with a fixed clock and seed, and prints a
fingerprint of everything the server sends and saves. A change that should not alter the game
must leave the fingerprint as it was:

```bash
make bench SAVE=run/worlds/world.json PLAYERS=4
```

It also prints the time of a 5 ms step (mean, p50, p99), the allocations per step, and with
`PCDS_BENCH_ACTION_MS=200` the players act as the load test's do; `PCDS_BENCH_SECTIONS=1` adds the
time of each part of the tick (timers, units, energy, machines, meteo, ...). With
`PCDS_BENCH_PLAYERS=64` the player limit goes up to that.

`pcds load 127.0.0.1:PORT DATA_DIR [PLAYERS] [SECONDS] [ACTION_MS]` is a load test:
that many scripted players join over direct IP (each from its own 127.0.0.N address) and play:
they walk, craft, drop and pick up items, build, eat, drink, mine, chat and sort. It prints the
round trip of every call the server answers. `GET /api/status` shows the server's
`busyPercent` and `longestLoopMicros` over the last second.

`server/layers.txt` puts every module in a layer and `scripts/check-layers` (also part of `make
test`) fails on a module that uses what its layer must not (docs/architecture.md). `make
plugin-bench` measures what plugins cost; `scripts/build-plugin-examples` rebuilds the example
modules (`--check` fails when a committed `plugin.wasm` is not what its source builds).

To add a server rule: a field in `src/rules.rs` with its default, its entry in `config::KNOBS`
(`src/config.rs`: type, range, unit, description; the rules are checked against it and the admin
page shows it with no page code), read where the game decides that value, and a line in
`rules::changed` when a change must reach what already exists.

## License

GPL-3.0-or-later; see [LICENSE](LICENSE).

As an additional permission under section 7 of the GNU GPL version 3, you may link this program
with the Steamworks SDK (`libsteam_api`) and convey the result, without that part being
covered by this license.

Plugins: the `plugin-api/` library is licensed MIT or Apache-2.0 so that a plugin that includes it
keeps its own licence; what that means for closed-source plugins, and where the line to the GPL is
drawn, is in docs/plugins.md ("Licensing").

`libsteam_api.so`, which a release and the Docker image carry beside `pcds`, is Valve's Steamworks
redistributable, unchanged. It is **not** covered by this licence: Valve's terms apply to it (the
Steamworks SDK Access Agreement). The licences of the Rust crates inside `pcds` and that note are
in [THIRD_PARTY_LICENSES](THIRD_PARTY_LICENSES), which every release and the image include.

The Planet Crafter is a trademark of Miju Games. Steam and the Steamworks SDK belong to Valve.

Reporting a vulnerability: [SECURITY.md](SECURITY.md). Contributing: [CONTRIBUTING.md](CONTRIBUTING.md).
Changes by release: [CHANGELOG.md](CHANGELOG.md).
