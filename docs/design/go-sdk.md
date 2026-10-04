# Go SDK plan

Status: proposed; documentation only. Implement after the v0.8.2 release and staging
verification are complete. This plan does not add a supported SDK or change payment behavior.

## Purpose and priority

Provide a first-party backend integration for Go merchants. Python currently supplies the full
API client; JavaScript supplies browser checkout and server-side cryptographic helpers, not a
complete authenticated API client. Go can fill a distinct backend need without expanding several
language support matrices at once. Validate the first Go integration's needs before committing
to full API parity.

## Initial scope

Implement a small complete payment flow: retrieve account configuration, create and retrieve
quotes, create and retrieve deposit addresses, list and retrieve deposits, and verify webhooks.
Include a net/http example that hands the browser a verified payment address and demonstrates
transactional fulfillment. Explicitly document unsupported administrative APIs. Defer treasury
signing, sweep builders, account exports, frontend components, and additional languages.

Generate transport models from the committed merchant OpenAPI snapshot; do not duplicate the
schema manually. Keep generated code internal behind a small handwritten public client and
security layer. Generation alone is not a safe payment SDK.

## Public API and transport contract

- Accept context.Context on every network operation, including pagination. Cancellation must
  interrupt both requests and retry delays. Allow an injected http.Client and a documented finite
  default timeout; never mutate the caller's client or the global default transport.
- Configure API base and restricted API key explicitly. Keep credentials out of URLs, errors,
  and logs, and prevent credential forwarding on cross-origin redirects.
- Return structured API errors with HTTP status, stable code, type, parameter, request ID,
  documentation URL, and retry information. Preserve wrapped transport errors for errors.Is/As.
- Generate one idempotency key per logical POST unless supplied by the caller; preserve it and
  the request body across retries. Match existing Phala retry semantics, including Retry-After,
  bounded backoff, idempotency_key_in_use, and terminal Idempotent-Replayed responses. Do not
  retry every conflict or replay a saved failure indefinitely.
- Offer explicit pages first, with an optional lazy iterator that propagates context and errors.
  Preserve cursor semantics and detect a repeated cursor instead of looping forever.
- Preserve atomic token amounts as decimal strings, distinguish absent/null/zero where required,
  and accept future response enum values. Confirm integer ranges against the actual schema.

## Security parity is a release requirement

Quotes and active deposit addresses must be independently recomputed from merchant-configured
account, factory, implementation, and per-chain treasury pins before returning payable addresses.
Never derive trust from the response's treasury. Live mode must fail closed when pins are absent
or inconsistent. Reuse cross-language derivation vectors, including wrong account, mode, chain,
quote, treasury, and address cases. Select a minimal reviewed Keccak dependency rather than
pulling in a full blockchain client solely for address derivation.

Webhook verification must use the original body bytes, Ed25519 v1a signatures, fixed rotation
keys, header/body event ID equality, expected account and mode, and the existing bilateral
inclusive timestamp window. Preserve the default 300 seconds and zero exact-match semantics.
Reject invalid time configuration, unsafe timestamps, and malformed signed envelope shapes.
Use shared signed fixtures with Python and JavaScript, including boundary and rejection cases.
Do not copy Stripe's signing algorithm or tolerance-zero behavior.

Verification is not exactly-once fulfillment. The example must commit a unique deposit credit and
its ledger update in one transaction, acknowledge only after commit, and handle duplicate,
concurrent, reordered, refund, and reversal snapshots as specified in the integration guide.
Use existing fulfillment semantics; do not introduce a second accounting model.

## Generator and packaging decision

Evaluate pinned released versions of oapi-codegen and ogen against the actual OpenAPI 3.1
snapshot. Check nullable unions, metadata updates, additional properties, errors, large values,
and future enum values by compiling and round-tripping representative payloads. Upstream branch
support is not proof that a released generator supports this schema. Do not silently downgrade
the source schema or weaken validation to make generation work.

Prefer the smallest maintainable output and dependency set, reproducible from a checked-in
configuration. Record the selected generator version, license, regeneration command, Go support
floor, and dependency policy in the implementation PR.

A proposed sdk/go module needs a separate release design: Go subdirectory modules require
sdk/go/vX.Y.Z tags, whereas the service uses vX.Y.Z. Decide and test module discovery, version
alignment, immutable tagging, and release automation before publishing. Do not assume the
existing root release tag publishes the Go module. The plan creates no module or tags.

## Delivery and acceptance

1. Confirm one target Go integration and record generator/schema compatibility evidence. Agree
   on the public API and module release contract before implementation.
2. Implement transport and the scoped endpoints with httptest coverage for cancellation,
   timeouts, error decoding, retry budgets, stable idempotency, redirect boundaries, and pages.
3. Add address derivation and webhook verification with shared positive and negative vectors.
   Test the example's duplicate and concurrent fulfillment against a disposable database.
4. Run a disposable service/Anvil integration through quote, payment, signed webhook, and one
   ledger credit; exercise reversal and replay scenarios. No live keys or infrastructure needed.
5. Require go test, race detection, vet, dependency review, generated-code no-diff checks, and
   supported-Go-version CI. Document installation, restricted-key permissions, compatibility,
   unsupported APIs, and the sample before a reviewed release.

Do not bundle this work into v0.8.2 or block its staging recovery. Do not claim production
readiness from generated code or unit tests alone.

## Comparison and references

Stripe Go is a useful ergonomic reference for context-aware requests, structured errors, list
iteration, and idempotent network retries. Phala's API semantics and cryptographic trust model
remain authoritative. Reference snapshots inspected for this plan:

- [Stripe Go README](https://github.com/stripe/stripe-go/blob/6c048f910ef3d1184802a34019c2307c3d208746/README.md).
- [oapi-codegen README](https://github.com/oapi-codegen/oapi-codegen/blob/43281d18a9d0d4adc921a2e697147aaf63fd6479/README.md).
- [ogen README](https://github.com/ogen-go/ogen/blob/2569478736aed33cf1fe9d2a5b6e2d659bea0e69/README.md).
- [Existing Python client](../../sdk/python/README.md), [JavaScript helpers](../../sdk/js/README.md),
  [integration contract](../integration.md), and [architecture](../architecture.md).
