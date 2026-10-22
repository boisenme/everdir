#!/bin/sh
# Runs the real everdir binary on the synthetic demo index, sandboxed so it cannot see your files.
# Usage: docs/demo/sandbox.sh <everdir-binary> <work-dir>   (then open http://127.0.0.1:7272)
#
# Inside the bubblewrap sandbox "/" is an empty directory, /usr is read-only, and $HOME, /mnt,
# /media and /sys do not exist. everdir loads the index from the cache that gen-data.mjs writes,
# and "/" is set to rescan only on request, so nothing is scanned. Even a manual rescan (or a
# delete) only reaches the empty sandbox root.
set -e
BIN=$(realpath "$1")
WORK=$(realpath -m "$2")
HERE=$(dirname "$(realpath "$0")")
# The root must be on a real filesystem (not tmpfs), or everdir finds no drive to index.
ROOT=${EVERDIR_DEMO_ROOT:-/var/tmp/everdir-demo-root}
mkdir -p "$ROOT" "$WORK/cache" "$WORK/config/everdir"
node "$HERE/gen-data.mjs" "$WORK/cache"
printf '{"drives":{"/":{"enabled":true,"every":0}}}\n' > "$WORK/config/everdir/drives.json"

exec bwrap --unshare-user --unshare-pid --unshare-ipc --unshare-uts --die-with-parent --clearenv \
    --bind "$ROOT" / \
    --ro-bind /usr /usr --symlink usr/lib /lib --symlink usr/lib /lib64 --symlink usr/bin /bin \
    --proc /proc --dev /dev --tmpfs /tmp \
    --ro-bind "$BIN" /opt/everdir \
    --bind "$WORK/cache" /cache --bind "$WORK/config" /config \
    --setenv HOME /home/alex --setenv XDG_CACHE_HOME /cache --setenv XDG_CONFIG_HOME /config \
    --setenv EVERDIR_PORT 7272 --setenv EVERDIR_NO_TRAY 1 --setenv PATH /usr/bin \
    /opt/everdir serve
