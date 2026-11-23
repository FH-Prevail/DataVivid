#!/bin/sh
# Removes DataVivid installed by install.sh: the menu entry, icons, MIME
# types, default-app entries and the binary. Your settings
# (~/.local/share/datavivid) are kept unless you pass --purge.
#
# Usage:  ./uninstall.sh [--purge]
set -eu

purge=0
[ "${1:-}" = "--purge" ] && purge=1

bin_dir=${DATAVIVID_BIN_DIR:-"$HOME/.local/bin"}
data_dir=${XDG_DATA_HOME:-"$HOME/.local/share"}/datavivid

if [ -x "$bin_dir/datavivid" ]; then
    "$bin_dir/datavivid" --unregister || true
    rm -f "$bin_dir/datavivid"
elif command -v datavivid >/dev/null 2>&1; then
    datavivid --unregister || true
fi

if [ "$purge" = 1 ]; then
    rm -rf "$data_dir"
else
    rm -f "$data_dir/uninstall.sh"
    rmdir "$data_dir" 2>/dev/null || true
fi
echo "DataVivid was removed."
