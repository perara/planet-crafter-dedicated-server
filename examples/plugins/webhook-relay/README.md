# webhook-relay

An out-of-process plugin: a Python script that reads the server's event stream and posts it to a
Discord-style webhook. See `docs/plugins.md`, "Out-of-process integrations".

```bash
./relay.py --self-test                                   # no server needed
PCDS_ADMIN_TOKEN=$(pcds token) ./relay.py --dry-run      # prints the lines
PCDS_ADMIN_TOKEN=$(pcds token) WEBHOOK_URL=... ./relay.py
```
Run it as a service of its own (systemd, Docker, a screen). It holds the admin token, so it can do
what the admin API can; the server's plugin capabilities do not apply to it.
