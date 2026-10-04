#!/usr/bin/env bash
# Checks the routes of a rendered topup compose against the Deploy environment it goes to
# (docs/design/multi-tenant.md D9): every route's `livemode` matches its chain, `true` on a known
# mainnet and `false` on a known public testnet, so production hosts test mode on Sepolia beside
# live mode on mainnet. Refused: a chain on neither list (add it here after review), a local
# development chain, a compose without routes, and any live route in staging, which moves no real
# money. The service validates `livemode` against the chain again when it loads a route.
#
# ENVIRONMENT is the one Deploy selected, never the configuration's own `environment` (a Sentry
# tag), so a staging configuration that calls itself production is still checked as staging. The
# routes are topup's own reading of the compose's inline topup.yaml (`topup config show`), so a
# route written in any YAML form is checked. It needs no network: the pinned Compose parses the
# compose (deploy/pinned-compose.sh --no-download), and topup runs from the compose's image,
# which must already be present (`--pull never`), or from TOPUP, a local topup binary (tests).
#
# Usage: deploy/check-route-modes.sh staging|production COMPOSE
set -euo pipefail

root="$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"
environment=${1:?usage: $0 staging|production COMPOSE}
compose=${2:?usage: $0 staging|production COMPOSE}
case "$environment" in
    staging | production) ;;
    *) echo "unknown Deploy environment: $environment" >&2; exit 64 ;;
esac

# Ethereum, OP, Base, Arbitrum One.
mainnets=" 1 10 8453 42161 "
# Sepolia, Holesky, Hoodi, Base Sepolia, OP Sepolia.
testnets=" 11155111 17000 560048 84532 11155420 "
# Anvil and Hardhat, Geth dev: never deployed.
devnets=" 31337 1337 "

tmp=$(mktemp -d "${TMPDIR:-/tmp}/check-route-modes.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
"$("$root/deploy/pinned-compose.sh" --no-download)" -f "$compose" config --no-interpolate \
    --format json >"$tmp/compose.json"
jq -j '. as $root | [.services.topup.configs[]? | select(.target == "/etc/topup/topup.yaml")
    | .source][0] as $name | $root.configs[$name].content // ""' "$tmp/compose.json" |
    sed 's/[$][$]/$/g' >"$tmp/topup.yaml"
[[ -s "$tmp/topup.yaml" ]] || { echo "the compose carries no topup.yaml" >&2; exit 1; }
if [[ -n "${TOPUP:-}" ]]; then
    "$TOPUP" config show /dev/stdin <"$tmp/topup.yaml" >"$tmp/config.json"
else
    docker run --rm -i --pull never --network none "$(jq -r '.services.topup.image' "$tmp/compose.json")" \
        topup config show /dev/stdin <"$tmp/topup.yaml" >"$tmp/config.json"
fi || { echo "topup refused the compose's configuration" >&2; exit 1; }

# One "route livemode chain_id" line per route, as topup reads them.
routes=$(jq -r '.routes[] | "\(.route) \(.livemode) \(.chain.chain_id)"' "$tmp/config.json")
[[ -n "$routes" ]] || { echo "the compose carries no route" >&2; exit 1; }

failed=0
refuse() {
    echo "route $1: $2" >&2
    failed=1
}
while read -r route livemode chain; do
    if [[ "$mainnets" == *" $chain "* ]]; then
        [[ "$livemode" == true ]] || refuse "$route" "chain $chain is a mainnet: livemode must be true"
    elif [[ "$testnets" == *" $chain "* ]]; then
        [[ "$livemode" == false ]] || refuse "$route" "chain $chain is a test network: livemode must be false"
    elif [[ "$devnets" == *" $chain "* ]]; then
        refuse "$route" "chain $chain is a local development chain"
        continue
    else
        refuse "$route" "chain $chain is not a known mainnet or test network"
        continue
    fi
    if [[ "$environment" == staging && "$livemode" == true ]]; then
        refuse "$route" "staging serves test mode only; a live route belongs in production"
    fi
done <<<"$routes"
if ((failed)); then
    echo "the $environment compose's routes do not match their chains' modes" >&2
    exit 1
fi
# The actual Deploy target must never inherit a staging-only licensing opt-in, even if
# a configuration carries a non-production reporting environment label.
if [[ "$environment" == production ]] &&
    jq -e 'any(.routes[]; .price.allow_unclear_sources == true)' "$tmp/config.json" >/dev/null; then
    echo "production refuses staging-only price licensing opt-in" >&2
    exit 1
fi

while read -r route livemode chain; do
    echo "$route: chain $chain, livemode $livemode"
done <<<"$routes"
