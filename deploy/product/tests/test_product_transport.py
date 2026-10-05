"""The ASGI boundary bounds malformed, oversized, stalled and concurrent requests."""

from __future__ import annotations

import asyncio
import gzip
import os
import select
import signal
import socket
import subprocess
import sys
import threading
import time
from collections.abc import AsyncIterator, Iterator
from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack, suppress
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from types import SimpleNamespace
from typing import cast
from unittest.mock import Mock

import httpx
import pytest
from starlette.applications import Starlette
from starlette.testclient import TestClient

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from reference_product import server as transport
from reference_product.demo import DemoConsole
from reference_product.demo import Response as DemoResponse
from reference_product.fulfillment import Answer
from reference_product.server import MAX_BODY_BYTES, ProductServer
from reference_product.transport import DeadlineTransport, operation_deadline


@pytest.fixture
def product() -> Iterator[ProductServer]:
    fulfillment = Mock()
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    fulfillment.config = SimpleNamespace(
        public_url="http://localhost/topup", listen_host="127.0.0.1", listen_port=port
    )
    fulfillment.handle.return_value = Answer(200, {"received": True})
    product = ProductServer(fulfillment)
    try:
        yield product
    finally:
        product.close()


@pytest.mark.parametrize("length", ["-1", "invalid", "1.5", "+2", "9" * 5000])
def test_bad_content_length(product: ProductServer, length: str) -> None:
    with TestClient(product.app) as client:
        response = client.post("/topup/webhooks", content=b"{}", headers={"content-length": length})
    assert response.status_code == 400
    assert response.json() == {"code": "bad_request"}
    cast(Mock, product.fulfillment.handle).assert_not_called()


@pytest.mark.parametrize(
    ("body", "length", "status"),
    [
        (b"{}", "3", 400),
        (b"{}", str(MAX_BODY_BYTES + 1), 413),
        (b"x" * (MAX_BODY_BYTES + 1), None, 413),
    ],
)
def test_body_limits(product: ProductServer, body: bytes, length: str | None, status: int) -> None:
    headers = {} if length is None else {"content-length": length}
    with TestClient(product.app) as client:
        response = client.post("/topup/webhooks", content=iter([body]), headers=headers)
    assert response.status_code == status
    cast(Mock, product.fulfillment.handle).assert_not_called()


def test_preserves_dispatch_and_hides_internal_errors(product: ProductServer) -> None:
    with TestClient(product.app) as client:
        assert client.get("/topup/healthz").json() == {"status": "ok"}
        assert client.post("/topup/webhooks", content=b"{}").json() == {"received": True}
        cast(Mock, product.fulfillment.handle).side_effect = RuntimeError("private detail")
        response = client.post("/topup/webhooks", content=b"{}")
        assert response.status_code == 500
        assert response.json() == {"code": "internal_server_error"}


def test_stalled_body_times_out(product: ProductServer, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(transport, "REQUEST_TIMEOUT_SECONDS", 0.01)

    async def body() -> AsyncIterator[bytes]:
        await asyncio.sleep(1)
        yield b"{}"

    async def run() -> httpx.Response:
        async with httpx.AsyncClient(
            transport=httpx.ASGITransport(product.app), base_url="http://test"
        ) as client:
            return await client.post("/topup/webhooks", content=body())

    response = asyncio.run(run())
    assert response.status_code == 408
    assert response.json() == {"code": "request_timeout"}
    cast(Mock, product.fulfillment.handle).assert_not_called()


def test_timed_out_workers_remain_bounded(
    product: ProductServer, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(transport, "REQUEST_TIMEOUT_SECONDS", 0.02)
    product._webhook_capacity = threading.BoundedSemaphore(1)
    done = threading.Event()
    cast(Mock, product.fulfillment.handle).side_effect = lambda *_: (done.wait(2), Answer(200))[1]
    try:
        with TestClient(product.app) as client:
            assert client.post("/topup/webhooks", content=b"{}").status_code == 408
            assert client.post("/topup/webhooks", content=b"{}").status_code == 503
            assert cast(Mock, product.fulfillment.handle).call_count == 1
    finally:
        done.set()


@pytest.mark.parametrize("length", ["-1", "invalid"])
def test_rejects_invalid_wire_framing(product: ProductServer, length: str) -> None:
    with product:
        port = product.fulfillment.config.listen_port
        with socket.create_connection(("127.0.0.1", port), timeout=2) as connection:
            connection.sendall(
                (
                    "POST /topup/webhooks HTTP/1.1\r\nHost: localhost\r\n"
                    f"Content-Length: {length}\r\nConnection: close\r\n\r\n"
                ).encode()
            )
            assert connection.recv(4096).startswith(b"HTTP/1.1 400")
    assert not product._thread.is_alive()
    cast(Mock, product.fulfillment.handle).assert_not_called()


@pytest.mark.parametrize("protected", [True, False])
def test_slow_headers_have_total_deadline_and_do_not_503(
    product: ProductServer, protected: bool
) -> None:
    assert product._server.http1_settings is not None
    product._server.http1_settings.header_read_timeout = 300 if protected else 30_000
    with product, ExitStack() as stack:
        port = product.fulfillment.config.listen_port
        # Exceed the worker admission cap: queued connections must also eventually drain.
        peers = [
            stack.enter_context(socket.create_connection(("127.0.0.1", port), timeout=2))
            for _ in range(2 * transport.CONNECTION_LIMIT)
        ]
        for peer in peers:
            peer.sendall(b"POST /topup/webhooks HTTP/1.1\r\nHost: localhost\r\nX-Slow: ")
        stop = threading.Event()

        def trickle() -> None:
            while not stop.wait(0.02):
                for peer in peers:
                    with suppress(OSError):
                        peer.sendall(b"x")

        thread = threading.Thread(target=trickle)
        thread.start()

        def assert_closed() -> None:
            pending = set(peers)
            deadline = time.monotonic() + 1.5
            while pending and time.monotonic() < deadline:
                readable, _, _ = select.select(list(pending), [], [], 0.05)
                for peer in readable:
                    with suppress(ConnectionResetError):
                        assert peer.recv(4096) == b""
                    pending.remove(peer)
            assert not pending, "slow header connections remained open past the total deadline"

        try:
            if protected:
                assert_closed()
                with httpx.Client(base_url=f"http://127.0.0.1:{port}", timeout=2) as client:
                    assert client.get("/topup/healthz").status_code == 200
                    assert client.post("/topup/webhooks", content=b"{}").status_code == 200
            else:
                # Mutation control: continuous trickling must fail the same closure assertion
                # when the deadline is removed. An idle read timeout would fail this too.
                with pytest.raises(AssertionError, match="remained open"):
                    assert_closed()
        finally:
            stop.set()
            thread.join()


@pytest.mark.parametrize("protected", [True, False])
def test_connection_admission_and_os_backlog_bound_slow_headers(
    product: ProductServer, protected: bool
) -> None:
    # Keep headers incomplete throughout the admission probe so no deadline releases permits.
    assert product._server.http1_settings is not None
    product._server.http1_settings.header_read_timeout = 30_000
    if not protected:
        product._server.backpressure = 10_000
    with product, ExitStack() as stack:
        port = product.fulfillment.config.listen_port
        refused = False
        # Linux's completed accept queue can hold backlog + 1. Probe beyond that plus all
        # worker permits. TCP handshakes can succeed while sockets are still in the OS queue;
        # they are not yet accepted by Granian and their header timer has not started.
        for _ in range(transport.CONNECTION_LIMIT + transport.LISTEN_BACKLOG + 16):
            try:
                peer = stack.enter_context(
                    socket.create_connection(("127.0.0.1", port), timeout=0.1)
                )
                peer.sendall(b"GET /topup/healthz HTTP/1.1\r\nHost: localhost\r\nX-Slow: ")
            except (TimeoutError, ConnectionRefusedError, ConnectionResetError):
                refused = True
                break

        def assert_bounded() -> None:
            assert refused, "excess connections were admitted beyond worker cap and OS backlog"

        if protected:
            assert_bounded()
        else:
            # Removing worker admission accepts the entire probe: this regression check must
            # fail even though the OS backlog and header deadline still exist.
            with pytest.raises(AssertionError, match="excess connections were admitted"):
                assert_bounded()


def test_shutdown_drains_trickling_sync_network_work(
    product: ProductServer, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(transport, "OPERATION_TIMEOUT_SECONDS", 0.4)
    entered = threading.Event()
    disconnected = threading.Event()

    class Upstream(BaseHTTPRequestHandler):
        def do_GET(self) -> None:
            self.send_response(200)
            self.send_header("Content-Length", "10000")
            self.end_headers()
            entered.set()
            try:
                for _ in range(10000):
                    self.wfile.write(b"x")
                    self.wfile.flush()
                    time.sleep(0.01)
            except (BrokenPipeError, ConnectionResetError):
                disconnected.set()

        def log_message(self, *_: object) -> None:
            pass

    upstream = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    upstream_thread = threading.Thread(target=upstream.serve_forever)
    upstream_thread.start()

    def handle(*_: object) -> Answer:
        with httpx.Client(transport=DeadlineTransport(), timeout=5) as client:
            client.get(f"http://127.0.0.1:{upstream.server_port}/")
        return Answer(200)

    cast(Mock, product.fulfillment.handle).side_effect = handle
    try:
        product.__enter__()
        with socket.create_connection(
            ("127.0.0.1", product.fulfillment.config.listen_port), timeout=2
        ) as peer:
            peer.sendall(
                b"POST /topup/webhooks HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n"
            )
            assert entered.wait(2)
            started = time.monotonic()
            product.__exit__()
            assert time.monotonic() - started < 2
            assert disconnected.wait(2)
            assert all(not thread.is_alive() for thread in product._workers._threads)
    finally:
        product.__exit__()
        upstream.shutdown()
        upstream.server_close()
        upstream_thread.join()


def _blocked_application(marker: str) -> Starlette:
    """A noncooperative handler exercises the production supervisor's final kill deadline."""
    fulfillment = Mock()
    fulfillment.config = SimpleNamespace(
        public_url="http://localhost/topup", listen_host="127.0.0.1", listen_port=0
    )

    def handle(*_: object) -> Answer:
        Path(marker).touch()
        time.sleep(60)
        return Answer(200)

    fulfillment.handle.side_effect = handle
    return ProductServer(fulfillment).app


def test_supervisor_bounds_shutdown_of_noncooperative_sync_work(tmp_path: Path) -> None:
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    marker = tmp_path / "in-flight"
    script = (
        "from functools import partial\n"
        "from granian import Granian\n"
        "from test_product_transport import _blocked_application\n"
        "from reference_product.server import _server_options\n"
        "import sys\n"
        "Granian('', address='127.0.0.1', port=int(sys.argv[1]), workers=1, "
        "workers_kill_timeout=1, **_server_options()).serve("
        "target_loader=partial(_blocked_application, sys.argv[2]), wrap_loader=False)\n"
    )
    env = {
        **os.environ,
        "PYTHONPATH": os.pathsep.join(
            [str(Path(__file__).parent), str(Path(__file__).resolve().parents[1])]
        ),
    }
    with (tmp_path / "server.log").open("w") as log:
        process = subprocess.Popen(  # noqa: S603 — fixed local test command
            [sys.executable, "-c", script, str(port), str(marker)],
            env=env,
            stdout=log,
            stderr=log,
            start_new_session=True,
        )
        try:
            deadline = time.monotonic() + 10
            while True:
                try:
                    peer = socket.create_connection(("127.0.0.1", port), timeout=0.2)
                    break
                except OSError:
                    assert process.poll() is None
                    assert time.monotonic() < deadline
                    time.sleep(0.02)
            with peer:
                peer.sendall(
                    b"POST /topup/webhooks HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n"
                )
                while not marker.exists():
                    assert time.monotonic() < deadline
                    time.sleep(0.02)
                started = time.monotonic()
                process.send_signal(signal.SIGTERM)
                assert process.wait(timeout=4) == 0
                assert time.monotonic() - started < 3
        finally:
            with suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)


def test_outbound_calls_share_total_budget_and_preserve_compressed_body(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async_client = httpx.AsyncClient
    calls: list[httpx.Request] = []

    async def upstream(request: httpx.Request) -> httpx.Response:
        calls.append(request)
        await asyncio.sleep(0.4)
        return httpx.Response(
            200, headers={"Content-Encoding": "gzip"}, stream=httpx.ByteStream(gzip.compress(b"ok"))
        )

    monkeypatch.setattr(
        httpx,
        "AsyncClient",
        lambda **kw: async_client(transport=httpx.MockTransport(upstream), **kw),
    )
    token = operation_deadline.set(time.monotonic() + 0.6)
    try:
        with httpx.Client(transport=DeadlineTransport()) as client:
            assert client.post("http://test/first?key=value", content=iter([b"body"])).text == "ok"
            with pytest.raises(httpx.TimeoutException, match="operation deadline"):
                client.get("http://test/second")
        assert len(calls) == 2
        assert calls[0].content == b"body"
        assert calls[0].url.query == b"key=value"
    finally:
        operation_deadline.reset(token)


def test_outbound_calls_reuse_one_connection() -> None:
    connections: set[tuple[str, int]] = set()

    class Upstream(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_GET(self) -> None:
            connections.add(self.client_address)
            self.send_response(200)
            self.send_header("Content-Length", "2")
            self.end_headers()
            self.wfile.write(b"ok")

        def log_message(self, *_: object) -> None:
            pass

    upstream = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    thread = threading.Thread(target=upstream.serve_forever)
    thread.start()
    transport = DeadlineTransport()
    try:
        with httpx.Client(transport=transport) as client:
            for path in ("first", "second"):
                assert client.get(f"http://127.0.0.1:{upstream.server_port}/{path}").text == "ok"
        assert len(connections) == 1
        assert not transport._thread.is_alive()
        transport.close()
    finally:
        transport.close()
        upstream.shutdown()
        upstream.server_close()
        thread.join()


def test_expired_outbound_deadline_never_calls_upstream(monkeypatch: pytest.MonkeyPatch) -> None:
    async_client = httpx.AsyncClient
    calls: list[httpx.Request] = []

    async def upstream(request: httpx.Request) -> httpx.Response:
        calls.append(request)
        return httpx.Response(200)

    monkeypatch.setattr(
        httpx,
        "AsyncClient",
        lambda **kw: async_client(transport=httpx.MockTransport(upstream), **kw),
    )
    token = operation_deadline.set(time.monotonic() - 1)
    try:
        with (
            httpx.Client(transport=DeadlineTransport()) as client,
            pytest.raises(httpx.TimeoutException, match="operation deadline"),
        ):
            client.get("http://test/")
        assert not calls
    finally:
        operation_deadline.reset(token)


def test_pooled_transport_keeps_concurrent_callers_deadlines_independent(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async_client = httpx.AsyncClient

    async def upstream(request: httpx.Request) -> httpx.Response:
        await asyncio.sleep(0.1)
        return httpx.Response(200, stream=httpx.ByteStream(b"ok"))

    monkeypatch.setattr(
        httpx,
        "AsyncClient",
        lambda **kw: async_client(transport=httpx.MockTransport(upstream), **kw),
    )
    with httpx.Client(transport=DeadlineTransport()) as client:

        def call(timeout: float) -> str:
            token = operation_deadline.set(time.monotonic() + timeout)
            try:
                return client.get("http://test/").text
            finally:
                operation_deadline.reset(token)

        with ThreadPoolExecutor(max_workers=2) as workers:
            short = workers.submit(call, 0.02)
            long = workers.submit(call, 2)
            with pytest.raises(httpx.TimeoutException, match="operation deadline"):
                short.result(timeout=2)
            assert long.result(timeout=2) == "ok"


def test_webhooks_have_reserved_workers_and_slots(product: ProductServer) -> None:
    done = threading.Event()
    for _ in range(16):
        assert product._capacity.acquire(blocking=False)
        product._workers.submit(done.wait, 5)
    try:
        with TestClient(product.app) as client:
            assert client.get("/topup/healthz").status_code == 503
            assert client.post("/topup/webhooks", content=b"{}").status_code == 200
        cast(Mock, product.fulfillment.handle).assert_called_once()
    finally:
        done.set()
        for _ in range(16):
            product._capacity.release()


def test_demo_gets_have_eight_second_operation_deadlines(product: ProductServer) -> None:
    demo = Mock(spec=DemoConsole)
    demo.handles.return_value = True
    remaining: list[float] = []

    def handle(*args: object) -> DemoResponse:
        deadline = operation_deadline.get()
        assert deadline is not None
        remaining.append(deadline - time.monotonic())
        return DemoResponse(HTTPStatus.OK, b"{}")

    demo.handle.side_effect = handle
    product.demo = cast(DemoConsole, demo)
    with TestClient(product.app) as client:
        assert client.get("/topup/api/assets").status_code == 200
        assert client.post("/topup/api/quotes", content=b"{}").status_code == 200
    assert 7.5 < remaining[0] <= 8
    assert 24.5 < remaining[1] <= 25
