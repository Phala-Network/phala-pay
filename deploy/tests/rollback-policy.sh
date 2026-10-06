#!/usr/bin/env bash
set -euo pipefail
root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d "$root/.rollback-policy.XXXXXX")
trap 'rm -r "$tmp"' EXIT
tmp=$(CDPATH='' cd -- "$tmp" && pwd)
notes="$tmp/notes"
policy="$root/deploy/rollback-policy.sh"

expect_failure() {
    local expected_status=$1 actual_status
    shift
    if "$policy" "$@" >"$tmp/result" 2>"$tmp/error"; then
        echo "Expected rollback policy to fail with exit $expected_status: $*" >&2
        exit 1
    else
        actual_status=$?
    fi
    if [[ "$actual_status" -ne "$expected_status" ]]; then
        cat "$tmp/error" >&2
        echo "Expected exit $expected_status, got $actual_status: $*" >&2
        exit 1
    fi
}

# Immutable release tags carry two committed compatibility floors in an isolated repository.
mkdir -p "$tmp/repo/crates/topup/src/db"
cd "$tmp/repo"
git init -q
source_file=crates/topup/src/db/migrations.rs
printf '%s\n' 'const COMPATIBILITY_FLOOR: i64 = 20261028000002;' >"$source_file"
git add "$source_file"
git -c commit.gpgsign=false -c user.name=Test -c user.email=test@example.invalid commit -qm '0.9.2 compatibility floor'
git -c tag.gpgsign=false tag v0.9.2
printf '%s\n' 'const COMPATIBILITY_FLOOR: i64 = 20261029030005;' >"$source_file"
git add "$source_file"
git -c commit.gpgsign=false -c user.name=Test -c user.email=test@example.invalid commit -qm '0.10.0 raised compatibility floor'
git -c tag.gpgsign=false tag v0.10.0

: >"$notes"
[[ $("$policy" v0.9.0 "$notes") == rollback ]]
[[ $("$policy" v0.9.2 "$notes") == rollback ]]
[[ $("$policy" v0.10.0 "$notes") == rollback ]]

# A release PR has empty Unreleased notes and the declaration in its dated version section.
cat >"$notes" <<'NOTES'
## [Unreleased]

## [0.10.0] - 2026-10-06

### Breaking (operators)

- no rollback to 0.9.2; restore required
NOTES
[[ $("$policy" v0.9.2 "$notes" 0.10.0) == declared ]]

# A dated declaration cannot authorize restore when the floor is unchanged or lower.
git show "v0.9.2:$source_file" >"$source_file"
expect_failure 1 v0.9.2 "$notes" 0.10.0
grep -Fq 'Declared restore requires COMPATIBILITY_FLOOR above 20261028000002' "$tmp/error"
printf '%s\n' 'const COMPATIBILITY_FLOOR: i64 = 20261028000001;' >"$source_file"
expect_failure 1 v0.9.2 "$notes" 0.10.0
grep -Fq 'Declared restore requires COMPATIBILITY_FLOOR above 20261028000002' "$tmp/error"
git show "v0.10.0:$source_file" >"$source_file"

# The same declaration in an older release must not authorize the current release.
cat >"$notes" <<'NOTES'
## [Unreleased]

## [0.10.0] - 2026-10-06

## [0.9.2] - 2026-10-05

### Breaking (operators)

- no rollback to 0.9.2; restore required
NOTES
[[ $("$policy" v0.9.2 "$notes" 0.10.0) == rollback ]]

# The matching dated section's Fixed notes are not an operator declaration.
cat >"$notes" <<'NOTES'
## [Unreleased]

## [0.10.0] - 2026-10-06

### Fixed

- no rollback to 0.9.2; restore required
NOTES
[[ $("$policy" v0.9.2 "$notes" 0.10.0) == rollback ]]

# N-1 below the supported minimum is rejected even with an operator declaration.
printf '%s\n' '### Breaking (operators)' '- no rollback to 0.8.9; restore required' >"$notes"
expect_failure 64 v0.8.9 "$notes" 0.10.0

# Unreleased declarations remain valid before the release PR moves them to a dated section.
cat >"$notes" <<'NOTES'
## [Unreleased]

### Breaking (operators)

- no rollback to 0.9.2; restore required

## [0.9.2] - 2026-10-05
NOTES
[[ $("$policy" v0.9.2 "$notes" 0.10.0) == declared ]]

# A declaration for a different version does not suppress the real rollback smoke.
[[ $("$policy" v0.10.0 "$notes") == rollback ]]
printf '%s\n' '### Fixed' '- no rollback to 0.9.2; restore required' >"$notes"
[[ $("$policy" v0.9.2 "$notes") == rollback ]]
printf '%s\n' '### Breaking (operators)' '- no rollback to 0.9.2; restore required' >"$notes"
[[ $("$policy" v0.9.2 "$notes") == declared ]]
git show "v0.9.2:$source_file" >"$source_file"
expect_failure 1 v0.9.2 "$notes"
git show "v0.10.0:$source_file" >"$source_file"
printf '%s\n' 'invalid floor' >"$source_file"
expect_failure 1 v0.9.2 "$notes"
echo 'declared restore requires exact operator notes and a raised floor for protocol N-1'
