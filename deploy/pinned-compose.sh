#!/usr/bin/env bash
# Prints the path of Docker Compose v2.26.0, the version the dstack-0.5.12 guest runs
# (docs/design/deploy-config.md), verified by the sha256 of its official release. render.sh renders
# the attested compose with it, so the bytes are the same on every machine and load on the CVM.
#
# The binary is cached under ${XDG_CACHE_HOME:-~/.cache}/phala-pay. --no-download fails instead of
# fetching a missing binary (preflight --offline); PINNED_COMPOSE names a binary to verify and use
# instead of the cache.
#
# Usage: deploy/pinned-compose.sh [--no-download]
set -euo pipefail

version=v2.26.0
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) asset=docker-compose-linux-x86_64
        sha256=59c6b262bedc4a02f46c8400e830e660935684899c770c3f5e804a2b7079fc16 ;;
    Linux-aarch64 | Linux-arm64) asset=docker-compose-linux-aarch64
        sha256=6f00ed24a846046b441c0f0a0f8c1e00194f4b0e33f2433fac0d2dd0e486fc80 ;;
    Darwin-x86_64) asset=docker-compose-darwin-x86_64
        sha256=90aac9e3dfd1ab57446347d6dc65ce2570d9d6165cf1b1f2185d1a6387746a45 ;;
    Darwin-arm64) asset=docker-compose-darwin-aarch64
        sha256=7443f0c51d9fcb8865a5ed4cf569c2db116b7d2a4762332ca06960ca3a622e72 ;;
    *) echo "no pinned Docker Compose $version for $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

# The owner runs preflight on their own machine (macOS: shasum).
digest() {
    if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi |
        awk '{print $1}'
}

binary=${PINNED_COMPOSE:-${XDG_CACHE_HOME:-$HOME/.cache}/phala-pay/$asset-$version}
if [[ ! -f "$binary" ]]; then
    if [[ "${1:-}" == --no-download || -n "${PINNED_COMPOSE:-}" ]]; then
        echo "Docker Compose $version is not at $binary; run deploy/pinned-compose.sh once" >&2
        exit 1
    fi
    mkdir -p "$(dirname "$binary")"
    partial=$(mktemp "$binary.XXXXXX")
    trap 'rm -f "$partial"' EXIT
    curl -fsSL --retry 3 -o "$partial" \
        "https://github.com/docker/compose/releases/download/$version/$asset"
    [[ "$(digest "$partial")" == "$sha256" ]] ||
        { echo "the downloaded $asset does not have the pinned sha256" >&2; exit 1; }
    chmod 0755 "$partial"
    mv "$partial" "$binary"
fi
[[ "$(digest "$binary")" == "$sha256" ]] ||
    { echo "$binary is not Docker Compose $version (sha256 differs)" >&2; exit 1; }
printf '%s\n' "$binary"
