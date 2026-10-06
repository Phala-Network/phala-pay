#!/usr/bin/env bash
# Signs with the existing RFC 9421 mechanism with maintenance-only authority. Key material never enters arguments or the record.
set -euo pipefail
[[ $# -eq 3 && "$1" =~ ^(pause|resume)$ ]] || {
    echo "usage: $0 pause|resume PUBLIC_URL OWNER" >&2; exit 64;
}
: "${TOPUP_MAINTENANCE_PRIVATE_KEY_PEM:?set the Environment maintenance signing key}"
: "${TOPUP_MAINTENANCE_KEY_ID:?set the attested maintenance key id}"
root=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=deploy/deadline.sh
source "$root/deadline.sh"
retry_base=${INSTANCE_PAUSE_RETRY_BASE_SECONDS:-1}
retry_max=${INSTANCE_PAUSE_RETRY_MAX_SECONDS:-16}
[[ "$retry_base" =~ ^[1-9][0-9]*$ && "$retry_max" =~ ^[1-9][0-9]*$ ]] || {
    echo "instance-pause: retry delays must be positive integers" >&2
    exit 64
}
stage_start "instance-$1" "${INSTANCE_PAUSE_DEADLINE_SECONDS:-60}"
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

retryable_transport() {
    case "$1" in
        # curl: could not resolve host, connect, timeout, TLS, empty reply, send/receive failure.
        6|7|28|35|52|55|56) return 0 ;;
        *) return 1 ;;
    esac
}

retry_delay() {
    local delay=$retry_base count=$1
    while ((count > 1)); do
        if ((delay >= retry_max)); then
            delay=$retry_max
            break
        fi
        delay=$((delay * 2))
        ((count--))
    done
    ((delay <= retry_max)) || delay=$retry_max
    printf '%s\n' "$delay"
}

retry_after() {
    local value target now
    value=$(awk 'tolower($0) ~ /^retry-after:[[:space:]]*[0-9]+[[:space:]]*$/ {
            sub(/^[^:]*:[[:space:]]*/, ""); sub(/[[:space:]]*$/, ""); value=$0
        }
        END { if (value != "") print value }' "$tmp/response.headers")
    if [[ "$value" =~ ^[0-9]+$ ]]; then
        printf '%s\n' "$value"
        return 0
    fi

    value=$(awk 'tolower($0) ~ /^retry-after:/ {
            sub(/^[^:]*:[[:space:]]*/, ""); sub(/[[:space:]]*$/, ""); value=$0
        }
        END { if (value != "") print value }' "$tmp/response.headers")
    [[ -n "$value" ]] || return 1
    target=$(date -u -d "$value" +%s 2>/dev/null) ||
        target=$(date -u -j -f '%a, %d %b %Y %H:%M:%S GMT' "$value" +%s 2>/dev/null) || return 1
    now=$(date -u +%s)
    if ((target > now)); then
        printf '%s\n' "$((target - now))"
    else
        printf '0\n'
    fi
}

attempt=1
while stage_remaining; do
    # Sign every attempt: the RFC 9421 `created` parameter is fresh for each request.
    "$root/runbooks/sign-admin-request.sh" POST "$url" "$tmp/body.json" \
        "$tmp/key.pem" "$TOPUP_MAINTENANCE_KEY_ID" >"$tmp/headers"
    mapfile -t headers <"$tmp/headers"
    ((${#headers[@]} == 3)) || {
        echo "instance-pause: signing returned an unexpected number of headers" >&2
        exit 1
    }

    : >"$tmp/response.headers"
    : >"$tmp/response.body"
    : >"$tmp/status"
    curl_status=0
    if stage_call 15 curl -sS --connect-timeout 5 --max-time 15 -X POST \
        -H 'content-type: application/json' \
        -H "${headers[0]}" -H "${headers[1]}" -H "${headers[2]}" \
        -D "$tmp/response.headers" -o "$tmp/response.body" -w '%{http_code}' \
        --data-binary @"$tmp/body.json" "$url" >"$tmp/status"; then
        curl_status=0
    else
        curl_status=$?
    fi
    http_status=$(<"$tmp/status")
    [[ "$http_status" =~ ^[0-9]{3}$ ]] || http_status=000

    if [[ "$http_status" =~ ^2[0-9][0-9]$ || "$http_status" =~ ^3[0-9][0-9]$ ]]; then
        cat "$tmp/response.body"
        exit 0
    fi

    should_retry=false retry_reason=
    if retryable_transport "$curl_status" || [[ "$http_status" == 000 ]]; then
        should_retry=true
        retry_reason="transport failure (curl=$curl_status)"
    elif [[ "$http_status" =~ ^(502|503|504)$ ]]; then
        should_retry=true
        retry_reason="HTTP $http_status"
    elif [[ "$http_status" =~ ^(408|429)$ ]]; then
        should_retry=true
        retry_reason="HTTP $http_status"
    fi

    if [[ "$should_retry" != true ]]; then
        [[ -s "$tmp/response.body" ]] && cat "$tmp/response.body"
        if [[ "$curl_status" -ne 0 ]]; then
            exit "$curl_status"
        fi
        exit 22
    fi

    delay=$(retry_delay "$attempt")
    if retry_after_value=$(retry_after); then
        delay=$retry_after_value
    fi
    echo "instance-pause: retrying $1 after $retry_reason (attempt $attempt, backoff ${delay}s)" >&2
    stage_sleep "$delay"
    ((attempt++))
done

stage_expired
exit 124
