#!/usr/bin/env bash
# Shared hermetic price fixtures for the CVM rehearsal and restore drill.
# Source deploy/contracts/common.sh first for CONTRACTS_DIR and the Anvil helpers.

anvil_price_set_code() {
    local rpc=$1 address=$2 code=$3
    cast rpc --rpc-url "$rpc" anvil_setCode "$address" "$code" >/dev/null
}
anvil_price_set_storage() {
    local rpc=$1 address=$2 slot=$3 value=$4
    cast rpc --rpc-url "$rpc" anvil_setStorageAt "$address" \
        "$(python3 - "$slot" <<'PY'
import sys
print(f"0x{int(sys.argv[1], 0):064x}")
PY
)" "$(python3 - "$value" <<'PY'
import sys
print(f"0x{int(sys.argv[1], 0):064x}")
PY
)" >/dev/null
}

# Usage: install_anvil_price_fixtures MAINNET_RPC_URL BASE_MAINNET_RPC_URL
# Exposes ANVIL_PRICE_PAIR_TIMESTAMP for the rehearsal's compressed TWAP history.
install_anvil_price_fixtures() {
    local mainnet_price_rpc_url=$1 base_mainnet_price_rpc_url=$2
    local fixture_aggregator_code fixture_pair_code
    local eth_feed usdc_feed usdt_feed sequencer_feed pair feed_address
    local price_timestamp base_price_timestamp pair_timestamp pair_packed

    install_anvil_multicall3 "$mainnet_price_rpc_url"
    install_anvil_multicall3 "$base_mainnet_price_rpc_url"
    fixture_aggregator_code=$(cd "$CONTRACTS_DIR" && forge inspect test/mocks/PriceFixtures.sol:MockPriceAggregator deployedBytecode)
    fixture_pair_code=$(cd "$CONTRACTS_DIR" && forge inspect test/mocks/PriceFixtures.sol:MockUniswapV2Pair deployedBytecode)
    eth_feed=0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419
    usdc_feed=0x8fFfFfd4AfB6115b954Bd326cbe7B4BA576818f6
    usdt_feed=0x3E7d1eAB13ad0104d2750B8863b489D65364e32D
    sequencer_feed=0xBCF85224fc0756B9Fa45aA7892530B47e10b6433
    pair=0x8867f20c1c63baccec7617626254a060eeb0e61e
    for feed_address in "$eth_feed" "$usdc_feed" "$usdt_feed"; do
        anvil_price_set_code "$mainnet_price_rpc_url" "$feed_address" "$fixture_aggregator_code"
    done
    anvil_price_set_code "$mainnet_price_rpc_url" "$pair" "$fixture_pair_code"
    anvil_price_set_code "$base_mainnet_price_rpc_url" "$sequencer_feed" "$fixture_aggregator_code"
    # MockPriceAggregator slots: decimals, answer, roundId, startedAt, updatedAt, answeredInRound.
    anvil_price_set_storage "$mainnet_price_rpc_url" "$eth_feed" 0 8
    anvil_price_set_storage "$mainnet_price_rpc_url" "$eth_feed" 1 200000000000
    anvil_price_set_storage "$mainnet_price_rpc_url" "$eth_feed" 2 1
    price_timestamp=$(cast block latest --field timestamp --rpc-url "$mainnet_price_rpc_url")
    price_timestamp=$(python3 - "$price_timestamp" <<'PY'
import sys
print(int(sys.argv[1], 0))
PY
    )
    anvil_price_set_storage "$mainnet_price_rpc_url" "$eth_feed" 3 "$price_timestamp"
    anvil_price_set_storage "$mainnet_price_rpc_url" "$eth_feed" 4 "$price_timestamp"
    anvil_price_set_storage "$mainnet_price_rpc_url" "$eth_feed" 5 1
    for feed_address in "$usdc_feed" "$usdt_feed"; do
        anvil_price_set_storage "$mainnet_price_rpc_url" "$feed_address" 0 8
        anvil_price_set_storage "$mainnet_price_rpc_url" "$feed_address" 1 100000000
        anvil_price_set_storage "$mainnet_price_rpc_url" "$feed_address" 2 1
        anvil_price_set_storage "$mainnet_price_rpc_url" "$feed_address" 3 "$price_timestamp"
        anvil_price_set_storage "$mainnet_price_rpc_url" "$feed_address" 4 "$price_timestamp"
        anvil_price_set_storage "$mainnet_price_rpc_url" "$feed_address" 5 1
    done
    anvil_price_set_storage "$base_mainnet_price_rpc_url" "$sequencer_feed" 0 0
    anvil_price_set_storage "$base_mainnet_price_rpc_url" "$sequencer_feed" 1 0
    anvil_price_set_storage "$base_mainnet_price_rpc_url" "$sequencer_feed" 2 1
    base_price_timestamp=$(cast block latest --field timestamp --rpc-url "$base_mainnet_price_rpc_url")
    base_price_timestamp=$(python3 - "$base_price_timestamp" <<'PY'
import sys
print(int(sys.argv[1], 0))
PY
    )
    anvil_price_set_storage "$base_mainnet_price_rpc_url" "$sequencer_feed" 3 "$((base_price_timestamp - 7200))"
    anvil_price_set_storage "$base_mainnet_price_rpc_url" "$sequencer_feed" 4 "$base_price_timestamp"
    anvil_price_set_storage "$base_mainnet_price_rpc_url" "$sequencer_feed" 5 1
    # MockUniswapV2Pair slots: token0, token1, packed reserves/timestamp, cumulative0, cumulative1.
    anvil_price_set_storage "$mainnet_price_rpc_url" "$pair" 0 0x6c5ba91642f10282b576d91922ae6448c9d52f4e
    anvil_price_set_storage "$mainnet_price_rpc_url" "$pair" 1 0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2
    pair_timestamp=$(cast block latest --field timestamp --rpc-url "$mainnet_price_rpc_url")
    pair_timestamp=$(python3 - "$pair_timestamp" <<'PY'
import sys
print(int(sys.argv[1], 0))
PY
    )
    pair_packed=$(python3 - "$pair_timestamp" <<'PY'
import sys
timestamp = int(sys.argv[1]) - 1800
reserve0 = 100_000 * 10**18
reserve1 = 100 * 10**18
print((reserve0 | (reserve1 << 112) | ((timestamp & 0xffffffff) << 224)))
PY
    )
    anvil_price_set_storage "$mainnet_price_rpc_url" "$pair" 2 "$pair_packed"
    anvil_price_set_storage "$mainnet_price_rpc_url" "$pair" 3 0
    anvil_price_set_storage "$mainnet_price_rpc_url" "$pair" 4 0
    # shellcheck disable=SC2034 # Read by the sourcing CVM rehearsal's TWAP setup.
    ANVIL_PRICE_PAIR_TIMESTAMP=$pair_timestamp
}
