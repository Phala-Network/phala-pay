//! Deterministic salt derivation and CREATE2 address prediction for forwarder clones.

use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_sol_types::SolValue;

/// Derives the salt of a customer's persistent deposit address:
/// `keccak256(abi.encode(account, livemode, client_reference_id, "deposit_address", version))`,
/// with the types `(string, bool, string, string, uint256)`.
///
/// `account` is the merchant's `acct_` id and `version` counts the customer's addresses from 1;
/// each rotation takes the next version, so a merchant recomputes every address it was ever given.
/// The salt names no chain and no asset: the factory and implementation have one address on every
/// chain, so the customer's forwarder is the same address on every chain whose treasury is the
/// same address, and takes every supported token there.
#[must_use]
pub fn deposit_address_salt(
    account: &str,
    livemode: bool,
    client_reference_id: &str,
    version: u64,
) -> B256 {
    keccak256(
        (
            account,
            livemode,
            client_reference_id,
            "deposit_address",
            U256::from(version),
        )
            .abi_encode_params()
            .as_slice(),
    )
}

/// Predicts the address of the forwarder clone that `factory` deploys for `treasury` and `salt`.
///
/// This is OpenZeppelin 5.x `Clones.predictDeterministicAddressWithImmutableArgs` with the clone
/// arguments `abi.encodePacked(treasury)`: the init code is the EIP-1167 proxy for
/// `implementation` preceded by a creation header and followed by the 20 treasury bytes, so the
/// address commits to the factory, the implementation, the treasury, and the salt.
#[must_use]
pub fn forwarder_address(
    factory: Address,
    implementation: Address,
    treasury: Address,
    salt: B256,
) -> Address {
    // `PUSH2 <runtime length>` then `RETURNDATASIZE DUP2 PUSH1 0x0a RETURNDATASIZE CODECOPY
    // RETURN`; the runtime is the 45-byte proxy followed by the arguments.
    const RUNTIME_LENGTH: [u8; 2] = [0x00, 0x2d + 20];
    const CREATION_SUFFIX: &[u8] = &alloy_primitives::hex!("3d81600a3d39f3");
    const RUNTIME_PREFIX: &[u8] = &alloy_primitives::hex!("363d3d373d3d3d363d73");
    const RUNTIME_SUFFIX: &[u8] = &alloy_primitives::hex!("5af43d82803e903d91602b57fd5bf3");

    let mut init_code = Vec::with_capacity(75);
    init_code.push(0x61);
    init_code.extend_from_slice(&RUNTIME_LENGTH);
    init_code.extend_from_slice(CREATION_SUFFIX);
    init_code.extend_from_slice(RUNTIME_PREFIX);
    init_code.extend_from_slice(implementation.as_slice());
    init_code.extend_from_slice(RUNTIME_SUFFIX);
    init_code.extend_from_slice(treasury.as_slice());

    factory.create2(salt, keccak256(init_code))
}
