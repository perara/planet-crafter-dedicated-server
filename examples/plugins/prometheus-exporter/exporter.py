#!/usr/bin/env python3
"""Serves a Planet Crafter dedicated server's numbers to Prometheus (text format, GET /metrics).

An out-of-process plugin: an ordinary program that reads the server's admin API and the plugin event
stream. Python 3's standard library only; it can run on another machine (reach the admin API through
an SSH tunnel), and a crash of it never touches the game.

  PCDS_TOKEN=pcdst_... ./exporter.py                  # http://127.0.0.1:9188/metrics

  PCDS_API       the admin API's address                              (default http://127.0.0.1:7782; --api)
  PCDS_TOKEN     the token. `events.read` (pcds plugin token create metrics --scope events.read) reaches
                 the event stream only. GET /api/status and GET /api/plugins take the ADMIN token today
                 (no scope opens them): with a scoped token use --no-admin. Read from the environment
                 only, never from the command line.

  --listen HOST:PORT   where /metrics is served                       (default 127.0.0.1:9188)
  --no-admin           do not ask /api/status and /api/plugins: only the stream's counters
  --consumer NAME      the name the stream lists this program under   (default prometheus)
  --wait SECONDS       long poll of the stream, at most 25            (default 25; 0: poll every 2 s)
  --replay FILE        no network: render {"events": [...], "status": {...}, "plugins": {...}} and print it
  --self-test          run against fake servers and check the text

What is exported (all names start with pcds_; the label values are few: no player is ever a label):

  from the event stream, counted from the moment the exporter started (a restart of the exporter starts
  the counters at 0, which Prometheus handles as a reset):
    pcds_chat_messages_total, pcds_player_joins_total, pcds_player_leaves_total, pcds_objects_placed_total,
    pcds_objects_destroyed_total, pcds_world_saves_total{result="ok|failed"}, pcds_rule_changes_total,
    pcds_stream_events_dropped_total (the ring is 512 events: more than that between two polls is lost),
    pcds_stream_resets_total (the server restarted: its event numbers began again)
  from GET /api/status: pcds_players_online, pcds_game_time_seconds, pcds_terraformation, the tick and loop times,
    the world's objects and units, the last save and backup, the process, the traffic, the plugin counts
  from GET /api/plugins, for each plugin (label plugin): calls, faults, restarts, slow calls, shed, denied,
    refused, actions, vetoes, fuel, the http counters, and pcds_plugin_status{plugin,status} as 0/1
  about itself: pcds_up (the server answered), pcds_exporter_source_up{source}, pcds_exporter_errors_total{source}

Not printed anywhere: the token and the URLs. Errors are shown by kind and HTTP status only.
"""
import argparse
import contextlib
import io
import json
import math
import os
import re
import signal
import sys
import threading
import time
import unittest
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# The kinds counted. Not `tick`: it is not streamed.
TYPES = ["chat", "player_join", "player_leave", "object_placed", "object_destroyed", "world_saved", "rule_change"]
# Every status a plugin can have (docs/plugins-api.md); one 0/1 sample for each.
STATUSES = ["running", "pending", "disabled", "changed", "unloading", "invalid", "incompatible", "failed"]
CONTENT_TYPE = "text/plain; version=0.0.4; charset=utf-8"


# ---- reading numbers -----------------------------------------------------------------------------------


def num(x):
    """A finite number, or None. Booleans are not numbers here."""
    if isinstance(x, bool) or not isinstance(x, (int, float)) or (isinstance(x, float) and not math.isfinite(x)):
        return None
    return x


def flag(x):
    return int(x) if isinstance(x, bool) else None


def dig(obj, *path):
    for key in path:
        obj = obj.get(key) if isinstance(obj, dict) else None
    return obj


def seconds(micros):
    n = num(micros)
    return None if n is None else n / 1e6


# ---- the text format -----------------------------------------------------------------------------------


def escape_label(value):
    return str(value).replace("\\", "\\\\").replace("\n", "\\n").replace('"', '\\"')


def escape_help(text):
    return text.replace("\\", "\\\\").replace("\n", "\\n")


def fmt(value):
    if isinstance(value, bool):
        return "1" if value else "0"
    if isinstance(value, int):
        return str(value)
    return repr(float(value))


class Out:
    """The samples, grouped by metric name so that each has one HELP and one TYPE line."""

    def __init__(self):
        self.families = {}

    def add(self, name, mtype, help, value, **labels):
        if value is None:
            return
        assert (mtype == "counter") == name.endswith("_total"), name
        family = self.families.setdefault(name, [mtype, help, []])
        family[2].append((labels, value))

    def text(self):
        lines = []
        for name, (mtype, help, samples) in self.families.items():
            lines.append("# HELP %s %s" % (name, escape_help(help)))
            lines.append("# TYPE %s %s" % (name, mtype))
            for labels, value in samples:
                shown = ",".join('%s="%s"' % (k, escape_label(v)) for k, v in labels.items())
                lines.append("%s%s %s" % (name, "{%s}" % shown if shown else "", fmt(value)))
        return "\n".join(lines) + "\n"


class Counts:
    """What the stream said, since this exporter started."""

    def __init__(self):
        self.lock = threading.Lock()
        self.n = dict.fromkeys(["chat", "join", "leave", "placed", "destroyed", "saved_ok", "saved_failed", "rules", "dropped", "resets"], 0)

    def add(self, event):
        key = {"chat": "chat", "player_join": "join", "player_leave": "leave", "object_placed": "placed", "object_destroyed": "destroyed", "rule_change": "rules"}.get(event.get("type"))
        if event.get("type") == "world_saved":
            key = "saved_ok" if event.get("ok") is True else "saved_failed"
        if key:
            with self.lock:
                self.n[key] += 1

    def snapshot(self):
        with self.lock:
            return dict(self.n)


def render(counts, status=None, plugins=None, up=True, sources=None, errors=None):
    """The whole /metrics text. `status` and `plugins` are the admin API's answers (None: not known now)."""
    out = Out()
    g, c = "gauge", "counter"
    out.add("pcds_up", g, "1 if the server's admin API answered, else 0", int(up))
    for source, ok in (sources or {}).items():
        out.add("pcds_exporter_source_up", g, "1 if the last request for this source was answered with success", int(ok), source=source)
    for source, n in (errors or {}).items():
        out.add("pcds_exporter_errors_total", c, "requests for this source that failed", n, source=source)
    n = counts
    out.add("pcds_chat_messages_total", c, "chat messages seen on the event stream since the exporter started", n["chat"])
    out.add("pcds_player_joins_total", c, "player joins seen on the event stream", n["join"])
    out.add("pcds_player_leaves_total", c, "player leaves seen on the event stream", n["leave"])
    out.add("pcds_objects_placed_total", c, "objects placed seen on the event stream", n["placed"])
    out.add("pcds_objects_destroyed_total", c, "objects destroyed seen on the event stream", n["destroyed"])
    out.add("pcds_world_saves_total", c, "world saves seen on the event stream", n["saved_ok"], result="ok")
    out.add("pcds_world_saves_total", c, "world saves seen on the event stream", n["saved_failed"], result="failed")
    out.add("pcds_rule_changes_total", c, "rule changes seen on the event stream", n["rules"])
    out.add("pcds_stream_events_dropped_total", c, "events the stream's ring lost before they were read", n["dropped"])
    out.add("pcds_stream_resets_total", c, "server restarts seen (the stream's epoch changed)", n["resets"])
    if isinstance(status, dict):
        add_status(out, status)
    if isinstance(plugins, dict):
        add_plugins(out, plugins)
    return out.text()


def add_status(out, s):
    g, c = "gauge", "counter"
    out.add("pcds_build_info", g, "the server and game versions, the planet and whether the world runs, always 1", 1, server_version=s.get("serverVersion") or "", game_version=s.get("gameVersion") or "", planet=s.get("planet") or "", state=s.get("state") or "")
    out.add("pcds_world_running", g, "1 if the world is loaded and running, 0 if it is stopped", None if s.get("state") is None else int(s.get("state") == "running"))
    out.add("pcds_world_held", g, "1 if the world is held (paused by the admin)", flag(s.get("held")))
    out.add("pcds_uptime_seconds", g, "seconds since the server process started", num(s.get("uptimeSeconds")))
    out.add("pcds_players_online", g, "players in the game", len(s["players"]) if isinstance(s.get("players"), list) else None)
    out.add("pcds_game_time_seconds", g, "the world's game time", num(s.get("gameTime")))
    out.add("pcds_terraformation", g, "the world's terraformation index", num(s.get("terraformation")))
    for kind, value in (s["objects"].items() if isinstance(s.get("objects"), dict) else []):
        out.add("pcds_world_objects", g, "objects in the world by kind (world, network, inventories)", num(value), kind=kind)
    for unit, v in (s["units"].items() if isinstance(s.get("units"), dict) else []):
        out.add("pcds_world_unit_value", g, "the planet's terraformation units: current value", num(dig(v, "value")), unit=unit)
        out.add("pcds_world_unit_per_second", g, "the planet's terraformation units: change per second", num(dig(v, "perSecond")), unit=unit)
    out.add("pcds_tick_rate", g, "the network tick rate the game asks for, per second", num(s.get("tickRate")))
    out.add("pcds_loop_rate", g, "game loops in the last second", num(s.get("fps")))
    busy = num(s.get("busyPercent"))
    out.add("pcds_loop_busy_ratio", g, "share of the time the game loop works (0 to 1)", None if busy is None else busy / 100)
    out.add("pcds_loop_longest_seconds", g, "the longest game loop lately", seconds(s.get("longestLoopMicros")))
    for q, key in (("0.5", "p50Micros"), ("0.99", "p99Micros"), ("1", "maxMicros")):
        out.add("pcds_tick_duration_seconds", g, "duration of the game's ticks over the last minute, by quantile (1 is the longest)", seconds(dig(s, "tick", key)), quantile=q)
    out.add("pcds_tick_samples", g, "ticks the quantiles above are made of", num(dig(s, "tick", "samples")))
    out.add("pcds_last_save_success", g, "1 if the last world save worked", flag(dig(s, "lastSave", "ok")))
    out.add("pcds_last_save_timestamp_seconds", g, "when the last world save was made, Unix seconds", num(dig(s, "lastSave", "at")))
    out.add("pcds_backup_enabled", g, "1 if backups are kept", flag(dig(s, "backup", "enabled")))
    out.add("pcds_backup_last_success_timestamp_seconds", g, "when the last backup was made since the server started, Unix seconds", num(dig(s, "backup", "lastOkAt")))
    if isinstance(s.get("backup"), dict):
        out.add("pcds_backup_failing", g, "1 if the last backup failed", int(s["backup"].get("error") is not None))
    out.add("pcds_s3_enabled", g, "1 if backups are uploaded to S3", flag(dig(s, "s3", "enabled")))
    out.add("pcds_s3_uploading", g, "1 while an S3 upload runs", flag(dig(s, "s3", "uploading")))
    out.add("pcds_s3_last_upload_success", g, "1 if the last S3 upload worked", flag(dig(s, "s3", "last", "ok")))
    out.add("pcds_s3_last_upload_timestamp_seconds", g, "when the last S3 upload ended, Unix seconds", num(dig(s, "s3", "last", "at")))
    out.add("pcds_process_cpu_seconds_total", c, "CPU seconds used by the server's processes", num(dig(s, "process", "cpuSeconds")))
    out.add("pcds_process_memory_bytes", g, "memory held by the server's processes", num(dig(s, "process", "memoryBytes")))
    out.add("pcds_network_received_bytes_total", c, "bytes received from players", num(dig(s, "traffic", "bytesIn")))
    out.add("pcds_network_sent_bytes_total", c, "bytes sent to players", num(dig(s, "traffic", "bytesOut")))
    out.add("pcds_bank_flips_total", c, "hot updates of the game program since the front started", num(dig(s, "banks", "flips")))
    out.add("pcds_bank_crashes_recent", g, "crashes of the game program in the front's recent window", num(dig(s, "banks", "crashes")))
    out.add("pcds_plugins_loaded", g, "plugins loaded (from /api/status)", num(dig(s, "plugins", "loaded")))
    out.add("pcds_plugins_running", g, "plugins running (from /api/status)", num(dig(s, "plugins", "running")))


# (name, kind, help, path in a plugin's JSON, scale)
PLUGIN_COUNTERS = [
    ("calls", "calls of the plugin", ("stats", "calls")),
    ("faults", "calls that faulted (a trap, a limit, an error)", ("stats", "faults")),
    ("restarts", "times the plugin was started again after a fault", ("stats", "restarts")),
    ("slow_calls", "calls over the slow-call limit", ("stats", "slow")),
    ("shed", "calls skipped because the tick budget was spent", ("stats", "shed")),
    ("denied", "host calls refused for a capability not granted", ("stats", "denied")),
    ("refused", "actions the host refused when applying them", ("stats", "refused")),
    ("actions", "actions the plugin asked for", ("stats", "actions")),
    ("vetoes", "RPCs the plugin refused", ("stats", "vetoes")),
    ("fuel", "fuel the plugin spent", ("stats", "fuel")),
    ("http_requests", "HTTP requests the plugin started", ("stats", "http", "requests")),
    ("http_answered", "HTTP requests that got an answer", ("stats", "http", "answered")),
    ("http_failed", "HTTP requests that failed", ("stats", "http", "failed")),
    ("http_sent_bytes", "HTTP request bytes sent", ("stats", "http", "bytesOut")),
    ("http_received_bytes", "HTTP answer bytes received", ("stats", "http", "bytesIn")),
]


def add_plugins(out, body):
    g = "gauge"
    out.add("pcds_stream_consumers", g, "programs registered on the event stream", len(dig(body, "stream", "consumers")) if isinstance(dig(body, "stream", "consumers"), list) else None)
    out.add("pcds_plugin_instances", g, "running WebAssembly instances", num(dig(body, "runtime", "instances")))
    out.add("pcds_plugin_memory_pages", g, "64 KiB pages the running plugins hold", num(dig(body, "runtime", "memoryPages")))
    plugins = [p for p in body.get("plugins") or [] if isinstance(p, dict) and p.get("id")]
    for p in plugins:
        who = {"plugin": p["id"]}
        out.add("pcds_plugin_info", g, "a plugin's version, always 1", 1, plugin=p["id"], version=p.get("version") or "")
        status = p.get("status")
        for name in STATUSES + ([status] if status not in STATUSES and status else []):
            out.add("pcds_plugin_status", g, "1 for the status the plugin is in, 0 for the others", int(name == status), plugin=p["id"], status=name)
    for name, help, path in PLUGIN_COUNTERS:
        for p in plugins:
            out.add("pcds_plugin_%s_total" % name, "counter", help + " since it started", num(dig(p, *path)), plugin=p["id"])
    for p in plugins:
        out.add("pcds_plugin_http_in_flight", g, "HTTP requests alive now", num(dig(p, "stats", "http", "inFlight")), plugin=p["id"])
    for p in plugins:
        out.add("pcds_plugin_call_average_seconds", g, "average time of a call", seconds(dig(p, "stats", "averageMicros")), plugin=p["id"])
    for p in plugins:
        out.add("pcds_plugin_call_max_seconds", g, "longest call", seconds(dig(p, "stats", "maxMicros")), plugin=p["id"])
    for p in plugins:
        out.add("pcds_plugin_last_fault_timestamp_seconds", g, "when the plugin last faulted, Unix seconds", num(dig(p, "stats", "lastFaultAt")), plugin=p["id"])


# ---- talking to the server ----------------------------------------------------------------------------


class HttpError(Exception):
    """An answer that is not 2xx. The status only: never the URL."""

    def __init__(self, status):
        super().__init__("HTTP %d" % status)
        self.status = status


def call(method, url, token, body=None, timeout=5):
    data = json.dumps(body).encode() if body is not None else None
    request = urllib.request.Request(url, data, {"Content-Type": "application/json", "Authorization": "Bearer " + token}, method=method)
    try:
        with urllib.request.urlopen(request, timeout=timeout) as answer:
            return json.loads(answer.read(8 << 20))
    except urllib.error.HTTPError as e:
        raise HttpError(e.code) from None


class Exporter:
    def __init__(self, api, token, admin=True, consumer="prometheus", wait=25, pause=2.0):
        self.api, self.token, self.admin, self.consumer, self.wait, self.pause = api.rstrip("/"), token, admin, consumer, wait, pause
        self.counts = Counts()
        self.after = None  # None: no poll yet, so the first one only finds where the stream is now
        self.epoch = None
        self.dropped_seen = self.dropped_base = 0
        self.stream_state = None  # (reached, ok) of the stream's last request
        self.errors = {"stream": 0, "status": 0, "plugins": 0}
        self.fetch_lock = threading.Lock()
        self.cache = (0.0, None)
        self.stop = threading.Event()

    # The stream.

    def poll(self):
        """One request to the stream; the number of events counted."""
        body = {"consumer": self.consumer, "types": TYPES, "limit": 500, "after": self.after or 0}
        if self.after is None:
            # Only the newest number is wanted: what happened before the exporter began is not counted.
            body["limit"] = 1
        elif self.wait:
            body["wait"] = self.wait
        answer = call("POST", self.api + "/api/plugins/events", self.token, body, timeout=self.wait + 15)
        if self.epoch not in (None, answer["epoch"]):
            # The server restarted: its numbers began again, and everything in the ring is new.
            self.epoch, self.after, self.dropped_base, self.dropped_seen = answer["epoch"], 0, self.dropped_base + self.dropped_seen, 0
            with self.counts.lock:
                self.counts.n["resets"] += 1
            return self.poll()
        self.epoch = answer["epoch"]
        dropped = num(answer.get("dropped")) or 0
        if self.after is None:
            self.after, self.dropped_base = answer["last"], -dropped
            return 0
        self.dropped_seen = dropped
        with self.counts.lock:
            self.counts.n["dropped"] = max(0, self.dropped_base + dropped)
        for event in answer["events"]:
            self.counts.add(event)
            self.after = event["id"]
        if not answer["events"]:
            # Nothing of our kinds: skip what was filtered, so the next poll starts after the newest.
            self.after = max(self.after, answer["last"])
        return len(answer["events"])

    def stream_loop(self):
        failures = 0
        while not self.stop.is_set():
            started = time.monotonic()
            try:
                counted = self.poll()
                failures = 0
                self.stream_state = (True, True)
                # An answer at once with nothing in it (no long poll, or a wait of 0): do not spin.
                wait = self.pause if not counted and time.monotonic() - started < 0.5 else 0
            except HttpError as e:
                failures, wait, self.stream_state = failures + 1, min(self.pause * 2 ** min(failures + 1, 5), 60), (True, False)
                self.errors["stream"] += 1
                print("exporter: stream %s (try %d)" % (e, failures), file=sys.stderr)
            except (urllib.error.URLError, OSError, ValueError, KeyError) as e:
                failures, wait, self.stream_state = failures + 1, min(self.pause * 2 ** min(failures + 1, 5), 60), (isinstance(e, (ValueError, KeyError)), False)
                self.errors["stream"] += 1
                # Never the URL or the token: only what went wrong.
                print("exporter: stream %s (try %d)" % (type(e).__name__, failures), file=sys.stderr)
            self.stop.wait(wait)

    def leave(self):
        """Deregisters: the server stops recording what only this program wanted."""
        with contextlib.suppress(HttpError, urllib.error.URLError, OSError, ValueError):
            call("POST", self.api + "/api/plugins/events", self.token, {"consumer": self.consumer, "unregister": True}, timeout=3)

    # The scrape.

    def get(self, path):
        """(answer or None, reached the server, success)."""
        source = path.rsplit("/", 1)[1]
        try:
            return call("GET", self.api + path, self.token, timeout=3), True, True
        except HttpError:
            self.errors[source] += 1
            return None, True, False
        except ValueError:
            self.errors[source] += 1
            return None, True, False
        except (urllib.error.URLError, OSError):
            self.errors[source] += 1
            return None, False, False

    def sample(self):
        """The admin API's two answers, asked at most once a second however many scrapers there are."""
        with self.fetch_lock:
            if time.monotonic() - self.cache[0] < 1.0 and self.cache[1] is not None:
                return self.cache[1]
            status, r1, ok1 = self.get("/api/status")
            plugins, r2, ok2 = self.get("/api/plugins")
            result = (status, plugins, {"status": ok1, "plugins": ok2}, r1 or r2)
            self.cache = (time.monotonic(), result)
            return result

    def metrics(self):
        sources = {}
        status = plugins = None
        if self.admin:
            status, plugins, ok, reached = self.sample()
            sources.update(ok)
        else:
            reached = bool(self.stream_state and self.stream_state[0])
        if self.stream_state is not None:
            sources["stream"] = self.stream_state[1]
        return render(self.counts.snapshot(), status, plugins, reached, sources, {k: v for k, v in self.errors.items() if self.admin or k == "stream"})


def replay(bundle):
    counts = Counts()
    for event in bundle.get("events") or []:
        if isinstance(event, dict):
            counts.add(event)
    return render(counts.snapshot(), bundle.get("status"), bundle.get("plugins"), True)


def serve(exporter, host, port):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def do_GET(self):
            if self.path.split("?")[0] == "/metrics":
                try:
                    body, status, kind = exporter.metrics().encode(), 200, CONTENT_TYPE
                except Exception as e:  # a bug here must not print a traceback with the token's neighbours
                    print("exporter: metrics failed: %s" % type(e).__name__, file=sys.stderr)
                    body, status, kind = b"exporter error\n", 500, "text/plain; charset=utf-8"
            elif self.path == "/":
                body, status, kind = b"pcds exporter: see /metrics\n", 200, "text/plain; charset=utf-8"
            else:
                body, status, kind = b"not found\n", 404, "text/plain; charset=utf-8"
            self.send_response(status)
            self.send_header("Content-Type", kind)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = ThreadingHTTPServer((host, port), Handler)
    server.daemon_threads = True
    return server


# ---- tests ---------------------------------------------------------------------------------------------

STATUS = {
    "state": "running", "gameVersion": "1.2.3", "serverVersion": "0.8.0", "planet": "Prime", "held": False, "uptimeSeconds": 4000,
    "gameTime": 3725.5, "terraformation": 12345.5, "tickRate": 60, "fps": 200, "busyPercent": 12.5, "longestLoopMicros": 2500,
    "players": [{"id": 1, "name": "Ann"}, {"id": 2, "name": "Bo \"B\""}],
    "objects": {"world": 900, "network": 1200, "inventories": 40},
    "units": {"Heat": {"value": 1.5, "perSecond": 0.25}, "Oxygen": {"value": 2, "perSecond": 0}},
    "tick": {"windowSeconds": 60, "samples": 1000, "p50Micros": 500, "p99Micros": 990, "maxMicros": None},
    "lastSave": {"ok": False, "at": 1760000000, "error": "disk full"},
    "backup": {"enabled": True, "lastOkAt": 1759999000, "error": {"at": 1, "message": "x"}},
    "s3": {"enabled": False},
    "process": {"cpuSeconds": 123.5, "memoryBytes": 1 << 30}, "traffic": {"bytesIn": 10, "bytesOut": 20},
    "banks": {"flips": 2, "crashes": 0}, "plugins": {"loaded": 2, "running": 1, "faults": 3},
}
PLUGINS = {
    "plugins": [
        {"id": "chat-bridge", "version": "0.1.0", "status": "running", "stats": {"calls": 10, "faults": 1, "restarts": 0, "slow": 2, "shed": 0, "denied": 0, "refused": 0, "actions": 4, "vetoes": 0, "fuel": 5000, "averageMicros": 12.5, "maxMicros": 80.0, "lastFaultAt": 1760000100, "http": {"requests": 7, "answered": 6, "failed": 1, "bytesOut": 700, "bytesIn": 60, "inFlight": 1}}},
        {"id": "we\"ird\\id", "version": "1.0.0", "status": "failed", "stats": {"calls": 0, "faults": 5, "lastFaultAt": None}},
    ],
    "runtime": {"instances": 1, "memoryPages": 8}, "stream": {"epoch": 5, "consumers": [{"name": "prometheus"}]},
}
EVENTS = [
    {"id": 1, "type": "chat", "time": 1, "client": 1, "name": "Ann", "text": "hi", "playerId": "7"},
    {"id": 2, "type": "player_join", "time": 1, "client": 1, "name": "Ann", "playerId": "7", "steamId": None},
    {"id": 3, "type": "player_leave", "time": 1, "client": 1, "name": "Ann", "playerId": "7"},
    {"id": 4, "type": "object_placed", "time": 1, "object": 5, "group": "Drill1", "position": [1, 2, 3], "client": 1},
    {"id": 5, "type": "object_destroyed", "time": 1, "object": 5, "group": "Drill1", "position": [1, 2, 3], "client": None},
    {"id": 6, "type": "world_saved", "time": 1, "gameSeconds": 5.0, "ok": True},
    {"id": 7, "type": "world_saved", "time": 1, "gameSeconds": 6.0, "ok": False},
    {"id": 8, "type": "rule_change", "time": 1, "changes": {"speed.growth": 2}},
]


def lint(text):
    """Every sample has a HELP and a TYPE, counters end in _total and nothing else does, names and labels are legal."""
    kinds, helps, seen = {}, set(), set()
    for line in text.splitlines():
        if line.startswith("# TYPE "):
            _, _, name, kind = line.split(" ")
            assert name not in kinds, "TYPE twice: " + name
            assert kind in ("counter", "gauge"), line
            assert (kind == "counter") == name.endswith("_total"), line
            kinds[name] = kind
        elif line.startswith("# HELP "):
            helps.add(line.split(" ")[2])
        else:
            m = re.fullmatch(r'(pcds_[a-z0-9_]+)(\{(?:[a-z_]+="(?:[^"\\\n]|\\.)*"(?:,|(?=\})))*\})? (-?[0-9.e+-]+)', line)
            assert m, "not a sample: " + line
            assert m.group(1) in kinds and m.group(1) in helps, "no TYPE or HELP: " + line
            assert line not in seen, "twice: " + line
            seen.add(line)
    assert text.endswith("\n")
    return seen


class Fake(BaseHTTPRequestHandler):
    """A server on one port. The test sets `state` on the class: what it answers and what it was asked."""

    state = None

    def log_message(self, *a):
        pass

    def reply(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def handle_any(self):
        s = Fake.state
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"null")
        s["asked"].append((self.command, self.path, self.headers.get("Authorization"), body))
        if self.path == "/api/plugins/events":
            if s["stream_status"]:
                return self.reply(s["stream_status"], {"error": "no"})
            if body.get("unregister"):
                return self.reply(200, {"ok": True})
            events = [e for e in s["events"] if e["id"] > body["after"]][: body["limit"]]
            return self.reply(200, {"epoch": s["epoch"], "events": events, "last": max([e["id"] for e in s["events"]], default=0), "dropped": s["dropped"]})
        if self.path in ("/api/status", "/api/plugins") and s["admin_status"]:
            return self.reply(s["admin_status"], {"error": "no scope"})
        self.reply(200, {"/api/status": STATUS, "/api/plugins": PLUGINS}.get(self.path, {}))

    do_GET = do_POST = handle_any


def free_server(handler):
    for port in range(8580, 8600):
        try:
            return ThreadingHTTPServer(("127.0.0.1", port), handler)
        except OSError:
            continue
    raise AssertionError("no free port in 8580-8599")


class Tests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = free_server(Fake)
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()
        cls.base = "http://127.0.0.1:%d" % cls.server.server_port

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()

    def setUp(self):
        Fake.state = {"asked": [], "events": [dict(e) for e in EVENTS[:3]], "epoch": 100, "dropped": 4, "stream_status": 0, "admin_status": 0}
        self.exp = Exporter(self.base, "pcdst_secret", wait=0)

    def value(self, text, line_start):
        (line,) = [l for l in text.splitlines() if l.startswith(line_start + " ") or l.startswith(line_start + "{")] or [None]
        return line.rsplit(" ", 1)[1] if line else None

    def test_replay_renders_everything_and_the_text_is_legal(self):
        text = replay({"events": EVENTS, "status": STATUS, "plugins": PLUGINS})
        lint(text)
        v = lambda s: self.value(text, s)
        self.assertEqual([v("pcds_up"), v("pcds_chat_messages_total"), v("pcds_player_joins_total"), v("pcds_objects_destroyed_total"), v("pcds_rule_changes_total")], ["1"] * 5)
        self.assertIn('pcds_world_saves_total{result="ok"} 1\n', text)
        self.assertIn('pcds_world_saves_total{result="failed"} 1\n', text)
        self.assertEqual([v("pcds_players_online"), v("pcds_terraformation"), v("pcds_loop_busy_ratio"), v("pcds_process_cpu_seconds_total"), v("pcds_last_save_success"), v("pcds_backup_failing")], ["2", "12345.5", "0.125", "123.5", "0", "1"])
        self.assertIn('pcds_tick_duration_seconds{quantile="0.99"} 0.00099\n', text)
        self.assertNotIn('quantile="1"', text, "a null is not exported")
        self.assertIn('pcds_world_unit_per_second{unit="Heat"} 0.25\n', text)
        self.assertIn('pcds_plugin_faults_total{plugin="chat-bridge"} 1\n', text)
        self.assertIn('pcds_plugin_http_failed_total{plugin="chat-bridge"} 1\n', text)
        self.assertIn('pcds_plugin_status{plugin="chat-bridge",status="running"} 1\n', text)
        self.assertIn('pcds_plugin_status{plugin="chat-bridge",status="failed"} 0\n', text)
        self.assertIn('pcds_plugin_status{plugin="we\\"ird\\\\id",status="failed"} 1\n', text, "label values are escaped")
        self.assertIn('pcds_build_info{server_version="0.8.0",game_version="1.2.3",planet="Prime",state="running"} 1\n', text)
        # No player is a label, and no secret is in the text.
        self.assertNotIn("Ann", text)
        self.assertNotIn("playerId", text)
        # Nothing known: only the counters and pcds_up. Odd shapes do not break it.
        lint(replay({}))
        lint(replay({"events": [1, None, {"type": "x"}], "status": {"players": "no", "units": [], "objects": 3, "tick": 5, "s3": {"enabled": "yes"}}, "plugins": {"plugins": [None, {"id": "a", "status": "weird", "stats": "x"}]}}))
        self.assertIn('pcds_plugin_status{plugin="a",status="weird"} 1\n', replay({"plugins": {"plugins": [{"id": "a", "status": "weird"}]}}))

    def test_stream_starts_at_now_counts_later_events_and_follows_a_restart(self):
        self.assertEqual(self.exp.poll(), 0, "the first poll only finds the newest number")
        self.assertEqual((self.exp.after, self.exp.counts.snapshot()["chat"]), (3, 0))
        Fake.state["events"] += [dict(EVENTS[3]), dict(EVENTS[5]), dict(EVENTS[6])]
        Fake.state["events"][3]["id"], Fake.state["events"][4]["id"], Fake.state["events"][5]["id"] = 4, 5, 6
        Fake.state["dropped"] = 9
        self.assertEqual(self.exp.poll(), 3)
        n = self.exp.counts.snapshot()
        self.assertEqual((n["placed"], n["saved_ok"], n["saved_failed"], n["dropped"]), (1, 1, 1, 5), "dropped counts from where the exporter began")
        self.assertEqual(self.exp.poll(), 0)
        # What it asked: its own name, the kinds, the position, the scoped token.
        asked = [a for a in Fake.state["asked"] if a[1] == "/api/plugins/events"]
        self.assertEqual(asked[0][3], {"consumer": "prometheus", "types": TYPES, "limit": 1, "after": 0})
        self.assertEqual((asked[1][3]["after"], asked[1][2]), (3, "Bearer pcdst_secret"))
        self.assertNotIn("wait", asked[1][3], "wait is 0 here")
        # The server restarts: a new epoch, the numbers begin again, the new ring is read from 0.
        Fake.state.update(epoch=200, events=[dict(EVENTS[0], id=1), dict(EVENTS[0], id=2)], dropped=0)
        self.assertEqual(self.exp.poll(), 2)
        n = self.exp.counts.snapshot()
        self.assertEqual((n["chat"], n["resets"], n["dropped"], self.exp.after), (2, 1, 5, 2), "counters go on, the position is reset")
        # A long poll asks for it; the answer's number is the next `after`.
        long = Exporter(self.base, "t", wait=25)
        long.poll()
        long.poll()
        self.assertEqual(Fake.state["asked"][-1][3]["wait"], 25)

    def test_scrape_text_from_the_fake_server_and_the_http_server(self):
        self.exp.poll()
        Fake.state["events"] += [dict(EVENTS[0], id=4)]
        self.exp.poll()
        self.exp.stream_state = (True, True)
        for port in range(8580, 8600):
            try:
                server = serve(self.exp, "127.0.0.1", port)
                break
            except OSError:
                continue
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            answer = urllib.request.urlopen("http://127.0.0.1:%d/metrics" % server.server_port, timeout=5)
            text = answer.read().decode()
            self.assertEqual(answer.headers["Content-Type"], CONTENT_TYPE)
            lint(text)
            self.assertEqual((self.value(text, "pcds_up"), self.value(text, "pcds_chat_messages_total"), self.value(text, "pcds_players_online")), ("1", "1", "2"))
            self.assertIn('pcds_exporter_source_up{source="stream"} 1\n', text)
            with self.assertRaises(urllib.error.HTTPError) as e:
                urllib.request.urlopen("http://127.0.0.1:%d/other" % server.server_port, timeout=5)
            self.assertEqual(e.exception.code, 404)
        finally:
            server.shutdown()
        # Every upstream request carried the token; none carried it in the URL.
        self.assertTrue(all(a[2] == "Bearer pcdst_secret" and "pcdst" not in a[1] for a in Fake.state["asked"]))

    def test_no_admin_asks_for_the_stream_only(self):
        exp = Exporter(self.base, "pcdst_secret", admin=False, wait=0)
        exp.stream_state = (True, True)
        exp.poll()
        text = exp.metrics()
        lint(text)
        self.assertNotIn("pcds_players_online", text)
        self.assertEqual(self.value(text, "pcds_up"), "1")
        self.assertEqual({a[1] for a in Fake.state["asked"]}, {"/api/plugins/events"})
        # A server that would answer 403 on both admin endpoints is never asked, so nothing is counted against it.
        Fake.state["admin_status"] = 403
        again = exp.metrics()
        lint(again)
        self.assertEqual({a[1] for a in Fake.state["asked"]}, {"/api/plugins/events"})
        self.assertIn('pcds_exporter_errors_total{source="stream"} 0\n', again)
        self.assertNotIn('source="status"', again)

    def test_a_big_bundle_renders_in_under_50_ms(self):
        bundle = {"events": [dict(EVENTS[i % 8], id=i + 1) for i in range(10000)], "status": STATUS, "plugins": PLUGINS}
        best = 1e9
        for _ in range(3):
            started = time.perf_counter()
            text = replay(bundle)
            best = min(best, time.perf_counter() - started)
        lint(text)
        self.assertEqual(self.value(text, "pcds_chat_messages_total"), "1250")
        self.assertLess(best, 0.05, "%.1f ms" % (best * 1000))

    def test_a_scoped_token_on_the_admin_endpoints_is_an_error_count_not_a_crash(self):
        Fake.state["admin_status"] = 403
        text = self.exp.metrics()
        lint(text)
        self.assertEqual(self.value(text, "pcds_up"), "1", "the server answered")
        self.assertIn('pcds_exporter_source_up{source="status"} 0\n', text)
        self.assertIn('pcds_exporter_errors_total{source="status"} 1\n', text)
        self.assertNotIn("pcds_players_online", text)

    def test_a_server_that_is_down_is_pcds_up_0_and_the_counters_stay(self):
        self.exp.poll()
        Fake.state["events"] += [dict(EVENTS[0], id=4)]
        self.exp.poll()
        down = Exporter("http://127.0.0.1:1", "pcdst_secret", wait=0)
        down.counts = self.exp.counts
        text = down.metrics()
        lint(text)
        self.assertEqual((self.value(text, "pcds_up"), self.value(text, "pcds_chat_messages_total")), ("0", "1"))
        self.assertNotIn("pcds_players_online", text)
        self.assertIn('pcds_exporter_errors_total{source="status"} 1\n', text)
        # The stream loop survives it, backs off, and prints the kind of error only.
        errors, slept = io.StringIO(), []
        down.stop = type("Stop", (), {"is_set": lambda s: len(slept) >= 2, "wait": lambda s, t: slept.append(t)})()
        with contextlib.redirect_stderr(errors):
            down.stream_loop()
        self.assertEqual(slept, [4.0, 8.0])
        self.assertIn("URLError (try 1)", errors.getvalue())
        for secret in ("pcdst_secret", "127.0.0.1", "/api/"):
            self.assertNotIn(secret, errors.getvalue())
        self.assertEqual(down.stream_state, (False, False))
        # The stream refusing (a token without the scope) is reached, but not ok.
        Fake.state["stream_status"] = 403
        refused = Exporter(self.base, "x", admin=False, wait=0)
        refused.stop = type("Stop", (), {"is_set": lambda s: len(slept) >= 3, "wait": lambda s, t: slept.append(t)})()
        with contextlib.redirect_stderr(errors):
            refused.stream_loop()
        self.assertEqual(refused.stream_state, (True, False))
        self.assertEqual(self.value(refused.metrics(), "pcds_up"), "1")


def self_test():
    stream = io.StringIO()
    result = unittest.TextTestRunner(stream=stream, verbosity=0).run(unittest.defaultTestLoader.loadTestsFromTestCase(Tests))
    if not result.wasSuccessful():
        sys.exit(stream.getvalue())
    print("self-test ok (%d tests)" % result.testsRun)


def main(argv):
    parser = argparse.ArgumentParser(description="Prometheus exporter for a Planet Crafter dedicated server")
    parser.add_argument("--api", default=os.environ.get("PCDS_API", "http://127.0.0.1:7782"))
    parser.add_argument("--listen", default="127.0.0.1:9188")
    parser.add_argument("--no-admin", action="store_true")
    parser.add_argument("--consumer", default="prometheus")
    parser.add_argument("--wait", type=int, default=25)
    parser.add_argument("--replay")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        return self_test()
    if args.replay:
        with open(args.replay) as f:
            return print(replay(json.load(f)), end="")
    token = os.environ.get("PCDS_TOKEN")
    if not token:
        sys.exit("set PCDS_TOKEN; see the top of this file")
    host, _, port = args.listen.rpartition(":")
    exporter = Exporter(args.api, token, not args.no_admin, args.consumer, max(0, min(args.wait, 25)))
    server = serve(exporter, host or "127.0.0.1", int(port))
    threading.Thread(target=exporter.stream_loop, daemon=True).start()
    signal.signal(signal.SIGTERM, lambda *_: threading.Thread(target=server.shutdown).start())
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        exporter.stop.set()
        exporter.leave()


if __name__ == "__main__":
    main(sys.argv[1:])
