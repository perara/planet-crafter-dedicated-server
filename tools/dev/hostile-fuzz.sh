#!/usr/bin/env bash
# The hostile-RPC test for longer than the suite runs it: every server RPC with extreme values,
# other players' ids, huge strings, truncated bodies and trailing garbage, from two joined
# players, with ticks between (game::tests::hostile_rpcs_do_not_crash_the_server). The suite
# runs 1500 rounds on one seed; this runs many rounds on several seeds. A seed that panics prints
# the RPC and the source line and fails; the seed reproduces it.
#
#   tools/dev/hostile-fuzz.sh [-r ROUNDS] [SEED...]
#
#   -r      rounds per seed (default 30000)
#   SEED    default: 101 202 303 404 505 606 707 808
#
# Requires: cargo, PCDS_TEST_DATA (the game tables; without them the test skips silently, which
# this script refuses). The build is the repository's own (server/), release profile. Runs under
# agent-run when it is installed. Environment read by the test: PCDS_FUZZ_ROUNDS, PCDS_FUZZ_SEED.
# A pass: "== seed N" followed by "test result: ok. 1 passed" for every seed and no PANIC line;
# exit 0. A failure prints the seed's PANIC / panicked lines and the exit code is 1.
set -euo pipefail

usage() { awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 2; }
rounds=30000
while getopts r:h opt; do
  case $opt in
    r) rounds=$OPTARG ;;
    *) usage ;;
  esac
done
shift $((OPTIND - 1))
seeds=("$@")
[[ ${#seeds[@]} -gt 0 ]] || seeds=(101 202 303 404 505 606 707 808)
: "${PCDS_TEST_DATA:?PCDS_TEST_DATA: the game tables (the test skips without them)}"
[[ $PCDS_TEST_DATA != none ]] || { echo "hostile-fuzz: PCDS_TEST_DATA=none skips the test; point it at the tables" >&2; exit 2; }
root=$(cd "$(dirname "$0")/../.." && pwd)

limit=()
command -v agent-run > /dev/null && limit=(agent-run planetcrafter --cpus 4 --)

status=0
for seed in "${seeds[@]}"; do
  echo "== seed $seed ($rounds rounds)"
  log=$(mktemp)
  if PCDS_FUZZ_ROUNDS=$rounds PCDS_FUZZ_SEED=$seed "${limit[@]}" cargo test --release --manifest-path "$root/server/Cargo.toml" \
    --bin pcds game::tests::hostile_rpcs -- --nocapture > "$log" 2>&1; then
    grep -E 'test result' "$log" | head -n 1
  else
    status=1
    grep -E 'PANIC|panicked|test result|^error' "$log" | head -n 20
  fi
  rm -f "$log"
done
exit "$status"
