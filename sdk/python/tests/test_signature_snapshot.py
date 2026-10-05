"""Committed public parameter contracts; annotations are deliberately excluded."""

from __future__ import annotations

import inspect
import json
from pathlib import Path
from typing import Any

from phala_pay import _client
from topup_client.types import UNSET
from topup_sdk.client import TopupClient

SNAPSHOT = Path(__file__).with_name("signature_snapshot.json")


def public_signatures() -> dict[str, dict[str, list[dict[str, str]]]]:
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
            parameters = []
            for parameter in inspect.signature(member).parameters.values():
                default: Any = parameter.default
                if default is inspect.Parameter.empty:
                    encoded = "<required>"
                elif default is UNSET:
                    encoded = "UNSET"
                elif callable(default):
                    encoded = f"{default.__module__}.{default.__qualname__}"
                else:
                    encoded = repr(default)
                parameters.append(
                    {"name": parameter.name, "kind": parameter.kind.name, "default": encoded}
                )
            methods[method] = parameters
        result[name] = methods
    return result


def test_public_signature_snapshot() -> None:
    assert public_signatures() == json.loads(SNAPSHOT.read_text())
