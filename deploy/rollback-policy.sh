#!/usr/bin/env bash
# Prints the rollback mode; legacy N-1 requires an explicit operator declaration.
set -euo pipefail
previous=${1:?previous stable release tag required}
notes=${2:?current changelog section required}
# 0.9.0 is the first immutable release implementing the compatibility-ledger protocol.
first_protocol_release=0.9.0
[[ "$previous" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || exit 64
version=${previous#v}
oldest=$(printf '%s\n' "$version" "$first_protocol_release" | sort -V | head -n 1)
if [[ "$oldest" != "$first_protocol_release" ]]; then
    declaration="no rollback to $version; restore required"
    if ! awk '/^### / { active = ($0 == "### Breaking (operators)"); next } active' "$notes" |
        grep -Fq "$declaration"; then
        echo "Legacy N-1 $previous requires a Breaking (operators) declaration: $declaration" >&2
        exit 1
    fi
    echo bootstrap
else
    echo rollback
fi
