#!/usr/bin/env bash
# scripts/version.sh on copies of this commit's version files:
# - this commit passes and prints the Cargo workspace version;
# - a drifted sdk/js/package.json, sdk/js-react/package.json, sdk/js-server/package.json, sdk/python/pyproject.toml, sdk/python/uv.lock, or
#   deploy/deploy.sh's SDK pin fails, naming it;
# - a pre-release passes as X.Y.Z-rc.N in Cargo and npm and X.Y.ZrcN in Python, and fails as
#   another release candidate in Python;
# - setting a version that is not X.Y.Z or X.Y.Z-rc.N is refused before any file changes.
# - setting a version updates all npm versions and React's exact core dependency, without adding a peer.
# - the stable and prerelease version setters leave the npm workspace's frozen install valid.
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
fail() {
    echo "version: $*" >&2
    exit 1
}
files=(Cargo.toml sdk/js/package.json sdk/js-react/package.json sdk/js-server/package.json sdk/python/pyproject.toml sdk/python/uv.lock deploy/deploy.sh)
npm_lock_files=(sdk/js/pnpm-workspace.yaml sdk/js/pnpm-lock.yaml)
# fixture: a fresh copy of the script and the version files, in $tmp/repo.
fixture() {
    local file
    rm -rf "$tmp/repo"
    mkdir -p "$tmp/repo/scripts" "$tmp/repo/sdk/js" "$tmp/repo/sdk/js-react" "$tmp/repo/sdk/js-server" "$tmp/repo/sdk/python" "$tmp/repo/deploy"
    cp "$root/scripts/version.sh" "$tmp/repo/scripts/"
    cp "$root/sdk/js-react/package.json" "$tmp/repo/sdk/js-react/"
    cp "$root/sdk/js-server/package.json" "$tmp/repo/sdk/js-server/"
    for file in "${files[@]}" "${npm_lock_files[@]}"; do cp "$root/$file" "$tmp/repo/$file"; done
}
# set_version FILE VERSION: the version as each file spells it.
set_version() {
    case "$1" in
        Cargo.toml) sed -i "/^\[workspace.package\]\$/,/^\[/s/^version = \".*\"\$/version = \"$2\"/" "$tmp/repo/$1" ;;
        *package.json)
            jq --arg version "$2" '.version = $version' "$tmp/repo/$1" >"$tmp/package.json"
            mv "$tmp/package.json" "$tmp/repo/$1"
            if [[ "$1" == sdk/js-react/package.json ]]; then
                jq --arg version "$2" '.dependencies["@phala/pay"] = $version' "$tmp/repo/$1" >"$tmp/package.json"
                mv "$tmp/package.json" "$tmp/repo/$1"
            fi ;;
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
jq '.dependencies["@phala/pay"] = "^9.9.9"' "$tmp/repo/sdk/js-react/package.json" >"$tmp/package.json"
mv "$tmp/package.json" "$tmp/repo/sdk/js-react/package.json"
if "$tmp/repo/scripts/version.sh" >/dev/null 2>"$tmp/err"; then fail "a drifted React core dependency passed"; fi
grep -q '^version.sh: sdk/js-react/package.json dependencies.@phala/pay names \^9.9.9,' "$tmp/err" ||
    fail "a drifted React core dependency was not named: $(cat "$tmp/err")"

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
for file in "${files[@]}" "${npm_lock_files[@]}"; do
    cmp -s "$root/$file" "$tmp/repo/$file" || fail "setting 0.5 changed $file"
done

# Stub the unrelated Cargo/Python lock refreshes; exercise the real npm version setter on fixtures.
cargo() { [[ "$*" == 'update --workspace --quiet' ]]; }
uv() {
    [[ "$*" == "version --quiet --project sdk/python --no-sync ${!#}" ]] || return 1
    local version=${!#}
    version=${version/-rc./rc}
    sed -i "/^\[project\]\$/,/^\[/s/^version = \".*\"\$/version = \"$version\"/" sdk/python/pyproject.toml
    sed -i "/^name = \"phala-pay\"\$/{n;s/^version = \".*\"\$/version = \"$version\"/}" sdk/python/uv.lock
}
export -f cargo uv
for version in 0.9.0 0.9.0-rc.1; do
    fixture
    [[ "$("$tmp/repo/scripts/version.sh" "$version")" == "$version" ]] || fail "setting $version failed"
    for package in sdk/js/package.json sdk/js-react/package.json sdk/js-server/package.json; do
        [[ "$(jq -r .version "$tmp/repo/$package")" == "$version" ]] || fail "setting $version missed $package"
    done
    [[ "$(jq -r '.dependencies["@phala/pay"]' "$tmp/repo/sdk/js-react/package.json")" == "$version" ]] ||
        fail "setting $version missed React's exact core dependency"
    jq -e '.peerDependencies | has("@phala/pay") | not' "$tmp/repo/sdk/js-react/package.json" >/dev/null ||
        fail "setting $version added a core peer dependency"
    npx -y "$(jq -r .packageManager "$tmp/repo/sdk/js/package.json")" --dir "$tmp/repo/sdk/js" \
        install --frozen-lockfile --lockfile-only --ignore-scripts >/dev/null ||
        fail "setting $version left the npm workspace lockfile out of date"
done

echo "version test passed"
