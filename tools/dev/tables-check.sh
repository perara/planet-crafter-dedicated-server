#!/usr/bin/env bash
# Checks that the program reads a game version's tables the way the reference tables say: reads
# the installed game with the built-in reader (`pcds read-game`) and compares the result
# section by section with a folder of tables known to be right (`pcds tables-diff`; integers
# exactly, floats within 1e-5). For a new game version, after a change in server/src/native/,
# and to see which sections moved when the game updates.
#
#   tools/dev/tables-check.sh GAME_DIR EXPECTED_TABLES [SECTION [SHOWN_PER_FILE]]
#
#   GAME_DIR          the game's folder (the one with Planet Crafter.exe)
#   EXPECTED_TABLES   a folder of tables to compare with (run/data/<version> of an installed server,
#                     or tables the reference game wrote)
#   SECTION           only this section (gamedata, netdata, rpcs, scenes, wrecks ...)
#   SHOWN_PER_FILE    how many first differences to show per file (default 10)
#
# Requires: a release build (make build) or PCDS=path to pcds. The read needs a few minutes and
# up to 8 GB of address space (capped with ulimit). The tables it writes go to OUT_DIR
# (default: a new temporary folder, removed at the end; set OUT_DIR to keep them).
# A pass: tables-diff ends without a difference and the script exits 0; otherwise it lists the
# first differences by JSON path and exits 1.
set -euo pipefail

usage() { awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 2; }
[[ $# -ge 2 && $# -le 4 && $1 != -h && $1 != --help ]] || usage
game=$1 expected=$2 section=${3:-} shown=${4:-10}
root=$(cd "$(dirname "$0")/../.." && pwd)
pcds=${PCDS:-$root/server/target/release/pcds}
if [[ -z ${OUT_DIR:-} ]]; then
  OUT_DIR=$(mktemp -d)
  trap 'rm -rf "$OUT_DIR"' EXIT
fi
mkdir -p "$OUT_DIR"

limit=()
command -v agent-run > /dev/null && limit=(agent-run planetcrafter --cpus 4 --)

echo "reading $game into $OUT_DIR"
(ulimit -v 8000000 && timeout 900 "${limit[@]}" "$pcds" read-game "$game" "$OUT_DIR")
args=("$expected" "$OUT_DIR")
if [[ -n $section ]]; then args+=("$section" "$shown"); fi
"$pcds" tables-diff "${args[@]}"
