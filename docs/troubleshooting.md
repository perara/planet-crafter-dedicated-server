# Troubleshooting

Find the line you saw, read what it means, run the command. Every command here runs from the
folder with `pcds` (or as `pcds` when you ran `make install`); `PCDS_HOME` and `PCDS_UNIT` have to
be set the same way as for the server when you run a second one on the machine.

Three commands answer most questions: `pcds doctor` (checks everything the server needs, one line
each as PASS, WARN or FAIL, and the line to type under each problem; works when the server is
stopped and when it runs), `pcds status` (is it running, which world, the invite code, what is
wrong with the game tables) and `pcds logs` (what the server said, newest last; press Ctrl+C to
leave). Run `pcds doctor` first: most of the table below is what it already checks.

## The server does not start

| You see | It means | Do this |
|---|---|---|
| `no server is set up here (…/server.env is missing)` | Nothing was set up in this folder, or `PCDS_HOME` points somewhere else than the server's. | `pcds up`. For a server you set up before, run `pcds` with the same `PCDS_HOME` as then. |
| `game not found: pass --game DIR …` | No Steam library on this machine lists The Planet Crafter. | `pcds up --game "/path/to/The Planet Crafter"` (the folder that holds `Planet Crafter.exe`), or set `PCDS_GAME_DIR`. No game on this machine: `pcds up --steam-user NAME`. |
| `no game in … the game folder holds 'Planet Crafter.exe' and 'Planet Crafter_Data'` | The folder you named is not the game's. | Give the folder that has both. |
| `no game tables … run pcds update`, or `the game changed: run pcds update` | The game was updated (or the server reads its tables in a newer format) and the tables of the new version were not read yet. The server only runs on tables of the game it finds. | `pcds update`. It reads the new tables and restarts the server. |
| `tables format 3, the server reads format 4` | Same, after a server upgrade. | `pcds update`. |
| `…/worlds/world.json: …` and the world does not load (a file cut short, no game settings, objects the tables do not know) | The world file is damaged, or from a newer game than the tables. | `pcds backup verify --all` finds the backups that load; `pcds backup restore NAME` (server stopped) puts one back and keeps the damaged file. [disaster-recovery.md](disaster-recovery.md). |
| `server.env: unknown setting PCDS_…` | A line of `server.env` names something that is not a setting. Saves, backups, updates, speeds and limits moved to the rules. | `pcds config`, delete the line, `pcds restart`. Set the same thing with `pcds rules set …` (`pcds rules` lists them). |
| `server.env: PCDS_SESSION: "five" is not a whole number …` | A setting has a value it cannot take. The message says what it takes. | `pcds config`, fix the line, `pcds restart`. |
| `direct IP 0.0.0.0:7777: Address already in use` | Another program, often a second pcds server, already uses that UDP port. | Stop that one, or give this server another port: `PCDS_LISTEN` in `pcds config`, then `pcds restart`. |
| `admin page 127.0.0.1:7782: Address already in use` | The same for the admin page's TCP port. | Change `PCDS_ADMIN` in `pcds config`, then `pcds restart`. |
| `another server runs this world (process N …)` | Two servers on one world file would damage it, so the second refuses. | Stop the other (`pcds stop`, or kill process N), or run another world. |
| `no world at …/NAME.json` | `PCDS_WORLD` names a world that is not in `worlds/`. | `pcds worlds`, then `pcds world use NAME`. |
| `… the game's old save format … pcds world import SAVE.json` | The save is from the game's layout of 2022 and before. | `pcds world import SAVE.json`, then `pcds world use NAME`. |
| `rules: …/rules.json: speed.growht: no such rule` (or `not JSON`, or a value out of range), then `the world did not start` | `rules.json` was edited by hand with a mistake. It is never read as if it were empty. | Fix the line it names, or move the file away (the rules go back to the game's own), then `pcds restart`. |
| `access: …/access.json: not valid access lists (line 1, column 2)` | The same for `access.json`: unreadable lists would unban everyone, so the server stops. | Fix the file or move it away, then `pcds restart`. |
| The unit stops after a few starts and `systemctl` says `start-limit-hit` | The server crashed 5 times in 10 minutes. The older backups are kept. | `pcds logs` for the reason, then `systemctl --user reset-failed pcds` and `pcds start`. |
| `no systemd user session here: start the server with 'pcds run'` | A machine without a systemd user session (a plain SSH login on some systems, a container). | `pcds run` keeps the server in that terminal; use `tmux` or the Docker image to leave it running. |

When the world does not start, the reason is a few lines above the last message in `pcds logs`. Narrow it with `pcds logs --since "1 hour ago" --grep error`; the log's format and level are the rules `log.format` and `log.level`, and how long it is kept is in [monitoring.md](monitoring.md#the-log).

## Players cannot join

| Symptom | Cause | Do this |
|---|---|---|
| `pcds status` says `no invite code: Steam did not log in` | The server could not log in to Steam as a game server. `pcds logs` has a line starting `steam:`: no `steamclient.so`, no connection to Steam within 30 s, or a `PCDS_STEAM_TOKEN` that Steam does not accept. Direct IP keeps working. | No `steamclient.so`: install the Steam client or run `pcds setup` again (it fetches it). Offline: fix the network. A token: it has to be a game server login token for app 1284190 (steamcommunity.com/dev/managegameservers); leave it empty to log in anonymously. `PCDS_STEAM=off` serves direct IP only. |
| The invite code is different after a restart | Without `PCDS_STEAM_TOKEN` the server is a new anonymous Steam game server at every start. | Put a login token in `PCDS_STEAM_TOKEN` (`pcds config`, `pcds restart`): the code stays the same. `pcds code` prints the current one. |
| `pcds logs` starts with `dlopen failed trying to load: steamclient.so` | Steam's own library tries a local file first. | Harmless when the next line says `SteamAPI_Init(): Loaded … OK`. |
| Direct IP works at home but not for friends | UDP is not forwarded, or a firewall drops it. | Forward UDP port 7777 (the `PCDS_LISTEN` port) on your router and allow it in the firewall. `pcds selftest` only checks from this machine. |
| Players are refused with a version message | Their game and the server run different game versions. | After a game update: `pcds update` on the server, and the players update their game. |
| Refused with "You are not allowed on this server" | A rule of the access lists: `allowedIPs`, `whitelist` or a ban. `pcds logs` shows `refused ("Name")`. | Look under Config › Access (or `access.json`); `pcds unban NAME_OR_IP` lifts a ban. A SteamID entry only matches players who joined by invite code. |
| Refused as full | `limits.maxPlayers` (16 unless set) players are in. | `pcds rules set limits.maxPlayers=24` (1 to 100). |
| One player cannot join while the name shows in `pcds players` | A second connection under a name or SteamID that is playing is refused. | Kick the old session: `pcds kick NAME`. |
| A big world joins by direct IP but not by invite code | Steam's transport takes 2.5 MiB in one message, and the world's join message is larger. | Use direct IP for that world (README, "Known limits"). |

## The admin page, the API and the commands

| Symptom | Cause | Do this |
|---|---|---|
| `the server does not answer on 127.0.0.1:7782: it is stopped or still starting` | Nothing listens on the admin port: the server is stopped, still loading its world (a few seconds, longer for a big world), or crashed. | `pcds status`, then `pcds start` and `pcds logs`. |
| `the server refused the admin token in server.env` | The server reads the token when it starts; `server.env` has another one now. | `pcds restart`. |
| The page says `The server did not accept that token` | The token in your browser is not the server's. | Open the address `pcds gui` prints (it carries the token), or paste what `pcds token` prints. |
| `the admin API is off or has no token` | `PCDS_ADMIN=off` or no `PCDS_ADMIN_TOKEN` in `server.env`. | `pcds config`, set both back (see README, "Settings"), `pcds restart`. |
| The page cannot be reached from another machine | The admin page listens on 127.0.0.1 on purpose. It is plain HTTP, and the token travels in it. | `ssh -L 7782:127.0.0.1:7782 HOST`, then open `http://127.0.0.1:7782/`. |
| Config, Plugins and Map are empty and the page says the world is stopped | These come from the running world. | Press Start world in the banner (or `pcds start`). |
| `pcds rules set … : speed.growth: 0 is not a number from 0.01 to 100` | The value is outside what the rule takes. Nothing was changed. | Use a value in that range (`pcds rules` lists every rule and its default; README, "Server rules"). `null` puts the default back. |
| `chat.webhooks: the value is not a list of text` | Lists are JSON. | `pcds rules set 'chat.webhooks=["https://example.com/hook"]'` (quotes around the whole argument). |

## Worlds, backups and updates

| Symptom | Cause | Do this |
|---|---|---|
| `world NAME exists (--force replaces it; the old one is kept in backups/)` | `world new` never writes over a world by itself. | Another name, or `--force` (stop the server first when it runs that world). |
| `"bad/name" is not a world name` | A world name is letters, digits, `.`, `_` and `-` (up to 64), never a path. | Give a plain name. `pcds world import PATH/Save.json` takes the name from the file. |
| `--planet Humble: a new world starts on Prime, Selenea` | A new world can start on a free planet only. A DLC planet is only continued by a world that is on it already. | Use Prime or Selenea (README, "Differences"). |
| `the server runs NAME: 'pcds world use' another one first` | `world remove` does not remove the running world. | `pcds world use OTHER`, then remove. |
| Something went wrong with a world and you want it back | Every start and every `save.backupMinutes` keep a copy; removed worlds go to `worlds/backups/` too. | Stop the server and copy the file from `worlds/backups/` over `worlds/NAME.json`, or restore it from the World page (Restore as a world). |
| `pcds upgrade` says the release needs a restart | The release changes how the server's two processes talk, or the snapshot format. | `pcds upgrade --restart VERSION` (everyone has to join again) (README, "Upgrading"). |
| `bank b holds no release to go back to` | `--rollback` goes back to the release that ran before; there is none yet. | Nothing to do; Roll back is greyed out on the page for the same reason. |
| `S3` upload shows an error in `GET /api/status` (`s3.last.error`) | The endpoint, the keys or the bucket are wrong, or the service is down. The game is never held by it. | `pcds api GET /api/status` and read `s3`; fix the `backup.s3…` rules. `Connection refused` means the endpoint address. |

## Still stuck

`pcds status --json` and `pcds logs` hold what to report. Nothing in them shows the admin token,
a webhook address or a plugin's secret. Do not paste `server.env` or the address `pcds gui`
prints: both carry the token.
