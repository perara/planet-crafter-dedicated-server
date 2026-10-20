#!/usr/bin/env bash
# The quickest end-to-end check of a build: a new world, served on a test port with direct IP
# only, joined by the world probe (a scripted player: join, spawn, walk), stopped. Shows the
# server log, the number of messages the probe got, and whether the server shut down cleanly.
#
#   tools/dev/probe-smoke.sh [PROBE_SCRIPT.json]
#
#   PROBE_SCRIPT   default server/probes/world.json
#
# Requires: a release build (make build, or PCDS=path to pcds) and DATA_DIR: the game tables
# (default $PCDS_TEST_DATA). The world is made with `pcds new-save` in a temporary folder, which is
# removed at the end. Port: PORT (default 8426; test ports stay in 8420-8439, never 7777).
# Only the server process this script started is stopped.
# A pass: "probe exit 0", a non-zero "probe messages" count and "server exit 0".
set -euo pipefail

usage() { awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 2; }
[[ ${1:-} != -h && ${1:-} != --help && $# -le 1 ]] || usage
root=$(cd "$(dirname "$0")/../.." && pwd)
pcds=${PCDS:-$root/server/target/release/pcds}
data=${DATA_DIR:-${PCDS_TEST_DATA:-}}
[[ -n $data && $data != none ]] || { echo "probe-smoke: DATA_DIR (the game tables) is needed" >&2; exit 2; }
script=${1:-$root/server/probes/world.json}
port=${PORT:-8426}
work=$(mktemp -d)
server=""
cleanup() {
  [[ -z $server ]] || kill "$server" 2> /dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

"$pcds" new-save "$work/world.json" "$data" > "$work/new-save.log" 2>&1 || { cat "$work/new-save.log" >&2; exit 1; }
PCDS_LISTEN=127.0.0.1:$port PCDS_STEAM=off PCDS_ADMIN=off PCDS_DEV=trust-loopback \
  "$pcds" serve "$data" "$work/world.json" > "$work/server.log" 2>&1 &
server=$!
sleep 3
rc=0
timeout 60 "$pcds" probe "127.0.0.1:$port" "$data" "$script" > "$work/probe.log" 2>&1 || rc=$?
echo "probe exit $rc"
echo "probe messages: $(grep -c '"msg"' "$work/probe.log" || true)"
kill -TERM "$server" 2> /dev/null || true
src=0
wait "$server" || src=$?
server=""
echo "server exit $src"
echo "--- server log (last 15 lines)"
tail -n 15 "$work/server.log"
[[ $rc -eq 0 && $src -eq 0 ]]
