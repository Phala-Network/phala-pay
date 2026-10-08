#!/usr/bin/env python3
"""Hermetic SLS publication endpoints for the CVM rehearsal's production binary."""

import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import ssl
import sys


xml = Path(sys.argv[2]).read_bytes()
preview = json.dumps([{
    "fileName": "SDN.XML",
    "hashCodes": {"SHA-256": hashlib.sha256(xml).hexdigest()},
    "lastUpdated": "2026-10-05T00:00:00Z",
}]).encode()


class Handler(BaseHTTPRequestHandler):
    def respond(self, body: bytes, content_type: str) -> None:
        self.send_response(200)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802 - stdlib handler API
        if self.path == "/api/download/SDN.XML":
            self.respond(xml, "application/xml")
        else:
            self.send_error(404)

    def do_POST(self) -> None:  # noqa: N802 - stdlib handler API
        if self.path == "/api/PublicationPreview/SdnList":
            self.respond(preview, "application/json")
        else:
            self.send_error(404)

    def log_message(self, *_args: object) -> None:
        return


context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(sys.argv[3], sys.argv[4])
server = ThreadingHTTPServer(("0.0.0.0", int(sys.argv[1])), Handler)
server.socket = context.wrap_socket(server.socket, server_side=True)
print(server.server_port, flush=True)
server.serve_forever()
