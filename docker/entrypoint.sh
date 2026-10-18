#!/bin/sh
# The container's start: as root only long enough to give /data to PUID:PGID (default
# 1000:1000, so a bind-mounted folder and files copied into it from the host are the
# server's), then pcds as that user. pcds container stays the parent of the server to its
# end: tini ends with it, and the container's end kills every process left (pcds waits for
# the banks of a front that crashed to save first).
set -e
PUID="${PUID:-1000}"
PGID="${PGID:-1000}"
if [ "$(id -u)" = 0 ]; then
  [ "$PUID" != 0 ] || {
    echo "PUID=0: the server does not run as root" >&2
    exit 1
  }
  mkdir -p /data/home
  # -h: symlinks themselves, never what they point to.
  find /data -xdev \( ! -uid "$PUID" -o ! -gid "$PGID" \) -exec chown -h "$PUID:$PGID" {} + 2> /dev/null || true
  exec setpriv --reuid="$PUID" --regid="$PGID" --clear-groups /opt/pcds/pcds container
fi
exec /opt/pcds/pcds container
