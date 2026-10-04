"""One synchronous request path for replay, deadlines and borrowed transport ownership."""

from __future__ import annotations

from collections.abc import Callable, Iterator
from contextvars import ContextVar
from dataclasses import dataclass
from typing import Any

import httpx

from .errors import ConfigurationError, TransportError


@dataclass
class RequestState:
    deadline: float
    idempotency_key: str | None
    request: httpx.Request | None = None
    response: httpx.Response | None = None


REQUEST_STATE: ContextVar[RequestState | None] = ContextVar("phala_pay_request", default=None)


class BorrowedTransport(httpx.BaseTransport):
    def __init__(self, transport: httpx.BaseTransport) -> None:
        self.transport = transport

    def handle_request(self, request: httpx.Request) -> httpx.Response:
        return self.transport.handle_request(request)

    def close(self) -> None:
        """The injector retains ownership."""


class DeadlineStream(httpx.SyncByteStream):
    def __init__(
        self, stream: httpx.SyncByteStream, deadline: float, clock: Callable[[], float]
    ) -> None:
        self.stream, self.deadline, self.clock = stream, deadline, clock

    def __iter__(self) -> Iterator[bytes]:
        for chunk in self.stream:
            if self.clock() >= self.deadline:
                raise httpx.ReadTimeout("response body exceeded attempt deadline")
            yield chunk
        if self.clock() >= self.deadline:
            raise httpx.ReadTimeout("response body exceeded attempt deadline")

    def close(self) -> None:
        self.stream.close()


class HTTPClient(httpx.Client):
    def __init__(
        self,
        *,
        attempt_timeout: float,
        clock: Callable[[], float],
        key_factory: Callable[[str | None], str],
        on_response: Callable[[httpx.Response], None],
        **kwargs: Any,
    ) -> None:
        self.attempt_timeout, self.clock, self.key_factory = attempt_timeout, clock, key_factory
        self.on_response = on_response
        super().__init__(**kwargs)

    def request(self, method: str, url: httpx.URL | str, **kwargs: Any) -> httpx.Response:
        state = REQUEST_STATE.get()
        if state is None:
            return super().request(method, url, **kwargs)
        if state.request is None:
            state.request = self.build_request(method, url, **kwargs)
            if method.upper() == "POST" and not isinstance(state.request.stream, httpx.ByteStream):
                raise ConfigurationError("POST body must be replayable")
            # Buffer the body once before authenticated IO.
            state.request.read()
            if method.upper() == "POST":
                state.request.headers["Idempotency-Key"] = self.key_factory(state.idempotency_key)
        request = state.request
        remaining = state.deadline - self.clock()
        if remaining <= 0:
            raise TransportError("timeout")
        timeout = min(self.attempt_timeout, remaining)
        request.extensions["timeout"] = httpx.Timeout(timeout).as_dict()
        attempt_deadline = self.clock() + timeout
        response = self.send(request, stream=True, follow_redirects=False)
        state.response = response
        if not isinstance(response.stream, httpx.SyncByteStream):
            raise TransportError("network")
        response.stream = DeadlineStream(response.stream, attempt_deadline, self.clock)
        try:
            response.read()
        finally:
            response.close()
        self.on_response(response)
        return response
