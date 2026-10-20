# Example plugins

Every folder here is a plugin you can install, read and copy; the guide (docs/plugins-guide.md, "Cookbook")
says what each teaches. Each has a README, a manifest, `scenarios/` for `pcds plugin test`, and for the
Rust ones tests with the mock server of the API crate (`cargo test`). The server's own tests run each of them
in the real game (`server/src/plugins/cookbook_tests.rs`).

| Folder | Kind | Shows |
|---|---|---|
| `welcome-rules/` | data (a TOML rules file) | commands, a welcome, announcements, rules by player count; no code. Unsigned on purpose |
| `server-motd/` | data | a message of the day, `!rules`/`!help`, tips that rotate, none to an empty server |
| `scaling-rules/` | data | the speed rules by bands of player counts |
| `hello-stats/` | WebAssembly (Rust) | reading the world, a chat command, state across restarts and hot updates, a state migration. Unsigned on purpose |
| `afk-guard/` | WebAssembly (Rust) | a moderator: idle players, who takes buildings apart, operator commands, strikes by `player_id` |
| `playtime-board/` | WebAssembly (Rust) | a leaderboard in plugin state, `!top`, state version 1 to 2 |
| `quest-rewards/` | WebAssembly (Rust) | giving items and Terra tokens for milestones, with honest bookkeeping |
| `claims/` | WebAssembly (Rust) | world protection: claims, trust, a veto of building and taking apart |
| `restart-notice/` | WebAssembly (Rust) | timers and the wall clock: warnings before a scheduled restart, save and update notices |
| `chat-bridge/` | WebAssembly (Rust) + program (Python) | Discord and Slack, both directions: `net.http` out, a poller with a scoped token in |
| `discord-notify/` | WebAssembly (Rust) | `net.http`: posting to a Discord webhook; batching; a secret the plugin never sees |
| `hello-c/` | WebAssembly (C) | the same interface from C, with no libc |
| `prometheus-exporter/` | program (Python) | metrics for Prometheus from the event stream, the status and the plugin listing |
| `webhook-relay/` | program (Python) | an out-of-process integration with a scoped token and a long poll |

`example-publisher.key` and `.pub` are a key pair made for these examples and the tests, and the private
half is here on purpose: the signatures (`plugin.sig`) of all the examples but the two marked are made with it so that
the signing flow can be tried (`pcds plugin trust example-publisher.pub --grant event.player,event.chat,event.tick,chat.say`).
**Never trust that key on a server that matters**: anybody can sign with it. Make your own with `pcds plugin keygen`.

`scripts/build-plugin-examples` builds the Rust modules (the bytes do not depend on where the checkout is; `--check` fails
when a committed `plugin.wasm` is not what its source builds). The C module is built with `make` in its folder: the
bytes of a C module depend on the compiler's version, so it is not compared. `scripts/sign-plugin-examples` signs the
examples (after the build: a signature covers the module); a test fails when a signature is not current.
