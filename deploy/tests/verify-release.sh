#!/usr/bin/env bash
# deploy/verify-release.sh against a stub `gh`: a good release verifies every asset, SHA256SUMS,
# and each image, and prints its commit; it stops, before any image, on an images.json that is not
# the three images by digest, and it fails on a commit outside main, a checksum mismatch, or a
# refused attestation. It peels an annotated tag to its commit, and with CALLED_AT, Deploy's
# job.workflow_sha, accepts the commit (a pinned caller) or the tag object (a caller at the tag).
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
commit=$(printf 'c%.0s' {1..40}) tag=$(printf 'a%.0s' {1..40})
mkdir -p "$tmp/bin" "$tmp/assets"
cat >"$tmp/bin/gh" <<'STUB'
#!/usr/bin/env bash
echo "$*" >>"$STUB_LOG"
case "$1 $2" in
    "api repos/Phala-Network/phala-pay/git/ref/tags/v9.9.9") printf 'HTTP/2 200 OK\r\n\r\n%s\n' "$STUB_REF" ;;
    "api repos/Phala-Network/phala-pay/git/tags/$STUB_TAG") printf 'HTTP/2 200 OK\r\n\r\ncommit %s\n' "$STUB_COMMIT" ;;
    "api repos/Phala-Network/phala-pay/compare/$STUB_COMMIT...main")
        if [[ "${STUB_COMPARE_FAILURE:-}" == true ]]; then
            echo 'HTTP/2 404 Not Found'; echo 'gh: Not Found (HTTP 404)' >&2; exit 1
        fi
        printf 'HTTP/2 200 OK\r\n\r\n%s\n' "$STUB_COMPARE" ;;
    "release download") cp "$STUB_ASSETS"/* "${@: -2:1}" ;;
    "attestation download")
        [[ "${STUB_NO_ATTESTATIONS:-}" != true ]] || exit 0
        echo '{}' >sha256:stub.jsonl ;;
    "attestation verify")
        [[ "$*" == *'--bundle '* && -s "${@: -1}" ]] || exit 1
        [[ "$3" != *"$STUB_REFUSE"* && "$*" != *"$STUB_PREDICATE_REFUSE"* ]] ;;
    *) exit 1 ;;
esac
STUB
chmod +x "$tmp/bin/gh"
export PATH="$tmp/bin:$PATH" STUB_LOG="$tmp/log" STUB_ASSETS="$tmp/assets" STUB_COMMIT="$commit" \
    STUB_TAG="$tag"

# assets IMAGES_JSON: the release's five assets, with IMAGES_JSON as images.json.
assets() {
    rm -f "$tmp/assets"/*
    printf '%s\n' "$1" >"$tmp/assets/images.json"
    echo kit >"$tmp/assets/phala-pay-deploy-v9.9.9.tar.gz"
    echo template >"$tmp/assets/phala-cloud-template.yml"
    echo script >"$tmp/assets/deploy.sh"
    (cd "$tmp/assets" && sha256sum images.json phala-pay-deploy-v9.9.9.tar.gz phala-cloud-template.yml \
        deploy.sh >SHA256SUMS)
}
digest=$(printf '1%.0s' {1..64})
good="{\"phala-pay\": \"ghcr.io/phala-network/phala-pay@sha256:$digest\",
    \"postgres-walg\": \"ghcr.io/phala-network/postgres-walg@sha256:$digest\",
    \"phala-pay-reference-product\": \"ghcr.io/phala-network/phala-pay-reference-product@sha256:$digest\"}"
# run NAME [ENV...]: verify-release.sh into a fresh directory, with ENV set, and called_at, when
# set, as its CALLED_AT. v9.9.9 is an annotated tag unless STUB_REF says otherwise.
run() {
    local name=$1
    shift
    : >"$STUB_LOG"
    env STUB_PREDICATE_REFUSE=never STUB_COMPARE=ahead STUB_REFUSE=never STUB_REF="tag $tag" "$@" \
        "$root/deploy/verify-release.sh" v9.9.9 "$tmp/$name" ${called_at:+"$called_at"} \
        >"$tmp/$name.out" 2>"$tmp/$name.err"
}
fail() {
    echo "verify-release: $*" >&2
    exit 1
}

assets "$good"
run good || { cat "$tmp/good.err" >&2; fail "refused a good release"; }
[[ "$(cat "$tmp/good.out")" == "$commit" ]] || fail "did not print the release's commit"
[[ "$(grep -c '^attestation verify' "$STUB_LOG")" == 11 ]] || fail "did not verify 5 assets and provenance plus SBOM for 3 images"
grep -q "^attestation verify $tmp/good/SHA256SUMS .*--source-digest $commit" "$STUB_LOG" ||
    fail "did not verify SHA256SUMS for the release's commit"

for bad in '{"phala-pay": ' "{\"extra\": \"x@sha256:$digest\", ${good#\{}" '{"phala-pay": "phala-pay:latest"}'; do
    assets "$bad"
    ! run bad || fail "accepted images.json $bad"
    ! grep -q '^attestation verify oci://' "$STUB_LOG" || fail "verified an image of a bad images.json"
done
assets "$good"
called_at=$commit run pinned || { cat "$tmp/pinned.err" >&2; fail "refused Deploy pinned at the commit"; }
called_at=$tag run at-tag || { cat "$tmp/at-tag.err" >&2; fail "refused Deploy called at the annotated tag"; }
[[ "$(cat "$tmp/at-tag.out")" == "$commit" ]] || fail "did not print the annotated tag's commit"
called_at=$commit run lightweight STUB_REF="commit $commit" ||
    { cat "$tmp/lightweight.err" >&2; fail "refused a tag that names the commit directly"; }
! called_at=$(printf 'e%.0s' {1..40}) run elsewhere || fail "accepted Deploy at another commit"
! grep -q '^release download' "$STUB_LOG" || fail "downloaded a release Deploy is not called at"
! run outside STUB_COMPARE=diverged || fail "accepted a commit outside main"
! grep -q '^release download' "$STUB_LOG" || fail "downloaded a release whose commit is outside main"
! run compare-failed STUB_COMPARE_FAILURE=true || fail 'accepted a failed main-history lookup'
grep -q 'HTTP 404' "$tmp/compare-failed.err" || fail 'hid the main-history API error'
! grep -q "is not in .* main" "$tmp/compare-failed.err" || fail 'misreported an API failure as a policy rejection'
! run missing-attestations STUB_NO_ATTESTATIONS=true || fail 'accepted missing attestation bundles'
grep -q 'no attestation bundle' "$tmp/missing-attestations.err" || fail 'missing bundle has no diagnostic'
! grep -q '^attestation verify' "$STUB_LOG" || fail 'verified without a downloaded bundle'
! run unattested STUB_REFUSE=SHA256SUMS || fail "accepted an unattested SHA256SUMS"
[[ $(grep -c '^attestation verify .*SHA256SUMS ' "$STUB_LOG") == 1 ]] || fail 'retried a verification rejection'
! run no-sbom STUB_PREDICATE_REFUSE=https://spdx.dev/Document/v2.3 || fail "accepted missing SBOM attestation"
[[ $(grep -c '^attestation verify .*--predicate-type https://spdx.dev/Document/v2.3' "$STUB_LOG") == 1 ]] ||
    fail 'retried an SBOM rejection'
echo tampered >"$tmp/assets/phala-cloud-template.yml"
! run tampered || fail "accepted an asset that does not match SHA256SUMS"
! grep -q '^attestation download' "$STUB_LOG" || fail 'fetched attestations after a checksum rejection'
[[ -z $(find "$tmp" -type d -name '.attestations.*' -print) ]] || fail 'left temporary bundles'
echo "release verifier test passed"
