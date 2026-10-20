#!/usr/bin/env python3
"""A/B: two servers side by side under the same load, to compare two builds or two disks.

Purpose: judge a change (a build, a setting, the filesystem the world lives on) without the
drift between two separate runs: server A and server B run at once on copies of the same save,
each with `pcds load` players, autosave and a backup every minute, so each saves often. For
every status sample with a new "longest loop" the tool keeps that loop's length, then reports
per server how many loops exceeded 50 ms, 500 ms and 5 s, the maximum, the median of the
long ones, and the CPU (front + bank, % of one core) over the run. Few and short long loops
is better; the saves are what makes them long.
With --players 0 it compares the idle cost of two builds.

Requirements: python3 (3.9+), built `pcds` program(s), the game tables, a save to run on
(copied, never written), Linux (/proc). Both servers use CPU at once: give the machine room.

Run:  tools/perf/ab.py --world run/worlds/world.json [--bin-a PROGRAM_A] [--bin-b PROGRAM_B]
          [--work-a DIR_A] [--work-b DIR_B] [--seconds 600] [--players 8]
      Defaults: both use --bin and --work. Put --work-a and --work-b on different filesystems
      to compare disks (e.g. a disk against /dev/shm). Ports: --port (default 8430) for A and
      --port + 2 for B (admin ports +1).

A pass: exit status 0 and a JSON line with the two result objects (samples, over_50ms,
over_500ms, over_5000ms, max_ms, median_of_long_ms, cpu_pct). Nothing is judged: you read it.
"""
import argparse
import copy
import json
import os
import statistics
import sys
import threading
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "chaos"))
import lib  # noqa: E402


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter, epilog="See the header of this file for requirements and what a pass is.")
    lib.add_server_args(ap, port=8430)
    ap.add_argument("--bin-a", default=None, help="server A's program (default --bin)")
    ap.add_argument("--bin-b", default=None, help="server B's program (default --bin)")
    ap.add_argument("--work-a", default=None, help="server A's folder (default --work)")
    ap.add_argument("--work-b", default=None, help="server B's folder (default --work)")
    ap.add_argument("--seconds", type=int, default=600, help="length of the run (default %(default)s)")
    ap.add_argument("--players", type=int, default=8, help="`pcds load` players per server (default %(default)s)")
    args = ap.parse_args()
    lib.check_inputs(args)
    os.makedirs(args.work, exist_ok=True)

    def side(tag, binary, work, port):
        a = copy.copy(args)
        a.bin = os.path.abspath(binary or args.bin)
        a.work = os.path.abspath(work or args.work)
        if not os.access(a.bin, os.X_OK):
            sys.exit(f"{a.bin}: not an executable")
        os.makedirs(a.work, exist_ok=True)
        srv = lib.Srv(f"ab{tag}", a, port=port, rules={"save": {"autosaveMinutes": 1, "backupsKept": 6, "backupMinutes": 1}})
        return a, srv

    sides = {"A": side("A", args.bin_a, args.work_a, args.port), "B": side("B", args.bin_b, args.work_b, args.port + 2)}
    for _, srv in sides.values():
        srv.start()
    loads = {k: (lib.load_proc(srv, a, args.players, args.seconds + 30) if args.players else None) for k, (a, srv) in sides.items()}
    cpu = lambda srv: sum((lib.proc(p) or {"cpu": 0})["cpu"] for p in srv.pids()["serve"] + srv.pids()["bank"])
    cpu0 = {k: cpu(srv) for k, (a, srv) in sides.items()}
    loops = {"A": [], "B": []}

    def poll(k, srv):
        end, last = time.time() + args.seconds, -1
        while time.time() < end:
            s, j = srv.status(timeout=60)
            if s == 200 and j["longestLoopMicros"] != last:
                last = j["longestLoopMicros"]
                loops[k].append(last / 1000)
            time.sleep(0.3)

    threads = [threading.Thread(target=poll, args=(k, srv)) for k, (a, srv) in sides.items()]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    out = {}
    for k, (a, srv) in sides.items():
        used = cpu(srv) - cpu0[k]
        v = sorted(loops[k])
        long_ = [x for x in v if x > 50]
        out[k] = dict(bin=a.bin, work=a.work, samples=len(v), over_50ms=len(long_), over_500ms=len([x for x in v if x > 500]), over_5000ms=len([x for x in v if x > 5000]),
                      max_ms=round(max(v), 1) if v else None, median_of_long_ms=round(statistics.median(long_), 1) if long_ else 0, cpu_pct=round(100 * used / args.seconds, 2))
    for p in loads.values():
        if p and p.poll() is None:
            p.terminate()
    for _, srv in sides.values():
        srv.stop()
    print(json.dumps(out))


if __name__ == "__main__":
    main()
