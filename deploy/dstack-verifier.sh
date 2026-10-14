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
# dstacktee/dstack-verifier:0.5.9 is built from dstack 282eeb27 (its /etc/.GIT_REV), the v0.5.9
# tag. Keep this verifier: dstack-0.5.12 retains ovmf_variant=pre202505 and the 0.5 measurement
# semantics. Its one-shot request and the verified cache pass through stdin; an empty tmpfs is
# mounted at its cache path, including on runners whose daemon cannot bind-mount host files.
if [ "$#" -ne 0 ]; then
    echo "usage: dstack-verifier.sh <REQUEST_JSON >RESULT_JSON" >&2
    exit 64
fi

image=dstacktee/dstack-verifier:0.5.9@sha256:cfc06d5bdaa71a8a942c8bfa04d2d17dc30f13d92f26386c5d45d454606e8b70
os_hash=8db40cb23008d6213668b5816702d100b37b859a8a5bfd18d938ae3a1b3715bd
release_url=https://github.com/Dstack-TEE/meta-dstack/releases/download/v0.5.12/dstack-0.5.12.tar.gz
release_sha256=e3037d99d99ec963bbe43929856c1e46e7197a1178da5867024c4b68064eb176
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
    echo "dstack verifier: measurement archive unavailable; using pinned meta-dstack v0.5.12 release" >&2
    curl -fsSL --retry 3 --connect-timeout 10 --max-time 300 "$release_url" -o "$tmp/image.tar.gz"
    printf '%s  %s\n' "$release_sha256" "$tmp/image.tar.gz" | sha256sum -c - >&2
    tar -xzf "$tmp/image.tar.gz" -C "$tmp/extracted"
    extracted=$tmp/extracted/dstack-0.5.12
fi

# Authenticate the manifest before interpreting its filenames, then every listed file. A
# downloaded or pre-existing directory is never a trusted cache; prepare a fresh one each run.
printf '%s  %s\n' "$os_hash" "$extracted/sha256sum.txt" | sha256sum -c - >&2
(cd "$extracted" && sha256sum -c sha256sum.txt >&2)
cache=$tmp/dstack-verifier/cache/images/$os_hash
mkdir -p "$cache"
# The approved manifest lists exactly the verifier's four inputs. Leave unlisted release files
# (rootfs.img.verity, digest.txt) out of the cache, as the official downloader does.
cp "$extracted/ovmf.fd" "$extracted/bzImage" "$extracted/initramfs.cpio.gz" \
    "$extracted/metadata.json" "$cache/"

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
