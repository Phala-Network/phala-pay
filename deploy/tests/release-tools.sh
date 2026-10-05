#!/usr/bin/env bash
# Deploy (deploy.yml) and deploy.sh fetch verify-release.sh on its own, outside the kit, so each
# must also fetch every script it sources; a missing one fails release verification at deploy.
set -euo pipefail
root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
sourced=$(sed -n 's#^source "$(dirname -- "$0")/\([^"]*\)"$#\1#p' "$root/deploy/verify-release.sh")
[[ -n "$sourced" ]] || { echo "no sourced scripts found in verify-release.sh; update this check" >&2; exit 1; }
for consumer in .github/workflows/deploy.yml deploy/deploy.sh; do
    tools=$(grep -E '^\s*for tool in verify-release\.sh' "$root/$consumer" | sed 's/.*for tool in//; s/;.*//')
    for script in $sourced; do
        [[ " $tools " == *" $script "* ]] ||
            { echo "$consumer does not fetch $script, which verify-release.sh sources" >&2; exit 1; }
    done
done
echo "deploy.yml and deploy.sh fetch every script verify-release.sh sources: $sourced"
