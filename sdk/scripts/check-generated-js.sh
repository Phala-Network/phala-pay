#!/usr/bin/env bash
set -euo pipefail
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
root="$repo/js-server"
cd "$root"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
pnpm exec openapi-typescript ../../crates/topup/openapi.json -o "$tmp/openapi.ts" --immutable
diff -u "$root/src/generated/openapi.ts" "$tmp/openapi.ts"
mkdir -p "$tmp/server"
node "$root/scripts/generate-server.mjs" "$tmp/server"
for file in resources.ts types.ts schemas.ts; do
  diff -u "$root/src/$file" "$tmp/server/$file"
done
