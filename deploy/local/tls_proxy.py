"""Disposable TLS ingress for local SDK rehearsals; never used by a deployed service."""

from __future__ import annotations

import asyncio
import ssl
from contextlib import suppress
from pathlib import Path


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
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(Path("/etc/test-tls/cert.pem"), Path("/etc/test-tls/key.pem"))
    server = await asyncio.start_server(
        forward,
        "0.0.0.0",  # noqa: S104 - private Compose network, no published port
        8443,
        ssl=context,
        ssl_handshake_timeout=5,
    )
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    asyncio.run(main())
