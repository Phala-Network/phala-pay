#!/usr/bin/env bash
# OS-image PATCH and convergence against a stub CLI; never calls a real CVM.
set -euo pipefail
root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
mkdir "$tmp/deploy" "$tmp/bin"
cp "$root/deploy/phala-cvm.sh" "$root/deploy/deadline.sh" "$tmp/deploy/"
cat >"$tmp/deploy/phala" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
jq -nc '$ARGS.positional' --args -- "$@" >>"$STUB_LOG"
if [[ "$1" == api ]]; then
    echo 'PATCH response diagnostic'
    exit "$STUB_PATCH_STATUS"
fi
[[ "$*" == 'cvms get cvm-1 --json' ]] || exit 64
count=$(($(cat "$STUB_COUNT") + 1))
echo "$count" >"$STUB_COUNT"
mapfile -t states <"$STUB_STATES"
((count <= ${#states[@]})) || count=${#states[@]}
if [[ "${states[count - 1]}" == ERROR ]]; then
    echo '{"error":"read failed","request_id":"req-os-image"}'
    exit 42
fi
printf '%s\n' "${states[count - 1]}"
STUB
printf '#!/bin/sh\n/bin/sleep 0.05\n' >"$tmp/bin/sleep"
chmod +x "$tmp/deploy/phala" "$tmp/bin/sleep"
export PATH="$tmp/bin:$PATH" STUB_LOG=$tmp/calls STUB_COUNT=$tmp/count STUB_STATES=$tmp/states
export STUB_PATCH_STATUS=0 OS_IMAGE_WAIT_SECONDS=2
fail() { echo "CVM os-image: $*" >&2; exit 1; }
state() {
    jq -nc --arg image "$1" --arg status "$2" --argjson busy "$3" \
        '{os: {name: $image}, status: $status, in_progress: $busy, compose_hash: "unchanged"}'
}
change_image() {
    echo 0 >"$STUB_COUNT"
    : >"$STUB_LOG"
    "$tmp/deploy/phala-cvm.sh" os-image cvm-1 dstack-0.6.1 >"$tmp/out" 2>"$tmp/err"
}

{
    # The name can change while the old boot is still running, before busy is published.
    state dstack-0.6.1 running false
    echo ERROR
    state dstack-0.5.12 running false
    state dstack-0.6.1 stopped false
    state dstack-0.6.1 running true
    state dstack-0.6.1 running false
} >"$STUB_STATES"
change_image || fail 'refused a settled CVM on the requested image'
[[ $(cat "$STUB_COUNT") == 6 ]] || fail 'accepted a stale idle state, old image or in-progress operation'
jq -e '.os.name == "dstack-0.6.1" and .compose_hash == "unchanged"' "$tmp/out" >/dev/null
jq -se '.[0] == ["api", "/cvms/cvm-1/os-image", "-X", "PATCH", "-F", "os_image_name=dstack-0.6.1"]
    and ([.[] | select(.[0] == "api")] | length) == 1' "$STUB_LOG" >/dev/null
grep -Fq 'PATCH response diagnostic' "$tmp/err"
grep -Fq 'req-os-image' "$tmp/err"

# A busy indication suffices even when a non-running status was not sampled.
{
    state dstack-0.6.1 running false
    state dstack-0.6.1 running true
    state dstack-0.6.1 running false
} >"$STUB_STATES"
change_image || fail 'refused recovery after observing the operation start'
[[ $(cat "$STUB_COUNT") == 3 ]] || fail 'accepted the new image before the operation started'

for STUB_PATCH_STATUS in 1 42 124; do
    status=0
    change_image || status=$?
    [[ "$status" == "$STUB_PATCH_STATUS" && ! -s "$tmp/out" && $(cat "$STUB_COUNT") == 0 ]] ||
        fail 'PATCH error was hidden or polling proceeded after failure'
    grep -Fq 'PATCH response diagnostic' "$tmp/err"
done
STUB_PATCH_STATUS=0
for fixture in 'dstack-0.5.12 running false' 'dstack-0.6.1 stopped false' 'dstack-0.6.1 running true' 'dstack-0.6.1 running false'; do
    read -r image status busy <<<"$fixture"
    state "$image" "$status" "$busy" >"$STUB_STATES"
    ! change_image || fail "accepted $fixture"
    grep -Fq 'stage=os-image deadline expired' "$tmp/err"
    grep -Fq 'did not run OS image dstack-0.6.1' "$tmp/err"
done
! "$tmp/deploy/phala-cvm.sh" os-image '../cvm' dstack-0.6.1 >/dev/null 2>&1 || fail 'accepted an invalid CVM id'
echo 'CVM OS-image PATCH, error diagnostics and deadline tests passed'
