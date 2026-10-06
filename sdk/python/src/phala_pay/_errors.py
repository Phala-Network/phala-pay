"""Public errors share the transport's exception hierarchy."""

from topup_sdk.errors import ConfigurationError, ResponseValidationError, TransportError
from topup_sdk.errors import TopupError as PhalaPayError


class LedgerSnapshotError(PhalaPayError):
    pass


__all__ = [
    "ConfigurationError",
    "LedgerSnapshotError",
    "PhalaPayError",
    "ResponseValidationError",
    "TransportError",
]
