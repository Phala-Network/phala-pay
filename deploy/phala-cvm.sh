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
#   deploy/phala-cvm.sh wait [--unsealed] CVM_ID [PREVIOUS_HASH] >cvm.json
#                                                    until the CVM runs, settled, with a compose hash
#                                                    other than PREVIOUS_HASH (15 minutes); with
#                                                    --unsealed, settled in any status
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
        STAGE_DIAGNOSTICS=0 phala deploy --json "$@" >"$output.raw" 2> >(tee "$output.stderr" >&2)
        # shellcheck disable=SC2002
        echo "stage=deploy elapsed=$((SECONDS - stage_started))s remaining=$((stage_deadline - SECONDS))s call_status=0" >&2
        sed -n '/^{/,$p' "$output.raw" | grep -v '^stage=' >"$output"
        jq -e '.success == true' "$output" >/dev/null
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
        if [[ "${1:-}" == --unsealed ]]; then
            shift
            outcome="settle with a new compose"
        else
            accepted='.status == "running" and '$accepted outcome="run a new compose"
        fi
        stage_start cvm-wait "${CVM_WAIT_SECONDS:-900}"
        while stage_remaining; do
            if cvm=$(phala cvms get "$1" --json); then
                jq -r '"status=\(.status) in_progress=\(.in_progress) compose_hash=\(.compose_hash)"' \
                    <<<"$cvm" >&2 || true
                if jq -e --arg previous "${2:-}" "$accepted" <<<"$cvm" >/dev/null; then
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
        echo "usage: $0 get|url|gateway-domain|deploy|wait|attestation|instance-id|healthz ARGS..." >&2
        exit 64
        ;;
esac
