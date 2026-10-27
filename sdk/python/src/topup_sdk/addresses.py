"""Deterministic deposit identities and CREATE2 forwarder addresses.

Products recompute every address the service hands out from the returned salt inputs, and
recompute every deposit id from its chain evidence, so neither has to be trusted blindly.
"""

from __future__ import annotations

import uuid

from Crypto.Hash import keccak

from .ids import DEPOSIT, object_id

DEPOSIT_NAMESPACE = uuid.UUID("d55bab89-f656-5796-a6a2-bddfa1dd9631")

# OpenZeppelin 5.x `Clones` with immutable arguments: `PUSH2 <runtime length>`, a creation
# header, the 45-byte EIP-1167 proxy, then the arguments (here the 20 treasury bytes).
_CLONE_RUNTIME_LENGTH = (0x2D + 20).to_bytes(2, "big")
_CLONE_CREATION_SUFFIX = bytes.fromhex("3d81600a3d39f3")
_CLONE_RUNTIME_PREFIX = bytes.fromhex("363d3d373d3d3d363d73")
_CLONE_RUNTIME_SUFFIX = bytes.fromhex("5af43d82803e903d91602b57fd5bf3")


def keccak256(data: bytes) -> bytes:
    """Returns the Ethereum Keccak-256 digest."""
    return bytes(keccak.new(digest_bits=256, data=data).digest())


def deposit_id(chain_id: int, tx_hash: str, receipt_log_index: int) -> str:
    """Returns a transfer's deposit id, `dep_` and the hex of
    UUIDv5(NS, `{chain_id}:{lowercase tx_hash}:{receipt_log_index}`), where `receipt_log_index`
    is the transfer's position among the logs of its transaction's receipt (0 for a plain token
    transfer). The id survives the transaction's re-inclusion in another block."""
    tx_hash = tx_hash.lower()
    if len(_hex_bytes(tx_hash)) != 32:
        raise ValueError("tx_hash must be 32 bytes")
    name = f"{chain_id}:{tx_hash}:{receipt_log_index}"
    return object_id(DEPOSIT, uuid.uuid5(DEPOSIT_NAMESPACE, name))


def deposit_address_salt(
    account: str,
    *,
    livemode: bool,
    client_reference_id: str,
    version: int,
) -> bytes:
    """keccak256(abi.encode(account, livemode, client_reference_id, "deposit_address", version)),
    with the types (string, bool, string, string, uint256): the salt of a customer's deposit
    address, where `account` is your `acct_` id and `version` the address's `version`. It names no
    chain or asset: the address is the same on every chain whose treasury is the same."""
    return keccak256(
        _abi_encode(account, livemode, client_reference_id, "deposit_address", version)
    )


def deposit_address(
    factory: str,
    implementation: str,
    treasury: str,
    *,
    account: str,
    livemode: bool,
    client_reference_id: str,
    version: int,
) -> str:
    """Recomputes a deposit address offline from the pinned forwarder, the treasury of the
    network (chain) it pays, and its salt inputs; every version a customer was ever given can be
    derived. Pass each network's `treasury`: a chain whose treasury differs has its own address."""
    salt = deposit_address_salt(
        account,
        livemode=livemode,
        client_reference_id=client_reference_id,
        version=version,
    )
    return forwarder_address(factory, implementation, treasury, salt)


def forwarder_address(factory: str, implementation: str, treasury: str, salt: bytes) -> str:
    """Predicts the forwarder `factory` deploys for `treasury` and `salt`: an EIP-1167 clone of
    `implementation` whose only immutable argument is the treasury, so the address commits to
    all four inputs (OpenZeppelin `Clones.predictDeterministicAddressWithImmutableArgs`)."""
    if len(salt) != 32:
        raise ValueError("salt must be 32 bytes")
    init_code = (
        b"\x61"
        + _CLONE_RUNTIME_LENGTH
        + _CLONE_CREATION_SUFFIX
        + _CLONE_RUNTIME_PREFIX
        + _address_bytes(implementation)
        + _CLONE_RUNTIME_SUFFIX
        + _address_bytes(treasury)
    )
    preimage = b"\xff" + _address_bytes(factory) + salt + keccak256(init_code)
    return to_checksum_address(keccak256(preimage)[12:])


def to_checksum_address(address: bytes | str) -> str:
    """Formats a 20-byte address with its EIP-55 checksum."""
    raw = address if isinstance(address, bytes) else _address_bytes(address)
    if len(raw) != 20:
        raise ValueError("address must be 20 bytes")
    lower = raw.hex()
    digest = keccak256(lower.encode("ascii")).hex()
    return "0x" + "".join(
        char.upper() if int(nibble, 16) >= 8 else char
        for char, nibble in zip(lower, digest[: len(lower)], strict=True)
    )


def same_address(left: str, right: str) -> bool:
    """Compares two EVM addresses byte-wise."""
    return _address_bytes(left) == _address_bytes(right)


def _address_bytes(address: str) -> bytes:
    raw = _hex_bytes(address)
    if len(raw) != 20:
        raise ValueError("address must be 20 bytes")
    return raw


def _hex_bytes(value: str) -> bytes:
    if not value.startswith(("0x", "0X")):
        raise ValueError("expected 0x-prefixed hexadecimal")
    return bytes.fromhex(value[2:])


def _abi_encode(*fields: str | bool | int) -> bytes:
    """ABI-encodes a tuple of `string`, `bool`, and `uint256` fields, as `abi.encode` does."""
    head = b""
    tail = b""
    offset = 32 * len(fields)
    for field in fields:
        if isinstance(field, bool):
            head += int(field).to_bytes(32, "big")
        elif isinstance(field, int):
            if not 0 <= field < 2**256:
                raise ValueError("uint256 out of range")
            head += field.to_bytes(32, "big")
        else:
            encoded = field.encode("utf-8")
            head += (offset + len(tail)).to_bytes(32, "big")
            tail += len(encoded).to_bytes(32, "big") + encoded + b"\x00" * (-len(encoded) % 32)
    return head + tail
