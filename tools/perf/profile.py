#!/usr/bin/env python3
"""Profile: perf record of the server's front and bank during load, a save, or idle.

Purpose: find where the time goes. It starts the front with its two banks on a copy of a save,
and records the front's and the bank's call graphs with perf(1) while one of three things runs:
  load   `pcds load` players (--players, --action-ms) for --seconds;
  save   one POST /api/save (a big world makes the save the thing to look at);
  idle   nothing.
The recordings are OUT/front.data and OUT/bank.data (--out); read them with
tools/perf/pstack.py (self time, inclusive time, callers) or `perf report -i`. By default the
sampling is by time (--freq 999 Hz); --instructions samples every 100000 user instructions
instead, which does not depend on how busy the machine is.

Requirements: python3 (3.9+), perf(1) and permission to record the server's processes (same
user; perf_event_paranoid <= 1 for the call graphs of kernel time), a built `pcds` that has
frame pointers and symbols, otherwise the stacks are shallow or nameless:
    CARGO_PROFILE_RELEASE_DEBUG=1 RUSTFLAGS="-C force-frame-pointers=yes" \\
        cargo build --release --manifest-path server/Cargo.toml
(a separate --target-dir keeps it from replacing your normal build). The game tables, a save.

Run:  tools/perf/profile.py --world run/worlds/world.json --during load|save|idle
          [--players 32] [--seconds 40] [--action-ms 200] [--freq 999 | --instructions]
          [--warmup-seconds 10] [--call-graph fp] [--mmap-pages 2] [--out DIR]
      Ports: --port (default 8430, admin 8431).

A pass: exit status 0, successful load clients and CPU samples in both .data files; the tool prints the
command that reads them.
"""
import argparse
import os
import subprocess
import sys
import threading
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "chaos"))
import lib  # noqa: E402


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter, epilog="See the header of this file for requirements and what a pass is.")
    lib.add_server_args(ap, port=8430)
    ap.add_argument("--during", choices=["load", "save", "idle"], default="load", help="what runs while recording (default %(default)s)")
    ap.add_argument("--players", type=int, default=32, help="load: `pcds load` players (default %(default)s)")
    ap.add_argument("--seconds", type=int, default=40, help="load: length of the run; the recording excludes warm-up and 4 s of leaving; save/idle: the recording (default %(default)s)")
    ap.add_argument("--warmup-seconds", type=int, default=10, help="load: warm-up before measurement (default %(default)s)")
    ap.add_argument("--action-ms", type=int, default=200, help="load: each player acts this often (default %(default)s)")
    ap.add_argument("--freq", type=int, default=999, help="samples per second (default %(default)s)")
    ap.add_argument("--instructions", action="store_true", help="sample every 100000 user instructions instead of by time")
    ap.add_argument("--call-graph", default="fp", help="perf call graph method, e.g. dwarf,4096 for a release without frame pointers (default %(default)s)")
    ap.add_argument("--mmap-pages", type=int, default=2, help="perf's -m: a user may map only perf_event_mlock_kb of buffers; raise it if samples are lost (default %(default)s)")
    ap.add_argument("--out", default=None, help="where the .data files go (default: the work folder)")
    args = ap.parse_args()
    if args.warmup_seconds < 0 or (args.during == "load" and args.seconds < args.warmup_seconds + 9):
        ap.error("load must leave at least 5 s of measurement after warm-up and 4 s of leaving")
    lib.check_inputs(args)
    out = os.path.abspath(args.out or os.path.join(args.work, "profile"))
    os.makedirs(out, exist_ok=True)
    srv = lib.Srv("profile", args, banks=True, rules={"limits": {"maxPlayers": max(16, args.players)}})
    srv.start()
    time.sleep(2)
    front, bank = srv.pid, srv.bank_pid()
    sample = ["-e", "instructions:u", "-c", "100000"] if args.instructions else ["-F", str(args.freq)]
    load = None
    if args.during == "load":
        load = lib.load_proc(srv, args, args.players, args.seconds, args.action_ms)
        time.sleep(args.warmup_seconds)
        length = max(5, args.seconds - args.warmup_seconds - 4)
    else:
        length = args.seconds if args.during == "idle" else 8
    rec = []
    for name, pid in (("front", front), ("bank", bank)):
        rec.append(subprocess.Popen(["perf", "record", *sample, "-m", str(args.mmap_pages), "--call-graph", args.call_graph, "-p", str(pid), "-o", os.path.join(out, f"{name}.data"), "--", "sleep", str(length)], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True))
    if args.during == "save":
        time.sleep(1)
        t = time.time()
        saver = threading.Thread(target=lambda: print("save", srv.post("/api/save", timeout=600)[0], f"{time.time() - t:.2f} s"))
        saver.start()
        saver.join()
    errors = [p.communicate()[1] for p in rec]
    if load:
        load.communicate(timeout=args.seconds + 60)
    srv.stop()
    bad = []
    for name, proc, error in zip(("front", "bank"), rec, errors):
        data = os.path.join(out, f"{name}.data")
        samples = subprocess.run(["perf", "script", "-i", data, "-G", "-F", "pid"], capture_output=True, text=True)
        count = len(samples.stdout.splitlines())
        print(f"{name}: {count} samples; {error.strip()}")
        if proc.returncode or samples.returncode or not count:
            bad.append(f"{name}: recording failed or contained no samples: {samples.stderr.strip()}")
    if load and load.returncode:
        bad.append("pcds load failed")
    for message in bad:
        print("FAIL:", message, file=sys.stderr)
    if bad:
        sys.exit(1)
    here = os.path.dirname(os.path.abspath(__file__))
    print(f"recorded {out}/front.data and {out}/bank.data; read them with:\n  {here}/pstack.py {out}/bank.data --top 30")


if __name__ == "__main__":
    main()
