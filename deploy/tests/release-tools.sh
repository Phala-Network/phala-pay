#!/usr/bin/env bash
# Deploy, deploy.sh, and the self-hosting guide fetch verify-release.sh outside the kit, so each
# must also fetch every script it sources; a missing one fails release verification at deploy.
set -euo pipefail
root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
sourced=$(sed -n 's#^source "$(dirname -- "$0")/\([^"]*\)"$#\1#p' "$root/deploy/verify-release.sh")
[[ -n "$sourced" ]] || { echo "no sourced scripts found in verify-release.sh; update this check" >&2; exit 1; }
for consumer in .github/workflows/deploy.yml deploy/deploy.sh docs/self-hosting.md; do
    tools=$(grep -E '^\s*for tool in verify-release\.sh' "$root/$consumer" | sed 's/.*for tool in//; s/;.*//' || true)
    for script in $sourced; do
        [[ " $tools " == *" $script "* ]] ||
            { echo "$consumer does not fetch $script, which verify-release.sh sources" >&2; exit 1; }
    done
    fetch=$(sed -n '/^[[:space:]]*for tool in verify-release\.sh/,/^[[:space:]]*done/p' "$root/$consumer")
    grep -Fq '/contents/deploy/$tool?ref=' <<<"$fetch" ||
        { echo "$consumer does not fetch helpers from deploy/\$tool" >&2; exit 1; }
done
echo "deploy.yml, deploy.sh, and docs/self-hosting.md fetch every script verify-release.sh sources: $sourced"
