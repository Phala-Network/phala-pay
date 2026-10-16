#!/usr/bin/env bash
# Verify the OS pin against the official verifier's result, never the unverified guest info.
set -euo pipefail
root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/attestation-tests.XXXXXX")
trap 'rm -rf "$tmp"' EXIT INT TERM
mkdir -p "$tmp/kit/deploy"
cp "$root/deploy/verify-attestation.sh" "$tmp/kit/deploy/"
hash=$(cat "$root/deploy/environments/phala-network/production/topup/os-image-hash")
cmp "$root/deploy/environments/phala-network/staging/topup/os-image-hash" \
    "$root/deploy/environments/phala-network/production/topup/os-image-hash"
export FIXTURE_PORT_POLICY='{"ports":[{"port":443,"pp":true}],"restrict_mode":false}'
export VERIFIED_RESULT="$tmp/result.json" FIXTURE_DIR="$tmp"
cat >"$tmp/kit/deploy/dstack-verifier.sh" <<'SH'
#!/bin/sh
cat >/dev/null
cat "$VERIFIED_RESULT"
SH
cat >"$tmp/kit/deploy/attested-compose.sh" <<'SH'
#!/bin/sh
jq -n --argjson policy "$FIXTURE_PORT_POLICY" '{port_policy: $policy}' >"$3/app-compose.json"
printf 'services: {}\n' >"$3/docker-compose.yml"
printf 'fixture-compose-hash\n'
SH
cat >"$tmp/kit/deploy/pinned-compose.sh" <<'SH'
#!/bin/sh
printf '%s/compose\n' "$FIXTURE_DIR"
SH
cat >"$tmp/compose" <<'SH'
#!/bin/sh
printf '{}\n'
SH
chmod +x "$tmp/kit/deploy/"*.sh "$tmp/compose"
# Compose policy has its own behavioral tests; this fixture isolates verified measurement pins.
cp "$root/deploy/compose-policy.jq" "$tmp/kit/deploy/compose-policy.jq"
printf '\ndef violations($variant; $runtime): []; def allowed_envs_violations($variant; $env): [];\n' \
    >>"$tmp/kit/deploy/compose-policy.jq"
jq -n '{app_certificates: [{position_in_chain: 0, quote: "fixture"}], tcb_info: {event_log: []}}' \
    >"$tmp/attestation.json"
jq -n --arg hash "$hash" '{vm_config: ({os_image_hash: $hash} | tojson)}' >"$tmp/info.json"
printf 'services: {}\n' >"$tmp/expected.yml"
result() {
    jq -n --arg hash "$1" '{is_valid: true, details: {tcb_status: "UpToDate", app_info: {
        app_id: "app", compose_hash: "fixture-compose-hash", os_image_hash: $hash}}}' >"$VERIFIED_RESULT"
}
verify() {
    "$tmp/kit/deploy/verify-attestation.sh" "$tmp/attestation.json" "$tmp/info.json" app \
        "$tmp/expected.yml" "${variant:-service}" "$hash" >"$tmp/out" 2>"$tmp/err"
}
refused() {
    if verify; then echo "verify-attestation: $1 was accepted" >&2; exit 1; fi
    grep -qF 'the verified attestation does not match' "$tmp/err"
}
result "$hash"
verify
result "$(printf '0%.0s' {1..64})"
refused 'a different verified OS hash, even with the approved hash in guest info'
result "$hash"
jq 'del(.details.app_info.os_image_hash)' "$VERIFIED_RESULT" >"$tmp/missing.json"
cp "$tmp/missing.json" "$VERIFIED_RESULT"
refused 'a missing verified OS hash'
result "$hash"
jq '.is_valid = false' "$VERIFIED_RESULT" >"$tmp/invalid.json"
cp "$tmp/invalid.json" "$VERIFIED_RESULT"
refused 'an invalid verifier result'
result "$hash"
if "$tmp/kit/deploy/verify-attestation.sh" "$tmp/attestation.json" "$tmp/info.json" app \
    "$tmp/expected.yml" service invalid >"$tmp/out" 2>"$tmp/err"; then
    echo 'verify-attestation: a malformed approved pin was accepted' >&2; exit 1
fi
grep -qF 'EXPECTED_OS_IMAGE_HASH must be 64' "$tmp/err"
echo 'attestation OS pin tests passed (matching, mismatch, missing, invalid result and malformed pin)'

result "$hash"
for variant in service restore-check template product; do
    case "$variant" in service | restore-check) proxy=true ;; *) proxy=false ;; esac
    FIXTURE_PORT_POLICY=$(jq -cn --argjson proxy "$proxy" \
        '{ports:[{port:443,pp:$proxy}],restrict_mode:false}')
    verify
    saved_policy=$FIXTURE_PORT_POLICY
    for FIXTURE_PORT_POLICY in null \
        "$(jq '.ports[0].pp |= not' <<<"$saved_policy")" \
        "$(jq '.restrict_mode = true' <<<"$saved_policy")" \
        "$(jq '.ports[0].port = 80' <<<"$saved_policy")" \
        "$(jq '.ports += [{port:8081,pp:true}]' <<<"$saved_policy")" \
        "$(jq '.ports[0].pp |= tostring' <<<"$saved_policy")" \
        "$(jq 'del(.restrict_mode)' <<<"$saved_policy")"; do
        ! verify || { echo "verify-attestation: accepted a foreign $variant port policy" >&2; exit 1; }
        grep -qF 'the attested port_policy must match' "$tmp/err"
    done
    FIXTURE_PORT_POLICY=$saved_policy
done
echo 'attested port-policy checks passed for every variant'

# Exercise the real render, attested-compose binding, and complete policy with the backend's
# instances-add contract: copy the source app-compose, replacing only compose and pre-launch.
# Only the external quote verifier is stubbed; all inherited fields remain in the measured hash.
cp "$root/deploy/"{attested-compose.sh,pinned-compose.sh,phala-cloud-pre-launch.sh,compose-policy.jq} "$tmp/kit/deploy/"
jq -n '{"phala-pay":"ghcr.io/phala-network/phala-pay@sha256:\("1" * 64)",
    "postgres-walg":"ghcr.io/phala-network/postgres-walg@sha256:\("2" * 64)"}' >"$tmp/images.json"
variant=restore-check
for environment in staging production; do
    env_dir="$root/deploy/environments/phala-network/$environment/topup"
    "$root/deploy/render.sh" --images "$tmp/images.json" --gateway-domain gateway.example.net \
        "$env_dir" >"$tmp/service.yml"
    "$root/deploy/render.sh" --restore-check --images "$tmp/images.json" --origin https://restore.example.net \
        "$env_dir" >"$tmp/expected.yml"
    compose=$("$root/deploy/pinned-compose.sh")
    "$compose" -f "$tmp/service.yml" config --no-interpolate --format json |
        jq -L "$root/deploy" --rawfile compose "$tmp/service.yml" \
            --rawfile pre_launch "$root/deploy/phala-cloud-pre-launch.sh" '
            include "compose-policy";
            {manifest_version:2, name:"source-service", runner:"docker-compose",
             docker_compose_file:$compose, pre_launch_script:$pre_launch, allowed_envs:sealed_names,
             port_policy:{ports:[{port:443,pp:true}],restrict_mode:false},
             gateway_enabled:true, tproxy_enabled:true, kms_enabled:true,
             public_logs:false, public_sysinfo:false, public_tcbinfo:true, secure_time:true,
             features:["kms","tproxy-net"], storage_fs:"ext4"}
        ' >"$tmp/source-app.json"
    jq -cj --rawfile compose "$tmp/expected.yml" \
        --rawfile pre_launch "$root/deploy/phala-cloud-pre-launch.sh" \
        '.docker_compose_file = $compose | .pre_launch_script = $pre_launch' \
        "$tmp/source-app.json" >"$tmp/inherited-app.json"
    inherited_hash=$(sha256sum "$tmp/inherited-app.json" | cut -d' ' -f1)
    jq -n --rawfile app "$tmp/inherited-app.json" --arg hash "$inherited_hash" '{
        compose_file:$app, app_certificates:[{position_in_chain:0,quote:"fixture"}],
        tcb_info:{event_log:[{event:"compose-hash",event_payload:$hash}]}}
    ' >"$tmp/attestation.json"
    result "$hash"
    jq --arg hash "$inherited_hash" '.details.app_info.compose_hash = $hash' \
        "$VERIFIED_RESULT" >"$tmp/inherited-result.json"
    cp "$tmp/inherited-result.json" "$VERIFIED_RESULT"
    verify || { cat "$tmp/err" >&2; exit 1; }
done
echo 'inherited service app-compose passes the full restore-check policy for staging and production'
