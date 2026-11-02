#!/bin/sh
# Runs as root only long enough to fix /data ownership, then drops to PUID:PGID.
set -eu

PUID="${PUID:-1000}"
PGID="${PGID:-1000}"
BIN=/usr/local/bin/kick-level-farm
CFG=/data/config.toml

if [ "$(id -u)" = "0" ]; then
    mkdir -p /data
    chown "$PUID:$PGID" /data
    # Only touch top-level files we own (config, token, analytics).
    find /data -mindepth 1 -maxdepth 1 ! -user "$PUID" -exec chown "$PUID:$PGID" {} + 2>/dev/null || true
    AS_USER="setpriv --reuid=$PUID --regid=$PGID --clear-groups"
else
    AS_USER=""
fi

# First start: write the default config (follows all your followed channels).
if [ ! -f "$CFG" ] && [ "${1:-}" != "init" ]; then
    $AS_USER "$BIN" -c "$CFG" init
fi

exec $AS_USER "$BIN" -c "$CFG" "$@"
