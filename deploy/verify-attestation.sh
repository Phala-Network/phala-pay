#!/bin/sh
set -eu

# Verifies a deployed CVM with the official dstack verifier (dstack-verifier.sh): the TDX quote
# and TCB, the RTMR3 event-log replay, and the OS image measurements. The verified OS image hash
# must match EXPECTED_OS_IMAGE_HASH, pinned in the reviewed environment directory. The replayed app id must be
# APP_ID and the replayed compose hash the SHA-256 of the attested app-compose (the full
# app-compose JSON the Phala CLI built, not the compose file), whose docker_compose_file must be
# EXPECTED_COMPOSE byte for byte. Then the compose's own policy for VARIANT (`service`,
# `restore-check`, or `template` for topup, `product` for the reference product):
# deploy/compose-policy.jq, and allowed_envs only the compose's `${NAME:-}` names. The app-compose's
# pre_launch_script, which the guest sources before `docker compose up`, must be byte for byte the
# reviewed deploy/phala-cloud-pre-launch.sh that Deploy sends (`--pre-launch-script`). A sealed name
# missing from allowed_envs is unset; the template's DSTACK_APP_DOMAIN is not an allowed env: that
# script exports it from the app id and the gateway domain.
#
# ATTESTATION_JSON is `phala cvms attestation --json` (the app certificate's quote, the event log,
# and the app-compose). INFO_JSON is the guest agent's public `GET /prpc/Info` on port 8090 (the
# CVM runs with public tcbinfo): the vm_config the quote does not carry. The app-compose's binding to
# EXPECTED_COMPOSE and the pre-launch script is deploy/attested-compose.sh's, which deploy.sh uses
# too.
if [ "$#" -ne 6 ]; then
    echo "usage: verify-attestation.sh ATTESTATION_JSON INFO_JSON APP_ID EXPECTED_COMPOSE service|restore-check|template|product EXPECTED_OS_IMAGE_HASH" >&2
    exit 64
fi
case "$5" in
    service | restore-check | template | product) variant=$5 ;;
    *) echo "unknown variant $5" >&2; exit 64 ;;
esac

root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
attestation=$1
info=$2
app_id=$(printf '%s' "${3#0x}" | tr 'A-F' 'a-f')
expected_compose=$4
expected_os_hash=$6
printf '%s' "$expected_os_hash" | LC_ALL=C grep -Eq '^[0-9a-f]{64}$' || {
    echo "EXPECTED_OS_IMAGE_HASH must be 64 lowercase hexadecimal characters" >&2
    exit 64
}
tmp=$(mktemp -d)

cleanup() {
    find "$tmp" -depth -delete
}
trap cleanup EXIT INT TERM

jq -e --slurpfile info "$info" '{
    attestation: null,
    quote: [.app_certificates[] | select(.position_in_chain == 0) | .quote][0],
    event_log: (.tcb_info.event_log | tojson),
    vm_config: $info[0].vm_config
} | select((.quote | type) == "string" and (.vm_config | type) == "string" and .vm_config != "")' \
    "$attestation" >"$tmp/request.json" || {
    echo "the attestation has no app certificate quote, or the guest agent info no vm_config" >&2
    exit 1
}
"$root/deploy/dstack-verifier.sh" <"$tmp/request.json" >"$tmp/result.json"

compose_hash=$("$root/deploy/attested-compose.sh" "$attestation" "$expected_compose" "$tmp")
jq -e --arg app_id "$app_id" --arg compose_hash "$compose_hash" --arg os_hash "$expected_os_hash" '
    .is_valid == true
    and .details.tcb_status == "UpToDate"
    and .details.app_info.app_id == $app_id
    and .details.app_info.compose_hash == $compose_hash
    and .details.app_info.os_image_hash == $os_hash
' "$tmp/result.json" >/dev/null || {
    echo "the verified attestation does not match: expected TCB UpToDate, app id $app_id, compose hash $compose_hash, OS image hash $expected_os_hash" >&2
    jq '.details | {tcb_status, advisory_ids, app_id: .app_info.app_id, compose_hash: .app_info.compose_hash, os_image_hash: .app_info.os_image_hash}' \
        "$tmp/result.json" >&2
    exit 1
}
jq -r '"dstack verifier: quote and TCB \(.details.tcb_status), RTMR3 event log, OS image \(.details.app_info.os_image_hash)",
    "attested app id: \(.details.app_info.app_id)",
    "attested compose hash: 0x\(.details.app_info.compose_hash)"' "$tmp/result.json"

compose=$("$root/deploy/pinned-compose.sh")
"$compose" -f "$tmp/docker-compose.yml" config --no-interpolate --format json >"$tmp/docker-compose.json"
# Every setting is attested: the env may set only the compose's sealed names.
violations=$(jq -r -L "$root/deploy" --arg variant "$variant" --slurpfile app "$tmp/app-compose.json" \
    'include "compose-policy"; (violations($variant; "dstack") + allowed_envs_violations($variant; $app[0].allowed_envs))[]' \
    "$tmp/docker-compose.json")
if [ -n "$violations" ]; then
    echo "the attested $variant compose breaks deploy/compose-policy.jq:" >&2
    printf '%s\n' "$violations" | sed 's/^/  /' >&2
    exit 1
fi
echo "attested compose, allowed_envs, and the $variant policy passed"
