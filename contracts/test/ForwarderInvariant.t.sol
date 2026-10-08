// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import { StdInvariant } from "forge-std/StdInvariant.sol";
import { Test } from "forge-std/Test.sol";

import { Forwarder } from "../src/Forwarder.sol";
import { ForwarderFactory } from "../src/ForwarderFactory.sol";
import { BlacklistToken } from "./mocks/MockTokens.sol";

/// Funds forwarders of two treasuries with a token and ETH, and lets arbitrary callers flush any
/// subset of salts under any treasury argument, including the wrong one.
contract ForwarderHandler is Test {
    uint256 public constant SALT_COUNT = 6;
    uint256 public constant TREASURY_COUNT = 2;

    ForwarderFactory public immutable factory;
    BlacklistToken public immutable token;

    address[TREASURY_COUNT] public treasuries;
    bytes32[SALT_COUNT] public salts;
    address[] public forwarders;
    mapping(address forwarder => address treasury) public treasuryOf;
    mapping(address forwarder => bool) public flushedWithBalance;

    /// Token and ETH sent into forwarders of each treasury.
    mapping(address treasury => uint256) public tokenFunded;
    mapping(address treasury => uint256) public ethFunded;

    constructor(ForwarderFactory factory_, BlacklistToken token_) {
        factory = factory_;
        token = token_;
        treasuries = [makeAddr("treasury-a"), makeAddr("treasury-b")];
        for (uint256 i; i < SALT_COUNT; ++i) {
            salts[i] = keccak256(abi.encode("invariant", i));
        }
        for (uint256 t; t < TREASURY_COUNT; ++t) {
            for (uint256 i; i < SALT_COUNT; ++i) {
                address forwarder = factory_.addressOf(treasuries[t], salts[i]);
                forwarders.push(forwarder);
                treasuryOf[forwarder] = treasuries[t];
            }
        }
    }

    function fundToken(uint256 seed, uint96 amount) external {
        address forwarder = forwarders[seed % forwarders.length];
        // The token refuses transfers to a blacklisted address, mints included.
        if (token.blacklisted(forwarder)) return;
        token.mint(forwarder, amount);
        tokenFunded[treasuryOf[forwarder]] += amount;
    }

    function fundEth(uint256 seed, uint96 amount) external {
        address forwarder = forwarders[seed % forwarders.length];
        vm.deal(forwarder, forwarder.balance + amount);
        ethFunded[treasuryOf[forwarder]] += amount;
    }

    /// Blocks or unblocks one forwarder as a token sender, so some flushes fail.
    function toggleBlacklist(uint256 seed) external {
        address forwarder = forwarders[seed % forwarders.length];
        token.setBlacklisted(forwarder, !token.blacklisted(forwarder));
    }

    /// Flushes a subset of salts chosen by `mask` for any non-zero treasury argument.
    function flush(
        address caller,
        uint256 treasurySeed,
        uint256 mask,
        bool native,
        bool anyTreasury
    ) external {
        // A forwarder address has no key: as a caller, the fuzzer would give it an account nonce
        // no real chain can, and CREATE2 refuses an address with a nonce.
        if (treasuryOf[caller] != address(0)) caller = address(0xCA11E2);
        address treasury = anyTreasury && address(uint160(treasurySeed)) != address(0)
            ? address(uint160(treasurySeed))
            : treasuries[treasurySeed % TREASURY_COUNT];
        bytes32[] memory batch = new bytes32[](SALT_COUNT);
        uint256 count;
        for (uint256 i; i < SALT_COUNT; ++i) {
            if (mask & (1 << i) != 0) batch[count++] = salts[i];
        }
        assembly ("memory-safe") {
            mstore(batch, count)
        }
        for (uint256 i; i < count; ++i) {
            address forwarder = factory.addressOf(treasury, batch[i]);
            uint256 balance = native ? forwarder.balance : token.balanceOf(forwarder);
            if (balance != 0) flushedWithBalance[forwarder] = true;
        }
        vm.prank(caller);
        factory.flush(treasury, batch, native ? address(0) : address(token));
    }

    function forwarderCount() external view returns (uint256) {
        return forwarders.length;
    }
}

contract ForwarderInvariantTest is StdInvariant, Test {
    ForwarderFactory private factory;
    BlacklistToken private token;
    ForwarderHandler private handler;

    function setUp() public {
        factory = new ForwarderFactory();
        token = new BlacklistToken();
        handler = new ForwarderHandler(factory, token);

        bytes4[] memory selectors = new bytes4[](4);
        selectors[0] = ForwarderHandler.fundToken.selector;
        selectors[1] = ForwarderHandler.fundEth.selector;
        selectors[2] = ForwarderHandler.toggleBlacklist.selector;
        selectors[3] = ForwarderHandler.flush.selector;
        targetSelector(FuzzSelector({ addr: address(handler), selectors: selectors }));
        targetContract(address(handler));
        for (uint256 i; i < handler.forwarderCount(); ++i) {
            excludeSender(handler.forwarders(i));
        }
    }

    function test_FlushWithNonZeroTreasurySeedThatTruncatesToZero() public {
        // Replay the shrunk CI sequence: a non-zero seed can still truncate to the zero address.
        uint256 treasurySeed = 0xd7bb818300000000000000000000000000000000000000000000000000000000;
        vm.prank(0x8BC840e877f6A1Ae3B4dC657e32956A375DA0d13);
        handler.flush(address(0x0400), treasurySeed, 669, false, true);
    }

    /// Every unit funded into a treasury's forwarders is either still in them or at that
    /// treasury: funds reach no other address, including another treasury or the caller.
    function invariant_FundsReachOnlyTheForwardersOwnTreasury() public view {
        for (uint256 t; t < handler.TREASURY_COUNT(); ++t) {
            address treasury = handler.treasuries(t);
            uint256 tokenHeld;
            uint256 ethHeld;
            for (uint256 i; i < handler.forwarderCount(); ++i) {
                address forwarder = handler.forwarders(i);
                if (handler.treasuryOf(forwarder) != treasury) continue;
                tokenHeld += token.balanceOf(forwarder);
                ethHeld += forwarder.balance;
            }
            assertEq(tokenHeld + token.balanceOf(treasury), handler.tokenFunded(treasury));
            assertEq(ethHeld + treasury.balance, handler.ethFunded(treasury));
        }
        assertEq(
            token.totalSupply(),
            handler.tokenFunded(handler.treasuries(0)) + handler.tokenFunded(handler.treasuries(1))
        );
    }

    /// A deployed forwarder is the clone `addressOf` predicted, bound to its own treasury, and
    /// was deployed only by a flush that found a balance in it.
    function invariant_DeployedForwardersMatchPredictionAndHeldFunds() public view {
        for (uint256 i; i < handler.forwarderCount(); ++i) {
            address forwarder = handler.forwarders(i);
            if (forwarder.code.length == 0) continue;
            assertEq(Forwarder(payable(forwarder)).treasury(), handler.treasuryOf(forwarder));
            assertEq(Forwarder(payable(forwarder)).factory(), address(factory));
            assertTrue(handler.flushedWithBalance(forwarder));
        }
    }
}
