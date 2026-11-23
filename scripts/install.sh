#!/bin/sh
# Installs DataVivid for the current user on Linux (no root needed).
#
#   - uses the `datavivid` binary next to this script (release tarball),
#     or builds it with cargo when run from the repository
#   - copies it to ~/.local/bin (or $DATAVIVID_BIN_DIR)
#   - adds it to the application menu and to the file manager's "Open with"
#     list (datavivid --register)
#   - with --default, also makes it the default app for CSV, TSV, Parquet,
#     JSON Lines, Arrow, Avro and SQLite files
#
# Usage:  ./install.sh [--default]
set -eu

here=$(cd "$(dirname "$0")" && pwd)
make_default=0
for arg in "$@"; do
    case "$arg" in
        --default) make_default=1 ;;
        -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
        *) echo "Unknown option: $arg" >&2; exit 2 ;;
    esac
done

src="$here/datavivid"
if [ ! -f "$src" ]; then
    repo=$(dirname "$here")
    if [ -f "$repo/Cargo.toml" ]; then
        echo "Building DataVivid (cargo build --release)..."
        (cd "$repo" && cargo build --release)
        target_dir=$(cd "$repo" && cargo metadata --format-version 1 --no-deps |
            sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
        src="$target_dir/release/datavivid"
    fi
fi
[ -f "$src" ] || { echo "datavivid binary not found next to this script" >&2; exit 1; }

bin_dir=${DATAVIVID_BIN_DIR:-"$HOME/.local/bin"}
mkdir -p "$bin_dir"
# Copy to a temporary name first, so a running copy is not disturbed.
cp "$src" "$bin_dir/.datavivid.new"
chmod 755 "$bin_dir/.datavivid.new"
mv -f "$bin_dir/.datavivid.new" "$bin_dir/datavivid"
if [ -f "$here/uninstall.sh" ]; then
    data_dir=${XDG_DATA_HOME:-"$HOME/.local/share"}/datavivid
    mkdir -p "$data_dir"
    cp "$here/uninstall.sh" "$data_dir/uninstall.sh"
    chmod 755 "$data_dir/uninstall.sh"
fi

"$bin_dir/datavivid" --register
if [ "$make_default" = 1 ]; then
    "$bin_dir/datavivid" --make-default
fi

echo "Installed $("$bin_dir/datavivid" --version) to $bin_dir/datavivid"
case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) echo "Note: $bin_dir is not on your PATH. The application menu entry works anyway;"
       echo "      add it to PATH to run 'datavivid' from a terminal." ;;
esac
