# pcds-plugin-api

The plugin API of the [Planet Crafter dedicated server](https://github.com/perara/planet-crafter-dedicated-server).

A plugin is a WebAssembly module the server loads. It gets events (a player joins, a chat
command, a tick, a timer), it may ask the world questions, and it may act (say, tell, kick,
teleport, give items, change rules, send HTTP requests). This crate is that contract: the
`Plugin` trait, the `export!` macro that turns it into the module's exports, the `Host` trait
(what a plugin calls), the events and query answers, and the byte codec both sides share.
It is `no_std` with `alloc` and has no dependencies, so it builds offline.

## An example

```rust
use pcds_plugin_api::*;

struct Hello;

impl Plugin for Hello {
    fn load(host: &dyn Host, _state: Option<State<'_>>) -> Self {
        host.log(Level::Info, "hello is loaded");
        Hello
    }

    fn event(&mut self, host: &dyn Host, event: &Event<'_>) -> Verdict {
        if let Event::Command { client, name, command: "hello", .. } = event {
            let _ = host.tell(*client, &format!("Hello, {name}!"));
        }
        Verdict::Continue
    }
}

export!(Hello);
```

Build it as a `cdylib` for `wasm32-unknown-unknown` (`panic = "abort"` keeps it small). The
guide shows the manifest and the whole workflow.

## Test it without a server

With the `testing` feature, a plugin runs in plain `cargo test`: a mock host holds a small
world and records what the plugin did, and a harness delivers events and moves the clock.

```toml
[dev-dependencies]
pcds-plugin-api = { version = "0.2", features = ["testing"] }
```

```rust
#[test]
fn hello_answers_the_command() {
    use pcds_plugin_api::testing::{Harness, MockHost};

    let mut test = Harness::<Hello>::start(MockHost::new());
    test.join(3, "Ann");
    test.command(3, "hello", &[]);
    assert_eq!(test.mock().told(), [(3, "Hello, Ann!".to_string())]);
}
```

The mock can also refuse capabilities, enforce the 16 actions per call, answer `nearby`
queries, and run timers, ticks, saves and reloads. See the `testing` module.

## Features

- `std`: links the standard library. Nothing in the API needs it.
- `testing`: the `testing` module (mock host and harness). Turns `std` on. Use it as a
  dev-dependency only.

The `json` module (a small strict JSON reader and writer for RPC arguments, rules and HTTP
bodies) is always there.

## Versions

The crate's version tracks the plugin ABI: ABI 1.3 is crate 0.2. While the crate is 0.x, a minor
version may change Rust signatures (a new field on an event, a new trait method). The ABI's
wire format, the thing a built module depends on, is only ever extended within major 1: a
module built for ABI 1.2 runs on a server that speaks 1.3. Rebuilding against a newer crate is
what you do to use new features.

## Minimum Rust version

1.85 (edition 2024) is what `rust-version` says. Tested on 1.92.0, 1.94.0, 1.96.0, 1.97.1 and
1.98.1; 1.85 itself is not installed on the machine where this was written, so it is not
tested yet.

## Licence

MIT OR Apache-2.0, at your choice (the two LICENSE files). A plugin that includes this crate
stays under whatever licence its author picks. The server itself is GPL-3.0-or-later. This is
a description of the intent, not legal advice.

## More

- Guide: <https://github.com/perara/planet-crafter-dedicated-server/blob/main/docs/plugins-guide.md>
- Reference: <https://github.com/perara/planet-crafter-dedicated-server/blob/main/docs/plugins-api.md>
