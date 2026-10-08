#!/usr/bin/env bash
# Verify the OS pin against the official verifier's result, never the unverified guest info.
set -euo pipefail
root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
tmp=$(mktemp -d "$root/.attestation-tests.XXXXXX")
trap 'rm -rf "$tmp"' EXIT INT TERM
mkdir -p "$tmp/kit/deploy"
cp "$root/deploy/verify-attestation.sh" "$tmp/kit/deploy/"
hash=$(cat "$root/deploy/environments/phala-network/production/topup/os-image-hash")
cmp "$root/deploy/environments/phala-network/staging/topup/os-image-hash" \
    "$root/deploy/environments/phala-network/production/topup/os-image-hash"
export VERIFIED_RESULT="$tmp/result.json" FIXTURE_DIR="$tmp"
cat >"$tmp/kit/deploy/dstack-verifier.sh" <<'SH'
#!/bin/sh
cat >/dev/null
cat "$VERIFIED_RESULT"
SH
cat >"$tmp/kit/deploy/attested-compose.sh" <<'SH'
#!/bin/sh
printf '{}\n' >"$3/app-compose.json"
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
printf 'def violations($variant; $runtime): []; def allowed_envs_violations($variant; $env): [];\n' \
    >"$tmp/kit/deploy/compose-policy.jq"
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
        "$tmp/expected.yml" service "$hash" >"$tmp/out" 2>"$tmp/err"
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
