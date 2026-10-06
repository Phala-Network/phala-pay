"""Total outbound I/O budgets for the synchronous reference-product handlers."""

from __future__ import annotations

import asyncio
import threading
import time
from contextvars import ContextVar

import httpx

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
        self._loop = asyncio.new_event_loop()
        self._thread = threading.Thread(target=self._run, name="product-http", daemon=True)
        self._thread.start()
        try:
            self._client = asyncio.run_coroutine_threadsafe(
                self._open_client(), self._loop
            ).result()
        except BaseException:
            self._loop.call_soon_threadsafe(self._loop.stop)
            self._thread.join()
            raise

    async def _open_client(self) -> httpx.AsyncClient:
        return httpx.AsyncClient(follow_redirects=False)

    def _run(self) -> None:
        asyncio.set_event_loop(self._loop)
        try:
            self._loop.run_forever()
        finally:
            pending = asyncio.all_tasks(self._loop)
            for task in pending:
                task.cancel()
            if pending:
                self._loop.run_until_complete(asyncio.gather(*pending, return_exceptions=True))
            self._loop.run_until_complete(self._loop.shutdown_asyncgens())
            self._loop.close()

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
            async with asyncio.timeout(max(0, deadline - time.monotonic())):
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
            future = asyncio.run_coroutine_threadsafe(exchange(), self._loop)
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
                asyncio.run_coroutine_threadsafe(self._client.aclose(), self._loop).result()
            finally:
                self._loop.call_soon_threadsafe(self._loop.stop)
                self._thread.join()
