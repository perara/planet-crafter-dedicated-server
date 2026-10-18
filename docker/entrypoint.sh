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
  # -h: symlinks themselves, never what they point to. A file it cannot give away (a
  # read-only or foreign mount inside /data) is said, and the server starts anyway.
  if ! find /data -xdev \( ! -uid "$PUID" -o ! -gid "$PGID" \) -exec chown -h "$PUID:$PGID" {} +; then
    echo "entrypoint: not every file in /data could be given to $PUID:$PGID (see above): the server cannot write those" >&2
  fi
  exec setpriv --reuid="$PUID" --regid="$PGID" --clear-groups /opt/pcds/pcds container
fi
exec /opt/pcds/pcds container
