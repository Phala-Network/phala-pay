#!/usr/bin/env bash
# shellcheck disable=SC2034  # the constants and TARGET_* are read by the scripts that source this

set -euo pipefail

DEPLOY_CONTRACTS_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$DEPLOY_CONTRACTS_DIR/../.." && pwd)"
CONTRACTS_DIR="$REPO_ROOT/contracts"

DETERMINISTIC_PROXY="0x4e59b44847b379578588920cA78FbF26c0B4956C"
DETERMINISTIC_PROXY_CODE_HASH="0x2fa86add0aed31f33a762c9d88e807c475bd51d0f52bd0955754b2608f7e4989"
FACTORY_SALT="0x26f1d8427b0c2db52d02ee55402198e592a278fb8541ba4dfaefbd1ea7b09eee"
ANVIL_PRIVATE_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
ZERO_ADDRESS="0x0000000000000000000000000000000000000000"
ZERO_HASH="0x0000000000000000000000000000000000000000000000000000000000000000"

export PATH="$HOME/.foundry/bin:$HOME/.cargo/bin:$PATH"

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

lower() {
    tr '[:upper:]' '[:lower:]' <<<"$1"
}

# The factory has no constructor arguments: its init code is the build's creation code, so its
# address depends only on the build and the fixed salt.
factory_init_code() {
    (cd "$CONTRACTS_DIR" && forge inspect ForwarderFactory bytecode)
}

predicted_factory() {
    cast compute-address "$DETERMINISTIC_PROXY" --salt "$FACTORY_SALT" \
        --init-code "$(factory_init_code)"
}

predicted_implementation() {
    cast compute-address "$1" --nonce 1
}

code_hash() {
    local rpc_url="$1"
    local address="$2"
    local code

    code="$(cast code "$address" --rpc-url "$rpc_url")"
    if [[ "$code" == "0x" ]]; then
        printf '%s\n' "$ZERO_HASH"
    else
        cast keccak "$code"
    fi
}

# Starts a local anvil in the background on a port the kernel picks (--port 0) and reads the
# bound address from its log, so concurrent jobs on a shared runner cannot race for a port.
# Call it directly, not in $(...): it sets ANVIL_PID and ANVIL_RPC_URL in the caller's shell.
# ANVIL_PID is set as soon as anvil is spawned so a caller's signal trap can stop it.
# Usage: start_anvil LOG_FILE [ANVIL_ARGS...]
start_anvil() {
    local log="$1"
    local address
    shift

    # Created here, not only by the background redirection: the child opens it when it is next
    # scheduled, which on a loaded host can be after the first read below.
    : >"$log"
    anvil --host 127.0.0.1 --port 0 "$@" >>"$log" 2>&1 &
    ANVIL_PID=$!
    ANVIL_RPC_URL=""
    for _ in $(seq 1 100); do
        if [[ -z "$ANVIL_RPC_URL" ]]; then
            address="$(sed -n 's/^Listening on \(127\.0\.0\.1:[0-9][0-9]*\)$/\1/p' "$log")"
            [[ -z "$address" ]] || ANVIL_RPC_URL="http://$address"
        fi
        if [[ -n "$ANVIL_RPC_URL" ]] && cast chain-id --rpc-url "$ANVIL_RPC_URL" >/dev/null 2>&1; then
            return 0
        fi
        kill -0 "$ANVIL_PID" 2>/dev/null || break
        sleep 0.1
    done
    kill "$ANVIL_PID" 2>/dev/null || true
    wait "$ANVIL_PID" 2>/dev/null || true
    die "anvil did not become ready; last log lines: $(tail -n 5 "$log")"
}

# Installs the canonical Multicall3 (deploy/contracts/multicall3.json) on a local Anvil chain,
# which has none. Real chains carry the canonical deployment; `topup run` refuses a chain where it
# is missing, because every balance and addressOf read is aggregated through it.
# Usage: install_anvil_multicall3 RPC_URL
install_anvil_multicall3() {
    local rpc_url="$1"
    local recorded="$DEPLOY_CONTRACTS_DIR/multicall3.json"
    local address client

    require_command jq
    client="$(cast rpc --rpc-url "$rpc_url" web3_clientVersion | tr -d '"')"
    [[ "$(lower "$client")" == *anvil* ]] || die "Multicall3 is installed only on Anvil"
    address="$(jq -er .address "$recorded")"
    cast rpc --rpc-url "$rpc_url" anvil_setCode "$address" "$(jq -er .runtime_code "$recorded")" \
        >/dev/null
    [[ "$(lower "$(code_hash "$rpc_url" "$address")")" == \
        "$(lower "$(jq -er .runtime_code_hash "$recorded")")" ]] ||
        die "Multicall3 at $address does not have the recorded code hash"
}

is_address() {
    [[ "$1" =~ ^0x[0-9a-fA-F]{40}$ ]]
}

# Statically validates a configured treasury Safe expectations file and exports the approved
# treasury as EXPECTED_TREASURY.
load_expectations() {
    local file="$1"
    local problems

    require_command jq
    [[ -f "$file" ]] || die "expectations file not found: $file"
    jq -e '.configured == true' "$file" >/dev/null || \
        die "expectations are not configured; Finance must commit the approved networks, treasury Safe, owners, threshold, proxy code hashes, and singleton"

    problems="$(jq -r --arg zero_address "$ZERO_ADDRESS" --arg zero_hash "$ZERO_HASH" '
        def address: type == "string" and test("^0x[0-9a-fA-F]{40}$") and ascii_downcase != $zero_address;
        def hash: type == "string" and test("^0x[0-9a-fA-F]{64}$") and ascii_downcase != $zero_hash;
        def address_or_zero: type == "string" and test("^0x[0-9a-fA-F]{40}$");
        def safe_entries($target): [.safes[] | select(.address | ascii_downcase == ($target | ascii_downcase))];
        if (.networks | type) != "object" or (.networks | length) == 0 or
            any(.networks[]; (.chain_id | type) != "number" or .chain_id <= 0 or .chain_id != (.chain_id | floor))
        then "networks must map each target network to a positive integer chain_id" else empty end,
        if (.treasury | address) then empty else "treasury must be a non-zero address" end,
        if (.safes | type) != "array" or (.safes | length) == 0 then "safes must list the approved Safes"
        else
            (.safes[] |
                if (.address | address) and (.owners | type == "array" and length > 0 and all(address)) and
                    (.owners | map(ascii_downcase) | unique | length) == (.owners | length) and
                    (.threshold | type == "number") and .threshold > 0 and .threshold <= (.owners | length) and
                    (.proxy_code_hashes | type == "array" and length > 0 and all(hash)) and
                    (.singleton | address) and (.singleton_code_hash | hash) and
                    (.modules | type == "array" and all(address)) and
                    (.modules | map(ascii_downcase) | unique | length) == (.modules | length) and
                    (.guard | address_or_zero) and (.fallback_handler | address_or_zero)
                then empty
                else "invalid Safe entry \(.address | tostring): need address, unique owners, 0 < threshold <= owners, proxy_code_hashes, singleton, singleton_code_hash, unique modules (may be empty), guard, and fallback_handler (zero address for none)"
                end),
            if (.treasury | address) and (safe_entries(.treasury) | length) != 1
            then "treasury must match exactly one approved Safe entry" else empty end
        end
    ' "$file" 2>/dev/null)" || die "malformed expectations file: $file"
    [[ -z "$problems" ]] || die "invalid expectations in $file: ${problems//$'\n'/; }"

    EXPECTED_TREASURY="$(jq -r '.treasury' "$file")"
}

# Parses NETWORK[/LABEL]=URL and resolves the network's committed chain id into TARGET_NAME,
# TARGET_NETWORK, TARGET_RPC_URL, and TARGET_EXPECTED_CHAIN_ID.
parse_target() {
    local file="$1"
    local entry="$2"

    [[ "$entry" == *=* ]] || die "RPC must be NETWORK[/LABEL]=URL: $entry"
    TARGET_NAME="${entry%%=*}"
    TARGET_RPC_URL="${entry#*=}"
    TARGET_NETWORK="${TARGET_NAME%%/*}"
    [[ -n "$TARGET_NETWORK" && -n "$TARGET_RPC_URL" ]] || die "RPC must be NETWORK[/LABEL]=URL: $entry"
    TARGET_EXPECTED_CHAIN_ID="$(jq -er --arg network "$TARGET_NETWORK" \
        '.networks[$network].chain_id' "$file")" || \
        die "unknown network '$TARGET_NETWORK'; expected one of: $(jq -r '.networks | keys | join(", ")' "$file")"
}

# Prints the chain id reported by the RPC, or "error" when the RPC cannot answer.
rpc_chain_id() {
    cast chain-id --rpc-url "$1" 2>/dev/null || printf 'error\n'
}

# Fails unless the RPC of the parsed target reports the network's committed chain id.
require_target_chain_id() {
    local actual

    actual="$(rpc_chain_id "$TARGET_RPC_URL")"
    [[ "$actual" == "$TARGET_EXPECTED_CHAIN_ID" ]] || \
        die "network $TARGET_NETWORK expects chain id $TARGET_EXPECTED_CHAIN_ID but the RPC reports $actual"
}
