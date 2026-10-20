# Example plugins

| Folder | Kind | Shows |
|---|---|---|
| `welcome-rules/` | data (a TOML rules file) | commands, a welcome, announcements, rules by player count; no code |
| `hello-stats/` | WebAssembly (Rust) | reading the world, a chat command, state across restarts and hot updates |
| `discord-notify/` | WebAssembly (Rust) | `net.http`: posting to a Discord webhook; batching; a secret that the plugin never sees; signed |
| `webhook-relay/` | program (Python) | the other way: an out-of-process integration with a scoped token and a long poll |

`example-publisher.key` and `.pub` are a key pair made for these examples and the tests, and the private
half is here on purpose: `discord-notify/plugin.sig` is made with it so that the signing flow can be
tried (`pcds plugin trust example-publisher.pub --grant event.player,event.chat,event.tick,chat.say`).
**Never trust that key on a server that matters**: anybody can sign with it. Make your own with
`pcds plugin keygen`.

`scripts/build-plugin-examples` builds the modules; `scripts/sign-plugin-examples` signs the one that
ships signed (after the build: a signature covers the module).
