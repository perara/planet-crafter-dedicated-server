# restart-notice

A WebAssembly plugin written in Rust. It warns the chat before the daily restart of the server
("Server restart in 15 minutes"), tells when a save failed, and tells when the server was updated
without a restart. It is the example for timers and the wall clock.

**It does not restart anything.** A plugin cannot. Your own scheduler restarts the server, and this
plugin only says so beforehand. Use the same time in both places.

## For a player

- `!restart` tells you how long until the next restart, as `Next restart in 2 hours 5 minutes (at 04:00 UTC).`
  (`Next restart in 1 hour (...)`, `Next restart in 7 minutes (...)`; the time is rounded up to whole
  minutes). Without a restart time set it answers `No restart is scheduled.`
- In the chat, from the server:
  - `Server restart in 15 minutes`, then `Server restart in 5 minutes`, then `Server restart in 1 minute`
    (the minutes are the admin's list), and at the restart time `The server restarts now`.
  - `The world could not be saved, tell an admin.` when a save failed. Always.
  - `World saved.` at every good save, only if the admin turned `announce_saves` on.
  - `The server was updated to version 0.9.1 without a restart.` after a hot update, unless the admin
    turned `announce_updates` off.

## For the admin

Set the time, in UTC, and make your scheduler restart a minute or so later:

```bash
pcds plugin config set restart-notice restart_at 04:00
pcds plugin config set restart-notice warn_minutes 30,10,5,1     # optional, the default is 15,5,1
```

A changed setting reloads the plugin; the warnings are worked out again at once.

Restart a minute after the time the plugin says, so that "The server restarts now" is heard first.
A systemd timer (`/etc/systemd/system/pcds-restart.timer` and `.service`):

```ini
# pcds-restart.timer
[Timer]
OnCalendar=*-*-* 04:01:00 UTC
Persistent=false
[Install]
WantedBy=timers.target

# pcds-restart.service
[Service]
Type=oneshot
User=pcds
ExecStart=/usr/local/bin/pcds restart
```

or cron (the cron daemon's own time zone: say UTC with `CRON_TZ=UTC`):

```cron
CRON_TZ=UTC
1 4 * * * pcds restart
```

### Capabilities it asks for

| Capability | Why |
|---|---|
| `event.timer` | the warnings are timers |
| `event.world` | to hear `world_saved` and `flip` |
| `event.chat` | the command `!restart` |
| `world.read` | the wall clock (`WorldInfo::unix_ms`): a module has none of its own |
| `chat.say` | to say the warnings and to answer `!restart` |

Budget: `fuel_per_event = 200000`. A warning costs about 12,000 fuel; the first call after a start
or a hot update costs up to about 80,000 more (the host translates the code on first use).

### Settings

| Key | Type | Default | Meaning |
|---|---|---|---|
| `restart_at` | string, at most 5 characters | empty | `HH:MM` in UTC, `04:00` or `4:00`. Empty: no scheduled restart. Anything else is logged as a warning at start, nothing is announced, and `!restart` says `The restart time is not set up right, tell an admin.` |
| `warn_minutes` | string, at most 40 characters | `15,5,1` | Up to 8 whole numbers from 1 to 1439, separated by commas, in any order. `none`: no warnings, only the last line. A bad list is logged and `15,5,1` is used. |
| `announce_saves` | bool | false | Say `World saved.` at every good save (at most one line in 30 seconds of game time). |
| `announce_updates` | bool | true | Say the version after a hot update. |

A setting cannot be set to an empty text (the host refuses an empty value): to go back to "no
restart" use `pcds plugin config unset restart-notice restart_at`.

### What the integration test can rely on

- Event kinds used: `world_saved`, `flip` (subscribed), `timer` (always delivered to the plugin that set it),
  and the chat command `restart`. No `tick`.
- Exact texts: `Server restart in N minutes` (`Server restart in 1 minute` for 1), `The server restarts now`,
  `World saved.`, `The world could not be saved, tell an admin.`,
  `The server was updated to version X without a restart.`, `Next restart in H hours M minutes (at HH:MM UTC).`
  All with `say` (to everyone) except the answer to `!restart`, which is a `tell`.
- Timer ids: 1 to 9 for the warnings (in the order of the list, largest first) and the restart itself (the
  last of them), 20 for the five-minute resync, 21 for the boot timer.
- A warning that arrives more than 90 seconds after its time is not said (it is logged at debug level).
- The plugin keeps no state.

## How it works, and what it teaches

1. **No clock in a module.** The wall clock comes from `Host::world().unix_ms` (and `Event::Tick`'s
   `unix_ms`, which this plugin does not need). The game clock stops when the world stands still.
2. **Timers belong to the running instance.** They are gone after a reload, a restart and a hot update.
   So there is nothing to save: the plugin works the schedule out from the settings and the wall clock
   every time it starts. A reload in the middle of the 15-minute window neither moves a warning nor says
   one twice.
3. **The world cannot be read inside `load`.** The host lets a plugin ask the world only while it handles
   an event. So `load` reads the settings (they need no capability) and sets a one-shot timer of 1 ms, the
   "boot" timer. In the call it brings, the plugin reads the clock and sets the real timers. Any other
   event that comes first does the same. (`testing::MockHost` does answer a query inside `load`; the real host
   does not, so the tests here call the boot timer by hand and one test checks that `load` asks nothing.)
4. **Timers count game time, the wall clock does not.** A world that stands still when it is empty
   (`world.pauseWhenEmpty`) does not count game time, so a timer set for 03:45 goes off late. Every
   five minutes a repeating timer sets the others again from the wall clock, and a warning that has
   become more than 90 seconds old is dropped instead of telling a time that is not true any more.
   Tomorrow's warning is set at once.
5. **The wall clock can be set back** (a time sync). A timer that goes off more than 90 seconds before its
   time is set again for the real time and says nothing.
6. **Settings** are typed in the manifest (`[[config]]` with a type, a default and a length), checked by
   the host before they are stored, and read with `Host::config`. A non-secret string must say
   `secret = false`, since a string is a secret by default. The mock does not know the manifest's
   defaults, so the code knows them too.
7. **A flood is bounded.** Saves and failed saves are said at most once in 30 seconds of game time each,
   so a broken disk does not fill the chat; the version in the update line is cut and cleaned.

## Build, test, run

```bash
cd examples/plugins/restart-notice
cargo test                                           # 18 tests with the mock server, no game
cargo clippy --all-targets -- -D warnings
cargo build --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/restart_notice.wasm plugin.wasm   # or scripts/build-plugin-examples
pcds plugin check .                                  # static checks and the fuel of a sample of each event
pcds plugin test .                                   # the four scenarios in scenarios/ in the real host
pcds plugin install .
pcds plugin enable restart-notice
```

Scenarios: `daily-restart.toml` walks one morning with a reload and a hot update in the middle;
`world-stood-still.toml` jumps the wall clock; `unscheduled.toml` and `bad-setting.toml` are the empty
and the malformed setting.

## Limits and known gaps

- One restart time a day. Two times a day need two copies of the idea (or a list setting).
- The time is UTC. There is no time zone setting.
- A warning is as exact as the game: timers are delivered on the next game tick after they are due, and
  the server may be busy. Expect a second or two.
- `pcds plugin check` runs a sample of each event kind but not of timers, so the cost of a timer call
  is seen in `pcds plugin test` (the "cost of each kind of event" table) and `pcds plugin stats`.
- The plugin cannot know that the restart really happens. If your scheduler fails, the chat has said
  "The server restarts now" and nothing happened.
