# webhook-relay

An out-of-process plugin: a Python script that reads the server's event stream and posts it to a
Discord-style webhook. See `docs/plugins.md`, "Out-of-process integrations" and "Scoped tokens".

```bash
./relay.py --self-test                                   # no server needed
pcds plugin token create relay --scope events.read       # a token that may read the stream and nothing else
PCDS_TOKEN=pcdst_... ./relay.py --dry-run                # prints the lines
PCDS_TOKEN=pcdst_... WEBHOOK_URL=... ./relay.py
```
Run it as a service of its own (systemd, Docker, a screen). Give it a scoped token, not the admin token:
a leak of this program is then a leak of the event stream, not of the server. It asks with a long poll
(`"wait": 25`), so events reach the webhook when they happen; with a front older than that feature it falls
back to polling every `RELAY_POLL` seconds. (If you only want Discord to hear of joins and leaves, the WebAssembly
plugin `../discord-notify` does it inside the server with `net.http` and needs no program of its own.)
