"""Public errors share the transport's exception hierarchy."""

from topup_sdk.errors import ConfigurationError, ResponseValidationError, TopupError, TransportError

PhalaPayError = TopupError


class LedgerSnapshotError(TopupError):
    pass


__all__ = [
    "ConfigurationError",
    "LedgerSnapshotError",
    "PhalaPayError",
    "ResponseValidationError",
    "TransportError",
]
