"""Disposable TLS ingress for local SDK rehearsals; never used by a deployed service."""

from __future__ import annotations

import argparse
import asyncio
import ssl
from contextlib import suppress
from functools import partial
from pathlib import Path
from urllib.parse import urlsplit


async def _copy(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
    while chunk := await reader.read(65536):
        writer.write(chunk)
        await writer.drain()


async def forward(
    reader: asyncio.StreamReader,
    writer: asyncio.StreamWriter,
    *,
    upstream_host: str = "topup",
    upstream_port: int = 8080,
) -> None:
    upstream: asyncio.StreamWriter | None = None
    try:
        async with asyncio.timeout(30):
            source, upstream = await asyncio.open_connection(upstream_host, upstream_port)
            tasks = [
                asyncio.create_task(_copy(reader, upstream)),
                asyncio.create_task(_copy(source, writer)),
            ]
            try:
                # EOF in either direction ends this exchange, without an idle relay task.
                done, _ = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
                for task in done:
                    task.result()
            finally:
                for task in tasks:
                    task.cancel()
                await asyncio.gather(*tasks, return_exceptions=True)
    except (OSError, TimeoutError):
        pass
    finally:
        for stream in (upstream, writer):
            if stream is not None:
                stream.close()
                with suppress(OSError, TimeoutError):
                    await asyncio.wait_for(stream.wait_closed(), timeout=5)


async def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--certificate", type=Path, default=Path("/etc/test-tls/cert.pem"))
    parser.add_argument("--key", type=Path, default=Path("/etc/test-tls/key.pem"))
    parser.add_argument("--upstream", default="http://topup:8080")
    parser.add_argument("--bind", default="0.0.0.0")
    parser.add_argument("--port", type=int, default=8443)
    args = parser.parse_args()
    upstream = urlsplit(args.upstream)
    if upstream.scheme != "http" or not upstream.hostname or not upstream.port:
        parser.error("upstream must be an explicit http://host:port")
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(args.certificate, args.key)
    server = await asyncio.start_server(
        partial(forward, upstream_host=upstream.hostname, upstream_port=upstream.port),
        args.bind,
        args.port,
        ssl=context,
        ssl_handshake_timeout=5,
    )
    print(server.sockets[0].getsockname()[1], flush=True)
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    asyncio.run(main())
