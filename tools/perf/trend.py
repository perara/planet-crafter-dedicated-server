#!/usr/bin/env python3
"""Trends of a soak's samples.csv: Theil-Sen slope per hour and a verdict per metric.

Purpose: a soak writes one row every 30 s; this says for each column whether it is flat, creeping or
leaking, with numbers, as text tables (no images). The Theil-Sen estimator is the median of the slopes
of all pairs of samples (robust to the spikes of a loaded host and to a stall that piles up threads for
a while); the Kendall tau is the share of pairs that agree about the direction.

Requirements: python3 (3.9+), standard library only.

Run:  tools/perf/trend.py samples.csv [--skip 0.1] [--flat-pct 3] [--leak-pct 10] [--bank-col b_rss --pid-col b_pid]

What the columns are:
  levels      (RSS, threads, descriptors, objects, save size, disk): slope per hour from the first `--skip`
              fraction of the run on (warm-up dropped), and the drift over the run as a percent of the median.
              The verdict: flat when |drift| < --flat-pct or tau < 0.3; creeping when drift is up to --leak-pct
              or tau < 0.6; leaking above both. A decrease is "falling" (not a problem).
  per bank    the bank is a new process after each flip or restart, so its memory is also judged inside each
              run of one pid (the first 5 minutes of a pid dropped), as the median slope over the pids that ran
              at least 20 minutes.
  counters    (CPU seconds, log bytes and lines, S3 and hook hits): the rate per hour, and the rate of the
              last third against the first third.
  windows     (busy, tick, longest loop, status latency): median and p95 of the whole run, and the trend of the
              hourly medians (tail latencies of a shared host are marked as such by the caller).
Exit status 0.
"""
import argparse
import csv
import statistics
import sys


def theil_sen(xs, ys, cap=1600):
    """(slope, tau). Pairs of at most `cap` evenly spaced samples (1.3 million pairs)."""
    n = len(xs)
    if n > cap:
        step = n / cap
        idx = [int(i * step) for i in range(cap)]
        xs, ys = [xs[i] for i in idx], [ys[i] for i in idx]
        n = cap
    slopes, pos, neg = [], 0, 0
    for i in range(n):
        xi, yi = xs[i], ys[i]
        for j in range(i + 1, n):
            dx = xs[j] - xi
            if dx <= 0:
                continue
            d = ys[j] - yi
            slopes.append(d / dx)
            if d > 0:
                pos += 1
            elif d < 0:
                neg += 1
    if not slopes:
        return 0.0, 0.0
    return statistics.median(slopes), (pos - neg) / max(1, pos + neg)


def verdict(drift_pct, tau, flat, leak):
    if drift_pct < -flat and tau < -0.3:
        return "falling"
    if abs(drift_pct) < flat or abs(tau) < 0.3:
        return "flat"
    if drift_pct > 0 and (drift_pct >= leak and tau >= 0.6):
        return "LEAKING"
    if drift_pct > 0:
        return "creeping"
    return "falling"


def col(rows, k):
    out = []
    for r in rows:
        try:
            out.append(float(r[k]))
        except (KeyError, ValueError):
            out.append(float("nan"))
    return out


def fmt(x):
    if abs(x) >= 1e6:
        return f"{x:,.0f}"
    if abs(x) >= 100:
        return f"{x:.0f}"
    return f"{x:.2f}"


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("csv")
    ap.add_argument("--skip", type=float, default=0.1, help="fraction of the run dropped as warm-up (default %(default)s)")
    ap.add_argument("--flat-pct", type=float, default=3.0, help="drift over the run under which a level is flat (default %(default)s)")
    ap.add_argument("--leak-pct", type=float, default=10.0, help="drift over the run from which a level leaks (default %(default)s)")
    ap.add_argument("--levels", default="f_rss,b_rss,unit_anon,f_thr,b_thr,f_fds,b_fds,wo,net,inv,save_bytes,backups,backups_bytes,home_bytes,tcp_admin,udp_rx,plugins_running", help="level columns")
    ap.add_argument("--counters", default="f_cpu,log_bytes,log_lines,s3_hits,hook_hits,plug_hits,bytes_in,bytes_out", help="cumulative columns")
    ap.add_argument("--windows", default="f_cpu_pct,b_cpu_pct,busy,tick_p50_us,tick_p99_us,tick_max_us,winmax_longest_us,st_ms_p50,st_ms_max,fps", help="per-window columns")
    a = ap.parse_args()
    rows = list(csv.DictReader(open(a.csv)))
    if len(rows) < 10:
        sys.exit("too few samples")
    t = col(rows, "t")
    span_h = (t[-1] - t[0]) / 3600
    cut = int(len(rows) * a.skip)
    print(f"samples {len(rows)}, span {span_h:.2f} h, warm-up dropped: {cut} samples ({a.skip:.0%})\n")

    print("## Levels (Theil-Sen over the run after warm-up)\n")
    print("| metric | median | min | max | slope /h | drift over run | tau | verdict |")
    print("|---|---|---|---|---|---|---|---|")
    for k in a.levels.split(","):
        ys = col(rows, k)
        pts = [(t[i] / 3600, ys[i]) for i in range(cut, len(rows)) if ys[i] == ys[i] and ys[i] >= 0]
        if len(pts) < 10:
            continue
        xs, vs = zip(*pts)
        sl, tau = theil_sen(list(xs), list(vs))
        med = statistics.median(vs)
        drift = 100 * sl * (xs[-1] - xs[0]) / med if med else 0.0
        print(f"| {k} | {fmt(med)} | {fmt(min(vs))} | {fmt(max(vs))} | {sl:+.4g} | {drift:+.1f} % | {tau:+.2f} | {verdict(drift, tau, a.flat_pct, a.leak_pct)} |")

    if "b_pid" in rows[0]:
        print("\n## The bank, inside each process (a flip or a restart starts a new one; first 5 min of each dropped)\n")
        segs = []
        cur, start = None, 0
        pid = col(rows, "b_pid")
        for i in range(len(rows) + 1):
            p = pid[i] if i < len(rows) else None
            if p != cur:
                if cur is not None:
                    segs.append((cur, start, i))
                cur, start = p, i
        print("| metric | segments >= 20 min | median slope /h | worst slope /h | median drift in segment | verdicts |")
        print("|---|---|---|---|---|---|")
        for k in ("b_rss", "b_thr", "b_fds"):
            ys = col(rows, k)
            res = []
            for p, s0, s1 in segs:
                if p <= 0 or t[s1 - 1] - t[s0] < 20 * 60:
                    continue
                pts = [(t[i] / 3600, ys[i]) for i in range(s0, s1) if t[i] - t[s0] >= 300 and ys[i] == ys[i] and ys[i] >= 0]
                if len(pts) < 20:
                    continue
                xs, vs = zip(*pts)
                sl, tau = theil_sen(list(xs), list(vs), cap=400)
                med = statistics.median(vs)
                res.append((sl, 100 * sl * (xs[-1] - xs[0]) / med if med else 0.0, tau, p))
            if res:
                vd = [verdict(d, tau, a.flat_pct, a.leak_pct) for _, d, tau, _ in res]
                print(f"| {k} | {len(res)} | {statistics.median(r[0] for r in res):+.4g} | {max(r[0] for r in res):+.4g} | {statistics.median(r[1] for r in res):+.1f} % | " + ", ".join(f"{v} {vd.count(v)}" for v in sorted(set(vd))) + " |")

    print("\n## Counters (rate per hour; last third against the first third)\n")
    print("| metric | total | per hour (whole) | first third /h | last third /h | ratio |")
    print("|---|---|---|---|---|---|")
    for k in a.counters.split(","):
        ys = col(rows, k)
        if all(y != y or y < 0 for y in ys):
            continue
        n = len(rows)
        i1, i2 = n // 3, 2 * n // 3

        def rate(i, j):
            dt = (t[j - 1] - t[i]) / 3600
            return (ys[j - 1] - ys[i]) / dt if dt > 0 else 0.0

        r_all, r1, r3 = rate(0, n), rate(0, i1), rate(i2, n)
        print(f"| {k} | {fmt(ys[-1] - ys[0])} | {fmt(r_all)} | {fmt(r1)} | {fmt(r3)} | {r3 / r1 if r1 else float('nan'):.2f} |")

    print("\n## Windows (the 30 s windows; hourly medians trended)\n")
    print("| metric | median | p95 | max | first hour median | last hour median | hourly-median slope /h | verdict |")
    print("|---|---|---|---|---|---|---|---|")
    for k in a.windows.split(","):
        ys = col(rows, k)
        pts = [(t[i], ys[i]) for i in range(len(rows)) if ys[i] == ys[i] and ys[i] >= 0]
        if len(pts) < 10:
            continue
        vs = sorted(v for _, v in pts)
        hours = {}
        for tt, v in pts:
            hours.setdefault(int(tt // 3600), []).append(v)
        hm = [(h + 0.5, statistics.median(v)) for h, v in sorted(hours.items()) if len(v) >= 20]
        sl, tau = theil_sen([h for h, _ in hm], [v for _, v in hm]) if len(hm) >= 4 else (0.0, 0.0)
        med = statistics.median(vs)
        drift = 100 * sl * (hm[-1][0] - hm[0][0]) / med if len(hm) >= 4 and med else 0.0
        print(f"| {k} | {fmt(med)} | {fmt(vs[int(.95 * (len(vs) - 1))])} | {fmt(vs[-1])} | {fmt(hm[0][1]) if hm else '-'} | {fmt(hm[-1][1]) if hm else '-'} | {sl:+.4g} | {verdict(drift, tau, 10, 30) if len(hm) >= 4 else '-'} |")

    print("\n## Per hour\n")
    print("| hour | f_rss MB | b_rss MB | wo | save KB | backups | home MB | log KB/h | threads f/b | fds f/b | busy % med | tick p99 us med | longest loop ms med | status ms max |")
    print("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|")
    by = {}
    for i, r in enumerate(rows):
        by.setdefault(int(t[i] // 3600), []).append(r)
    prev_log = None
    for h, rs in sorted(by.items()):
        g = lambda k: [float(r[k]) for r in rs if r.get(k) not in (None, "") and float(r[k]) >= 0]
        med = lambda k: statistics.median(g(k)) if g(k) else float("nan")
        lg = g("log_bytes")
        logh = (lg[-1] - (prev_log if prev_log is not None else lg[0])) / 1024 if lg else float("nan")
        prev_log = lg[-1] if lg else prev_log
        print(f"| {h} | {med('f_rss') / 1024:.1f} | {med('b_rss') / 1024:.1f} | {med('wo'):.0f} | {med('save_bytes') / 1024:.0f} | {med('backups'):.0f} | {med('home_bytes') / 1048576:.1f} | {logh:.0f} | {med('f_thr'):.0f}/{med('b_thr'):.0f} | {med('f_fds'):.0f}/{med('b_fds'):.0f} | {med('busy'):.1f} | {med('tick_p99_us'):.0f} | {med('winmax_longest_us') / 1000:.1f} | {max(g('st_ms_max') or [float('nan')]):.0f} |")


if __name__ == "__main__":
    main()
