from topup_sdk.errors import TopupError


class PhalaPayError(TopupError):
    pass


class ConfigurationError(PhalaPayError):
    pass


class ResponseValidationError(PhalaPayError):
    pass


class TransportError(PhalaPayError):
    def __init__(self, code: str, message: str = "transport failure") -> None:
        self.code = code
        super().__init__(message)


class LedgerSnapshotError(PhalaPayError):
    pass
