#!/usr/bin/env python3
"""Load run: what a real server (front + bank) costs idle and under `pcds load` players.

Purpose: the cost of the server as a user runs it, to compare two builds or two commits. On a
copy of a save it starts the front with its two banks, measures an idle window, then runs
`pcds load` players (they walk, craft, build, chat, mine, sort...) and measures the same for the
steady part of the load. Reported per process (front, bank): CPU as % of one core split into
user and system, context switches per second, and, with --perf, user/kernel instructions and
cycles from `perf stat`. Also: the memory (RSS) before, during and after, the round trips and
bytes per player that `pcds load` prints, the tick times the status reports (p50, p99, max), and what went
over the wire in the steady window (datagrams and bytes out and in, how many datagrams only acknowledged,
GET /api/status traffic.wire). A save on a DLC planet needs PCDS_TOOLS_RULES='{"world":{"dlcPlanets":["Humble"]}}'.

Requirements: python3 (3.9+), a built `pcds` (make build), the game tables, a save to run on
(copied, never written; the bigger and older the world, the more the numbers mean), Linux
(/proc). --perf also needs perf(1) and permission to count (perf_event_paranoid <= 1 counts
the kernel part). The wall-clock numbers are only comparable on an otherwise quiet machine:
run both builds back to back and look at the instructions, which do not depend on the load.

Run:  tools/perf/loadrun.py --world run/worlds/world.json [--players 8] [--seconds 40]
          [--action-ms 200] [--idle-seconds 20] [--perf] [--bin PCDS]
      Ports: --port (default 8430, admin 8431).
      To compare builds: run it once per --bin and compare the "instructions" lines.

A pass: exit status 0 (every `pcds load` player played to the end, nothing panicked) and the
numbers printed; the last line is one JSON object with them. There is no threshold: it is a
measurement.
"""
import argparse
import json
import os
import re
import subprocess
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "chaos"))
import lib  # noqa: E402

HZ = os.sysconf("SC_CLK_TCK")


def snap(pid):
    """(utime ticks, stime ticks, context switches) of a pid."""
    stat = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
    ctx = sum(int(m) for m in re.findall(r"ctxt_switches:\s+(\d+)", open(f"/proc/{pid}/status").read()))
    return int(stat[11]), int(stat[12]), ctx


def rate(a, b, secs):
    return dict(user_pct=round(100 * (b[0] - a[0]) / HZ / secs, 2), sys_pct=round(100 * (b[1] - a[1]) / HZ / secs, 2), ctx_per_s=round((b[2] - a[2]) / secs))


def perf_stat(pid, secs, out):
    return subprocess.Popen(["perf", "stat", "-x,", "-e", "instructions:u,instructions:k,cycles:u,cycles:k", "-p", str(pid), "-o", out, "--", "sleep", str(secs)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def perf_read(path, secs):
    """{'instructions:u': M/s, ...} from a perf stat -x, file; {} when perf could not count."""
    d = {}
    try:
        for line in open(path):
            p = line.strip().split(",")
            if len(p) < 3 or not p[0].replace(".", "").isdigit():
                continue
            m = re.search(r"(instructions|cycles)[/:]?(u|k)?", p[2])
            if m:
                key = f"{m.group(1)}:{m.group(2) or '?'}"
                d[key] = d.get(key, 0) + float(p[0]) / secs / 1e6
    except OSError:
        pass
    return {k: round(v, 2) for k, v in d.items()}


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter, epilog="See the header of this file for requirements and what a pass is.")
    lib.add_server_args(ap, port=8430)
    ap.add_argument("--players", type=int, default=8, help="`pcds load` players (default %(default)s)")
    ap.add_argument("--seconds", type=int, default=40, help="length of the load run, at least 20 (default %(default)s)")
    ap.add_argument("--action-ms", type=int, default=200, help="each player acts this often (default %(default)s)")
    ap.add_argument("--idle-seconds", type=int, default=20, help="the idle window (default %(default)s)")
    ap.add_argument("--perf", action="store_true", help="also count instructions and cycles with perf stat")
    args = ap.parse_args()
    if args.seconds < 20:
        ap.error("--seconds must be at least 20 (8 s of joining, 4 s of leaving, the rest measured)")
    lib.check_inputs(args)
    os.makedirs(args.work, exist_ok=True)
    srv = lib.Srv("loadrun", args, banks=True, rules={"limits": {"maxPlayers": min(100, max(16, args.players))}})
    srv.start()
    time.sleep(1)
    front, bank = srv.pid, srv.bank_pid()
    res = {"bin": args.bin, "players": args.players, "seconds": args.seconds, "action_ms": args.action_ms}
    mem = lambda: {"front_mb": round(lib.proc(front)["rss"] / 1024, 1), "bank_mb": round(lib.proc(bank)["rss"] / 1024, 1)}
    res["rss_before"] = mem()

    # idle
    ps = [perf_stat(p, args.idle_seconds, os.path.join(srv.home, f"{n}-idle.txt")) for n, p in (("front", front), ("bank", bank))] if args.perf else []
    a = (snap(front), snap(bank))
    time.sleep(args.idle_seconds)
    b = (snap(front), snap(bank))
    for p in ps:
        p.wait()
    res["idle"] = {"front": rate(a[0], b[0], args.idle_seconds), "bank": rate(a[1], b[1], args.idle_seconds)}
    if args.perf:
        res["idle"]["instructions_M_per_s"] = {n: perf_read(os.path.join(srv.home, f"{n}-idle.txt"), args.idle_seconds) for n in ("front", "bank")}

    # load: 8 s of joins, a steady window, 4 s of leaving
    load = lib.load_proc(srv, args, args.players, args.seconds, args.action_ms)
    time.sleep(8)
    steady = args.seconds - 12
    ps = [perf_stat(p, steady, os.path.join(srv.home, f"{n}-load.txt")) for n, p in (("front", front), ("bank", bank))] if args.perf else []
    a = (snap(front), snap(bank))
    traffic0 = (srv.status()[1] or {}).get("traffic")
    time.sleep(steady / 2)
    res["rss_during"] = mem()
    time.sleep(steady / 2)
    b = (snap(front), snap(bank))
    traffic1 = (srv.status()[1] or {}).get("traffic")
    for p in ps:
        p.wait()
    res["load"] = {"front": rate(a[0], b[0], steady), "bank": rate(a[1], b[1], steady)}
    if isinstance(traffic0, dict) and isinstance(traffic1, dict) and "wire" in traffic1:
        # What went over the wire in the steady window (UTP's headers counted, IP's and UDP's not).
        w0, w1 = traffic0["wire"], traffic1["wire"]
        per = lambda k: round((w1[k] - w0[k]) / steady, 1)
        res["wire_per_second"] = {"datagramsOut": per("datagramsOut"), "bytesOut": per("bytesOut"), "ackOnlyOut": per("ackOnlyOut"), "heartbeatsOut": per("heartbeatsOut"), "resendsOut": per("resendsOut"), "datagramsIn": per("datagramsIn"), "bytesIn": per("bytesIn"), "batchBytesOut": round((traffic1["bytesOut"] - traffic0["bytesOut"]) / steady, 1)}
    if args.perf:
        res["load"]["instructions_M_per_s"] = {n: perf_read(os.path.join(srv.home, f"{n}-load.txt"), steady) for n in ("front", "bank")}
    out, _ = load.communicate(timeout=args.seconds + 60)
    res["rss_after"] = mem()
    s, j = srv.status()
    if s == 200:
        res["status"] = {k: j.get(k) for k in ("busyPercent", "longestLoopMicros", "fps", "objects", "tick")}
    text = srv.logtext()
    srv.stop()

    print(out.strip())
    for k in ("idle", "load"):
        for who in ("front", "bank"):
            r = res[k][who]
            print(f"{k.upper():5} {who:5} user {r['user_pct']:6.2f}% sys {r['sys_pct']:6.2f}% of a core, {r['ctx_per_s']} context switches/s")
    print("rss MB:", json.dumps({k: res[k] for k in ("rss_before", "rss_during", "rss_after")}))
    if "wire_per_second" in res:
        w = res["wire_per_second"]
        print(f"wire per second, all {args.players} players: out {w['bytesOut'] / 1024:.1f} KB in {w['datagramsOut']:.0f} datagrams ({w['ackOnlyOut']:.0f} only acknowledging, {w['resendsOut']:.0f} resends; the batches inside {w['batchBytesOut'] / 1024:.1f} KB), in {w['bytesIn'] / 1024:.1f} KB in {w['datagramsIn']:.0f} datagrams")
    print("status:", json.dumps(res.get("status")))
    print(json.dumps(res))
    bad = []
    if load.returncode != 0:
        bad.append("pcds load: a player failed")
    if "panicked" in text:
        bad.append(f"a panic in {srv.log}")
    for m in bad:
        print("FAIL:", m, file=sys.stderr)
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
