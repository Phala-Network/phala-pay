#!/usr/bin/env bash
# Base KMS half of phala-cvm.sh. Only explicit prepare/commit commands can call Cloud.
# Never signs a chain transaction. Safe approval stays outside GitHub and this script.
set -euo pipefail
root=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=deploy/deadline.sh
source "$root/deadline.sh"
# shellcheck source=deploy/app-compose-baseline.sh
source "$root/app-compose-baseline.sh"
stage_start onchain-cloud 300
phala() { stage_call 30 "$root/phala" "$@"; }
kms() { stage_call 120 node "$root/tools/onchain-kms.mjs" "$@"; }
fail() { echo "$*" >&2; exit 1; }
load_environment() {
    [[ $(cat "$env_dir/kms") == base ]] || fail "onchain command requires kms base"
    safe=$(cat "$env_dir/dstack-owner")
    [[ "$safe" =~ ^0x[0-9a-fA-F]{40}$ ]] || fail "dstack-owner must be an expected Base Safe address"
    app=""
    [[ ! -f "$env_dir/dstack-app" ]] || app=$(cat "$env_dir/dstack-app")
}
check_baseline() {
    local actual=$1 baseline=$2 violations
    violations=$(jq -r -L "$root" --slurpfile baseline "$baseline" 'include "compose-policy";
        base_app_violations(.; $baseline[0])[]' "$actual")
    [[ -z "$violations" ]] || fail "$violations"
}
check_response() {
    kms check-hash "$request" "$output" >/dev/null
    device=$(jq -er '.device_id | select(test("^(0x)?[0-9a-fA-F]{64}$"))' "$output")
    hash=$(jq -er '.compose_hash' "$output")
    jq -e --slurpfile pin "$root/onchain-kms.json" '
        .kms_info.chain_id == 8453 and
        (.kms_info.kms_contract_address | ascii_downcase) == ($pin[0].kms_auth | ascii_downcase)
    ' "$output" >/dev/null || fail "Cloud response must name the reviewed Base KmsAuth"
}
command=${1:-}; shift || true
case "$command" in
    provision-prepare)
        [[ $# == 3 ]] || fail "usage: phala-cvm.sh provision-prepare REQUEST_JSON OUT ENVIRONMENT_DIR"
        request=$1 output=$2 env_dir=$3
        load_environment
        jq -e '.kms == "BASE" and (.node_id // .teepod_id | type == "number") and
            (.compose_file | type == "object") and (has("encrypted_env") | not)' "$request" >/dev/null ||
            fail "provision requires full compose_file, kms BASE, a pinned node and no encrypted_env"
        load_app_compose_baseline "$env_dir" "$output.baseline.json"
        jq '.compose_file' "$request" >"$output.request.json"
        check_baseline "$output.request.json" "$output.baseline.json"
        identity=$(kms identity)
        violations=$(jq -r -L "$root" --arg identity "$identity" 'include "compose-policy";
            kms_violations("base"; .; $identity)[]' "$output.request.json")
        [[ -z "$violations" ]] || fail "$violations"
        # Published OpenAPI: handle_provision_cvm_api_v1_cvms_provision_post.
        # Construct from the reviewed baseline, carrying only the two reviewed render fields.
        jq --slurpfile baseline "$output.baseline.json" '.compose_file as $render |
            .compose_file = ($baseline[0] | .docker_compose_file = $render.docker_compose_file |
                .pre_launch_script = $render.pre_launch_script)' "$request" >"$output.provision-request.json"
        phala api /cvms/provision -X POST --input "$output.provision-request.json" >"$output"
        request=$output.request.json
        check_response
        factory=$(jq -r .kms_auth "$root/onchain-kms.json")
        kms batch "$output.batch.json" "$safe" "$factory" deployAndRegisterApp "$hash" "$device"
        ;;
    provision-commit)
        [[ $# == 3 ]] || fail "usage: phala-cvm.sh provision-commit TX PREPARE_JSON ENVIRONMENT_DIR"
        tx=$1 output=$2 env_dir=$3 request=$2.request.json
        load_environment
        [[ "$app" =~ ^0x[0-9a-fA-F]{40}$ ]] || fail "commit the receipt's dstack-app before provision-commit"
        load_app_compose_baseline "$env_dir" "$output.baseline.json"
        check_baseline "$request" "$output.baseline.json"
        check_response
        deployed=$(kms deployment "$tx" "$safe")
        [[ -z "$app" || "${app,,}" == "${deployed,,}" ]] || fail "receipt app differs from committed dstack-app"
        "$root/verify-onchain-app.sh" "$deployed" "$safe" "$hash" "$device" >"$output.onchain.json"
        # Pinned CLI src/commands/deploy/index.ts hI: commitCvmProvision after deployAppAuth.
        # OpenAPI: handle_create_cvm_from_povision_api_v1_cvms_post (CreateTappRequest).
        jq --arg app "${deployed#0x}" --arg contract "$deployed" --arg safe "$safe" \
            --slurpfile request "$request" '{app_id: $app, compose_hash, kms_id,
                contract_address: $contract, deployer_address: $safe,
                env_keys: $request[0].allowed_envs} | with_entries(select(.value != null))' "$output" >"$output.commit.json"
        phala api /cvms -X POST --input "$output.commit.json"
        ;;
    compose-prepare)
        [[ $# == 6 && "$1" =~ ^[A-Za-z0-9_-]+$ ]] ||
            fail "usage: phala-cvm.sh compose-prepare CVM_ID COMPOSE PRE_LAUNCH PORT_POLICY_JSON OUT ENVIRONMENT_DIR"
        cvm_id=$1 compose=$2 pre_launch=$3 policy=$4 output=$5 env_dir=$6 request=$5.request.json
        load_environment
        [[ "$app" =~ ^0x[0-9a-fA-F]{40}$ ]] || fail "commit dstack-app before preparing an upgrade"
        phala cvms get "$cvm_id" --json >"$output.cvm.json"
        actual=$(jq -er '.app_id | ascii_downcase | ltrimstr("0x")' "$output.cvm.json")
        [[ "$actual" == "$(tr 'A-F' 'a-f' <<<"${app#0x}")" ]] || fail "CVM app differs from committed dstack-app"
        jq -e --slurpfile pin "$root/onchain-kms.json" '
            (.kms_type | ascii_downcase) == "base" and .kms_info.chain_id == 8453 and
            ((.kms_info.dstack_kms_address // .kms_info.kms_contract_address) | ascii_downcase) ==
                ($pin[0].kms_auth | ascii_downcase)' "$output.cvm.json" >/dev/null ||
            fail "existing CVM must use the reviewed Base KmsAuth before prepare"
        load_app_compose_baseline "$env_dir" "$output.baseline.json"
        phala api "/cvms/$cvm_id/compose_file" >"$output.current.json"
        check_baseline "$output.current.json" "$output.baseline.json"
        # Unified PATCH cannot alter port_policy: it must already equal the reviewed policy
        # installed during initial provisioning. Every other field comes from the baseline.
        jq -e --argjson policy "$policy" '.port_policy == $policy and
            (has("encrypted_env") | not) and (has("env_keys") | not) and
            (has("update_env_vars") | not)' "$output.current.json" >/dev/null ||
            fail "current port policy differs or current compose contains env update fields"
        jq --rawfile compose "$compose" --rawfile pre_launch "$pre_launch" '
            .docker_compose_file = $compose | .pre_launch_script = $pre_launch' "$output.baseline.json" >"$request"
        check_baseline "$request" "$output.baseline.json"
        identity=$(kms identity)
        violations=$(jq -r -L "$root" --arg identity "$identity" 'include "compose-policy";
            kms_violations("base"; .; $identity)[]' "$request")
        [[ -z "$violations" ]] || fail "$violations"
        # SDK patchCvm sends X-Prepare-Only and extracts 465's commit_token, as CLI 1.1.22.
        # This token is private working state: no stdout, no uploaded artifacts.
        local_hash=$(kms hash "$request")
        current_hash=$(jq -er '.compose_hash | ascii_downcase | ltrimstr("0x")' "$output.cvm.json")
        if [[ "$local_hash" == "$current_hash" ]]; then
            # Acceptance/retry with identical bytes needs no Cloud mutation or bearer token.
            jq --arg hash "$local_hash" '{compose_hash: $hash, app_id, no_op: true,
                device_id: .node_info.device_id, kms_info: {chain_id: .kms_info.chain_id,
                kms_contract_address: (.kms_info.dstack_kms_address // .kms_info.kms_contract_address)}}' \
                "$output.cvm.json" >"$output"
        else
            kms cloud-prepare "$cvm_id" "$request" "$output"
        fi
        check_response
        returned_app=$(jq -er '.app_id | ascii_downcase | ltrimstr("0x")' "$output")
        [[ "$returned_app" == "$actual" ]] || fail "prepare app differs from CVM"
        kms batch "$output.batch.json" "$safe" "$app" addComposeHash "$hash"
        # Always show device approval as a separate batch; a repeat addDevice is idempotent.
        kms batch "$output.device-batch.json" "$safe" "$app" addDevice "$device"
        jq '{compose_hash, app_id, device_id, kms_info}' "$output" >"$output.summary.json"
        ;;
    compose-commit)
        [[ $# == 5 && "$1" =~ ^[A-Za-z0-9_-]+$ ]] ||
            fail "usage: phala-cvm.sh compose-commit CVM_ID HASH TX|already-registered PREPARE_JSON ENVIRONMENT_DIR"
        cvm_id=$1 hash=$2 tx=$3 output=$4 env_dir=$5 request=$4.request.json
        [[ "$tx" == already-registered || "$tx" =~ ^0x[0-9a-fA-F]{64}$ ]] || fail "invalid transaction hash"
        expected_hash=$hash
        load_environment
        load_app_compose_baseline "$env_dir" "$output.baseline.json"
        check_baseline "$request" "$output.baseline.json"
        check_response
        [[ "${hash#0x}" == "${expected_hash#0x}" ]] || fail "commit hash differs from prepared hash"
        "$root/verify-onchain-app.sh" "$app" "$safe" "$hash" "$device" >"$output.onchain.json"
        if [[ "$tx" != already-registered ]]; then kms receipt "$tx" >/dev/null; fi
        if jq -e '.no_op == true' "$output" >/dev/null; then
            echo "compose already current and onchain policy verified; no commit needed" >&2
            exit 0
        fi
        jq --arg tx "$tx" '{token, compose_hash, transaction_hash: $tx}' "$output" >"$output.commit.json"
        # Pinned CLI deploy --commit calls commitCvmUpdate (POST commit-update).
        # OpenAPI: commit_cvm_update_api_v1_cvms__cvm_id__commit_update_post.
        status=0
        phala api "/cvms/$cvm_id/commit-update" -X POST --input "$output.commit.json" \
            >"$output.commit-response.json" 2>"$output.commit-error.txt" || status=$?
        ((status == 0)) || fail "Cloud commit-update failed (exit $status); prepare again if the token expired"
        # A token endpoint's response is untrusted; never print arbitrary stdout or stderr.
        jq '{status: (if .status == "in_progress" then .status else "accepted" end),
            correlation_id: (if (.correlation_id | type) == "string" and
                (.correlation_id | test("^[A-Za-z0-9_-]{1,100}$")) then .correlation_id else null end)}' \
            "$output.commit-response.json" 2>"$output.commit-parse-error.txt" ||
            fail "Cloud commit-update returned an invalid response"
        ;;
    *) fail "unknown onchain command" ;;
esac
