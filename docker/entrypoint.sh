#!/bin/sh
# The container's start: as root only long enough to give /data to PUID:PGID (default
# 1000:1000, so a bind-mounted folder and files copied into it from the host are the
# server's), then pcds as that user. Started as another user (docker run --user, a Kubernetes
# runAsUser), nothing is given away: /data must be writable by that user already. pcds
# container stays the parent of the server to its end: tini ends with it, and the container's
# end kills every process left (pcds waits for the banks of a front that crashed to save
# first).
set -e
PUID="${PUID:-1000}"
PGID="${PGID:-1000}"
if [ "$(id -u)" = 0 ]; then
  [ "$PUID" != 0 ] || {
    echo "PUID=0: the server does not run as root: set PUID and PGID to the user that owns /data (default 1000)" >&2
    exit 1
  }
  mkdir -p /data/home || {
    echo "entrypoint: root cannot write /data (capabilities dropped?): start the container as that user instead (docker run --user UID:GID, a Kubernetes runAsUser), or keep CHOWN, SETUID, SETGID, DAC_OVERRIDE and FOWNER" >&2
    exit 1
  }
  # -h: symlinks themselves, never what they point to. A file it cannot give away (a
  # read-only or foreign mount inside /data) is said, and the server starts anyway.
  if ! find /data -xdev \( ! -uid "$PUID" -o ! -gid "$PGID" \) -exec chown -h "$PUID:$PGID" {} +; then
    echo "entrypoint: not every file in /data could be given to $PUID:$PGID (see above): the server cannot write those" >&2
  fi
  exec setpriv --reuid="$PUID" --regid="$PGID" --clear-groups /opt/pcds/pcds container
fi
# Not root: /data is written as this user or not at all, and the server says so now.
mkdir -p /data/home || {
  echo "entrypoint: /data is not writable by uid $(id -u): give it to that user (a Kubernetes fsGroup, chown on the host), or start the container as root with PUID and PGID set" >&2
  exit 1
}
exec /opt/pcds/pcds container
