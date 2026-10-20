#!/usr/bin/env python3
"""Tick bench driver: the game loop's cost per tick, for several player counts, best of N.

Purpose: the in-process benchmark `bench::bench` (server/src/bench.rs; `make bench` runs one
case) loads a save, plays scripted players for a number of game seconds and prints the time per
tick, the allocations per tick and a fingerprint of every message and of the final save. This
driver runs it for a list of cases, keeps the best run of each (lowest mean), and can compare
two builds: the fingerprints must be EQUAL (same behaviour) and the means show the speed-up.

Requirements: python3 (3.9+), cargo (to build the test binary once) or --test-bin a binary built
with `cargo test --release --no-run` (its path is on cargo's "Executable unittests" line), the
game tables, a save. --instructions also needs perf(1). The run is capped at 6 GB of address
space, as `make test` does. Use a quiet machine for the times; the fingerprint does not care.

Run:  tools/perf/bench.py --world run/worlds/world.json [--seconds 300] [--runs 3]
          [--cases 0:0,4:0,4:200,8:200,32:200,64:200] [--base-test-bin OLD_TESTBIN]
          [--instructions] [--dump FILE] [--sections]
      A case is PLAYERS:ACTION_MS (ACTION_MS 0: the players only move).
      --base-test-bin  also run this other build and compare fingerprints and times
      --instructions   also count user instructions per tick (a 1500 s run minus a 300 s one)
      --dump FILE      write every message and the final save to FILE (PCDS_BENCH_DUMP) for a
                       diff of two runs; use with one case and --runs 1
      --sections       print the time per subsystem of the tick

A pass: exit status 0, one line per case "players=P actions=Nms: mean X us/tick, p50, p99,
allocations per tick, fingerprint"; with --base-test-bin every case also says EQUAL or DIFFERENT
and the exit status is 1 when any is DIFFERENT. Use the same --seconds for both builds.
"""
import argparse
import os
import re
import resource
import subprocess
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "chaos"))
import lib  # noqa: E402

UNIT = {"ns": 1e-3, "µs": 1.0, "us": 1.0, "ms": 1e3, "s": 1e6}


def limit():
    resource.setrlimit(resource.RLIMIT_AS, (6_000_000_000, 6_000_000_000))


def run_once(exe, args, players, action_ms, seconds, perf=False, dump=None):
    env = lib.steam_env(exe)
    env.update(PCDS_TEST_DATA=args.data, PCDS_BENCH_SAVE=args.world, PCDS_RNG_SEED="1", PCDS_BENCH_PLAYERS=str(players), PCDS_BENCH_ACTION_MS=str(action_ms), PCDS_BENCH_SECONDS=str(seconds))
    if args.sections:
        env["PCDS_BENCH_SECTIONS"] = "1"
    if dump:
        env["PCDS_BENCH_DUMP"] = dump
    cmd = [exe, "bench::bench", "--ignored", "--nocapture", "--exact", "--test-threads", "1"]
    if perf:
        cmd = ["perf", "stat", "-x,", "-e", "instructions:u"] + cmd
    r = subprocess.run(cmd, env=env, capture_output=True, text=True, preexec_fn=limit, cwd=os.path.join(lib.REPO, "server"))
    out = r.stdout + r.stderr
    if perf:
        return sum(float(l.split(",")[0]) for l in out.splitlines() if "instructions" in l and l.split(",")[0].replace(".", "").isdigit())
    m = re.search(r"ticks: ([0-9.]+)(ns|µs|us|ms|s) per tick", out)
    if r.returncode or not m:
        sys.exit(f"the bench failed ({r.returncode}):\n" + out[-1500:])
    q = re.search(r"per tick p50 ([0-9.]+) us, p99 ([0-9.]+) us", out)
    a = re.search(r"([0-9.]+) allocations \((\d+) bytes\) per tick", out)
    fp = re.search(r"fingerprint ([0-9a-f]+)", out)
    return dict(mean=float(m.group(1)) * UNIT[m.group(2)], p50=float(q.group(1)) if q else None, p99=float(q.group(2)) if q else None,
                allocs=float(a.group(1)) if a else None, fingerprint=fp.group(1) if fp else None, sections=[l for l in out.splitlines() if l.startswith("bench: section")])


def best(exe, args, players, action_ms, dump=None):
    runs = [run_once(exe, args, players, action_ms, args.seconds, dump=dump) for _ in range(args.runs)]
    return min(runs, key=lambda r: r["mean"])


def line(label, players, action_ms, r):
    return f"{label}players={players} actions={action_ms}ms: mean {r['mean']:.2f} us/tick, p50 {r['p50']} us, p99 {r['p99']} us, {r['allocs']} allocations/tick, fp {r['fingerprint']}"


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter, epilog="See the header of this file for requirements and what a pass is.")
    ap.add_argument("--world", default=os.path.join(lib.REPO, "run", "worlds", "world.json"), help="the save the bench plays on (default: %(default)s)")
    ap.add_argument("--data", default=os.environ.get("PCDS_TEST_DATA") or os.path.join(lib.REPO, "run", "data", "2.103"), help="the game tables (default: $PCDS_TEST_DATA, else run/data/2.103)")
    ap.add_argument("--test-bin", default=None, help="a cargo test binary of this build (default: build/find it with cargo)")
    ap.add_argument("--base-test-bin", default=None, help="the test binary of another build to compare with")
    ap.add_argument("--seconds", type=int, default=300, help="game seconds per run (default %(default)s)")
    ap.add_argument("--runs", type=int, default=3, help="runs per case; the best counts (default %(default)s)")
    ap.add_argument("--cases", default="0:0,4:200", help="PLAYERS:ACTION_MS,... (default %(default)s)")
    ap.add_argument("--instructions", action="store_true", help="also count user instructions per tick")
    ap.add_argument("--dump", default=None, help="write all messages and the final save here")
    ap.add_argument("--sections", action="store_true", help="print the time per subsystem")
    args = ap.parse_args()
    for k in ("world", "data", "test_bin", "base_test_bin", "dump"):
        if getattr(args, k):
            setattr(args, k, os.path.abspath(getattr(args, k)))
    if not os.path.isfile(args.world):
        sys.exit(f"--world {args.world}: no such save (none ships with the repository)")
    if not os.path.isfile(os.path.join(args.data, "netdata.json")):
        sys.exit(f"--data {args.data}: no game tables there")
    exe = lib.test_binary(args)
    cases = [tuple(int(x) for x in c.split(":")) for c in args.cases.split(",")]
    different = False
    for players, action_ms in cases:
        r = best(exe, args, players, action_ms, args.dump)
        print(line("", players, action_ms, r), flush=True)
        for s in r["sections"]:
            print("   ", s)
        if args.instructions:
            hi = run_once(exe, args, players, action_ms, 1500, perf=True)
            lo = run_once(exe, args, players, action_ms, 300, perf=True)
            print(f"    user instructions per tick: {round((hi - lo) / (1200 / 0.005))} (a 1500 s run minus a 300 s one, per 5 ms tick; includes the bench's hashing)")
        if args.base_test_bin:
            b = best(args.base_test_bin, args, players, action_ms)
            same = b["fingerprint"] == r["fingerprint"]
            different |= not same
            print(line("base: ", players, action_ms, b))
            print(f"    fingerprints {'EQUAL' if same else 'DIFFERENT'}; mean {r['mean'] / b['mean']:.3f} x the base")
    sys.exit(1 if different else 0)


if __name__ == "__main__":
    main()
