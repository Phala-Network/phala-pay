#!/usr/bin/env bash
# Shared with the preflight regression test. topup, ok, fail and redact are caller functions.
check_rpc_endpoints() {
    local probe_dir=$1 reason
    if topup rpc check --config >"$probe_dir/healthy.json" 2>"$probe_dir/probe.err"; then
        ok "both RPC endpoints passed the typed self-test through compose"
    else
        # The CLI reserves stdout for JSON and emits a complete sanitized summary on stderr.
        reason=$(tool_error "$probe_dir/probe.err")
        [[ -n "$reason" ]] || reason='RPC check exited without diagnostic output'
        fail "RPC endpoint preflight failed: $(redact "$reason")"
        printf '[]' >"$probe_dir/healthy.json"
    fi
}

# The verifier reserves stdout for JSON and emits safe, fixed diagnostic labels on stderr.
# Keep stderr live so a slow provider/stage is visible while preflight is still running.
check_contract_deployment() {
    local verification=$1 chain_id=$2
    shift 2
    if "$DEPLOY_CONTRACTS_DIR/verify-deployment.sh" "$@" >"$verification"; then
        ok "verify-deployment.sh passed on every provider of chain $chain_id"
    else
        fail "verify-deployment.sh failed on chain $chain_id (see verification diagnostics above)"
    fi
}
