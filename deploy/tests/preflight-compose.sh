#!/usr/bin/env bash
# Real Compose candidate-env propagation and command override for both preflight variants.
# Typed RPC evidence is covered by rpc_probe; this probe tests the container boundary.
set -euo pipefail
root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
source "$root/deploy/preflight-rpc.sh"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/topup-preflight-compose.XXXXXX")
project=topup-preflight-compose-$$
probe=alpine:3.22@sha256:5291449c3df73caf6ed85e649dec1b9e818b39a5d8c871e97afc13e9cd5e8fa8
compose=$("$root/deploy/pinned-compose.sh")
cleanup() {
    for file in "$tmp"/probe-*.json; do
        [[ -f "$file" ]] || continue
        "$compose" -f "$file" down --timeout 1 >/dev/null 2>&1 || true
    done
    rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
docker image inspect "$probe" >/dev/null 2>&1 || docker pull "$probe" >/dev/null
jq -n --arg image "$probe" '{"phala-pay":$image,"postgres-walg":$image}' >"$tmp/images.json"
printf '%s\n' TOPUP_RPC_ANKR_KEY TOPUP_RPC_INFURA_KEY SENTRY_DSN >"$tmp/sealed"
printf '%s\n' TOPUP_RPC_ANKR_KEY=candidate-read TOPUP_RPC_INFURA_KEY=candidate-verify >"$tmp/candidate.env"
# These must never override the candidate, including a name omitted from the candidate.
export TOPUP_RPC_ANKR_KEY=ambient-read TOPUP_RPC_INFURA_KEY=ambient-verify SENTRY_DSN=ambient-sentry
for variant in service restore-check; do
    inputs=(--gateway-domain gateway.dstack-pha-prod5.phala.network)
    [[ "$variant" != restore-check ]] || inputs=(--restore-check --origin https://restore.example.net)
    "$root/deploy/render.sh" "${inputs[@]}" --project-name "$project-$variant" \
        --images "$tmp/images.json" "$root/deploy/environments/phala-network/staging/topup" >"$tmp/rendered.yml"
    "$compose" -f "$tmp/rendered.yml" config --no-interpolate --format json |
        jq --arg image "$probe" '
            .services |= with_entries(select(.key == "topup" or .key == "restore-check")
                | .value |= (del(.depends_on,.volumes,.ports,.healthcheck,.networks)
                    | .image=$image | .network_mode="none" | .restart="no"
                    | .entrypoint=["/bin/sh","-c","test \"$1 $2 $3 $4 $5\" = \"topup rpc check --config /etc/topup/topup.yaml\" && test \"$TOPUP_RPC_ANKR_KEY\" = candidate-read && test \"$TOPUP_RPC_INFURA_KEY\" = candidate-verify && test -z \"$SENTRY_DSN\" && test -r /etc/topup/topup.yaml", "probe"]))
            | del(.volumes,.networks)
        ' >"$tmp/raw.json"
    # Escape only shell dollars; preserve candidate interpolation in the rendered environment.
    jq '.services[].entrypoint |= map(if startswith("test ") then gsub("\\$"; "$$") else . end)' \
        "$tmp/raw.json" >"$tmp/probe-$variant.json"
    compose_topup "$tmp/candidate.env" "$tmp/probe-$variant.json" "$tmp/sealed" rpc check --config
    grep -v '^TOPUP_RPC_INFURA_KEY=' "$tmp/candidate.env" >"$tmp/missing.env"
    if compose_topup "$tmp/missing.env" "$tmp/probe-$variant.json" "$tmp/sealed" rpc check --config; then
        echo 'ambient verify key supplied a missing candidate secret' >&2; exit 1
    fi
    jq 'del(.services.topup.environment.TOPUP_RPC_INFURA_KEY)' "$tmp/probe-$variant.json" >"$tmp/probe-$variant-missing.json"
    if compose_topup "$tmp/candidate.env" "$tmp/probe-$variant-missing.json" "$tmp/sealed" rpc check --config; then
        echo 'missing compose key mapping passed' >&2; exit 1
    fi
    echo "ok: $variant candidate env, missing key, missing mapping, command override"
done
