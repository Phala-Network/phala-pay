#!/bin/sh
set -eu

# Runs the official dstack verifier of the pinned dstack release on the verification request on
# stdin and prints its JSON result; full mode exits non-zero unless the result is valid. The request is
# `{"quote": null, "attestation": HEX}` for a versioned attestation (the dstack guest agent's
# Attest, as /v1/attestation returns it), or `{"attestation": null, "quote": HEX, "event_log":
# JSON, "vm_config": JSON}`. The verifier checks the TDX quote and its TCB with Intel's collateral,
# replays the event log against RTMR3, and recomputes the OS image measurements from the
# vm_config's os_image_hash. Seed only freshly verified measurement files for the approved image.
#
# dstack-verifier 0.6.5 supports the approved mkosi image's measurement documents. Its one-shot
# request and verified cache pass through stdin; an empty tmpfs is mounted at its cache path,
# including on runners whose daemon cannot bind-mount host files.
# Partial is only for pre-seal provisioning: on dstack TDX quote_verified includes RTMR3
# replay and payload/digest validation, before the separate OS/vm_config check. See dstack
# v0.6.5 dstack-attest::verify_tdx_quote_with_events and verifier::VerificationDetails.
partial=false
if [ "$#" -eq 1 ] && [ "$1" = --partial ]; then partial=true
elif [ "$#" -ne 0 ]; then
    echo "usage: dstack-verifier.sh [--partial] <REQUEST_JSON >RESULT_JSON" >&2
    exit 64
fi

image=ghcr.io/dstack-tee/dstack-verifier:0.6.5@sha256:a8f4d606ec53bbbf05bb128d618de1de8fbe2f0b8679d326b34710260a290ca4
os_hash=4771a29e03e93f0e2d5e37174cd8bb1112942c9689deb36b16c5df9e183d8623
release_url=https://github.com/Dstack-TEE/dstack/releases/download/mkosi-os-v0.6.5/dstack-0.6.5.tar.gz
release_sha256=f33965e051234044b1805845b3318a92c87477b728882665cd8488d1a0d66016
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
cat >"$tmp/request.json"

mkdir -p "$tmp/dstack-verifier"
if [ "$partial" = true ]; then
    jq -e '.attestation == null and .vm_config == null and (.quote | type) == "string"
        and (.event_log | type) == "string"' "$tmp/request.json" >/dev/null || {
        echo "partial verification requires only a raw quote and event log" >&2; exit 64;
    }
else
    mkdir "$tmp/extracted"
    if curl -fsSL --connect-timeout 10 --max-time 300 \
        "https://download.dstack.org/os-images/mr_$os_hash.tar.gz" -o "$tmp/image.tar.gz"; then
        tar -xzf "$tmp/image.tar.gz" -C "$tmp/extracted"
        extracted=$tmp/extracted
    else
        echo "dstack verifier: measurement archive unavailable; using pinned mkosi-os-v0.6.5 release (~602 MiB)" >&2
        curl -fsSL --retry 3 --connect-timeout 10 --max-time 300 "$release_url" -o "$tmp/image.tar.gz"
        printf '%s  %s\n' "$release_sha256" "$tmp/image.tar.gz" | sha256sum -c - >&2
        tar -xzf "$tmp/image.tar.gz" -C "$tmp/extracted"
        extracted=$tmp/extracted/dstack-0.6.5
    fi

    # Authenticate the manifest before interpreting its filenames, then every listed file. A
    # downloaded or pre-existing directory is never a trusted cache; prepare a fresh one each run.
    printf '%s  %s\n' "$os_hash" "$extracted/sha256sum.txt" | sha256sum -c - >&2
    (cd "$extracted" && sha256sum -c sha256sum.txt >&2)
    cache=$tmp/dstack-verifier/cache/images/$os_hash
    mkdir -p "$cache"
    # Keep the authenticated manifest and all nine listed files: verifier 0.6.5 validates the cache
    # against the manifest and reads the platform measurement documents. Exclude unlisted files.
    cp "$extracted/sha256sum.txt" "$extracted/ovmf.fd" "$extracted/bzImage" \
        "$extracted/initramfs.cpio.gz" "$extracted/metadata.json" \
        "$extracted/measurement.tdx.cbor" "$extracted/measurement.snp.cbor" \
        "$extracted/measurement.gcp.cbor" "$extracted/measurement.aws.cbor" \
        "$extracted/measurement.aws.replay.json" "$cache/"
fi

result=$(tar -C "$tmp" -cf - request.json dstack-verifier |
    docker run --rm -i --tmpfs /tmp/dstack-verifier/cache --entrypoint sh "$image" -c '
    tar -xf - -C /tmp &&
        { dstack-verifier --config /etc/dstack/dstack-verifier.toml --verify /tmp/request.json >&2 || :; } &&
        cat /tmp/request.json.verification.json')
printf '%s\n' "$result"
if [ "$partial" = true ]; then
    printf '%s' "$result" | jq -e '.details.quote_verified == true
        and .details.tcb_status == "UpToDate" and .details.tee_variant == "dstack-tdx"' >/dev/null || {
        echo "partial quote/event-log verification failed" >&2; exit 1;
    }
    exit 0
fi
printf '%s' "$result" | jq -e '.is_valid == true' >/dev/null || {
    echo "dstack verifier: $(printf '%s' "$result" | jq -r '.reason // "invalid"')" >&2
    exit 1
}
