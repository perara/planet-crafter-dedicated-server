#!/usr/bin/env python3
"""Long soak: hours of changing conditions on a real late-game world, sampled every 30 s.

Purpose: soak.py runs one steady load. This runs everything an operator's week holds, on a
timetable repeated every hour, to find what degrades only after hours: leaks, creeping
memory or descriptors, log spam, backups that pile up, a queue that grows, a stall that
reaches the tick.

The server runs as a transient systemd user unit (`pcds run` with the service file's restart
rules, lib.UnitSrv), with autosave every minute, a backup every 10 minutes (24 kept), S3 uploads
to a fake S3, chat hooks to a fake webhook, the plugins hello-stats, welcome-rules and (when
--netplugin points at a built one) a net.http plugin that posts to the same fake, and a dashboard-like
client that reads the status once a second. In every hour (minute of the hour, scaled by --scale):

  :05  pause the world for 60 s                  :07 :22 :37 :52  another set of live rules
  :10 :30 :50  hot flip to the other bank        :15 :45  reload the plugins
  :22-:34  16 players   :40-:52  32 players      :53-:59  join/leave churn (24 workers)
  other times 8 players acting every 200 ms (the steady load: `rejoin` bots, which come back when dropped)

and, once: a real update from a local feed (hours 3, 7 and 11: download, install in the standby
bank, flip), kill -9 of the bank every 2 hours at :35 (the front restarts it from the last snapshot
or save), SIGSTOP of the bank for 30 s at hour 6 :12 (the watchdog must not replace it), a 10 minute
S3 outage in hours 1, 5, 9 and a 5 minute webhook outage in hours 2, 6, 10. Every hour the world file is
checked for its invariants.

Outputs in the work folder: samples.csv (COLS), events.log, flips.csv, plugins.csv, sidecar.csv (what the
dashboard-like client saw), incidents.log. Analyse with tools/perf/trend.py samples.csv.

Requirements: python3 (3.9+), a built `pcds`, the game tables, a save (copied), systemd --user, and a
cargo test binary for the bots (--test-bin, built with `cargo test --release --no-run`).

Run:  tools/chaos/longsoak.py --world SAVE --data TABLES --bin PCDS --work DIR --test-bin TESTBIN [--hours 12]
          [--scale 1] [--port 8640] [--unit pcds-qa-soak2-long] [--netplugin DIR]

A pass: exit status 0 and a last JSON line {"pass": true, ...}: no invariant broken, the status answered, no flip dropped a player,
the bank was never replaced by the watchdog during the SIGSTOP, the final stop exited 0 and the restart from the
final save kept every container. The trends are judged by trend.py.
"""
import argparse
import glob
import hashlib
import json
import os
import secrets
import shutil
import signal
import subprocess
import sys
import tarfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import lib  # noqa: E402

COLS = ("t,f_rss,f_thr,f_fds,f_cpu,f_cpu_pct,b_pid,b_rss,b_thr,b_fds,b_cpu,b_cpu_pct,busy,longest_us,winmax_longest_us,winmax_busy,fps,tick_p50_us,tick_p99_us,tick_max_us,wo,net,inv,players,gametime,"
        "save_bytes,backups,backups_bytes,home_bytes,log_bytes,log_lines,udp_rx,udp_tx,tcp_admin,plugins_running,plugin_faults,s3_hits,hook_hits,plug_hits,"
        "st_ms_p50,st_ms_max,st_fails,bytes_in,bytes_out,flips,crashes,unit_anon,unit_mem,unit_restarts,phase").split(",")

RULE_SETS = [
    {"speed": {"growth": 2, "machines": 1}, "gauges": {"oxygenDrain": 1, "thirstDrain": 1}, "meteo": {"eventChance": 1}, "loot": {"amount": 1}},
    {"speed": {"growth": 1, "machines": 2}, "gauges": {"oxygenDrain": 0.5, "thirstDrain": 2}, "meteo": {"eventChance": 3}, "loot": {"amount": 2}},
    {"speed": {"growth": 4, "machines": 1}, "gauges": {"oxygenDrain": 2, "thirstDrain": 0.5}, "meteo": {"eventChance": 0}, "loot": {"amount": 0.5}},
    {"speed": {"growth": 1, "machines": 1}, "gauges": {"oxygenDrain": 1, "thirstDrain": 1}, "meteo": {"eventChance": 1}, "loot": {"amount": 1}},
]


class Fake:
    """One HTTP server for the chat hook (/chat), the plugin's posts (/hook), S3 (PUT /bucket/...) and the release feed.
    Each kind has a mode: ok, 500, slow (8 s), drop (the connection closes unanswered)."""

    def __init__(self, port, feed):
        self.modes = dict(chat="ok", plug="ok", s3="ok")
        self.hits = dict(chat=0, plug=0, s3=0, s3bytes=0, other=0)
        self.feed = feed
        fake = self

        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def handle_any(self):
                path = self.path.split("?")[0]
                if self.command == "GET" and path.startswith(("/releases/", "/download/")):
                    f = os.path.join(fake.feed, path.lstrip("/"))
                    if os.path.isfile(f) and ".." not in path:
                        data = open(f, "rb").read()
                        self.send_response(200)
                        self.send_header("Content-Length", str(len(data)))
                        self.send_header("Connection", "close")
                        self.end_headers()
                        self.wfile.write(data)
                        return
                kind = "chat" if path.startswith("/chat") else "plug" if path.startswith("/hook") else "s3" if self.command == "PUT" else "other"
                n = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(n) if n else b""
                if kind == "other":
                    fake.hits["other"] += 1
                else:
                    fake.hits[kind] += 1
                if kind == "s3":
                    fake.hits["s3bytes"] += len(body)
                mode = fake.modes.get(kind, "ok")
                if mode == "drop":
                    self.close_connection = True
                    return
                if mode == "slow":
                    time.sleep(8)
                code = 500 if mode == "500" else 200
                out = b'{"reply":"pong"}' if self.command == "POST" else b""
                self.send_response(code)
                self.send_header("Content-Length", str(len(out)))
                self.end_headers()
                self.wfile.write(out)

            do_POST = do_PUT = do_GET = handle_any

        self.httpd = ThreadingHTTPServer((lib.LOOPBACK, port), H)
        self.httpd.daemon_threads = True
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter, epilog="See the header of this file for requirements and what a pass is.")
    lib.add_server_args(ap, port=8640)
    ap.add_argument("--hours", type=float, default=12.0, help="length of the run (default %(default)s)")
    ap.add_argument("--scale", type=float, default=1.0, help="scales the timetable (an hour lasts 3600*scale s): 0.02 is a dry run of the schedule (default %(default)s)")
    ap.add_argument("--sample", type=int, default=30, help="seconds between samples (default %(default)s)")
    ap.add_argument("--unit", default="pcds-qa-soak2-long", help="the transient systemd user unit (must start with pcds-qa-)")
    ap.add_argument("--cpus", default="8-15", help="AllowedCPUs of the unit (default %(default)s)")
    ap.add_argument("--netplugin", default="", help="folder of a built net.http plugin (plugin.toml, plugin.wasm) that posts to http://127.0.0.1:PORT+3/hook")
    ap.add_argument("--steady", type=int, default=8, help="players of the steady load (default %(default)s)")
    ap.add_argument("--examples", default=os.path.join(lib.REPO, "examples", "plugins"), help="the example plugins (default %(default)s)")
    args = ap.parse_args()
    lib.check_inputs(args)
    os.makedirs(args.work, exist_ok=True)
    work = os.path.join(args.work, "run")
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(work)
    args.work = work
    log = lib.Log(os.path.join(work, "events.log"))
    inc = open(os.path.join(work, "incidents.log"), "w")
    verdict = lib.Verdict("longsoak", log)
    L = log
    H, M = 3600.0 * args.scale, 60.0 * args.scale
    PORT = args.port
    total = args.hours * H

    def incident(*a):
        line = f"{time.time() - log.t0:9.1f} " + " ".join(str(x) for x in a)
        inc.write(line + "\n")
        inc.flush()
        L("INCIDENT", *a)

    feed = os.path.join(work, "feed")
    os.makedirs(os.path.join(feed, "releases", "tags"))
    os.makedirs(os.path.join(feed, "download"))
    fake = Fake(PORT + 3, feed)
    endpoint = f"http://{lib.LOOPBACK}:{PORT + 3}"

    rules = {"save": {"autosaveMinutes": 1, "backupsKept": 24, "backupMinutes": 10}, "limits": {"maxPlayers": 40}, "world": {"dlcPlanets": ["Humble"]}}
    srv = lib.UnitSrv("long", args, args.unit, rules=rules, env={"PCDS_RELEASES": f"{endpoint}/releases"}, cpus=args.cpus)
    home = srv.home
    srv.start()
    base_bad, base_counts = lib.check_save(srv.world)
    L("started; home", home, "save", base_counts)
    unit_cg = ""
    try:
        cg = subprocess.run(["systemctl", "--user", "show", "-p", "ControlGroup", "--value", f"{args.unit}.service"], capture_output=True, text=True).stdout.strip()
        unit_cg = "/sys/fs/cgroup" + cg if cg else ""
    except Exception:
        pass

    def cg_stat():
        try:
            anon = int([l for l in open(unit_cg + "/memory.stat") if l.startswith("anon ")][0].split()[1])
            return anon, int(open(unit_cg + "/memory.current").read())
        except Exception:
            return -1, -1

    # ---- settings the soak keeps on -------------------------------------------------------
    s3keys = {"backup.s3Endpoint": endpoint, "backup.s3Bucket": "soak-bucket", "backup.s3Region": "auto", "backup.s3AccessKey": secrets.token_hex(8), "backup.s3SecretKey": secrets.token_hex(16)}
    st, body = srv.post("/api/config", {**s3keys, "chat.webhooks": [f"{endpoint}/chat"], "chat.secret": secrets.token_hex(8), "chat.commandPrefix": "!", "chat.timeoutSeconds": 3, "chat.retries": 1})
    L("config", st, str(body)[:200])
    if st != 200:
        verdict.fail("could not set S3 and chat hooks", status=st, body=str(body)[:300])

    plugdir = os.path.join(home, "plugins")
    os.makedirs(plugdir, exist_ok=True)
    plug_ids = []
    for pid in ("hello-stats", "welcome-rules"):
        d = os.path.join(plugdir, pid)
        shutil.rmtree(d, ignore_errors=True)
        shutil.copytree(os.path.join(args.examples, pid), d, ignore=shutil.ignore_patterns("src", "target", "Cargo.*", "*.md"))
        plug_ids.append(pid)
    if args.netplugin:
        d = os.path.join(plugdir, "net-soak")
        shutil.rmtree(d, ignore_errors=True)
        shutil.copytree(args.netplugin, d, ignore=shutil.ignore_patterns("src", "target", "Cargo.*", "plugin-api"))
        plug_ids.append("net-soak")

    def plugins_reload(enable=False):
        s, _ = srv.post("/api/plugins/reload", timeout=60)
        if enable:
            for pid in plug_ids:
                s2, j2 = srv.post("/api/plugins/enable", {"id": pid}, timeout=60)
                if s2 != 200:
                    verdict.fail("could not approve a plugin", id=pid, status=s2, body=str(j2)[:300])
        s3, j = srv.get("/api/plugins", timeout=60)
        rows = []
        if s3 == 200 and isinstance(j, dict):
            for p in j.get("plugins", []):
                stt = p.get("stats") or {}
                rows.append((p["id"], p.get("status"), stt.get("calls"), stt.get("faults"), stt.get("restarts"), stt.get("slow"), stt.get("shed"), stt.get("fuel"), stt.get("averageMicros"), stt.get("maxMicros"), (stt.get("http") or {}).get("requests"), (stt.get("http") or {}).get("failed")))
        return s, rows

    pl_csv = open(os.path.join(work, "plugins.csv"), "w")
    pl_csv.write("t,id,status,calls,faults,restarts,slow,shed,fuel,avg_us,max_us,http_requests,http_failed\n")

    def plugins_snapshot():
        s3, j = srv.get("/api/plugins", timeout=60)
        out = []
        if s3 == 200 and isinstance(j, dict):
            for p in j.get("plugins", []):
                stt = p.get("stats") or {}
                h = stt.get("http") or {}
                out.append((p["id"], p.get("status"), stt.get("calls"), stt.get("faults"), stt.get("restarts"), stt.get("slow"), stt.get("shed"), stt.get("fuel"), stt.get("averageMicros"), stt.get("maxMicros"), h.get("requests"), h.get("failed")))
        for row in out:
            pl_csv.write(f"{round(time.time() - log.t0)}," + ",".join(str(x) for x in row) + "\n")
        pl_csv.flush()
        return out

    plugins_reload(enable=True)
    time.sleep(2)
    ps = plugins_snapshot()
    L("plugins", [(r[0], r[1]) for r in ps])
    for r in ps:
        if r[1] != "running":
            verdict.fail("a plugin is not running at the start", plugin=r[0], status=r[1])

    # ---- the bots ----------------------------------------------------------------------------
    steady = {"b": None}

    def start_steady():
        steady["b"] = lib.Bots("rejoin", args, f"{lib.LOOPBACK}:{PORT}", players=args.steady, seconds=int(total + 3600))
        L("steady bots started", args.steady)

    start_steady()
    extra = {}  # name -> Popen

    def burst(name, players, seconds):
        p = lib.load_proc(srv, args, players, int(seconds), name=name, stdout=open(os.path.join(work, f"load-{name}.out"), "ab"))
        extra[name] = p
        L("burst", name, players, "players for", int(seconds), "s pid", p.pid)

    def burst_end(name):
        p = extra.pop(name, None)
        if p and p.poll() is None:
            p.terminate()
            try:
                p.wait(10)
            except subprocess.TimeoutExpired:
                p.kill()
        L("burst ended", name, "rc", p.returncode if p else None)

    churn = {"b": None}

    def churn_start(seconds):
        churn["b"] = lib.Bots("churn", args, f"{lib.LOOPBACK}:{PORT}", players=24, seconds=int(seconds), PCDS_LOAD_REUSE_NAMES=1, PCDS_LOAD_SEED=int(time.time()) % 1000)
        L("churn started", int(seconds), "s")

    def churn_end():
        b = churn["b"]
        if b:
            time.sleep(1)
            b.stop()
            L("churn ended", b.stats())
            churn["b"] = None

    # ---- the sidecar: a dashboard-like client --------------------------------------------------
    win = dict(lat=[], longest=0, busy=0.0, fails=0, calls=0, last=None, lock=threading.Lock())
    stop = threading.Event()
    side = open(os.path.join(work, "sidecar.csv"), "w")
    side.write("t,route,status,ms\n")
    slow_calls = []

    def sidecar():
        n = 0
        while not stop.is_set():
            t0 = time.time()
            routes = [("/api/status", 1)]
            if n % 10 == 0:
                routes += [("/api/plugins", 1), ("/api/worlds", 1)]
            if n % 30 == 0:
                routes += [("/api/map", 1)]
            if n % 60 == 0:
                routes += [("/api/logs", 1)]
            for r, _ in routes:
                t1 = time.time()
                s, j = srv.get(r, timeout=10)
                ms = (time.time() - t1) * 1000
                if r == "/api/status":
                    with win["lock"]:
                        win["calls"] += 1
                        win["lat"].append(ms)
                        if s == 200 and isinstance(j, dict):
                            win["last"] = j
                            win["longest"] = max(win["longest"], j.get("longestLoopMicros", 0))
                            win["busy"] = max(win["busy"], j.get("busyPercent", 0))
                        else:
                            win["fails"] += 1
                if s != 200 or ms > 1000:
                    side.write(f"{round(t1 - log.t0, 1)},{r},{s},{ms:.0f}\n")
                    side.flush()
            n += 1
            stop.wait(max(0.0, 1.0 - (time.time() - t0)))

    threading.Thread(target=sidecar, daemon=True).start()

    # ---- helpers for the actions ------------------------------------------------------------------
    acked = {"containers": set()}

    def read_world():
        try:
            shutil.copy(srv.world, f"{home}/check.json")
            bad, counts = lib.check_save(f"{home}/check.json")
            return [b for b in bad if b not in base_bad], counts
        except Exception as e:
            return [f"world file unreadable: {e}"], {}

    def mark():
        s, _ = srv.post("/api/save", timeout=300)
        if s == 200:
            shutil.copy(srv.world, f"{home}/acked.json")
            acked["containers"] = lib.containers(f"{home}/acked.json")
            return True
        return False

    def status():
        s, j = srv.status(timeout=30)
        return j if s == 200 and isinstance(j, dict) else {}

    def bank_pid():
        try:
            return status()["banks"]["live"]["pid"]
        except Exception:
            return None

    def wait_players(n, timeout=90):
        end = time.time() + timeout
        k = -1
        while time.time() < end:
            k = len(status().get("players", []))
            if k >= n:
                return k
            time.sleep(2)
        return k

    flips_csv = open(os.path.join(work, "flips.csv"), "w")
    flips_csv.write("t,kind,ok,seconds,detail\n")

    def flip(kind="flip", body=None):
        t = time.time()
        steady_t = time.time()
        s, j = srv.post("/api/update", body or {"rollback": True}, timeout=120)
        if s != 200:
            incident("flip refused", s, str(j)[:200])
            flips_csv.write(f"{round(t - log.t0)},{kind},refused,0,{str(j)[:100]!r}\n")
            return
        seq, done = j["seq"], None
        end = time.time() + 180
        while time.time() < end:
            u = srv.get("/api/update")[1]
            if isinstance(u, dict):
                done = next((e for e in u.get("history", []) if e.get("seq") == seq), None)
                if done:
                    break
            time.sleep(0.3)
        time.sleep(3)
        b = steady["b"]
        drops = b.stats(steady_t)["fails"] if b else 0
        ok = bool(done) and done.get("ok") is not False
        flips_csv.write(f"{round(t - log.t0)},{kind},{ok},{time.time() - t:.1f},{json.dumps(done)[:300]!r}\n")
        flips_csv.flush()
        L("flip", kind, ok, f"{time.time() - t:.1f}s", str(done)[:200])
        if not ok:
            verdict.fail("a flip failed", kind=kind, outcome=str(done)[:300])
        if drops:
            verdict.fail("a flip dropped players", kind=kind, drops=drops, outcome=str(done)[:200])

    rel = {"n": 0}

    def real_update():
        rel["n"] += 1
        tag = f"v0.8.0-qa.{rel['n']}"
        name = f"pcds-{tag}-linux-x86_64"
        exe = os.path.realpath(args.bin)
        d = os.path.dirname(exe)
        tar = os.path.join(feed, "download", name + ".tar.gz")
        web = os.path.join(d, "web", "index.html")
        web = web if os.path.exists(web) else os.path.join(lib.REPO, "server", "web", "index.html")
        with tarfile.open(tar, "w:gz", compresslevel=1) as tf:
            for src, arc in ((exe, "pcds"), (os.path.join(d, "libsteam_api.so"), "libsteam_api.so"), (web, "web/index.html")):
                tf.add(src, arcname=f"{name}/{arc}")
        sha = hashlib.sha256(open(tar, "rb").read()).hexdigest()
        open(tar + ".sha256", "w").write(f"{sha}  {name}.tar.gz\n")
        base = f"{endpoint}/download"
        doc = {"tag_name": tag, "assets": [{"name": name + ".tar.gz", "browser_download_url": f"{base}/{name}.tar.gz"}, {"name": name + ".tar.gz.sha256", "browser_download_url": f"{base}/{name}.tar.gz.sha256"}]}
        json.dump(doc, open(os.path.join(feed, "releases", "tags", tag), "w"))
        shutil.copy(os.path.join(feed, "releases", "tags", tag), os.path.join(feed, "releases", "latest"))
        flip("update", {"version": tag})
        for old in sorted(glob.glob(os.path.join(feed, "download", "*.tar.gz")))[:-2]:
            os.remove(old)

    def pause():
        srv.post("/api/pause", {"paused": True})
        time.sleep(3)
        g1 = status().get("gameTime")
        time.sleep(max(1.0, 57 * args.scale))
        st = status()
        g2 = st.get("gameTime")
        if g1 is not None and g2 is not None and abs(g2 - g1) > 0.5:
            verdict.fail("game time advanced while paused", g1=g1, g2=g2)
        if not st.get("held"):
            verdict.fail("a paused world is not held", held=st.get("held"))
        srv.post("/api/pause", {"paused": False})
        time.sleep(3)
        g3 = status().get("gameTime")
        if g3 is not None and g2 is not None and g3 <= g2:
            verdict.fail("game time did not advance after resume", g2=g2, g3=g3)
        L("pause/resume", g1, g2, g3)

    rule_n = {"n": 0}

    def rules_change():
        r = RULE_SETS[rule_n["n"] % len(RULE_SETS)]
        rule_n["n"] += 1
        s, j = srv.post("/api/rules", r)
        if s != 200:
            verdict.fail("rules change refused", status=s, body=str(j)[:200])
        L("rules", rule_n["n"], s)

    def integrity(label):
        bad, counts = read_world()
        L("integrity", label, counts)
        if bad:
            verdict.fail("world invariants", label=label, bad=bad[:6])

    def kill_bank():
        if not mark():
            incident("kill_bank: the save before the kill failed")
        p = bank_pid()
        t = time.time()
        crashes0 = status().get("banks", {}).get("crashes")
        if not p:
            incident("kill_bank: no bank")
            return
        os.kill(p, signal.SIGKILL)
        L("kill -9 bank", p)
        end = time.time() + 120
        back = None
        while time.time() < end:
            st = status()
            if st.get("state") == "running" and st.get("banks", {}).get("live", {}).get("pid") not in (None, p):
                back = time.time() - t
                break
            time.sleep(0.5)
        if back is None:
            verdict.fail("the bank did not come back after kill -9", waited=120)
            return
        n = wait_players(args.steady, 120)
        incident("kill -9 of the bank: world back in %.1f s, %d players back after %.1f s, crashes %s -> %s" % (back, n, time.time() - t, crashes0, status().get("banks", {}).get("crashes")))
        if n < args.steady:
            verdict.fail("players did not come back after the bank's kill", players=n)
        time.sleep(5)
        s, _ = srv.post("/api/save", timeout=300)
        shutil.copy(srv.world, f"{home}/after.json")
        lost = acked["containers"] - lib.containers(f"{home}/after.json")
        if lost:
            verdict.fail("acknowledged containers lost after the bank's kill", lost=len(lost))
        integrity("after kill")

    def sigstop_bank():
        p = bank_pid()
        if not p:
            incident("sigstop: no bank")
            return
        flips0 = status().get("banks", {}).get("crashes")
        os.kill(p, signal.SIGSTOP)
        L("SIGSTOP bank", p)
        time.sleep(30)
        os.kill(p, signal.SIGCONT)
        t = time.time()
        end = t + 60
        while time.time() < end:
            if status().get("state") == "running":
                break
            time.sleep(0.5)
        q = bank_pid()
        st = status()
        incident("SIGSTOP of the bank for 30 s: bank pid %s -> %s (must be the same), crashes %s -> %s, status again after %.1f s, players %d" % (p, q, flips0, st.get("banks", {}).get("crashes"), time.time() - t, len(st.get("players", []))))
        if q != p:
            verdict.fail("the watchdog replaced a bank stopped for only 30 s", before=p, after=q)

    def outage(kind, secs):
        old = fake.modes[kind]
        fake.modes[kind] = "500"
        L("outage", kind, "start", secs)
        time.sleep(secs)
        fake.modes[kind] = old
        L("outage", kind, "end")

    # ---- the timetable --------------------------------------------------------------------------
    ev = []

    def at(t, name, fn, *a):
        ev.append((t, name, fn, a))

    hours = int(args.hours + 0.999)
    for h in range(hours):
        b = h * H
        m = lambda x: b + x * M
        at(m(5), "pause", pause)
        for x in (7, 22, 37, 52):
            at(m(x), "rules", rules_change)
        for x in (10, 30, 50):
            at(m(x), "flip", flip)
        for x in (15, 45):
            at(m(x), "plugin reload", lambda: (plugins_reload(), time.sleep(2), plugins_snapshot()))
        at(m(22), "burst16 start", burst, "Ba", 8, 12 * M)
        at(m(34), "burst16 end", burst_end, "Ba")
        at(m(40), "burst32 start", burst, "Bb", 24, 12 * M)
        at(m(52), "burst32 end", burst_end, "Bb")
        at(m(53), "churn start", churn_start, 6 * M)
        at(m(59.5), "churn end", churn_end)
        at(m(58), "integrity", integrity, f"hour {h}")
        at(m(1), "plugin sample", plugins_snapshot)
        if h in (3, 7, 11):
            at(m(17), "real update", real_update)
        if h % 2 == 1 and h >= 1:
            at(m(35), "kill bank", kill_bank)
        if h == 6:
            at(m(12), "sigstop", sigstop_bank)
        if h % 4 == 1:
            at(m(0.5), "s3 outage", outage, "s3", 10 * M)
        if h % 4 == 2:
            at(m(0.5), "webhook outage", outage, "chat", 5 * M)
    ev.sort(key=lambda e: e[0])
    L("timetable", len(ev), "events over", f"{total:.0f} s")

    def run(name, fn, a):
        try:
            L("EVENT", name)
            fn(*a)
        except Exception as e:  # an action that fails is an incident, not the end of the run
            import traceback
            incident("event", name, "raised", repr(e), traceback.format_exc()[-400:])

    def phase_at(t):
        mm = (t % H) / M
        if 22 <= mm < 34:
            return "16"
        if 40 <= mm < 52:
            return "32"
        if 53 <= mm < 60:
            return "churn"
        return "8"

    # ---- the sampler / main loop -----------------------------------------------------------------
    csv = open(os.path.join(work, "samples.csv"), "w")
    csv.write(",".join(COLS) + "\n")
    t0 = time.time()
    next_sample = t0 + args.sample
    idx = 0
    log_lines = {"off": 0, "n": 0}
    prev = {"f": (0, 0, 0), "b": (0, 0, 0)}

    def count_log_lines():
        try:
            with open(srv.log, "rb") as f:
                f.seek(log_lines["off"])
                data = f.read()
                log_lines["off"] += len(data)
                log_lines["n"] += data.count(b"\n")
        except OSError:
            pass
        return log_lines["n"]

    def tcp_live(port):
        """Admin connections that are open (established or closing), not the TIME_WAIT of the dashboard's polls."""
        n = 0
        try:
            for line in open("/proc/net/tcp").read().splitlines()[1:]:
                p = line.split()
                if int(p[1].split(":")[1], 16) == port and p[3] in ("01", "04", "05", "08", "09"):
                    n += 1
        except OSError:
            pass
        return n

    def home_usage():
        tot, bk, nb = 0, 0, 0
        for root, dirs, files in os.walk(home):
            if root.startswith(os.path.join(home, "bin")) or root.startswith(os.path.join(home, "release")) or root.startswith(os.path.join(home, "feed")):
                dirs[:] = []
                continue
            for f in files:
                try:
                    sz = os.lstat(os.path.join(root, f)).st_size
                except OSError:
                    continue
                tot += sz
                if f.endswith(".json") and os.path.basename(root) == "backups":
                    bk += sz
                    nb += 1
        return tot, bk, nb

    while True:
        now = time.time()
        el = now - t0
        if el >= total:
            break
        while idx < len(ev) and ev[idx][0] <= el:
            _, name, fn, a = ev[idx]
            idx += 1
            threading.Thread(target=run, args=(name, fn, a), daemon=True).start()
        steady_b = steady["b"]
        if steady_b and steady_b.poll() is not None:
            incident("the steady bots exited", steady_b.poll())
            start_steady()
        if args.unit and srv.unit_state().get("ActiveState") not in ("active", "activating", "deactivating"):
            why = srv.ensure_running()
            incident("the unit was not running", why)
        if now >= next_sample:
            next_sample += args.sample
            with win["lock"]:
                j = win["last"] or {}
                lat = sorted(win["lat"])
                row_win = dict(longest=win["longest"], busy=win["busy"], fails=win["fails"], p50=lat[len(lat) // 2] if lat else -1, mx=lat[-1] if lat else -1)
                win["lat"], win["longest"], win["busy"], win["fails"], win["calls"] = [], 0, 0.0, 0, 0
            bk = (j.get("banks") or {})
            bpid = (bk.get("live") or {}).get("pid") or 0
            f, b = lib.proc(srv.pid or 0), lib.proc(bpid)
            fpid = (srv.pids()["serve"] or [0])[0]
            f = lib.proc(fpid)
            z = lambda d, k: d[k] if d else -1
            o = j.get("objects", {})
            q = lib.udp_queues(srv.port) or (-1, -1)
            tot, bkb, nb = home_usage()
            anon, mem = cg_stat()
            pl = j.get("plugins") or {}
            pct = lambda key, pid, d: round(100 * (d["cpu"] - prev[key][1]) / (now - prev[key][2]), 2) if d and prev[key][0] == pid and now > prev[key][2] else -1
            f_pct, b_pct = pct("f", fpid, f), pct("b", bpid, b)
            prev["f"], prev["b"] = (fpid, f["cpu"] if f else 0, now), (bpid, b["cpu"] if b else 0, now)
            row = dict(t=round(el), f_rss=z(f, "rss"), f_thr=z(f, "thr"), f_fds=z(f, "fds"), f_cpu=z(f, "cpu"), f_cpu_pct=f_pct, b_pid=bpid, b_rss=z(b, "rss"), b_thr=z(b, "thr"), b_fds=z(b, "fds"), b_cpu=z(b, "cpu"), b_cpu_pct=b_pct,
                       busy=j.get("busyPercent", -1), longest_us=j.get("longestLoopMicros", -1), winmax_longest_us=row_win["longest"], winmax_busy=row_win["busy"], fps=j.get("fps", -1),
                       tick_p50_us=(j.get("tick") or {}).get("p50Micros", -1), tick_p99_us=(j.get("tick") or {}).get("p99Micros", -1), tick_max_us=(j.get("tick") or {}).get("maxMicros", -1),
                       wo=o.get("world", -1), net=o.get("network", -1), inv=o.get("inventories", -1), players=len(j.get("players", [])), gametime=j.get("gameTime", -1),
                       save_bytes=os.path.getsize(srv.world) if os.path.exists(srv.world) else -1, backups=nb, backups_bytes=bkb, home_bytes=tot,
                       log_bytes=os.path.getsize(srv.log) if os.path.exists(srv.log) else -1, log_lines=count_log_lines(), udp_rx=q[0], udp_tx=q[1], tcp_admin=tcp_live(srv.admin),
                       plugins_running=pl.get("running", -1), plugin_faults=pl.get("faults", -1), s3_hits=fake.hits["s3"], hook_hits=fake.hits["chat"], plug_hits=fake.hits["plug"],
                       st_ms_p50=round(row_win["p50"], 1), st_ms_max=round(row_win["mx"], 1), st_fails=row_win["fails"],
                       bytes_in=(j.get("traffic") or {}).get("bytesIn", -1), bytes_out=(j.get("traffic") or {}).get("bytesOut", -1), flips=bk.get("flips", -1), crashes=bk.get("crashes", -1),
                       unit_anon=anon, unit_mem=mem, unit_restarts=srv.unit_state().get("NRestarts", -1), phase=phase_at(el))
            csv.write(",".join(str(row[c]) for c in COLS) + "\n")
            csv.flush()
        time.sleep(0.5)

    # ---- the end ------------------------------------------------------------------------------------
    stop.set()
    for n in list(extra):
        burst_end(n)
    churn_end()
    if steady["b"]:
        steady["b"].stop()
    time.sleep(2)
    integrity("final")
    final_status = status()
    L("final lastSave", json.dumps(final_status.get("lastSave")), "backup", json.dumps(final_status.get("backup")), "s3", json.dumps(final_status.get("s3")))
    text = srv.logtext()
    srv.stop()
    rc = srv.final_rc()
    L("server stopped rc", rc)
    if rc != 0:
        verdict.fail("the final stop did not exit 0", rc=rc)
    if "panicked" in text:
        verdict.fail("a panic in the server log", log=srv.log)
    # A restart from the final save: every container is still there.
    before = lib.containers(srv.world)
    srv.start(wait=120)
    time.sleep(5)
    s, _ = srv.post("/api/save", timeout=300)
    after = lib.containers(srv.world)
    L("restart from the final save: containers", len(before), "->", len(after))
    if before - after:
        verdict.fail("containers lost over the final restart", lost=len(before - after))
    srv.stop()
    shutil.copy(srv.log, os.path.join(work, "front.log.copy"))
    verdict.exit(samples=os.path.join(work, "samples.csv"), fake_hits=fake.hits, home=home)


if __name__ == "__main__":
    main()
