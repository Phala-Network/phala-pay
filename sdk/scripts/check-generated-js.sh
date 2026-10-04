#!/usr/bin/env bash
set -euo pipefail
mode=${1:-core}
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
if [[ "$mode" == server ]]; then
  root="$repo/js-server"
  openapi="$repo/js-server/src/generated/openapi.ts"
  generator="$repo/js-server/scripts/generate-server.mjs"
  generated_dir="$repo/js-server/src"
else
  root="$repo/js"
  openapi="$repo/js/src/generated/openapi.d.ts"
  generator=""
  generated_dir=""
fi
cd "$root"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
pnpm exec openapi-typescript ../../crates/topup/openapi.json -o "$tmp/openapi.$([[ "$mode" == server ]] && echo ts || echo d.ts)" --immutable
diff -u "$openapi" "$tmp/openapi.$([[ "$mode" == server ]] && echo ts || echo d.ts)"
if [[ "$mode" == server ]]; then
  mkdir -p "$tmp/server"
  node "$generator" "$tmp/server"
  for file in resources.ts types.ts schemas.ts; do
    diff -u "$generated_dir/$file" "$tmp/server/$file"
  done
fi
