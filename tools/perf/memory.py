#!/usr/bin/env python3
"""Memory: the front's and the bank's RSS, PSS and anonymous memory over time, with saves.

Purpose: see how much memory a world costs, and what a save or a backup adds at its peak. It
starts the server (front + two banks) on a copy of a save with autosave on, optionally with
`pcds load` players, and every --every seconds prints per process the RSS, PSS and anonymous
memory in MiB (from /proc/PID/smaps_rollup) and the bank's peak (VmHWM), and last how much of each
process the host has swapped out (on a host with swap an idle process's RSS falls as its cold pages
leave: read the first rows, or add the swapped to the RSS). At the end it says the bank's peak
before the first autosave and after it: the difference is what saving costs.

Requirements: python3 (3.9+), a built `pcds` (make build), the game tables, a save to run on
(copied, never written), Linux (/proc/PID/smaps_rollup). Players need no cargo here (`pcds load`).

Run:  tools/perf/memory.py --world run/worlds/world.json [--seconds 90] [--every 5]
          [--autosave-minutes 1] [--players 0] [--bin PCDS]
      Ports: --port (default 8430, admin 8431).

A pass: exit status 0 and the table, then a last line like
"bank peak MiB before the first save 120, after 180 (3 saves)" and a JSON object. Nothing is
judged; compare the numbers between builds or world sizes.
"""
import argparse
import json
import os
import re
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "chaos"))
import lib  # noqa: E402


def mem(pid):
    """RSS, PSS, anonymous, swapped-out MiB and the peak RSS (VmHWM) MiB of a pid; None if it is gone."""
    try:
        roll = open(f"/proc/{pid}/smaps_rollup").read()
        status = open(f"/proc/{pid}/status").read()
    except OSError:
        return None
    kb = lambda text, k: int(re.search(rf"^{k}:\s+(\d+)", text, re.M).group(1)) / 1024
    return dict(rss=kb(roll, "Rss"), pss=kb(roll, "Pss"), anon=kb(roll, "Anonymous"), swap=kb(roll, "Swap"), hwm=kb(status, "VmHWM"))


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter, epilog="See the header of this file for requirements and what a pass is.")
    lib.add_server_args(ap, port=8430)
    ap.add_argument("--seconds", type=int, default=90, help="how long to watch (default %(default)s)")
    ap.add_argument("--every", type=int, default=5, help="seconds between rows (default %(default)s)")
    ap.add_argument("--autosave-minutes", type=int, default=1, help="the autosave interval; 0: none (default %(default)s)")
    ap.add_argument("--players", type=int, default=0, help="`pcds load` players (default %(default)s)")
    args = ap.parse_args()
    lib.check_inputs(args)
    os.makedirs(args.work, exist_ok=True)
    srv = lib.Srv("memory", args, banks=True, rules={"save": {"autosaveMinutes": args.autosave_minutes, "backupsKept": 4, "backupMinutes": 1}, "limits": {"maxPlayers": max(16, args.players)}})
    t0 = time.time()
    srv.start()
    start_s = time.time() - t0
    front, bank = srv.pid, srv.bank_pid()
    load = lib.load_proc(srv, args, args.players, args.seconds) if args.players else None
    print(f"started in {start_s:.2f} s; front {front}, bank {bank}")
    print(f"{'t':>5} {'front rss/pss/anon':>22} {'bank rss/pss/anon':>22} {'bank peak':>10} saves  swapped out (front, bank)")
    rows, first_save_at, saves, last_at = [], None, 0, None
    t0 = time.time()
    while time.time() - t0 < args.seconds:
        time.sleep(args.every)
        f, b = mem(front), mem(bank)
        s, j = srv.status()
        at = ((j.get("lastSave") or {}).get("at")) if s == 200 else None
        if at and at != last_at:
            last_at, saves = at, saves + 1
            first_save_at = first_save_at or len(rows)
        rows.append(dict(t=round(time.time() - t0), front=f, bank=b))
        print(f"{rows[-1]['t']:5} {f['rss']:8.1f}/{f['pss']:6.1f}/{f['anon']:6.1f} {b['rss']:8.1f}/{b['pss']:6.1f}/{b['anon']:6.1f} {b['hwm']:10.1f} {saves}  {f['swap']:.1f} {b['swap']:.1f}")
    peak_before = max((r["bank"]["hwm"] for r in rows[:first_save_at]), default=None) if first_save_at else None
    peak_after = rows[-1]["bank"]["hwm"]
    if load and load.poll() is None:
        load.terminate()
    srv.stop()
    before = f"{round(peak_before)}" if peak_before else "- (no save in the first rows)"
    print(f"bank peak MiB before the first save {before}, after {round(peak_after)} ({saves} saves seen)")
    print(json.dumps(dict(start_s=round(start_s, 2), bank_peak_before_first_save_mb=peak_before and round(peak_before), bank_peak_mb=round(peak_after), saves=saves, last=rows[-1])))


if __name__ == "__main__":
    main()
