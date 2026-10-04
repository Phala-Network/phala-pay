"""Canonical, immutable deployment trust pins."""

from __future__ import annotations

import base64
import binascii
import json
import re
import zlib
from collections.abc import Mapping
from dataclasses import dataclass
from types import MappingProxyType
from typing import Any

from topup_sdk._origin import normalize_origin
from topup_sdk.errors import ConfigurationError

MAX_CHAIN = 2**53 - 1
MAX_BYTES = 16 * 1024


class PinsError(ConfigurationError):
    pass


@dataclass(frozen=True)
class Pins:
    api_base: str
    account: str
    livemode: bool
    factory: str
    implementation: str
    treasuries: Mapping[int, str]
    webhook_keys: tuple[tuple[int, str], ...]

    def __post_init__(self) -> None:
        if not isinstance(self.account, str) or not re.fullmatch(
            r"acct_[0-9a-f]{32}", self.account
        ):
            raise PinsError("invalid account")
        if type(self.livemode) is not bool:
            raise PinsError("livemode must be boolean")
        object.__setattr__(
            self, "api_base", normalize_origin(self.api_base, test=not self.livemode)
        )
        object.__setattr__(self, "factory", _address(self.factory))
        object.__setattr__(self, "implementation", _address(self.implementation))
        if not isinstance(self.treasuries, Mapping) or not self.treasuries:
            raise PinsError("treasuries must be nonempty")
        treasuries: dict[int, str] = {}
        for chain, address in self.treasuries.items():
            if type(chain) is not int or not 1 <= chain <= MAX_CHAIN:
                raise PinsError("invalid treasury chain id")
            treasuries[chain] = _address(address)
        object.__setattr__(self, "treasuries", MappingProxyType(treasuries))
        if not self.webhook_keys:
            raise PinsError("webhook_keys must be nonempty")
        versions: set[int] = set()
        public_keys: set[str] = set()
        for version, key in self.webhook_keys:
            if type(version) is not int or not 1 <= version <= 2**32 - 1 or version in versions:
                raise PinsError("invalid webhook key version")
            if not isinstance(key, str) or not key.startswith("whpk_"):
                raise PinsError("invalid webhook public key")
            try:
                raw = base64.b64decode(key[5:], validate=True)
            except (ValueError, binascii.Error):
                raise PinsError("invalid webhook public key") from None
            if len(raw) != 32 or base64.b64encode(raw).decode() != key[5:] or key in public_keys:
                raise PinsError("invalid or duplicate webhook public key")
            versions.add(version)
            public_keys.add(key)
        object.__setattr__(self, "webhook_keys", tuple(sorted(self.webhook_keys)))


def _address(value: Any) -> str:
    if (
        not isinstance(value, str)
        or not re.fullmatch(r"0x[0-9a-fA-F]{40}", value)
        or int(value, 16) == 0
    ):
        raise PinsError("contract and treasury addresses must be nonzero 20-byte hex")
    return value.lower()


def parse_pins(value: str) -> Pins:
    if not isinstance(value, str) or not value.startswith("ppay_pins_v1."):
        raise PinsError("pins must use ppay_pins_v1 encoding")
    encoded = value.split(".", 1)[1]
    if len(encoded) > (MAX_BYTES * 4 + 2) // 3:
        raise PinsError("pins payload exceeds 16 KiB")
    if not encoded or not re.fullmatch(r"[A-Za-z0-9_-]+", encoded) or len(encoded) % 4 == 1:
        raise PinsError("pins payload must be canonical unpadded base64url")
    try:
        raw = base64.urlsafe_b64decode(encoded + "=" * (-len(encoded) % 4))
        if len(raw) > MAX_BYTES or base64.urlsafe_b64encode(raw).decode().rstrip("=") != encoded:
            raise PinsError("non-canonical or oversized pins encoding")
        obj = json.loads(raw.decode("utf-8"), object_pairs_hook=_pairs)
    except (UnicodeDecodeError, ValueError, binascii.Error):
        raise PinsError("invalid pins encoding or JSON") from None
    required = {
        "api_base",
        "account",
        "livemode",
        "factory",
        "implementation",
        "treasuries",
        "webhook_keys",
    }
    if not isinstance(obj, dict) or set(obj) != required:
        raise PinsError("pins fields are missing or unknown")
    chains = obj["treasuries"]
    if not isinstance(chains, dict) or any(not re.fullmatch(r"[1-9][0-9]*", c) for c in chains):
        raise PinsError("invalid treasury chain id")
    keys = obj["webhook_keys"]
    if not isinstance(keys, list) or any(
        not isinstance(k, dict) or set(k) != {"version", "public_key"} for k in keys
    ):
        raise PinsError("invalid webhook keys")
    return Pins(
        obj["api_base"],
        obj["account"],
        obj["livemode"],
        obj["factory"],
        obj["implementation"],
        {int(c): a for c, a in chains.items()},
        tuple((k["version"], k["public_key"]) for k in keys),
    )


def _pairs(items: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, item in items:
        if key in result:
            raise PinsError("duplicate pins field")
        result[key] = item
    return result


def encode_pins(pins: Pins) -> str:
    if not isinstance(pins, Pins):
        raise TypeError("encode_pins expects Pins")
    obj = {
        "account": pins.account,
        "api_base": pins.api_base,
        "factory": pins.factory,
        "implementation": pins.implementation,
        "livemode": pins.livemode,
        "treasuries": {str(k): v for k, v in pins.treasuries.items()},
        "webhook_keys": [{"public_key": k, "version": v} for v, k in pins.webhook_keys],
    }
    raw = json.dumps(obj, separators=(",", ":"), sort_keys=True).encode()
    if len(raw) > MAX_BYTES:
        raise PinsError("pins payload exceeds 16 KiB")
    return "ppay_pins_v1." + base64.urlsafe_b64encode(raw).decode().rstrip("=")


def key_livemode(key: str) -> bool:
    """Match the service's base62 CRC32 contract (crates/topup/src/api_keys.rs)."""
    if not isinstance(key, str) or not re.fullmatch(
        r"ppay_(?:sk|rk)_(?:test|live)_[A-Za-z0-9]{49}", key
    ):
        raise ConfigurationError("invalid API key format")
    body, checksum = key[:-6], key[-6:]
    value = zlib.crc32(body.encode())
    alphabet = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
    digits = ""
    for _ in range(6):
        value, digit = divmod(value, 62)
        digits = alphabet[digit] + digits
    if checksum != digits:
        raise ConfigurationError("invalid API key checksum")
    return "_live_" in body
