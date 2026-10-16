#!/usr/bin/env bash
# Preflight for the staging reference-product CVM (deploy/phala.md, "Staging reference product").
# Read-only against remote systems, like deploy/preflight.sh: it reads the env file, the compose
# rendered by deploy/render.sh (which holds the product's config), the image in its registry, the
# product's RPC of each of its chains, topup's attestation endpoint, and the Phala Cloud account the
# CLI is logged in to.
#
# Usage: deploy/product/preflight.sh --env FILE --compose FILE --environment-dir DIR
#          (--workspace NAME --os-image NAME | --offline) [--unsealed]
#
# --offline runs only the local checks (env file, compose, and config) and makes no network access.
# --unsealed accepts an empty PRODUCT_API_KEY: Deploy (target `product`) provisions without it and
# the owner seals it from their own machine. Each chain's `rpc_url` is published with the compose,
# so it must be keyless; online, each must report its chain, where the chain's treasury must be a
# contract (the Safe). Output never prints an RPC URL. Every failure is reported; the exit status is
# 1 if any.
set -euo pipefail
source "$(dirname -- "$0")/../contracts/common.sh"
source "$(dirname -- "$0")/../preflight-phala.sh"

approved_os_image=dstack-0.6.1

usage() {
    echo "usage: $0 --env FILE --compose FILE --environment-dir DIR" \
        "(--workspace NAME --os-image NAME | --offline) [--unsealed]" >&2
    exit 64
}
env_file="" compose="" env_dir="" workspace="" os_image="" offline=0 unsealed=0
while (($#)); do
    case "$1" in
        --env) env_file="${2:-}"; shift 2 ;;
        --compose) compose="${2:-}"; shift 2 ;;
        --environment-dir) env_dir="${2:-}"; shift 2 ;;
        --workspace) workspace="${2:-}"; shift 2 ;;
        --os-image) os_image="${2:-}"; shift 2 ;;
        --offline) offline=1; shift ;;
        --unsealed) unsealed=1; shift ;;
        *) usage ;;
    esac
done
[[ -f "$env_file" && -f "$compose" && -d "$env_dir" ]] || usage
((offline)) || [[ -n "$workspace" && -n "$os_image" ]] || usage
for command in docker jq curl; do
    require_command "$command"
done

tmp=$(mktemp -d "${TMPDIR:-/tmp}/product-preflight.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
failures=0
fail() {
    printf 'FAIL: %s\n' "$*" >&2
    failures=$((failures + 1))
}
ok() {
    printf 'ok: %s\n' "$*"
}
origin_pattern='^https://[a-z0-9.-]+(:[0-9]+)?$'

declare -A env=()
account=""
check_artifact "$env_file" "$compose" "$env_dir" product
refuse_example_values "$compose"
value=${env[PRODUCT_API_KEY]-}
if [[ "$value" == *replace-me* ]]; then
    fail "PRODUCT_API_KEY still contains replace-me"
elif [[ -z "$value" ]] && ((unsealed == 0)); then
    fail "PRODUCT_API_KEY is empty"
fi
api_key=$value
# 43 random base62 characters and a 6-character checksum (crates/topup/src/api_keys.rs). The
# staging product runs with a restricted test key; a secret test key is accepted too.
[[ -z "$api_key" || "$api_key" =~ ^ppay_(rk|sk)_test_[0-9A-Za-z]{49}$ ]] ||
    fail "PRODUCT_API_KEY must be a Phala Pay test API key (ppay_rk_test_… or ppay_sk_test_…)"
if [[ -n "$os_image" && "$os_image" != "$approved_os_image" ]]; then
    fail "OS image $os_image is not the approved $approved_os_image (deploy/README.md)"
fi

echo "== product config"
# The attested config (the policy already matched its public_url to dstack-ingress's domain).
if jq -er '.configs | to_entries[] | select(.key | startswith("product_")) | .value.content' \
    "$tmp/compose.json" 2>/dev/null | sed 's/[$][$]/$/g' >"$tmp/config.json" &&
    jq -e 'type == "object"' "$tmp/config.json" >/dev/null 2>&1; then
    service_url=$(jq -r '.service_url // "" | strings' "$tmp/config.json")
    public_url=$(jq -r '.public_url // "" | strings' "$tmp/config.json")
    driver_public_key=$(jq -r '.driver_public_key // "" | strings' "$tmp/config.json")
    account=$(jq -r '.account // "" | strings' "$tmp/config.json")
    web_origin=$(jq -r '.web_origin // "" | strings' "$tmp/config.json")
    # One line per chain: its id, treasury, and RPC URL.
    jq -r '.chains // [] | .[] | [(.chain_id | tostring), (.treasury // ""), (.rpc_url // "")]
        | @tsv' "$tmp/config.json" >"$tmp/chains.tsv" 2>/dev/null || : >"$tmp/chains.tsv"
else
    fail "the compose's product config is not a JSON object"
    : >"$tmp/chains.tsv"
fi
# The product's Phala Pay account: the account its webhooks and attestation must name, and the
# first input of every address it pins.
[[ "${account-}" =~ ^acct_[0-9a-f]{32}$ ]] ||
    fail "the product config's account must be the product's acct_ id (32 lowercase hex digits)"
for name in service_url public_url; do
    [[ "${!name-}" =~ $origin_pattern ]] ||
        fail "the product config's $name must be https://HOST[:PORT] in lowercase with no path"
done
# The website's origin, the only one the demo's API allows (deploy/phala.md, "Website").
[[ "${web_origin-}" =~ $origin_pattern && "$web_origin" != "${public_url-}" ]] ||
    fail "the product config's web_origin must be the website's https://HOST, not its public_url"
# The staging-only product has no sealed RPC key: each chain's URL is published and must be
# keyless.
[[ -s "$tmp/chains.tsv" ]] || fail "the product config names no chains"
[[ -z "$(cut -f1 "$tmp/chains.tsv" | sort | uniq -d)" ]] || fail "the product config repeats a chain"
while IFS=$'\t' read -r id treasury rpc; do
    [[ "$id" =~ ^[1-9][0-9]*$ ]] || fail "a chain of the product config has no chain_id"
    [[ "$treasury" =~ ^0x[0-9a-fA-F]{40}$ ]] || fail "chain $id's treasury is not an address"
    [[ "$rpc" == https://* ]] || fail "chain $id's rpc_url must use https"
    ! embeds_key "$rpc" ||
        fail "chain $id's rpc_url seems to embed an API key, which the compose publishes; use a keyless URL"
done <"$tmp/chains.tsv"
driver_key_bytes=$(base64 -d 2>/dev/null <<<"${driver_public_key-}" | wc -c) || driver_key_bytes=0
[[ "$driver_key_bytes" == 32 ]] || fail "the product config's driver_public_key must be standard base64 of 32 bytes"

if ((failures)); then
    echo "preflight: $failures local check(s) failed; online checks not run" >&2
    exit 1
fi
if ((offline)); then
    echo "preflight: local checks passed (offline)"
    exit 0
fi

check_anonymous_pulls "$tmp/images"

echo "== product account"
[[ "$account" != acct_00000000000000000000000000000000 ]] ||
    fail "the product config's account is the placeholder; commit the product's acct_ id to $env_dir/config.json"

echo "== product RPCs and topup (no RPC URL is printed)"
require_command cast
while IFS=$'\t' read -r id treasury rpc; do
    if ! reported=$(ETH_RPC_URL=$rpc cast chain-id 2>"$tmp/cast.err"); then
        error=$(tool_error "$tmp/cast.err")
        reported="error: ${error//"$rpc"/rpc_url}"
    fi
    if [[ "$reported" != "$id" ]]; then
        fail "chain $id's rpc_url reports chain id $reported"
        continue
    fi
    # Every address the product shows pays this treasury: on staging, the finance Safe.
    if code=$(ETH_RPC_URL=$rpc cast code "$treasury" 2>"$tmp/cast.err") && [[ "$code" != 0x ]]; then
        ok "chain $id: its RPC reports the chain, and its treasury is a contract"
    else
        fail "chain $id's treasury $treasury has no contract code on the chain (expected the Safe)"
    fi
done <"$tmp/chains.tsv"
# The product pins its account's webhook keys from this endpoint, fetched with its API key, and
# checks their binding. Without the key sealed yet, the endpoint must refuse anonymous calls.
nonce=$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')
attestation_url="$service_url/v1/attestation?nonce=$nonce"
if [[ -n "$api_key" ]]; then
    if curl -fsS --max-time 30 -H @- "$attestation_url" >"$tmp/attestation.json" \
        <<<"Authorization: Bearer $api_key" &&
        jq -e --arg account "$account" '.livemode == false and .account == $account
            and (.webhook_keys[0].public_key | test("^whpk_[A-Za-z0-9+/]{43}=$"))' \
            "$tmp/attestation.json" >/dev/null; then
        ok "the service_url attests the product account's test-mode webhook key"
    else
        fail "the service_url does not attest a test-mode webhook key of the config's account for PRODUCT_API_KEY"
    fi
elif [[ "$(curl -sS --max-time 30 -o /dev/null -w '%{http_code}' "$attestation_url")" == 401 ]]; then
    ok "the service_url serves /v1/attestation to API keys only (PRODUCT_API_KEY not sealed yet)"
else
    fail "the service_url does not serve an authenticated /v1/attestation"
fi

check_phala_cloud "$workspace" "$os_image"

if ((failures)); then
    echo "preflight: $failures check(s) failed" >&2
    exit 1
fi
echo "preflight: all checks passed"
