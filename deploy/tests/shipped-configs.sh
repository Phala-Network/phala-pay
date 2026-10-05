#!/usr/bin/env bash
# Validate committed topup configs as shipped, including their declared environment.
# Route-only examples have deployment placeholders, so use the route template validator;
# their price-source opt-ins permit noncommercial rehearsal, not production use.
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
: "${TOPUP:?set TOPUP to a topup binary, for example target/debug/topup}"
cd "$root"
mapfile -d '' -t configs < <(git ls-files -z 'deploy/environments/**/topup.yaml' 'examples/*.yaml')
for config in "${configs[@]}"; do
    echo "Checking shipped config: $config"
    if [[ "$config" == examples/* ]] && grep -q '^route:' "$config"; then
        "$TOPUP" route validate --template "$config"
    else
        "$TOPUP" config check "$config"
    fi
done
echo "shipped config checks passed (${#configs[@]} files)"
