#!/usr/bin/env bash
# Verifies release VERSION of Phala-Network/phala-pay (deploy/README.md, "Releases") and leaves its
# assets in DIR; prints the release's commit. Deploy runs it, and so does an operator by hand. It
# stops at the first failure:
#   1. The release's commit is the one its tag names (a protected tag of an immutable release),
#      peeled through annotated tag objects, and a commit of main's history. With CALLED_AT, the
#      SHA Deploy runs at (job.workflow_sha), that SHA must be the release's commit (a caller
#      pinned it) or a tag object naming it (a caller called Deploy at the annotated tag).
#   2. Every asset matches SHA256SUMS.
#   3. Every asset and SHA256SUMS has a GitHub build provenance attestation signed by release.yml
#      at refs/tags/VERSION on a GitHub-hosted runner, for that commit.
#   4. images.json names exactly the three images, each by repository@sha256 digest, and each image
#      has the same provenance attestation and a signed SPDX SBOM attestation.
#
# It needs the GitHub CLI 2.101.0 (the version Deploy pins), logged in or with GH_TOKEN, and jq.
#
# Usage: verify-release.sh VERSION DIR [CALLED_AT]
set -euo pipefail

# shellcheck source=deploy/deadline.sh
source "$(dirname -- "$0")/deadline.sh"
# shellcheck source=deploy/github-read.sh
source "$(dirname -- "$0")/github-read.sh"
stage_start release-verification 600
gh() { stage_call 60 gh "$@"; }
trap 'echo "verify-release: failed at line $LINENO (exit $?)" >&2' ERR

repository=Phala-Network/phala-pay
version=${1:-} dir=${2:-} called_at=${3:-}
[[ "$version" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?$ && -n "$dir" &&
    "$called_at" =~ ^([0-9a-f]{40})?$ ]] ||
    { echo "usage: $0 VERSION DIR [CALLED_AT]" >&2; exit 64; }

object='"\(.object.type) \(.object.sha)"'
named=$(github_read api "repos/$repository/git/ref/tags/$version" --jq "$object")
read -r type commit <<<"$named"
shas=("$commit")
while [[ "$type" == tag ]]; do
    [[ "$commit" =~ ^[0-9a-f]{40}$ ]] ||
        { echo "$version has an invalid annotated tag object: $named" >&2; exit 1; }
    named=$(github_read api "repos/$repository/git/tags/$commit" --jq "$object")
    read -r type commit <<<"$named"
    shas+=("$commit")
done
[[ "$type" == commit && "$commit" =~ ^[0-9a-f]{40}$ ]] ||
    { echo "$version names $named, not a commit" >&2; exit 1; }
[[ -z "$called_at" || " ${shas[*]} " == *" $called_at "* ]] ||
    { echo "Deploy runs at $called_at, not $version's commit $commit: call it at $version" >&2; exit 1; }
comparison=$(github_read api "repos/$repository/compare/$commit...main" --jq .status)
[[ "$comparison" =~ ^(ahead|identical)$ ]] ||
    { echo "$version's commit $commit is not in $repository's main" >&2; exit 1; }
echo "$version is commit $commit, in main's history" >&2

mkdir -p "$dir"
gh release download "$version" -R "$repository" -D "$dir" --clobber
(cd "$dir" && if command -v sha256sum >/dev/null; then sha256sum --quiet -c SHA256SUMS; else
    shasum -a 256 --quiet -c SHA256SUMS; fi)

provenance=(-R "$repository" --source-digest "$commit" --deny-self-hosted-runners
    --cert-identity "https://github.com/$repository/.github/workflows/release.yml@refs/tags/$version")
# Download into a fresh directory: gh download succeeds without writing a file when no
# attestations exist. Never reuse a stale bundle, and verify each fetched predicate once.
bundles=$(mktemp -d "$dir/.attestations.XXXXXX")
bundles=$(cd "$bundles" && pwd)
trap 'rm -rf "$bundles"' EXIT
verify_attestation() {
    local artifact=$1 predicate=${2:-https://slsa.dev/provenance/v1} bundle
    rm -f "$bundles"/*.jsonl
    (cd "$bundles" && github_read attestation download "$artifact" -R "$repository" \
        --predicate-type "$predicate") >/dev/null
    local files=("$bundles"/*.jsonl)
    [[ ${#files[@]} == 1 && -s "${files[0]}" ]] ||
        { echo "no attestation bundle for $artifact ($predicate)" >&2; return 1; }
    bundle=${files[0]}
    gh attestation verify "$artifact" "${provenance[@]}" --predicate-type "$predicate" \
        --bundle "$bundle" >/dev/null ||
        { echo "attestation verification rejected $artifact ($predicate)" >&2; return 1; }
}
dir=$(cd "$dir" && pwd)
for asset in images.json "phala-pay-deploy-$version.tar.gz" phala-cloud-template.yml deploy.sh SHA256SUMS; do
    verify_attestation "$dir/$asset"
    echo "verified $asset" >&2
done
jq -e 'type == "object" and keys == ["phala-pay", "phala-pay-reference-product", "postgres-walg"]
    and all(.[]; type == "string" and test("^[a-z0-9]+([._/-][a-z0-9]+)*@sha256:[0-9a-f]{64}$"))' \
    "$dir/images.json" >/dev/null || { echo "images.json is not the three images by digest" >&2; exit 1; }
images=$(jq -r '.[]' "$dir/images.json")
for image in $images; do
    verify_attestation "oci://$image"
    verify_attestation "oci://$image" https://spdx.dev/Document/v2.3
    echo "verified provenance and SPDX SBOM for $image" >&2
done
echo "$commit"
