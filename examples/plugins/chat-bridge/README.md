# chat-bridge

The game's chat in a Discord (or Slack) channel, and the channel's messages in the game's chat.
Two programs, one for each direction:

```
 game ──chat, joins, leaves──▶ chat-bridge (WebAssembly plugin, net.http) ──▶ Discord / Slack webhook
 game ◀──"[Discord] Ann: hi"── bridge_in.py (a program, admin API, scoped token) ◀── Discord, polled
```

| | outbound: `plugin.wasm` | inbound: `bridge_in.py` |
|---|---|---|
| runs | inside the server | anywhere that reaches both Discord and the admin API |
| holds | nothing secret: the webhook is `${config.webhook}`, put in by the host | the Discord bot token and a scoped token |
| can do | send HTTP to `discord.com` and `hooks.slack.com`, nothing else | say things in the chat, nothing else |
| crash | switched off after repeated faults, the game goes on | the game never notices |

A plugin cannot wait for a network message: it can only send one. So the inbound half has to be a
program that asks. See `docs/plugins.md`, "Out-of-process integrations".

## The outbound half

```bash
cd examples/plugins/chat-bridge
cargo test                                   # 14 tests, no server
cargo build --target wasm32-unknown-unknown --release && cp target/wasm32-unknown-unknown/release/chat_bridge.wasm plugin.wasm
pcds plugin check .                          # fuel per event, what it asks for
pcds plugin test .                           # scenarios/*.toml in the real host, nothing is sent
pcds plugin install .
pcds plugin config set chat-bridge webhook   # Discord: ID/TOKEN. Slack: T000/B000/XXXX. Read from the terminal.
pcds plugin enable chat-bridge               # shows discord.com and hooks.slack.com as the only places it may reach
```

The webhook is the part of the URL after `https://discord.com/api/webhooks/` (or
`https://hooks.slack.com/services/`). Both hosts are in `[net] allow`; `style` decides which one is used.

| Setting | Type | Default | Meaning |
|---|---|---|---|
| `webhook` | secret | none | the end of the webhook URL; the plugin never sees it |
| `style` | `discord` or `slack` | `discord` | payload shape and host |
| `flush_seconds` | int 1 to 60 | 3 | at most one request in this many seconds of game time; lines in between go in one message |
| `send_chat` | bool | true | what players say |
| `send_joins` | bool | true | joins and leaves |
| `command_prefix` | string, up to 4 | `!` | messages that start with it are commands and are not sent |

`command_prefix` repeats the server's `chat.commandPrefix` rule: reading the rule needs `world.read`, which the plugin
would need for nothing else.

What it does:

- **Batching.** Lines are kept and sent together, up to 1800 bytes a message. The first line after a quiet
  time goes at the next tick (at most a second); after that at most one request per `flush_seconds`.
- **Text from strangers.** Names and messages lose control, zero width and direction characters and are cut
  (32 and 300 characters). `<` and `>` become look-alikes (so `<@123>`, `<!channel>` and `<url|text>` are
  plain text), `@` is followed by a zero width space (`@everyone` is not a mention), backticks become
  apostrophes, and the markdown characters `* _ ~ | \` are escaped (Discord) or replaced by look-alikes
  (Slack). Discord also gets `"allowed_mentions": {"parse": []}`, so nothing pings even if something got through.
  JSON is built with `json::Object`.
- **Commands are not conversation.** A message that starts with the prefix is not sent.
- **Failure.** A request that did not get a 2xx answer leaves its lines in the buffer. The wait before the
  next try doubles (6 s, 12 s ... up to 5 minutes) and is at least a `Retry-After` header. A 4xx other than 408
  and 429 (a deleted webhook, a refused message) drops the lines of that request, so one bad line cannot block
  the rest. A request with no answer in a minute counts as failed.
- **Bounded.** 200 lines at most; the oldest go first, and the next message starts with
  `(N earlier lines were lost)`. The buffer and the failure count are saved state: a restart or a hot update
  loses nothing it had not sent.
- **The first tick of a new instance** (a restart, a hot update) carries the whole game time as `since_ms`, because the host counts from 0, so lines that were waiting go at the next tick whatever the wait was.
- **No chat text in the log.** Log lines have counts and HTTP statuses only.
- **An empty world has no ticks** (`world.pauseWhenEmpty`). The leave of the last player is therefore sent in
  the leave's own call, and so is anything that came while that request was out. A failure at that moment
  waits for the next player's first tick.

No echo: the inbound half says `[Discord] Ann: hi` as the server, and the plugin hears `chat` events only for
what players type through the game's chat RPC. A test (`a_player_cannot_make_the_bridge_echo_the_other_half`)
documents it; confirm it once on your server by saying something with `POST /api/chat` and watching the
stream for a `chat` event.

`pcds plugin check` reports 54 KB, about 30 to 70 thousand fuel per event of a million (a 300 character
message of nothing but `@`, the worst case, is 290 thousand).

## The inbound half

`bridge_in.py` polls a Discord channel (`GET /channels/{id}/messages?after=...&limit=20`, every 3 seconds) and
says each new message in the game with `POST /api/chat` as `[Discord] Name: text`.

```bash
./bridge_in.py --self-test                                       # no network: fake Discord, fake server
pcds plugin token create bridge --scope chat.send                # shown once
DISCORD_TOKEN=... DISCORD_CHANNEL=123456789 ./bridge_in.py --dry-run          # prints the lines
DISCORD_TOKEN=... DISCORD_CHANNEL=123456789 PCDS_TOKEN=pcdst_... ./bridge_in.py
```

Environment: `DISCORD_TOKEN`, `DISCORD_CHANNEL`, `PCDS_TOKEN`, `PCDS_API` (default `http://127.0.0.1:7782`),
`BRIDGE_POLL` (seconds, default 3). Run it as a service of its own (systemd, Docker).

- It ignores bots, webhooks (the outbound half's own messages in the same channel) and its own messages.
- The text is cut to 200 characters; control characters, mention and emoji syntax (`<@123>` becomes `@Name`)
  and `<`/`>` (the game takes rich text tags) are removed or replaced.
- At most 5 messages in 10 seconds are said. The rest are counted, and one line, `[Discord] (N messages
  skipped)`, says how many once the window has room.
- Errors back it off (3, 6, 12 ... up to 60 seconds, or a Discord `Retry-After`). A message that could not
  be said stays where it is and is tried again; one the server refuses with a 4xx (not 401, 403, 408, 429) is let
  go. Errors are printed by kind and status only, never a token or a URL.
- It starts after the newest message of the channel: history is not replayed, and messages written while it
  is down are not brought in.

### Tokens and why not the admin token

`events.read` is not needed by either half. The inbound program needs one scope, **`chat.send`**
(`POST /api/chat`); a scoped token opens that request and nothing else. The admin token also stops the world,
changes the rules, installs plugins and makes more tokens: a leak of this program (it talks to the Internet
all day and holds a bot token as well) would be a leak of the whole server. With the scoped token it is the
ability to say things in the chat, which the bridge does anyway. Revoke with
`pcds plugin token revoke bridge`.

### The Discord bot in six lines

1. discord.com/developers/applications, New Application, Bot, Reset Token: that is `DISCORD_TOKEN`.
2. On the same page switch on **Message Content Intent**. Without it Discord sends empty messages.
3. OAuth2, URL Generator, scope `bot`, permissions View Channel and Read Message History; open the URL and add the bot to your server.
4. In the channel's permissions give the bot only those two (no Send Messages: it never writes to Discord).
5. Settings, Advanced, Developer Mode on; right click the channel, Copy Channel ID: that is `DISCORD_CHANNEL`.
6. For the outbound half: channel settings, Integrations, Webhooks, New Webhook, Copy URL, and keep only what follows `/webhooks/`.

## What is not built

- **No Discord gateway.** The inbound half polls the REST API (a message arrives after up to `BRIDGE_POLL`
  seconds); a websocket client is not in the standard library.
- No state file: the position in the channel is in memory.
- No two-way names: Discord users appear by display name; there is no mapping to players.
- No Discord slash commands, no embeds, no attachments (a message with only a file shows `(sent a file)`), no threads.
- Slack inbound (Slack needs Socket Mode or an HTTPS endpoint the server does not have).
- No per-channel routing: one webhook, one channel.

## Licence

MIT (see ../LICENSE).
