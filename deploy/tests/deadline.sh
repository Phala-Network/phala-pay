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
