#!/usr/bin/env python3
"""Churn: many players that join and leave badly, against a server with autosave and backups on.

Purpose: find leaks and stuck slots in the session handling. Workers keep starting sessions of
a kind drawn from a seeded stream: play a few seconds and say goodbye, play and vanish (the
socket just dropped), get approved and never become a player, only handshake, or join and
leave at once. Afterwards every session must time out, a fresh player must get in, the save
must be sound and the memory of the front and the bank must not have grown by much.

The bots are NOT a pcds command: they are the ignored cargo test load::tests::churn
(server/src/load/tests.rs), driven by the test-only variables PCDS_LOAD_SERVER (HOST:PORT),
PCDS_LOAD_PLAYERS (workers), PCDS_LOAD_SECONDS, PCDS_LOAD_SEED, PCDS_LOAD_REUSE_NAMES (any
value) and PCDS_TEST_DATA (the tables); this tool sets them. It prints one JSON line per
session (t, w, i, kind, name, ok, joined, failed, took) and a summary line, which go to
churn.events in the work folder.

Requirements: python3 (3.9+), a built `pcds` (make build), the game tables, a save to run on
(copied, never written), cargo (to build the test once; or --test-bin a binary built with
`cargo test --release --no-run`). Linux (/proc).

Run:  tools/chaos/churn.py --world run/worlds/world.json [--seconds 120] [--workers 24]
          [--seed 1] [--reuse-names] [--max-rss-growth-mb 150]
      Ports: --port (default 8420, admin 8421). Several seeds: run it again with --seed N.

A pass: exit status 0 and a last JSON line {"pass": true, ...}. That means: the bots ended
cleanly; within 90 s of the end no player is left (the sessions timed out); a fresh `pcds load`
of 4 players for 5 s succeeds; POST /api/save works and the save keeps its invariants (unique
ids, every item held once, links resolve); neither process grew by more than the allowed MB;
the server log has no panic. Sessions that fail while the server is full are expected and only
counted in the summary.
"""
import argparse
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import lib  # noqa: E402


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter, epilog="See the header of this file for requirements and what a pass is.")
    lib.add_server_args(ap)
    ap.add_argument("--seconds", type=int, default=120, help="how long the bots churn (default %(default)s)")
    ap.add_argument("--workers", type=int, default=24, help="bot workers; the server takes 16 players (default %(default)s)")
    ap.add_argument("--seed", type=int, default=1, help="the seed of the session stream (default %(default)s)")
    ap.add_argument("--reuse-names", action="store_true", help="each worker always joins under one name")
    ap.add_argument("--max-rss-growth-mb", type=int, default=150, help="allowed growth of the front's and the bank's RSS (default %(default)s)")
    args = ap.parse_args()
    lib.check_inputs(args)
    os.makedirs(args.work, exist_ok=True)
    log = lib.Log(os.path.join(args.work, f"churn{args.seed}.log"))
    verdict = lib.Verdict("churn", log)

    srv = lib.Srv(f"churn{args.seed}", args, rules={"save": {"autosaveMinutes": 1, "backupsKept": 3, "backupMinutes": 1}, "limits": {"maxPlayers": 16}})
    srv.start()
    base_bad, _ = lib.check_save(srv.world)
    bank = srv.bank_pid()
    f0, b0 = lib.proc(srv.pid), lib.proc(bank)
    log("start", json.dumps(srv.status()[1]["objects"]), "front", f0, "bank", b0)

    env = {"PCDS_LOAD_SEED": args.seed}
    if args.reuse_names:
        env["PCDS_LOAD_REUSE_NAMES"] = "1"
    bots = lib.Bots("churn", args, f"{lib.LOOPBACK}:{srv.port}", players=args.workers, seconds=args.seconds, **env)
    maxp = 0
    while bots.poll() is None:
        time.sleep(5)
        s, j = srv.status()
        if s == 200:
            maxp = max(maxp, len(j["players"]))
            log("sample players", len(j["players"]), "objs", json.dumps(j["objects"]), "front", lib.proc(srv.pid), "bank", lib.proc(srv.bank_pid()), "busy", j["busyPercent"], "longest us", j["longestLoopMicros"])
        else:
            log("status", s, j)
    time.sleep(1)  # the reader thread drains the last lines
    with open(os.path.join(args.work, f"churn{args.seed}.events"), "w") as f:
        for _, e in bots.events:
            f.write(json.dumps(e) + "\n")
    summary = next((e["summary"] for _, e in bots.events if "summary" in e), None)
    log("bots done rc", bots.poll(), "max players", maxp, "summary", json.dumps(summary))
    if bots.poll() != 0 or summary is None:
        verdict.fail("the bots did not end cleanly", rc=bots.poll(), summary=summary)

    # The sessions of the last seconds time out after 30 s of silence.
    left = None
    for _ in range(9):
        time.sleep(10)
        s, j = srv.status()
        left = len(j["players"]) if s == 200 else -1
        log("settle players", left, "objs", json.dumps(j["objects"]) if s == 200 else j)
        if left == 0:
            break
    if left != 0:
        verdict.fail("players still in the game 90 s after the bots ended", players=left)

    # A fresh player gets a slot.
    p = lib.load_proc(srv, args, 4, 5)
    out, _ = p.communicate(timeout=120)
    log("fresh load rc", p.returncode, out.strip().replace("\n", " | ")[:300])
    if p.returncode != 0:
        verdict.fail("a fresh player could not play after the churn", out=out[-400:])

    s, j = srv.post("/api/save")
    if s != 200:
        verdict.fail("save failed", status=s, body=str(j)[:200])
    bad, counts = lib.check_save(srv.world)
    new = [b for b in bad if b not in base_bad]
    log("save check", counts, new[:10])
    if new:
        verdict.fail("save invariants", bad=new[:6])

    f1, b1 = lib.proc(srv.pid), lib.proc(srv.bank_pid())
    for name, a, b in (("front", f0, f1), ("bank", b0, b1)):
        if not (a and b):
            verdict.fail(f"the {name} is gone")
        elif (b["rss"] - a["rss"]) / 1024 > args.max_rss_growth_mb:
            verdict.fail(f"the {name}'s memory grew", from_mb=a["rss"] // 1024, to_mb=b["rss"] // 1024)
    if "panicked" in srv.logtext():
        verdict.fail("a panic in the server log", log=srv.log)
    srv.stop()
    log("stopped rc", srv.proc.returncode)
    verdict.exit(summary=summary, max_players=maxp)


if __name__ == "__main__":
    main()
