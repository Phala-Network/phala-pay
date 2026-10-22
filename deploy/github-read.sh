#!/usr/bin/env bash
# Sourced with deadline.sh. Only GitHub reads go here; verification is never retried.
# Buffer stdout so a failed request cannot leave a partial API response or bundle path.
github_read() (
    local read_tmp read_status http_status delay retry_after attempt=1
    local read_deadline=${stage_deadline:?call stage_start before github_read}
    read_tmp=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/github-read.XXXXXX") || return 1
    trap 'rm -rf "$read_tmp"' EXIT
    case "$1 $2" in
        api\ *) set -- "$@" --method GET --include ;;
        'attestation download') ;;
        *) echo 'github-read: only API reads and attestation downloads are supported' >&2; return 64 ;;
    esac
    while stage_remaining; do
        read_status=0
        stage_call 60 gh "$@" >"$read_tmp/out" 2>"$read_tmp/err" || read_status=$?
        cat "$read_tmp/err" >&2
        if ((read_status == 0)); then
            if [[ "$1" == api ]]; then
                # gh api --include writes response headers, a blank line, then the body.
                sed '1,/^\r\{0,1\}$/d' "$read_tmp/out"
            else
                cat "$read_tmp/out"
            fi
            return 0
        fi
        http_status=$(sed -n 's/^HTTP\/[^ ]* \([0-9][0-9][0-9]\).*/\1/p' "$read_tmp/out")
        if [[ "$1" == attestation ]]; then
            # gh download exposes HTTP errors on stderr, but never runs a verifier.
            http_status=$(sed -nE 's/.*(HTTP |returned status code )([0-9]{3})([^0-9].*|$)/\2/p' "$read_tmp/err")
        fi
        echo "github-read: gh $1 $2 failed (HTTP ${http_status:-unknown}, exit $read_status, attempt $attempt/4)" >&2
        delay=$((1 << (attempt - 1)))
        if [[ "$http_status" == 429 ]]; then
            # Retry only with a server-provided delay. gh download does not expose headers.
            retry_after=$(awk 'tolower($0) ~ /^retry-after:/ {
                sub(/^[^:]*:[[:space:]]*/, ""); sub(/[[:space:]]*$/, ""); print
            }' "$read_tmp/out")
            [[ "$retry_after" =~ ^[0-9]{1,9}$ ]] || return "$read_status"
            ((10#$retry_after <= delay)) || delay=$((10#$retry_after))
        elif [[ "$http_status" =~ ^5[0-9][0-9]$ ]]; then
            :
        elif [[ -z "$http_status" ]] && { ((read_status == 124)) ||
            grep -Eq '(dial tcp|TLS handshake timeout|i/o timeout|connection reset by peer|unexpected EOF|: EOF|network is unreachable)' "$read_tmp/err"; }; then
            :
        else
            return "$read_status"
        fi
        ((attempt < 4)) || return "$read_status"
        if ((delay >= read_deadline - SECONDS)); then
            echo 'github-read: retry delay exceeds the remaining stage budget' >&2
            return "$read_status"
        fi
        echo "github-read: retrying in ${delay}s" >&2
        stage_sleep "$delay"
        attempt=$((attempt + 1))
    done
    stage_expired
    return 124
)
