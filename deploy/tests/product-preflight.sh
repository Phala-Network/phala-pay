#!/usr/bin/env bash
# Local (offline) checks of deploy/product/preflight.sh against the staging product rendered as
# Deploy renders it: a placeholder, malformed, or live key, a stale render, a malformed account or
# website origin, an ingress domain other than the public URL's host, a chain's RPC URL over http
# or with an API key, and a repeated chain are refused; an unsealed env file is accepted only with
# --unsealed, and a restricted or secret test key without it.
set -euo pipefail

root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
preflight="$root/deploy/product/preflight.sh"
product="$root/deploy/environments/phala-network/staging/product"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

jq -n '{"phala-pay-reference-product": "ghcr.io/phala-network/phala-pay-reference-product@sha256:\("3" * 64)"}' \
    >"$tmp/images.json"
# render NAME ENV_DIR
render() {
    "$root/deploy/render.sh" --images "$tmp/images.json" \
        --gateway-domain gateway.dstack-pha-prod5.phala.network "$2" >"$tmp/$1.yml"
}
# edited NAME JQ: the staging product with its config edited, rendered.
edited() {
    cp -r "$product" "$tmp/$1"
    jq "$2" "$product/config.json" >"$tmp/$1/config.json"
    render "$1" "$tmp/$1"
}
render compose "$product"
# A hand edit is stale.
sed 's|https://base-sepolia-rpc.publicnode.com|https://rpc.example/other|' "$tmp/compose.yml" \
    >"$tmp/stale.yml"
edited http-rpc '(.chains[] | select(.chain_id == 84532) | .rpc_url) = "http://base-sepolia-rpc.publicnode.com"'
edited keyed-rpc '(.chains[] | select(.chain_id == 84532) | .rpc_url) = "https://base-sepolia.infura.io/v3/0123456789abcdef0123456789abcdef"'
edited repeated '(.chains[] | select(.chain_id == 84532) | .chain_id) = 11155111'
edited slug '.account = "phala-cloud"'
edited web-origin '.web_origin = "https://pay.phala.com/"'
printf 'PRODUCT_API_KEY=\n' >"$tmp/unsealed.env"
printf 'PRODUCT_API_KEY=replace-me\n' >"$tmp/example.env"
printf 'PRODUCT_API_KEY=sk_test_123\n' >"$tmp/bad-key.env"
key_body=$(printf 'A%.0s' {1..43})000000
printf 'PRODUCT_API_KEY=ppay_rk_live_%s\n' "$key_body" >"$tmp/live-key.env"
printf 'PRODUCT_API_KEY=ppay_rk_test_%s\n' "$key_body" >"$tmp/restricted.env"
printf 'PRODUCT_API_KEY=ppay_sk_test_%s\n' "$key_body" >"$tmp/secret.env"

expect_failure() {
    local name=$1 message=$2
    shift 2
    if "$preflight" "$@" --offline >"$tmp/$name.out" 2>"$tmp/$name.err"; then
        echo "product preflight accepted $name" >&2
        exit 1
    fi
    grep -F -- "$message" "$tmp/$name.err" >/dev/null || {
        echo "product preflight rejected $name for an unexpected reason:" >&2
        cat "$tmp/$name.err" >&2
        exit 1
    }
}
staging=(--environment-dir "$product")

expect_failure example "PRODUCT_API_KEY still contains replace-me" \
    --env "$tmp/example.env" --compose "$tmp/compose.yml" "${staging[@]}"
expect_failure sealed-empty "PRODUCT_API_KEY is empty" --env "$tmp/unsealed.env" \
    --compose "$tmp/compose.yml" "${staging[@]}"
expect_failure bad-key "PRODUCT_API_KEY must be a Phala Pay test API key" --unsealed \
    --env "$tmp/bad-key.env" --compose "$tmp/compose.yml" "${staging[@]}"
expect_failure live-key "PRODUCT_API_KEY must be a Phala Pay test API key" \
    --env "$tmp/live-key.env" --compose "$tmp/compose.yml" "${staging[@]}"
expect_failure slug "account must be the product's acct_ id" --unsealed \
    --env "$tmp/unsealed.env" --compose "$tmp/slug.yml" --environment-dir "$tmp/slug"
expect_failure web-origin "web_origin must be the website's https://HOST" --unsealed \
    --env "$tmp/unsealed.env" --compose "$tmp/web-origin.yml" --environment-dir "$tmp/web-origin"
expect_failure stale "differs from a fresh render" --unsealed \
    --env "$tmp/unsealed.env" --compose "$tmp/stale.yml" "${staging[@]}"
sed 's|DOMAIN: pay-demo-api.phala.com|DOMAIN: other.phala.com|' "$tmp/compose.yml" >"$tmp/other-domain.yml"
expect_failure other-domain "dstack-ingress must serve the host of the product's public_url" --unsealed \
    --env "$tmp/unsealed.env" --compose "$tmp/other-domain.yml" "${staging[@]}"
expect_failure http-rpc "chain 84532's rpc_url must use https" --unsealed \
    --env "$tmp/unsealed.env" --compose "$tmp/http-rpc.yml" --environment-dir "$tmp/http-rpc"
expect_failure keyed-rpc "chain 84532's rpc_url seems to embed an API key" --unsealed \
    --env "$tmp/unsealed.env" --compose "$tmp/keyed-rpc.yml" --environment-dir "$tmp/keyed-rpc"
expect_failure repeated "the product config repeats a chain" --unsealed \
    --env "$tmp/unsealed.env" --compose "$tmp/repeated.yml" --environment-dir "$tmp/repeated"
if grep -q 0123456789abcdef "$tmp/keyed-rpc.out" "$tmp/keyed-rpc.err"; then
    echo "product preflight printed the RPC key" >&2
    exit 1
fi
printf 'PRODUCT_API_KEY=\nEXTRA=\n' >"$tmp/extra.env"
expect_failure extra "the env may set only the compose's sealed names, not EXTRA" --unsealed \
    --env "$tmp/extra.env" --compose "$tmp/compose.yml" "${staging[@]}"
"$preflight" --env "$tmp/unsealed.env" --compose "$tmp/compose.yml" "${staging[@]}" --offline --unsealed >/dev/null
"$preflight" --env "$tmp/restricted.env" --compose "$tmp/compose.yml" "${staging[@]}" --offline >/dev/null
"$preflight" --env "$tmp/secret.env" --compose "$tmp/compose.yml" "${staging[@]}" --offline >/dev/null

# Both previously approved images and dev/GPU variants must fail locally.
for image in dstack-0.5.9 dstack-0.5.12 dstack-dev-0.6.1 dstack-nvidia-0.6.1; do
    expect_failure "image-$image" "OS image $image is not the approved dstack-0.6.1" --unsealed \
        --env "$tmp/unsealed.env" --compose "$tmp/compose.yml" "${staging[@]}" --os-image "$image"
done
"$preflight" --env "$tmp/unsealed.env" --compose "$tmp/compose.yml" "${staging[@]}" \
    --offline --unsealed --os-image dstack-0.6.1 >/dev/null

echo "product preflight local checks test passed"
