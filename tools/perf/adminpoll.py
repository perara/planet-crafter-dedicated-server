#!/usr/bin/env python3
"""adminpoll: watch or hammer the admin API of a server that is already running.

Purpose: two small probes on a server you started yourself (it never starts one):
  watch    polls GET /api/status four times a second and prints every sample where the game
           loop's longest tick of the last second was above --threshold-us (default 3000),
           and the longest seen: find the stalls while something else (a load run, a save, a
           backup) happens.
  hammer   --threads threads request one admin path in a loop for --seconds: requests per
           second and the worst answer time. For the admin HTTP thread, which must not be held
           by the game loop (docs/admin-api.md).
The token is read from a file or an environment variable named by you; it is never put on a
command line or printed.

Requirements: python3 (3.9+). A running server whose admin address is on loopback.

Run:  tools/perf/adminpoll.py watch  --port ADMIN_PORT (--token-file FILE | --token-env NAME)
          [--seconds 60] [--threshold-us 3000]
      tools/perf/adminpoll.py hammer --port ADMIN_PORT (--token-file FILE | --token-env NAME)
          [--path /api/status] [--threads 4] [--seconds 10]
      The default admin address of an installed server is 127.0.0.1:7782 (PCDS_ADMIN); this
      tool only reads, except that hammer repeats the GET you name.

A pass: exit status 0 and the summary line ("longest loop seen: N us" / "PATH: N req/s, worst
N ms"); exit 1 when the server does not answer. Nothing is judged.
"""
import argparse
import os
import sys
import threading
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "chaos"))
import lib  # noqa: E402


def token(args):
    if args.token_file:
        return open(args.token_file).read().strip()
    if args.token_env and os.environ.get(args.token_env):
        return os.environ[args.token_env].strip()
    sys.exit("give the admin token with --token-file FILE or --token-env NAME")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter, epilog="See the header of this file for requirements and what a pass is.")
    ap.add_argument("mode", choices=["watch", "hammer"])
    ap.add_argument("--port", type=int, required=True, help="the admin API's port on 127.0.0.1")
    ap.add_argument("--token-file", default=None, help="a file holding the admin token")
    ap.add_argument("--token-env", default=None, help="the name of an environment variable holding the admin token")
    ap.add_argument("--seconds", type=float, default=None, help="how long (watch 60, hammer 10)")
    ap.add_argument("--threshold-us", type=int, default=3000, help="watch: report loops longer than this (default %(default)s)")
    ap.add_argument("--path", default="/api/status", help="hammer: the GET path (default %(default)s)")
    ap.add_argument("--threads", type=int, default=4, help="hammer: concurrent requesters (default %(default)s)")
    args = ap.parse_args()
    tok = token(args)
    if args.mode == "watch":
        secs, longest = args.seconds or 60, 0
        for i in range(int(secs * 4)):
            s, j = lib.api(args.port, tok, "GET", "/api/status", timeout=10)
            if s != 200:
                print(f"status {s}: {str(j)[:100]}", file=sys.stderr)
                sys.exit(1)
            v = j.get("longestLoopMicros", 0)
            if v > args.threshold_us:
                print(f"  t={i / 4:.2f}s loop {v} us")
            longest = max(longest, v)
            time.sleep(0.25)
        print(f"longest loop seen: {longest} us")
        return
    secs = args.seconds or 10
    done, worst, failed = [0] * args.threads, [0.0] * args.threads, [0] * args.threads

    def run(i):
        end = time.time() + secs
        while time.time() < end:
            t = time.time()
            s, _ = lib.api(args.port, tok, "GET", args.path, timeout=30)
            failed[i] += s != 200
            worst[i] = max(worst[i], time.time() - t)
            done[i] += 1

    ts = [threading.Thread(target=run, args=(i,)) for i in range(args.threads)]
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    print(f"{args.path}: {sum(done) / secs:.0f} req/s, worst {max(worst) * 1000:.0f} ms, {sum(failed)} not 200")
    sys.exit(1 if sum(failed) == sum(done) else 0)


if __name__ == "__main__":
    main()
