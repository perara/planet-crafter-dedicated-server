# Writing plugins

A plugin adds something to your server without changing it: a chat command, a reaction to a player
joining, a rule that follows how many are online, a notice to Discord. This page gets you from nothing
to a working plugin in about ten minutes, shows the tools that make writing one pleasant, and then goes
through the plugins in `examples/plugins/` that do real jobs (the cookbook). The ideas behind the design
are in docs/plugins.md; every detail is in docs/plugins-api.md.

## Pick a kind

| You want | Make | Needs |
|---|---|---|
| a message, a command that answers with a text, an announcement, a rule change by player count | **a rules file** (a TOML file; "Data plugins" below) | an editor |
| logic: counting, remembering, deciding, reading positions, giving things | **a WebAssembly module** (this page) | Rust, or C, Zig... |
| a Discord notice, a web hook, a call to a few known hosts | **a module with `net.http`** ("Reaching the Internet") | Rust, or C, Zig... |
| a database, a web page, anything big or long-lived | **a program** that reads the event stream ("A program of its own") | any language |

## First plugin in ten minutes

You need `pcds` on your PATH and, for a Rust plugin, `rustup target add wasm32-unknown-unknown` once. You do not need a
running server for steps 1 to 5.

**1. Make it.**

```bash
pcds plugin new greeter          # --lang c for C, --kind data for a rules file
cd greeter
```

This writes `plugin.toml` (the manifest), `Cargo.toml`, `src/lib.rs`, a `Makefile` and a `justfile`, a scenario in
`scenarios/basic.toml`, and a README. The crate depends on `pcds-plugin-api`, the small library that holds the
events and the way to talk to the server (`--api-path DIR` points it at a copy on your disk).

**2. Read the manifest.** It says what the plugin is and what it asks for:

```toml
[plugin]
id = "greeter"
name = "greeter"
version = "0.1.0"
kind = "wasm"
entry = "plugin.wasm"
abi = 1
capabilities = ["event.chat", "chat.say"]

[budget]
fuel_per_event = 500000

[[command]]
name = "hello"
help = "says hello"

[[config]]
key = "greeting"
type = "string"
secret = false
default = "Hello"
```

`capabilities` are the things the plugin may do **if the admin allows them**. Ask for what you use and no more: the admin
reads the list when approving, and a long one is a reason to say no. `[[command]]` declares `!hello`: the server delivers
a chat message that starts with `!hello` to your plugin and to nobody else. `[[config]]` is a setting the admin can change
without touching your code; yours reads it with `host.config("greeting")`. `fuel_per_event` is how much work one event may
do before it is stopped; 500,000 is about a sixth of a millisecond, and most events need a hundredth of it.

**3. Read the code.** `src/lib.rs` is a `Plugin`: three functions and a line that turns it into a module.

```rust
use pcds_plugin_api::*;

struct MyPlugin { greeted: u32 }

impl Plugin for MyPlugin {
    // Once at the start, after a reload, after a hot update. `state` is what `save` gave last time.
    fn load(host: &dyn Host, state: Option<State<'_>>) -> Self { /* ... */ }

    // Each event you subscribed to. Answer Verdict::Continue unless you mean to refuse an RPC.
    fn event(&mut self, host: &dyn Host, event: &Event<'_>) -> Verdict { /* ... */ }

    // What to keep across restarts and hot updates.
    fn save(&mut self) -> Option<SavedState> { /* ... */ }
}

export!(MyPlugin);
```

`host` is how a plugin asks and acts: `host.world()` and `host.players()` read the world, `host.tell(client, text)` writes to one
player, `host.say(text)` to everyone, `host.log(level, text)` to the plugin's log. Each call needs a capability, and a call
returns a `Result`: a plugin that is not allowed gets `Err(Error::Denied)` and carries on without it (`let _ =` says so). Do not
`unwrap()` anything that comes from a player: a panic is a fault.

**4. Test it on your own computer.**

```bash
cargo test
```

The template has a test that runs your plugin against `pcds_plugin_api::testing`, a mock server in the crate: it joins
a player, types `!hello`, checks what was told, saves, reloads and checks again. No server, no WebAssembly. `Harness` moves the
game clock (`advance_ms`), delivers ticks and timers, kicks players for real, enforces the limits of the real host
(16 actions a call, ...), and records what your plugin did. You write ordinary Rust tests.

**5. Build it and run it in the real host.**

```bash
make build              # cargo build --target wasm32-unknown-unknown --release, then copies plugin.wasm here
pcds plugin check .     # what it asks for, and what each kind of event costs in fuel
pcds plugin test .      # scenarios/basic.toml: the module in the real host, against a made-up world
```

`pcds plugin test` is the host the server runs (budgets, capabilities, commands, timers, state, faults) with a stand-in
world, so it works anywhere, with no game tables. The scenario is a file of steps and expectations; a failing one prints the
step and what the plugin did instead. You can run it in CI.

**6. Install it.**

```bash
pcds plugin install .       # copies the manifest and the module to run/plugins/greeter/; it does nothing yet
pcds plugin enable greeter  # shows what it asks for in plain words; you approve it
```

A running server notices within a few seconds. Type `!hello` in the game's chat.

**7. Change it and see it.** Edit, `make build`, and with the server running:

```bash
pcds plugin watch . --approve     # every new build is installed and, if it asks for nothing new, approved again
pcds plugin logs greeter -f       # the plugin's log lines, as they come
pcds plugin stats greeter         # what each event costs
```

The plugin's state goes from the old version to the new one (`save`, then the new `load`), so a counter keeps counting. A build
that asks for more than you approved is installed but waits for `pcds plugin enable`, which shows what is new.

**8. Ship it.** `pcds plugin keygen acme`, `pcds plugin sign --key acme.key .`, `make package` (a `.pcdsplugin` file and its
sha256). See "Sharing".

## The tools

| Command | What it is for |
|---|---|
| `pcds plugin check PATH` | the manifest read strictly; what the module imports against what the manifest asks for; a sample of each event in a world of 16 players, with the fuel as a share of the budget; the state size; warnings |
| `pcds plugin test PATH [--scenario FILE]` | scenarios against the real host: joins, chat, commands, clock, objects, RPC vetoes, timers, hot updates, HTTP answers. Exit code 1 when an expectation fails; a transcript of everything the plugin did |
| `pcds plugin watch PATH [--approve] [--once]` | install each new build over the installed plugin |
| `pcds plugin logs ID [-n N] [-f]` | the plugin's last 200 lines (its own, and the host's about it: faults, unloading, slow calls) |
| `pcds plugin stats [ID]` | calls, fuel, microseconds per kind of event; faults and their texts |
| `pcds plugin config list\|set\|unset` | the settings a plugin declares; checked against its declaration |
| the dashboard's Plugins page | the same, with the capability diff when a new version asks for more, and the log and cost of each plugin |
| `cargo test` with `pcds_plugin_api::testing` | the plugin's own logic, natively |

### Scenarios

```toml
name = "welcome"

[config]                      # settings, checked against the manifest's [[config]]
greeting = "Hi"

[[player]]
client = 1
name = "Ann"
operator = true               # a player in access.operators

[[step]]
join = { client = 2, name = "Bo" }
expect = [{ tell = 2, contains = "Welcome" }]    # what this step made the plugin do

[[step]]
advance = "10m"               # game time passes: ticks and timers
expect = [{ say = true, contains = "Tip", count = 1 }]

[[step]]
chat = { client = 1, text = "!top" }
expect = [{ tell = 1, contains = "1." }]

[[step]]
flip = true                   # a hot update: save, then a new instance loads the state
```

Steps: `join`, `leave`, `chat` (a message starting with `!` is a command, checked against the manifest's permission and argument
counts as on the server), `advance`, `place`, `destroy`, `rpc` (`expect = [{ veto = true }]`), `world_saved`, `http_done` (the answer
to the plugin's Nth request), `save`, `reload`, `flip`, `set` (positions, backpacks, terraformation, the clock...). Expectations:
`say`, `tell = CLIENT`, `kick`, `teleport`, `give`, `tokens`, `rules_patch`, `http`, `veto`, `log`, each with `contains`, `equals`,
`count` (0 means "never"). The full format is the comment at the top of `server/src/plugins/scenario.rs`. Nothing is ever sent to the
network: the requests are listed, with a secret setting shown as `${config.KEY}`.

### Reading the messages

Every failure says what to do. The ones you will meet:

| You see | It means |
|---|---|
| `unknown field capabilites ... did you mean capabilities` | a key in `plugin.toml` (or a scenario) that does not exist; the one meant is named |
| `"event.chatt" is not a capability (did you mean event.chat?)` | likewise for a capability, an event name, a setting's type |
| `plugin greeter: it used something that needs the capability chat.say, which is not granted` | your module called `say` and the admin (or your manifest) did not grant `chat.say`; said once per capability in the log, and `denied` counts the calls |
| `the module was built for plugin ABI 1.4; this server speaks 1.3` | rebuild against an older `pcds-plugin-api`, or update the server |
| `chat: out of fuel: the call used all 500000 fuel it is allowed (raise budget.fuel_per_event, at most 20000000, or do less work per call)` | the event name, the budget, and the knob; a loop, or a lot of work in one call |
| `trap: ... unreachable (a panic in Rust compiles to this ...)` | a panic, an `unwrap`, an index out of range, a division by zero |
| `the stored setting afk_minutes: 500 is above the most, 240: set it again` | the admin's value no longer fits the declaration; the plugin does not start until it is set again |
| `the module imports pcds.fd_write, which this server does not provide` | the module was built for WASI; build for `wasm32-unknown-unknown` |
| `!kickit is for the server's operators.` / `Use: !kickit <player> [reason]` | the host answered a player for your command; you were not called |

## Things that are different here

- **There is no clock but the ones events carry.** `Tick::unix_ms` and `WorldInfo::unix_ms` are the wall clock; `game_ms` is
  game time and stops when the world stands still (a paused or empty world delivers no tick and no timer).
- **Timers belong to the instance.** `host.set_timer(id, delay_ms, repeat_ms)` counts game time. After a reload, a fault or a hot
  update the plugin is a new instance: set them again in `load`. Compute them from the wall clock if the job is about the wall clock.
- **Remember players by `player_id`**, not `client`. `client` is the connection number of this session; `player_id` is the same every
  visit (the verified SteamID64 for a Steam player, else made from the name). Everything you keep about a player goes under it.
- **State is bytes you version.** `save` is asked at every world save and before a hot update; `load` gets the version the bytes were
  written with. Write version 2 as soon as you change the layout and read both: `examples/plugins/playtime-board` shows the migration, with a
  test that loads real version 1 bytes. State is at most 1 MiB, and `save` has a fuel budget too.
- **Actions are asked, not done.** `say`, `tell`, `kick`, `give`, `tokens` are queued and applied after your call. If the player left or the
  backpack has no room, you are not told. Look first (`host.inventory(client)`), and do not record a reward as given before you checked it
  could be.
- **Reading the world costs fuel.** `players()` costs about 6,000 fuel per player online, `nearby()` 8 per object in the world; `player(client)` is
  one player. A float formatted with `format!` costs 100,000 fuel or more in a module: print integers.
- **The first call costs what the second does.** The module is translated when it is read. Budget for the steady state.
- **A plugin runs on the game thread.** Everything it does comes out of the game's 5 ms. All plugins together get 2 ms of a tick; the
  rest wait their turn.
- **A veto is a request the client has already sent.** The client may show a ghost for a moment (docs/plugins.md, "The one veto"). Tell the
  player why in the chat.

## Cookbook

The plugins in `examples/plugins/`, each with a README, a manifest, a scenario, and a test in the real game
(`server/src/plugins/cookbook_tests.rs`). Copy one and change it.

| Plugin | Kind | Shows | Key lines |
|---|---|---|---|
| `server-motd` | data | the message of the day, `!rules`, `!help`, tips that rotate every ten minutes, none to an empty server | `[[every]] say = [..]`, `min_players` |
| `scaling-rules` | data | rules by player count in bands (one or two players: faster; six: the game's speed) | `[[when_players]] at_least`, `at_most` |
| `welcome-rules` | data | the first example: a welcome, `!rules`, `!discord`, a rule switch at six players | |
| `hello-stats` | wasm | reading the world, a command, state across restarts and hot updates, a state migration | `STATE_VERSION`, `load` |
| `afk-guard` | wasm | a moderator: warns and kicks idle players, counts who takes buildings apart, operator commands | `Event::ObjectDestroyed.client`, `permission = "operator"`, `player_id` strikes |
| `playtime-board` | wasm | a leaderboard in plugin state, `!top`, `!playtime`, state version 1 to 2 | `Tick.since_ms`, `load` migration |
| `quest-rewards` | wasm | milestones that give items and Terra tokens; the action API; honest bookkeeping | `host.give`, `host.tokens`, `rpc_after` |
| `claims` | wasm | world protection: `!claim`, `!trust`, a veto of building and taking apart near a claim | `rpc.veto`, `host.object(Net)` |
| `restart-notice` | wasm | warnings before a scheduled restart from the wall clock, save and update notices | `set_timer`, `unix_ms`, `Event::Flip` |
| `chat-bridge` | wasm + program | Discord/Slack, both directions: batched webhook posts out, a poller that says Discord messages in the game with a scoped token | `net.http`, `${config.webhook}`, `chat.send` |
| `prometheus-exporter` | program | `/metrics` for Prometheus from the event stream, the status and the plugin listing | the stream, `--replay` |
| `hello-c` | wasm (C) | the same interface from C, with no libc: the contract is the table in docs/plugins-api.md | `pcds.h` |
| `discord-notify` | wasm | `net.http`, batching, a secret the plugin never sees, a signed plugin | |
| `webhook-relay` | program | the first out-of-process example | |

### Data plugins

A rules file is the smallest plugin and cannot loop. Texts may use `{name}` `{client}` `{players}` `{planet}` `{time}` `{args}`
`{arg1}`..`{arg3}` and your `[vars]`.

```toml
[vars]
discord = "https://discord.gg/example"

[[command]]
name = "rules"
reply = "Be kind. More at {discord}"
cooldown_seconds = 5

[[on_join]]
tell = "Welcome, {name}! {players} online."

[[every]]                       # a list rotates; min_players = 1 says nothing to an empty server
seconds = 600
min_players = 1
say = ["Tip one", "Tip two"]

[[when_players]]                # a band of player counts; the rules change once when the band changes
at_least = 1
at_most = 2
set_rules = { speed = { growth = 2.0 } }
```

The reference is in docs/plugins-api.md ("Rules files"). The manifest has `kind = "data"` and the capabilities the rules need (`pcds plugin check`
says which). `pcds plugin test` runs a data plugin like any other.

### A moderator (afk-guard)

*Teaches: events, operator commands, strikes by `player_id`, bounded state.* The plugin subscribes to `tick` (every 5 s), `player_join`,
`player_leave` and `object_destroyed`. On a tick it asks `host.players()` for positions and compares them with the last; a player who has
not moved a unit for `afk_minutes` is told, then kicked. `ObjectDestroyed.client` says who took a building apart, so many deconstructions in a
minute by one `player_id` is a strike and a message to the operators. The commands `!guard` and `!pardon` are marked
`permission = "operator"` in the manifest: the host answers everyone else and does not call the plugin. Settings are typed in the
manifest (`afk_minutes` is an int from 1 to 240), so the admin cannot set nonsense.

### Announcements from a data file (server-motd)

*Teaches: that most plugins are data.* Three `[[on_join]]` entries are the message of the day; `[[command]]` entries answer `!motd`, `!rules`,
`!help`; a rotating `[[every]]` says the next tip every ten minutes, and nothing while the server is empty.

### A leaderboard (playtime-board)

*Teaches: persistence and migration.* Every tick adds `since_ms` to each online player's time, keyed by `player_id`, and keeps the last name.
`!top 5` tells the asker five lines. The state is a sorted list; version 1 stored minutes, version 2 stores seconds and the last-seen
time, and `load` reads both. The board is bounded (`board_size`), and the plugin drops the least played offline player when it is full.

### A metrics exporter (prometheus-exporter)

*Teaches: the out-of-process way.* A Python program (standard library only) long-polls `POST /api/plugins/events` with a scoped token that has
`events.read`, counts chats, joins, leaves, builds and saves, reads `/api/status` and `/api/plugins` (they need the admin token today, so
`--no-admin` exports only the stream), and serves them in Prometheus' text format on `/metrics`. `--replay FILE` renders a saved capture
without a network, which is how it is tested. A crash of the exporter never touches the game.

### Rewards (quest-rewards)

*Teaches: the action API and honest bookkeeping.* Milestones (first building, ten buildings, an hour played, terraformation passing a threshold) pay
items with `host.give` and tokens with `host.tokens`. A building is counted from `rpc_after` of the build RPC, not from every placed object (a
dropped item is an object too). The plugin checks `host.inventory(client)` for room and gives nothing in part; tokens are paid at once; a
`Full` answer (the host's rate: 10 gives and 2 token changes a second) is retried at the next tick. The rewards are recomputed from small
counters, so nothing is paid twice after a hot update.

### World protection (claims)

*Teaches: the veto.* `!claim 20` claims a sphere around you. The plugin lists `GroupNetworkBase.DeconstructServerRpc` and
`WorldObjectsHandler.CreateAndInstantiateWorldObjectServerRpc` in `[events] rpc` and asks for `rpc.veto`. For a deconstruct, the event's `object` is a network
object id: `host.object(ObjectRef::Net(id))` gives its position. For a build, `args_json` is
`[hash,[x,y,z],[qx,qy,qz,qw],true,false,true,false]`: the plugin cuts the position out of the text (cheaper than parsing it all). If a claim is
there and the player is not the owner, trusted, or an operator, the plugin returns `Verdict::Veto` and tells the player why. Anything it cannot
parse passes: a plugin bug must not stop the game.

### A restart notice (restart-notice)

*Teaches: timers and the wall clock.* The setting `restart_at = "04:00"` (UTC) and `warn_minutes = "15,5,1"` make the plugin set a timer for the next
warning, computed from `unix_ms`. A timer counts game time, so a world that stands still delivers none; a resync timer every five minutes sets them again
from the wall clock. The plugin cannot restart anything: your own scheduler does (a systemd timer running `pcds restart`), and the README shows one.
`world_saved` and `flip` (the hot update) are announced if the settings ask.

### A bridge to Discord and Slack (chat-bridge)

*Teaches: `net.http`, a secret, and a program with a scoped token.* The wasm plugin collects chat, joins and leaves and posts them in one request
every few seconds to a webhook whose secret part the plugin never sees (`https://discord.com/api/webhooks/${config.webhook}`), with mentions neutralised
and `allowed_mentions` empty; a failed request waits longer each time and keeps its lines in the saved state. The other half, `bridge_in.py`, polls
a Discord channel with a bot token of its own and says new messages in the game with `POST /api/chat` and a token that has only the scope `chat.send`.

### C (hello-c)

*Teaches: that the interface is language neutral.* `pcds plugin new demo --lang c` writes `pcds.h` (the imports, the exports, an allocator, a reader for
events) and a short `plugin.c`. `make build` runs `clang --target=wasm32 -nostdlib`. The bytes of a C module depend on the compiler, so the repository does
not compare them; the scenario is the test.

## Reaching the Internet

A module has no network. With `net.http` the manifest names the places it may go and the host sends the requests from its own threads:

```toml
capabilities = ["event.tick", "net.http"]
[net]
allow = ["https://discord.com"]
[[config]]
key = "webhook"
description = "the end of the webhook URL: ID/TOKEN"
# a string setting is a secret unless it says secret = false
```

The plugin builds a request (`HttpRequest { method, url, headers, body }`; `url` may hold `${config.webhook}`), gets a ticket, and later an
`Event::HttpDone { ticket, status, error, body, headers }`. Use `json::Object` to build a body. The rules (only the listed
hosts, no local addresses, 2 requests alive, 10 a second, the answer cut at `max_response_bytes`) are in docs/plugins-api.md ("`net.http`"). In a
scenario the request is printed, never sent, and `http_done` hands back an answer.

## A program of its own

For a database, a web page, a library from another ecosystem. The server writes events into a ring; the program reads them with `POST /api/plugins/events`
(a long poll with `"wait": 25`) and acts through the admin API with a **scoped token** (`pcds plugin token create NAME --scope events.read,chat.send`)
that opens only those endpoints. It has no manifest and no capabilities; what it may do is what its token may do. `examples/plugins/webhook-relay`,
`chat-bridge/bridge_in.py` and `prometheus-exporter` are the models: standard library only, `--dry-run`, `--self-test` against fakes.

## Sharing

A plugin is a folder, or one file:

```bash
pcds plugin pack greeter                       # greeter-0.1.0.pcdsplugin, and its sha256
pcds plugin install greeter-0.1.0.pcdsplugin --sha256 HEX
pcds plugin install https://example.org/greeter-0.1.0.pcdsplugin --sha256 HEX
pcds plugin search motd --index https://example.org/plugins-index.json
pcds plugin install server-motd --index https://example.org/plugins-index.json
```

Installing shows the package, where it came from, its sha256, whether it is signed and by whom, and what it asks for, and asks you (or takes your
`--sha256` as the answer). It never runs anything: the plugin waits for `pcds plugin enable`. A community hosts the index (`plugins-index.json`) as a
file; this project runs none. The format, the trust model and what is not built are in docs/plugins.md ("Sharing plugins"). The licence of your
plugin is yours: the API crate is MIT or Apache-2.0 (docs/plugins.md, "Writing a plugin: the licence").

## Signing

`pcds plugin keygen acme` makes `acme.key` (keep it secret) and `acme.pub`; `pcds plugin sign --key acme.key DIR` writes `plugin.sig` over the plugin's digest.
An admin who trusts the key (`pcds plugin trust acme.pub --name Acme --grant chat.say`) need not approve each version that asks for nothing beyond that grant.
A changed file makes the signature not hold, and then the plugin is judged by its digest as if unsigned. The example key in `examples/plugins/` is published on
purpose: never trust it on a server that matters.

## When something is wrong

- **Nothing happens**: `pcds plugin list`: is it `running`? A `pending` plugin waits for `enable`; `changed` waits again; `failed` has a reason; `invalid` too.
- **It does the wrong thing**: `pcds plugin logs ID`, and `pcds plugin test` with a scenario that reproduces it.
- **It is slow or faults**: `pcds plugin stats ID` shows the fuel of each event against the budget; the log has the fault texts; the dashboard shows the fault history.
- **A hot update changed behaviour**: the state migration. The plugin's `load` gets the version of the bytes; log what you could not read.
