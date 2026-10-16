#!/usr/bin/env bash
# Fresh authenticated measurement caches, including the pinned release fallback; no network.
set -euo pipefail
root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
mkdir "$tmp/bin" "$tmp/fixture" "$tmp/fixture/dstack-0.6.1"
files=(ovmf.fd bzImage initramfs.cpio.gz metadata.json measurement.tdx.cbor measurement.snp.cbor
    measurement.gcp.cbor measurement.aws.cbor measurement.aws.replay.json)
for file in "${files[@]}"; do
    printf '%s fixture\n' "$file" >"$tmp/fixture/dstack-0.6.1/$file"
done
(cd "$tmp/fixture/dstack-0.6.1" && sha256sum "${files[@]}" >sha256sum.txt)
printf 'unlisted\n' >"$tmp/fixture/dstack-0.6.1/rootfs.img.verity"
tar -C "$tmp/fixture" -czf "$tmp/release.tar.gz" dstack-0.6.1
tar -C "$tmp/fixture/dstack-0.6.1" -czf "$tmp/primary.tar.gz" .
os_hash=$(sha256sum "$tmp/fixture/dstack-0.6.1/sha256sum.txt" | cut -d ' ' -f1)
archive_hash=$(sha256sum "$tmp/release.tar.gz" | cut -d ' ' -f1)
# Replace only the two immutable pins in a private copy with the corresponding fixture pins.
sed -e "s/^os_hash=.*/os_hash=$os_hash/" -e "s/^release_sha256=.*/release_sha256=$archive_hash/" \
    "$root/deploy/dstack-verifier.sh" >"$tmp/verifier.sh"
chmod +x "$tmp/verifier.sh"
cat >"$tmp/bin/curl" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$STUB_LOG"
output=""
while (($#)); do
    case "$1" in
        -o) output=$2; shift ;;
        https://download.dstack.org/*) [[ "$PRIMARY_AVAILABLE" == true ]] || exit 22
            archive=$PRIMARY_ARCHIVE ;;
        https://github.com/Dstack-TEE/dstack/releases/download/mkosi-os-v0.6.1/dstack-0.6.1.tar.gz)
            archive=$RELEASE_ARCHIVE ;;
    esac
    shift
done
cp "$archive" "$output"
STUB
cat >"$tmp/bin/docker" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
[[ "$*" == *'--tmpfs /tmp/dstack-verifier/cache'* ]] || exit 64
printf '%s\n' "$*" >"$DOCKER_LOG"
mkdir "$DOCKER_INPUT"
tar -xf - -C "$DOCKER_INPUT"
cmp "$DOCKER_INPUT/request.json" "$REQUEST"
cache=$DOCKER_INPUT/dstack-verifier/cache/images/$EXPECTED_HASH
for file in sha256sum.txt ovmf.fd bzImage initramfs.cpio.gz metadata.json measurement.tdx.cbor \
    measurement.snp.cbor measurement.gcp.cbor measurement.aws.cbor measurement.aws.replay.json; do
    cmp "$cache/$file" "$FIXTURE/$file"
done
[[ $(find "$cache" -type f | wc -l) == 10 && ! -e "$DOCKER_INPUT/dstack-verifier/cache/measurements" ]]
printf '{"is_valid":%s,"reason":"fixture result"}\n' "$VERIFIER_VALID"
STUB
chmod +x "$tmp/bin/"*
export PATH="$tmp/bin:$PATH" STUB_LOG=$tmp/downloads DOCKER_LOG=$tmp/docker
export PRIMARY_ARCHIVE=$tmp/primary.tar.gz RELEASE_ARCHIVE=$tmp/release.tar.gz
export REQUEST=$tmp/request.json EXPECTED_HASH=$os_hash FIXTURE=$tmp/fixture/dstack-0.6.1
export DOCKER_INPUT=$tmp/container VERIFIER_VALID=true PRIMARY_AVAILABLE=false
printf '{"quote":"fixture","vm_config":"fixture"}\n' >"$REQUEST"
fail() { echo "dstack verifier cache: $*" >&2; exit 1; }
verify() {
    rm -rf "$DOCKER_INPUT"
    rm -f "$STUB_LOG" "$DOCKER_LOG"
    "$tmp/verifier.sh" <"$REQUEST" >"$tmp/result" 2>"$tmp/err"
}
refused() {
    ! verify || fail "$1 was accepted"
    [[ ! -e "$DOCKER_LOG" ]] || fail "$1 reached the verifier with unverified files"
}
verify || fail 'release fallback failed'
jq -e '.is_valid' "$tmp/result" >/dev/null
[[ $(wc -l <"$STUB_LOG") == 2 ]] || fail 'fallback did not try both sources'
grep -Fq 'pinned mkosi-os-v0.6.1 release' "$tmp/err"
PRIMARY_AVAILABLE=true
verify || fail 'primary measurement archive failed'
[[ $(wc -l <"$STUB_LOG") == 1 ]] || fail 'downloaded the fallback when primary was available'

# The release archive pin, the manifest pin and each file checksum independently fail closed.
PRIMARY_AVAILABLE=false
cp "$tmp/release.tar.gz" "$tmp/bad-release.tar.gz"
printf 'tampered' >>"$tmp/bad-release.tar.gz"
RELEASE_ARCHIVE=$tmp/bad-release.tar.gz
refused 'a tampered release archive'
RELEASE_ARCHIVE=$tmp/release.tar.gz PRIMARY_AVAILABLE=true
printf 'tampered\n' >>"$FIXTURE/sha256sum.txt"
tar -C "$FIXTURE" -czf "$tmp/bad-manifest.tar.gz" .
PRIMARY_ARCHIVE=$tmp/bad-manifest.tar.gz
refused 'a substituted manifest'
sed -i '$d' "$FIXTURE/sha256sum.txt"
for file in "${files[@]}"; do
    printf 'tampered\n' >>"$FIXTURE/$file"
    tar -C "$FIXTURE" -czf "$tmp/bad-file.tar.gz" .
    PRIMARY_ARCHIVE=$tmp/bad-file.tar.gz
    refused "$file with the wrong checksum"
    printf '%s fixture\n' "$file" >"$FIXTURE/$file"
done
mv "$FIXTURE/measurement.tdx.cbor" "$tmp/measurement.tdx.cbor"
tar -C "$FIXTURE" -czf "$tmp/missing-file.tar.gz" .
PRIMARY_ARCHIVE=$tmp/missing-file.tar.gz
refused 'a missing platform measurement document'
mv "$tmp/measurement.tdx.cbor" "$FIXTURE/measurement.tdx.cbor"
PRIMARY_ARCHIVE=$tmp/primary.tar.gz VERIFIER_VALID=false
! verify || fail 'an invalid verifier result was accepted'
[[ -e "$DOCKER_LOG" ]] || fail 'invalid-result case did not reach the verifier'
grep -Fq 'dstack verifier: fixture result' "$tmp/err"
echo 'dstack verifier primary/fallback authentication and invalid-result tests passed'
