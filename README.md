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

It checks the machine and offers to install what is missing (Wine, Xvfb, ...) with your package
manager. Then it downloads the latest release into `~/planet-crafter-server` and sets everything
up. It reads the game tables from your game, which takes a few minutes the first time. It asks
whether to play one of your saves or a new world, and starts the server as a service that
survives reboots. It checks that players can get in, and prints the invite code and the address
to join. Run the same command again to upgrade; your worlds and settings stay.

Options go after `bash -s --`, for example
`curl -fsSL .../install.sh | bash -s -- --world ~/Standard-1.json`:
`--dir DIR`, `--world SAVE.json`, `--new [--planet ID]`, `--game DIR` (a game folder outside Steam),
`--steam-user NAME` (no game on this machine: download it with steamcmd and your Steam login),
`--yes` (install packages without asking).

Or with Docker, on any Linux with Docker (the host needs no Wine):

```bash
docker run -d --name pcds --restart unless-stopped \
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
- Wine, curl, unzip and tar, and Xvfb on a machine without a display (the installer and the
  Docker image bring them). curl is only used for downloads (BepInEx, steamcmd).
- For invite codes: Valve's `steamclient.so`, from the Steam client if it is installed, else
  fetched with steamcmd (which needs 32-bit libraries such as `lib32gcc-s1`). Without it the
  server offers direct IP only.
- To build from source: [Rust](https://rustup.rs) and make. To rebuild the extractor: Docker.

**Why Wine?** The server is native Linux code and never runs under Wine. Once per game
version `pcds` reads the tables the server needs (network registrations, items, buildings,
every planet's scene) straight from the game's files, in seconds. Only the procedural wreck
interiors are laid out by the game itself, so for those `pcds` runs the Windows game headless
under Wine once, for a few minutes. After that Wine is not used until the next game update.
The Docker image carries its own Wine.

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
`libsteam_api.so`, `PcdsExtractor.dll` and `web/` next to it.

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
game folder is never modified, and a modded one works too: the extraction leaves the game's own
BepInEx, doorstop and `steam_appid.txt` out of its copy and brings its own. `setup --world`
refuses while the server runs, and when the save is the current world already. The selftest does not become a player, so nothing is saved for it.

Where the game keeps its saves:
- Windows: `%USERPROFILE%\AppData\LocalLow\MijuGames\Planet Crafter\`
- Linux (Proton): `~/.local/share/Steam/steamapps/compatdata/1284190/pfx/drive_c/users/steamuser/AppData/LocalLow/MijuGames/Planet Crafter/`

## Run

```bash
./pcds start              # also: stop, restart, status, logs
```

### Upgrading

- Run the installer again, or `./pcds up` in a newer release folder (move the old folder's `run/`
  into it first).
- From source: `git pull`, `make build`, then `pcds restart`.

The systemd units run `run/bin/pcds`, a copy of the program with `libsteam_api.so`,
`PcdsExtractor.dll` and `web/` beside it, so `make clean` or a removed release folder does not
take the server down. `setup`, `up` and `install-service` install the program there, and `start`
and `restart` install a newer build or release first.

From 0.5 or older (the bash `pcds`):
- The units ran the bash `pcds`, which is gone: the server does not start until
  `pcds install-service` (or `pcds up`) has written the units again.
- A game build is recognised by a hash of its code now: the server stops with "the game changed"
  until `pcds update` has extracted the game tables once more.
- A world still in `run/world.json` (before 0.5) is not moved any more: move it to
  `run/worlds/world.json` yourself.

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

Mount your game folder read-only at `/game`. The first start extracts its tables, which takes a
few minutes (`docker logs -f pcds`); later starts only extract again after a game update. Without
the game on the machine, start once interactively and download it with your Steam login:
`docker run -it -e STEAM_USER=yourname -v "$PWD/pcds-data:/data" ... ghcr.io/perara/planet-crafter-dedicated-server`.
From then on the container runs that download (`data/game`, updated at each start). The game
must be there at every start: the server runs only on the tables of the game it finds.

Manage it with `docker exec`:

```bash
docker exec pcds pcds worlds                 # the worlds in data/worlds, * = the one running
docker exec pcds pcds world use mybase && docker restart pcds
docker exec pcds pcds world new second --planet Aqualis
docker exec pcds pcds world remove old       # kept in worlds/backups/
docker exec pcds pcds code                   # the invite code
docker exec pcds pcds players | rules | gui
```

The admin page listens on port 7782 inside the container; publish it on 127.0.0.1 only, as
above. `PUID`/`PGID` (default 1000) set who owns `data/` on the host. On the first start without
a world, the newest save in `data/worlds/` runs, or a new world is made (`PCDS_NEW_WORLD`, for
example `--planet Aqualis`, chooses its options). [compose.yaml](compose.yaml) does the same with
`docker compose up -d`.

## Joining

- **Invite code:** `./pcds code` prints it; players enter it in the game's multiplayer join
  screen. Without a login token (see below) the code changes on every start.
- **Direct IP:** players join `HOST:7777` from the game's IP join screen. Forward UDP port 7777
  on your router for players outside your network. Direct-IP traffic is not encrypted.

## Settings

`./pcds config` edits `run/server.env`; `./pcds restart` applies it.

| Setting | Default | |
|---|---|---|
| `PCDS_LISTEN` | `0.0.0.0:7777` | direct IP (UDP); `off` disables it |
| `PCDS_STEAM` | `on` | invite codes through Steam; `off` disables them |
| `PCDS_STEAM_TOKEN` | empty | a [game server login token](https://steamcommunity.com/dev/managegameservers) (app 1284190) keeps the invite code the same across restarts |
| `PCDS_SESSION` | `4242` | the last four digits of the invite code |
| `PCDS_AUTOSAVE` | `5` | minutes between saves; `0` saves only on stop |
| `PCDS_BACKUPS` | `24` | save backups kept in `run/worlds/backups/`; `0` keeps none |
| `PCDS_BACKUP_MINUTES` | `60` | minutes between backups |
| `PCDS_ADMIN` | `127.0.0.1:7782` | admin API and web page; `off` disables them. `pcds` reaches it on 127.0.0.1, so bind it to 127.0.0.1 or 0.0.0.0 |
| `PCDS_ADMIN_TOKEN` | generated | the admin API's bearer token |
| `PCDS_MAX_PLAYERS` | `16` | players at once; more are refused with the game's session-full message (the `limits.maxPlayers` rule overrides it) |
| `PCDS_WORLD` | `world` | the world the server runs: `run/worlds/<name>.json` (`pcds world use NAME`) |
| `PCDS_RULES` | `run/rules.json` | the server rules file (see [Server rules](#server-rules)) |

`run/server.env` holds the admin token and is readable by your user only.

## Administration

```bash
./pcds players                 # who is online
./pcds save
./pcds kick NAME
./pcds ban NAME                # also bans the SteamID (invite code) or the IP (direct IP)
./pcds unban NAME_OR_IP
./pcds gui                     # the admin web page's address, with its token
```

The admin web page has five tabs. **Overview**: players, uptime, loop rate and load, CPU,
memory, network traffic, object counts, the server, game and table versions, the world, its
seed and settings, and the terraformation units. **Players**: who is online (ID, how they
joined, state, gauges, position) with kick, ban and teleport, and a map of the players among the
buildings. **World**: save now, save and stop, the worlds beside this one and this world's
backups. **Config**: every setting (rules, access lists, autosave; the `server.env` settings
read-only), drawn from the server's configuration schema. **Tools**: a chat message to everyone,
kick, ban and unban by name, and the server log (from the journal when it runs as the `pcds`
unit). The page uses the admin API: `GET /api/config` lists every setting with its type, range,
unit, default, value and whether it applies live; `POST /api/config` with
`{"speed.growth": 2, "access.whitelist": ["Ann"]}` changes any of them (`null`: back to the
default), all or none.

Access lists live in `run/access.json`: `allowedIPs` (addresses or CIDR ranges allowed to
connect; empty means everyone), `whitelist` (names or SteamID64s; empty means everyone),
`bannedNames` and `bannedIPs`. The server reads the file at start and writes it on every ban,
unban or change from the web page: edit it by hand only while the server is stopped.

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
tunnel (`ssh -L 7782:127.0.0.1:7782 server`) rather than exposing it.

To run a second server on the same machine, give it its own folder and unit names:
`PCDS_HOME=/srv/world2 PCDS_UNIT=pcds-world2 ./pcds setup ...` (and the same two variables for
every later `pcds` command), with other ports in its `server.env`.

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
| `speed.growth` | `1` | plants in vegetubes, spreaders, farms and vegetable growers grow this many times faster (clients see the same growth). Incubators keep their time |
| `death.consequences` | world's | `NoConsequences`, `DropSomeItems`, `DropAllItems` or `DeleteSaveFile` |
| `death.dropChance` | `50` | with `DropSomeItems`: the percent chance each item drops (with 4 items or fewer, all drop) |
| `loot.amount` | `1` | items in generated containers (wreck safes, crates), times this, up to the container's size |
| `limits.maxPlayers` | `PCDS_MAX_PLAYERS` | players at once |
| `limits.buildings` | none | building group id to how many the world may have; one more is taken down at once and its materials go back to the builder |
| `world.pauseWhenEmpty` | `false` | with no player in the game the world stands still (time, day and night, growth, production, terraformation, meteo, timers and despawns, drones and vehicles) and goes on from there, with no catching up, when one is. A player counts once loaded in, as the game counts its players; one still loading sees the world as it stopped. An autosave of a world that stood still since the last one is skipped (no write, no backup) |

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
with the events after `ID`, waiting up to `wait` seconds (60 at most) when there are none yet.
Pass `last` as the next `after`. The server keeps the last 256 events. A gap in the ids means
you missed some. A waiting request holds one of the admin API's 16 connections. One thread sends each event to every webhook and then to the program, in
order. When they fall more than 256 events behind, the oldest are dropped, counted in `dropped`
and logged. Webhook URLs, the secret and the program's arguments are never logged (the log and
`/api/status` show them as `(set)`). Webhooks get no redirects and no proxy, so events go only
to the URLs you set. Messages the server sends itself are not events.

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
read the new ones, and it stops at start when a table file is missing. One extraction runs at a
time (it locks `run/.extract.lock`): it uses ports 7791 and 7781 on 127.0.0.1, which a second
server on the same machine shares.

## Worlds and backups

- `run/worlds/` holds the worlds, one save per world in the game's format. `PCDS_WORLD` in
  `server.env` names the one the server runs (default `world`).
- `./pcds worlds` lists them; `./pcds world use NAME` switches (and restarts the server);
  `./pcds world import SAVE.json [NAME]` adds a save, as does copying it into `run/worlds/`;
  `./pcds world new NAME [--planet ID] [--start LOCATION] [--seed N] [--set KEY=VALUE]...` makes a
  new one with the game's Standard settings (`--set` changes any game setting, for example
  `--set modifierGaugeDrain=0.5 --set dyingConsequencesLabel=NoConsequences`);
  `./pcds world remove NAME` moves one into `run/worlds/backups/`.
- `./pcds new [OPTIONS] --force` replaces the running world with a new one; the old one is kept.
- `run/worlds/backups/` keeps timestamped copies of each world (one per `PCDS_BACKUP_MINUTES`,
  the newest `PCDS_BACKUPS`), named by UTC time. Each start also backs up the world as it was
  before the server touched it, at most once per 10 minutes. To restore one, stop the server and
  copy it over the world's file.
- A server that keeps crashing stops after 5 starts in 10 minutes, so the older backups stay.
  `./pcds logs` shows why; `systemctl --user reset-failed pcds` and `./pcds start` try again.
- Access lists (`access.json`) and rules (`rules.json`) belong to the installation and apply to
  whichever world runs.

## Differences from hosting in the game

- **Free planets only.** DLC planets need DLC ownership, which only the game itself has.
- **No physics on the server.** What a host game simulates itself (items still falling, parked
  vehicles, wild animals) is handed to the nearest player, by the game's own hand-over rules.
  Meteor impacts land where a player's game computes them.
- **Every player sees every animal,** where the game shows them only to players nearby.
- **Endings do not restart the session:** after a player's ending the server keeps running.
- **New worlds start from the Standard settings.** Other modes are a matter of `--set`.
- **Procedural wrecks come in 64 layouts per size.** The game lays wrecks out from a random
  seed with a solver the server does not run; `pcds extract` has the game lay out 64 seeds of
  each wreck size, and the server gives its wrecks those. What a wreck then holds (safes,
  containers, locked doors, generators, loot) is picked from its seed as in the game. Wrecks a
  world from the game had already generated keep what they hold.
- **Sleeping does not skip the night:** the game sends the server no signal for it.

Where the server has to approximate what the game's physics would decide (positions on a
platform, landing spots, seats, machine ranges), the code says so in a `ponytail:` comment.

## Known limits

- **Direct IP can be spoofed.** Unity Transport lets the client pick its connection token, so
  someone who forges a player's address can start a session in their name without seeing the
  answers. Invite codes (Steam networking) do not have this problem.
- **Connection slots can be flooded.** The server caps connections (128 in all, 8 per IP, 10
  seconds to send data) to protect its memory; a flood can still keep new players out while it
  lasts.
- **Clients are trusted with their own inventories**, as the game does: there are no ownership
  checks on inventory changes, and no rate limits on game messages.

## How it works

- `server/` is the server (Rust). It implements Netcode for GameObjects' messages over Unity
  Transport (direct IP, UDP) and Steam networking sockets (invite codes: the code encodes the
  SteamID of an anonymous Steam game server). The game logic ports the game's host-side code,
  class by class.
- `server/src/native/` reads the game's tables from its files (Unity serialized files, the
  game's .NET metadata and IL) into `run/data/<game version>/`: `pcds read-game GAME_DIR OUT`.
- `extractor/` is a BepInEx plugin that `pcds extract` runs in the game, under Wine, on a world
  of its own on 127.0.0.1, for the one table the game must compute itself: the procedural
  wrecks' layouts (wrecks.json). A prebuilt DLL is in `extractor/prebuilt/`.
- `server/src/cli/` is the command line that ties them together, in the same program: `pcds
  serve DATA_DIR SAVE.json` is the server, the other commands set it up and run it. It needs no
  Python; curl only downloads, unzip and tar unpack BepInEx and steamcmd, systemd runs the
  service.

## Development

```bash
make build      # server/target/release/pcds (make install links it as ~/.local/bin/pcds)
make test       # also: make lint, make docker, make dist (the release tarball), make clean
```

Tests that need game tables use `run/data/2.103` (or `PCDS_TEST_DATA`) and skip themselves
without them, as in CI. `pcds probe HOST:PORT DATA_DIR SCRIPT.json` is a scripted player
that prints everything a server sends. Run the same script against the extractor's game (a real
host, `127.0.0.1:7791` while `pcds extract` runs) and against this server, then compare the two
with `tools/probe_diff.py`. `make extractor` rebuilds the extractor plugin into
`extractor/prebuilt/` (with Docker, against the game in `run/extractor/game`).

`src/bench.rs` times the server's tick on a real save with a fixed clock and seed, and prints a
fingerprint of everything the server sends and saves. A change that should not alter the game
must leave the fingerprint as it was:

```bash
make bench SAVE=run/worlds/world.json PLAYERS=4
```

`pcds load 127.0.0.1:PORT DATA_DIR [PLAYERS] [SECONDS] [ACTION_MS]` is a load test:
that many scripted players join over direct IP (each from its own 127.0.0.N address) and play:
they walk, craft, drop and pick up items, build, eat, drink, mine, chat and sort. It prints the
round trip of every call the server answers. `GET /api/status` shows the server's
`busyPercent` and `longestLoopMicros` over the last second.

To add a server rule: a field in `src/rules.rs` with its default, its entry in `config::KNOBS`
(`src/config.rs`: type, range, unit, description; the rules are checked against it and the admin
page shows it with no page code), read where the game decides that value, and a line in
`rules::changed` when a change must reach what already exists.

## License

GPL-3.0-or-later; see [LICENSE](LICENSE).

As an additional permission under section 7 of the GNU GPL version 3, you may link this program
with the Steamworks SDK (`libsteam_api`), and the extractor with The Planet Crafter's, Unity's
and BepInEx's assemblies, and convey the result, without those parts being covered by this
license.

The Planet Crafter is a trademark of Miju Games. Steam and the Steamworks SDK belong to Valve.
