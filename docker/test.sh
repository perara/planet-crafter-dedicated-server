#!/bin/sh
# Hot updates inside the container, end to end, against an image (docs/docker.md).
#
#   docker/test.sh [IMAGE]          IMAGE is a built image (default: planet-crafter-dedicated-server,
#                                   what `make docker` builds)
#   PCDS_TEST_DATA=DIR docker/test.sh IMAGE
#                                   DIR holds game tables (netdata.json, ...), as for cargo test
#   PCDS_TEST_PORTS="UDP ADMIN FEED" docker/test.sh IMAGE
#                                   three ports to use instead of three free ones the system picks
#
# Checks that need no game run always (part 1): the image's updater installs a release into the
# standby bank on a read-only root filesystem with /data a volume, and a start without the game
# says what to do. With PCDS_TEST_DATA (part 2) a server runs in the container with a made-up
# game folder, a new world and three scripted players, and the release feed is a local one
# (the image's own program published as release v999.0.0): `pcds upgrade` hot-updates it with
# all three connected, `docker restart` comes back on the new bank, and `pcds upgrade
# --rollback --restart` restarts the server inside the running container. The container shares
# the host's network, because a local release feed is plain http on 127.0.0.1, which the updater
# accepts from nowhere else. Needs docker, python3, tar and sha256sum; no other container is
# touched (names start with pcds-dockertest-). Exits 0 on success, 1 on a failed check.
#
# Rootless Docker (and a daemon with user-namespace remapping) works too. There the container's
# root is the user running this script and a container user 1000 is a sub-uid of it, which
# cannot write the folders this script makes; pcds does not run as root. So a throwaway root
# container gives each folder a one-off container writes to that user first (`given`), and
# another gives everything back at the end (the files would be a sub-uid's, which this user
# cannot delete).
set -eu

IMAGE="${1:-planet-crafter-dedicated-server}"
NAME="pcds-dockertest-$$"
W="$(mktemp -d)"
TAG=v999.0.0
UID_="$(id -u)"
GID_="$(id -g)"
ROOTLESS=no
case "$(docker info --format '{{.SecurityOptions}}' 2>/dev/null || true)" in *rootless* | *userns*) ROOTLESS=yes ;; esac
FEED_PID=""
LOAD_PID=""
C=""

say() { echo "docker/test.sh: $*"; }
ok() { echo "ok: $*"; }
fail() {
  echo "FAIL: $*" >&2
  [ -z "$C" ] || docker logs --tail 60 "$C" >&2 || true
  exit 1
}
cleanup() {
  [ -z "$LOAD_PID" ] || kill "$LOAD_PID" 2>/dev/null || true
  [ -z "$FEED_PID" ] || kill "$FEED_PID" 2>/dev/null || true
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  if [ "$ROOTLESS" = yes ] && [ -d "$W" ]; then
    docker run --rm --user 0:0 --entrypoint chown -v "$W:/w" "$IMAGE" -R 0:0 /w >/dev/null 2>&1 || true
  fi
  rm -rf "$W"
}
trap cleanup EXIT

for tool in docker python3 tar sha256sum; do
  command -v "$tool" >/dev/null || fail "$tool is needed"
done
docker image inspect "$IMAGE" >/dev/null 2>&1 || fail "no image $IMAGE (make docker, or pass the image's name)"

# Three ports free now (a made-up game's UDP port, the admin API, the release feed).
PORTS="${PCDS_TEST_PORTS:-$(python3 - <<'EOF'
import socket
socks = [socket.socket(socket.AF_INET, socket.SOCK_DGRAM) for _ in range(3)]
for s in socks:
    s.bind(("127.0.0.1", 0))
print(" ".join(str(s.getsockname()[1]) for s in socks))
EOF
)}"
set -- $PORTS
UDP=$1 ADMIN=$2 FEED=$3

# The image's own program as release $TAG in a feed directory, served on 127.0.0.1:$FEED.
make_feed() {
  rel="pcds-$TAG-linux-x86_64"
  mkdir -p "$W/feed/releases/tags" "$W/feed/download" "$W/stage/$rel/web"
  id="$(docker create "$IMAGE")"
  for f in pcds libsteam_api.so web/index.html; do
    docker cp "$id:/opt/pcds/$f" "$W/stage/$rel/$f" >/dev/null
  done
  docker rm "$id" >/dev/null
  tar -C "$W/stage" -czf "$W/feed/download/$rel.tar.gz" "$rel"
  (cd "$W/feed/download" && sha256sum "$rel.tar.gz" >"$rel.tar.gz.sha256")
  base="http://127.0.0.1:$FEED/download"
  printf '{"tag_name":"%s","assets":[{"name":"%s.tar.gz","browser_download_url":"%s/%s.tar.gz"},{"name":"%s.tar.gz.sha256","browser_download_url":"%s/%s.tar.gz.sha256"}]}' \
    "$TAG" "$rel" "$base" "$rel" "$rel" "$base" "$rel" >"$W/feed/releases/tags/$TAG"
  cp "$W/feed/releases/tags/$TAG" "$W/feed/releases/latest"
  (cd "$W/feed" && exec python3 -m http.server "$FEED" --bind 127.0.0.1 >/dev/null 2>&1) &
  FEED_PID=$!
  for _ in $(seq 1 50); do
    python3 -c "import urllib.request; urllib.request.urlopen('http://127.0.0.1:$FEED/releases/latest')" 2>/dev/null && return 0
    sleep 0.2
  done
  fail "the release feed did not start"
}

# Folder $1 writable by the user the one-off containers run as (only rootless Docker needs it:
# elsewhere that user is this one and the folder is its own).
given() {
  [ "$ROOTLESS" = no ] || docker run --rm --user 0:0 --entrypoint chown -v "$1:/data" "$IMAGE" -R "$UID_:$GID_" /data >/dev/null || fail "could not give $1 to the containers' user"
}

# pcds in a throwaway container of the image: a read-only root, /data from $1, as this user.
pcds_once() {
  data="$1"
  shift
  docker run --rm --read-only --tmpfs /tmp --network host --user "$UID_:$GID_" \
    -e "PCDS_RELEASES=http://127.0.0.1:$FEED/releases" -v "$data:/data" "$@"
}

say "image $IMAGE"
make_feed

# ---- Part 1: no game needed ---------------------------------------------------------------

health="$(docker image inspect -f '{{json .Config.Healthcheck.Test}}' "$IMAGE")"
case "$health" in *"pcds health"*) ok "the image has its health check" ;; *) fail "no health check (pcds health) in the image: $health" ;; esac

mkdir -p "$W/d1"
given "$W/d1"
out="$(pcds_once "$W/d1" --entrypoint pcds "$IMAGE" upgrade "$TAG" 2>&1)" || fail "pcds upgrade $TAG with the server stopped, on a read-only root:
$out"
case "$out" in *"$TAG installed in bank"*"sha256 checked"*) ok "the updater installs $TAG into a bank on a read-only root filesystem" ;; *) fail "unexpected output of pcds upgrade:
$out" ;; esac
out="$(pcds_once "$W/d1" --entrypoint pcds "$IMAGE" status 2>&1)" || true
echo "$out" | grep -q "^bank .: .*($TAG).*\[active\]" && echo "$out" | grep -q "^image: pcds" || fail "pcds status:
$out"
ok "pcds status lists the bank and says which release runs against the image's"
out="$(pcds_once "$W/d1" "$IMAGE" 2>&1)" && fail "the container started without a game folder"
case "$out" in *"mount your game folder at /game"*) ok "a start without the game says what to do" ;; *) fail "no instructions without the game:
$out" ;; esac

# ---- Part 2: a server in the container ----------------------------------------------------

if [ -z "${PCDS_TEST_DATA:-}" ] || [ "$PCDS_TEST_DATA" = none ]; then
  say "part 2 SKIPPED: no PCDS_TEST_DATA (a directory of game tables); the hot update itself was not run"
  say "PASS (part 1 only)"
  exit 0
fi
[ -f "$PCDS_TEST_DATA/netdata.json" ] || fail "PCDS_TEST_DATA=$PCDS_TEST_DATA holds no game tables (netdata.json)"

mkdir -p "$W/game/Planet Crafter_Data/Managed" "$W/d2/data"
: >"$W/game/Planet Crafter.exe"
echo "a made-up game build for docker/test.sh" >"$W/game/Planet Crafter_Data/Managed/Assembly-CSharp.dll"
cp -r "$PCDS_TEST_DATA" "$W/d2/data/test"
# The tables must be those of this game's build: its id is what pcds status says is missing.
mkdir "$W/d0"
given "$W/d0"
build="$(pcds_once "$W/d0" -e PCDS_GAME_DIR=/game -v "$W/game:/game:ro" --entrypoint pcds "$IMAGE" status 2>&1 | sed -n 's/^game build \([0-9a-f]*\) has no tables.*/\1/p')"
[ -n "$build" ] || fail "pcds status did not name the made-up game's build"
printf '%s' "$build" >"$W/d2/data/test/buildid"
cat >"$W/d2/server.env" <<EOF
PCDS_LISTEN=127.0.0.1:$UDP
PCDS_STEAM=off
PCDS_ADMIN=127.0.0.1:$ADMIN
PCDS_ADMIN_TOKEN=test
PCDS_WORLD=world
PCDS_RELEASES=http://127.0.0.1:$FEED/releases
EOF
chmod 600 "$W/d2/server.env"
# No backups: the rule save.backupsKept.
echo '{"save": {"backupsKept": 0}}' >"$W/d2/rules.json"

C="$NAME"
docker run -d --name "$C" --network host --read-only --tmpfs /tmp --restart unless-stopped --stop-timeout 90 \
  -e "PUID=$UID_" -e "PGID=$GID_" -v "$W/d2:/data" -v "$W/game:/game:ro" "$IMAGE" >/dev/null

# Healthy: the health check's own command answers.
healthy() {
  for _ in $(seq 1 120); do
    docker exec "$C" pcds health >/dev/null 2>&1 && return 0
    [ "$(docker inspect -f '{{.State.Running}}' "$C")" = true ] || fail "the container stopped"
    sleep 1
  done
  fail "the server did not answer within 2 minutes"
}
players() { docker exec "$C" pcds players | tail -n 1; }
status() { docker exec "$C" pcds status; }
healthy
ok "a server with a new world runs in the container (read-only root, /data a volume)"

docker exec "$C" pcds load "127.0.0.1:$UDP" /data/data/test 3 25 200 >"$W/load.log" 2>&1 &
LOAD_PID=$!
for _ in $(seq 1 30); do
  [ "$(players)" = "3 online" ] && break
  sleep 1
done
[ "$(players)" = "3 online" ] || fail "three scripted players did not connect ($(players))"

out="$(docker exec "$C" pcds upgrade "$TAG" 2>&1)" || fail "pcds upgrade $TAG in the running container:
$out"
case "$out" in *"flip "*"(update to $TAG)"*) ok "hot update inside the container: $(echo "$out" | sed -n 's/^pcds: flip [^:]*: //p' | cut -c1-80)" ;; *) fail "no flip:
$out" ;; esac
[ "$(players)" = "3 online" ] || fail "players dropped by the hot update ($(players))"
status | grep -q "^bank .: .*($TAG).*\[active, running" || fail "the new bank does not run:
$(status)"
docker exec "$C" pcds health >/dev/null || fail "the health check failed after the hot update"
ok "the new bank runs the world with all three players connected, and the health check says so"
wait "$LOAD_PID" || true
LOAD_PID=""
grep -q "load: 3 players" "$W/load.log" && ! grep -q "failed" "$W/load.log" || fail "the scripted players did not stay connected through the update:
$(cat "$W/load.log")"
ok "the players stayed connected until their script ended"

docker restart -t 90 "$C" >/dev/null
healthy
status | grep -q "^bank .: .*($TAG).*\[active, running" || fail "after docker restart:
$(status)"
ok "docker restart comes back on the hot-updated bank"
docker logs "$C" 2>&1 | grep -q "image: pcds .* from bank" || fail "the container's start did not say which release runs"

started="$(docker inspect -f '{{.State.StartedAt}}' "$C")"
out="$(docker exec "$C" pcds upgrade --rollback --restart 2>&1)" || fail "pcds upgrade --rollback --restart:
$out"
healthy
[ "$(docker inspect -f '{{.State.StartedAt}} {{.RestartCount}}' "$C")" = "$started 0" ] || fail "the container itself restarted"
status | grep -q "^bank a: .*\[active, running" || fail "after the restart in place:
$(status)"
ok "pcds upgrade --rollback --restart restarts the server inside the running container"

say "PASS"
