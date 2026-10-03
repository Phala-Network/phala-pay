#!/usr/bin/env bash
# Shared with the preflight regression test. topup, ok, fail and redact are caller functions.
check_rpc_groups() {
    local probe_dir=$1 reason
    if topup rpc check --config >"$probe_dir/healthy.json" 2>"$probe_dir/probe.err"; then
        ok "RPC groups have a fully validated serving member each"
    else
        # The CLI reserves stdout for JSON and emits a complete sanitized summary on stderr.
        reason=$(tool_error "$probe_dir/probe.err")
        [[ -n "$reason" ]] || reason='RPC check exited without diagnostic output'
        fail "RPC group preflight failed: $(redact "$reason")"
        printf '[]' >"$probe_dir/healthy.json"
    fi
}
