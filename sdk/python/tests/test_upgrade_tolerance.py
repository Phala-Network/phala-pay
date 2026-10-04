"""Fake-clock acceptance cases for the upgrade tolerance amendment."""

from __future__ import annotations

from collections.abc import Callable, Iterator
from email.utils import formatdate
from typing import Any

import httpx
import pytest

from phala_pay import (
    ApiError,
    ConfigurationError,
    PhalaPay,
    ResponseValidationError,
    TransportError,
)
from topup_sdk._transport import REQUEST_STATE, HTTPClient

from .test_ergonomics_transport import Clock, pay
from .test_phala_pay import QUOTE_ID, _quote

EPOCH = 1_790_000_000


def merchant(
    handler: Callable[[httpx.Request], httpx.Response], clock: Clock, **options: Any
) -> PhalaPay:
    client = pay(handler, **options)
    # Exercise public resources using the transport's existing injected clock/RNG.
    client._client._clock = clock
    client._client._sleep = clock.sleep
    client._client._rng = lambda: 0.0
    client._client._wall_clock = lambda: EPOCH + clock.now
    http = client._client._client.get_httpx_client()
    assert isinstance(http, HTTPClient)
    http.clock = clock
    return client


def maintenance(*, headers: dict[str, str] | None = None) -> httpx.Response:
    return httpx.Response(
        503,
        json={"error": {"code": "service_maintenance", "message": "Restarting"}},
        headers=headers,
    )


def invoke(client: PhalaPay, method: str, **options: Any) -> None:
    if method == "GET":
        client.quotes.retrieve(QUOTE_ID, **options)
    else:
        client.quotes.create(
            client_reference_id="order-42", amount=2500, chain_id=11155111, asset="pha", **options
        )


@pytest.mark.parametrize("method", ["GET", "POST"])
@pytest.mark.parametrize("outage", ["maintenance", "network", "timeout", "html", "mixed"])
def test_three_minute_outage_recovers_with_fixed_body_and_key(method: str, outage: str) -> None:
    clock = Clock()
    attempts: list[tuple[float, bytes, str | None]] = []

    def handler(request: httpx.Request) -> httpx.Response:
        attempts.append((clock.now, request.content, request.headers.get("Idempotency-Key")))
        assert request.extensions["timeout"]["read"] == 15.0
        if clock.now >= 180:
            return httpx.Response(200, json=_quote())
        fault = outage
        if outage == "mixed":
            fault = "maintenance" if clock.now < 5 else "network" if clock.now < 60 else "html"
        if fault == "maintenance":
            return maintenance(headers={"Retry-After": "5"})
        if fault == "network":
            raise httpx.ConnectError("connection refused")
        if fault == "timeout":
            clock.now += request.extensions["timeout"]["read"]
            raise httpx.ReadTimeout("attempt timed out")
        return httpx.Response(502, text="<html>Bad Gateway</html>")

    with merchant(handler, clock, upgrade_tolerance=True) as client:
        invoke(client, method)
    assert 180 <= clock.now < 200
    assert len(attempts) > 4
    assert len({body for _, body, _ in attempts}) == 1
    keys = {key for _, _, key in attempts}
    assert len(keys) == 1
    assert (next(iter(keys)) is not None) == (method == "POST")
    assert max(clock.delays) <= 10
    if outage in {"maintenance", "mixed"}:
        assert attempts[1][0] == 5


@pytest.mark.parametrize("rng_value", [0.0, 0.25, 1.0])
def test_upgrade_jitter_half_full_exponential_and_ten_second_cap(rng_value: float) -> None:
    clock = Clock()
    calls = 0

    def handler(_: httpx.Request) -> httpx.Response:
        nonlocal calls
        calls += 1
        return maintenance() if calls <= 8 else httpx.Response(200, json=_quote())

    with merchant(handler, clock, upgrade_tolerance=True) as client:
        client._client._rng = lambda: rng_value
        invoke(client, "GET")
    caps = [0.5, 1, 2, 4, 8, 10, 10, 10]
    assert clock.delays == [cap * (0.5 + 0.5 * rng_value) for cap in caps]


@pytest.mark.parametrize("method", ["GET", "POST"])
@pytest.mark.parametrize("outage", ["maintenance", "network", "timeout", "html"])
def test_upgrade_budget_exhaustion_is_bounded_by_original_start(method: str, outage: str) -> None:
    clock = Clock()
    times: list[float] = []

    def handler(request: httpx.Request) -> httpx.Response:
        times.append(clock.now)
        if outage == "network":
            raise httpx.ConnectError("offline")
        if outage == "timeout":
            clock.now += request.extensions["timeout"]["read"]
            raise httpx.ReadTimeout("attempt timed out")
        if outage == "html":
            return httpx.Response(503, text="<html>internal gateway details</html>")
        return maintenance(headers={"Retry-After": "5"})

    expected = ApiError if outage == "maintenance" else TransportError
    with (
        merchant(handler, clock, upgrade_tolerance=True) as client,
        pytest.raises(expected) as raised,
    ):
        invoke(client, method)
    assert 290 < clock.now <= 300
    assert len(times) > 4
    assert max(times) < 300
    assert "internal gateway details" not in str(raised.value)
    assert REQUEST_STATE.get() is None


@pytest.mark.parametrize("scope", ["client", "call"])
@pytest.mark.parametrize("deadline", [2.0, 60.0, 400.0])
def test_explicit_deadline_is_never_extended(scope: str, deadline: float) -> None:
    clock = Clock()
    timeouts: list[float] = []

    def handler(request: httpx.Request) -> httpx.Response:
        timeouts.append(request.extensions["timeout"]["read"])
        clock.now += timeouts[-1]
        raise httpx.ReadTimeout("offline")

    constructor = {"request_deadline": deadline} if scope == "client" else {}
    options = {"request_deadline": deadline} if scope == "call" else {}
    with (
        merchant(handler, clock, upgrade_tolerance=True, **constructor) as client,
        pytest.raises(TransportError),
    ):
        invoke(client, "GET", **options)
    assert clock.now <= min(deadline, 300)
    assert all(timeout <= 15 for timeout in timeouts)
    if deadline == 2:
        assert timeouts == [2]


@pytest.mark.parametrize("http_date", [False, True])
def test_gateway_retry_after_minimum_and_remaining_deadline(http_date: bool) -> None:
    clock = Clock()
    calls: list[float] = []

    def handler(_: httpx.Request) -> httpx.Response:
        calls.append(clock.now)
        seconds = 5 if len(calls) == 1 else 300
        header = formatdate(EPOCH + clock.now + seconds, usegmt=True) if http_date else str(seconds)
        return httpx.Response(502, text="<html>down</html>", headers={"Retry-After": header})

    with merchant(handler, clock, upgrade_tolerance=True) as client, pytest.raises(TransportError):
        invoke(client, "GET")
    assert calls == [0, 5]
    assert clock.delays == [5]


@pytest.mark.parametrize("stage", ["request", "body", "sleep"])
def test_keyboard_interrupt_is_terminal_and_closes_body(stage: str) -> None:
    clock = Clock()
    calls = 0
    closed: list[bool] = []

    class InterruptedBody(httpx.SyncByteStream):
        def __iter__(self) -> Iterator[bytes]:
            raise KeyboardInterrupt
            yield b""  # pragma: no cover

        def close(self) -> None:
            closed.append(True)

    def handler(_: httpx.Request) -> httpx.Response:
        nonlocal calls
        calls += 1
        if stage == "request":
            raise KeyboardInterrupt
        if stage == "body":
            return httpx.Response(503, stream=InterruptedBody())
        return maintenance()

    def interrupted_sleep(_: float) -> None:
        raise KeyboardInterrupt

    with merchant(handler, clock, upgrade_tolerance=True) as client:
        if stage == "sleep":
            client._client._sleep = interrupted_sleep
        with pytest.raises(KeyboardInterrupt):
            invoke(client, "POST")
    assert calls == 1
    assert clock.delays == []
    assert closed == ([True] if stage == "body" else [])
    assert REQUEST_STATE.get() is None


@pytest.mark.parametrize("fault", ["api", "html", "body_timeout"])
def test_replayed_error_ends_upgrade_retries(fault: str) -> None:
    clock = Clock()
    calls = 0

    class SlowBody(httpx.SyncByteStream):
        def __iter__(self) -> Iterator[bytes]:
            clock.now += 16
            yield b"late"

    def handler(_: httpx.Request) -> httpx.Response:
        nonlocal calls
        calls += 1
        headers = {"Idempotent-Replayed": "True"}
        if fault == "api":
            return maintenance(headers=headers)
        if fault == "html":
            return httpx.Response(502, text="<html>down</html>", headers=headers)
        return httpx.Response(503, stream=SlowBody(), headers=headers)

    expected = TransportError if fault == "body_timeout" else ApiError
    with merchant(handler, clock, upgrade_tolerance=True) as client, pytest.raises(expected):
        invoke(client, "POST")
    assert calls == 1
    assert clock.delays == []


@pytest.mark.parametrize("method", ["DELETE", "PATCH"])
@pytest.mark.parametrize("fault", ["network", "html"])
def test_non_idempotent_methods_are_not_extended(method: str, fault: str) -> None:
    clock = Clock()
    calls = 0

    def handler(_: httpx.Request) -> httpx.Response:
        nonlocal calls
        calls += 1
        if fault == "network":
            raise httpx.ConnectError("offline")
        return httpx.Response(502, text="<html>down</html>")

    with merchant(handler, clock, upgrade_tolerance=True) as client:
        inner = client._client

        def operation() -> Any:
            return inner._client.get_httpx_client().request(method, "/test")

        with pytest.raises(TransportError if fault == "network" else ApiError):
            inner._call(operation, object)
    assert calls == 1
    assert clock.delays == []


def test_unreplayable_post_is_rejected_before_io() -> None:
    clock = Clock()
    calls: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        calls.append(request)
        return maintenance()

    with merchant(handler, clock, upgrade_tolerance=True) as client:
        inner = client._client

        def operation() -> Any:
            return inner._client.get_httpx_client().request("POST", "/test", content=iter([b"x"]))

        with pytest.raises(ConfigurationError, match="replayable"):
            inner._call(operation, object)
    assert calls == []


@pytest.mark.parametrize(
    ("status", "code"), [(500, "internal"), (429, "rate_limit"), (409, "idempotency_key_in_use")]
)
def test_other_retryable_errors_keep_ordinary_attempt_cap(status: int, code: str) -> None:
    clock = Clock()
    calls = 0

    def handler(_: httpx.Request) -> httpx.Response:
        nonlocal calls
        calls += 1
        if calls <= 5:
            return maintenance()
        return httpx.Response(status, json={"error": {"code": code}})

    with merchant(handler, clock, upgrade_tolerance=True) as client, pytest.raises(ApiError):
        invoke(client, "GET")
    assert calls == 6
    assert len(clock.delays) == 5


@pytest.mark.parametrize("fault", ["redirect", "invalid_success", "permanent"])
def test_terminal_responses_are_not_retried(fault: str) -> None:
    clock = Clock()
    calls = 0

    def handler(_: httpx.Request) -> httpx.Response:
        nonlocal calls
        calls += 1
        if fault == "redirect":
            return httpx.Response(302, headers={"Location": "https://other.test"})
        if fault == "invalid_success":
            return httpx.Response(200, text="<html>invalid</html>")
        return httpx.Response(400, json={"error": {"code": "bad_request"}})

    expected = ResponseValidationError if fault == "invalid_success" else ApiError
    with merchant(handler, clock, upgrade_tolerance=True) as client, pytest.raises(expected):
        invoke(client, "GET")
    assert calls == 1
    assert clock.delays == []


@pytest.mark.parametrize("method", ["GET", "POST"])
@pytest.mark.parametrize(
    ("client_option", "call_option"), [(False, None), (True, False), (False, True)]
)
def test_opt_in_and_per_call_override_preserve_interactive_defaults(
    method: str, client_option: bool, call_option: bool | None
) -> None:
    clock = Clock()
    timeouts: list[float] = []

    def handler(request: httpx.Request) -> httpx.Response:
        timeouts.append(request.extensions["timeout"]["read"])
        return maintenance() if len(timeouts) < 6 else httpx.Response(200, json=_quote())

    with merchant(handler, clock, upgrade_tolerance=client_option) as client:
        if call_option is True:
            invoke(client, method, upgrade_tolerance=call_option)
            assert len(timeouts) == 6
        else:
            with pytest.raises(ApiError):
                invoke(client, method, upgrade_tolerance=call_option)
            assert len(timeouts) == 4
        assert client._client._request_deadline is None
    assert all(timeout == 15 for timeout in timeouts)


@pytest.mark.parametrize("scope", ["client", "call"])
@pytest.mark.parametrize("invalid", [1, "true", []])
def test_upgrade_tolerance_requires_boolean_before_io(scope: str, invalid: Any) -> None:
    clock = Clock()
    calls: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        calls.append(request)
        return maintenance()

    with (
        pytest.raises(ConfigurationError, match="boolean"),
        merchant(
            handler, clock, **({"upgrade_tolerance": invalid} if scope == "client" else {})
        ) as client,
    ):
        invoke(client, "GET", **({"upgrade_tolerance": invalid} if scope == "call" else {}))
    assert calls == []


def test_default_total_deadline_includes_attempts_and_sleeps() -> None:
    clock = Clock()
    timeouts: list[float] = []

    def handler(request: httpx.Request) -> httpx.Response:
        timeouts.append(request.extensions["timeout"]["read"])
        clock.now += timeouts[-1]
        raise httpx.ReadTimeout("offline")

    with merchant(handler, clock) as client, pytest.raises(TransportError):
        invoke(client, "GET")
    assert timeouts == [15, 15, 15, 13.25]
    assert clock.now == 60


@pytest.mark.parametrize("status", [502, 503, 504])
@pytest.mark.parametrize(
    "body",
    [
        "<html>private</html>",
        "",
        {},
        {"error": {"code": "broken"}},
        {"error": {"code": "broken", "message": 42}},
    ],
)
@pytest.mark.parametrize("enabled", [False, True])
def test_gateway_envelope_classification_preserves_ordinary_mode(
    status: int, body: Any, enabled: bool
) -> None:
    clock = Clock()
    calls = 0

    def handler(_: httpx.Request) -> httpx.Response:
        nonlocal calls
        calls += 1
        headers = {"Retry-After": "301"}
        return (
            httpx.Response(status, text=body, headers=headers)
            if isinstance(body, str)
            else httpx.Response(status, json=body, headers=headers)
        )

    expected = TransportError if enabled else ApiError
    with (
        merchant(handler, clock, upgrade_tolerance=enabled) as client,
        pytest.raises(expected) as raised,
    ):
        invoke(client, "GET")
    assert calls == 1
    assert clock.delays == []
    assert "private" not in str(raised.value)
    assert raised.value.__context__ is None


@pytest.mark.parametrize("key", [None, "order-42"])
def test_upgrade_post_freezes_body_and_explicit_or_automatic_key(key: str | None) -> None:
    clock = Clock()
    sent: list[tuple[bytes, str]] = []
    metadata = {"order": "first"}

    def handler(request: httpx.Request) -> httpx.Response:
        sent.append((request.content, request.headers["Idempotency-Key"]))
        metadata["order"] = "changed"
        return maintenance() if len(sent) < 6 else httpx.Response(200, json=_quote())

    with merchant(handler, clock, upgrade_tolerance=True) as client:
        invoke(client, "POST", metadata=metadata, idempotency_key=key)
    assert len(sent) == 6
    assert len(set(sent)) == 1
    assert b'"first"' in sent[0][0]
    assert b'"changed"' not in sent[0][0]
    if key is not None:
        assert sent[0][1] == '"order-42"'


@pytest.mark.parametrize("method", ["GET", "POST"])
def test_attempt_body_timeout_activates_upgrade_budget(method: str) -> None:
    clock = Clock()
    calls = 0
    closed: list[bool] = []

    class SlowBody(httpx.SyncByteStream):
        def __iter__(self) -> Iterator[bytes]:
            clock.now += 15
            yield b"unfinished"

        def close(self) -> None:
            closed.append(True)

    def handler(_: httpx.Request) -> httpx.Response:
        nonlocal calls
        calls += 1
        if clock.now < 180:
            return httpx.Response(200, stream=SlowBody())
        return httpx.Response(200, json=_quote())

    with merchant(handler, clock, upgrade_tolerance=True) as client:
        invoke(client, method)
    assert calls > 4
    assert len(closed) == calls - 1
    assert 180 <= clock.now <= 200


@pytest.mark.parametrize("method", ["list", "list_page"])
def test_pagination_forwards_per_call_upgrade_tolerance(method: str) -> None:
    clock = Clock()
    calls = 0

    def handler(_: httpx.Request) -> httpx.Response:
        nonlocal calls
        calls += 1
        if calls < 6:
            return maintenance()
        return httpx.Response(
            200, json={"object": "list", "url": "/v1/quotes", "has_more": False, "data": [_quote()]}
        )

    with merchant(handler, clock) as client:
        result = getattr(client.quotes, method)(upgrade_tolerance=True)
        items = list(result) if method == "list" else result["data"]
    assert len(items) == 1
    assert calls == 6


def test_expired_wait_returns_last_error_without_another_request() -> None:
    clock = Clock()
    calls = 0

    def handler(_: httpx.Request) -> httpx.Response:
        nonlocal calls
        calls += 1
        return maintenance()

    def delayed_sleep(_: float) -> None:
        clock.now = 300

    with merchant(handler, clock, upgrade_tolerance=True) as client:
        client._client._sleep = delayed_sleep
        with pytest.raises(ApiError) as raised:
            invoke(client, "GET")
    assert raised.value.code == "service_maintenance"
    assert calls == 1
