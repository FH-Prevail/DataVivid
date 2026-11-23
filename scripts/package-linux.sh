#!/bin/sh
# Packages a built Linux binary into dist/DataVivid-<ver>-linux-x64.tar.gz
# and copies the bare binary to dist/datavivid-<ver>-linux-x64.
# Used by the release workflow and by scripts/release-linux.sh.
#
# Usage:  scripts/package-linux.sh path/to/datavivid
#
# Tarball contents: datavivid, datavivid.desktop, datavivid.png (256 px),
# install.sh, uninstall.sh, README.md, LICENSE.
set -eu

bin=$1
repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
name="DataVivid-$version-linux-x64"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The icon is drawn by the program itself (src/icon_art.rs): register into a
# throwaway XDG data folder and take the 256 px PNG from there.
XDG_DATA_HOME="$work/xdg" XDG_CONFIG_HOME="$work/config" "$bin" --register >/dev/null
icon="$work/xdg/icons/hicolor/256x256/apps/datavivid.png"
[ -s "$icon" ] || { echo "icon was not generated" >&2; exit 1; }

stage="$work/$name"
mkdir -p "$stage" dist
install -m755 "$bin" "$stage/datavivid"
install -m644 packaging/linux/datavivid.desktop "$icon" README.md LICENSE "$stage/"
install -m755 scripts/install.sh scripts/uninstall.sh "$stage/"
tar -C "$work" --owner=0 --group=0 -czf "dist/$name.tar.gz" "$name"
install -m755 "$bin" "dist/datavivid-$version-linux-x64"
echo "Packaged dist/$name.tar.gz and dist/datavivid-$version-linux-x64"
