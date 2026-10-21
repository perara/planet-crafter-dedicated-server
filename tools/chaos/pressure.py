#!/usr/bin/env python3
"""Resource pressure: the server under a squeezed CPU, a slow disk, little memory, few descriptors,
many worlds, hooks and S3 that fail for a long time, and a frozen front.

Purpose: longsoak.py shows what happens over hours with all the resources there; this takes one resource
away at a time (with the cgroup of the unit: CPUQuota, AllowedCPUs, IOWriteBandwidthMax, MemoryHigh,
LimitNOFILE) and says what the server does about it: does the tick stay alive, does a save or a backup stall
the loop, does something grow without bound, does the log fill, does it recover.

Each scenario starts its own server as a transient user unit (lib.UnitSrv, the unit must be named pcds-qa-*)
on a copy of a save, with 'pcds load' players, samples the status every 5 s into <work>/<scenario>.csv and
prints one JSON line with the numbers and the failed checks. Exit status 0: every scenario passed its checks
(the numbers are for you to read: a loaded host stretches them, and they are marked so).

Scenarios (--only a,b to choose; --minutes sets the length of each phase):
  cpu       32 players on a free server, then on CPUQuota=100% (one core), then on 2 cores (AllowedCPUs); --cpu-runs adds a quota of 12 % and of 6 % of a core
  slowdisk  8 players, autosave and backup every minute, writes limited to --io (default 512K) a second, then reads too
  memhigh   8 players under MemoryHigh (--memhigh, default 128M and 96M) with no swap: the reclaim of a small host
  fds       100 players and 300 idle admin connections under LimitNOFILE=256
  worlds    20 small worlds, 200 switches between them (--switches), the memory, threads and descriptors after
  chat      16 players chatting 1500-character lines while the chat hook answers 500, hangs, and drops the connection
  s3        backups every minute while S3 answers 500, hangs and refuses, then works again
  freeze    SIGSTOP of the front for 60 s with 8 players, then CONT
  future    backups stamped in the future (the clock stepped back) do not stop the pruning of the real ones

Requirements: python3 (3.9+), systemd --user, a built `pcds` and the cargo test binary (bots), the tables, a save.
Ports: --port (default 8644): a scenario takes four (UDP, admin, the fake services, spare) at an offset of 0 or 4,
so two runs of this tool with --port 8644 and --port 8652 can run side by side; the scenarios of one run go one after the other.

Run:  tools/chaos/pressure.py --bin PCDS --data TABLES --world SAVE --work DIR --test-bin TESTBIN [--only cpu,fds]
"""
import argparse
import json
import os
import random
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import lib  # noqa: E402
from longsoak import Fake  # noqa: E402


class Run:
    """One scenario's server, its sampler and its verdict."""

    def __init__(self, args, name, port, rules=None, props=(), cpus="8-15", env=None, world=None, start_wait=90):
        self.args, self.name, self.port = args, name, port
        self.log = lib.Log(os.path.join(args.work, f"{name}.events"))
        self.verdict = lib.Verdict(name, self.log)
        self.fail = self.verdict.fail
        self.fake = Fake(port + 2, os.path.join(args.work, "nofeed"))
        self.endpoint = f"http://{lib.LOOPBACK}:{port + 2}"
        base = {"save": {"autosaveMinutes": 1, "backupsKept": 6, "backupMinutes": 5}, "limits": {"maxPlayers": 40}, "world": {"dlcPlanets": ["Humble"]}}
        self.srv = lib.UnitSrv(name, args, f"pcds-qa-soak2-{name}", port=port, rules=lib.merge(base, rules or {}), env=env, cpus=cpus, props=props, world=world)
        t_start = time.time()
        self.srv.start(wait=start_wait)
        self.start_seconds = round(time.time() - t_start, 1)
        self.t0 = time.time()
        self.rows = []
        self.csv = open(os.path.join(args.work, f"{name}.csv"), "w")
        self.cols = "t,f_rss,f_thr,f_fds,f_cpu_pct,b_rss,b_thr,b_fds,b_cpu_pct,busy,longest_us,tick_p99_us,tick_max_us,players,wo,st_ms,st_ok,log_lines,unit_mem,unit_anon,phase".split(",")
        self.csv.write(",".join(self.cols) + "\n")
        self.phase = "start"
        self._stop = threading.Event()
        self.prev = {"f": (0, 0, 0), "b": (0, 0, 0)}
        self.lines = {"off": 0, "n": 0}
        self.worst = {"longest": 0}
        cg = subprocess.run(["systemctl", "--user", "show", "-p", "ControlGroup", "--value", f"{self.srv.unit}.service"], capture_output=True, text=True).stdout.strip()
        self.cg = "/sys/fs/cgroup" + cg if cg else ""
        self.th = threading.Thread(target=self._sample, daemon=True)
        self.th.start()

    def _cg(self):
        try:
            anon = int([l for l in open(self.cg + "/memory.stat") if l.startswith("anon ")][0].split()[1])
            return anon, int(open(self.cg + "/memory.current").read())
        except Exception:
            return -1, -1

    def _sample(self):
        every = 5
        nxt = time.time()
        while not self._stop.is_set():
            nxt += every
            t1 = time.time()
            s, j = self.srv.status(timeout=20)
            ms = (time.time() - t1) * 1000
            ok = s == 200 and isinstance(j, dict)
            j = j if ok else {}
            bpid = ((j.get("banks") or {}).get("live") or {}).get("pid") or 0
            fpid = (self.srv.pids()["serve"] or [0])[0]
            f, b = lib.proc(fpid), lib.proc(bpid)
            now = time.time()

            def pct(key, pid, d):
                p = self.prev[key]
                r = round(100 * (d["cpu"] - p[1]) / (now - p[2]), 1) if d and p[0] == pid and now > p[2] else -1
                self.prev[key] = (pid, d["cpu"] if d else 0, now)
                return r

            try:
                with open(self.srv.log, "rb") as fh:
                    fh.seek(self.lines["off"])
                    data = fh.read()
                    self.lines["off"] += len(data)
                    self.lines["n"] += data.count(b"\n")
            except OSError:
                pass
            anon, mem = self._cg()
            z = lambda d, k: d[k] if d else -1
            tk = j.get("tick") or {}
            row = dict(t=round(now - self.t0), f_rss=z(f, "rss"), f_thr=z(f, "thr"), f_fds=z(f, "fds"), f_cpu_pct=pct("f", fpid, f), b_rss=z(b, "rss"), b_thr=z(b, "thr"), b_fds=z(b, "fds"), b_cpu_pct=pct("b", bpid, b),
                       busy=j.get("busyPercent", -1), longest_us=j.get("longestLoopMicros", -1), tick_p99_us=tk.get("p99Micros", -1), tick_max_us=tk.get("maxMicros", -1), players=len(j.get("players", [])),
                       wo=(j.get("objects") or {}).get("world", -1), st_ms=round(ms), st_ok=int(ok), log_lines=self.lines["n"], unit_mem=mem, unit_anon=anon, phase=self.phase)
            self.rows.append(row)
            self.csv.write(",".join(str(row[c]) for c in self.cols) + "\n")
            self.csv.flush()
            self._stop.wait(max(0.0, nxt - time.time()))

    def window(self, phase):
        """The rows of a phase, summarised."""
        rs = [r for r in self.rows if r["phase"] == phase]
        if not rs:
            return {}
        g = lambda k, ok=lambda v: v >= 0: [r[k] for r in rs if ok(r[k])]
        q = lambda xs, p: sorted(xs)[min(len(xs) - 1, int(p * len(xs)))] if xs else None
        out = dict(phase=phase, samples=len(rs), unanswered=sum(1 for r in rs if not r["st_ok"]),
                   status_ms_p50=q(g("st_ms"), .5), status_ms_max=max(g("st_ms")) if g("st_ms") else None,
                   busy_p50=q(g("busy"), .5), busy_max=max(g("busy")) if g("busy") else None,
                   longest_ms_p50=round(q(g("longest_us"), .5) / 1000, 1) if g("longest_us") else None, longest_ms_p95=round(q(g("longest_us"), .95) / 1000, 1) if g("longest_us") else None,
                   longest_ms_max=round(max(g("longest_us")) / 1000, 1) if g("longest_us") else None,
                   tick_p99_ms_p50=round(q(g("tick_p99_us"), .5) / 1000, 2) if g("tick_p99_us") else None, tick_max_ms_max=round(max(g("tick_max_us")) / 1000, 1) if g("tick_max_us") else None,
                   players_min=min(g("players") or [0]), players_max=max(g("players") or [0]),
                   f_cpu_pct_p50=q(g("f_cpu_pct"), .5), b_cpu_pct_p50=q(g("b_cpu_pct"), .5), f_rss_mb=round(max(g("f_rss") or [0]) / 1024, 1), b_rss_mb=round(max(g("b_rss") or [0]) / 1024, 1),
                   f_thr_max=max(g("f_thr") or [0]), f_fds_max=max(g("f_fds") or [0]), unit_mem_mb=round(max(g("unit_mem") or [0]) / 1048576, 1),
                   log_lines=rs[-1]["log_lines"] - rs[0]["log_lines"])
        return out

    def bots(self, players, seconds, name=None, action_ms=200, pad=0):
        env = dict(os.environ)
        if pad:
            env["PCDS_LOAD_CHAT_BYTES"] = str(pad)
        a = self.args
        return subprocess.Popen([a.bin, "load", f"{lib.LOOPBACK}:{self.port}", a.data, str(players), str(int(seconds)), str(action_ms)] + ([name] if name else []), stdout=open(os.path.join(a.work, f"{self.name}-load.out"), "ab"), stderr=subprocess.STDOUT, env=env)

    def wait_players(self, n, timeout=120):
        end = time.time() + timeout
        k = -1
        while time.time() < end:
            s, j = self.srv.status()
            k = len(j.get("players", [])) if s == 200 and isinstance(j, dict) else -1
            if k >= n:
                return k
            time.sleep(2)
        return k

    def phase_run(self, phase, seconds):
        self.phase = phase
        self.log("phase", phase, seconds, "s")
        time.sleep(seconds)

    def finish(self, **extra):
        self._stop.set()
        self.th.join(10)
        text = self.srv.logtext()
        bad_save = None
        try:
            self.srv.post("/api/save", timeout=300)
            shutil.copy(self.srv.world, os.path.join(self.srv.home, "check.json"))
            bad, counts = lib.check_save(os.path.join(self.srv.home, "check.json"))
            bad_save = bad
        except Exception as e:
            bad_save = [repr(e)]
        if bad_save and set(bad_save) - set(self.base_bad):
            self.fail("world invariants", bad=list(set(bad_save) - set(self.base_bad))[:5])
        if "panicked" in text:
            self.fail("a panic in the server log", log=self.srv.log)
        self.srv.stop()
        self.fake.httpd.shutdown()
        self.fake.httpd.server_close()
        rc = self.srv.final_rc()
        shutil.copy(self.srv.log, os.path.join(self.args.work, f"{self.name}.front.log"))
        top = {}
        for line in text.splitlines():
            key = "".join("#" if c.isdigit() else c for c in line)[:90]
            top[key] = top.get(key, 0) + 1
        topn = sorted(top.items(), key=lambda kv: -kv[1])[:5]
        print(json.dumps({"scenario": self.name, "pass": not self.verdict.failures, "failures": self.verdict.failures, "stop_rc": rc, "log_lines": len(text.splitlines()), "log_top": topn, **extra}, default=str), flush=True)
        return not self.verdict.failures

    base_bad = ()


def scenario_cpu(args):
    ok = True
    every = {"free": ((), "8-15"), "quota1": (("CPUQuota=100%",), "8-15"), "two": ((), "20-21"),
             # a quota under what the bank needs (it uses about 10 % of a core with 32 players): the loop cannot keep up
             "tight12": (("CPUQuota=12%",), "8-15"), "tight6": (("CPUQuota=6%",), "8-15")}
    for tag, (props, cpus) in [(k, every[k]) for k in args.cpu_runs.split(",")]:
        r = Run(args, f"cpu-{tag}", args.port, props=props, cpus=cpus)
        r.base_bad = lib.check_save(r.srv.world)[0]
        p = r.bots(32, args.minutes * 60 + 60)
        n = r.wait_players(32, 120)
        r.log("players", n)
        if n < 32:
            r.fail("32 players did not get in", players=n)
        r.phase_run("play", args.minutes * 60)
        rc = p.poll()
        w = r.window("play")
        if w.get("unanswered", 0) > w.get("samples", 1) * 0.1:
            r.fail("the status did not answer", **w)
        if p.poll() is not None and p.returncode:
            r.fail("players failed", rc=p.returncode)
        p.terminate()
        ok &= r.finish(window=w, bots_rc=rc)
        time.sleep(5)
    return ok


def scenario_slowdisk(args):
    """Writes limited, then reads limited as well, then free: the same server, the same players."""
    dev = args.device
    r = Run(args, "slowdisk", args.port, props=(f"IOWriteBandwidthMax={dev} {args.io}",), rules={"save": {"autosaveMinutes": 1, "backupsKept": 6, "backupMinutes": 1}})
    r.base_bad = lib.check_save(r.srv.world)[0]
    p = r.bots(8, args.minutes * 60 * 3 + 120)
    r.wait_players(8, 120)
    prop = lambda *kv: subprocess.run(["systemctl", "--user", "set-property", "--runtime", f"{r.srv.unit}.service", *kv], capture_output=True)
    r.phase_run("write-slow", args.minutes * 60)
    # Reads as well: the pages of the program the loop touches for the first time in a while are read at that speed.
    prop(f"IOReadBandwidthMax={dev} {args.io}")
    r.phase_run("read-slow", args.minutes * 60)
    prop("IOWriteBandwidthMax=", "IOReadBandwidthMax=")
    r.phase_run("free", args.minutes * 60)
    s, j = r.srv.status()
    last = (j or {}).get("lastSave") if isinstance(j, dict) else None
    r.log("lastSave", json.dumps(last))
    w1, w2, w3 = r.window("write-slow"), r.window("read-slow"), r.window("free")
    if w1.get("longest_ms_p95") and w3.get("longest_ms_p95") and w1["longest_ms_p95"] > max(150, 3 * w3["longest_ms_p95"]):
        r.fail("a disk that is slow to write reaches the loop", slow_p95_ms=w1["longest_ms_p95"], free_p95_ms=w3["longest_ms_p95"])
    p.terminate()
    return r.finish(write_slow=w1, read_slow=w2, free=w3, lastSave=last)


def scenario_memhigh(args):
    """The unit's memory.high below what the server wants and no swap: the kernel makes every allocation over it wait."""
    ok = True
    for high in args.memhigh.split(","):
        r = Run(args, f"memhigh-{high}", args.port, props=(f"MemoryHigh={high}", "MemorySwapMax=0"), start_wait=600)
        r.base_bad = lib.check_save(r.srv.world)[0]
        r.log("started after", round(time.time() - r.t0), "s")
        p = r.bots(8, args.minutes * 60 + 300)
        n = r.wait_players(8, 300)
        if n < 8:
            r.fail("8 players did not get in under MemoryHigh", players=n)
        r.phase_run("play", args.minutes * 60)
        ev = ""
        try:
            ev = open(r.cg + "/memory.events").read().replace("\n", " ")
        except Exception:
            pass
        w = r.window("play")
        p.terminate()
        ok &= r.finish(window=w, memory_events=ev, start_seconds=r.start_seconds)
        time.sleep(5)
    return ok


def scenario_fds(args):
    r = Run(args, "fds", args.port + 4, props=("LimitNOFILE=256",), rules={"limits": {"maxPlayers": 100}})
    r.base_bad = lib.check_save(r.srv.world)[0]
    p = r.bots(100, args.minutes * 60 + 180)
    n = r.wait_players(100, 240)
    r.log("players", n)
    if n < 100:
        r.fail("100 players did not get in", players=n)
    r.phase_run("play", args.minutes * 30)
    held = []
    opened = 0
    for _ in range(300):
        try:
            s = socket.create_connection((lib.LOOPBACK, r.srv.admin), timeout=2)
            held.append(s)
            opened += 1
        except OSError:
            break
    r.log("idle admin connections held", opened)
    r.phase = "idle-flood"
    answered = [r.srv.status(timeout=10)[0] for _ in range(5)]
    r.log("status answers while flooded", answered)
    time.sleep(60)
    for s in held:
        s.close()
    time.sleep(10)
    r.phase = "after"
    s, j = r.srv.status(timeout=20)
    if s != 200:
        r.fail("the status did not answer after the flood closed", status=s)
    r.phase_run("after", args.minutes * 30)
    w = r.window("play"), r.window("idle-flood"), r.window("after")
    if w[2].get("log_lines", 0) > 2000:
        r.fail("the log filled while the descriptors were gone", lines=w[2]["log_lines"])
    p.terminate()
    return r.finish(opened=opened, status_during=answered, play=w[0], flood=w[1], after=w[2])


def scenario_worlds(args):
    r = Run(args, "worlds", args.port)
    r.base_bad = lib.check_save(r.srv.world)[0]
    names = []
    small = args.small_world
    for i in range(20):
        n = f"w{i:02d}"
        shutil.copy(small, os.path.join(r.srv.home, "worlds", n + ".json"))
        names.append(n)
    rnd = random.Random(7)
    times, fails = [], 0
    f0 = lib.proc((r.srv.pids()["serve"] or [0])[0])
    r.phase = "switch"
    cur = None
    for i in range(args.switches):
        n = rnd.choice([x for x in names if x != cur])
        t = time.time()
        s, j = r.srv.post("/api/world/switch", {"world": n}, timeout=120)
        ok = False
        end = time.time() + 60
        while time.time() < end and s == 200:
            st = r.srv.status(timeout=20)[1]
            if isinstance(st, dict) and st.get("state") == "running" and st.get("save") == n:
                ok = True
                break
            time.sleep(0.2)
        times.append(time.time() - t)
        if not ok:
            fails += 1
            r.log("switch", i, n, "failed", s, str(j)[:120])
        else:
            cur = n
        if i % 20 == 0:
            r.log("switch", i, f"{times[-1]:.1f}s")
    f1 = lib.proc((r.srv.pids()["serve"] or [0])[0])
    r.phase = "after"
    # A player gets in on the last world.
    p = r.bots(4, 30)
    n = r.wait_players(4, 60)
    p.terminate()
    if fails:
        r.fail("switches that did not end in the world asked for", n=fails, of=args.switches)
    if n < 4:
        r.fail("players could not join after the switches", players=n)
    third = max(1, len(times) // 3)
    if f0 and f1 and f1["rss"] > f0["rss"] * 1.5 + 20000:
        r.fail("the front grew over the switches", before_kb=f0["rss"], after_kb=f1["rss"])
    stray = [x for x in os.listdir(os.path.join(r.srv.home, "worlds")) if x.endswith(".tmp")]
    if stray:
        r.fail("stray .tmp files in worlds/", files=stray[:5])
    return r.finish(switches=args.switches, failed=fails, seconds_first_third=round(statistics.median(times[:third]), 2), seconds_last_third=round(statistics.median(times[-third:]), 2), seconds_max=round(max(times), 2),
                    front_before_kb=f0 and f0["rss"], front_after_kb=f1 and f1["rss"], front_thr=(f0 and f0["thr"], f1 and f1["thr"]), front_fds=(f0 and f0["fds"], f1 and f1["fds"]), pids_left=r.srv.pids())


def scenario_chat(args):
    r = Run(args, "chat", args.port + 4)
    r.base_bad = lib.check_save(r.srv.world)[0]
    r.srv.post("/api/config", {"chat.webhooks": [f"{r.endpoint}/chat"], "chat.secret": "x" * 16, "chat.commandPrefix": "!", "chat.timeoutSeconds": 3, "chat.retries": 2})
    p = r.bots(16, args.minutes * 60 * 4 + 180, pad=1500)
    r.wait_players(16, 120)
    out = {}
    for mode in ("ok", "500", "slow", "drop", "ok"):
        r.fake.modes["chat"] = mode
        r.phase_run(f"hook-{mode}-{len(out)}", args.minutes * 60)
        ev = r.srv.get("/api/chat/events?after=0")[1]
        out[f"{mode}-{len(out)}"] = dict(hits=r.fake.hits["chat"], dropped=(ev or {}).get("dropped") if isinstance(ev, dict) else None)
    for ph in [x for x in {row["phase"] for row in r.rows} if x.startswith("hook-")]:
        w = r.window(ph)
        if w.get("unanswered", 0) > 3:
            r.fail("the status did not answer during a hook failure", phase=ph)
    shutil.copy(r.srv.world, os.path.join(r.srv.home, "c.json"))
    msgs = len(lib.parse_save(os.path.join(r.srv.home, "c.json"))["messages"])
    p.terminate()
    return r.finish(hook=out, windows={ph: r.window(ph) for ph in sorted({row["phase"] for row in r.rows}) if ph.startswith("hook-")}, saved_messages=msgs)


def scenario_s3(args):
    r = Run(args, "s3", args.port, rules={"save": {"autosaveMinutes": 1, "backupsKept": 4, "backupMinutes": 1}})
    r.base_bad = lib.check_save(r.srv.world)[0]
    keys = {"backup.s3Endpoint": r.endpoint, "backup.s3Bucket": "pressure", "backup.s3Region": "auto", "backup.s3AccessKey": "k" * 8, "backup.s3SecretKey": "s" * 16}
    r.srv.post("/api/config", keys)
    p = r.bots(8, args.minutes * 60 * 5 + 180)
    r.wait_players(8, 120)
    out = {}
    for mode in ("ok", "500", "slow", "drop", "ok"):
        r.fake.modes["s3"] = mode
        h0 = r.fake.hits["s3"]
        r.phase_run(f"s3-{mode}-{len(out)}", args.minutes * 60)
        s3 = (r.srv.status()[1] or {}).get("s3")
        out[f"{mode}-{len(out)}"] = dict(attempts=r.fake.hits["s3"] - h0, status=(s3 or {}).get("last"))
    p.terminate()
    bk = os.listdir(os.path.join(r.srv.home, "worlds", "backups"))
    if len(bk) > 6:
        r.fail("more backups than the rule keeps", n=len(bk))
    if not out["ok-4"]["status"] or not out["ok-4"]["status"].get("ok"):
        r.fail("S3 did not recover after the outage", last=out["ok-4"])
    return r.finish(phases=out, backups_on_disk=len(bk), windows={ph: r.window(ph) for ph in sorted({row["phase"] for row in r.rows}) if ph.startswith("s3-")}, s3bytes=r.fake.hits["s3bytes"])


def scenario_freeze(args):
    r = Run(args, "freeze", args.port + 4)
    r.base_bad = lib.check_save(r.srv.world)[0]
    bots = lib.Bots("rejoin", args, f"{lib.LOOPBACK}:{r.port}", players=8, seconds=args.minutes * 60 * 3 + 300)
    n = r.wait_players(8, 120)
    r.phase_run("before", args.minutes * 60 // 2)
    fpid = r.srv.pids()["serve"][0]
    bank = r.srv.status()[1]["banks"]["live"]["pid"]
    os.kill(fpid, signal.SIGSTOP)
    r.phase = "frozen"
    r.log("SIGSTOP front", fpid)
    time.sleep(60)
    os.kill(fpid, signal.SIGCONT)
    t = time.time()
    r.phase = "after"
    back = r.wait_players(8, 180)
    r.log("players back", back, f"{time.time() - t:.1f}s")
    bank2 = r.srv.status()[1]["banks"]["live"]["pid"]
    if bank2 != bank:
        r.fail("the bank was replaced by a front that was frozen", before=bank, after=bank2)
    if back < 8:
        r.fail("the players did not come back after a frozen front", players=back)
    r.phase_run("after", args.minutes * 60)
    bots.stop()
    return r.finish(back_players=back, back_seconds=round(time.time() - t, 1), before=r.window("before"), frozen=r.window("frozen"), after=r.window("after"))


def scenario_future(args):
    r = Run(args, "future", args.port, rules={"save": {"autosaveMinutes": 1, "backupsKept": 3, "backupMinutes": 1}})
    r.base_bad = lib.check_save(r.srv.world)[0]
    bdir = os.path.join(r.srv.home, "worlds", "backups")
    os.makedirs(bdir, exist_ok=True)
    # Backups stamped a year ahead, as after a clock that stepped back; the world's name is "world".
    for i in range(5):
        shutil.copy(r.srv.world, os.path.join(bdir, f"world-2099010{i + 1}-120000.json"))
    r.phase_run("run", 5 * 60 + 30)
    names = sorted(os.listdir(bdir))
    r.log("backups", names)
    real = [n for n in names if not n.startswith("world-2099")]
    future = [n for n in names if n.startswith("world-2099")]
    if len(real) < 2:
        r.fail("the real backups were not made or were pruned away", names=names)
    if len(names) > 3 + 0:
        r.fail("more than the rule keeps", names=names)
    return r.finish(backups=names, real=len(real), future=len(future))


SCEN = dict(cpu=scenario_cpu, slowdisk=scenario_slowdisk, memhigh=scenario_memhigh, fds=scenario_fds, worlds=scenario_worlds, chat=scenario_chat, s3=scenario_s3, freeze=scenario_freeze, future=scenario_future)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter, epilog="See the header of this file for requirements and what a pass is.")
    lib.add_server_args(ap, port=8644)
    ap.add_argument("--only", default=",".join(SCEN), help="scenarios, comma-separated (default: all)")
    ap.add_argument("--minutes", type=float, default=10, help="the length of a phase (default %(default)s)")
    ap.add_argument("--io", default="512K", help="slowdisk: bytes a second the unit may write and read (default %(default)s)")
    ap.add_argument("--device", default="/dev/nvme0n1", help="slowdisk: the disk the work folder is on (default %(default)s)")
    ap.add_argument("--memhigh", default="128M,96M", help="memhigh: MemoryHigh of the unit, one run each (default %(default)s)")
    ap.add_argument("--cpu-runs", default="free,quota1,two", help="cpu: which runs: free, quota1 (one core), two (two cores), tight12 and tight6 (a CPUQuota of 12%% and of 6%% of a core) (default %(default)s)")
    ap.add_argument("--switches", type=int, default=200, help="worlds: how many switches (default %(default)s)")
    ap.add_argument("--small-world", default="", help="worlds: a small save to copy 20 times")
    args = ap.parse_args()
    lib.check_inputs(args)
    os.makedirs(args.work, exist_ok=True)
    os.makedirs(os.path.join(args.work, "nofeed"), exist_ok=True)
    ok = True
    for name in args.only.split(","):
        print(f"=== {name}", flush=True)
        try:
            ok &= bool(SCEN[name](args))
        except Exception as e:  # a scenario that raises is a failed scenario; the others still run
            import traceback
            traceback.print_exc()
            print(json.dumps({"scenario": name, "pass": False, "raised": repr(e)}), flush=True)
            ok = False
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
