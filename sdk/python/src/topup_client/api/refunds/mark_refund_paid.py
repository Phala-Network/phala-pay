from http import HTTPStatus
from typing import Any, cast
from urllib.parse import quote

import httpx

from ...client import AuthenticatedClient, Client
from ...types import Response, UNSET
from ... import errors

from ...models.error_response import ErrorResponse
from ...models.mark_refund_paid_request import MarkRefundPaidRequest
from ...models.refund import Refund
from ...types import UNSET, Unset
from typing import cast


def _get_kwargs(
    id: str,
    *,
    body: MarkRefundPaidRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> dict[str, Any]:
    headers: dict[str, Any] = {}
    if not isinstance(idempotency_key, Unset):
        headers["Idempotency-Key"] = idempotency_key

    _kwargs: dict[str, Any] = {
        "method": "post",
        "url": "/v1/refunds/{id}/mark_paid".format(
            id=quote(str(id), safe=""),
        ),
    }

    _kwargs["json"] = body.to_dict()

    headers["Content-Type"] = "application/json"

    _kwargs["headers"] = headers
    return _kwargs


def _parse_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> ErrorResponse | Refund | None:
    if response.status_code == 200:
        response_200 = Refund.from_dict(response.json())

        return response_200

    if response.status_code == 400:
        response_400 = ErrorResponse.from_dict(response.json())

        return response_400

    if response.status_code == 401:
        response_401 = ErrorResponse.from_dict(response.json())

        return response_401

    if response.status_code == 403:
        response_403 = ErrorResponse.from_dict(response.json())

        return response_403

    if response.status_code == 404:
        response_404 = ErrorResponse.from_dict(response.json())

        return response_404

    if response.status_code == 409:
        response_409 = ErrorResponse.from_dict(response.json())

        return response_409

    if response.status_code == 422:
        response_422 = ErrorResponse.from_dict(response.json())

        return response_422

    if response.status_code == 429:
        response_429 = ErrorResponse.from_dict(response.json())

        return response_429

    if response.status_code == 503:
        response_503 = ErrorResponse.from_dict(response.json())

        return response_503

    if client.raise_on_unexpected_status:
        raise errors.UnexpectedStatus(response.status_code, response.content)
    else:
        return None


def _build_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> Response[ErrorResponse | Refund]:
    return Response(
        status_code=HTTPStatus(response.status_code),
        content=response.content,
        headers=response.headers,
        parsed=_parse_response(client=client, response=response),
    )


def sync_detailed(
    id: str,
    *,
    client: AuthenticatedClient,
    body: MarkRefundPaidRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> Response[ErrorResponse | Refund]:
    """Attaches the transaction that pays a pending refund, as BTCPay's payout `mark-paid`. At
    `finalized`, both providers must show a `Transfer` of the deposit's token from the refund's
    `treasury` to `destination_address` for exactly `amount_atomic`, in a log no other refund uses
    (`receipt_log_index`, or any such log when absent). Then the refund is `succeeded` and
    `deposit.refunded` is sent; otherwise it is `failed` with a `failure_reason`. Repeating the same
    transaction returns the refund. From here on the refund cannot be canceled: it is `failed`
    only when its transaction is proven not to pay it.
    Each environment has a configured attached-pending refund limit (production 2, staging 1),
    and permits one new attachment per
    rolling 24 hours across all accounts and modes. Repeating the same attachment consumes no
    quota. A limit refusal preserves the reservation; contact the operator before another payout.

    Args:
        id (str):
        idempotency_key (None | str | Unset):
        body (MarkRefundPaidRequest): `POST /v1/refunds/{id}/mark_paid` body: the merchant's
            refund transaction. Example: {'receipt_log_index': 0, 'transaction_hash':
            '0x4b6d8f0a2c4e6a8c0e2b4d6f8a0c2e4b6d8f0a2c4e6b8d0f2a4c6e8b0d2f4a6c'}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[ErrorResponse | Refund]
    """

    kwargs = _get_kwargs(
        id=id,
        body=body,
        idempotency_key=idempotency_key,
    )

    response = client.get_httpx_client().request(
        **kwargs,
    )

    return _build_response(client=client, response=response)


def sync(
    id: str,
    *,
    client: AuthenticatedClient,
    body: MarkRefundPaidRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> ErrorResponse | Refund | None:
    """Attaches the transaction that pays a pending refund, as BTCPay's payout `mark-paid`. At
    `finalized`, both providers must show a `Transfer` of the deposit's token from the refund's
    `treasury` to `destination_address` for exactly `amount_atomic`, in a log no other refund uses
    (`receipt_log_index`, or any such log when absent). Then the refund is `succeeded` and
    `deposit.refunded` is sent; otherwise it is `failed` with a `failure_reason`. Repeating the same
    transaction returns the refund. From here on the refund cannot be canceled: it is `failed`
    only when its transaction is proven not to pay it.
    Each environment has a configured attached-pending refund limit (production 2, staging 1),
    and permits one new attachment per
    rolling 24 hours across all accounts and modes. Repeating the same attachment consumes no
    quota. A limit refusal preserves the reservation; contact the operator before another payout.

    Args:
        id (str):
        idempotency_key (None | str | Unset):
        body (MarkRefundPaidRequest): `POST /v1/refunds/{id}/mark_paid` body: the merchant's
            refund transaction. Example: {'receipt_log_index': 0, 'transaction_hash':
            '0x4b6d8f0a2c4e6a8c0e2b4d6f8a0c2e4b6d8f0a2c4e6b8d0f2a4c6e8b0d2f4a6c'}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        ErrorResponse | Refund
    """

    return sync_detailed(
        id=id,
        client=client,
        body=body,
        idempotency_key=idempotency_key,
    ).parsed


async def asyncio_detailed(
    id: str,
    *,
    client: AuthenticatedClient,
    body: MarkRefundPaidRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> Response[ErrorResponse | Refund]:
    """Attaches the transaction that pays a pending refund, as BTCPay's payout `mark-paid`. At
    `finalized`, both providers must show a `Transfer` of the deposit's token from the refund's
    `treasury` to `destination_address` for exactly `amount_atomic`, in a log no other refund uses
    (`receipt_log_index`, or any such log when absent). Then the refund is `succeeded` and
    `deposit.refunded` is sent; otherwise it is `failed` with a `failure_reason`. Repeating the same
    transaction returns the refund. From here on the refund cannot be canceled: it is `failed`
    only when its transaction is proven not to pay it.
    Each environment has a configured attached-pending refund limit (production 2, staging 1),
    and permits one new attachment per
    rolling 24 hours across all accounts and modes. Repeating the same attachment consumes no
    quota. A limit refusal preserves the reservation; contact the operator before another payout.

    Args:
        id (str):
        idempotency_key (None | str | Unset):
        body (MarkRefundPaidRequest): `POST /v1/refunds/{id}/mark_paid` body: the merchant's
            refund transaction. Example: {'receipt_log_index': 0, 'transaction_hash':
            '0x4b6d8f0a2c4e6a8c0e2b4d6f8a0c2e4b6d8f0a2c4e6b8d0f2a4c6e8b0d2f4a6c'}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[ErrorResponse | Refund]
    """

    kwargs = _get_kwargs(
        id=id,
        body=body,
        idempotency_key=idempotency_key,
    )

    response = await client.get_async_httpx_client().request(**kwargs)

    return _build_response(client=client, response=response)


async def asyncio(
    id: str,
    *,
    client: AuthenticatedClient,
    body: MarkRefundPaidRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> ErrorResponse | Refund | None:
    """Attaches the transaction that pays a pending refund, as BTCPay's payout `mark-paid`. At
    `finalized`, both providers must show a `Transfer` of the deposit's token from the refund's
    `treasury` to `destination_address` for exactly `amount_atomic`, in a log no other refund uses
    (`receipt_log_index`, or any such log when absent). Then the refund is `succeeded` and
    `deposit.refunded` is sent; otherwise it is `failed` with a `failure_reason`. Repeating the same
    transaction returns the refund. From here on the refund cannot be canceled: it is `failed`
    only when its transaction is proven not to pay it.
    Each environment has a configured attached-pending refund limit (production 2, staging 1),
    and permits one new attachment per
    rolling 24 hours across all accounts and modes. Repeating the same attachment consumes no
    quota. A limit refusal preserves the reservation; contact the operator before another payout.

    Args:
        id (str):
        idempotency_key (None | str | Unset):
        body (MarkRefundPaidRequest): `POST /v1/refunds/{id}/mark_paid` body: the merchant's
            refund transaction. Example: {'receipt_log_index': 0, 'transaction_hash':
            '0x4b6d8f0a2c4e6a8c0e2b4d6f8a0c2e4b6d8f0a2c4e6b8d0f2a4c6e8b0d2f4a6c'}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        ErrorResponse | Refund
    """

    return (
        await asyncio_detailed(
            id=id,
            client=client,
            body=body,
            idempotency_key=idempotency_key,
        )
    ).parsed
