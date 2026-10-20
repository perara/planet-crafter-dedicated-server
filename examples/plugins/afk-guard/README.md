# afk-guard

A small moderator for the server. It does two jobs: it removes players who have gone away from
the keyboard, and it notices a player who takes buildings apart much faster than anyone builds.

## For a player

- Stand still for a long time (15 minutes by default) and you get a warning, then you are
  kicked. The kick text says why and that you can rejoin. Move more than 1 unit and the clock
  starts again.
- Take apart a lot of objects in a short time (more than 25 in 60 seconds by default) and you get a
  message telling you to slow down. The operators online are told too. The admin may choose
  that you are kicked instead.
- Operators are never kicked, never struck and never warned by this plugin.

Texts a player can see (the numbers change with the settings):

| When | Text |
|---|---|
| warning, in the last `afk_warn_seconds` | `You seem to be away. Move within 20 seconds or you will be removed from the server.` |
| kick for idling | `Removed for being away from the keyboard (1m 00s). You are welcome to rejoin.` |
| to the player, grief, action warn | `Guard: you took apart 4 objects in 60 s. Slow down; the operators have been told.` |
| kick reason, grief, action kick | `Removed by the grief guard: 4 objects destroyed within 60 seconds.` |
| to every operator online, grief | `Guard: Ann destroyed 4 objects in 60 s (strike 1). Action: warned.` (or `Action: kicked.`) |

## For the admin

Commands (both for operators only; the host answers anyone else with `!guard is for the server's operators.`):

- `!guard` tells you three lines: the settings, who has been idle for a minute or more (and for
  how long, longest first, at most 10), and who has strikes (most first, at most 10).
  Example: `Idle: Ann 1m 30s, Bo 1m 30s, Root 1m 30s (operator)` and `Strikes: Ann 2, Bo 1`.
  Lines when empty: `Idle: nobody.` and `Strikes: none.`
- `!pardon <player name>` clears the strikes of one player. The name is compared without regard
  to ASCII case; an exact name wins, otherwise a name that begins with what you typed is taken if
  only one player matches (`!pardon an` finds Ann). Players online and players with saved strikes
  (by the name they had) are searched. A name with a doubled space cannot be typed, so it is not found.
  Replies: `Pardoned Ann: 1 strike cleared.`, `No player named X is online or has strikes.`,
  `Several players match: A, B. Type more of the name.` With no name the host answers `Use: !pardon <player name>`.

Settings (`pcds plugin config set afk-guard KEY VALUE`; read again at every tick, so a change works within 5 seconds):

| Key | Default | Range | Meaning |
|---|---|---|---|
| `afk_minutes` | 15 | 1 to 240 | minutes without moving more than 1 unit before the kick |
| `afk_warn_seconds` | 60 | 0 to 600 | seconds before the kick that the warning comes; at most half of `afk_minutes` is used. 0: no warning |
| `destroy_limit` | 25 | 1 to 500 | more objects than this inside the window is a strike |
| `destroy_window_seconds` | 60 | 5 to 3600 | the window |
| `grief_action` | `warn` | `warn` or `kick` | besides the strike: tell the player and the operators, or also kick |

A strike is forgotten 24 hours after it was given (wall clock). At most 500 players are kept with
strikes; when full, the player with the oldest strike is dropped. This is a constant in the code.

## Capabilities it asks for

- `event.player`: to forget the idle clock and the destroy count of a player who leaves, and to keep names up to date.
- `event.object`: `object_destroyed` says which client took an object apart (`client` 0 means nobody did, and is ignored).
- `event.tick`: every 5 seconds (`tick_every_ms = 5000`) it compares each player's position with where they last moved to.
- `event.chat`: for `!guard` and `!pardon`.
- `world.read`: `players()` (positions, `player_id`, `operator`), `world()` (`standing_still`), `settings()`.
- `chat.say`: warnings and replies.
- `player.kick`: the kicks.

Budget: `fuel_per_event = 800000`. A tick costs about 90000 fuel plus 6000 for each player online
(`pcds plugin test` with 24 players: 228000 on average, 366000 at most, while it kicks 24 players).
`pcds plugin check` prints 145758 for its first tick (the first call after a start costs
100000 to 150000 more than the later ones), 9087 for a join, 5632 for a leave, 91514 and 73687
for the two commands and 23032 for a destroyed object.

## How it works, and how it keeps its promises honest

- Idle time is counted in game time from the tick's `since_ms` (at most 60 seconds per tick). While
  `standing_still` is true nothing is counted, so a paused world kicks nobody. The warning and the
  kick come at the first tick at or after the time, so up to 5 seconds late. The first tick
  after a player joins or after the plugin starts credits the time since the plugin's last tick.
- Positions: "moved" means more than 1 unit away from the place the player last moved to (not from
  the last tick), so slow drifting still counts as moving, and a jitter under 1 unit does not.
  A position that is not a number counts as having moved.
- The idle clocks are not saved. A restart or a reload starts everyone at zero, which is the kind way round.
- Destroys are counted by `player_id` in a sliding window: the game times of the last destroys,
  at most `destroy_limit + 1` of them for each player. The game time used is the time of the last
  tick, so the window is exact to about 5 seconds. After a strike the count starts again from zero.
- A warning or a kick that the host refuses (`Full`: more than 16 actions in one call) is tried again
  at the next tick. The plugin does not hear later whether the host really kicked: it counts a kick as
  done when `kick` returned `Ok`.
- State (version 1): the players with strikes (player_id, name, strikes, time of the last one).
  A state with a newer version is ignored with a log line (`state version N is newer ...`).

## Build and test

```bash
cargo test                                                    # 18 tests with pcds_plugin_api::testing
cargo clippy --all-targets -- -D warnings
cargo build --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/afk_guard.wasm plugin.wasm   # or ../../../scripts/build-plugin-examples
pcds plugin check .
pcds plugin test .                                            # scenarios/afk.toml, grief.toml, grief-kick.toml
pcds plugin install . && pcds plugin enable afk-guard
```

## What it teaches

- Count by `player_id`, not by `client`. A player who rejoins has a new `client`.
- `object_destroyed.client` and `PlayerInfo::operator` (ABI 1.3) make moderation possible without any RPC hook.
- `permission = "operator"`, `min_args` and `usage` in the manifest move the permission and argument checks out of the plugin.
- Typed `[[config]]` settings with bounds, read with `Host::settings`. A string setting needs `secret = false` or the plugin cannot read it.
- Every list is bounded: tracks by the players online, destroy counts by `limit + 1`, strikes by 500.
- Work in a tick grows with the players online; measure it.

## Limits and known gaps

- Anything that moves a player less than 1 unit while they sit at the keyboard (reading chat, a menu, a map) is "away". Raise `afk_minutes` if that bites.
- A player in a vehicle that is moving by itself is not idle.
- Tidying one's own base can take apart more than 25 objects in a minute. With `grief_action = "warn"` that costs a message and a strike that expires; raise `destroy_limit` if it is noisy.
- It cannot tell a legitimate deconstruction from a grief; it only counts.
- The chat command prefix is not known to a plugin, so the texts say `!pardon`; a server with another prefix shows the wrong one in the text it makes itself (the host's own `Use:` line is right).

## Licence

MIT (see ../LICENSE).
