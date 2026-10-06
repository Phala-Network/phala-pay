"""Total outbound I/O budgets for the synchronous reference-product handlers."""

from __future__ import annotations

import threading
import time
from contextlib import ExitStack
from contextvars import ContextVar

import anyio
import httpx
from anyio.from_thread import start_blocking_portal

OPERATION_TIMEOUT_SECONDS = 25
operation_deadline: ContextVar[float | None] = ContextVar("operation_deadline", default=None)


class DeadlineTransport(httpx.BaseTransport):
    """Pool cancellable async HTTP I/O beneath the SDK's synchronous transport interface.

    HTTPX's phase timeouts alone do not stop a peer that continuously trickles bytes. The
    shared handler deadline covers the entire response, including all pages and SDK calls.
    One event loop and client serve exchanges submitted by the handler threads.
    """

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._closed = False
        self._resources = ExitStack()
        self._portal = self._resources.enter_context(
            start_blocking_portal(backend="asyncio", name="product-http")
        )
        try:
            self._client = self._portal.call(self._open_client)
        except BaseException:
            self._resources.close()
            raise

    async def _open_client(self) -> httpx.AsyncClient:
        return httpx.AsyncClient(follow_redirects=False)

    def handle_request(self, request: httpx.Request) -> httpx.Response:
        request = httpx.Request(
            request.method,
            request.url,
            headers=request.headers,
            content=request.read(),
            extensions=request.extensions,
        )
        deadline = operation_deadline.get()
        if deadline is None:
            deadline = time.monotonic() + OPERATION_TIMEOUT_SECONDS
        if deadline <= time.monotonic():
            raise httpx.TimeoutException("product operation deadline exceeded", request=request)

        async def exchange() -> httpx.Response:
            with anyio.fail_after(max(0, deadline - time.monotonic())):
                response = await self._client.send(request, stream=True)
                try:
                    content = b"".join([chunk async for chunk in response.aiter_raw()])
                    # Preserve wire headers and let the sync response decode content once.
                    return httpx.Response(
                        response.status_code,
                        headers=response.headers,
                        content=content,
                        extensions=response.extensions,
                    )
                finally:
                    await response.aclose()

        with self._lock:
            if self._closed:
                raise RuntimeError("product transport is closed")
            future = self._portal.start_task_soon(exchange)
        try:
            return future.result(timeout=max(0, deadline - time.monotonic()))
        except TimeoutError as error:
            future.cancel()
            raise httpx.TimeoutException(
                "product operation deadline exceeded", request=request
            ) from error

    def close(self) -> None:
        with self._lock:
            if self._closed:
                return
            self._closed = True
            try:
                self._portal.call(self._client.aclose)
            finally:
                self._resources.close()
