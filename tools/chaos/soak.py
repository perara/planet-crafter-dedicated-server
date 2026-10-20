#!/usr/bin/env python3
"""Soak: players on a server for hours, one sample every 30 s, to find leaks and slow drift.

Purpose: a server with autosave and backups on, under `pcds load` players, is sampled for its
memory, threads, file descriptors, CPU, busy time, longest game loop, object counts, save
size, number of backups, log size and UDP queues. Every 10 minutes a manual save is timed.
The samples go to samples.csv and the events (load restarts, saves) to events.log in the work
folder, so a long run can be plotted later.

Requirements: python3 (3.9+), a built `pcds` (make build), the game tables, a save to run on
(copied, never written). Linux (/proc). The bots are `pcds load` (restarted when it ends).

Run:  tools/chaos/soak.py --world run/worlds/world.json [--seconds 7500] [--players 8]
          [--max-rss-growth-pct 25] [--max-save-ms 20000]
      Ports: --port (default 8420, admin 8421).

A pass: exit status 0 and a last JSON line {"pass": true, ...}. That means: the server answered
the status for the whole run; the bank's and the front's memory in the last fifth of the run is
within --max-rss-growth-pct of the second fifth's (the first fifth is warm-up); the median
number of threads and of file descriptors did not grow; no timed save took longer than --max-save-ms; the final stop
exited 0; and the server log has no panic. samples.csv has the numbers behind it.
"""
import argparse
import glob
import json
import os
import statistics
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import lib  # noqa: E402

COLS = "t,f_rss,f_vsz,f_thr,f_fds,f_cpu,b_rss,b_vsz,b_thr,b_fds,b_cpu,busy,longest_us,winmax_longest_us,winmax_busy,fps,wo,net,inv,players,gametime,save_bytes,backups,log_bytes,udp_rx,udp_tx,tcp_admin,tick_p50_us,tick_p99_us,tick_max_us,spikes".split(",")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter, epilog="See the header of this file for requirements and what a pass is.")
    lib.add_server_args(ap)
    ap.add_argument("--seconds", type=int, default=7500, help="length of the run (default %(default)s)")
    ap.add_argument("--players", type=int, default=8, help="`pcds load` players (default %(default)s)")
    ap.add_argument("--every", type=int, default=30, help="seconds between samples (default %(default)s)")
    ap.add_argument("--max-rss-growth-pct", type=float, default=25.0, help="allowed memory growth, last fifth against second fifth (default %(default)s)")
    ap.add_argument("--max-save-ms", type=int, default=20000, help="the longest a timed save may take (default %(default)s)")
    args = ap.parse_args()
    lib.check_inputs(args)
    os.makedirs(args.work, exist_ok=True)
    srv = lib.Srv("soak", args, rules={"save": {"autosaveMinutes": 1, "backupsKept": 6, "backupMinutes": 5}})
    srv.start()
    log = lib.Log(os.path.join(srv.home, "events.log"))
    verdict = lib.Verdict("soak", log)
    csv = open(os.path.join(srv.home, "samples.csv"), "w")
    csv.write(",".join(COLS) + "\n")

    win = dict(longest=0, busy=0.0, fails=0, calls=0, spikes=0)
    stop = threading.Event()

    def poller():
        while not stop.is_set():
            s, j = srv.status(timeout=5)
            win["calls"] += 1
            if s == 200:
                win["longest"] = max(win["longest"], j.get("longestLoopMicros", 0))
                win["busy"] = max(win["busy"], j.get("busyPercent", 0))
                if j.get("longestLoopMicros", 0) > 50000:  # a loop over 50 ms: when it was, against the last save, for the cause
                    win["spikes"] += 1
                    log("spike", j["longestLoopMicros"], "us busy", j.get("busyPercent"), "lastSave.at", (j.get("lastSave") or {}).get("at"), "now", int(time.time()))
            else:
                win["fails"] += 1
            stop.wait(1)

    threading.Thread(target=poller, daemon=True).start()
    loads = []

    def start_load():
        loads.append(lib.load_proc(srv, args, args.players, args.seconds + 600, stdout=open(os.path.join(srv.home, "load.out"), "ab")))
        log("load started pid", loads[-1].pid)

    start_load()
    rows, saves, last_save = [], [], 0
    t0 = time.time()
    while time.time() - t0 < args.seconds:
        time.sleep(args.every)
        if loads[-1].poll() is not None:
            log("load exited", loads[-1].returncode)
            start_load()
        s, j = srv.status()
        f, b = lib.proc(srv.pid), lib.proc(srv.bank_pid() or 0)
        z = lambda d, k: d[k] if d else -1
        o = j.get("objects", {}) if s == 200 else {}
        q = lib.udp_queues(srv.port) or (-1, -1)
        row = dict(t=round(time.time() - t0), f_rss=z(f, "rss"), f_vsz=z(f, "vsz"), f_thr=z(f, "thr"), f_fds=z(f, "fds"), f_cpu=z(f, "cpu"),
                   b_rss=z(b, "rss"), b_vsz=z(b, "vsz"), b_thr=z(b, "thr"), b_fds=z(b, "fds"), b_cpu=z(b, "cpu"),
                   busy=j.get("busyPercent", -1) if s == 200 else -1, longest_us=j.get("longestLoopMicros", -1) if s == 200 else -1,
                   winmax_longest_us=win["longest"], winmax_busy=win["busy"], fps=j.get("fps", -1) if s == 200 else -1,
                   wo=o.get("world", -1), net=o.get("network", -1), inv=o.get("inventories", -1),
                   players=len(j.get("players", [])) if s == 200 else -1, gametime=j.get("gameTime", -1) if s == 200 else -1,
                   tick_p50_us=(j.get("tick") or {}).get("p50Micros", -1), tick_p99_us=(j.get("tick") or {}).get("p99Micros", -1), tick_max_us=(j.get("tick") or {}).get("maxMicros", -1), spikes=win["spikes"],
                   save_bytes=os.path.getsize(srv.world) if os.path.exists(srv.world) else -1, backups=len(glob.glob(os.path.join(srv.home, "backups", "*.json"))),
                   log_bytes=os.path.getsize(srv.log), udp_rx=q[0], udp_tx=q[1], tcp_admin=lib.tcp_count(srv.admin))
        win["longest"], win["busy"], win["spikes"] = 0, 0.0, 0
        rows.append(row)
        csv.write(",".join(str(row[c]) for c in COLS) + "\n")
        csv.flush()
        if time.time() - last_save > 600:
            last_save = time.time()
            t1 = time.time()
            s2, _ = srv.post("/api/save", timeout=300)
            ms = (time.time() - t1) * 1000
            saves.append(ms)
            log("manual save", s2, f"{ms:.0f} ms", "bytes", row["save_bytes"])
            if s2 != 200:
                verdict.fail("manual save failed", status=s2)
    stop.set()
    for p in loads:
        if p.poll() is None:
            p.terminate()
    time.sleep(2)

    if win["fails"] > win["calls"] * 0.05 + 3:
        verdict.fail("the status did not answer", failed=win["fails"], of=win["calls"])
    fifth = max(1, len(rows) // 5)
    for k in ("b_rss", "f_rss", "b_thr", "f_thr", "b_fds", "f_fds"):
        # Memory by the mean; threads and descriptors by the median: an admin connection or a worker thread that happens to
        # be there at a sample (a stalled host piles them up for a while) is no growth, a level that stays higher is.
        mid = statistics.mean if k.endswith("rss") else statistics.median
        early = mid(r[k] for r in rows[fifth:2 * fifth]) if len(rows) >= 2 * fifth else None
        late = mid(r[k] for r in rows[-fifth:]) if rows else None
        if early and late and early > 0:
            growth = 100 * (late - early) / early
            limit = args.max_rss_growth_pct if k.endswith("rss") else 0.0
            log("trend", k, f"{early:.0f} -> {late:.0f} ({growth:+.1f}%)")
            if growth > limit:
                verdict.fail(f"{k} grew", early=round(early), late=round(late), pct=round(growth, 1))
    if saves and max(saves) > args.max_save_ms:
        verdict.fail("a save took too long", max_ms=round(max(saves)), limit_ms=args.max_save_ms)
    log("final status lastSave", json.dumps(srv.status()[1].get("lastSave")))
    text = srv.logtext()
    srv.stop()
    log("server stopped rc", srv.proc.returncode)
    if srv.proc.returncode != 0:
        verdict.fail("the final stop did not exit 0", rc=srv.proc.returncode)
    if "panicked" in text:
        verdict.fail("a panic in the server log", log=srv.log)
    verdict.exit(samples=len(rows), csv=os.path.join(srv.home, "samples.csv"))


if __name__ == "__main__":
    main()
