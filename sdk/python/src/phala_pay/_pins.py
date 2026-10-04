from __future__ import annotations

import base64
import binascii
import json
import re
from collections.abc import Mapping
from dataclasses import dataclass
from typing import Any


class PinsError(ValueError):
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
        object.__setattr__(self, "treasuries", dict(self.treasuries))


def _address(value: Any, name: str) -> str:
    if not isinstance(value, str) or not re.fullmatch(r"0x[0-9a-fA-F]{40}", value):
        raise PinsError(f"{name} must be a 20-byte hex address")
    return value.lower()


def parse_pins(value: str) -> Pins:  # noqa: PLR0912, PLR0915
    if not isinstance(value, str) or not value.startswith("ppay_pins_v1."):
        raise PinsError("pins must use ppay_pins_v1 encoding")
    encoded = value.split(".", 1)[1]
    if not encoded or "=" in encoded or not re.fullmatch(r"[A-Za-z0-9_-]+", encoded):
        raise PinsError("pins payload must be unpadded base64url")
    if len(encoded) % 4 == 1:
        raise PinsError("invalid pins encoding")
    try:
        raw = base64.urlsafe_b64decode(encoded + "=" * (-len(encoded) % 4))
        if base64.urlsafe_b64encode(raw).decode().rstrip("=") != encoded:
            raise PinsError("non-canonical pins encoding")
    except (ValueError, binascii.Error) as exc:
        raise PinsError("invalid pins encoding") from exc
    if len(raw) > 16 * 1024:
        raise PinsError("pins payload exceeds 16 KiB")
    try:

        def pairs(items: list[tuple[str, Any]]) -> dict[str, Any]:
            result: dict[str, Any] = {}
            for key, item in items:
                if key in result:
                    raise PinsError("duplicate pins field")
                result[key] = item
            return result

        obj = json.loads(raw.decode("utf-8"), object_pairs_hook=pairs)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise PinsError("pins payload is not UTF-8 JSON") from exc
    if not isinstance(obj, dict):
        raise PinsError("pins payload must be an object")
    required = {
        "api_base",
        "account",
        "livemode",
        "factory",
        "implementation",
        "treasuries",
        "webhook_keys",
    }
    if set(obj) != required:
        raise PinsError("pins fields are missing or unknown")
    account = obj["account"]
    if not isinstance(account, str) or not re.fullmatch(r"acct_[0-9a-f]{32}", account):
        raise PinsError("invalid account")
    if not isinstance(obj["livemode"], bool):
        raise PinsError("livemode must be boolean")
    api_base = obj["api_base"]
    if not isinstance(api_base, str) or not re.fullmatch(r"https://[^/?#]+", api_base):
        raise PinsError("api_base must be an HTTPS origin")
    api_base = api_base.rstrip("/").lower()
    treasuries_obj = obj["treasuries"]
    if not isinstance(treasuries_obj, dict) or not treasuries_obj:
        raise PinsError("treasuries must be nonempty")
    treasuries: dict[int, str] = {}
    for chain, address in treasuries_obj.items():
        if not isinstance(chain, str) or not re.fullmatch(r"[1-9][0-9]*", chain):
            raise PinsError("invalid treasury chain id")
        treasuries[int(chain)] = _address(address, "treasury")
    keys_obj = obj["webhook_keys"]
    if not isinstance(keys_obj, list) or not keys_obj:
        raise PinsError("webhook_keys must be nonempty")
    keys: list[tuple[int, str]] = []
    versions: set[int] = set()
    for item in keys_obj:
        if not isinstance(item, dict) or set(item) != {"version", "public_key"}:
            raise PinsError("invalid webhook key")
        version, key = item["version"], item["public_key"]
        if type(version) is not int or version <= 0 or version > 2**32 - 1 or version in versions:
            raise PinsError("invalid webhook key version")
        if not isinstance(key, str) or not key.startswith("whpk_"):
            raise PinsError("invalid webhook public key")
        try:
            decoded = base64.b64decode(key[5:], validate=True)
        except (ValueError, binascii.Error) as exc:
            raise PinsError("invalid webhook public key") from exc
        if len(decoded) != 32:
            raise PinsError("invalid webhook public key")
        versions.add(version)
        keys.append((version, key))
    return Pins(
        api_base,
        account,
        obj["livemode"],
        _address(obj["factory"], "factory"),
        _address(obj["implementation"], "implementation"),
        treasuries,
        tuple(sorted(keys)),
    )


def encode_pins(pins: Pins) -> str:
    if not isinstance(pins, Pins):
        raise TypeError("encode_pins expects Pins")
    obj = {
        "account": pins.account,
        "api_base": pins.api_base,
        "factory": pins.factory,
        "implementation": pins.implementation,
        "livemode": pins.livemode,
        "treasuries": {str(k): v.lower() for k, v in sorted(pins.treasuries.items())},
        "webhook_keys": [{"public_key": k, "version": v} for v, k in pins.webhook_keys],
    }
    raw = json.dumps(obj, separators=(",", ":"), sort_keys=True).encode()
    return "ppay_pins_v1." + base64.urlsafe_b64encode(raw).decode().rstrip("=")
