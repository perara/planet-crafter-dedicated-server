#!/bin/bash
# Runs sweep scripts (sweep.py gen) against the real game host and this server, each from a
# fresh start save, and compares them (parity.py, savediff.py). README "Parity with the game".
#
#   server/tests/parity/sweep.sh both|ref2 OUT_DIR SCRIPT.json...
#
#   both   the reference and this server at once: NAME.ref.jsonl, NAME.ours.jsonl, NAME.diff,
#          NAME.savediff, NAME.ref.player.log (the game's log), NAME.ours.log
#   ours   only this server, against the reference's transcript and save already in OUT_DIR
#          (after a fix: no game host is started; the same files as `both`, with a new .ours.*)
#   ref2   the reference once more: NAME.ref2.jsonl, NAME.noise (two runs of the game against each
#          other: its own randomness and timing), NAME.savenoise
#
# X.A.json runs together with X.B.json, a second player started delay_ms later.
#
# Environment: REF_DIR (the reference: game/ with BepInEx and the extractor plugin with its
# POST /api/save, prefix/ the Wine prefix), DATA_DIR (the tables), START (the start save, with
# the reference's host record: mkstart.py's output, the same file for the reference runs a sweep
# is compared with; a different start save makes hundreds of differences), PCDS (default: this
# checkout's release build). Ports REF_PORT (7951), REF_API (7952) and OURS_PORT (7953); parallel
# `ours` workers each take an OURS_PORT and a folder of their own. One reference at a time.
set -u
[[ $# -ge 3 ]] || { sed -n '2,22p' "$0"; exit 2; }
mode=$1 out=$2; shift 2
here=$(cd "$(dirname "$0")" && pwd)
: "${REF_DIR:?}" "${DATA_DIR:?}" "${START:?}"
PCDS=${PCDS:-$here/../../target/release/pcds}
REF_PORT=${REF_PORT:-7951} REF_API=${REF_API:-7952} OURS_PORT=${OURS_PORT:-7953}
export WINEPREFIX=$REF_DIR/prefix WINEDEBUG=-all
SAVES=$(ls -d "$REF_DIR"/prefix/drive_c/users/*/AppData/LocalLow/MijuGames/"Planet Crafter" | head -1)
TOKEN=$(head -c 12 /dev/urandom | base64 | tr -d '/+=')
mkdir -p "$out"

api() { curl -s -m "${3:-30}" -X "$1" $([[ $1 == POST ]] && echo "-d x") -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:$REF_API$2"; }

ref_start() {
  printf '[Extractor]\r\nSaveName = Parity-1\r\nPort = %s\r\nApiPort = %s\r\nApiToken = %s\r\n' "$REF_PORT" "$REF_API" "$TOKEN" > "$REF_DIR/game/BepInEx/config/pcds.extractor.cfg"
  rm -f "$SAVES"/Parity-1*.json "$REF_DIR/player.log" "$REF_DIR/game/BepInEx/LogOutput.log"
  cp "$START" "$SAVES/Parity-1.json"
  (cd "$REF_DIR/game" && exec setsid xvfb-run -a -s "-screen 0 1280x720x24" wine "Planet Crafter.exe" -batchmode -nographics -logFile "Z:${REF_DIR//\//\\}\\player.log" > "$REF_DIR/wine.log" 2>&1 < /dev/null) &
  ref_pid=$!
  for _ in $(seq 600); do grep -qs "PCDS READY" "$REF_DIR/game/BepInEx/LogOutput.log" && break; sleep 1; done
  grep -qs "PCDS READY" "$REF_DIR/game/BepInEx/LogOutput.log" || { echo "the reference did not start: $REF_DIR/wine.log"; exit 1; }
  # Until the world units' rates stand still (the save's machines started).
  local last="" same=0 cur
  for _ in $(seq 40); do
    cur=$(api GET /api/status | python3 -c 'import json,sys; print(json.dumps({k: round(v["perSecond"], 3) for k, v in (json.load(sys.stdin).get("units") or {}).items()}))' 2> /dev/null)
    [[ -n $cur && $cur == "$last" ]] && same=$((same + 1)) || same=0
    [[ $same -ge 2 ]] && break
    last=$cur; sleep 5
  done
}

ref_stop() {
  api POST /api/stop > /dev/null
  for _ in $(seq 12); do kill -0 "$ref_pid" 2> /dev/null || break; sleep 1; done
  kill -0 "$ref_pid" 2> /dev/null && kill -TERM -- "-$ref_pid" 2> /dev/null
  wineserver -k 2> /dev/null; wineserver -w 2> /dev/null
}

ours_start() {
  mkdir -p "$out/ours"; cp "$START" "$out/ours/world.json"
  PCDS_LISTEN=127.0.0.1:$OURS_PORT PCDS_STEAM=off PCDS_ADMIN=off PCDS_AUTOSAVE=0 PCDS_TRUST_LOOPBACK=1 \
    "$PCDS" serve "$DATA_DIR" "$out/ours/world.json" > "$out/ours/server.log" 2>&1 < /dev/null &
  ours_pid=$!
  sleep 2
}

ours_stop() {
  kill -TERM "$ours_pid"
  for _ in $(seq 30); do kill -0 "$ours_pid" 2> /dev/null || break; sleep 1; done
}

probe() {  # probe PORT SCRIPT OUT_BASE SUFFIX
  local b=${2%.A.json}.B.json
  if [[ $2 == *.A.json && -f $b ]]; then
    timeout "${PROBE_TIMEOUT:-5400}" "$PCDS" probe "127.0.0.1:$1" "$DATA_DIR" "$2" > "$3.A.$4.jsonl" 2> "$3.A.$4.err" &
    local pa=$!
    sleep "$(python3 -c "import json; print(json.load(open('$b')).get('delay_ms', 0) / 1000)")"
    timeout "${PROBE_TIMEOUT:-5400}" "$PCDS" probe "127.0.0.1:$1" "$DATA_DIR" "$b" > "$3.B.$4.jsonl" 2> "$3.B.$4.err"
    wait $pa
  else
    timeout "${PROBE_TIMEOUT:-5400}" "$PCDS" probe "127.0.0.1:$1" "$DATA_DIR" "$2" > "$3.$4.jsonl" 2> "$3.$4.err"
  fi
}

for script in "$@"; do
  [[ $script == *.B.json ]] && continue
  name=$(basename "$script" .json); name=${name%.A}
  base=$out/$name
  echo "== $name $mode $(date +%T)"
  [[ $mode == ours ]] || ref_start
  if [[ $mode == ours ]]; then
    ours_start
    probe "$OURS_PORT" "$script" "$base" ours
    ours_stop; cp "$out/ours/world.json" "$base.ours.save.json"; cp "$out/ours/server.log" "$base.ours.log"
  elif [[ $mode == both ]]; then
    ours_start
    probe "$REF_PORT" "$script" "$base" ref & pr=$!
    probe "$OURS_PORT" "$script" "$base" ours
    wait $pr
    ours_stop; cp "$out/ours/world.json" "$base.ours.save.json"; cp "$out/ours/server.log" "$base.ours.log"
  else
    probe "$REF_PORT" "$script" "$base" ref2
  fi
  if [[ $mode != ours ]]; then
    sfx=$([[ $mode == both ]] && echo ref || echo ref2)
    cp "$REF_DIR/player.log" "$base.$sfx.player.log"
    api POST /api/save "" 120 > /dev/null; cp "$SAVES/Parity-1.json" "$base.$sfx.save.json"
    ref_stop
  fi
  for p in "" .A .B; do
    if [[ $mode != ref2 && -f $base$p.ours.jsonl ]]; then
      python3 "$here/parity.py" "$DATA_DIR" "$START" "$base$p.ref.jsonl" "$base$p.ours.jsonl" > "$base$p.diff" 2>&1
      echo "   $name$p: $(tail -1 "$base$p.diff")"
    elif [[ $mode == ref2 && -f $base$p.ref2.jsonl ]]; then
      python3 "$here/parity.py" "$DATA_DIR" "$START" "$base$p.ref.jsonl" "$base$p.ref2.jsonl" --both-game > "$base$p.noise" 2>&1
      echo "   $name$p noise: $(tail -1 "$base$p.noise")"
    fi
  done
  if [[ $mode != ref2 ]]; then
    python3 "$here/savediff.py" "$START" "$base.ref.save.json" "$base.ours.save.json" > "$base.savediff" 2>&1
    echo "   save: $(tail -1 "$base.savediff")"
  else
    python3 "$here/savediff.py" "$START" "$base.ref.save.json" "$base.ref2.save.json" > "$base.savenoise" 2>&1
    echo "   save noise: $(tail -1 "$base.savenoise")"
  fi
done
