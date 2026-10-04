#!/usr/bin/env bash
# scripts/version.sh on copies of this commit's version files:
# - this commit passes and prints the Cargo workspace version;
# - a drifted sdk/js/package.json, sdk/js-react/package.json, sdk/js-server/package.json, sdk/python/pyproject.toml, sdk/python/uv.lock, or
#   deploy/deploy.sh's SDK pin fails, naming it;
# - a pre-release passes as X.Y.Z-rc.N in Cargo and npm and X.Y.ZrcN in Python, and fails as
#   another release candidate in Python;
# - setting a version that is not X.Y.Z or X.Y.Z-rc.N is refused before any file changes.
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
fail() {
    echo "version: $*" >&2
    exit 1
}
files=(Cargo.toml sdk/js/package.json sdk/js-react/package.json sdk/js-server/package.json sdk/python/pyproject.toml sdk/python/uv.lock deploy/deploy.sh)
# fixture: a fresh copy of the script and the version files, in $tmp/repo.
fixture() {
    rm -rf "$tmp/repo"
    mkdir -p "$tmp/repo/scripts" "$tmp/repo/sdk/js" "$tmp/repo/sdk/js-react" "$tmp/repo/sdk/js-server" "$tmp/repo/sdk/python" "$tmp/repo/deploy"
    cp "$root/scripts/version.sh" "$tmp/repo/scripts/"
    cp "$root/sdk/js-react/package.json" "$tmp/repo/sdk/js-react/"
    cp "$root/sdk/js-server/package.json" "$tmp/repo/sdk/js-server/"
    for file in "${files[@]}"; do cp "$root/$file" "$tmp/repo/$file"; done
}
# set_version FILE VERSION: the version as each file spells it.
set_version() {
    case "$1" in
        Cargo.toml) sed -i "/^\[workspace.package\]\$/,/^\[/s/^version = \".*\"\$/version = \"$2\"/" "$tmp/repo/$1" ;;
        *package.json) sed -i "0,/\"version\": \".*\"/s//\"version\": \"$2\"/" "$tmp/repo/$1" ;;
        *pyproject.toml) sed -i "/^\[project\]\$/,/^\[/s/^version = \".*\"\$/version = \"$2\"/" "$tmp/repo/$1" ;;
        *uv.lock) sed -i "/^name = \"phala-pay\"\$/{n;s/^version = \".*\"\$/version = \"$2\"/}" "$tmp/repo/$1" ;;
        *deploy.sh) sed -i "s/^sdk=phala-pay==.*\$/sdk=phala-pay==$2/" "$tmp/repo/$1" ;;
    esac
}

fixture
cargo_version=$(sed -n '/^\[workspace.package\]$/,/^\[/s/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml")
[[ "$("$tmp/repo/scripts/version.sh")" == "$cargo_version" ]] || fail "this commit's versions differ"

for file in "${files[@]:1}"; do
    fixture
    set_version "$file" 9.9.9
    if "$tmp/repo/scripts/version.sh" >/dev/null 2>"$tmp/err"; then fail "a drifted $file passed"; fi
    grep -q "^version.sh: $file names 9.9.9," "$tmp/err" || fail "a drifted $file was not named: $(cat "$tmp/err")"
done

fixture
set_version Cargo.toml 0.5.0-rc.1
set_version sdk/js/package.json 0.5.0-rc.1
set_version sdk/js-react/package.json 0.5.0-rc.1
set_version sdk/js-server/package.json 0.5.0-rc.1
set_version sdk/python/pyproject.toml 0.5.0rc1
set_version sdk/python/uv.lock 0.5.0rc1
set_version deploy/deploy.sh 0.5.0rc1
[[ "$("$tmp/repo/scripts/version.sh")" == 0.5.0-rc.1 ]] || fail "a pre-release failed"
set_version sdk/python/pyproject.toml 0.5.0rc2
if "$tmp/repo/scripts/version.sh" >/dev/null 2>&1; then fail "another release candidate in Python passed"; fi

fixture
status=0
"$tmp/repo/scripts/version.sh" 0.5 2>/dev/null || status=$?
((status == 64)) || fail "setting 0.5 exited $status, not 64"
for file in "${files[@]}"; do
    cmp -s "$root/$file" "$tmp/repo/$file" || fail "setting 0.5 changed $file"
done

echo "version test passed"
