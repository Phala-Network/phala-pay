"""Pure Stripe parameters; no Stripe dependency, API calls, or currency conversion.

Parameter contracts: https://docs.stripe.com/api/invoices/create,
https://docs.stripe.com/api/invoiceitems/create and
https://docs.stripe.com/api/credit_notes/create (2026-10-28.endive).
Run receipt creation from a queue after fulfillment, never in the webhook response path.
"""

from __future__ import annotations

import re
from typing import Literal, TypedDict

from topup_client.models.deposit import Deposit
from topup_client.models.quote import Quote
from topup_client.types import Unset

from .chains import network_name, transaction_url


class StripeCustomField(TypedDict):
    name: str
    value: str


class StripeReceiptInvoiceParams(TypedDict):
    customer: str
    currency: str
    collection_method: Literal["send_invoice"]
    days_until_due: int
    auto_advance: Literal[False]
    description: str
    footer: str
    custom_fields: list[StripeCustomField]
    metadata: dict[str, str]


class StripeReceiptInvoiceItemParams(TypedDict):
    invoice: str
    customer: str
    amount: int
    currency: str
    tax_behavior: Literal["inclusive"]
    discountable: Literal[False]
    description: str
    metadata: dict[str, str]


class StripeReceiptCreditNoteParams(TypedDict):
    invoice: str
    amount: int
    out_of_band_amount: int
    metadata: dict[str, str]


def _amount(value: object, name: str) -> int:
    if type(value) is not int or value < 0:
        raise ValueError(f"{name} must be a nonnegative integer in minor units")
    return value


def _metadata(deposit: Deposit) -> dict[str, str]:
    if deposit.status not in ("credited", "reversed") or _amount(deposit.amount, "amount") == 0:
        raise ValueError("receipt requires a deposit with a positive credited amount")
    if not re.fullmatch(r"[a-z]{3}", deposit.currency):
        raise ValueError("currency must be a lowercase three-letter currency code")
    quote = deposit.quote
    metadata = {
        "phala_pay_deposit": deposit.id,
        "chain_id": str(deposit.chain_id),
        "asset": deposit.asset if isinstance(deposit.asset, str) else deposit.asset_contract,
        "tx_hash": deposit.tx_hash,
        "livemode": "true" if deposit.livemode else "false",
    }
    if isinstance(quote, Quote):
        metadata["phala_pay_quote"] = quote.id
    elif isinstance(quote, str):
        metadata["phala_pay_quote"] = quote
    elif quote is not None and not isinstance(quote, Unset):
        raise ValueError("quote must be an id, expanded quote, or null")
    # Stripe metadata values have a 500-character limit; never truncate identifiers.
    # https://docs.stripe.com/api/metadata
    if any(not value or len(value) > 500 for value in metadata.values()):
        raise ValueError("receipt metadata values must contain 1 to 500 characters")
    return metadata


def _description(deposit: Deposit) -> str:
    token = deposit.asset.upper() if isinstance(deposit.asset, str) else deposit.asset_contract
    return f"{token} on {network_name(deposit.chain_id)}, {deposit.tx_hash}"


def stripe_receipt_invoice(
    deposit: Deposit, *, customer: str, description: str | None = None
) -> StripeReceiptInvoiceParams:
    """Draft invoice params; finalize with auto_advance=False, then pay(paid_out_of_band=True).

    Always uses the deposit currency. The invoice must contain only this deposit's line;
    check its total and amount_remaining before marking it paid (integration guide §2.7).
    """
    metadata = _metadata(deposit)
    token = deposit.asset.upper() if isinstance(deposit.asset, str) else deposit.asset_contract
    # Stripe: max 4 custom fields; name <=40 and value <=140 characters.
    fields: list[StripeCustomField] = [
        {"name": "Network", "value": network_name(deposit.chain_id)[:140]},
        {"name": "Token", "value": token[:140]},
        {"name": "Transaction", "value": deposit.tx_hash[:140]},
    ]
    url = transaction_url(deposit.chain_id, deposit.tx_hash)
    if url is not None and len(url) <= 140:
        fields.append({"name": "Explorer", "value": url})
    return {
        "customer": customer,
        "currency": deposit.currency,
        "collection_method": "send_invoice",
        "days_until_due": 0,
        "auto_advance": False,
        "description": description if description is not None else _description(deposit),
        "footer": f"Paid in cryptocurrency. Transaction: {url or deposit.tx_hash}",
        "custom_fields": fields,
        "metadata": metadata,
    }


def stripe_receipt_invoice_item(
    deposit: Deposit, *, invoice: str, customer: str
) -> StripeReceiptInvoiceItemParams:
    """One tax-inclusive line in deposit currency, without a Stripe Product/Price dependency.

    InvoiceItem.create supports amount/currency/tax_behavior directly. Disabling discounts
    preserves the credited amount even when the customer has an inherited discount.
    """
    metadata = _metadata(deposit)
    return {
        "invoice": invoice,
        "customer": customer,
        "amount": _amount(deposit.amount, "amount"),
        "currency": deposit.currency,
        "tax_behavior": "inclusive",
        "discountable": False,
        "description": _description(deposit),
        "metadata": metadata,
    }


def stripe_receipt_credit_note(
    deposit: Deposit, *, invoice: str, previous_refunded: int, previous_reversed: int
) -> StripeReceiptCreditNoteParams | None:
    """Credit only new refund/reversal growth against an already paid receipt invoice.

    Previous totals must be those successfully recorded in Stripe for this deposit. Persist
    componentwise maxima after success so stale/out-of-order snapshots never lower them.
    """
    metadata = _metadata(deposit)
    refunded = _amount(deposit.amount_refunded, "amount_refunded")
    reversed_amount = _amount(deposit.amount_reversed, "amount_reversed")
    previous_refunded = _amount(previous_refunded, "previous_refunded")
    previous_reversed = _amount(previous_reversed, "previous_reversed")
    if max(refunded, previous_refunded) + max(reversed_amount, previous_reversed) > _amount(
        deposit.amount, "amount"
    ):
        raise ValueError("refund and reversal totals exceed the credited amount")
    refund_growth = max(0, refunded - previous_refunded)
    reversal_growth = max(0, reversed_amount - previous_reversed)
    growth = refund_growth + reversal_growth
    if growth == 0:
        return None
    metadata.update(
        amount_refunded=str(max(refunded, previous_refunded)),
        amount_reversed=str(max(reversed_amount, previous_reversed)),
        refund_growth=str(refund_growth),
        reversal_growth=str(reversal_growth),
    )
    return {
        "invoice": invoice,
        "amount": growth,
        "out_of_band_amount": growth,
        "metadata": metadata,
    }
