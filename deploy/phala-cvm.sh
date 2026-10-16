#!/usr/bin/env bash
# Phala Cloud CVM steps of .github/workflows/deploy.yml, with the locked CLI (deploy/phala: Phala
# Cloud CLI 1.1.22; PHALA_CLOUD_API_KEY and PHALA_CLOUD_DIR come from the environment).
#
# Usage:
#   deploy/phala-cvm.sh get CVM_ID >cvm.json
#   deploy/phala-cvm.sh url CVM_JSON PORT            the gateway URL of PORT
#   deploy/phala-cvm.sh gateway-domain CVM_JSON      the CVM's gateway host, gateway.BASE_DOMAIN: a
#                                                    custom domain's CNAME target
#   deploy/phala-cvm.sh deploy OUTPUT CLI_ARGS...    `deploy --json`; its output goes to the log, its
#                                                    JSON object to OUTPUT, and it must succeed
#   deploy/phala-cvm.sh compose-update CVM_ID COMPOSE PRE_LAUNCH PORT_POLICY_JSON OUT
#                                                    preserve the sealed env, provision and commit
#                                                    one app-compose; OUT holds the provision JSON
#   deploy/phala-cvm.sh os-image CVM_ID IMAGE >cvm.json
#                                                    change the OS image, then wait for it to run,
#                                                    settled (15 minutes including the PATCH)
#   deploy/phala-cvm.sh wait [--unsealed] [--compose-hash HASH] CVM_ID [PREVIOUS_HASH] >cvm.json
#                                                    until the CVM runs, settled, with a compose hash
#                                                    other than PREVIOUS_HASH (15 minutes); with
#                                                    --unsealed, settled in any status
#                                                    --compose-hash waits for exactly that hash
#   deploy/phala-cvm.sh attestation CVM_ID HASH >attestation.json
#                                                    until the attestation reports compose hash HASH
#                                                    (10 minutes; an upgrade attests late): the CVM
#                                                    booted that compose
#   deploy/phala-cvm.sh instance-id ATTESTATION_JSON the instance id, from the event log's single
#                                                    instance-id event
#   deploy/phala-cvm.sh healthz URL                  until URL/healthz answers (10 minutes)
set -euo pipefail

# shellcheck source=deploy/deadline.sh
source "$(dirname -- "$0")/deadline.sh"

phala() {
    stage_call 30 "$(dirname -- "$0")/phala" "$@"
}

# The compose hash of a `cvms get` or attestation document: lowercase hex without 0x.
normal_hash='ascii_downcase | ltrimstr("0x")'

command=${1:-}
shift || true
case "$command" in
    get)
        stage_start get 60
        phala cvms get "$1" --json
        ;;
    url)
        jq -er --arg port "$2" '"https://\(.app_id | ltrimstr("0x"))-\($port).\(.gateway.base_domain)"' "$1"
        ;;
    gateway-domain)
        jq -er '"gateway.\(.gateway.base_domain)"' "$1"
        ;;
    deploy)
        stage_start deploy 300
        output=$1
        shift
        # `deploy --json` writes `Provisioning CVM ...` before the JSON object on a new CVM.
        # Keep the CLI's machine-readable stdout separate from stage diagnostics on stderr.  The
        # deadline wrapper reports elapsed time on stderr; merging both streams makes jq parse the
        # diagnostic line as if it were part of the JSON response.
        # On failure the CLI's --json error (code, message, request id) is on stdout: show it.
        status=0
        stage_call 300 "$(dirname -- "$0")/phala" deploy --json "$@" >"$output.raw" || status=$?
        cat "$output.raw" >&2
        ((status == 0)) || exit "$status"
        sed -n '/^{/,$p' "$output.raw" >"$output"
        jq -e '.success == true' "$output" >/dev/null
        ;;
    compose-update)
        [[ $# -eq 5 && "$1" =~ ^[A-Za-z0-9_-]+$ ]] || {
            echo "usage: $0 compose-update CVM_ID COMPOSE PRE_LAUNCH PORT_POLICY_JSON OUT" >&2
            exit 64
        }
        cvm_id=$1 compose=$2 pre_launch=$3 policy=$4 output=$5
        jq -en --argjson policy "$policy" '$policy | type == "object" and
            keys == ["ports", "restrict_mode"] and .restrict_mode == false and
            (.ports | type == "array" and all(type == "object" and keys == ["port", "pp"] and
                (.port | type == "number" and floor == . and . >= 1 and . <= 65535) and
                (.pp | type == "boolean"))) and
            ([.ports[].port] | length == (unique | length))' >/dev/null || {
            echo "invalid port policy (restrict_mode must remain false)" >&2
            exit 64
        }
        [[ -r "$compose" && -r "$pre_launch" ]] || {
            echo "compose and pre-launch script must be readable files" >&2
            exit 64
        }
        stage_start compose-update "${COMPOSE_UPDATE_SECONDS:-300}"
        # Raw API JSON goes to files; print errors even when the CLI puts them on stdout (#466).
        status=0
        phala api "/cvms/$cvm_id/compose_file" >"$output.current.json" || status=$?
        if ((status != 0)); then cat "$output.current.json" >&2; exit "$status"; fi
        # Keep the current app-compose, including allowed_envs and all KMS/storage settings.
        # No encrypted_env, env_keys, or update_env_vars: the sealed environment stays untouched.
        jq -e --rawfile compose "$compose" --rawfile pre_launch "$pre_launch" --argjson policy "$policy" '
            select(type == "object" and (.docker_compose_file | type == "string")
                and (has("encrypted_env") | not) and (has("env_keys") | not)
                and (has("update_env_vars") | not))
            | .docker_compose_file = $compose | .pre_launch_script = $pre_launch
            | .port_policy = $policy | .public_logs = false | .public_sysinfo = false
        ' "$output.current.json" >"$output.request.json" || {
            cat "$output.current.json" >&2
            echo "invalid current app-compose response" >&2
            exit 1
        }
        status=0
        phala api "/cvms/$cvm_id/compose_file/provision" -X POST --input "$output.request.json" >"$output" || status=$?
        cat "$output" >&2
        ((status == 0)) || exit "$status"
        hash=$(jq -er '.compose_hash | select(type == "string" and test("^(0x)?[0-9a-fA-F]{64}$"))' "$output") || {
            echo "compose provision returned no valid compose hash" >&2
            exit 1
        }
        jq -n --arg hash "$hash" '{compose_hash: $hash}' >"$output.commit.json"
        status=0
        response=$(phala api "/cvms/$cvm_id/compose_file" -X PATCH --input "$output.commit.json") || status=$?
        [[ -z "$response" ]] || printf '%s\n' "$response" >&2
        ((status == 0)) || exit "$status"
        ;;
    os-image)
        [[ $# -eq 2 && "$1" =~ ^[A-Za-z0-9_-]+$ && "$2" =~ ^[A-Za-z0-9._-]+$ ]] || {
            echo "usage: $0 os-image CVM_ID IMAGE" >&2
            exit 64
        }
        stage_start os-image "${OS_IMAGE_WAIT_SECONDS:-900}"
        # CLI 1.1.22 has no OS-image subcommand. Its raw API sends this JSON body; Cloud
        # responds 202 with no body and reboots the same CVM, retaining app/KMS identity.
        status=0
        response=$(phala api "/cvms/$1/os-image" -X PATCH -F "os_image_name=$2") || status=$?
        [[ -z "$response" ]] || printf '%s\n' "$response" >&2
        ((status == 0)) || exit "$status"
        operation_started=false
        while stage_remaining; do
            status=0
            cvm=$(phala cvms get "$1" --json) || status=$?
            if ((status == 0)); then
                jq -r '"status=\(.status) in_progress=\(.in_progress) os_image=\(.os.name)"' \
                    <<<"$cvm" >&2 || true
                # Cloud can publish the new image name before the reboot starts. Require a
                # busy/non-running observation before accepting its settled running state.
                if jq -e '.in_progress == true or
                    ((.status | type) == "string" and .status != "running")' <<<"$cvm" >/dev/null; then
                    operation_started=true
                fi
                if [[ "$operation_started" == true ]] && jq -e --arg image "$2" \
                    '.os.name == $image and .status == "running" and .in_progress == false' \
                    <<<"$cvm" >/dev/null; then
                    printf '%s\n' "$cvm"
                    exit 0
                fi
            else
                # --json errors are on stdout, just as with `deploy --json` (#466).
                [[ -z "$cvm" ]] || printf '%s\n' "$cvm" >&2
            fi
            stage_sleep 15
        done
        stage_expired
        echo "::error::CVM $1 did not run OS image $2" >&2
        exit 1
        ;;
    wait)
        # Settled: no operation in progress, with a new compose. A provision (--unsealed) only
        # creates the CVM: an unsealed CVM's app-compose fails until its owner seals the secrets
        # (topup's PostgreSQL refuses to start without the storage credentials), and Phala Cloud
        # shows it as error, so it is accepted in any status. Whether it booted the compose is the
        # attestation's to say (attestation, then instance-id): `cvms get` reports instance_id
        # null in practice. The upgrade after the sealing proves the CVM: running, /healthz, the
        # attestation, the certificate.
        accepted='(.in_progress | not) and ((.compose_hash | '"$normal_hash"') != $previous)'
        expected_hash=""
        if [[ "${1:-}" == --unsealed ]]; then
            shift
            outcome="settle with a new compose"
        else
            accepted='.status == "running" and '$accepted outcome="run a new compose"
        fi
        if [[ "${1:-}" == --compose-hash ]]; then
            expected_hash=$(jq -ner --arg hash "${2:-}" '$hash | ascii_downcase | ltrimstr("0x")
                | select(test("^[0-9a-f]{64}$"))') || {
                echo "--compose-hash requires a SHA-256 hash" >&2
                exit 64
            }
            shift 2
            accepted+=' and ((.compose_hash | '"$normal_hash"') == $expected)'
            outcome="settle with compose hash $expected_hash"
        fi
        stage_start cvm-wait "${CVM_WAIT_SECONDS:-900}"
        while stage_remaining; do
            if cvm=$(phala cvms get "$1" --json); then
                jq -r '"status=\(.status) in_progress=\(.in_progress) compose_hash=\(.compose_hash)"' \
                    <<<"$cvm" >&2 || true
                if jq -e --arg previous "${2:-}" --arg expected "$expected_hash" "$accepted" <<<"$cvm" >/dev/null; then
                    printf '%s\n' "$cvm"
                    exit 0
                fi
            fi
            stage_sleep 15
        done
        stage_expired
        echo "::error::CVM $1 did not $outcome" >&2
        exit 1
        ;;
    attestation)
        attested=""
        stage_start attestation "${ATTESTATION_WAIT_SECONDS:-600}"
        while stage_remaining; do
            if attestation=$(phala cvms attestation "$1" --json) &&
                attested=$(jq -r '[.tcb_info.event_log[]? | select(.event == "compose-hash")
                    | .event_payload][0] // "" | '"$normal_hash" <<<"$attestation") &&
                [[ "$attested" == "$2" ]]; then
                printf '%s\n' "$attestation"
                exit 0
            fi
            stage_sleep 15
        done
        stage_expired
        echo "::error::the attestation reports compose hash '${attested:-none}', not the deployed $2" >&2
        exit 1
        ;;
    instance-id)
        jq -er '[.tcb_info.event_log[]? | select(.event == "instance-id") | .event_payload | ascii_downcase
            | select(test("^[0-9a-f]{40}$"))] | select(length == 1)[0]' "$1" ||
            { echo "::error::the attestation's event log names no single instance id" >&2; exit 1; }
        ;;
    healthz)
        stage_start healthz "${HEALTH_WAIT_SECONDS:-600}"
        while stage_remaining; do
            stage_call 10 curl -fsS --connect-timeout 5 --max-time 10 "$1/healthz" >/dev/null && exit 0
            stage_sleep 10
        done
        stage_expired
        echo "::error::$1/healthz did not answer" >&2
        exit 1
        ;;
    *)
        echo "usage: $0 get|url|gateway-domain|deploy|compose-update|os-image|wait|attestation|instance-id|healthz ARGS..." >&2
        exit 64
        ;;
esac
