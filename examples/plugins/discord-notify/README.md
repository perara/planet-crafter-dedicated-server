# discord-notify

A WebAssembly plugin that uses the capability `net.http`: it posts who joins and leaves, and what
players `!report`, to a Discord webhook, in one request every five seconds. It is the example for
"a plugin that talks to the Internet" (docs/plugins.md, "net.http"; docs/plugins-api.md).

```bash
../../../scripts/build-plugin-examples        # builds plugin.wasm
pcds plugin check .                           # says where it may send requests, and what it needs from you
pcds plugin install .
pcds plugin config set discord-notify webhook # the ID/TOKEN end of the webhook's URL; read from the terminal
pcds plugin enable discord-notify             # shows https://discord.com as the only place it may reach
```

What it shows: the request returns a ticket and the answer comes as an event later; lines are
batched; a failure leaves them for the next tick; the buffer is saved state; the webhook URL is
`${config.webhook}` in the request, which the host fills in, so the module never has it and no
log or listing shows it.

`plugin.sig` is a signature by the example publisher key in `../example-publisher.pub` (a key whose
private half is in this repository, for the examples and the tests only: **never trust that key on
a real server**; make your own with `pcds plugin keygen`). See docs/plugins.md, "Signing".

## Licence

MIT (see ../LICENSE).
