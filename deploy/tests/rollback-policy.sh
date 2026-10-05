#!/usr/bin/env bash
set -euo pipefail
root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d "$root/.rollback-policy.XXXXXX")
trap 'rm -r "$tmp"' EXIT
tmp=$(CDPATH='' cd -- "$tmp" && pwd)
notes="$tmp/notes"
policy="$root/deploy/rollback-policy.sh"
printf '%s\n' '### Breaking (operators)' '- no rollback to 0.8.3; restore required' >"$notes"
[[ $("$policy" v0.8.3 "$notes") == declared ]]
if "$policy" v0.8.4 "$notes"; then exit 1; fi
printf '%s\n' '### Fixed' '- no rollback to 0.8.3; restore required' >"$notes"
if "$policy" v0.8.3 "$notes"; then exit 1; fi
: >"$notes"
if "$policy" v0.8.3 "$notes"; then exit 1; fi
[[ $("$policy" v0.9.0 "$notes") == rollback ]]
[[ $("$policy" v0.10.0 "$notes") == rollback ]]
[[ $("$policy" v1.0.0 "$notes") == rollback ]]
# A release PR has empty Unreleased notes and the declaration in its dated version section.
cat >"$notes" <<'NOTES'
## [Unreleased]

## [0.9.0] - 2026-10-05

### Breaking (operators)

- 0.9.0: **no rollback to 0.8.3; restore required**.
NOTES
[[ $("$policy" v0.8.3 "$notes" 0.9.0) == declared ]]
# The same declaration in an older release must not authorize the current release.
if "$policy" v0.8.3 "$notes" 0.10.0; then exit 1; fi
sed -i 's/### Breaking (operators)/### Fixed/' "$notes"
if "$policy" v0.8.3 "$notes" 0.9.0; then exit 1; fi
cat >"$notes" <<'NOTES'
## [Unreleased]

### Breaking (operators)

- no rollback to 0.8.3; restore required

## [0.8.3] - 2026-10-04
NOTES
[[ $("$policy" v0.8.3 "$notes" 0.9.0) == declared ]]
# A tiny repository fixture exercises comparison against immutable N-1 tags.
mkdir -p "$tmp/repo/crates/topup/src/db"
cd "$tmp/repo"
git init -q
source_file=crates/topup/src/db/migrations.rs
printf '%s\n' 'const COMPATIBILITY_FLOOR: i64 = 20261028000002;' >"$source_file"
git add "$source_file"
git -c commit.gpgsign=false -c user.name=Test -c user.email=test@example.invalid commit -qm 'protocol floor'
git -c tag.gpgsign=false tag v0.9.0
printf '%s\n' '### Breaking (operators)' '- no rollback to 0.9.0; restore required' >"$notes"
if "$policy" v0.9.0 "$notes"; then exit 1; fi
printf '%s\n' 'const COMPATIBILITY_FLOOR: i64 = 20261028000001;' >"$source_file"
if "$policy" v0.9.0 "$notes"; then exit 1; fi
printf '%s\n' 'const COMPATIBILITY_FLOOR: i64 = 20261029030005;' >"$source_file"
[[ $("$policy" v0.9.0 "$notes") == declared ]]
# Dated declarations still require a raised floor for protocol-era N-1.
cat >"$notes" <<'NOTES'
## [Unreleased]

## [0.10.0] - 2026-10-06

### Breaking (operators)

- no rollback to 0.9.0; restore required
NOTES
[[ $("$policy" v0.9.0 "$notes" 0.10.0) == declared ]]
printf '%s\n' 'const COMPATIBILITY_FLOOR: i64 = 20261028000002;' >"$source_file"
if "$policy" v0.9.0 "$notes" 0.10.0; then exit 1; fi
printf '%s\n' 'const COMPATIBILITY_FLOOR: i64 = 20261029030005;' >"$source_file"
# A declaration for a different version does not suppress the real rollback smoke.
[[ $("$policy" v0.10.0 "$notes") == rollback ]]
printf '%s\n' '### Fixed' '- no rollback to 0.9.0; restore required' >"$notes"
[[ $("$policy" v0.9.0 "$notes") == rollback ]]
printf '%s\n' '### Breaking (operators)' '- no rollback to 0.9.0; restore required' >"$notes"
printf '%s\n' 'invalid floor' >"$source_file"
if "$policy" v0.9.0 "$notes"; then exit 1; fi
echo 'declared restore requires exact operator notes and a raised floor for protocol N-1'
