"""Trust, webhook and ledger boundary rules; shared fixture data is read-only."""

from __future__ import annotations

import base64
import json
import time
from dataclasses import FrozenInstanceError, replace
from itertools import permutations
from typing import Any, cast

import httpx
import pytest

from phala_pay import (
    AddressMismatchError,
    ConfigurationError,
    LedgerSnapshotError,
    PhalaPay,
    ResponseValidationError,
    SignatureVerificationError,
    balance_delta,
    deposit_net_amount,
    encode_pins,
    parse_pins,
)
from phala_pay._client import _BoundWebhook
from topup_sdk import sign_webhook

from ._support import (
    ACCOUNT,
    EVENT_ID,
    KEY,
    SECRET,
    SERVICE_KEY,
    SERVICE_PUBLIC_KEY,
    _delivery,
    _deposit,
    _deposit_address,
    _quote,
    load,
    pay,
    pins,
    valid_key,
)


def encoded(value: Any) -> str:
    raw = value if isinstance(value, bytes) else json.dumps(value).encode()
    return "ppay_pins_v1." + base64.urlsafe_b64encode(raw).decode().rstrip("=")


def pin_document() -> dict[str, Any]:
    document = json.loads(base64.urlsafe_b64decode(encode_pins(pins()).split(".")[1] + "=="))
    assert isinstance(document, dict)
    return document


@pytest.mark.parametrize(
    "value",
    [
        "ppay_pins_v2.e30",
        "ppay_pins_v1.!",
        "ppay_pins_v1.e30=",
        "ppay_pins_v1.e31",
        "ppay_pins_v1.A",
        "ppay_pins_v1._w",
        encoded([]),
        encoded(b'{"account":1,"account":2}'),
        encoded(b" " * (16 * 1024 + 1)),
    ],
)
def test_pins_envelope_rejects_format_padding_unused_bits_utf8_duplicates_and_size(
    value: str,
) -> None:
    with pytest.raises(ConfigurationError):
        parse_pins(value)


def test_pins_accept_json_key_order_and_exact_size_boundary() -> None:
    raw = json.dumps(dict(reversed(list(pin_document().items())))).encode()
    assert encode_pins(parse_pins(encoded(raw))) == encode_pins(pins())
    assert parse_pins(encoded(raw + b" " * (16 * 1024 - len(raw)))) == pins()


@pytest.mark.parametrize(
    ("field", "value"),
    [
        ("account", "acct_" + "AA" * 16),
        ("account", "acct_123"),
        ("livemode", 0),
        ("factory", "0x" + "00" * 20),
        ("implementation", "0x1234"),
        ("treasuries", {}),
        ("treasuries", {"01": "0x" + "11" * 20}),
        ("treasuries", {"0": "0x" + "11" * 20}),
        ("treasuries", {str(2**53): "0x" + "11" * 20}),
        ("treasuries", {"1": "0x" + "00" * 20}),
        ("webhook_keys", []),
        ("webhook_keys", [{"version": 0, "public_key": SERVICE_PUBLIC_KEY}]),
        ("webhook_keys", [{"version": True, "public_key": SERVICE_PUBLIC_KEY}]),
        ("webhook_keys", [{"version": 2**32, "public_key": SERVICE_PUBLIC_KEY}]),
        ("webhook_keys", [{"version": 1, "public_key": "whpk_bad"}]),
        (
            "webhook_keys",
            [{"version": 1, "public_key": "whpk_" + base64.b64encode(b"x" * 31).decode()}],
        ),
        (
            "webhook_keys",
            [
                {"version": 1, "public_key": SERVICE_PUBLIC_KEY},
                {"version": 1, "public_key": "whpk_" + base64.b64encode(b"x" * 32).decode()},
            ],
        ),
        (
            "webhook_keys",
            [
                {"version": 1, "public_key": SERVICE_PUBLIC_KEY},
                {"version": 2, "public_key": SERVICE_PUBLIC_KEY},
            ],
        ),
        ("webhook_keys", [{"version": 1, "public_key": SERVICE_PUBLIC_KEY, "extra": 1}]),
    ],
)
def test_pins_field_boundaries(field: str, value: Any) -> None:
    document = pin_document()
    document[field] = value
    with pytest.raises(ConfigurationError):
        parse_pins(encoded(document))


@pytest.mark.parametrize("change", ["unknown", "missing", "nested_duplicate"])
def test_pins_unknown_missing_and_recursive_duplicate_fields(change: str) -> None:
    document = pin_document()
    if change == "unknown":
        document["future"] = 1
    elif change == "missing":
        del document["account"]
    else:
        raw = json.dumps(document).replace(
            '"11155111":', '"11155111":"0x' + "11" * 20 + '","11155111":'
        )
        with pytest.raises(ConfigurationError):
            parse_pins(encoded(raw.encode()))
        return
    with pytest.raises(ConfigurationError):
        parse_pins(encoded(document))


def test_pins_encoding_sorts_textual_chain_keys_versions_and_lowercases_addresses() -> None:
    document = pin_document()
    document["api_base"] = "HTTPS://SERVICE.TEST:443/"
    document["factory"] = "0x" + "AB" * 20
    document["treasuries"] = {"2": "0x" + "CD" * 20, "10": "0x" + "EF" * 20}
    document["webhook_keys"] = [
        {"version": 2, "public_key": "whpk_" + base64.b64encode(b"x" * 32).decode()},
        {"version": 1, "public_key": SERVICE_PUBLIC_KEY},
    ]
    parsed = parse_pins(encoded(document))
    canonical = base64.urlsafe_b64decode(encode_pins(parsed).split(".")[1] + "==").decode()
    assert canonical == json.dumps(json.loads(canonical), sort_keys=True, separators=(",", ":"))
    assert canonical.index('"10"') < canonical.index('"2"')
    assert parsed.factory == "0x" + "ab" * 20
    assert parsed.webhook_keys[0][0] == 1
    assert parsed.api_base == "https://service.test"
    assert (
        encode_pins(parse_pins(load("pins-v1.json")["canonical_encoding"]))
        == load("pins-v1.json")["canonical_encoding"]
    )


def test_pins_are_frozen_and_copy_nested_input_and_client_properties_are_readonly() -> None:
    source = {1: "0x" + "11" * 20}
    value = replace(pins(), treasuries=source)
    source[1] = "0x" + "22" * 20
    assert value.treasuries[1] == "0x" + "11" * 20
    with pytest.raises(TypeError):
        value.treasuries[1] = "0x" + "33" * 20  # type: ignore[index]
    with pytest.raises(FrozenInstanceError):
        setattr(value, "account", "changed")  # noqa: B010 - deliberately exercise frozen boundary
    with pay(lambda _: httpx.Response(200, json=_quote())) as client:
        for name in ("pins", "livemode"):
            with pytest.raises(AttributeError):
                setattr(client, name, None)


@pytest.mark.parametrize(
    "base",
    [
        "http://service.test",
        "https://user:pass@service.test",
        "https://service.test?x=1",
        "https://service.test#x",
        "https://service.test/prefix",
        "https://service.test:bad",
        "https://",
        "ftp://localhost",
    ],
)
def test_origins_reject_non_https_credentials_query_fragment_and_prefix(base: str) -> None:
    with pytest.raises(ConfigurationError):
        replace(pins(), api_base=base)


def test_origin_normalization_override_and_test_loopback_are_fail_closed() -> None:
    with PhalaPay(
        KEY,
        pins=pins(),
        api_base="HTTPS://SERVICE.TEST:443/",
        transport=httpx.MockTransport(lambda _: httpx.Response(200)),
    ) as client:
        assert client.pins.api_base == "https://service.test"
    with pytest.raises(ConfigurationError, match="match pins"):
        PhalaPay(KEY, pins=pins(), api_base="https://other.test")
    for origin in ("http://localhost:3000/", "http://127.0.0.1:80", "http://[::1]:3000"):
        assert replace(pins(), api_base=origin).api_base.startswith("http://")
    with pytest.raises(ConfigurationError):
        replace(pins(), livemode=True, api_base="http://localhost")


@pytest.mark.parametrize("live", [False, True])
@pytest.mark.parametrize("restricted", [False, True])
def test_api_key_format_checksum_and_mode(live: bool, restricted: bool) -> None:
    key = valid_key(live=live, restricted=restricted)
    trust = replace(pins(), livemode=live)
    with PhalaPay(key, pins=trust) as client:
        assert client.livemode is live
    for bad in (
        key[:-1] + ("0" if key[-1] != "0" else "1"),
        key[:-1],
        key + "x",
        "ppay_pk_test_" + "A" * 49,
    ):
        with pytest.raises(ConfigurationError):
            PhalaPay(bad, pins=trust)
    with pytest.raises(ConfigurationError, match="mode"):
        PhalaPay(key, pins=replace(trust, livemode=not live))
    with pytest.raises(ConfigurationError, match="pins is required"):
        PhalaPay(key)


@pytest.mark.filterwarnings("ignore:The legacy PhalaPay constructor:DeprecationWarning")
def test_from_env_reads_exactly_two_values_and_never_merges_legacy_trust() -> None:
    class Env(dict[str, str]):
        def __init__(self) -> None:
            super().__init__(PHALA_PAY_API_KEY=KEY, PHALA_PAY_PINS=encode_pins(pins()))
            self.reads: list[str] = []

        def __getitem__(self, key: str) -> str:
            self.reads.append(key)
            return super().__getitem__(key)

    env = Env()
    with PhalaPay.from_env(env) as client:
        assert client.pins == pins()
    assert env.reads == ["PHALA_PAY_API_KEY", "PHALA_PAY_PINS"]
    for values in (
        {},
        {"PHALA_PAY_API_KEY": KEY},
        {"PHALA_PAY_API_KEY": KEY, "PHALA_PAY_PINS": "bad", "PHALA_PAY_ACCOUNT": ACCOUNT},
    ):
        with pytest.raises(ConfigurationError):
            PhalaPay.from_env(values)
    with pytest.raises(ConfigurationError):
        PhalaPay(KEY, pins=pins(), account=ACCOUNT)


@pytest.mark.parametrize("resource", ["quote", "deposit_address", "account"])
def test_response_identity_rejects_wrong_mode_and_account(resource: str) -> None:
    body = _quote(livemode=True) if resource == "quote" else _deposit_address(livemode=True)
    if resource == "account":
        body = {
            "object": "account",
            "id": "acct_" + "99" * 16,
            "livemode": False,
            "name": "other",
            "charges_enabled": True,
            "paused_scopes": [],
            "webhook_keys": [],
            "created": 1,
        }
    with (  # noqa: PT012
        pay(lambda _: httpx.Response(200, json=body)) as client,
        pytest.raises(ResponseValidationError),
    ):
        if resource == "quote":
            client.quotes.retrieve("qt_1")
        elif resource == "deposit_address":
            client.deposit_addresses.retrieve("da_1")
        else:
            client.account.retrieve()


@pytest.mark.parametrize(
    ("offset", "tolerance", "accept"),
    [
        (-300, 300, True),
        (300, 300, True),
        (-301, 300, False),
        (301, 300, False),
        (0, 0, True),
        (1, 0, False),
        (-1, 0, False),
    ],
)
def test_bound_webhook_inclusive_bilateral_window_and_zero_exact_match(
    monkeypatch: pytest.MonkeyPatch, offset: int, tolerance: int, accept: bool
) -> None:
    monkeypatch.setattr(time, "time", lambda: 1_790_000_000)
    body, headers = _delivery(timestamp=1_790_000_000 + offset)
    with pay(lambda _: httpx.Response(200)) as client:
        if accept:
            assert (
                cast(_BoundWebhook, client.webhooks)
                .construct_event(body, headers, tolerance=tolerance)
                .deposit.amount
                == 2500
            )
        else:
            with pytest.raises(SignatureVerificationError):
                cast(_BoundWebhook, client.webhooks).construct_event(
                    body, headers, tolerance=tolerance
                )


@pytest.mark.parametrize("tolerance", [-1, float("nan"), float("inf"), True])
def test_bound_webhook_invalid_tolerance_is_configuration_error(tolerance: float) -> None:
    body, headers = _delivery()
    with pay(lambda _: httpx.Response(200)) as client, pytest.raises(ConfigurationError):
        cast(_BoundWebhook, client.webhooks).construct_event(body, headers, tolerance=tolerance)


@pytest.mark.parametrize("duplicate", ["webhook-id", "webhook-timestamp", "webhook-signature"])
def test_bound_webhooks_reject_duplicate_signing_headers(duplicate: str) -> None:
    body, headers = _delivery()
    headers[duplicate.upper()] = headers[duplicate]
    with (
        pay(lambda _: httpx.Response(200)) as client,
        pytest.raises(SignatureVerificationError, match="duplicate"),
    ):
        cast(_BoundWebhook, client.webhooks).construct_event(body, headers)
    pairs = [(name, value) for name, value in headers.items()]
    with pay(lambda _: httpx.Response(200)) as client, pytest.raises(SignatureVerificationError):
        cast(_BoundWebhook, client.webhooks).construct_event(body, httpx.Headers(pairs))


@pytest.mark.parametrize(
    "change", ["account", "mode", "id", "envelope", "resource", "unsigned", "utf8"]
)
def test_bound_webhooks_fail_closed_for_invalid_identity_envelope_and_resource(change: str) -> None:
    body, headers = _delivery()
    envelope = json.loads(body)
    if change == "account":
        envelope["account"] = "acct_" + "99" * 16
    elif change == "mode":
        envelope["livemode"] = True
    elif change == "id":
        envelope["id"] = "evt_other"
    elif change == "envelope":
        del envelope["actor"]
    elif change == "resource":
        envelope["data"]["object"]["amount"] = "invalid"
    body = b"\xff" if change == "utf8" else json.dumps(envelope).encode()
    headers = sign_webhook(SERVICE_KEY, EVENT_ID, int(time.time()), body)
    if change == "unsigned":
        headers = {}
    with pay(lambda _: httpx.Response(200)) as client, pytest.raises(SignatureVerificationError):
        cast(_BoundWebhook, client.webhooks).construct_event(body, headers)


def test_bound_webhooks_use_only_pins_accept_text_and_unknown_types_stay_raw() -> None:
    for event_type in ("future.created", "deposit.future", "quote.future", "refund.future"):
        body, headers = _delivery(event_type, {"future": 1})
        with pay(lambda _: pytest.fail("webhook verification must not fetch keys")) as client:
            event = cast(_BoundWebhook, client.webhooks).construct_event(
                body.decode(), {k.upper(): v for k, v in headers.items()}
            )
            assert event.data.object == {"future": 1}
            with pytest.raises(TypeError):
                cast(Any, client.webhooks).construct_event(
                    body, headers, public_key=SERVICE_PUBLIC_KEY
                )


def test_bound_webhook_rotation_requires_explicit_pins_and_notices_do_not_change_trust() -> None:
    new_key = "whpk_" + base64.b64encode(b"x" * 32).decode()
    trust = replace(pins(), webhook_keys=((1, SERVICE_PUBLIC_KEY), (2, new_key)))
    body, headers = _delivery(
        "account.updated", {"webhook_keys": [{"version": 3, "public_key": "whpk_bad"}]}
    )
    with PhalaPay(KEY, pins=trust) as client:
        assert (
            cast(_BoundWebhook, client.webhooks).construct_event(body, headers).type
            == "account.updated"
        )
        assert client.pins.webhook_keys == trust.webhook_keys
    with (
        PhalaPay(KEY, pins=replace(trust, webhook_keys=((2, new_key),))) as client,
        pytest.raises(SignatureVerificationError),
    ):
        cast(_BoundWebhook, client.webhooks).construct_event(body, headers)


def snapshot(**fields: Any) -> dict[str, Any]:
    return {
        "id": "dep_1",
        "livemode": False,
        "client_reference_id": "team-42",
        "currency": "usd",
        "status": "credited",
        "amount": 2500,
        "amount_refunded": 0,
        "amount_reversed": 0,
        **fields,
    }


@pytest.mark.parametrize(
    ("field", "value"),
    [
        ("id", None),
        ("livemode", 0),
        ("client_reference_id", []),
        ("currency", None),
        ("status", "future"),
        ("status", []),
        ("amount", -1),
        ("amount", 1.5),
        ("amount", True),
        ("amount", 2**53),
        ("amount", None),
        ("amount_refunded", -1),
        ("amount_refunded", 1.5),
        ("amount_refunded", False),
        ("amount_refunded", None),
        ("amount_reversed", None),
        ("amount_refunded", 2501),
        ("amount_reversed", 2**53),
    ],
)
def test_ledger_rejects_invalid_identity_status_integers_and_deductions(
    field: str, value: Any
) -> None:
    with pytest.raises(LedgerSnapshotError):
        deposit_net_amount(snapshot(**{field: value}))


@pytest.mark.parametrize("field", ["id", "livemode", "client_reference_id", "currency", "amount"])
def test_ledger_merge_rejects_conflicting_identity_and_valuation(field: str) -> None:
    old = snapshot()
    value = {
        "id": "dep_other",
        "livemode": True,
        "client_reference_id": "other",
        "currency": "eur",
        "amount": 2501,
    }[field]
    with pytest.raises(LedgerSnapshotError):
        balance_delta(old, snapshot(**{field: value}))


def test_ledger_is_pure_json_serializable_and_converges_on_all_delivery_orders() -> None:
    events = [
        snapshot(status="pending", amount=None),
        snapshot(),
        snapshot(amount_refunded=500),
        snapshot(amount_refunded=750),
    ]
    original = json.dumps(events, sort_keys=True)
    for ordered in permutations(events):
        previous = None
        total = 0
        for deposit in (*ordered, ordered[-1]):
            change = balance_delta(previous, deposit)
            total += change.delta
            assert change.contribution == deposit_net_amount(change.snapshot)
            assert json.loads(json.dumps(change.snapshot)) == change.snapshot
            previous = change.snapshot
        assert total == 1750
        assert previous is not None
        assert previous["amount_refunded"] == 750
    assert json.dumps(events, sort_keys=True) == original


def test_ledger_pending_rejected_reversal_and_null_to_valued_rules() -> None:
    for status in ("pending", "rejected", "reversed"):
        assert deposit_net_amount(snapshot(status=status, amount=None)) == 0
    assert balance_delta(snapshot(status="pending", amount=None), snapshot()).delta == 2500
    assert balance_delta(snapshot(status="pending"), snapshot(status="rejected")).delta == 0
    reversal = snapshot(status="reversed", amount_reversed=2500)
    for ordered in ((snapshot(), reversal), (reversal, snapshot())):
        old = None
        total = 0
        for value in ordered:
            change = balance_delta(old, value)
            total += change.delta
            old = change.snapshot
        assert total == 0
    for invalid in (
        snapshot(status="reversed", amount_reversed=1),
        snapshot(amount_refunded=1, amount_reversed=1),
        snapshot(status="pending", amount=None, amount_refunded=1),
    ):
        with pytest.raises(LedgerSnapshotError):
            deposit_net_amount(invalid)
    with pytest.raises(LedgerSnapshotError):
        balance_delta(snapshot(), snapshot(status="rejected"))


def test_webhook_quote_secret_does_not_leak_in_event_repr() -> None:
    body, headers = _delivery("quote.expired", _quote(status="expired", client_secret=SECRET))
    with pay(lambda _: httpx.Response(200)) as client:
        event = cast(_BoundWebhook, client.webhooks).construct_event(body, headers)
    assert SECRET not in repr(event)
    assert event.quote.client_secret == SECRET


def test_unvalued_reversal_remains_zero_when_valuation_arrives() -> None:
    for previous, current in (
        (snapshot(status="reversed", amount=None), snapshot()),
        (snapshot(), snapshot(status="reversed", amount=None)),
    ):
        result = balance_delta(previous, current)
        assert result.snapshot["status"] == "reversed"
        assert result.snapshot["amount_reversed"] == 2500
        assert result.contribution == 0
        assert result.delta == -deposit_net_amount(previous)


def test_active_address_summary_empty_networks_and_historical_objects() -> None:
    for body in (_deposit_address(address="0x" + "99" * 20), _deposit_address(networks=[])):

        def handler(_: httpx.Request, value: dict[str, object] = body) -> httpx.Response:
            return httpx.Response(200, json=value)

        with pay(handler) as client, pytest.raises(AddressMismatchError):
            client.deposit_addresses.retrieve("da_1")
    historical = _deposit_address(status="retired", address="0x" + "99" * 20)
    with pay(lambda _: httpx.Response(200, json=historical)) as client:
        assert client.deposit_addresses.retrieve("da_1").status == "retired"
    with pay(
        lambda _: httpx.Response(200, json=_quote(status="expired", address="0x" + "99" * 20))
    ) as client:
        assert client.quotes.retrieve("qt_1").status == "expired"


def test_shared_webhook_vectors_execute_through_bound_client(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    fixtures = load("webhooks-v1.json")
    for case in fixtures["cases"]:
        monkeypatch.setattr(time, "time", lambda value=case["now"]: value)
        trust = replace(
            pins(),
            account=case["expected_account"],
            livemode=case["expected_livemode"],
            webhook_keys=((1, fixtures["public_key"]),),
        )
        with PhalaPay(valid_key(live=trust.livemode), pins=trust) as client:
            if case["outcome"] == "accept":
                assert (
                    cast(_BoundWebhook, client.webhooks)
                    .construct_event(
                        case["body"], case["headers"], tolerance=case.get("tolerance", 300)
                    )
                    .id
                    == case["headers"]["webhook-id"]
                )
            else:
                with pytest.raises(SignatureVerificationError):
                    cast(_BoundWebhook, client.webhooks).construct_event(
                        case["body"], case["headers"], tolerance=case.get("tolerance", 300)
                    )


@pytest.mark.filterwarnings("ignore:The legacy PhalaPay constructor:DeprecationWarning")
@pytest.mark.parametrize("missing", ["account", "forwarder", "treasuries"])
def test_legacy_live_constructor_requires_all_address_pins_at_construction(missing: str) -> None:
    arguments: dict[str, Any] = {
        "account": pins().account,
        "forwarder": (pins().factory, pins().implementation),
        "treasuries": dict(pins().treasuries),
    }
    del arguments[missing]
    with pytest.raises(ConfigurationError, match="live mode requires"):
        PhalaPay("https://service.test", valid_key(live=True), **arguments)


@pytest.mark.parametrize("event_type", ["deposit.credited", "quote.expired"])
def test_bound_webhook_resource_mode_must_match_pins(event_type: str) -> None:
    resource = _deposit() if event_type == "deposit.credited" else _quote()
    resource["livemode"] = True
    body, headers = _delivery(event_type, resource)
    with (
        pay(lambda _: pytest.fail("verification must stay offline")) as client,
        pytest.raises(SignatureVerificationError, match="mode"),
    ):
        cast(_BoundWebhook, client.webhooks).construct_event(body, headers)


def test_webhook_request_and_previous_attributes_secret_reprs_are_redacted() -> None:
    body, _ = _delivery()
    envelope = json.loads(body)
    envelope["request"] = {"id": KEY, "idempotency_key": SECRET}
    envelope["data"]["previous_attributes"] = {"client_secret": SECRET, "api_key": KEY}
    body = json.dumps(envelope).encode()
    headers = sign_webhook(SERVICE_KEY, EVENT_ID, int(time.time()), body)
    with pay(lambda _: httpx.Response(200)) as client:
        event = cast(_BoundWebhook, client.webhooks).construct_event(body, headers)
    assert event.request.id == KEY
    assert event.request.idempotency_key == SECRET
    assert event.data.previous_attributes == {"client_secret": SECRET, "api_key": KEY}
    assert KEY not in repr(event)
    assert SECRET not in repr(event)
