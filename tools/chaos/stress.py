#!/usr/bin/env python3
"""Stress: four short abuse scenarios against one fresh server each.

  burst    16 `pcds load` players act every 200, 20, 5 and 1 ms for 30 s each: the game loop's
           longest tick and busy time must stay low, nobody may be dropped.
  bigchat  8 players whose chat lines are 900 KB long (the ignored test load::tests::rejoin with
           PCDS_LOAD_CHAT_BYTES): oversize chat must be refused or dropped, not stall the loop
           nor grow the front's memory.
  full     40 rejoining players against a 16-player server (load::tests::rejoin): no more than
           16 get in, the loop stays fast.
  stop     16 rejoining players; the bank process is stopped (SIGSTOP) for 30 s and 120 s
           (--stop-seconds), then continued: the front's memory must stay bounded while the
           bank stands (it holds the players' frames), it must answer again and the players must
           be back. (This also covers what was a separate "stall" driver.)

Requirements: python3 (3.9+), a built `pcds` (make build), the game tables, a save to run on
(copied, never written), cargo for bigchat/full/stop (the bots are the ignored cargo tests of
server/src/load/tests.rs, driven by PCDS_LOAD_SERVER, PCDS_LOAD_PLAYERS, PCDS_LOAD_SECONDS and
PCDS_LOAD_CHAT_BYTES, PCDS_TEST_DATA; or --test-bin a binary built with
`cargo test --release --no-run`). Linux (/proc).

Run:  tools/chaos/stress.py --world run/worlds/world.json [burst] [bigchat] [full] [stop]
          [--max-loop-ms 250] [--stop-seconds 30,120]
      No scenario named: all four. Ports: --port (default 8420, admin 8421).

A pass: exit status 0 and a last JSON line {"pass": true, ...}; each scenario's measurements are
printed as it goes. The checks: the longest game loop stays under --max-loop-ms (bigchat, full,
burst); `pcds load` exits 0; the server (front and bank) is alive at the end; the front's RSS
stays under 400 MB (512 MB while the bank stands); full ends with no more than the 16 slots held; after a stop the status answers
within 30 s of the CONT and all players are back within 60 s.
"""
import argparse
import os
import signal
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import lib  # noqa: E402


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter, epilog="See the header of this file for requirements and what a pass is.")
    ap.add_argument("scenarios", nargs="*", help="burst, bigchat, full, stop (default: all four)")
    lib.add_server_args(ap)
    ap.add_argument("--max-loop-ms", type=float, default=250.0, help="the longest game loop allowed (default %(default)s)")
    ap.add_argument("--stop-seconds", default="30,120", help="how long the bank is stopped, comma-separated (default %(default)s)")
    ap.add_argument("--stop-players", type=int, default=16, help="rejoining players during the stop scenario (default %(default)s)")
    ap.add_argument("--max-front-mb", type=int, default=512, help="the front's memory allowed while the bank stands (default %(default)s)")
    args = ap.parse_args()
    which = args.scenarios or ["burst", "bigchat", "full", "stop"]
    if [w for w in which if w not in ("burst", "bigchat", "full", "stop")]:
        ap.error("scenarios: burst, bigchat, full, stop")
    lib.check_inputs(args)
    os.makedirs(args.work, exist_ok=True)
    log = lib.Log(os.path.join(args.work, "stress.log"))
    verdict = lib.Verdict("stress", log)

    def fresh(name, maxp=16):
        srv = lib.Srv(name, args, banks=True, rules={"limits": {"maxPlayers": maxp}})
        srv.start()
        return srv

    def alive(srv, what):
        p = srv.pids()
        if not p["serve"] or not p["bank"] or srv.status()[0] != 200:
            verdict.fail(f"{what}: the server is not up at the end", pids=p)
        f = lib.proc(srv.pid)
        if f and f["rss"] > 400 * 1024:
            verdict.fail(f"{what}: the front's memory", rss_mb=f["rss"] // 1024)

    def loop_check(what, longest):
        if longest > args.max_loop_ms:
            verdict.fail(f"{what}: the game loop stalled", longest_ms=longest, limit_ms=args.max_loop_ms)

    if "burst" in which:
        srv = fresh("st-burst")
        for ms in (200, 20, 5, 1):
            p = lib.load_proc(srv, args, 16, 30, ms)
            w = lib.window(srv, 28)
            out, _ = p.communicate(timeout=120)
            log(f"burst: 16 players, an action every {ms} ms: longest loop {w[0]} ms, busy mean {w[1]}% max {w[2]}%;", " | ".join(x.strip() for x in out.splitlines()[:5]))
            loop_check(f"burst {ms} ms", w[0])
            if p.returncode != 0:
                verdict.fail(f"burst {ms} ms: pcds load failed", out=out[-300:])
        alive(srv, "burst")
        srv.stop()

    if "bigchat" in which:
        srv = fresh("st-chat")
        bots = lib.Bots("rejoin", args, f"{lib.LOOPBACK}:{srv.port}", players=8, seconds=30, PCDS_LOAD_CHAT_BYTES=900000)
        w = lib.window(srv, 28)
        bots.p.wait(timeout=120)
        log(f"bigchat: 8 players, 900 KB chat lines: longest loop {w[0]} ms, busy {w[1]}% max {w[2]}%; bank", lib.proc(srv.bank_pid()), "front", lib.proc(srv.pid))
        loop_check("bigchat", w[0])
        alive(srv, "bigchat")
        srv.stop()

    if "full" in which:
        srv = fresh("st-full")
        bots = lib.Bots("rejoin", args, f"{lib.LOOPBACK}:{srv.port}", players=40, seconds=40)
        peak, final, longest = 0, 0, 0
        end = time.time() + 38
        while time.time() < end:
            s, j = srv.status(timeout=30)
            if s == 200:
                # "joining" lists sessions that are only connected (a refused one stays until its kick is
                # delivered), so only the players that are in the game count against the slots.
                inside = sum(1 for p in j["players"] if p.get("status") in ("ready", "dying"))
                peak, final = max(peak, inside), inside
                longest = max(longest, j["longestLoopMicros"] / 1000)
            time.sleep(0.5)
        bots.p.wait(timeout=120)
        st = bots.stats()
        log(f"full: 40 players for a server of 16: in the game at the end {final} (peak {peak}); joins {st['joins']}, failures {st['fails']}; longest loop {longest:.1f} ms")
        if final > 16:
            verdict.fail("full: more players than slots", players=final)
        loop_check("full", longest)
        alive(srv, "full")
        srv.stop()

    if "stop" in which:
        for secs in (int(x) for x in args.stop_seconds.split(",")):
            srv = fresh("st-stop")
            n = args.stop_players
            bots = lib.Bots("rejoin", args, f"{lib.LOOPBACK}:{srv.port}", players=n, seconds=secs + 180)
            time.sleep(15)
            bank, since, peak = srv.bank_pid(), time.time(), 0
            log(f"stop: before: players {len(srv.status()[1]['players'])} front", lib.proc(srv.pid), "bank", lib.proc(bank))
            os.kill(bank, signal.SIGSTOP)
            t0 = time.time()
            while time.time() - t0 < secs:  # the front keeps the players' frames for the bank: its memory must stay bounded
                time.sleep(min(10, secs))
                peak = max(peak, lib.proc(srv.pid)["rss"] // 1024)
                log(f"stop: {time.time() - t0:.0f} s: front rss {peak} MB; bot failures {bots.stats(since)['fails']}")
            try:
                os.kill(bank, signal.SIGCONT)
            except ProcessLookupError:
                log("stop: the watchdog had ended the bank already")
            t1 = time.time()
            first = lib.wait_for(lambda: srv.status(timeout=30)[0] == 200, 30)
            log(f"stop: bank stopped {secs} s: status answers {'after %.1f s' % (time.time() - t1) if first else 'NOT within 30 s'}")
            if not first:
                verdict.fail(f"stop {secs} s: no status 30 s after CONT")
            back = lib.wait_for(lambda: (lambda s, j: s == 200 and len(j["players"]) == n)(*srv.status(timeout=30)), 60, 1)
            reasons = bots.failures(since)
            log(f"stop: {secs} s: players back: {bool(back)}; bot failures", {r: reasons.count(r) for r in set(reasons)} if len(set(reasons)) < 6 else len(reasons), "log:", [l for l in srv.logtext().splitlines() if "stall" in l or "dropped" in l][:3])
            if not back:
                verdict.fail(f"stop {secs} s: players did not come back within 60 s")
            if peak > args.max_front_mb:
                verdict.fail(f"stop {secs} s: the front's memory grew while the bank stood", peak_mb=peak, limit_mb=args.max_front_mb)
            alive(srv, f"stop {secs} s")
            bots.stop()
            srv.stop()
    verdict.exit(scenarios=which)


if __name__ == "__main__":
    main()
