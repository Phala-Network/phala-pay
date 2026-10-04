#!/usr/bin/env bash
# deploy/phala-cvm.sh wait against a stub Phala Cloud CLI that answers `cvms get` with a sequence of
# CVM states, each with instance_id null as Phala Cloud reports it: an upgrade waits for the CVM to
# run a new compose; a provision (--unsealed) for it to settle with a new compose, in any status.
# Neither accepts a CVM still in progress or with the previous compose, and both stop at their absolute
# deadline. And deploy/phala-cvm.sh instance-id reads the instance id from an attestation's event log,
# which must name exactly one.
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
mkdir -p "$tmp/deploy" "$tmp/bin"
cp "$root/deploy/deadline.sh" "$root/deploy/phala-cvm.sh" "$tmp/deploy/"
# The stub answers the Nth `cvms get` with line N of STUB_STATES, then repeats the last line.
cat >"$tmp/deploy/phala" <<'STUB'
#!/usr/bin/env bash
[[ "$*" == "cvms get cvm-1 --json" ]] || exit 1
count=$(($(cat "$STUB_COUNT") + 1))
echo "$count" >"$STUB_COUNT"
mapfile -t states <"$STUB_STATES"
((count <= ${#states[@]})) || count=${#states[@]}
printf '%s\n' "${states[count - 1]}"
STUB
printf '#!/bin/sh\n/bin/sleep 0.05\n' >"$tmp/bin/sleep"
export CVM_WAIT_SECONDS=3
chmod +x "$tmp/deploy/phala" "$tmp/bin/sleep"
export PATH="$tmp/bin:$PATH" STUB_COUNT="$tmp/count" STUB_STATES="$tmp/states"

# state STATUS IN_PROGRESS HASH: one `cvms get` answer.
state() {
    jq -nc --arg status "$1" --argjson in_progress "$2" --arg hash "$3" \
        '{status: $status, in_progress: $in_progress, compose_hash: $hash, instance_id: null}'
}
fail() {
    echo "phala-cvm wait: $*" >&2
    exit 1
}
# wait_for ARGS... with the states on stdin, each a JSON object: the CVM JSON it accepts, on stdout.
# It runs in a pipeline's subshell, so a bad state leaves no poll and no error for the checks below.
wait_for() {
    echo 0 >"$STUB_COUNT"
    : >"$tmp/err"
    cat >"$STUB_STATES"
    jq -es 'all(type == "object")' "$STUB_STATES" >/dev/null || fail "a stub state is not a JSON object"
    "$tmp/deploy/phala-cvm.sh" wait "$@" 2>"$tmp/err"
}
# accepted CASE OUTPUT STATE POLLS: the wait accepted exactly STATE, at poll POLLS.
accepted() {
    [[ "$(jq -c . <<<"$2")" == "$3" && "$(cat "$STUB_COUNT")" == "$4" ]] ||
        fail "$1: accepted '$2' at poll $(cat "$STUB_COUNT"), not '$3' at poll $4"
}
# timed_out CASE OUTCOME: the wait failed only by timing out, at the stage deadline, waiting for OUTCOME.
timed_out() {
    if ! grep -q "deadline expired" "$tmp/err" ||
        ! grep -qx "::error::CVM cvm-1 did not $2" "$tmp/err"; then
        fail "$1: did not time out waiting to $2 ($(tail -1 "$tmp/err"))"
    fi
}
new=0xAB12 old=0xcd34

# A provision accepts a settled CVM with the new compose, in any status.
settled=$(state error false "$new")
output=$({ state starting true "$new"; echo "$settled"; } | wait_for --unsealed cvm-1) ||
    fail "a provision refused the settled CVM: $(tail -1 "$tmp/err")"
accepted "a provision" "$output" "$settled" 2
redeployed=$(state stopped false ab12)
output=$({ state error false "$old"; state error true ab12; echo "$redeployed"; } |
    wait_for --unsealed cvm-1 cd34) || fail "a provision's redeploy refused its new compose: $(tail -1 "$tmp/err")"
accepted "a provision's redeploy" "$output" "$redeployed" 3
! state starting true "$new" | wait_for --unsealed cvm-1 >/dev/null || fail "a provision accepted a CVM in progress"
timed_out "a CVM in progress" "settle with a new compose"
! state error false "$old" | wait_for --unsealed cvm-1 cd34 >/dev/null || fail "a provision accepted the previous compose"
timed_out "a provision's redeploy with the previous compose" "settle with a new compose"

# An upgrade accepts only a settled CVM running the new compose.
running=$(state running false "$new")
output=$({ state error false "$new"; state running true "$new"; echo "$running"; } | wait_for cvm-1) ||
    fail "an upgrade refused the running CVM: $(tail -1 "$tmp/err")"
accepted "an upgrade" "$output" "$running" 3
! state error false "$new" | wait_for cvm-1 >/dev/null || fail "an upgrade accepted a CVM that does not run"
timed_out "an upgrade of a CVM that does not run" "run a new compose"
! state running true "$new" | wait_for cvm-1 >/dev/null || fail "an upgrade accepted a CVM in progress"
timed_out "an upgrade in progress" "run a new compose"
! state running false "$old" | wait_for cvm-1 cd34 >/dev/null || fail "an upgrade accepted the previous compose"
timed_out "an upgrade with the previous compose" "run a new compose"

# instance-id: the event log's single instance-id event, as 40 lowercase hex digits.
# attestation EVENT... : an attestation whose event log has each instance-id EVENT payload.
attestation() {
    jq -n '{tcb_info: {event_log: ([{event: "compose-hash", event_payload: "ab12"}]
        + [$ARGS.positional[] | {event: "instance-id", event_payload: .}])}}' --args "$@" >"$tmp/attestation.json"
}
id=$(printf 'A%.0s' {1..40})
attestation "$id"
[[ "$("$tmp/deploy/phala-cvm.sh" instance-id "$tmp/attestation.json")" == "$(printf 'a%.0s' {1..40})" ]] ||
    fail "instance-id did not read the event log's instance id"
for events in "" "$id $id" "${id}0"; do
    # shellcheck disable=SC2086 # each word an event
    attestation $events
    ! "$tmp/deploy/phala-cvm.sh" instance-id "$tmp/attestation.json" >/dev/null 2>&1 ||
        fail "instance-id accepted the events '$events'"
done
echo "CVM wait test passed"
