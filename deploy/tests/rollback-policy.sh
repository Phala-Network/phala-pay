#!/usr/bin/env bash
set -euo pipefail
root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d "$root/.rollback-policy.XXXXXX")
trap 'rm -r "$tmp"' EXIT
notes="$tmp/notes"
policy="$root/deploy/rollback-policy.sh"
printf '%s\n' '### Breaking (operators)' '- no rollback to 0.8.3; restore required' >"$notes"
[[ $("$policy" v0.8.3 "$notes") == bootstrap ]]
if "$policy" v0.8.4 "$notes"; then exit 1; fi
printf '%s\n' '### Fixed' '- no rollback to 0.8.3; restore required' >"$notes"
if "$policy" v0.8.3 "$notes"; then exit 1; fi
: >"$notes"
if "$policy" v0.8.3 "$notes"; then exit 1; fi
[[ $("$policy" v0.9.0 "$notes") == rollback ]]
[[ $("$policy" v0.10.0 "$notes") == rollback ]]
[[ $("$policy" v1.0.0 "$notes") == rollback ]]
echo 'rollback bootstrap requires the exact Breaking declaration; protocol N-1 runs the smoke'
