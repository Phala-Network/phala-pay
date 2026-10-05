#!/usr/bin/env python3
"""Hermetic HTTPS ticker responses for the CVM rehearsal."""

from http.server import BaseHTTPRequestHandler, HTTPServer
import sys


class Handler(BaseHTTPRequestHandler):
    def do_GET(self) -> None:  # noqa: N802 - stdlib handler API
        if self.path.startswith("/0/public/Ticker"):
            body = b'{"error":[],"result":{"PHAUSD":{"c":["2.00000000"]}}}'
        elif self.path.startswith("/api/v3/ticker/price"):
            body = b'{"symbol":"PHAUSDT","price":"2.00000000"}'
        else:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_args: object) -> None:
        return


HTTPServer(("0.0.0.0", int(sys.argv[1])), Handler).serve_forever()
