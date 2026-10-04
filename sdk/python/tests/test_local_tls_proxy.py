"""The disposable Docker ingress keeps SDK tests on verified HTTPS."""

from __future__ import annotations

import asyncio
import ssl
import sys
from datetime import UTC, datetime, timedelta
from functools import partial
from pathlib import Path

import pytest
from cryptography import x509
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.x509.oid import NameOID

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "deploy/local"))

from tls_proxy import forward


def test_local_tls_ingress_verifies_certificate_and_preserves_wire_bytes(tmp_path: Path) -> None:
    key = Ed25519PrivateKey.generate()
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "topup-tls")])
    now = datetime.now(UTC)
    certificate = (
        x509.CertificateBuilder()
        .subject_name(name)
        .issuer_name(name)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - timedelta(minutes=1))
        .not_valid_after(now + timedelta(days=1))
        .add_extension(x509.SubjectAlternativeName([x509.DNSName("topup-tls")]), critical=False)
        .sign(key, None)
    )
    cert_path, key_path = tmp_path / "cert.pem", tmp_path / "key.pem"
    cert_path.write_bytes(certificate.public_bytes(serialization.Encoding.PEM))
    key_path.write_bytes(
        key.private_bytes(
            serialization.Encoding.PEM,
            serialization.PrivateFormat.PKCS8,
            serialization.NoEncryption(),
        )
    )
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(cert_path, key_path)
    trusted = ssl.create_default_context(cafile=cert_path)
    request = (
        b"POST /v1/test?a=%26 HTTP/1.1\r\nAuthorization: Bearer test-key\r\n"
        b"Content-Length: 4\r\n\r\nbody"
    )
    response = b"HTTP/1.1 307 Temporary Redirect\r\nLocation: /other\r\nContent-Length: 2\r\n\r\nok"
    received: list[bytes] = []

    async def upstream(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            received.append(await reader.readuntil(b"\r\n\r\n") + await reader.readexactly(4))
            writer.write(response)
            await writer.drain()
        finally:
            writer.close()
            await writer.wait_closed()

    async def run() -> None:
        async with await asyncio.start_server(upstream, "127.0.0.1", 0) as backend:
            port = backend.sockets[0].getsockname()[1]
            handler = partial(forward, upstream_host="127.0.0.1", upstream_port=port)
            async with await asyncio.start_server(handler, "127.0.0.1", 0, ssl=context) as proxy:
                tls_port = proxy.sockets[0].getsockname()[1]
                with pytest.raises(ssl.SSLCertVerificationError):
                    await asyncio.open_connection(
                        "127.0.0.1",
                        tls_port,
                        ssl=ssl.create_default_context(),
                        server_hostname="topup-tls",
                    )
                with pytest.raises(ssl.SSLCertVerificationError):
                    await asyncio.open_connection(
                        "127.0.0.1",
                        tls_port,
                        ssl=trusted,
                        server_hostname="wrong-host",
                    )
                reader, writer = await asyncio.open_connection(
                    "127.0.0.1",
                    tls_port,
                    ssl=trusted,
                    server_hostname="topup-tls",
                )
                try:
                    writer.write(request)
                    await writer.drain()
                    assert await asyncio.wait_for(reader.read(), timeout=2) == response
                finally:
                    writer.close()
                    await writer.wait_closed()
        assert received == [request]

    asyncio.run(run())
