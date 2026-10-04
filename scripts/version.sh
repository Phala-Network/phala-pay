#!/usr/bin/env bash
# Phala Pay's one version: the Cargo workspace version, which the service, @phala/pay, @phala/pay-react, and @phala/pay-server (sdk/js, sdk/js-react, sdk/js-server), and
# phala-pay (sdk/python) share, all released by one `v<version>` tag (CONTRIBUTING.md, "Releasing").
#
#   scripts/version.sh            prints it, and fails unless sdk/js/package.json, sdk/js-react/package.json, sdk/js-server/package.json,
#                                 sdk/python/pyproject.toml, sdk/python/uv.lock, and the Python SDK
#                                 deploy/deploy.sh pins name it too
#   scripts/version.sh VERSION    sets it in Cargo.toml, Cargo.lock, all three npm package manifests,
#                                 the npm workspace lockfile, and the Python files (needs cargo, jq,
#                                 npx, and uv)
#
# VERSION is X.Y.Z, or X.Y.Z-rc.N for a pre-release, which Python spells X.Y.ZrcN (PEP 440).
set -euo pipefail

cd "$(dirname "$0")/.."

cargo_version() {
    sed -n '/^\[workspace.package\]$/,/^\[/s/^version = "\(.*\)"$/\1/p' Cargo.toml
}
python_version() {
    sed -n '/^\[project\]$/,/^\[/s/^version = "\(.*\)"$/\1/p' sdk/python/pyproject.toml
}
# The version of the lock's own package, phala-pay.
python_lock_version() {
    awk '/^\[\[package\]\]$/ { own = 0 } $0 == "name = \"phala-pay\"" { own = 1 }
        own && /^version = / { gsub(/^version = "|"$/, ""); print; exit }' sdk/python/uv.lock
}
# The Python SDK version deploy/deploy.sh generates an admin key with.
deploy_sdk_version() {
    sed -n 's/^sdk=phala-pay==//p' deploy/deploy.sh
}

if (($# == 1)); then
    [[ "$1" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-rc\.(0|[1-9][0-9]*))?$ ]] ||
        { echo "version.sh: $1 is not X.Y.Z or X.Y.Z-rc.N" >&2; exit 64; }
    sed -i "/^\[workspace.package\]\$/,/^\[/s/^version = \".*\"\$/version = \"$1\"/" Cargo.toml
    cargo update --workspace --quiet
    for package in sdk/js/package.json sdk/js-react/package.json sdk/js-server/package.json; do
        if [[ "$package" == sdk/js-react/package.json ]]; then
            jq --arg version "$1" '.version = $version | .dependencies["@phala/pay"] = $version' "$package" >"$package.tmp"
        else
            jq --arg version "$1" '.version = $version' "$package" >"$package.tmp"
        fi
        mv "$package.tmp" "$package"
    done
    # React's exact core dependency is also an importer specifier in the workspace lockfile.
    # Keep the release's frozen install valid, without running package lifecycle scripts.
    npx -y "$(jq -r .packageManager sdk/js/package.json)" --dir sdk/js install --lockfile-only --ignore-scripts >&2
    uv version --quiet --project sdk/python --no-sync "$1"
    sed -i "s/^sdk=phala-pay==.*\$/sdk=phala-pay==${1/-rc./rc}/" deploy/deploy.sh
elif (($# != 0)); then
    echo "usage: version.sh [VERSION]" >&2
    exit 64
fi

version=$(cargo_version)
python=${version/-rc./rc}
status=0
mismatch() {
    echo "version.sh: $1 names ${2:-no version}, not the Cargo workspace version $version" >&2
    status=1
}
for package in sdk/js/package.json sdk/js-react/package.json sdk/js-server/package.json; do
    [[ "$(jq -r .version "$package")" == "$version" ]] ||
        mismatch "$package" "$(jq -r .version "$package")"
done
[[ "$(jq -r '.dependencies["@phala/pay"]' sdk/js-react/package.json)" == "$version" ]] ||
    mismatch 'sdk/js-react/package.json dependencies.@phala/pay' "$(jq -r '.dependencies["@phala/pay"]' sdk/js-react/package.json)"
[[ "$(python_version)" == "$python" ]] || mismatch sdk/python/pyproject.toml "$(python_version)"
[[ "$(python_lock_version)" == "$python" ]] || mismatch sdk/python/uv.lock "$(python_lock_version)"
[[ "$(deploy_sdk_version)" == "$python" ]] || mismatch deploy/deploy.sh "$(deploy_sdk_version)"
((status == 0)) || exit 1
echo "$version"
