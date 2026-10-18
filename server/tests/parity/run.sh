#!/bin/bash
# Runs one probe script against the real game host and against this server and compares what
# each sent back (README "Parity with the game").
#
#   server/tests/parity/run.sh REF_ADDR OURS_ADDR DATA_DIR START_SAVE SCRIPT OUT_DIR
#
# Both servers must already run the same world, loaded from START_SAVE (the reference hosts it
# as its save, ours as world.json), with nobody else connected. The probe joins each over
# direct IP; OUT_DIR gets NAME.ref.jsonl, NAME.ours.jsonl and NAME.diff. Afterwards, compare
# the saves each server wrote with savediff.py START_SAVE REF_SAVE OURS_SAVE.
set -euo pipefail
[[ $# -eq 6 ]] || { sed -n '2,10p' "$0"; exit 2; }
ref=$1 ours=$2 data=$3 start=$4 script=$5 out=$6
here=$(cd "$(dirname "$0")" && pwd)
pcds=${PCDS:-$here/../../target/release/pcds}
name=$(basename "$script" .json)
mkdir -p "$out"
timeout "${PROBE_TIMEOUT:-600}" "$pcds" probe "$ref" "$data" "$script" > "$out/$name.ref.jsonl"
timeout "${PROBE_TIMEOUT:-600}" "$pcds" probe "$ours" "$data" "$script" > "$out/$name.ours.jsonl"
python3 "$here/parity.py" "$data" "$start" "$out/$name.ref.jsonl" "$out/$name.ours.jsonl" > "$out/$name.diff" || true
tail -1 "$out/$name.diff"
