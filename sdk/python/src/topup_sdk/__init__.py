"""Python SDK for the Phala Pay service product API.

`topup_client` is generated from the service's OpenAPI document; this package adds request
signing, inbound verification, deterministic address helpers, and an idempotent client.
"""

from .addresses import (
    deposit_address,
    deposit_address_salt,
    deposit_id,
    forwarder_address,
    quote_address,
    quote_salt,
)
from .attestation import attestation_report_data, verify_attestation_binding
from .client import TopupClient
from .errors import (
    AddressMismatchError,
    ApiError,
    ApiErrorCode,
    KnownErrorCode,
    AttestationError,
    SignatureError,
    TopupError,
    UnpinnedTreasuryWarning,
)
from .export import export_account
from .fulfillment import (
    CREDITED_EVENT,
    CreditedDeposit,
    FulfillmentError,
    credited_event_id,
)
from .signing import RequestSigner, SigningAuth, VerifiedRequest, load_public_key, verify_request
from .sweeps import (
    batch_checksum,
    flush_transaction,
    flush_transactions,
    safe_batch,
    write_safe_batch,
)
from .treasury import sign_treasury_challenge
from .webhooks import (
    WebhookEvent,
    load_webhook_public_key,
    sign_webhook,
    verify_webhook,
    verify_webhook_signature,
    webhook_public_key_bytes,
)

__all__ = [
    "CREDITED_EVENT",
    "AddressMismatchError",
    "ApiError",
    "ApiErrorCode",
    "KnownErrorCode",
    "AttestationError",
    "CreditedDeposit",
    "FulfillmentError",
    "RequestSigner",
    "SignatureError",
    "SigningAuth",
    "TopupClient",
    "TopupError",
    "UnpinnedTreasuryWarning",
    "VerifiedRequest",
    "WebhookEvent",
    "attestation_report_data",
    "batch_checksum",
    "credited_event_id",
    "deposit_address",
    "deposit_address_salt",
    "deposit_id",
    "export_account",
    "flush_transaction",
    "flush_transactions",
    "forwarder_address",
    "load_public_key",
    "load_webhook_public_key",
    "quote_address",
    "quote_salt",
    "safe_batch",
    "sign_treasury_challenge",
    "sign_webhook",
    "verify_attestation_binding",
    "verify_request",
    "verify_webhook",
    "verify_webhook_signature",
    "webhook_public_key_bytes",
    "write_safe_batch",
]
