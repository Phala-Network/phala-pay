#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root/js"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
pnpm exec openapi-typescript ../../crates/topup/openapi.json -o "$tmp/openapi.d.ts" --immutable
diff -u src/generated/openapi.d.ts "$tmp/openapi.d.ts"
node scripts/generate-server.mjs "$tmp"
for file in resources.ts types.ts schemas.ts; do
  diff -u "src/server/$file" "$tmp/$file"
done
