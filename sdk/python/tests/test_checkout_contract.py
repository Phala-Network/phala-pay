"""Keep Python's browser handoff aligned with the committed JS public contract."""

import json
import re
from pathlib import Path

import httpx
import pytest

from phala_pay import ResponseValidationError

from .test_ergonomics_transport import SECRET, pay
from .test_phala_pay import _quote


def test_checkout_params_matches_js_checkout_params_contract() -> None:
    source = (Path(__file__).resolve().parents[2] / "js/src/checkout-params.ts").read_text()
    interface = re.search(r"export interface CheckoutParams \{([^}]+)\}", source)
    assert interface is not None
    fields = {}
    for declaration in interface[1].strip().splitlines():
        field = re.fullmatch(r"\s*(\w+):\s*(\w+);\s*", declaration)
        assert field is not None, declaration
        fields[field[1]] = field[2]
    assert fields == {"clientSecret": "string", "expectedAddress": "string", "apiBase": "string"}

    with pay(lambda _: httpx.Response(200, json=_quote(client_secret=SECRET))) as client:
        quote = client.quotes.create(
            client_reference_id="team-42", amount=2500, chain_id=11155111, asset="pha"
        )
        checkout = client.checkout_params(quote)
        assert set(checkout) == set(fields)
        assert all(isinstance(value, str) for value in checkout.values())
        assert json.loads(json.dumps(checkout)) == {
            "clientSecret": SECRET,
            "expectedAddress": quote.address,
            "apiBase": client.pins.api_base,
        }


def test_checkout_params_rejects_secret_replaced_after_creation() -> None:
    with pay(lambda _: httpx.Response(200, json=_quote(client_secret=SECRET))) as client:
        quote = client.quotes.create(
            client_reference_id="team-42", amount=2500, chain_id=11155111, asset="pha"
        )
        quote.client_secret = SECRET + "changed"
        with pytest.raises(ResponseValidationError):
            client.checkout_params(quote)


def test_checkout_params_never_authorizes_missing_secret_by_later_assignment() -> None:
    with pay(lambda _: httpx.Response(200, json=_quote())) as client:
        quote = client.quotes.create(
            client_reference_id="team-42", amount=2500, chain_id=11155111, asset="pha"
        )
        quote.client_secret = SECRET
        with pytest.raises(ResponseValidationError):
            client.checkout_params(quote)
