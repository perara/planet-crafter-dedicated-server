#!/usr/bin/env bash
# Did a commit change what the server sends or saves? Builds the test binary at each commit in
# a git worktree of its own, runs the tick benchmark (server/src/bench.rs) with a fixed seed
# and clock twice (players only walking, and acting every 200 ms), keeps the message-and-save
# dump of each run, prints the fingerprint lines, and compares each commit's dump with the
# next one's (dumpcmp.py). To find the commit an optimization or a fix changed the output at
# (bisecting by hand), and to prove one did not.
#
#   tools/parity/bench-compare.sh -w WORKTREE [-o OUT_DIR] COMMIT COMMIT...
#
#   -w   a git worktree of this repository that nothing else uses: each COMMIT is checked out in it
#        (detached); it must be clean. Make one with `git worktree add --detach DIR`, remove it after.
#   -o   where the dumps go (default ./bench-compare): COMMIT-0.txt and COMMIT-200.txt
#   COMMIT  two or more, oldest first
#
# Requires: cargo, python3; PCDS_TEST_DATA (the game tables) and PCDS_BENCH_SAVE (a late-game
# save to run on; none is shipped). Optional: PCDS_BENCH_PLAYERS (default 4), PCDS_BENCH_SECONDS
# (default 300), CARGO_TARGET_DIR (shared across commits, default WORKTREE/server/target).
# Builds run under `heavy agent-run` when installed (the shared host's queue).
# A pass: for every neighbouring pair the lines "positions that differ: 0" and "entries
# differing 0" (and equal "fingerprint ..." values in the printed run lines); a pair that
# differs is where the output changed. Takes a build and 2 x 5 minutes per commit.
set -euo pipefail

usage() { awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 2; }
wt="" out=./bench-compare
while getopts w:o:h opt; do
  case $opt in
    w) wt=$OPTARG ;;
    o) out=$OPTARG ;;
    *) usage ;;
  esac
done
shift $((OPTIND - 1))
[[ -n $wt && $# -ge 2 ]] || usage
: "${PCDS_TEST_DATA:?PCDS_TEST_DATA: the game tables}" "${PCDS_BENCH_SAVE:?PCDS_BENCH_SAVE: a save to run on}"
here=$(cd "$(dirname "$0")" && pwd)
wt=$(cd "$wt" && pwd)
mkdir -p "$out"
out=$(cd "$out" && pwd)
export PCDS_RNG_SEED=1 PCDS_BENCH_PLAYERS=${PCDS_BENCH_PLAYERS:-4} PCDS_BENCH_SECONDS=${PCDS_BENCH_SECONDS:-300}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$wt/server/target}

heavy=()
command -v heavy > /dev/null && command -v agent-run > /dev/null && heavy=(heavy agent-run planetcrafter --cpus 8 --)
limit=()
command -v agent-run > /dev/null && limit=(agent-run planetcrafter --cpus 4 --)

[[ -z $(git -C "$wt" status --porcelain) ]] || { echo "bench-compare: $wt is not clean" >&2; exit 1; }
done_commits=()
for c in "$@"; do
  git -C "$wt" checkout -q --detach "$c"
  short=$(git -C "$wt" rev-parse --short "$c")
  echo "== $short $(git -C "$wt" log -1 --format=%s)"
  exe=$(cd "$wt/server" && "${heavy[@]}" cargo test --release --bin pcds --no-run --message-format=json \
    | python3 -c '
import json, sys
for line in sys.stdin:
    try:
        m = json.loads(line)
    except ValueError:
        continue
    if m.get("executable") and m.get("profile", {}).get("test"):
        print(m["executable"])' | tail -n 1)
  [[ -x $exe ]] || { echo "bench-compare: no test executable built at $short" >&2; exit 1; }
  lib=$(ls -t "$CARGO_TARGET_DIR"/release/build/steamworks-sys-*/out/libsteam_api.so 2> /dev/null | head -n 1 || true)
  if [[ -n $lib ]]; then
    libdir=$(dirname "$lib")
    export LD_LIBRARY_PATH=$libdir
  fi
  for ms in 0 200; do
    echo "$short action_ms=$ms: $(PCDS_BENCH_ACTION_MS=$ms PCDS_BENCH_DUMP="$out/$short-$ms.txt" "${limit[@]}" "$exe" bench::bench --ignored --nocapture 2>&1 \
      | grep -E '^bench: [0-9]+ s|panicked' | sed 's/.*messages, //')"
  done
  done_commits+=("$short")
done

for ((i = 1; i < ${#done_commits[@]}; i++)); do
  a=${done_commits[i - 1]} b=${done_commits[i]}
  for ms in 0 200; do
    echo "== action_ms=$ms: $a vs $b"
    python3 "$here/dumpcmp.py" "$out/$a-$ms.txt" "$out/$b-$ms.txt" 4
  done
done
