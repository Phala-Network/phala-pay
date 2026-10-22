#!/usr/bin/env bash
# Retry only read failures; preserve stdout, HTTP diagnostics, backoff and the stage deadline.
set -euo pipefail
root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir "$tmp/bin"
cat >"$tmp/bin/gh" <<'STUB'
#!/usr/bin/env bash
count=$(cat "$STUB_TMP/count")
echo "$((count + 1))" >"$STUB_TMP/count"
if ((count >= STUB_FAILURES)); then
    [[ "$1" != api ]] || printf 'HTTP/2 200 OK\r\nContent-Type: application/json\r\n\r\n'
    echo success
    exit 0
fi
if [[ -n "$STUB_HTTP" ]]; then
    if [[ "$1" == api ]]; then
        printf 'HTTP/2 %s Error\r\n' "$STUB_HTTP"
        [[ -z "$STUB_RETRY_AFTER" ]] || printf 'retry-after: %s\r\n' "$STUB_RETRY_AFTER"
        printf '\r\npartial response\n'
    fi
    if [[ "$1" == attestation ]]; then
        echo "Failed to download the artifact's bundle(s): failed to fetch attestations: HTTP $STUB_HTTP: Server Error" >&2
    else
        echo "gh: HTTP $STUB_HTTP: Server Error" >&2
    fi
else
    echo partial
    echo "$STUB_ERROR" >&2
fi
exit 1
STUB
cat >"$tmp/bin/sleep" <<'STUB'
#!/usr/bin/env bash
echo "$1" >>"$STUB_TMP/sleep"
STUB
chmod +x "$tmp/bin/gh" "$tmp/bin/sleep"
export PATH="$tmp/bin:$PATH" STUB_TMP="$tmp" RUNNER_TEMP="$tmp"
source "$root/deploy/deadline.sh"
source "$root/deploy/github-read.sh"
run() {
    echo 0 >"$tmp/count"
    : >"$tmp/sleep"
    stage_start test-read "${STUB_BUDGET:-600}"
    github_read "$@" >"$tmp/out" 2>"$tmp/err"
}
fail() { echo "github-read: $*" >&2; exit 1; }
export STUB_FAILURES=1 STUB_HTTP=502 STUB_RETRY_AFTER='' STUB_ERROR=''
run api repos/example/repo/releases || fail 'did not recover from HTTP 502'
[[ $(cat "$tmp/out") == success && $(cat "$tmp/count") == 2 && $(cat "$tmp/sleep") == 1 ]] ||
    fail 'wrong API response or backoff after HTTP 502'
grep -q 'HTTP 502' "$tmp/err" || fail 'lost HTTP error diagnostic'
run attestation download artifact || fail 'did not recover an attestation fetch'
[[ $(cat "$tmp/out") == success && $(cat "$tmp/count") == 2 ]] || fail 'wrong download response'

STUB_HTTP='' STUB_ERROR='Get "https://api.github.com/repos/example/repo": dial tcp: i/o timeout' \
    run api repos/example/repo || fail 'did not retry a transport failure'
[[ $(cat "$tmp/count") == 2 ]] || fail 'wrong transport retry count'
! STUB_HTTP='' STUB_ERROR='failed to digest artifact: dial tcp: i/o timeout' \
    run attestation download artifact || fail 'retried a registry digest failure'
[[ $(cat "$tmp/count") == 1 ]] || fail 'retried outside the GitHub fetch'
STUB_HTTP='' STUB_ERROR='failed to fetch attestations: Get "https://api.github.com": unexpected EOF' \
    run attestation download artifact || fail 'did not retry attestation API transport failure'
[[ $(cat "$tmp/count") == 2 ]] || fail 'wrong attestation API retry count'
STUB_HTTP=429 STUB_RETRY_AFTER=08 run api repos/example/repo || fail 'did not recover from 429 with Retry-After'
[[ $(cat "$tmp/sleep") == 8 ]] || fail 'ignored Retry-After'
! STUB_HTTP=429 run api repos/example/repo || fail 'retried 429 without Retry-After'
[[ $(cat "$tmp/count") == 1 ]] || fail 'retried 429 without a delay'
! STUB_HTTP=429 STUB_RETRY_AFTER=2 STUB_BUDGET=1 run api repos/example/repo || fail 'exceeded retry budget'
[[ $(cat "$tmp/count") == 1 && ! -s "$tmp/sleep" ]] || fail 'slept beyond the deadline'
grep -q 'remaining stage budget' "$tmp/err" || fail 'budget failure has no diagnostic'

for code in 401 403 404 422; do
    ! STUB_HTTP=$code run api repos/example/repo || fail "accepted HTTP $code"
    [[ $(cat "$tmp/count") == 1 && ! -s "$tmp/out" ]] || fail "retried or emitted a partial HTTP $code response"
done
! STUB_HTTP='' STUB_ERROR='invalid JSON response' run api repos/example/repo || fail 'accepted invalid data'
[[ $(cat "$tmp/count") == 1 ]] || fail 'retried invalid data'
! STUB_HTTP='' STUB_ERROR='' run api repos/example/repo || fail 'accepted silent CLI failure'
grep -q 'exit 1' "$tmp/err" || fail 'silent CLI failure still has no diagnostic'
! STUB_FAILURES=10 run api repos/example/repo || fail 'ignored attempt limit'
[[ $(cat "$tmp/count") == 4 && $(cat "$tmp/sleep") == $'1\n2\n4' ]] || fail 'wrong retry bound or backoff'
! run attestation verify artifact || fail 'allowed a verifier into the retry helper'
[[ $(cat "$tmp/count") == 0 ]] || fail 'invoked a verifier'
[[ -z $(find "$tmp" -type d -name 'github-read.*' -print) ]] || fail 'left request buffers'
echo 'GitHub read retry, diagnostic, and deadline tests passed'
