# The admin API

Generated from the table in `server/src/front/admin_routes.rs` (a test keeps the two equal; the text below
is what it expects). Every route but the web page needs `Authorization: Bearer <token>` (docs/hot-update.md, README).
The answers are JSON; an error is `{"error": "..."}` with the status. Who answers: the HTTP thread (admin.rs: its own
thread, never held by the game loop), the front's loop (the process: updates, stopping, the world's controls), or the bank
(the world; the front passes the request on and adds its part to `GET /api/status`).

| Route | Answered by | Takes | Does |
|---|---|---|---|
| `GET /` | HTTP thread | nothing | The web page (no token). |
| `GET /index.html` | HTTP thread | nothing | The same. |
| `GET /api/logs` | HTTP thread | nothing | The server's recent output, from the journal of its systemd unit (`lines`, `source` or a `note` why there are none). |
| `GET /api/chat/events` | HTTP thread | query `after=ID&wait=SECONDS` | The chat events after ID, waiting up to SECONDS (at most 30) for one: `events`, `last`, `dropped`. At most 8 polls wait at once. |
| `POST /api/chat` | HTTP thread | `{"text": "..."}` | Says the text in the game's chat, from the server. |
| `any /api/chat*` | HTTP thread | nothing | Any other path under /api/chat: 404. |
| `POST /api/stop` | front | nothing | Saves the world and stops the server. |
| `GET /api/update` | front | nothing | The update state: `state`, `doing`, `seq`, `last`, `history` (the last 20), `auto`, `checkHours`, `latest`, `available`, `checkedAt`, `releases`. |
| `POST /api/update` | front | `{"version"?: "v0.6.0", "onlyNewer"?: bool, "check"?: bool, "rollback"?: bool}` | Downloads a release into the standby bank and flips to it, only looks up the latest (`check`), or goes back to the other bank (`rollback`). Answers at once with `seq`; the outcome is in GET /api/update's `history`. |
| `POST /api/world/stop` | front | nothing | Tells every player, saves the world and ends its bank; the page and API stay. |
| `POST /api/world/start` | front | `{"world"?: "name"}` | Starts the world that ran, or another one, in the active bank. |
| `POST /api/world/switch` | front | `{"world": "name"}` | Stops the world that runs, if any, and starts another. |
| `GET /api/status` | front, while the world is stopped | nothing | While the world is stopped: the front's part of the status, and which world it was. |
| `GET /api/worlds` | front, while the world is stopped | nothing | While the world is stopped: the worlds and the backups. |
| `GET /api/status` | bank | nothing | The server's state: world, planet, the DLC planets stated as owned, players, rules changed, loop times, save and backup state, tables, plugins; the front adds the network, the invite code, the process, the banks and the update state. |
| `GET /api/ping` | bank | nothing | Any answer is a game loop that runs (the front's watchdog asks it). |
| `POST /api/save` | bank | nothing | Saves the world now (and its backup, when they are on). |
| `POST /api/kick` | bank | `{"id": N}` or `{"name": "..."}` | Kicks a player. |
| `POST /api/ban` | bank | `{"id": N}` or `{"name": "..."}` | Bans a player (their name, SteamID and IP) and kicks them. |
| `POST /api/unban` | bank | `{"value": "..."}` | Lifts a ban. |
| `GET /api/rules` | bank | nothing | The rules as they are and the defaults. |
| `POST /api/rules` | bank | a JSON merge patch of the rules | Changes rules; `null` resets one. |
| `GET /api/config` | bank | nothing | Every setting with its type, range, unit, default, value and whether it applies live. |
| `POST /api/config` | bank | `{"speed.growth": 2, ...}` | Changes settings by name. |
| `POST /api/teleport` | bank | `{"id"? or "name"?, "position": [x, y, z]}` | Moves a player. |
| `GET /api/worlds` | bank | nothing | The worlds beside the running one, its backups, the choices for a new world, the backup rules and S3 state. |
| `POST /api/worlds/new` | bank | `{"name", "planet"?, "start"?, "seed"?, "mode"?, "dying"?}` | Makes a new world beside the running one. |
| `POST /api/worlds/remove` | bank | `{"name"}` | Moves a world into backups/ (never deletes it); not the running one. |
| `POST /api/worlds/file` | bank | `{"name", "backup"?: bool}` | A world's or a backup's file text. |
| `POST /api/worlds/restore` | bank | `{"backup", "name"}` | A backup as a new world. |
| `POST /api/backup` | bank | nothing | Saves and makes a backup now, copied to S3 when that is set. |
| `POST /api/backup/s3` | bank | nothing | Copies the newest backup to S3. |
| `POST /api/pause` | bank | `{"paused": bool}` | The world stands still until resumed. |
| `GET /api/map` | bank | nothing | The buildings on this planet, for the players' map. |
| `GET /api/plugins` | bank | nothing | Every plugin with its status, capabilities, budgets and counters. |
| `POST /api/plugins/reload` | bank | nothing | Reads the plugins' folder again. |
| `POST /api/plugins/enable` | bank | `{"id", "grant"?: [...], "digest"?}` | Approves a plugin (all it asks for, by default) and reloads; with the `digest` the listing showed, only if the files are still those. |
| `POST /api/plugins/disable` | bank | `{"id"}` | Disables a plugin. |
| `POST /api/plugins/config` | bank | `{"id", "key", "value"?: "..."}` | Sets a config key the plugin declares (an empty or missing value clears it) and reloads. |
| `POST /api/plugins/trust` | bank | `{"publicKey", "name"?, "grant"?: [...], "force"?: bool}` | Trusts a publisher's Ed25519 key: plugins signed by it load without approving each file's digest, with the capabilities granted. |
| `POST /api/plugins/untrust` | bank | `{"id"}` | Revokes a trusted key (its id or name) and reloads. |
| `GET /api/plugins/tokens` | bank | nothing | The scoped tokens (never the token itself), the scopes there are and the rate limits. |
| `POST /api/plugins/tokens` | bank | `{"name", "scopes": [...], "ratePerMinute"?}` | Makes a scoped token for an out-of-process plugin; the answer's `token` is shown this once. |
| `POST /api/plugins/tokens/revoke` | bank | `{"id"}` (the id or the name) | Revokes a scoped token at once. |
| `POST /api/plugins/logs` | bank | `{"id", "limit"?: N}` | The last log lines of one plugin (its own and the host's about it), newest last; at most 200 are kept. |
| `POST /api/plugins/events` | bank | `{"after"?, "types"?, "limit"?, "consumer"?, "unregister"?, "wait"?: SECONDS}` | The plugin event stream; with `wait` (at most 25 s; at most 8 polls wait at once) the front holds the request until an event comes or the wait is over. At most 16 consumer names are registered at once (429 for a new one beyond). |
