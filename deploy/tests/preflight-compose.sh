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
# These must never override the candidate, including a name omitted from the candidate.
export TOPUP_RPC_ANKR_KEY=ambient-read TOPUP_RPC_INFURA_KEY=ambient-verify SENTRY_DSN=ambient-sentry
export TOPUP_RPC_BLOCKPI_ETHEREUM_KEY=ambient-verify TOPUP_RPC_BLOCKPI_BASE_KEY=ambient-verify
for environment in staging production; do
    verify_keys=(TOPUP_RPC_INFURA_KEY)
    [[ "$environment" != production ]] || verify_keys=(TOPUP_RPC_BLOCKPI_ETHEREUM_KEY TOPUP_RPC_BLOCKPI_BASE_KEY)
    printf '%s\n' TOPUP_RPC_ANKR_KEY "${verify_keys[@]}" SENTRY_DSN >"$tmp/sealed"
    printf '%s\n' TOPUP_RPC_ANKR_KEY=candidate-read >"$tmp/candidate.env"
    probe_command='test "$1 $2 $3 $4 $5" = "topup rpc check --config /etc/topup/topup.yaml" && test "$TOPUP_RPC_ANKR_KEY" = candidate-read && test -z "$SENTRY_DSN" && test -r /etc/topup/topup.yaml'
    for key in "${verify_keys[@]}"; do
        printf '%s=candidate-verify\n' "$key" >>"$tmp/candidate.env"
        probe_command+=" && test \"\$$key\" = candidate-verify"
    done
    for variant in service restore-check; do
        name="$environment-$variant"
        inputs=(--gateway-domain gateway.dstack-pha-prod5.phala.network)
        [[ "$variant" != restore-check ]] || inputs=(--restore-check --origin https://restore.example.net)
        "$root/deploy/render.sh" "${inputs[@]}" --project-name "$project-$name" \
            --images "$tmp/images.json" "$root/deploy/environments/phala-network/$environment/topup" >"$tmp/rendered.yml"
        "$compose" -f "$tmp/rendered.yml" config --no-interpolate --format json |
            jq --arg image "$probe" --arg command "$probe_command" '
                .services |= with_entries(select(.key == "topup" or .key == "restore-check")
                    | .value |= (del(.depends_on,.volumes,.ports,.healthcheck,.networks)
                        | .image=$image | .network_mode="none" | .restart="no"
                        | .entrypoint=["/bin/sh","-c",$command, "probe"]))
                | del(.volumes,.networks)
            ' >"$tmp/raw.json"
        # Escape only shell dollars; preserve candidate interpolation in the rendered environment.
        jq '.services[].entrypoint |= map(if startswith("test ") then gsub("\\$"; "$$") else . end)' \
            "$tmp/raw.json" >"$tmp/probe-$name.json"
        compose_topup "$tmp/candidate.env" "$tmp/probe-$name.json" "$tmp/sealed" rpc check --config
        for key in "${verify_keys[@]}"; do
            grep -v "^$key=" "$tmp/candidate.env" >"$tmp/missing.env"
            if compose_topup "$tmp/missing.env" "$tmp/probe-$name.json" "$tmp/sealed" rpc check --config; then
                echo 'ambient verify key supplied a missing candidate secret' >&2; exit 1
            fi
            jq --arg key "$key" 'del(.services.topup.environment[$key])' "$tmp/probe-$name.json" >"$tmp/probe-$name-missing.json"
            if compose_topup "$tmp/candidate.env" "$tmp/probe-$name-missing.json" "$tmp/sealed" rpc check --config; then
                echo 'missing compose key mapping passed' >&2; exit 1
            fi
        done
        echo "ok: $name candidate env, missing key, missing mapping, command override"
    done
done
