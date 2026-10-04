"""Keep secret values usable on the wire while making diagnostic representations safe."""

from __future__ import annotations

import re
from typing import Any, cast

import attrs

_PATTERN = re.compile(
    r"ppay_(?:sk|rk)_[A-Za-z0-9_+-]+|(?:qt|da)_[A-Za-z0-9]+_secret_[A-Za-z0-9_+-]+",
    re.IGNORECASE,
)


def redact(value: str) -> str:
    return _PATTERN.sub("[REDACTED]", value)


class Secret(str):
    def __repr__(self) -> str:
        return "'[REDACTED]'"


def protect[T](value: T, *, sensitive: bool = False) -> T:
    return cast(T, _protect(value, sensitive=sensitive))


def _protect(value: Any, *, sensitive: bool = False) -> Any:
    if isinstance(value, str):
        return Secret(value) if sensitive or _PATTERN.search(value) else value
    if isinstance(value, list):
        return [_protect(item) for item in value]
    if isinstance(value, dict):
        return {
            key: _protect(item, sensitive=key in {"secret", "client_secret", "token"})
            for key, item in value.items()
        }
    if attrs.has(type(value)):
        for field in attrs.fields(type(value)):
            setattr(
                value,
                field.name,
                _protect(
                    getattr(value, field.name),
                    sensitive=field.name in {"secret", "client_secret", "token"},
                ),
            )
    return value
