# Design: SDK integration ergonomics

Status: Accepted (owner, 2026-10-04); implementation pending

## 1. Purpose and precedents

Configure once, create a verified quote, show checkout, apply a verified webhook. Preserve
[integration](../integration.md), [OpenAPI](../../crates/topup/openapi.json) and [Go
parity](go-sdk.md). [Stripe Node](https://github.com/stripe/stripe-node#readme)/
[Python](https://github.com/stripe/stripe-python#readme) inform resources/types/retries/errors;
[Elements](https://docs.stripe.com/payments/accept-a-payment) the backend/browser split.
[Commerce](https://github.com/coinbase/coinbase-commerce-node#readme) informs key/base/timeout
setup; [BTCPay](https://github.com/btcpayserver/node-btcpay#readme)/
[Greenfield](https://docs.btcpayserver.org/Development/GreenFieldExample/) scoped credentials after
setup. Avoid global clients, shared-secret signatures, legacy pairing and secret-printing examples.

## 2. Server clients

JS: `new PhalaPay({apiKey, pins, apiBase?, timeoutMs?, maxAttempts?, requestDeadlineMs?, fetch?})`
from `@phala/pay/server`. Python: `PhalaPay(api_key, *, pins, api_base=None, timeout=15.0,
max_attempts=4, request_deadline=60.0, transport=None)`. Pins accept string or frozen Pins value. JS
defaults are 15000 ms / four attempts / 60000 ms. No global mutable client or trust discovery.

Resources mirror Python: quotes, depositAddresses, deposits, refunds, paymentSettings, config,
account, treasuries, apiKeys, webhookEndpoints, events, balance, sweeps, forwarders; Python uses
snake_case. Wire fields stay snake_case. [Normative reference](sdk-ergonomics-reference.md)
specifies all methods, signatures, pages, error fields, retries and boundary validation; both agents
must use it.

Generate OpenAPI types with openapi-typescript==7.13.0 / openapi-python-client==0.29.1. `@phala/pay`
stays keyless; `/server` rejects browser/non-Node use before reading keys. Non-browser helpers move
to `/server/helpers`; Python stays synchronous with context cleanup.

## 3. Two-variable configuration

Only **PHALA_PAY_API_KEY** and **PHALA_PAY_PINS**. fromEnv(env=process.env, {apiBase}={}) /
from_env(env=None, *, api_base=None) read these two values; no dotenv/file/network loading, no
legacy trust-env merging. Base comes from pins; explicit test/proxy override must match its
normalized origin. Alternate origins require separately provisioned pins, never an override that
bypasses trust.

`ppay_pins_v1.<base64url>` encodes UTF-8 JSON with these required fields:

```json
{
  "api_base": "https://api.phala-pay.example",
  "account": "acct_...", "livemode": false,
  "factory": "0x...", "implementation": "0x...",
  "treasuries": {"11155111": "0x..."},
  "webhook_keys": [{"version": 1, "public_key": "whpk_..."}]
}
```

parsePins/parse_pins return frozen Pins; encodePins/encode_pins return string. Shared encoding and
rejection fixtures required. Pins carry public webhook keys for atomic trust deployment.

Readonly pay.pins/pay.livemode; missing/wrong pins fail closed. Verify response identity and derive
open quotes/active networks from pinned contracts/account/chain treasury, never response treasury.
Historical objects are not payable; active treasury changes need new pins. Preserve legacy test
warnings without live fallback; [migration
rules](sdk-ergonomics-reference.md#address-pins-and-migration).

Bound constructEvent / construct_event uses client pins/mode and original body bytes. Verify Ed25519
v1a, account/mode, ID and envelope before returning typed Event/event.deposit; no overrides or
automatic key discovery. [Full contract](sdk-ergonomics-reference.md#bound-webhooks) preserves the
inclusive 300-second window, zero exact match, explicit functions/errors and manual overlap
rotation.

## 4. One setup command

`npx @phala/pay@X.Y.Z setup` requires **Node >=20.3 and Docker** for the digest-pinned dstack
verifier. A Python-only shop runs it on a trusted admin workstation, transfers two env vars through
its secret manager, and keeps the admin key offline. Python servers need neither Node nor Docker.
One maintained CLI avoids duplicated sensitive onboarding; defer a Python alias. Missing tools stop
with instructions.

Prompt for account/origin, deployment identity/contracts, treasury/assets and webhook URL; [all
flags](sdk-ergonomics-reference.md#setup-flags-and-permissions) are optional interactive inputs.

1. Independently verify deployment identity/contracts; roll first key with 3600-second overlap,
   persist replacement before revoking old key.
2. Fresh nonce + dstack verifier + account/mode/key binding, expected TCB/app/compose/report data;
   binding alone is insufficient. Missing verification stops both modes.
3. Exact SIWE: EOA personal_sign or deployed Safe EIP-1271, never wallet keys. Show threshold/expiry
   tasks (10 minutes EOA, 24 hours Safe), 48-hour live changes; readiness requires active treasury.
4. Reconcile settings and four deposit event subscriptions; security notices automatic. Reuse exact
   endpoint; conflicting settings/endpoints need human choice. Never roll webhook keys on rerun.
5. Minimal quotes.write runtime key, opt-in grants per reference. Persist secret immediately,
   atomically write two env values (0600); admin key stays separate.

Journal before mutation; secret-free output, private files, idempotent reruns and --resume follow
the [recovery contract](sdk-ergonomics-reference.md#setup-persistence-and-recovery). Exit 0 ready, 2
human task, 1 failure. Endpoint test proves signature, not payment; simulator setup is separate.

## 5. Checkout handoff

Symmetric methods: `pay.checkoutParams(quote): CheckoutParams` / `pay.checkout_params(quote) ->
dict[str, str]`. Both return `{clientSecret, expectedAddress, apiBase}`; no quote wrapper method.
Require originating-client Quote, recheck pins/open status and create/replay client_secret,
otherwise ResponseValidationError. Retrieval cannot invent a secret. Return checkout only to the
order's authenticated browser, never API key/pins. Existing component props keep working.

Shared browser entry (HTML contains #checkout; backend is same-origin):

```tsx
import { createRoot } from "react-dom/client";
import { Checkout } from "@phala/pay/react";
import "@phala/pay/styles.css";

const container = document.getElementById("checkout");
if (!container) throw new Error("Missing checkout container");

try {
  const response = await fetch("/topups", { method: "POST" });
  if (!response.ok) throw new Error("Payment unavailable");
  const { checkout } = await response.json();
  createRoot(container).render(<Checkout {...checkout} />);
} catch {
  container.textContent = "Payment unavailable. Please retry.";
}
```

Compare expectedAddress on fetch/wallet/QR/manual paths; browser success is display, never
fulfillment. Document createCheckout({...checkout}) with
subscribe/render/wallet-error/cancel/teardown examples for vanilla/Vue/Svelte. Defer custom element:
shadow styling/accessibility/SSR/secret attributes/events need a separate demand-backed UI contract.

## 6. Ledger: pure helper and copied recipe

depositNetAmount/deposit_net_amount returns integer minor units; balanceDelta/balance_delta takes
(previous snapshot, deposit) and returns `{snapshot, contribution, delta}`. Exact, pure arithmetic;
[merge/validation rules](sdk-ergonomics-reference.md#pure-helper-validation) never credit unknown
statuses.

Choose [copied recipes](sdk-ergonomics-reference.md#copied-transaction-recipes), not SDK SQL APIs:
merchants own schema/authorization/transactions. Recipes commit snapshot + balance delta before
acknowledgment; errors roll back/return 5xx. Production adapters run the same convergence fixtures.

## 7. Quickstarts and line budget

Count formatter-normalized physical lines, **including blanks/imports/errors/lifecycle/frontend**.
Core target **N=65**; recipes are visible additional cost. Baseline/count methodology is in the
[reference](sdk-ergonomics-reference.md#recipe-verification-and-line-counts). Commands,
authentication and production database adaptation are separate in both columns; no invented Node
baseline.

| Integration | Today | Proposed |
|---|---|---|
| Node code | No API client; hand-written HTTP (LOC unavailable). | 64 core + 49 copied recipe = 113 lines. |
| Python code | 216 lines, ledger included. | 60 core + 55 copied recipe = 115 lines. |
| Configuration | Six Python env values; Node repeats pins/mode and HTTP setup. | Two env values. |
| Setup | Eight manual API operations plus verifier/signature. | One command plus human signature; Node/Docker prerequisite. |

Install exact-version @phala/pay + viem/React/Express/better-sqlite3, or phala-pay +
FastAPI/uvicorn; run §4, load generated env via process manager, copy recipe beside merchant file.
These local test apps fix team/amount/order/asset; authenticate/rate-limit and persist order ids in
production. Run node merchant.mjs or uvicorn merchant:app --port 3000 behind the reachable test
webhook URL. Pay displayed tokens with funded test wallet; query recipe balance to confirm verified
2500 credit.

Node merchant.mjs (raw webhook route must precede JSON middleware):

```javascript
import express from "express";
import { PhalaPay, SignatureVerificationError } from "@phala/pay/server";
import { createLedger } from "./ledger.mjs";

const pay = PhalaPay.fromEnv();
const ledger = createLedger(pay);
const app = express();

app.post("/topups", async (_request, response) => {
  try {
    const quote = await pay.quotes.create(
      { client_reference_id: "team-42", amount: 2500, chain_id: 11155111, asset: "pha" },
      { idempotencyKey: "test-order-1" },
    );
    response.json({ checkout: pay.checkoutParams(quote) });
  } catch {
    response.status(503).json({ code: "payment_unavailable" });
  }
});

app.post(
  "/webhooks/phala-pay",
  express.raw({ type: "application/json", limit: "1mb" }),
  async (request, response) => {
    try {
      const event = await pay.webhooks.constructEvent(request.body, request.headers);
      ledger.apply(event);
      response.sendStatus(200);
    } catch (error) {
      response.sendStatus(error instanceof SignatureVerificationError ? 400 : 500);
    }
  },
);

const server = app.listen(3000);
server.on("error", () => {
  ledger.close();
  process.exitCode = 1;
});
process.on("SIGTERM", () => {
  server.close(() => {
    pay
      .close()
      .catch(() => {
        process.exitCode = 1;
      })
      .finally(() => ledger.close());
  });
});
```

Python merchant.py (database work runs off the event loop; uncaught application failures return
500):

```python
from contextlib import asynccontextmanager

from fastapi import FastAPI, HTTPException, Request, Response
from ledger import create_ledger
from phala_pay import PhalaPay, SignatureVerificationError
from starlette.concurrency import run_in_threadpool

pay = PhalaPay.from_env()
ledger = create_ledger(pay)


@asynccontextmanager
async def lifespan(app):
    try:
        yield
    finally:
        pay.close()


app = FastAPI(lifespan=lifespan)


@app.post("/topups")
def topup():
    try:
        quote = pay.quotes.create(
            client_reference_id="team-42",
            amount=2500,
            chain_id=11155111,
            asset="pha",
            idempotency_key="test-order-1",
        )
        return {"checkout": pay.checkout_params(quote)}
    except Exception:
        raise HTTPException(503, detail={"code": "payment_unavailable"}) from None


@app.post("/webhooks/phala-pay")
async def webhook(request: Request):
    try:
        event = pay.webhooks.construct_event(await request.body(), request.headers)
        await run_in_threadpool(ledger.apply, event)
    except SignatureVerificationError:
        return Response(status_code=400)
    return Response(status_code=200)
```

Next.js Node route: request.arrayBuffer() → constructEvent → recipe apply; signature 400, ledger
failure 500, committed 200. No edge merchant client.

## 8. Compatibility: Decided (owner, 2026-10-04)

Choose additive-only stable /v1 from 1.0: fewer coordinated merchant upgrades without dated
serializers.

- Every 1.x SDK works against every later 1.x service; no exact-version pairing from 1.0.
- /v1 allows only new endpoints, optional params, response fields and event types. Clients ignore
  unknown fields/event types; this does not authorize unknown deposit statuses to credit.
- Announce deprecation in CHANGELOG/docs; retain behaviour at least 12 months. Removals and semantic
  changes ship only in /v2, after that window; never remove behaviour from /v1.
- Until 1.0, 0.x retains exact SDK/service pairing; integration §5.9 is unchanged now. Pins format
  version stays independent of API/service version.

## 9. Phased delivery and gates

1. Contract/vectors PR: common pins/address/webhook/ledger fixtures and generator config; no Go
   code.
2. Parallel JS/Python PRs: resources/transport, pins/env/webhooks, symmetric handoff/helpers.
   Parity/ regeneration gates both releases. HTTP wrapping, retry budget and helper relocation need
   one 0.x minor with migration notes; legacy constructors/props/functions remain until announced
   minor.
3. Setup PR: JS bin/journal/env tests, interrupted keys, duplicates, expiry/Safe, attestation
   failure, secret-free output. Then examples/recipes/docs PR with rollback tests; coordinate
   #318/density.
4. **1.0 gate:** freeze initial baseline; thereafter run every retained previous 1.x SDK test suite
   against new service. CI OpenAPI diff rejects non-additive /v1 changes; semantic/webhook contracts
   catch non-schema breaks. Both SDKs explicitly test ignored unknown fields and event types.

Acceptance: disposable service/Anvil reaches paid quote + exactly one verified credit through each
formatted core/recipe. Exercise trust substitution, missing live pins, unknown keys/bad envelopes,
deadline bounds, browser exclusion and concurrent/reordered ledger convergence. Run SDK/doc CI; this
record alone claims no implemented APIs or executed paid-flow acceptance.
