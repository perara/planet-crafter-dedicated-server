#!/usr/bin/env bash
# This server as the parity sweep runs it: a copy of a start save, direct IP on a test port, no
# Steam, no admin API, autosave off, loopback players believed (what sweep.sh `ours` starts for a
# script). For replaying a probe by hand against this build, next to ref-host.sh's reference.
#
#   tools/parity/ours-host.sh start START.json [PORT]
#   tools/parity/ours-host.sh stop [PORT]
#
#   start   copies START.json to OURS_DIR/PORT/world.json and starts `pcds serve` on it
#   stop    SIGTERM to the pid it started (saves the world to OURS_DIR/PORT/world.json), waits up to 30 s
#
# Requires: a release build (make build), DATA_DIR: the game tables (run/data/<version>).
# Environment: DATA_DIR (required), PCDS (default server/target/release/pcds of this checkout),
# OURS_DIR (default $TMPDIR/pcds-ours; one folder per port), CPUS (default 2, used when agent-run
# is installed). PORT defaults to 8422 (test ports stay in 8420-8439; never the game's 7777).
# A pass: `start` prints the server's first log lines and it answers `pcds probe 127.0.0.1:PORT
# DATA_DIR SCRIPT.json`; `stop` prints "stopped" and OURS_DIR/PORT/world.json is the end state.
set -euo pipefail

usage() { awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 2; }
[[ $# -ge 1 ]] || usage
case $1 in -h | --help) usage ;; esac
here=$(cd "$(dirname "$0")" && pwd)
: "${DATA_DIR:?DATA_DIR: the game tables (see --help)}"
PCDS=${PCDS:-$here/../../server/target/release/pcds}
OURS_DIR=${OURS_DIR:-${TMPDIR:-/tmp}/pcds-ours}

limit=()
command -v agent-run > /dev/null && limit=(agent-run planetcrafter --cpus "${CPUS:-2}" --)

case $1 in
  start)
    [[ $# -ge 2 ]] || usage
    port=${3:-8422}
    o=$OURS_DIR/$port
    rm -rf "$o"
    mkdir -p "$o"
    cp "$2" "$o/world.json"
    echo '{"save": {"autosaveMinutes": 0}}' > "$o/rules.json"
    PCDS_LISTEN=127.0.0.1:$port PCDS_STEAM=off PCDS_ADMIN=off PCDS_DEV=trust-loopback \
      "${limit[@]}" "$PCDS" serve "$DATA_DIR" "$o/world.json" > "$o/server.log" 2>&1 < /dev/null &
    echo $! > "$o/pid"
    sleep 2
    tail -n 2 "$o/server.log"
    ;;
  stop)
    port=${2:-8422}
    o=$OURS_DIR/$port
    pid=$(cat "$o/pid")
    kill -TERM "$pid" 2> /dev/null || true
    for _ in $(seq 30); do
      kill -0 "$pid" 2> /dev/null || break
      sleep 1
    done
    rm -f "$o/pid"
    echo stopped
    ;;
  *) usage ;;
esac
