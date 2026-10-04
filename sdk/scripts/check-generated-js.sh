#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root/js"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
pnpm exec openapi-typescript ../../crates/topup/openapi.json -o "$tmp/openapi.d.ts" --immutable
diff -u src/generated/openapi.d.ts "$tmp/openapi.d.ts"
