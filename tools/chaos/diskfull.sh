#!/usr/bin/env bash
# Disk-full test of the server: runs tools/chaos/diskfull.py on a small tmpfs that only the
# test sees (a private user + mount namespace via unshare(1): no root, nothing mounted on the
# host). See diskfull.py for what is checked.
#
#   tools/chaos/diskfull.sh --world SAVE.json [--bin PCDS] [--data TABLES] [--work DIR] [--port N]
#     TFS_SIZE   the tmpfs size, default 8m (a save with backups must fit several times)
#
# Needs: util-linux unshare, and unprivileged user namespaces enabled on the host.
# A pass: exit status 0 and a last JSON line {"pass": true, ...} from diskfull.py.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
work=${TMPDIR:-/tmp}/pcds-tools
# --work may be among the options; the mount point is beside it.
args=("$@")
for ((i = 0; i < ${#args[@]}; i++)); do
  if [[ ${args[i]} == --work && $((i + 1)) -lt ${#args[@]} ]]; then work=${args[i + 1]}; fi
done
case $work in /*) ;; *) work=$PWD/$work ;; esac

if [ "${DISKFULL_STAGE:-}" != mounted ]; then
  command -v unshare >/dev/null || { echo "diskfull: unshare (util-linux) is needed" >&2; exit 2; }
  mkdir -p "$work/tfs"
  # Stage 1: a user + mount namespace in which we are root, to mount the tmpfs.
  DISKFULL_STAGE=mounted DISKFULL_UID=$(id -u) DISKFULL_GID=$(id -g) exec unshare -Urm "$0" "$@"
fi

# Stage 2 (in the namespace): mount, then go back to the caller's own ids for the test.
mount -t tmpfs -o "size=${TFS_SIZE:-8m}" tmpfs "$work/tfs"
exec unshare -U --map-user="$DISKFULL_UID" --map-group="$DISKFULL_GID" python3 "$here/diskfull.py" --tfs "$work/tfs" "$@"
