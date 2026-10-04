#!/usr/bin/env bash
# Signs with the existing RFC 9421 mechanism with maintenance-only authority. Key material never enters arguments or the record.
set -euo pipefail
[[ $# -eq 3 && "$1" =~ ^(pause|resume)$ ]] || {
    echo "usage: $0 pause|resume PUBLIC_URL OWNER" >&2; exit 64;
}
: "${TOPUP_MAINTENANCE_PRIVATE_KEY_PEM:?set the Environment maintenance signing key}"
: "${TOPUP_MAINTENANCE_KEY_ID:?set the attested maintenance key id}"
root=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
chmod 700 "$tmp"
umask 077
printf '%s\n' "$TOPUP_MAINTENANCE_PRIVATE_KEY_PEM" >"$tmp/key.pem"
jq -n --arg owner "$3" --arg reason "Deploy $3: $1 instance mutations" \
    '{owner: $owner, reason: $reason, duration_seconds: 900}' >"$tmp/body.json"
url="${2%/}/v1/admin/instance/$1"
# Process substitution hides failures; use a file so signing failures stop the deployment.
"$root/runbooks/sign-admin-request.sh" POST "$url" "$tmp/body.json" \
    "$tmp/key.pem" "$TOPUP_MAINTENANCE_KEY_ID" >"$tmp/headers"
mapfile -t headers <"$tmp/headers"
# No automatic replay of a single-use signature. Workflow retries sign fresh requests.
curl --fail-with-body -sS --max-time 15 -X POST -H 'content-type: application/json' \
    -H "${headers[0]}" -H "${headers[1]}" -H "${headers[2]}" \
    --data-binary @"$tmp/body.json" "$url"
