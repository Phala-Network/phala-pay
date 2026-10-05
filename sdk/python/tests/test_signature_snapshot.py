"""Committed public parameter contracts; annotations are deliberately excluded.

Regenerate from sdk/python with:
    uv run --locked python -m tests.test_signature_snapshot

Callable defaults and UNSET use stable names instead of process-specific object addresses.
"""

from __future__ import annotations

import inspect
import json
from pathlib import Path

from phala_pay import _client
from topup_client.types import UNSET
from topup_sdk.client import TopupClient

SNAPSHOT = Path(__file__).with_name("signature_snapshot.json")


class _DefaultRepr:
    def __init__(self, value: str) -> None:
        self.value = value

    def __repr__(self) -> str:
        return self.value


def public_signatures() -> dict[str, dict[str, str]]:
    classes = {
        name: cls
        for name, cls in vars(_client).items()
        if inspect.isclass(cls) and cls.__module__ == _client.__name__ and not issubclass(cls, dict)
    }
    classes["TopupClient"] = TopupClient
    result = {}
    for name, cls in sorted(classes.items()):
        methods = {}
        for method, member in inspect.getmembers(cls, callable):
            if method.startswith("_") and method not in {"__init__", "__enter__", "__exit__"}:
                continue
            signature = inspect.signature(member)
            parameters = []
            for parameter in signature.parameters.values():
                default = parameter.default
                if default is UNSET:
                    default = _DefaultRepr("UNSET")
                elif default is not inspect.Parameter.empty and callable(default):
                    default = _DefaultRepr(f"{default.__module__}.{default.__qualname__}")
                parameters.append(
                    parameter.replace(annotation=inspect.Parameter.empty, default=default)
                )
            methods[method] = str(
                signature.replace(parameters=parameters, return_annotation=inspect.Signature.empty)
            )
        result[name] = methods
    return result


def test_public_signature_snapshot() -> None:
    assert public_signatures() == json.loads(SNAPSHOT.read_text())


if __name__ == "__main__":
    SNAPSHOT.write_text(json.dumps(public_signatures(), indent=2) + "\n")
