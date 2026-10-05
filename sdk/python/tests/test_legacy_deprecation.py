"""Legacy configuration stays compatible throughout its deprecation period."""

from __future__ import annotations

import warnings
from typing import Any

import httpx
import pytest

from phala_pay import ConfigurationError, PhalaPay
from topup_sdk import UnpinnedTreasuryWarning

from ._support import KEY, QUOTE_ID, _quote, pins


@pytest.mark.parametrize("positional", [False, True])
def test_legacy_constructor_warns_and_retains_test_treasury_warning(positional: bool) -> None:
    args = ("https://service.test", KEY) if positional else (KEY,)
    options: dict[str, Any] = {} if positional else {"api_base": "https://service.test"}
    with pytest.warns(DeprecationWarning, match="removed in 0.10.0"):
        client = PhalaPay(
            *args,
            account=pins().account,
            forwarder=(pins().factory, pins().implementation),
            transport=httpx.MockTransport(lambda _: httpx.Response(200, json=_quote())),
            **options,
        )
    with client, pytest.warns(UnpinnedTreasuryWarning):
        assert client.quotes.retrieve(QUOTE_ID).id == QUOTE_ID


def test_pins_constructor_does_not_emit_legacy_warning() -> None:
    with warnings.catch_warnings(record=True) as emitted:
        warnings.simplefilter("always")
        with PhalaPay(KEY, pins=pins()):
            pass
    assert emitted == []


@pytest.mark.parametrize("option", ["account", "forwarder", "treasuries"])
def test_explicit_none_still_selects_legacy_configuration(option: str) -> None:
    options: dict[str, Any] = {option: None}
    with (
        pytest.warns(DeprecationWarning, match="legacy PhalaPay"),
        pytest.raises(ConfigurationError, match="cannot be combined"),
    ):
        PhalaPay(KEY, pins=pins(), **options)
    with (
        pytest.warns(DeprecationWarning, match="legacy PhalaPay"),
        PhalaPay(KEY, api_base="https://service.test", **options),
    ):
        pass
    with pytest.raises(ConfigurationError, match="pins is required"):
        PhalaPay(KEY, api_base="https://service.test")


def test_unknown_legacy_argument_remains_configuration_error() -> None:
    with (
        pytest.warns(DeprecationWarning, match="legacy PhalaPay"),
        pytest.raises(ConfigurationError, match="unknown constructor argument"),
    ):
        PhalaPay(KEY, api_base="https://service.test", unexpected=True)
