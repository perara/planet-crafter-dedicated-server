# Your first plugin, in ten steps

A plugin adds something to your server without changing it: a chat command, a reaction to a player
joining, a rule that follows how many are online. This page makes one, from nothing to working in
the game. The ideas behind it are in docs/plugins.md; every detail is in docs/plugins-api.md.

There are three kinds of plugin. Pick the smallest that does the job:

| You want | Make | Needs |
|---|---|---|
| a message, a command that answers with a text, an announcement, a rule change by player count | **a rules file** (a TOML file; "A plugin that is only data" below) | nothing but an editor |
| logic: counting, remembering, deciding, reading positions | **a WebAssembly module** (this guide, steps 1 to 10) | Rust (or C, Zig...) |
| a Discord bridge, a database, a web page, anything that must reach the Internet | **a program** that reads the event stream ("A program of its own" below) | any language |

## The ten steps

You need: a server of a release with plugins running (README), its game tables (the server runs),
Rust with the WebAssembly target (`rustup target add wasm32-unknown-unknown`), and `pcds` on your
PATH.

### 1. Make the plugin

```bash
pcds plugin new greeter
cd greeter
```

This makes a folder with a Rust crate (`Cargo.toml`, `src/lib.rs`), a manifest (`plugin.toml`), a
`.cargo/config.toml` that makes `cargo build` build for WebAssembly, and a README. The crate
depends on `pcds-plugin-api`, the small library that holds the events and the way to talk to the
server. (`--api-path DIR` points it at a copy of the library on your disk instead of the
repository's tag.)

### 2. Read the manifest

`plugin.toml` says what the plugin is and what it asks for:

```toml
[plugin]
id = "greeter"
name = "greeter"
version = "0.1.0"
kind = "wasm"
entry = "target/wasm32-unknown-unknown/release/greeter.wasm"
abi = 1
capabilities = ["event.chat", "chat.say"]

[budget]
fuel_per_event = 500000

[[command]]
name = "hello"
help = "says hello"
```

- `capabilities` is the list of things the plugin may do **if the admin allows them**. `event.chat`
  is "be told about chat messages and commands", `chat.say` is "write to the chat". The full list
  is in docs/plugins-api.md. Ask for what you use and no more: the admin reads this list when
  approving, and a long one is a reason to say no.
- `[[command]]` declares `!hello`. The server delivers a chat message that starts with `!` and
  `hello` to your plugin and to nobody else.
- `fuel_per_event` is how much work one event may do before it is stopped. 500,000 is about a
  fifth of a millisecond of ordinary code; most events need a hundredth of it.

### 3. Read the code

`src/lib.rs` is a `Plugin`: three functions, and a line that turns it into a module.

```rust
use pcds_plugin_api::*;

struct MyPlugin { greeted: u32 }

impl Plugin for MyPlugin {
    // Once at the start, after a reload, after a hot update. `state` is what `save` gave last time.
    fn load(_host: &dyn Host, state: Option<State<'_>>) -> Self { ... }

    // Each event you subscribed to. Answer Verdict::Continue unless you mean to refuse an RPC.
    fn event(&mut self, host: &dyn Host, event: &Event<'_>) -> Verdict { ... }

    // What to keep across restarts and hot updates.
    fn save(&mut self) -> Option<SavedState> { ... }
}

export!(MyPlugin);
```

`host` is how a plugin asks and acts: `host.world()` reads the world, `host.tell(client, text)`
writes to one player, `host.say(text)` to everyone, `host.log(level, text)` to the server's log.
Everything it does goes through `host`, and each call needs a capability.

### 4. Change it

Make `!hello` say who is online. In `plugin.toml` add `world.read` to the capabilities; in
`src/lib.rs`:

```rust
fn event(&mut self, host: &dyn Host, event: &Event<'_>) -> Verdict {
    if let Event::Command { client, name, command: "hello", .. } = event {
        self.greeted += 1;
        let online = host.players().map(|p| p.len()).unwrap_or(0);
        let _ = host.tell(*client, &format!("Hello {name}! {online} online. I have said hello {} times.", self.greeted));
    }
    Verdict::Continue
}
```

Calls return a `Result`: a plugin that is not allowed to do something gets `Err(Error::Denied)`,
and should carry on without it. `let _ =` says so. Do not `unwrap()`: a panic in a plugin is a
fault (the server restarts the plugin and counts it, and five in a minute switch it off).

### 5. Build it

```bash
cargo build --release
```

That is `target/wasm32-unknown-unknown/release/greeter.wasm`, about 20 KB.

### 6. Check it

```bash
pcds plugin check .
```

```
  greeter 0.1.0 (wasm, plugin ABI 1)
  asks for event.chat: read chat messages and chat commands
  asks for world.read: read the world: time, players, positions, counts, rules
  asks for chat.say: write to the chat, to everyone or to one player
  module of 20220 bytes, 1 imports
  commands: !hello
  command sample: ok, 101209 fuel, 1 actions asked
  state: 4 bytes, version 1
pcds: . is fine (0 warnings)
```

`check` starts the plugin and shows it a sample of every event it subscribed to, in a made-up
world of two players, so a plugin that traps on the first day is found now. It also compares the
server functions the module calls with the capabilities the manifest asks for: forget `world.read`
in step 4 and it says

```
  ERROR: the module calls a host function that needs world.read, which the manifest does not ask
  for: every such call would be refused
```

### 7. Install it

```bash
pcds plugin install .
```

This copies `plugin.toml` and the module to `run/plugins/greeter/`. It does nothing yet: a plugin
waits for you to approve it. The server does not need to be told: it looks at the folder every few
seconds, so a plugin folder copied there by hand (or a `plugin.wasm` you build over and copy again)
is found within a few seconds with the server running. No restart, no hot update.

### 8. Approve it

```bash
pcds plugin enable greeter
```

```
greeter 0.1.0 by You (wasm, MIT licence)
  What this plugin does, in one sentence
  grant: event.chat: may read chat messages and chat commands
  grant: world.read: may read the world: time, players, positions, counts, rules
  grant: chat.say: may write to the chat, to everyone or to one player
  digest 539cd82d...
Approve with these capabilities? [y/N]
```

Say `y`. This is the admin's decision, not the author's: you can allow less
(`--grant event.chat,chat.say`) and the plugin runs with what it got. If the server is running it
reads the change at once; otherwise at its next start. The same is on the web page, in **Plugins**
(key 9).

The approval is for these files: the digest above is what you approved. If the plugin's files
change, the plugin waits for you again, and the version you approved keeps running meanwhile
(`pcds plugin list` shows the notice). Approving the new files replaces it, with its state handed
over; if the new version does not start, the old one goes on and the listing says why.

### 9. Try it in the game

Type `!hello` in the chat. You get the answer, and only you.

```bash
pcds plugin list
```

```
ID                   VERSION  KIND  STATE        CALLS FAULTS  CAPABILITIES (granted of asked)
greeter              0.1.0    wasm  running          3      0  3/3: event.chat world.read chat.say
```

`CALLS` and `FAULTS` are counted since the bank started. The log (`pcds logs`) has the plugin's
own lines (`host.log(...)`) as `plugin greeter: ...`.

### 10. Keep something, through a restart and a hot update

`save` is asked at every world save and just before a hot update; `load` gets it back. That is all
it takes for `greeted` to survive: `load` reads it, `save` writes it (the template does). Try:

```bash
pcds api POST /api/update '{"rollback": true}'     # hands the world to the other bank
```

Then `!hello` again: the count goes on. The state is bytes and a version number you choose. When
your plugin's state changes shape, raise the version and let `load` read both:

```rust
fn load(host: &dyn Host, state: Option<State<'_>>) -> Self {
    match state {
        Some(State { version: 1, bytes }) => { /* the old layout */ }
        Some(State { version: 2, bytes }) => { /* the new one */ }
        Some(State { version, .. }) => host.log(Level::Warn, &format!("state version {version} is from a newer plugin; starting empty")),
        None => { /* first start */ }
    }
}
```

`examples/plugins/hello-stats` does this (its version 1 had one counter, version 2 has two).

### Switching off, replacing and removing, live

`pcds plugin disable greeter`, deleting `run/plugins/greeter/`, or approving a rebuilt module all
take effect with the server running. When a plugin is switched off or removed it gets one last
call, `Plugin::unload`, and then `save`, each with a budget, before its instance and memory are
dropped; its commands are gone at once. Replacing it (a reload) does not call `unload`: `save` and
the new version's `load` carry the state over.

```rust
fn unload(&mut self) {
    // Optional. Put the last things into your state; nothing you queue here is applied.
}
```

## Testing on your PC

A plugin is Rust against the `Host` trait, so its tests need no server and no WebAssembly: write
a `Mock` that implements `Host`, make the plugin with `Plugin::load`, hand it `Event` values and look
at what the mock was told. `examples/plugins/hello-stats/src/lib.rs` has two such tests; run them
with `cargo test --target x86_64-unknown-linux-gnu` (the template's `.cargo/config.toml` sets the
WebAssembly target for `cargo build`).

## When something goes wrong

| You see | It means | Do |
|---|---|---|
| `pending` in `pcds plugin list` | installed, never approved | `pcds plugin enable ID` |
| `changed` | a file differs from what was approved (an update, an edit) | look at what changed, `pcds plugin enable ID` again |
| `invalid: the module imports X.Y, which this server does not provide` | the module was built with WASI or another import | build for `wasm32-unknown-unknown`, not `wasm32-wasip1` |
| `incompatible: built for plugin ABI 2` | made for another API major than this server speaks (a major bump breaks plugins: there is no shim) | update the server, or rebuild the plugin for this server's ABI |
| `failed: start failed: ...` | the module trapped at its start, or wants more memory than allowed | read the message; raise `memory_pages` in `[budget]` up to 1024 |
| `plugin ID: chat: out of fuel` | an event did more work than its budget | do less per event (move work to when it is needed), or ask for more `fuel_per_event` |
| `plugin ID: chat: trap: wasm unreachable` | the plugin panicked | read the message above it; the plugin was restarted from its last saved state |
| `failed: switched off after 5 faults in 60 s` | it keeps faulting | fix it, `pcds plugin reload` |
| `running` with a notice `a reload failed: ...` | the new version did not start; the old one goes on | read the reason, rebuild, approve again |
| commands do nothing | the chat prefix, or the capability | `pcds rules` shows `chat.commandPrefix` (unset means `!`); `pcds plugin list` shows what is granted |

## Rules for a good plugin

- **Be short.** An event has a budget of about a third of a millisecond. Read, decide, ask, return.
  Anything that takes long belongs in a program of its own.
- **Never panic, never `unwrap`.** Every call returns a `Result`; a refusal is normal.
- **Answer `Verdict::Continue`** to every event you do not understand. A newer server may send
  kinds you do not know; the `export!` macro does that for you for unknown kinds.
- **Ask for the least.** Each capability is a sentence the admin must read.
- **Do not refuse RPCs** (`rpc.veto`) unless you must: an unmodified game client that sent one
  expects it to work. Tell the player why with `host.tell`.
- **Keep state small and versioned.** A megabyte at most; raise the version when the layout
  changes.
- **Do not log what players say.** The server never logs chat text, tokens or payloads; neither
  should a plugin.

## A plugin that is only data

For the many servers whose wish is "when someone types !rules, say this", no code is needed.

```bash
pcds plugin new rules-and-welcome --kind data
cd rules-and-welcome
```

`rules.toml`:

```toml
[vars]
discord = "https://discord.gg/example"

[[command]]
name = "rules"
reply = "Be kind. No griefing. More: {discord}"
cooldown_seconds = 5

[[on_join]]
say = "Welcome, {name}! {players} online."

[[every]]
seconds = 900
say = "Join us: {discord}"

[[when_players]]                # from 6 players on, plants grow twice as fast
at_least = 6
set_rules = { speed = { growth = 2.0 } }
otherwise_rules = { speed = { growth = 1.0 } }
```

`plugin.toml` lists the capabilities the file needs (`event.chat`, `event.player`, `event.tick`,
`world.read` for `{players}`, `chat.say`, `rules.write`); `pcds plugin check .` tells which are
missing or unused. Then `install` and `enable` as above. There is a complete one in
`examples/plugins/welcome-rules`. Rule changes from a plugin are limited to the gameplay rules
(`speed`, `gauges`, `meteo`, `death`, `loot`, `limits`, `world`): never the chat, backup or access
rules.

## A program of its own

For anything that reaches outside the server, run a separate program. It asks the server for
events and uses the admin API to act. `examples/plugins/webhook-relay/relay.py` relays chat,
joins and leaves to a Discord webhook:

```bash
PCDS_ADMIN_TOKEN=$(pcds token) WEBHOOK_URL=https://discord.com/api/webhooks/... ./relay.py
```

The core of any such program is a loop:

```python
answer = post(api + "/api/plugins/events", {"consumer": "my-bridge", "after": after, "types": ["chat", "player_join"]},
              {"Authorization": "Bearer " + token})
for event in answer["events"]:
    ...                       # {"id", "time", "type": "chat", "client", "name", "text"}
    after = event["id"]
```

and, to act, the endpoints that exist: `POST /api/chat {"text"}`, `/api/kick`, `/api/teleport`,
`/api/rules`. The first call registers you (by the `consumer` name) and turns the recording on for the kinds
you name, for the minute after each call; poll every second or two. Leave with
`{"consumer": "my-bridge", "unregister": true}` when you stop (`relay.py` does, on exit and on SIGTERM);
silent for a minute, you are dropped anyway. `pcds plugin list` and the dashboard show who is registered. If `epoch` in an answer is not the one you saw, the server
restarted: start again from `after: 0`. Such a program has the admin token and so every power the
admin API has: run it as a service of its own, as a user of its own, and keep the token out of its
logs.

## Next

- docs/plugins-api.md: every event, field, capability, limit and endpoint.
- docs/plugins.md: why it is built this way, what it costs, what it will not do.
- `examples/plugins/`: `hello-stats` (Rust, with state), `welcome-rules` (data), `webhook-relay`
  (a program).
