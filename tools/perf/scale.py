#!/usr/bin/env python3
"""Scale: how the server behaves on worlds of growing size.

Purpose: a late-game world has tens of thousands of objects. For each size in --objects this
grows a copy of your save to about that many world objects (tools/grow-world.py), starts the
server (front + two banks) on it and measures:
  start_s            seconds from launch to "state running"
  bank_rss_mb, front_rss_mb   memory after the start
  idle_longest_ms, idle_busy_pct   the game loop at rest (15 s)
  save_s, save_mb    a POST /api/save, the file size, and the longest loop just after
  join_s             a player joining alone (the ignored test load::tests::rejoin)
  load_*             8 `pcds load` players for 25 s: longest loop, busy %, round trips
  flip               a hot update to the other bank (rollback): ok, stoodMs (how long the world
                     stood still), snapshot bytes, restore ms, and the bank's memory afterwards
One JSON line per size, also appended to scale.jsonl in the work folder.

Requirements: python3 (3.9+), a built `pcds` (make build), the game tables, a save to grow
(copied, never written), cargo for the join measurement (or --test-bin a binary built with
`cargo test --release --no-run`; use --no-join to skip it). Linux (/proc). RAM for the biggest
size (a 100000-object world takes a few hundred MB per process).

Run:  tools/perf/scale.py --world run/worlds/world.json [--objects 10000,40000,100000]
          [--max-start-s 60] [--max-loop-ms 250] [--no-join] [--no-flip]
      Ports: --port (default 8430, admin 8431).

A pass: exit status 0 and one JSON line per size with no check failed: the server started within
--max-start-s, the idle and loaded game loops stayed under --max-loop-ms, the save worked, the
flip succeeded. The numbers themselves are the result; compare them between builds.
"""
import argparse
import json
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "chaos"))
import lib  # noqa: E402


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter, epilog="See the header of this file for requirements and what a pass is.")
    lib.add_server_args(ap, port=8430)
    ap.add_argument("--objects", default="10000,40000", help="world object counts to grow to, comma-separated (default %(default)s)")
    ap.add_argument("--max-start-s", type=float, default=60.0, help="the longest start allowed (default %(default)s)")
    ap.add_argument("--max-loop-ms", type=float, default=250.0, help="the longest game loop allowed (default %(default)s)")
    ap.add_argument("--no-join", action="store_true", help="skip the join measurement (no cargo needed)")
    ap.add_argument("--no-flip", action="store_true", help="skip the flip")
    args = ap.parse_args()
    lib.check_inputs(args)
    os.makedirs(args.work, exist_ok=True)
    log = lib.Log(os.path.join(args.work, "scale.log"))
    verdict = lib.Verdict("scale", log)
    grow = os.path.join(lib.REPO, "tools", "grow-world.py")
    results = []
    for n in (int(x) for x in args.objects.split(",")):
        tag = f"s{n}"
        grown = os.path.join(args.work, f"{tag}.json")
        if not os.path.exists(grown):
            subprocess.run([sys.executable, grow, args.world, grown, str(n)], check=True)
        res = {"tag": tag, "file_mb": round(os.path.getsize(grown) / 1e6, 2)}
        t = time.time()
        srv = lib.Srv(tag, args, world=grown, banks=True, rules={"limits": {"maxPlayers": 16}})
        srv.start(wait=900)
        res["start_s"] = round(time.time() - t, 2)
        if res["start_s"] > args.max_start_s:
            verdict.fail(f"{tag}: slow start", start_s=res["start_s"])
        s, j = srv.status()
        res["objs"] = j["objects"]
        res["bank_rss_mb"] = lib.proc(srv.bank_pid())["rss"] // 1024
        res["front_rss_mb"] = round(lib.proc(srv.pid)["rss"] / 1024, 1)
        res["idle_longest_ms"], res["idle_busy_pct"], _ = lib.window(srv, 15, 0.4)
        t = time.time()
        s, _ = srv.post("/api/save", timeout=900)
        res["save_s"], res["save_mb"] = round(time.time() - t, 3), round(os.path.getsize(srv.world) / 1e6, 2)
        if s != 200:
            verdict.fail(f"{tag}: save failed", status=s)
        res["after_save_longest_ms"] = lib.window(srv, 2, 0.4)[0]
        if not args.no_join:
            bots = lib.Bots("rejoin", args, f"{lib.LOOPBACK}:{srv.port}", players=1, seconds=10)
            bots.p.wait(timeout=300)
            res["join_s"] = [round(d["joined"], 2) for _, d in bots.events if d.get("joined") is not None][:2]
        p = lib.load_proc(srv, args, 8, 25)
        res["load_longest_ms"], res["load_busy_pct"], _ = lib.window(srv, 24, 0.4)
        out, _ = p.communicate(timeout=120)
        res["load_summary"] = " | ".join(x.strip() for x in out.splitlines()[1:4])
        if p.returncode != 0:
            verdict.fail(f"{tag}: pcds load failed", out=out[-300:])
        for k in ("idle_longest_ms", "load_longest_ms"):
            if res[k] > args.max_loop_ms:
                verdict.fail(f"{tag}: {k}", ms=res[k], limit=args.max_loop_ms)
        if not args.no_flip:
            s, j = srv.post("/api/update", {"rollback": True})
            seq = j.get("seq") if isinstance(j, dict) else None
            done = None
            end = time.time() + 120
            while seq and time.time() < end and not done:
                u = srv.get("/api/update")[1]
                done = next((e for e in u.get("history", []) if e.get("seq") == seq), None) if isinstance(u, dict) else None
                time.sleep(0.2)
            res["flip"] = {k: done.get(k) for k in ("ok", "stoodMs", "snapshotBytes", "snapshotMs", "restoreMs", "message")} if done else None
            if not done or not done.get("ok"):
                verdict.fail(f"{tag}: the flip failed", flip=res["flip"])
            res["bank_rss_after_flip_mb"] = lib.proc(srv.bank_pid())["rss"] // 1024
        srv.stop()
        print(json.dumps(res), flush=True)
        open(os.path.join(args.work, "scale.jsonl"), "a").write(json.dumps(res) + "\n")
        results.append(res)
    verdict.exit(sizes=len(results))


if __name__ == "__main__":
    main()
