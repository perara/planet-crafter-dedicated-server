#!/usr/bin/env bash
# Replays probe scripts on this server, in parallel workers, against a reference run that is
# already stored (the real game's transcripts and saves from `sweep.sh both`). After a fix or a
# new build no game host is started: every script is replayed from a fresh start save on its own
# copy of the server and compared with the stored reference (parity.py, savediff.py). The
# scripts are shared out over the workers by their step counts, longest first. Replaces the
# per-pass copies of this driver (ours3.sh, rerun.sh, par.sh): one script, the pass as arguments.
#
#   tools/parity/replay-ours.sh [-n WORKERS] [-p PORT_BASE] [-o OUT_DIR] RUN_DIR SCRIPT.json...
#
#   RUN_DIR   the stored reference run: NAME.ref.jsonl (NAME.A.ref.jsonl and NAME.B.ref.jsonl for a
#             pair), NAME.ref.save.json
#   SCRIPT    probe scripts (sweep.py gen); NAME.B.json is replayed with NAME.A.json
#   -n        workers (default 3; the host is shared: at most 6)
#   -p        first test port; worker i takes PORT_BASE+i (default 8424, ports stay in 8420-8439)
#   -o        where the results go (default RUN_DIR): NAME.ours.jsonl, NAME.ours.save.json,
#             NAME.ours.log, NAME.diff, NAME.savediff; the reference files are not touched
#
# Requires: the same environment as sweep.sh: DATA_DIR (the game tables), START (the start save
# the reference ran from), PCDS (default: this checkout's release build). REF_DIR is not needed.
# Worker logs end up in OUT_DIR/replay-wN.log. Runs under agent-run when it is installed.
# A pass: one line per script, "NAME: 0 difference(s)" and "save: 0 save difference(s)" or only
# differences the sweep lists as deviations (sweep.py report classifies them); exit 0 when every
# worker finished (not when the replays agree: read the lines).
set -euo pipefail

usage() { awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 2; }
workers=3 port=8424 out=""
while getopts n:p:o:h opt; do
  case $opt in
    n) workers=$OPTARG ;;
    p) port=$OPTARG ;;
    o) out=$OPTARG ;;
    *) usage ;;
  esac
done
shift $((OPTIND - 1))
[[ $# -ge 2 ]] || usage
run=$1
shift
out=${out:-$run}
: "${DATA_DIR:?DATA_DIR: the game tables}" "${START:?START: the start save of the reference run}"
here=$(cd "$(dirname "$0")" && pwd)
sweep=$here/../../server/tests/parity/sweep.sh
export REF_DIR=${REF_DIR:-/nonexistent} DATA_DIR START
export PCDS=${PCDS:-$here/../../server/target/release/pcds}
mkdir -p "$out"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

limit=()
command -v agent-run > /dev/null && limit=(agent-run planetcrafter --cpus 4 --)

# "worker<TAB>script", longest scripts first, each to the least loaded worker.
plan=$(python3 - "$workers" "$@" << 'PY'
import json, sys
n = int(sys.argv[1])
scripts = [s for s in sys.argv[2:] if not s.endswith('.B.json')]
load = [0] * n
for steps, s in sorted(((len(json.load(open(s)).get('steps', [])), s) for s in scripts), reverse=True):
    i = load.index(min(load))
    load[i] += steps
    print(f'{i}\t{s}')
PY
)

pids=()
for ((i = 0; i < workers; i++)); do
  d=$work/w$i
  mkdir -p "$d"
  files=()
  while IFS=$'\t' read -r w s; do
    [[ $w == "$i" ]] || continue
    files+=("$s")
    name=$(basename "$s" .json)
    base=${name%.A}
    for f in "$run/$base".*ref.*; do [[ -e $f ]] && cp "$f" "$d/"; done
  done <<< "$plan"
  [[ ${#files[@]} -gt 0 ]] || continue
  OURS_PORT=$((port + i)) "${limit[@]}" "$sweep" ours "$d" "${files[@]}" > "$work/w$i.log" 2>&1 &
  pids+=($!)
done

status=0
for p in "${pids[@]}"; do wait "$p" || status=1; done
for ((i = 0; i < workers; i++)); do
  [[ -d $work/w$i ]] || continue
  for f in "$work/w$i"/*; do
    case $f in *.ref.* | *.ref2.* | *.noise | *.savenoise) ;; *) cp -f "$f" "$out/" ;; esac
  done
  cp -f "$work/w$i.log" "$out/replay-w$i.log"
  cat "$work/w$i.log"
done
exit "$status"
