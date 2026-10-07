"""Executable transport and resource invariants from sdk-ergonomics-reference.md."""

from __future__ import annotations

import json
import logging
import uuid
from collections.abc import Iterator
from email.utils import formatdate
from typing import Any

import httpx
import pytest

from phala_pay import (
    AddressMismatchError,
    ApiError,
    ConfigurationError,
    PhalaPay,
    PhalaPayError,
    Quote,
    ResponseValidationError,
    TransportError,
)
from topup_client.types import Unset
from topup_sdk.client import TopupClient

from ._support import (
    EPOCH,
    KEY,
    QUOTE_ID,
    SECRET,
    TREASURY,
    Clock,
    _deposit,
    _deposit_address,
    _quote,
    error,
    invoke,
    maintenance,
    merchant,
    pay,
    pins,
    record_response,
)


@pytest.mark.parametrize("upgrade_tolerance", [False, True])
@pytest.mark.parametrize("rng_value", [0.0, 0.25, 1.0])
def test_retry_jitter_half_full_exponential_and_mode_cap(
    upgrade_tolerance: bool, rng_value: float
) -> None:
    clock = Clock()
    requests: list[httpx.Request] = []
    caps = [0.5, 1, 2, 4, 8, 10, 10, 10] if upgrade_tolerance else [0.5, 1, 2, 4, 5, 5]

    def handler(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        if len(requests) <= len(caps):
            return maintenance() if upgrade_tolerance else error(503)
        return httpx.Response(200, json=_quote())

    with merchant(
        handler,
        clock,
        upgrade_tolerance=upgrade_tolerance,
        max_attempts=7,
        rng=lambda: rng_value,
    ) as client:
        invoke(client, "GET")
    assert clock.delays == [cap * (0.5 + 0.5 * rng_value) for cap in caps]
    assert len(requests) == len(caps) + 1


@pytest.mark.parametrize("upgrade_tolerance", [False, True])
@pytest.mark.parametrize("http_date", [False, True])
@pytest.mark.parametrize("scenario", ["minimum", "budget", "minimum_then_budget"])
def test_retry_after_minimum_wait_and_remaining_budget(
    upgrade_tolerance: bool, http_date: bool, scenario: str
) -> None:
    clock = Clock()
    requests: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        if scenario == "minimum" and len(requests) == 2:
            return httpx.Response(200, json=_quote())
        if scenario == "budget":
            clock.now += 1.0
        seconds = {
            "minimum": 3,
            "budget": 4,
            "minimum_then_budget": 5 if len(requests) == 1 else 300,
        }[scenario]
        header = formatdate(EPOCH + clock.now + seconds, usegmt=True) if http_date else str(seconds)
        headers = {"retry-after": header, "request-id": "req_last"}
        if scenario == "budget":
            return error(503, "last_error", **headers)
        if upgrade_tolerance or scenario == "minimum_then_budget":
            return httpx.Response(502, text="<html>down</html>", headers=headers)
        return error(429, **headers)

    with merchant(
        handler,
        clock,
        upgrade_tolerance=upgrade_tolerance,
        request_deadline=4 if scenario == "budget" else None,
        rng=lambda: 1.0,
    ) as client:
        if scenario == "minimum":
            invoke(client, "GET")
        else:
            expected = TransportError if upgrade_tolerance else ApiError
            with pytest.raises(expected) as raised:
                invoke(client, "GET")
            if scenario == "budget" and not upgrade_tolerance:
                assert isinstance(raised.value, ApiError)
                assert (raised.value.code, raised.value.request_id, raised.value.retry_after) == (
                    "last_error",
                    "req_last",
                    4.0,
                )
    assert clock.delays == (
        [] if scenario == "budget" else [3.0] if scenario == "minimum" else [5.0]
    )
    assert clock.now == (1 if scenario == "budget" else 3 if scenario == "minimum" else 5)
    assert len(requests) == (1 if scenario == "budget" else 2)


@pytest.mark.parametrize("status", [301, 302, 303, 307, 308])
def test_authenticated_redirect_is_never_followed(status: int) -> None:
    requests: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        return httpx.Response(status, headers={"location": "https://other.test/leak"})

    with pay(handler) as client, pytest.raises(ApiError) as raised:
        client.quotes.retrieve(QUOTE_ID)
    assert raised.value.status_code == status
    assert len(requests) == 1
    assert requests[0].headers["authorization"] == f"Bearer {KEY}"


@pytest.mark.parametrize("status", [429, 500, 502, 503, 504, 409])
def test_only_documented_retry_statuses_and_409_code_retry(status: int) -> None:
    requests: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        return (
            error(status, "idempotency_key_in_use" if status == 409 else "unavailable")
            if len(requests) == 1
            else httpx.Response(200, json=_quote())
        )

    with pay(handler, sleep=lambda _: None) as client:
        client.quotes.retrieve(QUOTE_ID)
    assert len(requests) == 2


@pytest.mark.parametrize(
    ("status", "code"),
    [
        (400, "idempotency_key_in_use"),
        (409, "conflict"),
        (422, "address_capacity_reached"),
        (501, "unavailable"),
    ],
)
def test_other_statuses_and_conflicts_are_terminal(status: int, code: str) -> None:
    requests: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        return error(status, code)

    with pay(handler) as client, pytest.raises(ApiError) as raised:
        client.quotes.retrieve(QUOTE_ID)
    assert len(requests) == 1
    assert raised.value.code == code
    assert raised.value.status_code == status
    assert raised.value.retry_after is None


@pytest.mark.parametrize("upgrade_tolerance", [False, True])
@pytest.mark.parametrize("method", ["GET", "POST"])
@pytest.mark.parametrize("fault", ["success", "internal", "api", "html", "body_timeout"])
def test_replayed_responses_end_retries(upgrade_tolerance: bool, method: str, fault: str) -> None:
    clock = Clock()
    requests: list[httpx.Request] = []

    class SlowReplayedBody(httpx.SyncByteStream):
        def __iter__(self) -> Iterator[bytes]:
            clock.now += 16
            yield b"late"

    def handler(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        headers = {"Idempotent-Replayed": "True"}
        if fault == "success":
            return httpx.Response(200, json=_quote(), headers=headers)
        if fault == "internal":
            return error(500, **headers)
        if fault == "api":
            return maintenance(headers=headers)
        if fault == "html":
            return httpx.Response(502, text="<html>down</html>", headers=headers)
        return httpx.Response(503, stream=SlowReplayedBody(), headers=headers)

    with merchant(handler, clock, upgrade_tolerance=upgrade_tolerance) as client:
        if fault == "success":
            invoke(client, method)
        else:
            expected = TransportError if fault == "body_timeout" else ApiError
            with pytest.raises(expected):
                invoke(client, method)
    assert len(requests) == 1
    assert clock.delays == []


def test_post_replay_freezes_body_and_generates_one_uuid_per_invocation(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    requests: list[httpx.Request] = []
    metadata = {"order": "original"}
    keys: list[str] = []

    def new_uuid() -> uuid.UUID:
        value = uuid.UUID(int=len(keys) + 1)
        keys.append(str(value))
        return value

    monkeypatch.setattr("topup_sdk.client.uuid.uuid4", new_uuid)

    def handler(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        metadata["order"] = "changed"
        return error(503) if len(requests) == 1 else httpx.Response(200, json=_quote())

    with pay(handler, sleep=lambda _: None) as client:
        for _ in range(2):
            metadata["order"] = "original"
            client.quotes.create(
                client_reference_id="team-42",
                amount=2500,
                chain_id=11155111,
                asset="pha",
                metadata=metadata,
            )
    assert len(keys) == 2
    assert requests[0].content == requests[1].content
    assert json.loads(requests[1].content)["metadata"] == {"order": "original"}
    assert requests[0].headers["idempotency-key"] == requests[1].headers["idempotency-key"]
    assert requests[1].headers["idempotency-key"] != requests[2].headers["idempotency-key"]


@pytest.mark.parametrize("key", ["x" * 256, "", "line\nbreak", "非ASCII"])
def test_idempotency_key_validation_precedes_io(key: str) -> None:
    requests: list[httpx.Request] = []
    with (
        pay(lambda r: record_response(requests, r, httpx.Response(200, json=_quote()))) as client,
        pytest.raises(ConfigurationError),
    ):
        client.quotes.cancel(QUOTE_ID, idempotency_key=key)
    assert requests == []


def test_explicit_order_key_survives_client_restart() -> None:
    requests: list[httpx.Request] = []
    for _ in range(2):
        with pay(
            lambda r: record_response(requests, r, httpx.Response(200, json=_quote()))
        ) as client:
            client.quotes.cancel(QUOTE_ID, idempotency_key="order-1")
    assert [r.headers["idempotency-key"] for r in requests] == ['"order-1"', '"order-1"']


@pytest.mark.parametrize("failure", [httpx.ConnectError, httpx.ReadTimeout])
def test_network_failures_retry_then_raise_public_transport_error_without_cause(
    failure: type[httpx.TransportError],
) -> None:
    requests: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        raise failure(f"{KEY} {SECRET}")

    with (
        pay(handler, max_attempts=2, sleep=lambda _: None) as client,
        pytest.raises(TransportError) as raised,
    ):
        client.quotes.retrieve(QUOTE_ID)
    assert raised.value.code == ("timeout" if failure is httpx.ReadTimeout else "network")
    assert isinstance(raised.value, PhalaPayError)
    assert raised.value.__cause__ is None
    assert raised.value.__context__ is None
    assert len(requests) == 2


@pytest.mark.parametrize("failure", [KeyboardInterrupt, SystemExit])
def test_python_interrupts_are_terminal(failure: type[BaseException]) -> None:
    requests: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        raise failure

    with pay(handler) as client, pytest.raises(failure):
        client.quotes.retrieve(QUOTE_ID)
    assert len(requests) == 1


@pytest.mark.parametrize("network", [False, True])
def test_delete_is_never_retried_even_on_network_failure(network: bool) -> None:
    requests: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        if network:
            raise httpx.ConnectError("offline")
        return error(503)

    with pay(handler) as client, pytest.raises(TransportError if network else ApiError):
        client.webhook_endpoints.delete("we_1")
    assert len(requests) == 1
    assert requests[0].method == "DELETE"


@pytest.mark.parametrize("option", ["timeout", "request_deadline", "max_attempts"])
@pytest.mark.parametrize("value", [0, -1, float("inf"), float("nan"), True])
def test_transport_options_fail_before_io(option: str, value: float) -> None:
    with pytest.raises(ConfigurationError):
        pay(lambda _: httpx.Response(200), **{option: value})


@pytest.mark.parametrize("value", [1.5, 11])
def test_attempts_are_bounded_integers(value: float) -> None:
    with pytest.raises(ConfigurationError):
        pay(lambda _: httpx.Response(200), max_attempts=value)


class SlowBody(httpx.SyncByteStream):
    def __init__(self, clock: Clock) -> None:
        self.clock = clock
        self.closed = False

    def __iter__(self) -> Iterator[bytes]:
        self.clock.now += 0.6
        yield b'{"object":'
        self.clock.now += 0.6
        yield b'"quote"}'

    def close(self) -> None:
        self.closed = True


def test_attempt_timeout_includes_response_body_and_closes_stream() -> None:
    clock = Clock()
    body = SlowBody(clock)
    with (
        TopupClient(
            "https://service.test",
            KEY,
            timeout=1,
            max_attempts=1,
            clock=clock,
            transport=httpx.MockTransport(lambda _: httpx.Response(200, stream=body)),
        ) as client,
        pytest.raises(TransportError) as raised,
    ):
        client.get_quote(QUOTE_ID)
    assert raised.value.code == "timeout"
    assert body.closed


@pytest.mark.parametrize("upgrade_tolerance", [False, True])
@pytest.mark.parametrize("deadline", [None, 1.5])
def test_total_deadline_includes_attempts_sleeps_and_per_call_override(
    upgrade_tolerance: bool, deadline: float | None
) -> None:
    clock = Clock()
    timeouts: list[float] = []

    def handler(request: httpx.Request) -> httpx.Response:
        timeouts.append(request.extensions["timeout"]["read"])
        clock.now += 0.4 if deadline is not None else timeouts[-1]
        if deadline is not None:
            return maintenance()
        raise httpx.ReadTimeout("offline")

    with (
        merchant(
            handler,
            clock,
            upgrade_tolerance=upgrade_tolerance,
            request_deadline=20 if deadline is not None else None,
            rng=lambda: 1.0 if deadline is not None else 0.0,
        ) as client,
        pytest.raises(ApiError if deadline is not None else TransportError),
    ):
        client.quotes.retrieve(QUOTE_ID, request_deadline=deadline)
    if deadline is not None:
        assert timeouts == [1.5, pytest.approx(0.6)]
        assert clock.delays == [0.5]
        assert clock.now == pytest.approx(1.3)
    else:
        budget = 300 if upgrade_tolerance else 60
        assert sum(timeouts) + sum(clock.delays) == clock.now
        assert all(0 < timeout <= 15 for timeout in timeouts)
        if upgrade_tolerance:
            assert 290 < clock.now <= budget
            assert clock.now + 5 >= budget
            assert len(timeouts) > 4
        else:
            assert clock.now == budget
            assert timeouts == [15, 15, 15, 13.25]


@pytest.mark.parametrize(
    "body", ["<html>bad</html>", {"object": "quote"}, {**_quote(), "amount": "bad"}]
)
def test_malformed_response_has_public_validation_error_and_no_body(body: object) -> None:
    with (
        pay(
            lambda _: (
                httpx.Response(200, text=body, headers={"Request-Id": "req_bad"})
                if isinstance(body, str)
                else httpx.Response(200, json=body, headers={"Request-Id": "req_bad"})
            )
        ) as client,
        pytest.raises(ResponseValidationError) as raised,
    ):
        client.quotes.retrieve(QUOTE_ID)
    assert raised.value.status_code == 200
    assert raised.value.request_id == "req_bad"
    assert raised.value.__cause__ is None
    assert raised.value.__context__ is None
    assert "<html>" not in str(raised.value)


def test_api_errors_preserve_all_optional_fields() -> None:
    with (
        pay(
            lambda _: httpx.Response(
                400,
                json={
                    "error": {
                        "code": "bad",
                        "message": "safe",
                        "type": "future_error",
                        "param": "amount",
                        "doc_url": "https://docs.test/error",
                    }
                },
                headers={"Request-Id": "req_1", "Retry-After": "3"},
            )
        ) as client,
        pytest.raises(ApiError) as raised,
    ):
        client.quotes.retrieve(QUOTE_ID)
    e = raised.value
    assert (
        e.status_code,
        e.code,
        e.message,
        e.error_type,
        e.param,
        e.doc_url,
        e.request_id,
        e.retry_after,
    ) == (400, "bad", "safe", "future_error", "amount", "https://docs.test/error", "req_1", 3)
    with pay(lambda _: error(400, "bad")) as client, pytest.raises(ApiError) as absent:
        client.quotes.retrieve(QUOTE_ID)
    assert (
        absent.value.error_type,
        absent.value.param,
        absent.value.doc_url,
        absent.value.request_id,
        absent.value.retry_after,
    ) == (None, None, None, None, None)


@pytest.mark.parametrize("invalid", ["expired", "complete", "canceled", "future"])
def test_checkout_params_refuses_non_open_quotes(invalid: str) -> None:
    with pay(lambda _: httpx.Response(200, json=_quote(client_secret=SECRET))) as client:
        quote = client.quotes.create(
            client_reference_id="team-42", amount=2500, chain_id=11155111, asset="pha"
        )
        quote.status = invalid
        with pytest.raises(ResponseValidationError):
            client.checkout_params(quote)


@pytest.mark.parametrize("secret", [None, "", "absent"])
def test_checkout_params_refuses_missing_client_secret(secret: str | None) -> None:
    fields = {} if secret == "absent" else {"client_secret": secret}  # noqa: S105
    with pay(lambda _: httpx.Response(200, json=_quote(**fields))) as client:
        quote = client.quotes.create(
            client_reference_id="team-42", amount=2500, chain_id=11155111, asset="pha"
        )
        with pytest.raises(ResponseValidationError):
            client.checkout_params(quote)


def test_checkout_params_requires_originating_verified_quote_and_rechecks_pins() -> None:
    with (
        pay(lambda _: httpx.Response(200, json=_quote(client_secret=SECRET))) as first,
        pay(lambda _: httpx.Response(200, json=_quote(client_secret=SECRET))) as other,
    ):
        quote = first.quotes.create(
            client_reference_id="team-42", amount=2500, chain_id=11155111, asset="pha"
        )
        assert first.checkout_params(quote) == {
            "clientSecret": SECRET,
            "expectedAddress": quote.address,
            "apiBase": "https://service.test",
        }
        for unverified in (Quote.from_dict(quote.to_dict()), first.quotes.retrieve(QUOTE_ID)):
            with pytest.raises(ResponseValidationError):
                first.checkout_params(unverified)
        with pytest.raises(ResponseValidationError):
            other.checkout_params(quote)
        quote.address = "0x" + "11" * 20
        with pytest.raises(AddressMismatchError):
            first.checkout_params(quote)


@pytest.mark.parametrize(
    "resource",
    [
        "quotes",
        "deposits",
        "deposit_addresses",
        "refunds",
        "events",
        "webhook_endpoints",
        "api_keys",
        "treasuries",
        "sweeps",
        "forwarders",
    ],
)
@pytest.mark.parametrize("method", ["list", "list_page"])
def test_all_lists_reject_empty_continuing_pages(resource: str, method: str) -> None:
    requests: list[httpx.Request] = []
    with (  # noqa: PT012
        pay(
            lambda r: record_response(
                requests,
                r,
                httpx.Response(
                    200, json={"object": "list", "url": r.url.path, "has_more": True, "data": []}
                ),
            )
        ) as client,
        pytest.raises(ResponseValidationError, match="empty continuing"),
    ):
        result = getattr(getattr(client, resource), method)()
        if method == "list":
            list(result)
    assert len(requests) == 1


@pytest.mark.parametrize("resource", ["quotes", "deposits", "deposit_addresses"])
@pytest.mark.parametrize("method", ["list", "list_page"])
def test_lists_reject_repeated_cursor(resource: str, method: str) -> None:
    body = {"quotes": _quote(), "deposits": _deposit(), "deposit_addresses": _deposit_address()}[
        resource
    ]
    requests: list[httpx.Request] = []
    with (  # noqa: PT012
        pay(
            lambda r: record_response(
                requests,
                r,
                httpx.Response(
                    200,
                    json={"object": "list", "url": r.url.path, "has_more": True, "data": [body]},
                ),
            )
        ) as client,
        pytest.raises(ResponseValidationError, match="repeated"),
    ):
        if method == "list_page":
            getattr(client, resource).list_page(starting_after=body["id"])
        else:
            list(getattr(client, resource).list())
    assert len(requests) == (1 if method == "list_page" else 2)


@pytest.mark.parametrize(
    ("resource", "method"),
    [
        ("quotes", "list"),
        ("quotes", "list_page"),
        ("quotes", "retrieve"),
        ("quotes", "update"),
        ("quotes", "cancel"),
        ("deposit_addresses", "list"),
        ("deposit_addresses", "list_page"),
        ("deposit_addresses", "retrieve"),
        ("deposit_addresses", "update"),
        ("deposit_addresses", "rotate"),
    ],
)
def test_quotes_and_address_verification_runs_on_every_page_and_action(
    resource: str,
    method: str,
) -> None:
    forged = (
        _quote(address="0x" + "11" * 20)
        if resource == "quotes"
        else _deposit_address(
            networks=[
                {
                    "chain_id": 11155111,
                    "address": "0x" + "11" * 20,
                    "treasury": TREASURY,
                    "assets": [],
                }
            ]
        )
    )
    body = (
        {"object": "list", "url": f"/v1/{resource}", "has_more": False, "data": [forged]}
        if method.startswith("list")
        else forged
    )
    with (  # noqa: PT012
        pay(lambda _: httpx.Response(200, json=body)) as client,
        pytest.raises(AddressMismatchError),
    ):
        target = getattr(getattr(client, resource), method)
        result = target() if method.startswith("list") else target(str(forged["id"]))
        if method == "list":
            list(result)


def test_query_and_path_parameters_are_encoded_and_python_int64_is_preserved() -> None:
    requests: list[httpx.Request] = []
    with pay(
        lambda r: record_response(
            requests,
            r,
            httpx.Response(
                200,
                json=_quote(
                    amount=2**63 - 1,
                    amount_atomic="1000000000000000000000000",
                    exchange_rate="25.00000000",
                    status="future",
                    additional_field=True,
                ),
            ),
        )
    ) as client:
        quote = client.quotes.retrieve("qt_/ ?#客户")
    assert requests[0].url.raw_path == b"/v1/quotes/qt_%2F%20%3F%23%E5%AE%A2%E6%88%B7"
    assert quote.amount == 2**63 - 1
    assert quote.status == "future"
    assert quote.amount_atomic == "1000000000000000000000000"
    assert quote.exchange_rate == "25.00000000"
    assert quote.additional_properties["additional_field"] is True
    with pay(
        lambda r: record_response(
            requests,
            r,
            httpx.Response(
                200, json={"object": "list", "url": "/v1/quotes", "has_more": False, "data": []}
            ),
        )
    ) as client:
        assert client.quotes.list_page(client_reference_id="a&status=open?客户")["data"] == []
    assert requests[-1].url.params["client_reference_id"] == "a&status=open?客户"
    assert "status" not in requests[-1].url.params


class TrackingTransport(httpx.MockTransport):
    def __init__(self) -> None:
        self.closed = False
        super().__init__(lambda _: httpx.Response(200, json=_quote()))

    def close(self) -> None:
        self.closed = True


def test_context_manager_closes_client_without_closing_injected_transport() -> None:
    transport = TrackingTransport()
    with PhalaPay(KEY, pins=pins(), transport=transport) as client:
        http = client._client._client.get_httpx_client()
        client.quotes.retrieve(QUOTE_ID)
    assert http.is_closed
    assert not transport.closed
    client.close()
    assert not transport.closed


def test_api_keys_and_client_secrets_are_redacted_in_reprs_errors_and_logs(
    caplog: pytest.LogCaptureFixture,
) -> None:
    key_body = {
        "object": "api_key",
        "id": "key_1",
        "livemode": False,
        "created": 1,
        "name": "runtime",
        "redacted": "prefix-only",
        "status": "active",
        "type": "secret",
        "secret": KEY,
    }
    with (
        caplog.at_level(logging.DEBUG),
        pay(
            lambda r: httpx.Response(
                200, json=key_body if r.url.path == "/v1/api_keys" else _quote(client_secret=SECRET)
            )
        ) as client,
    ):
        quote = client.quotes.create(
            client_reference_id="team-42", amount=2500, chain_id=11155111, asset="pha"
        )
        key = client.api_keys.create()
        assert quote.client_secret == SECRET
        assert key.secret == KEY
        logging.getLogger("sdk_diagnostics").info(
            "%r %r %r %r", client, client._client._client, quote, key
        )
        assert KEY not in repr(key)
        assert SECRET not in repr(quote)
    with (
        caplog.at_level(logging.DEBUG),
        pay(
            lambda _: httpx.Response(
                400,
                json={
                    "error": {
                        "code": KEY,
                        "message": f"{KEY} {SECRET}",
                        "param": SECRET,
                        "doc_url": f"https://docs.test/{KEY}",
                        "type": KEY,
                    }
                },
                headers={"Request-Id": SECRET},
            )
        ) as client,
        pytest.raises(ApiError) as raised,
    ):
        client.quotes.retrieve(QUOTE_ID)
    logging.getLogger("sdk_diagnostics").error("%s %r", raised.value, raised.value)
    assert KEY not in str(raised.value)
    assert SECRET not in repr(raised.value)
    assert KEY not in repr(vars(raised.value))
    assert SECRET not in repr(vars(raised.value))
    assert KEY not in caplog.text
    assert SECRET not in caplog.text


@pytest.mark.parametrize("upgrade_tolerance", [False, True])
def test_unreplayable_post_body_is_rejected_before_authenticated_io(
    upgrade_tolerance: bool,
) -> None:
    clock = Clock()
    requests: list[httpx.Request] = []
    with merchant(
        lambda r: record_response(requests, r, httpx.Response(200)),
        clock,
        upgrade_tolerance=upgrade_tolerance,
    ) as client:

        def operation() -> Any:
            return client._client._client.get_httpx_client().request(
                "POST", "/v1/test", content=iter([b"one-shot"])
            )

        with pytest.raises(ConfigurationError, match="replayable"):
            client._client._call(operation, object)
    assert requests == []
    assert clock.delays == []


@pytest.mark.parametrize("method", ["list", "list_page"])
@pytest.mark.parametrize("limit", [0, 101, True, 1.5])
def test_pagination_limit_is_validated_before_io(method: str, limit: float) -> None:
    requests: list[httpx.Request] = []
    with (  # noqa: PT012
        pay(lambda r: record_response(requests, r, httpx.Response(200))) as client,
        pytest.raises(ConfigurationError),
    ):
        result = getattr(client.quotes, method)(limit=limit)
        if method == "list":
            list(result)
    assert requests == []


def test_iterator_rejects_long_cursor_cycles_before_yielding_duplicate_items() -> None:
    requests: list[httpx.Request] = []
    indexes = iter([1, 2, 1])

    def handler(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        return httpx.Response(
            200,
            json={
                "object": "list",
                "url": request.url.path,
                "has_more": True,
                "data": [_deposit(next(indexes))],
            },
        )

    with pay(handler) as client:
        iterator = client.deposits.list()
        assert next(iterator).id == str(_deposit(1)["id"])
        assert next(iterator).id == str(_deposit(2)["id"])
        with pytest.raises(ResponseValidationError, match="repeated"):
            next(iterator)
    assert len(requests) == 3


def test_retrieval_never_invents_a_client_secret() -> None:
    with pay(lambda _: httpx.Response(200, json=_quote())) as client:
        quote = client.quotes.retrieve(QUOTE_ID)
        assert isinstance(quote.client_secret, Unset)
        assert "client_secret" not in quote.to_dict()
        with pytest.raises(ResponseValidationError):
            client.checkout_params(quote)


@pytest.mark.parametrize("diagnostic", [KEY, SECRET])
def test_response_identity_validation_redacts_request_id_in_fields_and_logs(
    diagnostic: str,
    caplog: pytest.LogCaptureFixture,
) -> None:
    with (
        pay(
            lambda _: httpx.Response(
                200, json=_quote(livemode=True), headers={"Request-Id": diagnostic}
            )
        ) as client,
        pytest.raises(ResponseValidationError) as raised,
    ):
        client.quotes.retrieve(QUOTE_ID)
    assert raised.value.status_code == 200
    assert raised.value.request_id == "[REDACTED]"
    with caplog.at_level(logging.ERROR):
        logging.getLogger("sdk_diagnostics").error("%r %r", raised.value, vars(raised.value))
    assert diagnostic not in repr(vars(raised.value))
    assert diagnostic not in caplog.text
