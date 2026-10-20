#!/usr/bin/env python3
"""Chaos: random faults against a server with players, with the invariants checked after each.

Purpose: find what survives badly. A seeded stream draws actions: kill -9 of the bank, the
front, both, or the new bank in the middle of a flip; SIGSTOP of the bank, the front, the new
bank or the players; SIGTERM; stop/start bursts of the world; a flip (POST /api/update with
rollback, both banks are this program); saves in a storm and into a read-only folder; pause;
rule and setting changes (valid and invalid); bans; chat hooks and S3 backups against a fake
endpoint that answers fine, with 500 or slowly; 5000-character chat; status hammering. After
each action the tool waits for the server to settle (one front, one bank, idle, status
answering) and checks:
  - the active-bank file names a bank; the world file loads and keeps its invariants (unique
    ids, every item held once, links resolve; a flaw the start save already had is ignored);
  - no .tmp file of a dead writer is left in the world's folder;
  - every container an acknowledged save held (POST /api/save answered 200) is still there;
  - the players are back (the bots rejoin by themselves) within 45 s;
  - a flip drops nobody; invalid settings and rules are refused with 400; pause stops the
    game clock and resume restarts it; a read-only folder makes a save fail visibly.
The front is not run by systemd here: when it is gone the tool waits for its banks to end (an
orphan bank is a failure) and starts it again, as `pcds run` or the unit would.

The bots are NOT a pcds command: they are the ignored cargo test load::tests::rejoin
(server/src/load/tests.rs), driven by the test-only variables PCDS_LOAD_SERVER (HOST:PORT),
PCDS_LOAD_PLAYERS, PCDS_LOAD_SECONDS and PCDS_TEST_DATA; this tool sets them. They print one
JSON line per join (t, n, joined, actions, failed).

Two ways to supervise the server (--unit): by this tool (the default: it stands in for `pcds run`
and the unit, and starts the front again when it is gone), or by systemd (--unit pcds-qa-NAME: the
server is installed by `pcds setup` in its own folder and run as `pcds run` under a transient user
unit with the service file's restart rules; a start limit that systemd reaches is reset, counted and
reported as an event, as an owner would). Besides the faults above the stream draws: a 120 s SIGSTOP
of the bank (limits.bankWatchdogSeconds must replace it) and a 10 s one (it must not), a world switch,
a real update from a local release feed (download, install, flip) and, from a thread of its own all
the time, plugin churn on examples/plugins/hello-stats (install, approve, disable, remove, a flipped
byte in its module or manifest, commands), checked after every action against what was done.

Requirements: python3 (3.9+), a built `pcds` (make build), the game tables, a save to run on
(copied, never written), cargo (to build the test once; or --test-bin a binary built with
`cargo test --release --no-run`). Linux (/proc, signals). Runs the server with PCDS_DEV=flip-verify.

Run:  tools/chaos/chaos.py --world run/worlds/world.json [--seed 1] [--actions 40] [--bots 6]
          [--no-flip] [--only kill_bank,flip]
      Ports: --port (default 8420: players, 8421: admin, 8422: the fake endpoint).
      The same seed draws the same actions; the log (chaos-SEED.log in the work folder) has
      every action, its result and every check.

A pass: exit status 0 and a last JSON line {"pass": true, "failures": [], ...} after all
actions, the final stop (exit 0, no process left). Every failure is a line "FAIL ..." in the log.
"""
import argparse
import glob
import json
import os
import random
import secrets
import shutil
import signal
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import lib  # noqa: E402


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter, epilog="See the header of this file for requirements and what a pass is.")
    lib.add_server_args(ap)
    ap.add_argument("--seed", type=int, default=1, help="the seed of the action stream (default %(default)s)")
    ap.add_argument("--actions", type=int, default=40, help="how many actions (default %(default)s)")
    ap.add_argument("--bots", type=int, default=6, help="rejoining players (default %(default)s)")
    ap.add_argument("--no-flip", action="store_true", help="leave out the actions that flip to the other bank")
    ap.add_argument("--only", default="", help="only these actions, comma-separated (names after a_ below)")
    ap.add_argument("--unit", default="", help="run the server as a transient systemd user unit of this name (must start with pcds-qa-) instead of supervising it here")
    ap.add_argument("--no-plugins", action="store_true", help="no plugin churn")
    ap.add_argument("--no-fault", action="store_true", help="no plugin that traps in every event")
    ap.add_argument("--cpus", default="8-15", help="AllowedCPUs of the unit (default %(default)s)")
    args = ap.parse_args()
    lib.check_inputs(args)
    os.makedirs(args.work, exist_ok=True)
    name = f"chaos-{args.seed}"
    log = lib.Log(os.path.join(args.work, f"{name}.log"), quiet=False)
    verdict = lib.Verdict("chaos", log)
    FAIL = verdict.fail
    L = log
    rnd = random.Random(args.seed)
    BOTS = args.bots
    PORT = args.port

    # ---- a fake webhook / S3 endpoint that can misbehave --------------------------------
    fake = dict(mode="ok", hits=0)
    FEED = os.path.join(args.work, f"chaos-{args.seed}-feed")
    shutil.rmtree(FEED, ignore_errors=True)
    os.makedirs(os.path.join(FEED, "releases", "tags"))
    os.makedirs(os.path.join(FEED, "download"))

    class H(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def feed(self):
            """GET /releases/... and /download/...: the local release feed (files of FEED)."""
            path = self.path.split("?")[0]
            f = os.path.join(FEED, path.lstrip("/"))
            if not (path.startswith(("/releases/", "/download/")) and os.path.isfile(f) and ".." not in path):
                return False
            data = open(f, "rb").read()
            self.send_response(200)
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(data)
            return True

        def handle_any(self):
            if self.command == "GET" and self.feed():
                return
            fake["hits"] += 1
            n = int(self.headers.get("Content-Length") or 0)
            if n:
                self.rfile.read(n)
            if fake["mode"] == "slow":
                time.sleep(8)
            code = 500 if fake["mode"] == "500" else 200
            body = b'{"reply":"pong"}' if self.command == "POST" else b""
            self.send_response(code)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        do_POST = do_PUT = do_GET = handle_any

    httpd = ThreadingHTTPServer((lib.LOOPBACK, PORT + 2), H)
    httpd.daemon_threads = True
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    endpoint = f"http://{lib.LOOPBACK}:{PORT + 2}"

    # ---- the target ----------------------------------------------------------------------
    rules = {"save": {"autosaveMinutes": 1, "backupsKept": 6, "backupMinutes": 1}, "limits": {"maxPlayers": 16}}
    feed_env = {"PCDS_RELEASES": f"{endpoint}/releases"}
    if args.unit:
        srv = lib.UnitSrv(name, args, args.unit, dev="flip-verify", rules=rules, env=feed_env, cpus=args.cpus)
    else:
        srv = lib.Srv(name, args, banks=True, dev="flip-verify", rules=rules, env=feed_env)
    home = srv.home
    events = dict(start_limit=0, restarts=0, replaced=0, updates=0)

    def supervise(timeout=70):
        """Stand in for `pcds run` / the unit: when the front is gone, wait for its banks, start it again.
        With --unit systemd does that; this only resets a unit that gave up."""
        p = srv.pids()
        if args.unit:
            if not p["serve"]:
                t = time.time()
                while srv.pids()["bank"] and time.time() - t < timeout:
                    time.sleep(0.5)
                now = srv.pids()  # a front that started meanwhile (the unit's restart) has banks of its own
                if now["bank"] and not now["serve"]:
                    FAIL("an orphan bank outlived its front", banks=now["bank"])
                    srv.kill_all()
                why = srv.ensure_running()
                if why:
                    events["start_limit" if why == "start-limit-hit" else "unit_ended"] = events.get("start_limit" if why == "start-limit-hit" else "unit_ended", 0) + 1
                    L("supervisor: the unit was not running (Result=%s); reset and started" % why)
            return
        if not p["serve"]:
            t = time.time()
            while srv.pids()["bank"] and time.time() - t < timeout:
                time.sleep(0.5)
            now = srv.pids()
            if now["bank"] and not now["serve"]:
                FAIL("an orphan bank outlived its front", banks=now["bank"])
                srv.kill_all()
            srv.proc = None
            srv.start(wait=90)
            L("supervisor: front started again")

    def front_pid():
        f = srv.pids()["serve"]
        return f[0] if f else None

    def bank_pid():
        b = srv.pids()["bank"]
        return b[0] if b else None

    def settle(timeout=90):
        """Wait for one front, one bank, idle, and the status answering."""
        t, last = time.time(), None
        while time.time() - t < timeout:
            supervise()
            p = srv.pids()
            s, j = srv.status()
            ok = isinstance(j, dict)
            last = (len(p["serve"]), len(p["bank"]), s, j.get("state") if ok else j, j.get("banks", {}).get("phase") if ok else None)
            if s == 200 and len(p["serve"]) == 1 and ok and j.get("state") == "running" and len(p["bank"]) == 1 and j.get("banks", {}).get("phase") == "idle":
                return True
            time.sleep(1)
        FAIL("did not settle", last=last, pids=srv.pids())
        return False

    srv.start()
    for _ in range(120):
        if srv.status()[0] == 200:
            break
        time.sleep(0.5)
    L("started, seed", args.seed, "home", home)
    bots = lib.Bots("rejoin", args, f"{lib.LOOPBACK}:{PORT}", players=BOTS, seconds=86400)

    # ---- invariants ----------------------------------------------------------------------
    base_bad = set(lib.check_save(srv.world)[0])
    acked = {"containers": set()}

    def read_world():
        try:
            shutil.copy(srv.world, f"{home}/check.json")
            bad, counts = lib.check_save(f"{home}/check.json")
            return [b for b in bad if b not in base_bad], counts
        except Exception as e:
            return [f"world file unreadable: {e}"], {}

    def mark():
        """A save the server acknowledged: remember what it holds."""
        s, _ = srv.post("/api/save")
        if s == 200:
            shutil.copy(srv.world, f"{home}/acked.json")
            acked["containers"] = lib.containers(f"{home}/acked.json")
            return True
        return False

    def check(label, since=0):
        if not settle():
            return
        active = open(f"{home}/bin/active").read().strip() if os.path.exists(f"{home}/bin/active") else "a"
        if active not in ("a", "b"):
            FAIL("active file invalid", label=label, active=active)
        bad, counts = read_world()
        if bad:
            FAIL("world invariants", label=label, bad=bad[:6])
        stale = []
        for tmp in glob.glob(f"{home}/*.tmp") + glob.glob(f"{home}/backups/*.tmp"):
            try:
                pid = int(os.path.basename(tmp).rsplit(".tmp", 1)[0].rsplit(".", 1)[1].split("-")[0])
            except (IndexError, ValueError):
                continue
            if not os.path.exists(f"/proc/{pid}"):
                stale.append(os.path.basename(tmp))
        if stale:
            FAIL("stale tmp files of dead writers", label=label, files=stale[:4])
        if acked["containers"]:
            s, j = srv.post("/api/save")
            if s == 200:
                shutil.copy(srv.world, f"{home}/after.json")
                now = lib.containers(f"{home}/after.json")
                lost = acked["containers"] - now
                if lost:
                    FAIL("acknowledged objects lost", label=label, lost=len(lost), sample=sorted(lost)[:5])
                acked["containers"] = now
            else:
                L("  (post-save", s, str(j)[:100], ")")
        deadline, n = time.time() + 45, -1
        while time.time() < deadline:
            s, j = srv.status()
            n = len(j["players"]) if s == 200 and isinstance(j, dict) else -1
            if n == BOTS:
                break
            time.sleep(2)
        if n != BOTS:
            FAIL("players did not get back in", label=label, players=n, expected=BOTS, bots=bots.stats(since))
        pl_verify(label)
        L("  check", label, counts)

    # ---- actions -------------------------------------------------------------------------
    def kill(pid, sig=signal.SIGKILL):
        try:
            os.kill(pid, sig)
        except ProcessLookupError:
            pass

    def bg(path, body=None, timeout=60):
        th = threading.Thread(target=lambda: srv.post(path, body, timeout), daemon=True)
        th.start()
        return th

    def flip_async():
        return bg("/api/update", {"rollback": True})

    def new_bank(old, wait=3):
        t, new = time.time(), None
        while time.time() - t < wait and not new:
            new = next((p for p in srv.pids()["bank"] if p != old), None)
            time.sleep(0.002)
        return new

    def a_kill_bank():
        mark()
        p = bank_pid()
        if not p:
            return "no bank"
        kill(p)
        return f"kill -9 bank {p}"

    def a_kill_front():
        mark()
        p = front_pid()
        if not p:
            return "no front"
        kill(p)
        return f"kill -9 front {p}"

    def a_world_stop_start():
        s, j = srv.post("/api/world/stop")
        L("  stop", s, str(j)[:120])
        time.sleep(rnd.uniform(1, 5))
        st = srv.status()[1]
        if not (isinstance(st, dict) and st.get("state") != "running"):
            FAIL("a stopped world still reports running", status=str(st)[:200])
        if srv.pids()["bank"]:
            FAIL("a bank is alive in a stopped world", banks=srv.pids()["bank"])
        bad, _ = read_world()
        if bad:
            FAIL("world invariants after stop", bad=bad[:5])
        s, j = srv.post("/api/world/start")
        return f"stop/start {s} {str(j)[:100]}"

    def a_pause():
        srv.post("/api/pause", {"paused": True})
        time.sleep(3)
        g1 = srv.status()[1].get("gameTime")
        time.sleep(3)
        st = srv.status()[1]
        g2 = st.get("gameTime")
        if g1 is not None and g2 is not None and abs(g2 - g1) > 0.5:
            FAIL("game time advanced while paused", g1=g1, g2=g2)
        if not st.get("held"):
            FAIL("a paused world is not held", held=st.get("held"))
        srv.post("/api/pause", {"paused": False})
        time.sleep(3)
        g3 = srv.status()[1].get("gameTime")
        if g3 is not None and g2 is not None and g3 <= g2:
            FAIL("game time did not advance after resume", g2=g2, g3=g3)
        return f"pause/resume {g1} {g2} {g3}"

    def a_flip():
        t = time.time()
        s, j = srv.post("/api/update", {"rollback": True})
        if s != 200:
            return f"flip refused {s} {str(j)[:100]}"
        seq, done = j["seq"], None
        deadline = time.time() + 60
        while time.time() < deadline:
            u = srv.get("/api/update")[1]
            if isinstance(u, dict):
                done = next((e for e in u.get("history", []) if e.get("seq") == seq), None)
                if done:
                    break
            time.sleep(0.2)
        time.sleep(2)
        st = bots.stats(t)
        if st["fails"]:
            FAIL("a flip dropped players", stats=st, outcome=str(done)[:200])
        return f"flip {str(done)[:160]}"

    def a_save():
        t = time.time()
        s, j = srv.post("/api/save")
        if s != 200:
            FAIL("save failed", status=s, body=str(j)[:200])
        return f"save {s} {(time.time() - t) * 1000:.0f} ms"

    def a_config():
        k, v = rnd.choice([("speed.growth", rnd.choice([0.5, 1, 2, 4])), ("gauges.thirstDrain", rnd.choice([0.5, 1, 2])), ("meteo.eventChance", rnd.choice([0, 1, 3])),
                           ("save.autosaveMinutes", rnd.choice([1, 2, 3])), ("world.pauseWhenEmpty", rnd.choice([False, True])), ("limits.maxPlayers", rnd.choice([16, 12, 8])),
                           ("loot.amount", rnd.choice([0.5, 1, 2])), ("speed.machines", rnd.choice([1, 2]))])
        s, _ = srv.post("/api/config", {k: v})
        if k in ("limits.maxPlayers", "world.pauseWhenEmpty"):
            srv.post("/api/config", {"limits.maxPlayers": 16, "world.pauseWhenEmpty": False})  # the bots must fit again
        bad = srv.post("/api/config", {"speed.growth": "fast"})
        if bad[0] != 400:
            FAIL("an invalid setting was accepted", got=bad[0], body=str(bad[1])[:100])
        return f"config {k}={v} -> {s}"

    def a_rules():
        s, _ = srv.post("/api/rules", {"gauges": {"oxygenDrain": rnd.choice([0.5, 1, 2])}, "death": {"dropChance": rnd.choice([10, 50])}})
        bad = srv.post("/api/rules", {"gauges": {"nonsense": 1}})
        if bad[0] != 400:
            FAIL("an invalid rules patch was accepted", got=bad[0])
        return f"rules {s}"

    def a_access():
        n = rnd.randrange(BOTS)
        name_ = f"Load{n}"
        r = [srv.post("/api/ban", {"name": name_})[0]]
        time.sleep(rnd.uniform(1, 6))
        r.append(srv.post("/api/unban", {"value": name_})[0])
        r.append(srv.post("/api/unban", {"value": f"127.0.0.{2 + n}"})[0])  # a direct-IP ban also bans the address
        r.append(srv.post("/api/config", {"access.allowedIPs": ["127.0.0.0/8"]})[0])
        r.append(srv.post("/api/config", {"access.allowedIPs": None})[0])
        try:
            json.load(open(f"{home}/access.json"))
        except FileNotFoundError:
            pass
        except ValueError as e:
            FAIL("access.json is not valid JSON", error=str(e))
        return f"access {r}"

    def a_chat_hook():
        fake["mode"] = rnd.choice(["ok", "ok", "500", "slow"])
        srv.post("/api/config", {"chat.webhooks": [f"{endpoint}/hook"], "chat.secret": secrets.token_hex(8), "chat.commandPrefix": "!", "chat.timeoutSeconds": 2, "chat.retries": 1})
        s = srv.post("/api/chat", {"text": "!hello from chaos"}, timeout=15)[0]
        time.sleep(rnd.uniform(1, 4))
        ev = srv.get("/api/chat/events?after=0")[1]
        srv.post("/api/config", {f"chat.{k}": None for k in ("webhooks", "secret", "commandPrefix", "timeoutSeconds", "retries")})
        return f"chat hook mode={fake['mode']} post={s} events={len(ev.get('events', [])) if isinstance(ev, dict) else ev} hits={fake['hits']}"

    def a_s3():
        fake["mode"] = rnd.choice(["ok", "500", "slow"])
        keys = {"backup.s3Endpoint": endpoint, "backup.s3Bucket": "chaos-bucket", "backup.s3Region": "auto", "backup.s3AccessKey": secrets.token_hex(8), "backup.s3SecretKey": secrets.token_hex(16)}
        r = srv.post("/api/config", keys)[0]
        b = srv.post("/api/backup")[0]
        b2 = srv.post("/api/backup/s3", timeout=60)[0]
        time.sleep(rnd.uniform(0, 3))
        s3 = (srv.status()[1] or {}).get("s3")
        srv.post("/api/config", {k: None for k in keys})
        st = srv.status()[1]
        if isinstance(st, dict) and (st.get("s3") or {}).get("enabled"):
            FAIL("s3 still enabled after unsetting it", s3=st.get("s3"))
        return f"s3 mode={fake['mode']} cfg={r} backup={b} s3={b2} last={str(s3)[:120]}"

    def a_sigstop_bank():
        p = bank_pid()
        if not p:
            return "no bank"
        d = rnd.uniform(3, 9)
        kill(p, signal.SIGSTOP)
        time.sleep(d)
        kill(p, signal.SIGCONT)
        time.sleep(1)
        if bank_pid() != p:
            FAIL("a SIGSTOP of %.1f s replaced the bank" % d, old=p, now=bank_pid())
        return f"SIGSTOP bank {p} {d:.1f}s"

    def a_sigstop_front():
        p = front_pid()
        if not p:
            return "no front"
        d = rnd.uniform(2, 6)
        kill(p, signal.SIGSTOP)
        time.sleep(d)
        kill(p, signal.SIGCONT)
        return f"SIGSTOP front {p} {d:.1f}s"

    def a_freeze_clients():
        d = rnd.uniform(5, 25)
        kill(bots.pid, signal.SIGSTOP)
        time.sleep(d)
        kill(bots.pid, signal.SIGCONT)
        return f"SIGSTOP bots {d:.1f}s"

    def a_ro_world():
        if os.geteuid() == 0:
            return "skipped (root ignores folder permissions)"
        wd = os.path.dirname(srv.world)
        os.chmod(wd, 0o555)
        try:
            s, j = srv.post("/api/save")
            if s == 200:
                FAIL("a save into a read-only folder claimed success", body=str(j)[:100])
            ls = (srv.status()[1] or {}).get("lastSave")
            if not (ls and ls.get("ok") is False):
                FAIL("lastSave does not show the failure", lastSave=ls)
        finally:
            os.chmod(wd, 0o755)
        s2, _ = srv.post("/api/save")
        return f"read-only world folder: save {s} then {s2}"

    def a_kill_bank_during_save():
        for _ in range(3):
            bg("/api/save", timeout=20)
        time.sleep(rnd.uniform(0.0, 0.05))
        return a_kill_bank()

    def a_flip_kill_new_bank():
        mark()
        old = bank_pid()
        flip_async()
        new = new_bank(old)
        if new:
            time.sleep(rnd.uniform(0, 0.4))
            kill(new)
        time.sleep(2)
        return f"flip, kill -9 new bank {new} (old {old})"

    def a_flip_kill_old_bank():
        mark()
        old = bank_pid()
        flip_async()
        time.sleep(rnd.uniform(0.05, 0.6))
        kill(old)
        return f"flip, kill -9 old bank {old}"

    def a_flip_kill_front():
        mark()
        p = front_pid()
        flip_async()
        time.sleep(rnd.uniform(0.0, 0.5))
        kill(p)
        return f"flip, kill -9 front {p}"

    def a_flip_flip():
        a = srv.post("/api/update", {"rollback": True})
        b = srv.post("/api/update", {"rollback": True})
        time.sleep(2)
        return f"two flips at once: {a[0]} {b[0]} {str(b[1])[:80]}"

    def a_flip_stopworld():
        flip_async()
        time.sleep(rnd.uniform(0, 0.1))
        s, _ = srv.post("/api/world/stop")
        time.sleep(1)
        s2, j2 = srv.post("/api/world/start")
        time.sleep(2)
        return f"flip then stop {s} start {s2} {str(j2)[:80]}"

    def a_sigstop_new_bank():
        old = bank_pid()
        flip_async()
        new = new_bank(old)
        if new:
            kill(new, signal.SIGSTOP)
            time.sleep(rnd.uniform(1, 8))
            kill(new, signal.SIGCONT)
        time.sleep(3)
        return f"flip, SIGSTOP new bank {new}"

    def a_stop_start_burst():
        r = []
        for _ in range(rnd.randint(3, 8)):
            r.append(srv.post("/api/world/stop", timeout=30)[0])
            time.sleep(rnd.uniform(0, 0.3))
            r.append(srv.post("/api/world/start", timeout=30)[0])
            time.sleep(rnd.uniform(0, 0.5))
        return f"stop/start burst {r}"

    def a_kill_after_start():
        srv.post("/api/world/stop")
        time.sleep(0.5)
        srv.post("/api/world/start")
        time.sleep(rnd.uniform(0.0, 0.3))
        p = bank_pid()
        if p:
            kill(p)
        return f"stop, start, kill -9 bank {p} soon after"

    def a_kill_both():
        mark()
        fp, bp = front_pid(), bank_pid()
        for p in (fp, bp):
            if p:
                kill(p)
        return f"kill -9 front {fp} and bank {bp}"

    def a_sigterm_bank():
        p = bank_pid()
        if p:
            kill(p, signal.SIGTERM)
        time.sleep(2)
        return f"SIGTERM bank {p} (it ignores it)"

    def a_sigterm_front():
        mark()
        p = front_pid()
        if p:
            kill(p, signal.SIGTERM)
        return f"SIGTERM front {p}"

    def a_save_storm_flip():
        ths = [bg("/api/save", timeout=30) for _ in range(5)]
        if not args.no_flip:
            flip_async()
        for t in ths:
            t.join(35)
        return "5 saves + a flip at once"

    def a_big_chat():
        s = srv.post("/api/chat", {"text": "x" * 5000}, timeout=15)[0]
        return f"5000-char chat {s}"

    def a_status_hammer():
        n, t = 0, time.time()
        while time.time() - t < 3:
            srv.status(timeout=5)
            n += 1
        return f"{n} status calls in 3 s"


    # ---- more actions: watchdog, world switch, a real update, the supervisor ----------------
    def stopped_bank(secs, expect_replaced):
        """SIGSTOP the bank for up to `secs`: limits.bankWatchdogSeconds (60) must replace it when
        it is stopped longer, and a shorter stop must not."""
        mark()
        p = bank_pid()
        if not p:
            return "no bank"
        t0 = time.time()
        kill(p, signal.SIGSTOP)
        new = None
        while time.time() - t0 < secs:
            time.sleep(1 if expect_replaced else 0.5)
            new = next((q for q in srv.pids()["bank"] if q != p), None)
            if new:
                break
        waited = time.time() - t0
        kill(p, signal.SIGCONT)
        time.sleep(4)
        if expect_replaced:
            events["replaced"] += 1 if new else 0
            if not new:
                FAIL("a bank stopped for %d s was not replaced by the watchdog" % secs, bank=p, waited=round(waited))
            elif waited < 50:
                FAIL("the watchdog replaced a bank after %d s (rule: 60)" % waited, bank=p)
            try:
                state = open(f"/proc/{p}/stat").read().rsplit(")", 1)[1].split()[0]
            except FileNotFoundError:
                state = "gone"
            if state not in ("gone", "Z"):
                FAIL("the stopped bank was not killed when it was replaced", bank=p, state=state)
        else:
            if bank_pid() != p:
                FAIL("a stop of %d s replaced the bank (watchdog rule: 60 s)" % secs, old=p, now=bank_pid())
        return f"SIGSTOP bank {p} {waited:.0f} s -> {'replaced by %s' % new if new else 'same bank'}"

    def a_watchdog_long():
        return stopped_bank(125, True)

    def a_watchdog_short():
        return stopped_bank(10, False)

    def a_kill_supervisor():
        """kill -9 of `pcds run` itself (the unit's main process): systemd ends the rest and starts again."""
        if not args.unit:
            return "skipped (no unit)"
        mark()
        pid = int(srv.unit_state().get("MainPID") or srv.sysctl("show", "-p", "MainPID", "--value", f"{srv.unit}.service").stdout.strip() or 0)
        if pid:
            kill(pid)
        return f"kill -9 pcds run {pid}"

    def a_world_switch():
        r = []
        s, j = srv.post("/api/worlds/new", {"name": "alt", "planet": "Prime"}, timeout=90)
        r.append(("new", s))
        s, j = srv.post("/api/world/switch", {"world": "alt"}, timeout=200)
        r.append(("switch", s, str(j)[:80]))
        time.sleep(rnd.uniform(1, 5))
        st = srv.status()[1]
        if s == 200 and not (isinstance(st, dict) and st.get("save") == "alt"):
            FAIL("a switch to alt answered 200 but the world that runs is not alt", save=(st or {}).get("save") if isinstance(st, dict) else st)
        for _ in range(4):
            st = srv.status()[1]
            if isinstance(st, dict) and st.get("save") == "world" and st.get("state") == "running":
                break
            s2, j2 = srv.post("/api/world/switch", {"world": "world"}, timeout=200)
            if s2 != 200:
                srv.post("/api/world/start", {"world": "world"}, timeout=200)
            r.append(("back", s2, str(j2)[:80]))
            time.sleep(1)
        return f"world switch {r}"

    rel_n = {"n": 0}

    def a_update_release():
        """A real update: this build published as a new tag in the local feed; download, install in the standby bank, flip."""
        import hashlib
        import tarfile
        rel_n["n"] += 1
        tag = f"v0.7.0-qa.{rel_n['n']}"
        name = f"pcds-{tag}-linux-x86_64"
        exe = os.path.realpath(args.bin)
        d = os.path.dirname(exe)
        tar = os.path.join(FEED, "download", name + ".tar.gz")
        web = os.path.join(d, "web", "index.html")
        web = web if os.path.exists(web) else os.path.join(lib.REPO, "server", "web", "index.html")
        with tarfile.open(tar, "w:gz", compresslevel=1) as tf:
            for src, arc in ((exe, "pcds"), (os.path.join(d, "libsteam_api.so"), "libsteam_api.so"), (web, "web/index.html")):
                tf.add(src, arcname=f"{name}/{arc}")
        sha = hashlib.sha256(open(tar, "rb").read()).hexdigest()
        open(tar + ".sha256", "w").write(f"{sha}  {name}.tar.gz\n")
        base = f"{endpoint}/download"
        doc = {"tag_name": tag, "assets": [{"name": name + ".tar.gz", "browser_download_url": f"{base}/{name}.tar.gz"}, {"name": name + ".tar.gz.sha256", "browser_download_url": f"{base}/{name}.tar.gz.sha256"}]}
        json.dump(doc, open(os.path.join(FEED, "releases", "tags", tag), "w"))
        shutil.copy(os.path.join(FEED, "releases", "tags", tag), os.path.join(FEED, "releases", "latest"))
        t = time.time()
        mark()
        s, j = srv.post("/api/update", {"version": tag}, timeout=60)
        if s != 200:
            return f"update refused {s} {str(j)[:100]}"
        seq, done = j["seq"], None
        deadline = time.time() + 120
        while time.time() < deadline:
            u = srv.get("/api/update")[1]
            if isinstance(u, dict):
                done = next((e for e in u.get("history", []) if e.get("seq") == seq), None)
                if done:
                    break
            time.sleep(0.3)
        time.sleep(2)
        events["updates"] += 1
        st = bots.stats(t)
        if st["fails"]:
            FAIL("a real update dropped players", stats=st, outcome=str(done)[:200])
        if done and done.get("ok") is False:
            FAIL("a real update failed", tag=tag, outcome=str(done)[:300])
        for old in sorted(glob.glob(os.path.join(FEED, "download", "*.tar.gz")))[:-2]:
            os.remove(old)
        return f"update {tag} {str(done)[:140]}"

    # A module that traps in every event (`unreachable` in pcds_event), written as bytes: the host must count
    # the faults, restart it, switch it off after five in a minute, and go on with everything else.
    def leb(n):
        out = bytearray()
        while True:
            b, n = n & 0x7F, n >> 7
            out.append(b | (0x80 if n else 0))
            if not n:
                return bytes(out)

    def sec(i, body):
        return bytes([i]) + leb(len(body)) + body

    def vec(items):
        return leb(len(items)) + b"".join(items)

    code = lambda body: leb(len(body) + 1) + b"\x00" + body  # no locals
    FAULTY = (b"\x00asm\x01\x00\x00\x00"
              + sec(1, vec([b"\x60\x00\x01\x7f", b"\x60\x01\x7f\x01\x7f", b"\x60\x02\x7f\x7f\x00", b"\x60\x03\x7f\x7f\x7f\x01\x7f"]))
              + sec(3, vec([b"\x00", b"\x01", b"\x02", b"\x03"]))
              + sec(5, vec([b"\x00\x01"]))
              + sec(7, vec([b"\x06memory\x02\x00", b"\x08pcds_abi\x00\x00", b"\x0apcds_alloc\x00\x01", b"\x09pcds_free\x00\x02", b"\x0apcds_event\x00\x03"]))
              + sec(10, vec([code(b"\x41" + leb(65536) + b"\x0b"), code(b"\x41" + leb(4096) + b"\x0b"), code(b"\x0b"), code(b"\x00\x0b")])))
    FAULTY_TOML = b'[plugin]\nid = "faulty"\nname = "faulty"\nversion = "1.0.0"\nkind = "wasm"\nentry = "plugin.wasm"\nabi = 1\ncapabilities = ["event.chat"]\n\n[events]\nsubscribe = ["chat"]\n'
    FAULTY_DIR = os.path.join(home, "plugins", "faulty")
    fp = dict(present=False, faults=0, status=None)

    # ---- plugin churn: hello-stats installed, approved, disabled, removed, damaged, all the time ----
    PL_SRC = os.path.join(lib.REPO, "examples", "plugins", "hello-stats")
    PL_ID = "hello-stats"
    PL_DIR = os.path.join(home, "plugins", PL_ID)
    pl = dict(present=False, pristine=True, expect="absent", ops=0, faults=0, seen={})
    pl_lock = threading.Lock()
    pl_stop = threading.Event()
    prnd = random.Random(args.seed + 1000)
    wasm0, toml0 = open(os.path.join(PL_SRC, "plugin.wasm"), "rb").read(), open(os.path.join(PL_SRC, "plugin.toml"), "rb").read()

    def pl_write(path, data):
        tmp = path + ".part"
        open(tmp, "wb").write(data)
        os.replace(tmp, path)

    def pl_put(wasm, toml):
        stage = os.path.join(home, ".pl-stage")
        shutil.rmtree(stage, ignore_errors=True)
        os.makedirs(stage)
        open(os.path.join(stage, "plugin.wasm"), "wb").write(wasm)
        open(os.path.join(stage, "plugin.toml"), "wb").write(toml)
        os.makedirs(os.path.dirname(PL_DIR), exist_ok=True)
        shutil.rmtree(PL_DIR, ignore_errors=True)
        os.rename(stage, PL_DIR)

    def pl_op():
        r = prnd.choices(["install", "enable", "disable", "remove", "mutate_wasm", "mutate_toml", "restore", "cmd", "reload", "fault"], [3, 6, 3, 2, 3, 1, 3, 4, 1, 0 if args.no_fault else 4])[0]
        with pl_lock:
            pl["ops"] += 1
            L("  plugin op", r)
            if r == "install":
                pl_put(wasm0, toml0)
                pl.update(present=True, pristine=True, expect=None)
            elif r == "enable":
                s, _ = srv.post("/api/plugins/enable", {"id": PL_ID}, timeout=30)
                if s == 200 and pl["present"] and pl["pristine"]:
                    pl["expect"] = "running"
                r += f" {s}"
            elif r == "disable":
                s, _ = srv.post("/api/plugins/disable", {"id": PL_ID}, timeout=30)
                if s == 200 and pl["present"] and pl["pristine"]:  # a damaged manifest stays "invalid", disabled or not
                    pl["expect"] = "disabled"
                r += f" {s}"
            elif r == "remove":
                for _ in range(10):  # as an admin does: rm -rf again when the plugin wrote its state meanwhile
                    shutil.rmtree(PL_DIR, ignore_errors=True)
                    if not os.path.exists(PL_DIR):
                        break
                    time.sleep(0.2)
                pl.update(present=False, expect="absent")
            elif r == "mutate_wasm" and pl["present"]:
                w = bytearray(open(os.path.join(PL_DIR, "plugin.wasm"), "rb").read())
                for _ in range(prnd.choice([1, 1, 3])):
                    w[prnd.randrange(8, len(w))] ^= 1 << prnd.randrange(8)
                pl_write(os.path.join(PL_DIR, "plugin.wasm"), bytes(w))
                pl.update(pristine=False, expect=None)
            elif r == "mutate_toml" and pl["present"]:
                pl_write(os.path.join(PL_DIR, "plugin.toml"), prnd.choice([toml0[: len(toml0) // 2], b"\x00\xff[[[plugin", toml0.replace(b"abi = 1", b"abi = 99"), toml0.replace(b"world.read", b"world.write")]))
                pl.update(pristine=False, expect=None)
            elif r == "restore" and pl["present"]:
                pl_write(os.path.join(PL_DIR, "plugin.wasm"), wasm0)
                pl_write(os.path.join(PL_DIR, "plugin.toml"), toml0)
                pl.update(pristine=True, expect=None)
            elif r == "cmd":
                srv.post("/api/chat", {"text": prnd.choice(["!stats", "!uses", "!stats extra", "!nosuch"])}, timeout=10)
            elif r == "reload":
                srv.post("/api/plugins/reload", timeout=30)
            elif r == "fault":
                # the faulting plugin: put, approve, give it chat to trap on (or take it away again)
                if not fp["present"] or prnd.random() < 0.15:
                    shutil.rmtree(FAULTY_DIR, ignore_errors=True)
                    stage = os.path.join(home, ".pl-stage-faulty")
                    shutil.rmtree(stage, ignore_errors=True)
                    os.makedirs(stage)
                    open(os.path.join(stage, "plugin.wasm"), "wb").write(FAULTY)
                    open(os.path.join(stage, "plugin.toml"), "wb").write(FAULTY_TOML)
                    os.makedirs(os.path.dirname(FAULTY_DIR), exist_ok=True)
                    os.rename(stage, FAULTY_DIR)
                    fp["present"] = True
                    time.sleep(2.5)
                    srv.post("/api/plugins/enable", {"id": "faulty"}, timeout=30)
                    r += " (installed)"
                for _ in range(prnd.randint(1, 8)):
                    srv.post("/api/chat", {"text": "trap"}, timeout=10)
                    time.sleep(0.2)
        return r

    def pl_thread():
        while not pl_stop.wait(prnd.uniform(1.5, 7)):
            try:
                pl_op()
            except Exception as e:  # a file operation racing the server's own scan: the check afterwards decides
                L("  plugin op raised", repr(e))

    def pl_verify(label):
        """What the churn did against what the server says, with the churn held (pl_lock)."""
        if args.no_plugins:
            return
        with pl_lock:
            for _ in range(6):
                if srv.post("/api/plugins/reload", timeout=30)[0] == 200:
                    break
                time.sleep(1)
            s, j = srv.get("/api/plugins")
            if s != 200 or not isinstance(j, dict):
                FAIL("GET /api/plugins failed", label=label, status=s, body=str(j)[:100])
                return
            mine = next((p for p in j["plugins"] if p["id"] == PL_ID), None)
            summ = (srv.status()[1] or {}).get("plugins") if isinstance(srv.status()[1], dict) else None
            if summ and summ.get("loaded") != len(j["plugins"]):
                FAIL("the status counts other plugins than the list shows", label=label, summary=summ, listed=len(j["plugins"]))
            if not pl["present"] and mine is not None:
                FAIL("a removed plugin is still listed", label=label, status=mine["status"])
            elif pl["present"] and mine is None:
                FAIL("an installed plugin is not listed", label=label)
            elif mine and pl["expect"] == "running" and mine["status"] != "running":
                FAIL("an approved plugin does not run", label=label, status=mine["status"], detail=mine.get("detail"), stats=mine["stats"])
            elif mine and pl["expect"] == "disabled" and mine["status"] != "disabled":
                FAIL("a disabled plugin is not disabled", label=label, status=mine["status"])
            elif mine and mine["status"] not in ("running", "pending", "disabled", "changed", "invalid", "incompatible", "failed"):
                FAIL("an unknown plugin status", label=label, status=mine["status"])
            bad = next((p for p in j["plugins"] if p["id"] == "faulty"), None)
            if bad:
                fp["faults"] = max(fp["faults"], bad["stats"]["faults"])
                fp["status"] = bad["status"]
                fp["seen"] = fp.get("seen", {})
                fp["seen"][bad["status"]] = fp["seen"].get(bad["status"], 0) + 1
                if bad["status"] not in ("running", "failed", "pending", "disabled", "changed"):
                    FAIL("the faulting plugin is in an unknown state", label=label, status=bad["status"], detail=bad.get("detail"))
            seen = mine["status"] if mine else "absent"
            pl["seen"][seen] = pl["seen"].get(seen, 0) + 1
            if mine:
                pl["faults"] = max(pl["faults"], mine["stats"]["faults"])

    # (function, weight)
    table = [(a_kill_bank, 10), (a_kill_front, 8), (a_world_stop_start, 6), (a_pause, 4), (a_flip, 10), (a_save, 6), (a_config, 8), (a_rules, 6),
             (a_access, 6), (a_chat_hook, 5), (a_s3, 4), (a_sigstop_bank, 5), (a_sigstop_front, 3), (a_freeze_clients, 4), (a_ro_world, 3), (a_kill_bank_during_save, 4),
             (a_flip_kill_new_bank, 5), (a_flip_kill_old_bank, 4), (a_flip_kill_front, 3), (a_flip_flip, 3), (a_flip_stopworld, 3), (a_sigstop_new_bank, 3),
             (a_stop_start_burst, 4), (a_kill_after_start, 3), (a_kill_both, 3), (a_sigterm_bank, 2), (a_sigterm_front, 3), (a_save_storm_flip, 3), (a_big_chat, 1),
             (a_status_hammer, 1), (a_world_switch, 3), (a_update_release, 4), (a_watchdog_long, 1), (a_watchdog_short, 2), (a_kill_supervisor, 3 if args.unit else 0)]
    table = [x for x in table if x[1] > 0]
    if args.no_flip:
        table = [t for t in table if not t[0].__name__.startswith("a_flip") and t[0] not in (a_sigstop_new_bank,)]
    if args.only:
        want = {"a_" + w.strip() for w in args.only.split(",")}
        table = [t for t in table if t[0].__name__ in want]
        if not table:
            sys.exit("--only: no such action")

    check("initial")
    time.sleep(10)
    if not args.no_plugins:
        threading.Thread(target=pl_thread, daemon=True).start()
    for i in range(args.actions):
        fn = rnd.choices([t[0] for t in table], weights=[t[1] for t in table])[0]
        t = time.time()
        try:
            r = fn()
        except Exception as e:
            r = f"EXC {e!r}"
            FAIL("an action raised", action=fn.__name__, error=repr(e))
        L(f"#{i} {fn.__name__}: {r}")
        check(f"#{i} {fn.__name__}", since=t)
        time.sleep(rnd.uniform(0.3, 3))

    pl_stop.set()
    L("bots", bots.stats(), "plugin ops", pl["ops"], "plugin faults seen", pl["faults"], "events", events)
    bots.stop()
    settle()
    srv.post("/api/save")
    bad, counts = read_world()
    L("final world", counts, bad[:5])
    if bad:
        FAIL("final world invariants", bad=bad[:5])
    srv.stop()
    left = srv.pids()
    if left["serve"] or left["bank"]:
        FAIL("processes left after the stop", pids=left)
    if srv.final_rc() != 0:
        FAIL("the final stop did not exit 0", rc=srv.final_rc())
    L("DONE actions", args.actions, "failures", len(verdict.failures))
    verdict.exit(name=name, actions=args.actions, events=events, plugin_ops=pl["ops"], plugin_faults=pl["faults"], plugin_states_seen=pl["seen"], faulty=fp, mode="unit " + args.unit if args.unit else "serve")


if __name__ == "__main__":
    main()
