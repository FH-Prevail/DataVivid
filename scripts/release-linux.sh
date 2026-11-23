#!/bin/sh
# Builds the portable Linux release of DataVivid and packages it in dist/.
# Releases are normally built by .github/workflows/release.yml; this script
# produces the same files locally, e.g. to test a build before tagging.
#
# The binary is built inside an Ubuntu 22.04 container (podman or docker), so
# it links against glibc 2.35 and runs on Ubuntu 22.04+, Debian 12+,
# Fedora 36+ and other distributions of that age or newer. A build on the
# host would require the host's (often newer) glibc.
#
# Output:
#   dist/datavivid-<ver>-linux-x64                 the bare binary
#   dist/DataVivid-<ver>-linux-x64.tar.gz          see scripts/package-linux.sh
#
# Usage:  scripts/release-linux.sh
# The cargo registry and target dir are cached in target/linux-portable, so a
# second run only rebuilds DataVivid itself.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
engine=$(command -v podman || command -v docker) || { echo "podman or docker is required" >&2; exit 1; }
cache="$repo/target/linux-portable"
mkdir -p "$cache/cargo-home" "$cache/target" dist
# Rootless podman maps the container's root to the calling user. Docker runs
# as real root, so hand the build output back to the caller afterwards.
fix_owner=true
case "$engine" in
    *docker) fix_owner="chown -R $(id -u):$(id -g) /src/target/linux-portable /cargo" ;;
esac

"$engine" run --rm \
    -v "$repo:/src:Z" \
    -v "$cache/cargo-home:/cargo:Z" \
    -e CARGO_HOME=/cargo -e RUSTUP_HOME=/cargo/rustup \
    -e CARGO_TARGET_DIR=/src/target/linux-portable/target \
    -w /src docker.io/library/ubuntu:22.04 sh -euc '
        export DEBIAN_FRONTEND=noninteractive PATH=/cargo/bin:$PATH
        apt-get update -qq
        apt-get install -y -qq build-essential pkg-config curl ca-certificates >/dev/null
        if [ ! -x /cargo/bin/cargo ]; then
            curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs |
                sh -s -- -y --no-modify-path --profile minimal
        fi
        rustup update stable --no-self-update >/dev/null
        cargo build --release --locked
        '"$fix_owner"'
    '

bin="$cache/target/release/datavivid"
"$bin" --version
glibc=$(objdump -T "$bin" 2>/dev/null | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1 || true)
echo "Highest glibc symbol version required: ${glibc:-unknown}"

scripts/package-linux.sh "$bin"
(cd dist && sha256sum "datavivid-$version-linux-x64" "DataVivid-$version-linux-x64.tar.gz")
