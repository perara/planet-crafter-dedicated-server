#!/usr/bin/env python3
"""pstack: read a perf.data call graph as plain tables (python3, standard library only).

Purpose: a quicker answer than `perf report` to "where does the time go": the share of samples
by self function, by inclusive function (any frame of the stack), the callers of one function,
and what is called below one function. Works on any perf.data recorded with --call-graph fp (or
dwarf), such as tools/perf/profile.py makes.

Requirements: python3 (3.9+), perf(1) (it runs `perf script -F pid,ip,sym --no-inline`).

Run:  tools/perf/pstack.py PERF.DATA [PID] [--top N] [--caller SYM] [--below SYM [--depth D]]
      PID        only the samples of this process id (default: all in the file)
      --top N    rows per table (default 25)
      --caller SYM  also the three frames above the first frame containing SYM, by share
      --below SYM   the share of samples under SYM, by the D frames just below it (--depth, default 1)

A pass: exit status 0 and the tables printed ("N samples" first); an empty file says 0 samples.
"""
import argparse
import collections
import re
import subprocess
import sys


def read(data, pid):
    out = subprocess.run(["perf", "script", "-i", data, "-F", "pid,ip,sym", "--no-inline"], capture_output=True, text=True)
    if out.returncode:
        sys.exit("perf script failed: " + out.stderr[:400])
    samples, cur = [], None
    for line in out.stdout.splitlines():
        if not line.strip():
            if cur:
                samples.append(cur)
                cur = None
            continue
        if cur is None:
            cur = {"pid": line.strip().split()[0], "stack": []}  # the sample's header line; frames follow, leaf first
        else:
            parts = line.strip().split(None, 1)
            sym = parts[1] if len(parts) > 1 else parts[0]
            cur["stack"].append(re.sub(r"\+0x[0-9a-f]+$", "", re.sub(r"\s*\(inlined\)", "", sym)))
    if cur:
        samples.append(cur)
    return [s for s in samples if not pid or s["pid"] == pid]


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter, epilog="See the header of this file for requirements.")
    ap.add_argument("data")
    ap.add_argument("pid", nargs="?", default=None)
    ap.add_argument("--top", type=int, default=25)
    ap.add_argument("--caller", default=None)
    ap.add_argument("--below", default=None)
    ap.add_argument("--depth", type=int, default=1)
    args = ap.parse_args()
    samples = read(args.data, args.pid)
    n = len(samples)
    short = lambda s: s[:110]
    print(f"{n} samples")
    if not n:
        return
    print("--- top self")
    for sym, c in collections.Counter(s["stack"][0] for s in samples if s["stack"]).most_common(args.top):
        print(f"{100 * c / n:6.2f}% {short(sym)}")
    incl = collections.Counter()
    for s in samples:
        for sym in set(s["stack"]):
            incl[sym] += 1
    print("--- top inclusive")
    k = 0
    for sym, c in incl.most_common(400):
        if sym.startswith("0x"):
            continue
        print(f"{100 * c / n:6.2f}% {short(sym)}")
        k += 1
        if k >= args.top + 15:
            break
    if args.caller:
        cc = collections.Counter()
        for s in samples:
            for j, sym in enumerate(s["stack"]):
                if args.caller in sym:
                    cc[" <- ".join(short(x)[:60] for x in s["stack"][j + 1:j + 4])] += 1
                    break
        print(f"--- callers of {args.caller}")
        for sym, c in cc.most_common(8):
            print(f"{100 * c / n:6.2f}% {sym}")
    if args.below:
        under, cnt = 0, collections.Counter()
        for s in samples:
            frames = s["stack"][::-1]  # root first
            idx = next((i for i, f in enumerate(frames) if args.below in f), None)
            if idx is None:
                continue
            under += 1
            rest = frames[idx + 1:idx + 1 + args.depth]
            cnt[" > ".join(short(f)[:70] for f in rest) if rest else "(self)"] += 1
        print(f"--- under {args.below}: {under} samples ({100 * under / n:.1f}%)")
        for name, c in cnt.most_common(25):
            print(f"{100 * c / n:6.2f}%  {name}")


if __name__ == "__main__":
    main()
