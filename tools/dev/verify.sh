#!/usr/bin/env bash
# The whole verification of a checkout in one go, for the integrator who merges several lanes
# and wants one verdict: the test suite with the game tables, the suite as CI runs it (no
# tables, no game), clippy, the completeness map's consistency check, and, when a save is
# given, the tick benchmark's fingerprint and the hot-update handoff's equality. Output is a
# summary, not the logs: the lines that matter, section by section.
#
#   tools/dev/verify.sh [-o FILE]
#
#   -o    write the summary to FILE as well as to the terminal
#
# Requires: cargo (the repository's toolchain), python3, and PCDS_TEST_DATA: the game tables
# (run/data/<version> of an installed server). Optional: PCDS_BENCH_SAVE, a late-game save (none
# is shipped): adds the bench sections (PCDS_BENCH_PLAYERS default 4, PCDS_BENCH_SECONDS
# default 300, PCDS_RNG_SEED 1). Heavy: use CARGO_TARGET_DIR to share builds; under `heavy
# agent-run` when installed (the shared host's queue). Does not touch anything outside the
# build folders.
# A pass: every "test result:" line says "ok" (no FAILED / panicked line), clippy prints 0,
# map.py prints its OK line, and the bench prints the same "fingerprint" as before the change
# plus "EQUAL" for the handoff. "== DONE" ends the summary (missing = the run died).
set -euo pipefail

usage() { awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 2; }
file=""
while getopts o:h opt; do
  case $opt in
    o) file=$OPTARG ;;
    *) usage ;;
  esac
done
: "${PCDS_TEST_DATA:?PCDS_TEST_DATA: the game tables}"
root=$(cd "$(dirname "$0")/../.." && pwd)
manifest=$root/server/Cargo.toml

heavy=()
command -v heavy > /dev/null && command -v agent-run > /dev/null && heavy=(heavy agent-run planetcrafter --cpus 8 --)
limit=()
command -v agent-run > /dev/null && limit=(agent-run planetcrafter --cpus 4 --)

summary() {
  echo "== full suite (tables: $PCDS_TEST_DATA)"
  "${heavy[@]}" cargo test --release --manifest-path "$manifest" --no-fail-fast 2>&1 | grep -E '^test result|FAILED|panicked|^error' || true
  echo "== CI-style (no tables, no game)"
  PCDS_TEST_DATA=none PCDS_GAME_DIR=none "${heavy[@]}" cargo test --release --manifest-path "$manifest" --no-fail-fast 2>&1 \
    | grep -E '^test result|FAILED|panicked|^error' || true
  echo "== clippy (warnings and errors)"
  "${limit[@]}" cargo clippy --release --locked --manifest-path "$manifest" 2>&1 | grep -cE '^(warning|error)' || true
  echo "== completeness map"
  python3 "$root/server/tests/parity/completeness/map.py" check 2>&1 | tail -n 1
  if [[ -n ${PCDS_BENCH_SAVE:-} ]]; then
    export PCDS_RNG_SEED=1 PCDS_BENCH_PLAYERS=${PCDS_BENCH_PLAYERS:-4} PCDS_BENCH_SECONDS=${PCDS_BENCH_SECONDS:-300}
    echo "== bench"
    "${limit[@]}" cargo test --release --manifest-path "$manifest" --bin pcds bench::bench -- --ignored --nocapture 2>&1 \
      | grep -iE 'fingerprint|panicked' || true
    "${limit[@]}" cargo test --release --manifest-path "$manifest" --bin pcds bench::handoff -- --ignored --nocapture 2>&1 \
      | grep -iE 'EQUAL|panicked|handed-off' || true
  fi
  echo "== DONE"
}

if [[ -n $file ]]; then summary 2>&1 | tee "$file"; else summary 2>&1; fi
