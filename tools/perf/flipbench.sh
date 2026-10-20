#!/usr/bin/env bash
# Flip benchmark: how long the world stands still when a hot update hands it to the other bank.
#
# Two benchmarks (both are ignored cargo tests, see docs/hot-update.md):
#   default   bench::flipbench (server/src/bench.rs): in one process, per world, N flips of
#             the state: serialize, memfd write, fd pass, memfd read, each restore step, and the
#             sum (the "stand"), as p50 / p95 / max in ms. Also what a new bank loads.
#   --live    the flipbench of server/tests/hot_update.rs: the real front, two banks and the
#             real sockets, players playing; prints the stand and where it went (the front's
#             timeline) per world.
# Grow a save to a late-game size first with tools/grow-world.py SAVE OUT N.
#
#   tools/perf/flipbench.sh [--live] [--n FLIPS] [--players P] [--threads T] SAVE...
#     --n         flips per world (default 15 live, 20 in-process)
#     --players   scripted players playing during the flips (default 8)
#     --threads   PCDS_DEV=flip-threads=T: the threads a flip uses (default: the cores, at most 8)
#     SAVE...     the worlds (save files) whose flip is timed
#     PCDS_TEST_DATA  the game tables (default run/data/2.103 of the repository)
#
# Needs: cargo (builds the tests once), the game tables, saves of your own.
# A pass: exit status 0 and one "flipbench:" block per world; the in-process run prints the
# per-part times, the live one a line per world. Compare the numbers between builds.
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
live=0 n="" players=8 threads=""
while [ $# -gt 0 ]; do
  case $1 in
    --live) live=1 ;;
    --n) n=${2:?--n needs a number}; shift ;;
    --players) players=${2:?--players needs a number}; shift ;;
    --threads) threads=${2:?--threads needs a number}; shift ;;
    -h | --help) sed -n '2,/^set -euo/p' "$0" | sed '$d;s/^# \{0,1\}//'; exit 0 ;;
    --*) echo "flipbench: unknown option $1 (try --help)" >&2; exit 2 ;;
    *) break ;;
  esac
  shift
done
[ $# -gt 0 ] || { echo "flipbench: name at least one save (try --help)" >&2; exit 2; }
saves=""
for s in "$@"; do
  [ -f "$s" ] || { echo "flipbench: $s: no such save" >&2; exit 2; }
  saves="$saves,$(realpath "$s")"
done
[ -n "$n" ] || { if [ "$live" = 1 ]; then n=15; else n=20; fi; }

export PCDS_TEST_DATA=${PCDS_TEST_DATA:-$repo/run/data/2.103}
export PCDS_FLIPBENCH_SAVES=${saves#,}
export PCDS_FLIPBENCH_N=$n
export PCDS_FLIPBENCH_PLAYERS=$players
[ -z "$threads" ] || export PCDS_DEV="flip-threads=$threads"

# libsteam_api.so is in the steamworks crate's build output (the Makefile does the same).
steam=$(ls -t "$repo"/server/target/release/build/steamworks-sys-*/out/libsteam_api.so 2>/dev/null | head -1 || true)
if [ -n "$steam" ]; then
  steam_dir=$(dirname "$steam")
  export LD_LIBRARY_PATH="$steam_dir${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
fi

if [ "$live" = 1 ]; then
  cargo test --manifest-path "$repo/server/Cargo.toml" --release --locked --test hot_update flipbench -- --ignored --nocapture --test-threads=1
else
  cargo test --manifest-path "$repo/server/Cargo.toml" --release --locked --bin pcds bench::flipbench -- --ignored --nocapture --test-threads=1
fi 2>&1 | grep -E '^flipbench|panicked|^error' || { echo "flipbench: the run failed or printed no result lines" >&2; exit 1; }
