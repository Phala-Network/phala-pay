"""A token with no route is sent to a quote's address.

Expect: no merchant deposit or rejection webhook for the unsupported transfer. A supported
payment to the same address still credits normally and proves intake is progressing.
"""

from __future__ import annotations

from harness import TOKEN_UNIT, Context, ScenarioSkipped, check


def run(ctx: Context) -> None:
    if ctx.config.unsupported_token is None:
        raise ScenarioSkipped("no unsupported_token configured")
    team = ctx.team("asset")
    _, quote = ctx.lock(team, amount_minor=2500)
    ignored_tx = ctx.pay(quote.address, 1000 * TOKEN_UNIT, token=ctx.config.unsupported_token)
    supported_tx = ctx.pay(quote.address, int(quote.amount_atomic))
    deposit, _ = ctx.credited(team, quote.address, quote, tx_hash=supported_tx)
    check(
        all(
            item.tx_hash != ignored_tx
            for item in ctx.client.list_deposits(client_reference_id=team)
        ),
        "unsupported transfer created a merchant deposit",
    )
    check(
        all(event["type"] != "deposit.rejected" for event in ctx.ledger.events_for({team})),
        "unsupported transfer produced a rejection webhook",
    )
    check(ctx.ledger.find_order(deposit.id) is not None, "supported payment has no product order")
