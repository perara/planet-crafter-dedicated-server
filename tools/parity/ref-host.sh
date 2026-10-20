#!/usr/bin/env bash
# The real game as the parity reference, by hand: headless under Wine and Xvfb with BepInEx and
# the extractor plugin (server/tests/parity/SWEEP.md, "The reference host"). sweep.sh starts and
# stops it per script; this is for looking at the reference yourself (a probe by hand, a
# measurement, an experiment) and for putting a start save on it.
#
#   tools/parity/ref-host.sh start START.json   put START.json in the game's save folder as
#                                               Parity-1 and start the game on it
#   tools/parity/ref-host.sh status             GET /api/status of the extractor plugin
#   tools/parity/ref-host.sh save OUT.json      the extractor saves the world on demand; OUT.json is the save
#   tools/parity/ref-host.sh stop               stop the game and its Wine server
#
# Requires: wine, xvfb-run, curl, python3, and REF_DIR: the folder sweep.sh takes as REF_DIR
# (game/ = the game with BepInEx and the extractor plugin, prefix/ = the Wine prefix).
# Environment: REF_DIR (required), REF_PORT (game port, default 8420), REF_API (the plugin's API,
# default 8421). Test ports stay in 8420-8439. The API token is made at `start` and kept in
# REF_DIR/ref-host.token; the game's pid in REF_DIR/ref-host.pid (only that process group is
# stopped). Runs under agent-run when it is installed (a shared host's CPU budget).
# A pass: `start` prints "ready after Ns" (the plugin logged PCDS READY), `status` prints JSON
# with the world's units, `save` writes a JSON save, `stop` prints "stopped".
set -euo pipefail

usage() { awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 2; }
[[ $# -ge 1 ]] || usage
case $1 in -h | --help) usage ;; esac
: "${REF_DIR:?REF_DIR: the folder with game/ and prefix/ (see --help)}"
REF_PORT=${REF_PORT:-8420}
REF_API=${REF_API:-8421}
export WINEPREFIX=$REF_DIR/prefix WINEDEBUG=-all
pidfile=$REF_DIR/ref-host.pid
tokenfile=$REF_DIR/ref-host.token

saves=$(ls -d "$REF_DIR"/prefix/drive_c/users/*/AppData/LocalLow/MijuGames/"Planet Crafter" 2> /dev/null | head -1 || true)
[[ -n $saves ]] || { echo "ref-host: no save folder in $REF_DIR/prefix (has the game run once?)" >&2; exit 1; }

limit=()
command -v agent-run > /dev/null && limit=(agent-run planetcrafter --cpus "${CPUS:-6}" --)

api() { # api METHOD PATH [MAX_SECONDS]
  local token body=()
  token=$(cat "$tokenfile")
  [[ $1 == POST ]] && body=(-d x)
  curl -s -m "${3:-30}" -X "$1" "${body[@]}" -H "Authorization: Bearer $token" "http://127.0.0.1:$REF_API$2"
}

case $1 in
  start)
    [[ $# -eq 2 ]] || usage
    if [[ -f $pidfile ]] && kill -0 "$(cat "$pidfile")" 2> /dev/null; then echo "ref-host: already running" >&2; exit 1; fi
    head -c 12 /dev/urandom | base64 | tr -d '/+=' > "$tokenfile"
    printf '[Extractor]\r\nSaveName = Parity-1\r\nPort = %s\r\nApiPort = %s\r\nApiToken = %s\r\n' \
      "$REF_PORT" "$REF_API" "$(cat "$tokenfile")" > "$REF_DIR/game/BepInEx/config/pcds.extractor.cfg"
    rm -f "$saves"/Parity-1*.json "$REF_DIR/player.log" "$REF_DIR/game/BepInEx/LogOutput.log"
    cp "$2" "$saves/Parity-1.json"
    cd "$REF_DIR/game"
    setsid "${limit[@]}" xvfb-run -a -s "-screen 0 1280x720x24" wine "Planet Crafter.exe" -batchmode -nographics \
      -logFile "Z:${REF_DIR//\//\\}\\player.log" > "$REF_DIR/wine.log" 2>&1 < /dev/null &
    echo $! > "$pidfile"
    for i in $(seq 600); do
      if grep -qs "PCDS READY" "$REF_DIR/game/BepInEx/LogOutput.log"; then echo "ready after ${i}s"; exit 0; fi
      sleep 1
    done
    echo "ref-host: not ready after 600 s, see $REF_DIR/wine.log" >&2
    exit 1
    ;;
  status)
    api GET /api/status
    echo
    ;;
  save)
    [[ $# -eq 2 ]] || usage
    api POST /api/save 120 > /dev/null
    cp "$saves/Parity-1.json" "$2"
    echo "saved $2"
    ;;
  stop)
    api POST /api/stop > /dev/null || true
    pid=$(cat "$pidfile" 2> /dev/null || true)
    for _ in $(seq 12); do
      [[ -n $pid ]] && kill -0 "$pid" 2> /dev/null || break
      sleep 1
    done
    if [[ -n $pid ]] && kill -0 "$pid" 2> /dev/null; then kill -TERM -- "-$pid" 2> /dev/null || true; fi
    wineserver -k 2> /dev/null || true
    wineserver -w 2> /dev/null || true
    rm -f "$pidfile"
    echo stopped
    ;;
  *) usage ;;
esac
