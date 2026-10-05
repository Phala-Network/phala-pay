// SPDX-License-Identifier: MIT
pragma solidity 0.8.37;

/// Minimal immutable-layout fixtures used only by the hermetic CVM rehearsal.
contract MockPriceAggregator {
    uint8 public decimals;
    int256 public answer;
    uint80 public roundId;
    uint256 public startedAt;
    uint256 public updatedAt;
    uint80 public answeredInRound;

    function latestRoundData() external view returns (uint80, int256, uint256, uint256, uint80) {
        return (roundId, answer, startedAt, updatedAt, answeredInRound);
    }
}

/// Minimal Uniswap V2 pair state used to exercise the pinned local price path.
contract MockUniswapV2Pair {
    address public token0;
    address public token1;
    uint112 public reserve0;
    uint112 public reserve1;
    uint32 public blockTimestampLast;
    uint256 public price0CumulativeLast;
    uint256 public price1CumulativeLast;

    function getReserves() external view returns (uint112, uint112, uint32) {
        return (reserve0, reserve1, blockTimestampLast);
    }
}
