from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from topup_sdk.addresses import (
    deposit_address,
    deposit_address_salt,
    deposit_id,
    forwarder_address,
    same_address,
    to_checksum_address,
)

# Shared with contracts/ and crates/core/tests/create2_vectors.rs.
VECTORS: dict[str, Any] = json.loads(
    (Path(__file__).resolve().parents[3] / "contracts/test-vectors/create2.json").read_text(
        encoding="utf-8"
    )
)


@pytest.mark.parametrize(
    "vector",
    VECTORS["deposit_address"],
    ids=lambda vector: f"{vector['treasury'][:8]}-{vector['livemode']}-v{vector['version']}",
)
def test_deposit_address_salt_and_address_match_the_contract_vectors(
    vector: dict[str, Any],
) -> None:
    inputs = {
        "account": vector["account"],
        "livemode": vector["livemode"],
        "client_reference_id": vector["client_reference_id"],
        "version": vector["version"],
    }
    assert "0x" + deposit_address_salt(**inputs).hex() == vector["salt"]
    address = deposit_address(
        VECTORS["factory"], VECTORS["implementation"], vector["treasury"], **inputs
    )
    assert address == vector["predicted_address"]


def test_one_salt_gives_one_address_per_treasury() -> None:
    # The salt names no chain: the same inputs under the same treasury give one address on every
    # chain, and under another treasury another address.
    same_salt = [
        vector
        for vector in VECTORS["deposit_address"]
        if vector["salt"] == VECTORS["deposit_address"][0]["salt"]
    ]
    assert len({vector["treasury"] for vector in same_salt}) == 2
    assert len({vector["predicted_address"] for vector in same_salt}) == 2


@pytest.mark.parametrize("vector", VECTORS["forwarders"], ids=lambda vector: vector["salt"][:10])
def test_raw_salts_match_the_contract_vectors(vector: dict[str, Any]) -> None:
    address = forwarder_address(
        VECTORS["factory"],
        VECTORS["implementation"],
        vector["treasury"],
        bytes.fromhex(vector["salt"][2:]),
    )
    assert address == vector["predicted_address"]


def test_deposit_id_matches_the_core_vector() -> None:
    # crates/core/src/identity.rs::matches_python_uuid5_vector
    tx_hash = "0x0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef"
    assert deposit_id(1, tx_hash, 42) == "dep_20513a599b8053df983208749de3dcc5"
    with pytest.raises(ValueError, match="32 bytes"):
        deposit_id(1, "0x01", 42)


def test_checksum_and_comparison() -> None:
    lower = VECTORS["factory"].lower()
    assert to_checksum_address(lower) == VECTORS["factory"]
    assert same_address(lower, VECTORS["factory"])
    assert not same_address(lower, VECTORS["implementation"])
