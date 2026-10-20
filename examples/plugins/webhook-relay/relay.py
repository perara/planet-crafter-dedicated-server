#!/usr/bin/env python3
"""Relays what happens on a Planet Crafter dedicated server to a webhook (Discord's format).

An out-of-process plugin: it is an ordinary program that reads the server's event stream over the
admin API (POST /api/plugins/events) and posts text to a webhook. It needs only Python 3's standard
library, can run on another machine (reach the admin API through an SSH tunnel), and a crash of it
never touches the game.

  PCDS_TOKEN=pcdst_... WEBHOOK_URL=https://discord.com/api/webhooks/... ./relay.py

  PCDS_API        the admin API's address                       (default http://127.0.0.1:7782)
  PCDS_TOKEN      a scoped token that may read the stream: `pcds plugin token create relay --scope
                  events.read`. The admin token (`pcds token`) works too, and may do far more than
                  this program needs: use a scoped one.
  WEBHOOK_URL     where the lines go: a Discord webhook, or any URL taking {"content": "..."}
  RELAY_TYPES     events to relay, comma separated             (default chat,player_join,player_leave)
  RELAY_WAIT      seconds the server may hold a poll until there is an event (long poll; default 25,
                  at most 25, 0: poll every RELAY_POLL seconds)
  RELAY_POLL      seconds between polls when the server answers at once   (default 2)

  ./relay.py --dry-run   print the lines instead of posting them
  ./relay.py --self-test run against a fake server and a fake webhook, and check what arrives

The token and the webhook URL are never printed: the URL of a Discord webhook is a secret.
"""
import json
import os
import signal
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, HTTPServer


def line(event):
    """One event as a line of text, or None for an event this relay does not say."""
    kind = event.get("type")
    if kind == "chat":
        return "**%s**: %s" % (event.get("name") or "?", event.get("text", ""))
    if kind == "player_join":
        return "%s joined the game" % event.get("name")
    if kind == "player_leave":
        return "%s left the game" % event.get("name")
    if kind == "world_saved":
        return "World saved" if event.get("ok") else "The world could not be saved"
    if kind == "rule_change":
        return "Server rules changed: %s" % ", ".join("%s=%s" % kv for kv in sorted((event.get("changes") or {}).items()))
    return None


def post(url, payload, headers=None, timeout=10):
    request = urllib.request.Request(url, json.dumps(payload).encode(), {"Content-Type": "application/json", **(headers or {})})
    with urllib.request.urlopen(request, timeout=timeout) as answer:
        return json.load(answer) if answer.headers.get("Content-Type", "").startswith("application/json") else answer.read()


# The name this program registers under (GET /api/plugins lists it); it leaves with an unregister.
CONSUMER = "webhook-relay"


class Relay:
    def __init__(self, api, token, webhook, types, send, wait=25):
        self.api, self.token, self.webhook, self.types, self.send, self.wait = api, token, webhook, types, send, wait
        self.after, self.epoch = 0, None

    def poll(self):
        """One poll; the number of lines sent."""
        body = {"consumer": CONSUMER, "after": self.after, "types": self.types}
        if self.wait:
            # The server holds the request until there is an event or the wait is over (a front that does
            # not know "wait" answers at once; run() then paces the loop).
            body["wait"] = self.wait
        answer = post(self.api + "/api/plugins/events", body, {"Authorization": "Bearer " + self.token}, timeout=self.wait + 15)
        # The server restarted (a new epoch): its ids began again.
        if self.epoch not in (None, answer["epoch"]):
            self.after = 0
            self.epoch = answer["epoch"]
            return self.poll()
        self.epoch = answer["epoch"]
        sent = 0
        for event in answer["events"]:
            text = line(event)
            if text:
                self.send(text)
                sent += 1
            self.after = event["id"]
        # Nothing new: skip what was filtered, so the next poll starts after the newest.
        self.after = max(self.after, answer["last"]) if not answer["events"] else self.after
        return sent

    def leave(self):
        """Deregisters: the server stops recording what only this program wanted."""
        try:
            post(self.api + "/api/plugins/events", {"consumer": CONSUMER, "unregister": True}, {"Authorization": "Bearer " + self.token}, timeout=3)
        except (urllib.error.URLError, OSError, ValueError):
            pass

    def run(self, interval):
        failures = 0
        while True:
            try:
                started = time.monotonic()
                sent = self.poll()
                failures = 0
                # An answer at once with nothing in it (no long poll, or a wait of 0): do not spin.
                if not sent and time.monotonic() - started < 0.5:
                    time.sleep(interval)
            except (urllib.error.URLError, OSError, ValueError) as e:
                failures += 1
                # Never the URLs or the token: only what went wrong.
                print("relay: %s (try %d)" % (type(e).__name__, failures), file=sys.stderr)
            time.sleep(min(interval * 2 ** min(failures, 5), 60) if failures else interval)


def self_test():
    seen, hooked, left = [], [], []

    class Fake(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if self.path == "/api/plugins/events":
                assert self.headers["Authorization"] == "Bearer secret-token"
                if body.get("unregister"):
                    left.append(body["consumer"])
                    return self._answer({"ok": True})
                seen.append(body)
                events = [e for e in [
                    {"id": 1, "type": "player_join", "name": "Ann"},
                    {"id": 2, "type": "chat", "name": "Ann", "text": "hello"},
                    {"id": 3, "type": "tick"},
                ] if e["id"] > body["after"]]
                out = {"epoch": 7, "events": events, "last": 3, "dropped": 0}
            else:
                hooked.append(body)
                out = {}
            self._answer(out)

        def _answer(self, out):
            data = json.dumps(out).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    server = HTTPServer(("127.0.0.1", 0), Fake)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = "http://127.0.0.1:%d" % server.server_port
    relay = Relay(base, "secret-token", base + "/hook", ["chat", "player_join"], lambda t: post(base + "/hook", {"content": t}))
    # A poll without the long wait asks as before.
    plain = Relay(base, "secret-token", base + "/hook", ["chat"], lambda t: None, wait=0)
    assert relay.poll() == 2
    assert relay.poll() == 0, "the second poll starts after the first's last event"
    assert plain.poll() == 2 and "wait" not in seen[-1], seen
    relay.leave()
    server.shutdown()
    assert [h["content"] for h in hooked] == ["Ann joined the game", "**Ann**: hello"], hooked
    assert seen[0] == {"consumer": "webhook-relay", "after": 0, "types": ["chat", "player_join"], "wait": 25} and seen[1]["after"] == 3, seen
    assert left == ["webhook-relay"], left
    print("self-test ok")


def main(argv):
    if "--self-test" in argv:
        return self_test()
    token = os.environ.get("PCDS_TOKEN")
    webhook = os.environ.get("WEBHOOK_URL")
    dry = "--dry-run" in argv
    if not token or not (webhook or dry):
        sys.exit("set PCDS_TOKEN and WEBHOOK_URL (or use --dry-run); see the top of this file")
    send = print if dry else (lambda text: post(webhook, {"content": text[:1900]}))
    relay = Relay(os.environ.get("PCDS_API", "http://127.0.0.1:7782"), token, webhook, os.environ.get("RELAY_TYPES", "chat,player_join,player_leave").split(","), send, min(int(os.environ.get("RELAY_WAIT", "25")), 25))
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    try:
        relay.run(float(os.environ.get("RELAY_POLL", "2")))
    finally:
        relay.leave()


if __name__ == "__main__":
    main(sys.argv[1:])
