#!/bin/sh
set -eu

# Runs the official dstack verifier of the pinned dstack release on the verification request on
# stdin and prints its JSON result; exits non-zero unless the result is valid. The request is
# `{"quote": null, "attestation": HEX}` for a versioned attestation (the dstack guest agent's
# Attest, as /v1/attestation returns it), or `{"attestation": null, "quote": HEX, "event_log":
# JSON, "vm_config": JSON}`. The verifier checks the TDX quote and its TCB with Intel's collateral,
# replays the event log against RTMR3, and recomputes the OS image measurements from the
# vm_config's os_image_hash. Seed only freshly verified measurement files for the approved image.
#
# dstack-verifier 0.6.1 supports the approved mkosi image's measurement documents. Its one-shot
# request and verified cache pass through stdin; an empty tmpfs is mounted at its cache path,
# including on runners whose daemon cannot bind-mount host files.
if [ "$#" -ne 0 ]; then
    echo "usage: dstack-verifier.sh <REQUEST_JSON >RESULT_JSON" >&2
    exit 64
fi

image=ghcr.io/dstack-tee/dstack-verifier:0.6.1@sha256:9f2c07a94efd3d8a1a89268e5faa35e91e3cb345b9c4ab859c86d35142f4f788
os_hash=ec43cb8c5e04b6bb6c53b33b16076a01d83d01f7f4b703f2f78a0d947d6247e6
release_url=https://github.com/Dstack-TEE/dstack/releases/download/mkosi-os-v0.6.1/dstack-0.6.1.tar.gz
release_sha256=7583f51c717011b8086c2dd4d1e73fbbe7bf2d6d2420b90858d538c26efcca41
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
cat >"$tmp/request.json"

mkdir "$tmp/extracted"
if curl -fsSL --connect-timeout 10 --max-time 300 \
    "https://download.dstack.org/os-images/mr_$os_hash.tar.gz" -o "$tmp/image.tar.gz"; then
    tar -xzf "$tmp/image.tar.gz" -C "$tmp/extracted"
    extracted=$tmp/extracted
else
    echo "dstack verifier: measurement archive unavailable; using pinned mkosi-os-v0.6.1 release (~617 MB)" >&2
    curl -fsSL --retry 3 --connect-timeout 10 --max-time 300 "$release_url" -o "$tmp/image.tar.gz"
    printf '%s  %s\n' "$release_sha256" "$tmp/image.tar.gz" | sha256sum -c - >&2
    tar -xzf "$tmp/image.tar.gz" -C "$tmp/extracted"
    extracted=$tmp/extracted/dstack-0.6.1
fi

# Authenticate the manifest before interpreting its filenames, then every listed file. A
# downloaded or pre-existing directory is never a trusted cache; prepare a fresh one each run.
printf '%s  %s\n' "$os_hash" "$extracted/sha256sum.txt" | sha256sum -c - >&2
(cd "$extracted" && sha256sum -c sha256sum.txt >&2)
cache=$tmp/dstack-verifier/cache/images/$os_hash
mkdir -p "$cache"
# Keep the authenticated manifest and all nine listed files: verifier 0.6.1 validates the cache
# against the manifest and reads the platform measurement documents. Exclude unlisted files.
cp "$extracted/sha256sum.txt" "$extracted/ovmf.fd" "$extracted/bzImage" \
    "$extracted/initramfs.cpio.gz" "$extracted/metadata.json" \
    "$extracted/measurement.tdx.cbor" "$extracted/measurement.snp.cbor" \
    "$extracted/measurement.gcp.cbor" "$extracted/measurement.aws.cbor" \
    "$extracted/measurement.aws.replay.json" "$cache/"

result=$(tar -C "$tmp" -cf - request.json dstack-verifier |
    docker run --rm -i --tmpfs /tmp/dstack-verifier/cache --entrypoint sh "$image" -c '
    tar -xf - -C /tmp &&
        { dstack-verifier --config /etc/dstack/dstack-verifier.toml --verify /tmp/request.json >&2 || :; } &&
        cat /tmp/request.json.verification.json')
printf '%s\n' "$result"
printf '%s' "$result" | jq -e '.is_valid == true' >/dev/null || {
    echo "dstack verifier: $(printf '%s' "$result" | jq -r '.reason // "invalid"')" >&2
    exit 1
}
