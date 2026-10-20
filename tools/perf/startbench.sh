#!/usr/bin/env bash
# Start benchmark: the CPU cost of starting a server on a world, until it runs the world.
#
# Starts the front (it starts the bank) on a copy of SAVE with `perf stat` counting user
# instructions and user CPU time of both processes, waits for the bank's "running the world"
# line, stops the server through its admin API and prints what the start cost. User
# instructions do not depend on how busy the machine is: compare them between builds.
# Several runs show the spread.
#
#   tools/perf/startbench.sh --world SAVE [--bin PCDS] [--data TABLES] [--runs 3] [--port 8430]
#     --bin    the program (default server/target/release/pcds)
#     --data   the game tables (default $PCDS_TEST_DATA, else run/data/2.103)
#     WORK     a scratch folder (env, default: a new folder in the system's temp, removed at the end)
#
# Needs: perf(1) with permission to count user events, curl, a built pcds, the game tables, a
# save of your own (copied, never written). Linux. Uses ports --port and --port + 1 on loopback
# (default 8430-8431) and refuses to start when the admin port is already taken.
# A pass: exit status 0 and, per run, "ready after ...ms" and "instructions:u X G, user CPU N ms".
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
bin=$repo/server/target/release/pcds data=${PCDS_TEST_DATA:-$repo/run/data/2.103} world="" runs=3 port=8430
while [ $# -gt 0 ]; do
  case $1 in
    --world) world=${2:?--world needs a save}; shift ;;
    --bin) bin=${2:?--bin needs a program}; shift ;;
    --data) data=${2:?--data needs a folder}; shift ;;
    --runs) runs=${2:?--runs needs a number}; shift ;;
    --port) port=${2:?--port needs a number}; shift ;;
    -h | --help) sed -n '2,/^set -euo/p' "$0" | sed '$d;s/^# \{0,1\}//'; exit 0 ;;
    *) echo "startbench: unknown argument $1 (try --help)" >&2; exit 2 ;;
  esac
  shift
done
[ -f "$world" ] || { echo "startbench: --world SAVE is required (a save of your own)" >&2; exit 2; }
[ -x "$bin" ] || { echo "startbench: $bin: not an executable (make build)" >&2; exit 2; }
[ -f "$data/netdata.json" ] || { echo "startbench: $data: no game tables" >&2; exit 2; }
case $port in 7777 | 7776 | 7782 | 7781) echo "startbench: ports 7777/7782 are a real server's" >&2; exit 2 ;; esac
admin=$((port + 1))
if (exec 3<>"/dev/tcp/127.0.0.1/$admin") 2>/dev/null; then
  echo "startbench: 127.0.0.1:$admin is taken: a server that is not this script's runs there" >&2
  exit 2
fi

work=$(mktemp -d "${WORK:-${TMPDIR:-/tmp}}/pcds-startbench.XXXXXX")
perf_pid=""
cleanup() {
  [ -z "$perf_pid" ] || kill "$perf_pid" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

for ((i = 1; i <= runs; i++)); do
  rm -rf "${work:?}/home" "$work/world.json"*
  mkdir -p "$work/home"
  cp "$world" "$work/world.json"
  token=$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')
  printf 'Authorization: Bearer %s\n' "$token" >"$work/header"
  : >"$work/log"
  PCDS_LISTEN=127.0.0.1:$port PCDS_STEAM=off PCDS_ADMIN=127.0.0.1:$admin PCDS_ADMIN_TOKEN=$token PCDS_HOME=$work/home \
    PCDS_RELEASES=http://127.0.0.1:1 \
    perf stat -x, -e instructions:u,task-clock:u -o "$work/stat.txt" -- "$bin" serve "$data" "$work/world.json" >"$work/log" 2>&1 &
  perf_pid=$!
  ready=""
  for _ in $(seq 1 600); do
    kill -0 "$perf_pid" 2>/dev/null || break
    if grep -q "running the world" "$work/log"; then ready=1; break; fi
    sleep 0.1
  done
  [ -n "$ready" ] || { echo "startbench: run $i: the server did not start; log:" >&2; tail -5 "$work/log" >&2; exit 1; }
  sleep 0.5
  curl -fsS -X POST -H @"$work/header" -d '{}' "http://127.0.0.1:$admin/api/stop" >/dev/null || true
  wait "$perf_pid" || true
  perf_pid=""
  echo "run $i: $(grep 'running the world' "$work/log" | sed 's/.*after/ready after/')"
  awk -F, '/instructions/{i+=$1} /task-clock/{t=$1} END{printf "  instructions:u %.3f G, user CPU %.0f ms\n", i/1e9, t}' "$work/stat.txt"
done
