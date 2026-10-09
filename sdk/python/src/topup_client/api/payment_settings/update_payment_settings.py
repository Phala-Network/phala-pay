from http import HTTPStatus
from typing import Any, cast
from urllib.parse import quote

import httpx

from ...client import AuthenticatedClient, Client
from ...types import Response, UNSET
from ... import errors

from ...models.error_response import ErrorResponse
from ...models.payment_settings_object import PaymentSettingsObject
from ...models.update_payment_settings_request import UpdatePaymentSettingsRequest
from ...types import UNSET, Unset
from typing import cast


def _get_kwargs(
    *,
    body: UpdatePaymentSettingsRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> dict[str, Any]:
    headers: dict[str, Any] = {}
    if not isinstance(idempotency_key, Unset):
        headers["Idempotency-Key"] = idempotency_key

    _kwargs: dict[str, Any] = {
        "method": "post",
        "url": "/v1/payment_settings",
    }

    _kwargs["json"] = body.to_dict()

    headers["Content-Type"] = "application/json"

    _kwargs["headers"] = headers
    return _kwargs


def _parse_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> ErrorResponse | PaymentSettingsObject | None:
    if response.status_code == 200:
        response_200 = PaymentSettingsObject.from_dict(response.json())

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

    if response.status_code == 409:
        response_409 = ErrorResponse.from_dict(response.json())

        return response_409

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
) -> Response[ErrorResponse | PaymentSettingsObject]:
    return Response(
        status_code=HTTPStatus(response.status_code),
        content=response.content,
        headers=response.headers,
        parsed=_parse_response(client=client, response=response),
    )


def sync_detailed(
    *,
    client: AuthenticatedClient,
    body: UpdatePaymentSettingsRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> Response[ErrorResponse | PaymentSettingsObject]:
    """Updates your payment settings in the key's mode. A parameter not sent is unchanged; `chains`,
    when sent, replaces the whole list, and a term an element does not send takes the operator's
    default. Writes are last-write-wins. The settings govern quotes and deposit addresses issued
    from now on, and every payment recorded after the change; a quote keeps the terms it was
    issued with. After a restore of the service the settings are `held` until a `POST` with your
    complete configuration, even unchanged, reconfirms them: `chains` is then required, and a
    parameter not sent takes its default. Announced as `payment_settings.updated`.

    Args:
        idempotency_key (None | str | Unset):
        body (UpdatePaymentSettingsRequest): `POST /v1/payment_settings` body. A parameter not
            sent is unchanged; `chains`, when sent,
            replaces the whole list. Writes are last-write-wins. Example: {'chains': [{'assets':
            [{'asset': 'usdc', 'quote_spread_bps': 0}, {'asset': 'usdt'}], 'chain_id': 1,
            'confirmations': '12'}]}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[ErrorResponse | PaymentSettingsObject]
    """

    kwargs = _get_kwargs(
        body=body,
        idempotency_key=idempotency_key,
    )

    response = client.get_httpx_client().request(
        **kwargs,
    )

    return _build_response(client=client, response=response)


def sync(
    *,
    client: AuthenticatedClient,
    body: UpdatePaymentSettingsRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> ErrorResponse | PaymentSettingsObject | None:
    """Updates your payment settings in the key's mode. A parameter not sent is unchanged; `chains`,
    when sent, replaces the whole list, and a term an element does not send takes the operator's
    default. Writes are last-write-wins. The settings govern quotes and deposit addresses issued
    from now on, and every payment recorded after the change; a quote keeps the terms it was
    issued with. After a restore of the service the settings are `held` until a `POST` with your
    complete configuration, even unchanged, reconfirms them: `chains` is then required, and a
    parameter not sent takes its default. Announced as `payment_settings.updated`.

    Args:
        idempotency_key (None | str | Unset):
        body (UpdatePaymentSettingsRequest): `POST /v1/payment_settings` body. A parameter not
            sent is unchanged; `chains`, when sent,
            replaces the whole list. Writes are last-write-wins. Example: {'chains': [{'assets':
            [{'asset': 'usdc', 'quote_spread_bps': 0}, {'asset': 'usdt'}], 'chain_id': 1,
            'confirmations': '12'}]}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        ErrorResponse | PaymentSettingsObject
    """

    return sync_detailed(
        client=client,
        body=body,
        idempotency_key=idempotency_key,
    ).parsed


async def asyncio_detailed(
    *,
    client: AuthenticatedClient,
    body: UpdatePaymentSettingsRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> Response[ErrorResponse | PaymentSettingsObject]:
    """Updates your payment settings in the key's mode. A parameter not sent is unchanged; `chains`,
    when sent, replaces the whole list, and a term an element does not send takes the operator's
    default. Writes are last-write-wins. The settings govern quotes and deposit addresses issued
    from now on, and every payment recorded after the change; a quote keeps the terms it was
    issued with. After a restore of the service the settings are `held` until a `POST` with your
    complete configuration, even unchanged, reconfirms them: `chains` is then required, and a
    parameter not sent takes its default. Announced as `payment_settings.updated`.

    Args:
        idempotency_key (None | str | Unset):
        body (UpdatePaymentSettingsRequest): `POST /v1/payment_settings` body. A parameter not
            sent is unchanged; `chains`, when sent,
            replaces the whole list. Writes are last-write-wins. Example: {'chains': [{'assets':
            [{'asset': 'usdc', 'quote_spread_bps': 0}, {'asset': 'usdt'}], 'chain_id': 1,
            'confirmations': '12'}]}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[ErrorResponse | PaymentSettingsObject]
    """

    kwargs = _get_kwargs(
        body=body,
        idempotency_key=idempotency_key,
    )

    response = await client.get_async_httpx_client().request(**kwargs)

    return _build_response(client=client, response=response)


async def asyncio(
    *,
    client: AuthenticatedClient,
    body: UpdatePaymentSettingsRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> ErrorResponse | PaymentSettingsObject | None:
    """Updates your payment settings in the key's mode. A parameter not sent is unchanged; `chains`,
    when sent, replaces the whole list, and a term an element does not send takes the operator's
    default. Writes are last-write-wins. The settings govern quotes and deposit addresses issued
    from now on, and every payment recorded after the change; a quote keeps the terms it was
    issued with. After a restore of the service the settings are `held` until a `POST` with your
    complete configuration, even unchanged, reconfirms them: `chains` is then required, and a
    parameter not sent takes its default. Announced as `payment_settings.updated`.

    Args:
        idempotency_key (None | str | Unset):
        body (UpdatePaymentSettingsRequest): `POST /v1/payment_settings` body. A parameter not
            sent is unchanged; `chains`, when sent,
            replaces the whole list. Writes are last-write-wins. Example: {'chains': [{'assets':
            [{'asset': 'usdc', 'quote_spread_bps': 0}, {'asset': 'usdt'}], 'chain_id': 1,
            'confirmations': '12'}]}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        ErrorResponse | PaymentSettingsObject
    """

    return (
        await asyncio_detailed(
            client=client,
            body=body,
            idempotency_key=idempotency_key,
        )
    ).parsed
