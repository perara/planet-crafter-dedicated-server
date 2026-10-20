#!/usr/bin/env python3
"""Brings what is said in a Discord channel into a Planet Crafter dedicated server's chat.

The other half of chat-bridge (the WebAssembly plugin sends the game's chat out; this program brings
Discord's in). An out-of-process plugin: an ordinary program that polls Discord's REST API and says
the messages in the game with the admin API (POST /api/chat). Python 3's standard library only.
A crash of it never touches the game. It does not use Discord's gateway (a websocket): it polls.

  DISCORD_TOKEN=... DISCORD_CHANNEL=... PCDS_TOKEN=pcdst_... ./bridge_in.py

  DISCORD_TOKEN    the bot's token (Developer Portal, Bot, Reset Token). The bot needs "View Channel"
                   and "Read Message History" in the channel, and the Message Content intent switched
                   on, or Discord sends the messages with empty text.
  DISCORD_CHANNEL  the channel's ID (Discord, Settings, Advanced, Developer Mode; right click the channel).
  PCDS_API         the admin API's address                       (default http://127.0.0.1:7782)
  PCDS_TOKEN       a scoped token that may say things in the chat and nothing else:
                   `pcds plugin token create bridge --scope chat.send`. Not the admin token.
  BRIDGE_POLL      seconds between polls of Discord              (default 3, at least 1)

  ./bridge_in.py --dry-run    print the lines instead of saying them (PCDS_TOKEN is not needed)
  ./bridge_in.py --self-test  run against a fake Discord and a fake server, and check what arrives

In the game a Discord message appears as `[Discord] Name: text`. At most 5 messages in 10 seconds are
passed on; the rest are counted and one line, `[Discord] (N messages skipped)`, says so. The text is cut
to 200 characters and cleaned of control characters, Discord's mention and emoji syntax, and `<`/`>`
(the game's text takes rich text tags). Bots, webhooks (the outbound half's messages among them) and
this bot's own messages are ignored. Messages written while this program is not running are not
brought in: it starts after the newest message of the channel.

The tokens and the URLs are never printed: errors are shown by their kind and HTTP status only.
"""
import collections
import contextlib
import io
import json
import os
import re
import signal
import sys
import threading
import time
import unicodedata
import unittest
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, HTTPServer

DISCORD = "https://discord.com/api/v10"
# Discord refuses the default agent of urllib.
AGENT = "DiscordBot (https://github.com/planet-crafter-dedicated-server, 1) chat-bridge"
TEXT_MAX = 200
NAME_MAX = 24
# At most this many messages in this many seconds go into the game.
RATE_COUNT, RATE_SECONDS = 5, 10


class HttpError(Exception):
    """An answer that is not 2xx. Carries the status and the wait the server asked for, never the URL."""

    def __init__(self, status, retry_after=0.0):
        super().__init__("HTTP %d" % status)
        self.status, self.retry_after = status, retry_after


def call(method, url, headers, body=None, timeout=10):
    """One request; the answer's JSON (None for an empty answer). Raises HttpError, URLError, OSError, ValueError."""
    data = json.dumps(body).encode() if body is not None else None
    request = urllib.request.Request(url, data, {"Content-Type": "application/json", **headers}, method=method)
    try:
        with urllib.request.urlopen(request, timeout=timeout) as answer:
            raw = answer.read(1 << 20)
            return json.loads(raw) if raw.strip() else None
    except urllib.error.HTTPError as e:
        try:
            wait = float(e.headers.get("Retry-After") or 0)
        except ValueError:
            wait = 0.0
        raise HttpError(e.code, wait) from None


def tidy(text):
    """Control characters become spaces, invisible and direction characters go, runs of spaces become one."""
    out = []
    for c in text:
        kind = unicodedata.category(c)
        if kind == "Cc" or c in "  ":
            out.append(" ")
        elif kind not in ("Cf", "Co", "Cs", "Cn"):
            out.append(c)
    return " ".join("".join(out).split())


def clean(content, mentions=None, limit=TEXT_MAX):
    """A Discord message's text as the game should show it. `mentions`: {user id: name} from the message."""
    names = mentions or {}
    content = re.sub(r"<a?:(\w+):\d+>", r":\1:", content)
    content = re.sub(r"<@&\d+>", "@role", content)
    content = re.sub(r"<#\d+>", "#channel", content)
    content = re.sub(r"<@!?(\d+)>", lambda m: "@" + names.get(m.group(1), "user"), content)
    content = re.sub(r"<(https?://[^>\s]+)>", r"\1", content)
    text = tidy(content).replace("<", "‹").replace(">", "›")
    return text if len(text) <= limit else text[: limit - 1] + "…"


def author_name(author):
    name = tidy(author.get("global_name") or author.get("username") or "?").replace("<", "‹").replace(">", "›")
    return name[:NAME_MAX] or "?"


class Limiter:
    """`count` messages in `seconds`: a sliding window."""

    def __init__(self, count=RATE_COUNT, seconds=RATE_SECONDS, clock=time.monotonic):
        self.count, self.seconds, self.clock, self.times = count, seconds, clock, collections.deque()

    def allow(self):
        now = self.clock()
        while self.times and now - self.times[0] >= self.seconds:
            self.times.popleft()
        if len(self.times) >= self.count:
            return False
        self.times.append(now)
        return True


class Bridge:
    def __init__(self, channel, discord_token, say, discord=DISCORD, clock=time.monotonic):
        self.channel, self.say, self.discord = channel, say, discord
        self.headers = {"Authorization": "Bot " + discord_token, "User-Agent": AGENT}
        self.last = None  # the newest message seen (a snowflake as text); None: not looked yet
        self.me = None
        self.limiter = Limiter(clock=clock)
        self.skipped = 0

    def get(self, path):
        return call("GET", self.discord + path, self.headers) or []

    def poll(self):
        """One look at the channel. Returns True if there may be more at once (a full page was read).
        Raises HttpError, URLError, OSError or ValueError; what was not passed on is read again next time."""
        if self.me is None:
            self.me = str((call("GET", self.discord + "/users/@me", self.headers) or {}).get("id", ""))
        base = "/channels/%s/messages" % self.channel
        if self.last is None:
            # Start after the newest message: the history is not brought in.
            self.last = max((m["id"] for m in self.get(base + "?limit=1")), key=int, default="0")
            return False
        page = sorted(self.get(base + "?after=%s&limit=20" % self.last), key=lambda m: int(m["id"]))
        # Skipped messages are told as soon as the window has room.
        if self.skipped and self.limiter.allow():
            self.say("[Discord] (%d messages skipped)" % self.skipped)
            self.skipped = 0
        for message in page:
            if not self.pass_on(message):
                break
            self.last = message["id"]
        return len(page) >= 20

    def pass_on(self, message):
        """Says one message in the game. False: it could not be said and should be tried again."""
        author = message.get("author") or {}
        # Type 0 is a message, 19 a reply; joins, pins and the like are not conversation.
        if author.get("bot") or message.get("webhook_id") or str(author.get("id")) == self.me or message.get("type", 0) not in (0, 19):
            return True
        mentions = {str(u.get("id")): u.get("global_name") or u.get("username") or "user" for u in message.get("mentions") or []}
        text = clean(message.get("content") or "", mentions) or ("(sent a file)" if message.get("attachments") else "")
        if not text:
            return True
        if not self.limiter.allow():
            self.skipped += 1
            return True
        try:
            self.say("[Discord] %s: %s" % (author_name(author), text))
        except HttpError as e:
            # The server refused this message itself (a 4xx other than "slow down"): trying again will not help.
            if 400 <= e.status < 500 and e.status not in (401, 403, 408, 429):
                return True
            raise
        return True

    def run(self, interval):
        failures = 0
        while True:
            more = False
            wait = interval
            try:
                more = self.poll()
                failures = 0
            except HttpError as e:
                failures += 1
                wait = max(e.retry_after, min(interval * 2 ** min(failures, 5), 60))
                print("bridge: %s (try %d)" % (e, failures), file=sys.stderr)
            except (urllib.error.URLError, OSError, ValueError, KeyError) as e:
                failures += 1
                wait = min(interval * 2 ** min(failures, 5), 60)
                # Never the URLs or the tokens: only what went wrong.
                print("bridge: %s (try %d)" % (type(e).__name__, failures), file=sys.stderr)
            if not more or failures:
                time.sleep(wait)


def say_in_game(api, token):
    def say(text):
        # The admin API: POST /api/chat {"text": ...} says it in the game's chat as the server.
        call("POST", api + "/api/chat", {"Authorization": "Bearer " + token}, {"text": text}, timeout=10)

    return say


# ---- tests -------------------------------------------------------------------------------------------


class Fake(BaseHTTPRequestHandler):
    """A Discord and a server on one port: /discord/... and /api/chat. The test sets `state` on the class."""

    state = None

    def log_message(self, *a):
        pass

    def reply(self, status, body, headers=()):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        for k, v in headers:
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        s = Fake.state
        s["gets"].append((self.path, self.headers.get("Authorization"), self.headers.get("User-Agent")))
        if s["discord_status"]:
            return self.reply(s["discord_status"], {"message": "no"}, [("Retry-After", "7")] if s["discord_status"] == 429 else [])
        if self.path == "/discord/users/@me":
            return self.reply(200, {"id": "555", "username": "bridge-bot", "bot": True})
        query = dict(p.split("=") for p in self.path.split("?")[1].split("&"))
        after = int(query.get("after", 0))
        if "after" not in query:
            return self.reply(200, sorted(s["messages"], key=lambda m: -int(m["id"]))[: int(query["limit"])])
        found = [m for m in sorted(s["messages"], key=lambda m: int(m["id"])) if int(m["id"]) > after][: int(query["limit"])]
        # Discord answers newest first.
        self.reply(200, found[::-1])

    def do_POST(self):
        s = Fake.state
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        s["posts"].append((self.path, self.headers.get("Authorization"), body))
        if s["chat_status"]:
            return self.reply(s["chat_status"], {"error": "no"})
        self.reply(200, {"ok": True})


def human(id, name, content, **more):
    return {"id": str(id), "type": 0, "content": content, "author": {"id": "u" + name, "username": name.lower(), "global_name": name, "bot": False}, **more}


class Tests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = None
        for port in range(8580, 8600):
            try:
                cls.server = HTTPServer(("127.0.0.1", port), Fake)
                break
            except OSError:
                continue
        assert cls.server, "no free port in 8580-8599"
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()
        cls.base = "http://127.0.0.1:%d" % cls.server.server_port

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()

    def setUp(self):
        Fake.state = {"gets": [], "posts": [], "messages": [human(100, "Old", "history")], "discord_status": 0, "chat_status": 0}
        self.now = [0.0]
        self.bridge = Bridge("42", "discord-secret", say_in_game(self.base, "pcdst_secret"), self.base + "/discord", clock=lambda: self.now[0])

    def said(self):
        return [p[2]["text"] for p in Fake.state["posts"]]

    def test_clean(self):
        self.assertEqual(clean("hi <@1> and <@2> <@&9> <#7> <:wave:123> <a:dance:5> \x07bell‮​\nnew\tline"), "hi @user and @user @role #channel :wave: :dance: bell new line")
        self.assertEqual(clean("<@1> hi", {"1": "Ann"}), "@Ann hi")
        self.assertEqual(clean("<b>big</b> <size=99> <https://example.org/a>"), "‹b›big‹/b› ‹size=99› https://example.org/a")
        self.assertEqual(clean("x" * 500), "x" * 199 + "…")
        self.assertEqual(clean("   "), "")
        self.assertEqual(author_name({"username": "bob"}), "bob")
        self.assertEqual(author_name({"global_name": "<Ann>\n" + "n" * 50}), "‹Ann› " + "n" * 18)
        self.assertEqual(author_name({}), "?")

    def test_limiter(self):
        now = [0.0]
        limiter = Limiter(5, 10, lambda: now[0])
        self.assertEqual([limiter.allow() for _ in range(7)], [True] * 5 + [False] * 2)
        now[0] = 9.9
        self.assertFalse(limiter.allow())
        now[0] = 10.0
        self.assertTrue(limiter.allow())

    def test_starts_after_the_history_and_passes_on_people_only(self):
        self.assertFalse(self.bridge.poll())
        self.assertEqual((self.bridge.last, self.said()), ("100", []), "the history is not brought in")
        Fake.state["messages"] += [
            human(101, "Ann", "hello <@7> `code` @everyone \x07", mentions=[{"id": "7", "username": "bo", "global_name": "Bo"}]),
            dict(human(102, "Robot", "i am a bot"), author={"id": "b", "username": "robot", "bot": True}),
            dict(human(103, "Hook", "from the webhook"), webhook_id="9"),
            dict(human(104, "Me", "my own"), author={"id": "555", "username": "bridge-bot"}),
            dict(human(105, "Cy", "pinned something"), type=6),
            human(106, "Di", "", attachments=[{"id": "1"}]),
            human(107, "Ed", "  ​ "),
            human(108, "Fay", "last"),
        ]
        self.bridge.poll()
        self.assertEqual(self.said(), ["[Discord] Ann: hello @Bo `code` @everyone", "[Discord] Di: (sent a file)", "[Discord] Fay: last"])
        self.assertEqual(self.bridge.last, "108")
        # What the requests carried: the bot token to Discord with an agent, the scoped token to the server, `text` in the body.
        gets, posts = Fake.state["gets"], Fake.state["posts"]
        self.assertTrue(all(g[1] == "Bot discord-secret" and "DiscordBot" in g[2] for g in gets), gets)
        self.assertTrue(any(g[0] == "/discord/channels/42/messages?limit=1" for g in gets) and any(g[0] == "/discord/channels/42/messages?after=100&limit=20" for g in gets), gets)
        self.assertTrue(all(p[0] == "/api/chat" and p[1] == "Bearer pcdst_secret" and list(p[2]) == ["text"] for p in posts), posts)
        self.bridge.poll()
        self.assertEqual(len(self.said()), 3, "nothing is said twice")

    def test_five_in_ten_seconds_and_a_notice_for_the_rest(self):
        self.bridge.poll()
        Fake.state["messages"] += [human(101 + i, "Ann", "m%d" % i) for i in range(8)]
        self.bridge.poll()
        self.assertEqual(self.said(), ["[Discord] Ann: m%d" % i for i in range(5)])
        self.assertEqual((self.bridge.skipped, self.bridge.last), (3, "108"))
        self.bridge.poll()
        self.assertEqual(len(self.said()), 5, "the window is still full: no notice yet")
        self.now[0] = 10.0
        self.bridge.poll()
        self.assertEqual(self.said()[5:], ["[Discord] (3 messages skipped)"])
        self.assertEqual(self.bridge.skipped, 0)

    def test_a_server_that_does_not_answer_loses_nothing(self):
        self.bridge.poll()
        Fake.state["messages"] += [human(101, "Ann", "one"), human(102, "Ann", "two")]
        Fake.state["chat_status"] = 503
        with self.assertRaises(HttpError):
            self.bridge.poll()
        self.assertEqual(self.bridge.last, "100", "not passed on: read again")
        Fake.state["chat_status"] = 0
        self.bridge.poll()
        self.assertEqual(self.said()[-2:], ["[Discord] Ann: one", "[Discord] Ann: two"])
        # A 400 for one message is that message's problem: it is let go and the next one goes.
        Fake.state["messages"] += [human(103, "Ann", "bad"), human(104, "Ann", "good")]
        Fake.state["chat_status"] = 400
        self.bridge.poll()
        self.assertEqual(self.bridge.last, "104")
        # A refused token is not: the same message waits.
        self.now[0] = 100.0
        Fake.state["messages"] += [human(105, "Ann", "again")]
        Fake.state["chat_status"] = 401
        with self.assertRaises(HttpError):
            self.bridge.poll()
        self.assertEqual(self.bridge.last, "104")

    def test_discord_errors_carry_the_wait_and_nothing_secret(self):
        Fake.state["discord_status"] = 429
        with self.assertRaises(HttpError) as raised:
            self.bridge.poll()
        self.assertEqual((raised.exception.status, raised.exception.retry_after, str(raised.exception)), (429, 7.0, "HTTP 429"))
        # run() prints the kind and never a token or a URL, and waits at least as long as asked.
        errors, slept = io.StringIO(), []
        real = time.sleep

        def stop(seconds):
            slept.append(seconds)
            if len(slept) == 2:
                raise KeyboardInterrupt

        time.sleep = stop
        try:
            with contextlib.redirect_stderr(errors), self.assertRaises(KeyboardInterrupt):
                self.bridge.run(3)
        finally:
            time.sleep = real
        self.assertEqual(slept, [7.0, 12.0])
        for secret in ("discord-secret", "pcdst_secret", self.base, "/channels/"):
            self.assertNotIn(secret, errors.getvalue())
        self.assertIn("HTTP 429", errors.getvalue())
        # A connection that fails shows the exception's name only.
        down = Bridge("42", "discord-secret", print, "http://127.0.0.1:1/discord")
        errors, slept = io.StringIO(), []
        time.sleep = stop
        try:
            with contextlib.redirect_stderr(errors), self.assertRaises(KeyboardInterrupt):
                down.run(3)
        finally:
            time.sleep = real
        self.assertIn("URLError (try 1)", errors.getvalue())
        self.assertNotIn("127.0.0.1", errors.getvalue())

    def test_dry_run_prints_and_the_bridge_reads_the_next_page_at_once(self):
        lines = []
        bridge = Bridge("42", "t", lines.append, self.base + "/discord", clock=lambda: self.now[0])
        bridge.limiter = Limiter(100, 10, lambda: 0.0)
        bridge.poll()
        Fake.state["messages"] += [human(101 + i, "Ann", "m%d" % i) for i in range(25)]
        self.assertTrue(bridge.poll(), "a full page: more may wait")
        self.assertFalse(bridge.poll())
        self.assertEqual((len(lines), lines[0], lines[-1], Fake.state["posts"]), (25, "[Discord] Ann: m0", "[Discord] Ann: m24", []))


def self_test():
    stream = io.StringIO()
    result = unittest.TextTestRunner(stream=stream, verbosity=0).run(unittest.defaultTestLoader.loadTestsFromTestCase(Tests))
    if not result.wasSuccessful():
        sys.exit(stream.getvalue())
    print("self-test ok (%d tests)" % result.testsRun)


def main(argv):
    if "--self-test" in argv:
        return self_test()
    dry = "--dry-run" in argv
    env = os.environ
    missing = [k for k in ("DISCORD_TOKEN", "DISCORD_CHANNEL") + (() if dry else ("PCDS_TOKEN",)) if not env.get(k)]
    if missing:
        sys.exit("set %s (PCDS_TOKEN is not needed with --dry-run); see the top of this file" % ", ".join(missing))
    if not env["DISCORD_CHANNEL"].isdigit():
        sys.exit("DISCORD_CHANNEL is the channel's numeric ID")
    say = print if dry else say_in_game(env.get("PCDS_API", "http://127.0.0.1:7782").rstrip("/"), env["PCDS_TOKEN"])
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    Bridge(env["DISCORD_CHANNEL"], env["DISCORD_TOKEN"], say).run(max(1.0, float(env.get("BRIDGE_POLL", "3"))))


if __name__ == "__main__":
    main(sys.argv[1:])
