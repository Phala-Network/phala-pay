#!/usr/bin/env bash
# Preflight for a topup deploy (deploy/README.md, "Deploy" and "Sealing the secrets"). It is
# read-only against remote systems: it never pushes, deploys, updates, or sends a transaction.
#
# Usage: deploy/preflight.sh --env FILE --compose FILE --environment-dir DIR
#          [--restore-check | --template] (--workspace NAME --os-image NAME | --offline) [--unsealed] [--require-sentry]
#
# --compose is the rendered compose (deploy/render.sh), --environment-dir the environment it was
# rendered from, --restore-check expects the restore-check variant (deploy/RESTORE.md), and
# --template the Phala Cloud template variant, whose env file also holds the deploy form's values
# (deploy/README.md, "The Phala Cloud template variant").
#
# --offline makes no network access and pulls nothing. It checks:
# - the env file: only the compose's sealed names (one left out is unset), and no placeholder;
# - the compose: deploy/compose-policy.jq, no example value, and a byte-for-byte fresh render;
# - the configuration, with `topup config check` run in the compose's pinned image, which must
#   already be present (`docker pull` it first). With the env file's keys it also checks that
#   every RPC provider's sealed key fits its URL (--secrets). --unsealed skips that, and accepts
#   empty secrets: Deploy provisions with them empty and the owner seals them from their own
#   machine.
#
# Online, it also pulls every image anonymously, as the CVM does: a private image fails here. It
# checks each RPC URL (https, no embedded key) and each route's chain, contracts, and asset
# through every provider the route names; that needs the keys, so --unsealed skips it for a
# keyed provider. Finally it checks the Phala Cloud workspace and the owner-approved OS image
# dstack-0.5.9 (deploy/README.md). PHALA selects the CLI command (default
# deploy/phala, the locked CLI), and TOPUP a local topup command instead of the pinned image's (tests).
#
# Every failure is reported, and the exit status is 1 if any. Output never prints a URL with its
# key, or a secret.
set -euo pipefail
source "$(dirname -- "$0")/contracts/common.sh"
source "$(dirname -- "$0")/preflight-phala.sh"
source "$(dirname -- "$0")/preflight-rpc.sh"

networks="$DEPLOY_CONTRACTS_DIR/networks.json"
# The owner-approved OS image (deploy/README.md, "OS image"): production, dstack 0.5.9.
approved_os_image=dstack-0.5.9

usage() {
    echo "usage: $0 --env FILE --compose FILE --environment-dir DIR [--restore-check | --template]" \
        "(--workspace NAME --os-image NAME | --offline) [--unsealed] [--require-sentry]" >&2
    exit 64
}
env_file="" compose="" env_dir="" workspace="" os_image="" offline=0 unsealed=0 require_sentry=0 variant=service
while (($#)); do
    case "$1" in
        --env) env_file="${2:-}"; shift 2 ;;
        --compose) compose="${2:-}"; shift 2 ;;
        --environment-dir) env_dir="${2:-}"; shift 2 ;;
        --workspace) workspace="${2:-}"; shift 2 ;;
        --os-image) os_image="${2:-}"; shift 2 ;;
        --restore-check) variant=restore-check; shift ;;
        --template) variant=template; shift ;;
        --offline) offline=1; shift ;;
        --unsealed) unsealed=1; shift ;;
        --require-sentry) require_sentry=1; shift ;;
        *) usage ;;
    esac
done
[[ -f "$env_file" && -f "$compose" && -d "$env_dir" ]] || usage
((offline)) || [[ -n "$workspace" && -n "$os_image" ]] || usage
for command in docker jq; do
    require_command "$command"
done

tmp=$(mktemp -d "${TMPDIR:-/tmp}/topup-preflight.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
failures=0
fail() {
    printf 'FAIL: %s\n' "$*" >&2
    failures=$((failures + 1))
}
ok() {
    printf 'ok: %s\n' "$*"
}

declare -A env=()
check_artifact "$env_file" "$compose" "$env_dir" "$variant"
refuse_example_values "$compose"
while IFS= read -r name; do
    value=${env[$name]-}
    if [[ "$value" == *replace-me* ]]; then
        fail "$name still contains replace-me"
    elif [[ -z "$value" ]] && ((unsealed == 0)) && [[ "$name" != SENTRY_DSN && "$name" != TOPUP_RPC_*_KEY ]]; then
        # An empty or unset DSN turns Sentry off; no key means a keyless URL (config check --secrets).
        fail "$name is empty or not set"
    fi
done <"$tmp/sealed"
sentry_dsn=${env[SENTRY_DSN]-}
[[ -z "$sentry_dsn" || "$sentry_dsn" =~ ^https://[0-9a-f]{32}@[a-z0-9.-]+/[0-9]+$ ]] ||
    fail "SENTRY_DSN must be empty or the project's DSN, https://KEY@HOST/PROJECT_ID"
if [[ -n "$os_image" && "$os_image" != "$approved_os_image" ]]; then
    fail "OS image $os_image is not the approved $approved_os_image (deploy/README.md)"
fi
if ((failures)); then
    echo "preflight: $failures local check(s) failed; the configuration and online checks were not run" >&2
    exit 1
fi

((offline)) || check_anonymous_pulls "$tmp/images"

echo "== configuration"
topup_image=$(jq -r '.services.topup.image' "$tmp/compose.json")
jq -j --arg target /etc/topup/topup.yaml \
    '. as $root | [.services.topup.configs[] | select(.target == $target) | .source][0] as $name
    | $root.configs[$name].content' "$tmp/compose.json" | sed 's/[$][$]/$/g' >"$tmp/topup.yaml"
# RPC checks always use the rendered compose and candidate env, exactly as the CVM does.
topup() {
    local name values=() unset=()
    for name in "${!env[@]}"; do
        unset+=(-u "$name")
        [[ "$name" == SENTRY_DSN || "$name" == TOPUP_RPC_*_KEY ]] || continue
        values+=("$name=${env[$name]}")
    done
    if [[ -n "${TOPUP:-}" && "$1" != rpc ]]; then
        env -i PATH="$PATH" "${values[@]}" "$TOPUP" "$@" /dev/stdin <"$tmp/topup.yaml"
    else
        env "${unset[@]}" docker compose --env-file "$env_file" -f "$compose" \
            run --rm --no-deps topup topup "$@" /etc/topup/topup.yaml
    fi
}
secrets=()
((unsealed)) || secrets=(--secrets)
sentry_args=()
((require_sentry)) && sentry_args=(--require-sentry)
if [[ -z "${TOPUP:-}" ]] && ! docker image inspect "$topup_image" >/dev/null 2>&1; then
    fail "the pinned image is not present locally; docker pull $topup_image, then run preflight again"
elif topup config check "${sentry_args[@]}" "${secrets[@]}" >"$tmp/check.out" 2>&1 &&
    topup config show >"$tmp/config.json" 2>"$tmp/show.err"; then
    ok "$(head -n 1 "$tmp/check.out")"
else
    fail "topup config check refused the configuration: $(tool_error "$tmp/check.out")"
fi
if ((failures)); then
    echo "preflight: $failures check(s) failed; the online checks were not run" >&2
    exit 1
fi

# Every RPC URL is published with the compose: https, and a key only in its `{key}`.
declare -A provider_url=()
while IFS=$'\t' read -r id url configured_key; do
    [[ "$url" == https://* ]] || fail "RPC provider $id must use https"
    embeds_key "${url//\{key\}/}" &&
        fail "RPC provider $id's URL seems to embed an API key, which the compose publishes;" \
            "attest it with {key} in the key's place and seal the key"
    key_name=$(jq -rn --arg id "$id" '"TOPUP_RPC_\($id | ascii_upcase | gsub("[^A-Z0-9]"; "_"))_KEY"')
    [[ -z "$configured_key" ]] || key_name=$configured_key
    key=${env[$key_name]-}
    [[ -z "$key" ]] || url=${url//"{key}"/"$key"}
    provider_url[$id]=$url
    for service in topup restore-check; do
        # The rendered restore variant retains the topup service; its command is overridden above.
        if jq -e --arg service "$service" '.services[$service] != null' "$tmp/compose.json" >/dev/null; then
            jq -e --arg service "$service" --arg key "$key_name" \
                '.services[$service].environment | has($key)' "$tmp/compose.json" >/dev/null ||
                fail "configured sealed_key $key_name is missing from $service compose mapping"
        fi
    done
    if ((unsealed == 0)) && [[ -z "$key" ]]; then
        fail "configured sealed_key $key_name is missing from candidate sealed env"
    fi
done < <(jq -r '.rpc[] | .read,.verify | [.id, .url, .sealed_key] | @tsv' "$tmp/config.json")
while IFS=$'\t' read -r route factory implementation; do
    for key in "$factory" "$implementation"; do
        if ! is_address "$key" || grep -Eiq '^0x([0-9a-f])\1{39}$' <<<"$key"; then
            fail "route $route: $key is a placeholder or zero address; deploy the contracts" \
                "(deploy/CONTRACTS.md) and commit the real address"
        fi
    done
done < <(jq -r '.routes[] | [.route, .chain.forwarder_factory, .chain.implementation] | @tsv' \
    "$tmp/config.json")

if ((offline)); then
    if ((failures)); then
        echo "preflight: $failures local check(s) failed" >&2
        exit 1
    fi
    echo "preflight: local checks passed (offline)"
    exit 0
fi

echo "== asset chains (RPC URLs are not printed)"
if [[ "${provider_url[*]}" == *"{key}"* ]]; then
    echo "note: skipped: a keyed RPC provider has no key here (--unsealed); run preflight online" \
        "with the sealed env file to check the asset chains"
else
    # verify-deployment.sh compares with the committed reference deployment: cast, no Solidity build.
    require_command cast
    redact() {
        local text=$1 id key
        for id in "${!provider_url[@]}"; do
            text=${text//"${provider_url[$id]}"/provider $id}
        done
        for key in "${!env[@]}"; do
            [[ "$key" == TOPUP_RPC_*_KEY && -n "${env[$key]}" ]] && text=${text//"${env[$key]}"/[key]}
        done
        printf '%s' "$text"
    }
    # rpc URL CAST_ARGS...: cast's answer from the provider at URL, or "error: " and its redacted error.
    rpc() {
        local url=$1
        shift
        ETH_RPC_URL=$url cast "$@" 2>"$tmp/cast.err" ||
            printf 'error: %s' "$(redact "$(tool_error "$tmp/cast.err")")"
    }
    check_rpc_endpoints "$tmp"
    # Both endpoints must pass the typed self-test before manifest validation.
    while IFS=$'\t' read -r name chain_id factory implementation contract oracle decimals providers; do
        read -ra configured_ids <<<"$providers"
        ids=()
        for id in "${configured_ids[@]}"; do
            if jq -e --arg id "$id" 'index($id) != null' "$tmp/healthy.json" >/dev/null; then
                ids+=("$id")
            fi
        done
        ((${#ids[@]})) || continue
        chain_ok=1
        for id in "${ids[@]}"; do
            reported=$(rpc "${provider_url[$id]}" chain-id)
            if [[ "$reported" == "$chain_id" ]]; then
                ok "provider $id reports chain id $reported (route $name)"
            else
                fail "provider $id reports chain id $reported, route $name needs $chain_id"
                chain_ok=0
            fi
        done
        ((chain_ok)) || continue
        network=$(jq -r --argjson id "$chain_id" \
            '.networks | to_entries[] | select(.value.chain_id == $id) | .key' "$networks")
        if [[ -z "$network" ]]; then
            fail "$networks names no network with chain id $chain_id (route $name)"
            continue
        fi
        # Once per chain: a chain's routes name the same providers.
        verification=$tmp/verification-$chain_id.json
        if ! [[ -e "$verification" ]]; then
            targets=()
            for id in "${ids[@]}"; do
                targets+=(--rpc "$network/$id=${provider_url[$id]}")
            done
            check_contract_deployment "$verification" "$chain_id" "${targets[@]}"
        fi
        if jq -e --argjson count "${#ids[@]}" --arg factory "$factory" \
            --arg implementation "$implementation" \
            '(.chains | length) == $count and all(.chains[];
                (.factory | ascii_downcase) == ($factory | ascii_downcase) and
                (.implementation | ascii_downcase) == ($implementation | ascii_downcase))' \
            "$verification" >/dev/null 2>&1; then
            ok "route $name factory and implementation match the verified deployment"
        else
            fail "route $name contract addresses differ from the verified deployment"
        fi
        for id in "${ids[@]}"; do
            for address in "$contract" "$oracle"; do
                code=$(rpc "${provider_url[$id]}" code "$address")
                [[ "$code" =~ ^0x[0-9a-fA-F]+$ && "$code" != 0x ]] ||
                    fail "route $name: $address has no code on provider $id (${code:0:300})"
            done
        done
        reported=$(rpc "${provider_url[${ids[0]}]}" call "$contract" 'decimals()(uint8)')
        [[ "$reported" == "$decimals" ]] ||
            fail "route $name: asset decimals() is $reported, the route says $decimals"
    done < <(jq -r '. as $config | .routes[] | [.route, .chain.chain_id, .chain.forwarder_factory,
        .chain.implementation, .asset.contract, .chain.sanctions_oracle, .asset.decimals,
        (.chain.chain_id as $chain | [$config.rpc[] | select(.chain_id == $chain) | .read.id,.verify.id] | join(" "))] | @tsv' "$tmp/config.json")
fi

check_phala_cloud "$workspace" "$os_image"

if ((failures)); then
    echo "preflight: $failures check(s) failed" >&2
    exit 1
fi
echo "preflight: all checks passed"
