#!/usr/bin/env bash
# deploy/build-kit.sh works on its own, as an operator uses it: extracted outside any checkout, the
# kit renders an operator's environment directory, byte for byte as this checkout does, in every
# variant, and its preflight (--offline) and route-mode check accept the result. TOPUP names a local
# topup binary for those two (CI's build); without it they are skipped.
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
version=v0.0.0-test
fail() {
    echo "kit: $*" >&2
    exit 1
}

"$root/deploy/build-kit.sh" "$version" "$tmp/dist" >/dev/null

# The operator's side: the kit and an environment directory, in a directory of their own.
operator="$tmp/operator"
mkdir -p "$operator/kit" "$operator/production"
tar -xzf "$tmp/dist/phala-pay-deploy-$version.tar.gz" -C "$operator/kit" --strip-components=1
kit="$operator/kit"
cp -r "$kit/deploy/environments/example/topup" "$operator/production/topup"
env_dir="$operator/production/topup"
# Rehearse the example's test routes as a staging deployment; shipped config validation is separate.
sed -i -e 's|environment: production|environment: staging|' \
    -e 's|pay-api.example.com|pay-api.operator.test|' \
    -e 's|11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=|23Y9wEJMOTySGV3UXmcTFnQsbigA9/cYTvmqdQxzmdo=|' \
    "$env_dir/topup.yaml"
sed -i -e 's|s3://BUCKET/PATH|s3://operator-backups/production|' \
    -e 's|ACCOUNT.r2.cloudflarestorage.com|objects.operator.test|' \
    -e 's|pay-api.example.com|pay-api.operator.test|' "$env_dir/compose.yaml"
jq -n '{"phala-pay": "ghcr.io/phala-network/phala-pay@sha256:\("1" * 64)",
    "postgres-walg": "ghcr.io/phala-network/postgres-walg@sha256:\("2" * 64)",
    "phala-pay-reference-product": "ghcr.io/phala-network/phala-pay-reference-product@sha256:\("3" * 64)"}' \
    >"$operator/images.json"
gateway=(--gateway-domain gateway.dstack-pha-prod5.phala.network)
origin=(--restore-check --origin https://0123abcd-8081.dstack-pha-prod5.phala.network)
cd "$operator"
for inputs in "${gateway[*]}" "${origin[*]}"; do
    read -ra inputs <<<"$inputs"
    kit/deploy/render.sh "${inputs[@]}" --images images.json production/topup >rendered.yml ||
        fail "the kit does not render the operator's environment (${inputs[0]})"
    "$root/deploy/render.sh" "${inputs[@]}" --images images.json production/topup |
        cmp -s - rendered.yml || fail "the kit renders differently from the checkout (${inputs[0]})"
done
kit/deploy/render.sh --template --images images.json kit/deploy/environments/phala-cloud-template/topup |
    cmp -s - <("$root/deploy/render.sh" --template --images images.json \
        "$root/deploy/environments/phala-cloud-template/topup") ||
    fail "the kit renders the template differently from the checkout"

if [[ -z "${TOPUP:-}" ]]; then
    echo "deploy kit test passed (preflight skipped: TOPUP is not set)"
    exit 0
fi
kit/deploy/render.sh "${gateway[@]}" --images images.json production/topup >docker-compose.production.yml
(umask 077 && cat >.env.production <<'ENV'
AWS_ACCESS_KEY_ID=operator-key-id
AWS_SECRET_ACCESS_KEY=operator-secret
SENTRY_DSN=
TOPUP_RPC_ALCHEMY_SEPOLIA_KEY=operator-alchemy-key
ENV
)
kit/deploy/preflight.sh --env .env.production --compose docker-compose.production.yml \
    --environment-dir production/topup --offline >"$tmp/preflight.out" 2>&1 ||
    { cat "$tmp/preflight.out" >&2; fail "the kit's preflight refused the operator's environment"; }
kit/deploy/check-route-modes.sh staging docker-compose.production.yml >/dev/null ||
    fail "the kit's route-mode check refused the operator's routes"
echo "deploy kit test passed"
