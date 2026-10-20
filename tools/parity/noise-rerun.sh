#!/usr/bin/env bash
# The stability re-run of the sweep: the real game once more for the scripts that still show a
# difference against this server, to tell the game's own randomness and timing (class d) from a
# real difference. For each script of SCRIPT_DIR whose NAME.diff in RUN_DIR does not end in
# "0 difference(s)" and that has no NAME.noise yet, `sweep.sh ref2` runs the reference again
# from the same start save and writes NAME.ref2.jsonl, NAME.noise (the game against itself),
# NAME.savenoise. A difference of the same kind in the noise is the game's own.
#
#   tools/parity/noise-rerun.sh [--all] RUN_DIR SCRIPT_DIR
#
#   --all   rerun every script of SCRIPT_DIR that has no NAME.noise, differences or not
#
# Requires: the Wine reference as for sweep.sh: REF_DIR, DATA_DIR, START (and optionally
# REF_PORT/REF_API, default 8420/8421 here; PCDS_KEEP_ACTIVE=1 for the sweep's reference). One
# reference at a time: do not run it beside another sweep.sh both|ref2. Runs under agent-run
# when it is installed.
# A pass: it lists the scripts it reruns, then sweep.sh's "NAME noise: N difference(s)" lines;
# a script with nothing left to rerun is not listed. Exit 0 also when there was nothing to do.
set -euo pipefail

usage() { awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 2; }
all=0
if [[ ${1:-} == --all ]]; then all=1; shift; fi
[[ $# -eq 2 && $1 != -h && $1 != --help ]] || usage
run=$1 scripts=$2
: "${REF_DIR:?REF_DIR: the Wine reference}" "${DATA_DIR:?DATA_DIR: the game tables}" "${START:?START: the start save}"
export REF_DIR DATA_DIR START REF_PORT=${REF_PORT:-8420} REF_API=${REF_API:-8421}
sweep=$(cd "$(dirname "$0")" && pwd)/../../server/tests/parity/sweep.sh

limit=()
command -v agent-run > /dev/null && limit=(agent-run planetcrafter --cpus 6 --)

todo=()
shopt -s nullglob
for s in "$scripts"/*.json; do
  [[ $s == *.B.json || $s == */rows.json ]] && continue
  name=$(basename "$s" .json)
  name=${name%.A}
  if [[ -e $run/$name.noise || -e $run/$name.A.noise ]]; then continue; fi
  differs=0
  for d in "$run/$name.diff" "$run/$name.A.diff" "$run/$name.B.diff"; do
    [[ -f $d ]] && [[ $(tail -n 1 "$d") != "0 difference(s)" ]] && differs=1
  done
  if [[ $all == 1 || $differs == 1 ]]; then todo+=("$s"); fi
done
echo "${#todo[@]} script(s) to run against the reference again"
[[ ${#todo[@]} -gt 0 ]] || exit 0
printf '  %s\n' "${todo[@]}"
exec "${limit[@]}" "$sweep" ref2 "$run" "${todo[@]}"
