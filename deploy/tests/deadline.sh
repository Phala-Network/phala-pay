#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/../deadline.sh"
stage_start blocked-call 1
started=$SECONDS
if stage_call 30 bash -c 'sleep 20'; then
    echo 'blocked call escaped deadline' >&2; exit 1
else
    [[ $? == 124 ]] || exit 1
fi
((SECONDS - started <= 3))
if stage_remaining; then exit 1; fi
if stage_call 10 true; then exit 1; fi
echo 'absolute deadline and blocked-call timeout passed'

# A silent blocked pull must report expiry, not lose diagnostics through pipefail.
root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d "$root/.deadline-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
mkdir "$tmp/bin"
printf '#!/bin/sh\nexec /bin/sleep 20\n' >"$tmp/bin/docker"
chmod +x "$tmp/bin/docker"
printf '%s\n' example-image >"$tmp/images"
export PATH="$tmp/bin:$PATH" DOCKER_HOST=unix:///stub PULL_STAGE_SECONDS=1
fail() { echo "FAIL: $*"; }
ok() { echo "ok: $*"; }
source "$root/deploy/preflight-phala.sh"
if check_anonymous_pulls "$tmp/images" >"$tmp/out" 2>"$tmp/err"; then exit 1; fi
grep -q 'exceeded its deadline' "$tmp/out"
[[ $(tool_error "$tmp/pull.err") == *call_status=124* ]]
echo 'silent anonymous pull expires with preserved diagnostics'
