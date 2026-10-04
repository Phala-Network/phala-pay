"""Canonical origins shared by transport and trust configuration."""

from __future__ import annotations

from urllib.parse import urlsplit

from .errors import ConfigurationError


def normalize_origin(value: str, *, test: bool) -> str:
    try:
        parsed = urlsplit(value)
        port = parsed.port
    except (TypeError, ValueError):
        raise ConfigurationError("api_base must be a valid origin") from None
    if (
        not parsed.hostname
        or parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
        or "?" in value
        or "#" in value
        or parsed.path not in {"", "/"}
        or any(c.isspace() for c in value)
    ):
        raise ConfigurationError(
            "api_base must be an origin without credentials, query, fragment or path"
        )
    if parsed.scheme != "https" and not (
        parsed.scheme == "http" and test and parsed.hostname in {"127.0.0.1", "localhost", "::1"}
    ):
        raise ConfigurationError("api_base must use HTTPS (test mode permits loopback HTTP)")
    host = parsed.hostname.lower()
    if ":" in host:
        host = f"[{host}]"
    suffix = (
        ""
        if port is None or (parsed.scheme, port) in {("https", 443), ("http", 80)}
        else f":{port}"
    )
    return f"{parsed.scheme.lower()}://{host}{suffix}"
