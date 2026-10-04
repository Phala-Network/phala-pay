#!/usr/bin/env bash
# Checks deploy/check-route-modes.sh on Phala's staging environment, rendered as Deploy renders
# it, with routes appended to its topup.yaml in every YAML form (block, flow, and JSON): production
# hosts test routes on test networks beside live routes on mainnets, staging only test routes, and
# a route on an unknown or local chain is refused. The configuration's own `environment` never
# matters: staging's says `staging`, and the check is run as each Deploy environment. TOPUP is
# the topup binary it reads the configuration with (cargo build).
set -euo pipefail

root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
check="$root/deploy/check-route-modes.sh"
: "${TOPUP:?set TOPUP to a topup binary, for example target/debug/topup}"
export TOPUP
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

jq -n '{"phala-pay": "ghcr.io/phala-network/phala-pay@sha256:\("1" * 64)",
    "postgres-walg": "ghcr.io/phala-network/postgres-walg@sha256:\("2" * 64)"}' >"$tmp/images.json"
staging="$root/deploy/environments/phala-network/staging/topup"
# A resolved live route on Ethereum (the route fixture), one line of JSON.
"$TOPUP" route show "$root/crates/topup/tests/fixtures/phala-cloud-pha.yaml" | jq -c . >"$tmp/route.json"

# route_json NAME CHAIN_ID LIVEMODE: the fixture as another route, on its own two providers.
route_json() {
    jq -c --arg name "$1" --argjson chain "$2" --argjson live "$3" \
        '.route = $name | .chain.chain_id = $chain | .livemode = $live
        | .chain.rpc_groups = {a:"\($name)-a", b:"\($name)-b"}
        | .price.fx = [{source:"kraken", symbol:"USDTUSD", company:"kraken"}]' "$tmp/route.json"
}
# environment NAME ROUTE_LINE...: staging's environment with each route appended as a list item,
# and each route's two providers configured; rendered to $tmp/NAME.yml.
environment() {
    local name=$1 line id
    shift
    cp -r "$staging" "$tmp/$name"
    for line in "$@"; do
        printf '  - %s\n' "$line" >>"$tmp/$name/topup.yaml"
        id=$(sed -E 's/.*route"?: *"?([a-z0-9-]+).*/\1/' <<<"$line")
        chain=$(sed -E 's/.*chain_id"?: *([0-9]+).*/\1/' <<<"$line")
        if ! grep -q '^  test-a:' "$tmp/$name/topup.yaml"; then sed -i "s|^rpc_companies:$|rpc_companies:\n  test-a: { domains: [test-a.example] }\n  test-b: { domains: [test-b.example] }|" "$tmp/$name/topup.yaml"; fi
        sed -i "s|^rpc_groups:$|rpc_groups:\n  $id-a:\n    chain_id: $chain\n    members: [{id: $id-a, company: test-a, url: 'https://$id.test-a.example', account_budget: tenderly-account, key_budget: provider-a-key}]\n  $id-b:\n    chain_id: $chain\n    members: [{id: $id-b, company: test-b, url: 'https://$id.test-b.example', account_budget: publicnode-account, key_budget: provider-b-key}]|" "$tmp/$name/topup.yaml"

    done
    "$root/deploy/render.sh" --images "$tmp/images.json" \
        --gateway-domain gateway.dstack-pha-prod5.phala.network "$tmp/$name" >"$tmp/$name.yml"
}
# flow JSON: the route as a YAML flow mapping, with bare keys.
flow() {
    sed -E 's/"([a-z_]+)":/\1: /g' <<<"$1"
}
accepts() {
    "$check" "$1" "$tmp/$2.yml" >"$tmp/out" 2>&1 || {
        echo "check-route-modes refused $1 $2: $(cat "$tmp/out")" >&2
        exit 1
    }
}
refuses() {
    if "$check" "$1" "$tmp/$2.yml" >"$tmp/out" 2>&1; then
        echo "check-route-modes accepted $1 $2" >&2
        exit 1
    fi
    grep -Fq -- "$3" "$tmp/out" || {
        echo "check-route-modes refused $1 $2 for an unexpected reason: $(cat "$tmp/out")" >&2
        exit 1
    }
}

# Staging opts into Unclear sources; the actual production target refuses this opt-in.
environment staging
accepts staging staging
refuses production staging "production refuses staging-only price licensing opt-in"

# A live mainnet route appended after them, in flow style or as JSON, is found: production
# detects it, staging refuses its live mode and production refuses its licensing opt-in.
environment flow "$(flow "$(route_json mainnet-flow 1 true)")"
grep -q '^  - {route: ' "$tmp/flow/topup.yaml"
refuses staging flow "route mainnet-flow: staging serves test mode only"
refuses production flow "production refuses staging-only price licensing opt-in"
environment json "$(route_json mainnet-json 1 true)"
grep -q '^  - {"' "$tmp/json/topup.yaml"
refuses staging json "route mainnet-json: staging serves test mode only"
refuses production json "production refuses staging-only price licensing opt-in"

# A chain on neither list, and a local development chain, are refused everywhere.
environment unknown "$(flow "$(route_json polygon 137 true)")"
refuses production unknown "route polygon: chain 137 is not a known mainnet or test network"
environment devnet "$(route_json anvil 31337 false)"
refuses production devnet "route anvil: chain 31337 is a local development chain"
refuses staging devnet "route anvil: chain 31337 is a local development chain"

echo "route mode check test passed"
