#!/usr/bin/env bash
# parity.py and savediff.py again on the transcripts and saves already in a run folder, with no
# host started: after a change to parity.py, to the comparison's rules, or to hand a run to
# someone who only has the files. Optionally the sweep matrix and the class counts after it.
#
#   tools/parity/rediff.sh RUN_DIR [GEN_DIR]
#
#   RUN_DIR   a folder of `sweep.sh both|ours` results: NAME.ref.jsonl with NAME.ours.jsonl (and the
#             .A/.B pairs), NAME.ref.save.json with NAME.ours.save.json
#   GEN_DIR   the folder `sweep.py gen` wrote (the scripts and rows.json): with it, `sweep.py report`
#             then writes RUN_DIR/matrix.md, classified.json and components.md, and the number of
#             differences per class (a: our bug, b: listed deviation, c: new safety deviation, d:
#             the game's own randomness, e: not comparable) is printed
#
# Requires: python3; DATA_DIR (the game tables) and START (the start save the run used).
# A pass: every NAME.diff ends in "0 difference(s)" or only differences classified b-e; the class
# count has no `unclassified` (and no `a`).
set -euo pipefail

usage() { awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 2; }
[[ $# -ge 1 && $# -le 2 && $1 != -h && $1 != --help ]] || usage
run=$1
gen=${2:-}
: "${DATA_DIR:?DATA_DIR: the game tables}" "${START:?START: the start save of the run}"
parity=$(cd "$(dirname "$0")" && pwd)/../../server/tests/parity

shopt -s nullglob
for f in "$run"/*.ref.jsonl; do
  b=${f%.ref.jsonl}
  [[ -f $b.ours.jsonl ]] || continue
  python3 "$parity/parity.py" "$DATA_DIR" "$START" "$f" "$b.ours.jsonl" > "$b.diff" 2>&1 || true
  echo "$(basename "$b"): $(tail -n 1 "$b.diff")"
  name=${b%.A}
  name=${name%.B}
  if [[ $b != "$name.B" && -f $name.ref.save.json && -f $name.ours.save.json ]]; then
    python3 "$parity/savediff.py" "$START" "$name.ref.save.json" "$name.ours.save.json" > "$name.savediff" 2>&1 || true
  fi
done

if [[ -n $gen ]]; then
  python3 "$parity/sweep.py" report "$DATA_DIR" "$gen" "$run"
  python3 - "$run/classified.json" << 'PY'
import collections, json, sys
print(dict(collections.Counter(x['class'] for x in json.load(open(sys.argv[1])))))
PY
fi
