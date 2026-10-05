#!/usr/bin/env bash
# Prints declared or rollback; declarations for protocol N-1 must match a raised floor.
set -euo pipefail
previous=${1:?previous stable release tag required}
notes=${2:?current changelog section required}
# With a release version, read only Unreleased and that dated section from the full changelog.
# Release PRs move the declaration before tagging; older release notes must not authorize it.
release=${3:-}
[[ -z "$release" || "$release" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$ ]] || exit 64
# 0.9.0 is the first immutable release implementing the compatibility-ledger protocol.
first_protocol_release=0.9.0
[[ "$previous" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || exit 64
version=${previous#v}
oldest=$(printf '%s\n' "$version" "$first_protocol_release" | sort -V | head -n 1)
declaration="no rollback to $version; restore required"
if awk -v release="$release" '
    BEGIN { section = (release == "") }
    /^## / {
        section = (release == "" || $0 == "## [Unreleased]" ||
            index($0, "## [" release "] - ") == 1)
        active = 0
    }
    /^### / { active = (section && $0 == "### Breaking (operators)"); next }
    active
' "$notes" |
    grep -Fq "$declaration"; then
    if [[ "$oldest" == "$first_protocol_release" ]]; then
        source=crates/topup/src/db/migrations.rs
        floor_pattern='^const COMPATIBILITY_FLOOR: i64 = ([0-9]+);$'
        current_line=$(grep '^const COMPATIBILITY_FLOOR:' "$source")
        previous_line=$(git show "$previous:$source" | grep '^const COMPATIBILITY_FLOOR:')
        [[ "$current_line" =~ $floor_pattern ]] || { echo 'Invalid current compatibility floor' >&2; exit 1; }
        current_floor=${BASH_REMATCH[1]}
        [[ "$previous_line" =~ $floor_pattern ]] || { echo 'Invalid N-1 compatibility floor' >&2; exit 1; }
        previous_floor=${BASH_REMATCH[1]}
        if ((current_floor <= previous_floor)); then
            echo "Declared restore requires COMPATIBILITY_FLOOR above $previous_floor (N-1 $previous)" >&2
            exit 1
        fi
    fi
    echo declared
elif [[ "$oldest" != "$first_protocol_release" ]]; then
    echo "Legacy N-1 $previous requires a Breaking (operators) declaration: $declaration" >&2
    exit 1
else
    echo rollback
fi
