#!/usr/bin/env bash
# A/B repeat of one test between two builds: is the new build's test faster, and still green?
#
# Runs one test of two cargo test binaries alternately, N times each, and prints the test's
# result line and the CPU and wall time of every run, so a slow or flaky test shows up as a
# spread rather than as one number.
#
#   tools/perf/abtest.sh TEST BIN_A BIN_B [N]
#     TEST   a test name filter, e.g. the_world_moves (as `cargo test` takes it)
#     BIN_A  a test binary (cargo test --release --no-run prints its path on the "Executable"
#            line; e.g. server/target/release/deps/hot_update-<hash>), the base build
#     BIN_B  the same test binary built from the commit under test
#     N      runs of each, alternating (default 3)
#     LD_LIBRARY_PATH  must find libsteam_api.so (the steamworks crate's build output) when the
#                      tests start servers; PCDS_TEST_DATA the game tables (default run/data/2.103)
#
# Needs: two built test binaries, the game tables. Nothing is built or installed.
# A pass: exit status 0 and, per run, a "test result: ok" line for both A and B.
set -euo pipefail

if [ $# -lt 3 ] || [ "${1:-}" = -h ] || [ "${1:-}" = --help ]; then
  sed -n '2,/^set -euo/p' "$0" | sed '$d;s/^# \{0,1\}//'
  [ $# -ge 3 ] && exit 0 || exit 2
fi
test=$1 a=$2 b=$3 runs=${4:-3}
repo=$(cd "$(dirname "$0")/../.." && pwd)
export PCDS_TEST_DATA=${PCDS_TEST_DATA:-$repo/run/data/2.103}
for bin in "$a" "$b"; do [ -x "$bin" ] || { echo "abtest: $bin: not an executable" >&2; exit 2; }; done

status=0
TIMEFORMAT="user %U s sys %S s wall %R s"
for ((i = 1; i <= runs; i++)); do
  for side in A B; do
    bin=$a
    [ "$side" = A ] || bin=$b
    out=$({ time "$bin" "$test" 2>&1; } 2>&1 || true)
    result=$(printf '%s\n' "$out" | grep -E '^test result|panicked' | head -3 | tr '\n' ' ')
    timing=$(printf '%s\n' "$out" | tail -1)
    echo "$side run $i: ${result:-no result line} | $timing"
    case $result in *"test result: ok"*) ;; *) status=1 ;; esac
  done
done
exit $status
