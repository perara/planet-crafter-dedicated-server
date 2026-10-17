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

## Requirements

- Linux x86_64 with systemd.
- Your copy of The Planet Crafter for Windows, installed through Steam on this machine, or the
  game folder copied from a PC (the folder that contains `Planet Crafter.exe`).
- Wine, curl, python3 and unzip. Wine is only used once per game version, to run the game
  headless and read its tables.
- For invite codes: Valve's `steamclient.so`. Install
  [steamcmd](https://developer.valvesoftware.com/wiki/SteamCMD) and run `steamcmd +quit` once
  (it creates `~/.steam/sdk64`), or have the Steam client installed. Without it, the server
  offers direct IP only.
- To build from source: [Rust](https://rustup.rs). To rebuild the extractor: Docker.

Tested with game version 2.103.

## Install

From a [release](https://github.com/perara/planet-crafter-dedicated-server/releases):

```bash
tar xzf pcds-v*-linux-x86_64.tar.gz
cd pcds-v*-linux-x86_64
```

Or from source (built with cargo on the first `setup`):

```bash
git clone https://github.com/perara/planet-crafter-dedicated-server.git
cd planet-crafter-dedicated-server
```

Then set it up with a world from your game:

```bash
./pcds setup --world "/path/to/Standard-1.json"
```

or with a new world:

```bash
./pcds setup --new --planet Prime
```

`setup` finds the game in your Steam library (or pass `--game DIR`), reads the game tables from
it, puts the world in place, and writes the settings and a systemd user unit. Your game folder
is never modified.

Where the game keeps its saves:
- Windows: `%USERPROFILE%\AppData\LocalLow\MijuGames\Planet Crafter\`
- Linux (Proton): `~/.local/share/Steam/steamapps/compatdata/1284190/pfx/drive_c/users/steamuser/AppData/LocalLow/MijuGames/Planet Crafter/`

## Run

```bash
./pcds start              # also: stop, restart, status, logs
./pcds selftest           # a scripted player joins over direct IP and checks the answers
./pcds install-service    # start at login and pick up game updates hourly
loginctl enable-linger $USER   # keep it running while you are logged out
```

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
| `PCDS_BACKUPS` | `24` | save backups kept in `run/backups/`; `0` keeps none |
| `PCDS_BACKUP_MINUTES` | `60` | minutes between backups |
| `PCDS_ADMIN` | `127.0.0.1:7782` | admin API and web page; `off` disables them |
| `PCDS_ADMIN_TOKEN` | generated | the admin API's bearer token |

## Administration

```bash
./pcds players                 # who is online
./pcds save
./pcds kick NAME
./pcds ban NAME                # also bans the SteamID (invite code) or the IP (direct IP)
./pcds unban NAME_OR_IP
./pcds gui                     # the admin web page's address, with its token
```

Access lists live in `run/access.json`: `allowedIPs` (addresses or CIDR ranges allowed to
connect; empty means everyone), `whitelist` (names or SteamID64s; empty means everyone),
`bannedNames` and `bannedIPs`. SteamIDs are verified for invite-code players only. Over direct
IP the game's reported SteamID cannot be checked, so those players are identified by their name.

The admin API listens on 127.0.0.1. To reach the web page from another machine, use an SSH
tunnel (`ssh -L 7782:127.0.0.1:7782 server`) rather than exposing it.

To run a second server on the same machine, give it its own folder and unit names:
`PCDS_HOME=/srv/world2 PCDS_UNIT=pcds-world2 ./pcds setup ...` (and the same two variables for
every later `pcds` command), with other ports in its `server.env`.

## Game updates

Players and the server must run the same game version; players with another version are
refused with the game's version-mismatch message. After a game update, `./pcds update` reads the
new tables from the updated game and restarts the server (`install-service` runs it every hour).
On a machine without Steam, copy the updated game folder over and run
`./pcds update --game DIR`.

## Worlds and backups

- `run/world.json` is the world, in the game's save format. Stop the server before replacing it.
- `./pcds new [--planet ID] [--start LOCATION] [--seed N] [--set KEY=VALUE]... [--force]` makes a
  new world with the game's Standard settings. `--set` changes any game setting, for example
  `--set modifierGaugeDrain=0.5 --set dyingConsequencesLabel=NoConsequences`. `--force` replaces an
  existing world; the old one is kept in `run/backups/`.
- `run/backups/` keeps timestamped copies of the world (one per `PCDS_BACKUP_MINUTES`, the newest
  `PCDS_BACKUPS`). To restore one, stop the server and copy it over `run/world.json`.

## Differences from hosting in the game

- **Free planets only.** DLC planets need DLC ownership, which only the game itself has.
- **No physics on the server.** What a host game simulates itself (items still falling, parked
  vehicles, wild animals) is handed to the nearest player, by the game's own hand-over rules.
  Meteor impacts land where a player's game computes them.
- **Every player sees every animal,** where the game shows them only to players nearby.
- **Endings do not restart the session:** after a player's ending the server keeps running.
- **New worlds start from the Standard settings.** Other modes are a matter of `--set`.

Not done yet (host-side generation that needs more data from the game than the extractor
reads today):

- **Procedural wrecks have nothing inside** (no safes, containers, locked doors or loot), and
  **portal world instances are empty**.
- **Habitat compartments built side by side do not open into each other.** Build a door or glass
  panel on the shared wall and deconstruct it to get the corridor.
- **Sleeping does not skip the night** (the game sends the server no signal for it).

Where the server has to approximate what the game's physics would decide (positions on a
platform, landing spots, seats, machine ranges), the code says so in a `ponytail:` comment.

## How it works

- `server/` is the server (Rust). It implements Netcode for GameObjects' messages over Unity
  Transport (direct IP, UDP) and Steam networking sockets (invite codes: the code encodes the
  SteamID of an anonymous Steam game server). The game logic ports the game's host-side code,
  class by class.
- `extractor/` is a BepInEx plugin that `pcds extract` runs in the game, under Wine, on a world
  of its own on 127.0.0.1. It dumps the network tables, the game data and every free planet's
  scene to `run/data/<game version>/`. A prebuilt DLL is in `extractor/prebuilt/`.
- `pcds` is the command line that ties them together.

## Development

```bash
cd server && cargo test --release
```

Tests that need game tables use `run/data/2.103` (or `PCDS_TEST_DATA`) and skip themselves
without them, as in CI. `pcds-server probe HOST:PORT DATA_DIR SCRIPT.json` is a scripted player
that prints everything a server sends. Run the same script against the extractor's game (a real
host, `127.0.0.1:7791` while `pcds extract` runs) and against this server, then compare the two
with `tools/probe_diff.py`. `./pcds build` rebuilds the server and the extractor.

## License

GPL-3.0-or-later; see [LICENSE](LICENSE).

As an additional permission under section 7 of the GNU GPL version 3, you may link this program
with the Steamworks SDK (`libsteam_api`), and the extractor with The Planet Crafter's, Unity's
and BepInEx's assemblies, and convey the result, without those parts being covered by this
license.

The Planet Crafter is a trademark of Miju Games. Steam and the Steamworks SDK belong to Valve.
