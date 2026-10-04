#!/usr/bin/env bash
# Sourced stage deadline helpers. SECONDS measures elapsed wall time in the shell, including calls.
stage_start() {
    stage_name=$1 stage_started=$SECONDS
    local budget=$2
    [[ "$budget" =~ ^[1-9][0-9]*$ ]] || { echo "invalid stage budget" >&2; return 64; }
    stage_deadline=$((SECONDS + budget))
}
stage_remaining() { ((SECONDS < stage_deadline)); }
stage_call() {
    local limit=$1 remaining=$((stage_deadline - SECONDS)) result=0
    shift
    ((remaining > 0)) || { stage_expired; return 124; }
    ((limit <= remaining)) || limit=$remaining
    timeout --signal=TERM --kill-after=2 "$limit" "$@" || result=$?
    echo "stage=$stage_name elapsed=$((SECONDS - stage_started))s remaining=$((stage_deadline - SECONDS))s call_status=$result" >&2
    return "$result"
}
stage_sleep() {
    local delay=$1 remaining=$((stage_deadline - SECONDS))
    ((remaining > 0)) || return 0
    ((delay <= remaining)) || delay=$remaining
    sleep "$delay"
}
stage_expired() {
    echo "::error::stage=$stage_name deadline expired elapsed=$((SECONDS - stage_started))s budget=$((stage_deadline - stage_started))s" >&2
}
