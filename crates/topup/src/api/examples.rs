//! Examples of the API's objects and request bodies for the OpenAPI documents: one account's test
//! of a $25.00 PHA payment on Ethereum, from its quote to its refund. The tests of
//! [`super::openapi`] check each against its schema.

use serde_json::{Value, json};

const ACCOUNT: &str = "acct_0c6e1d0a9b3f4c2e8d7a6b5c4d3e2f10";
const QUOTE: &str = "qt_5f1c0b6a2d9e4f3a8b7c6d5e4f3a2b10";
const DEPOSIT: &str = "dep_8a1f4e2b6c3d49e0a7b5c1d2e3f40516";
const DEPOSIT_ADDRESS: &str = "da_7b2e9c4a1f6d48b3a5c0e2d4f6a8b1c3";
const REFUND: &str = "re_3c9e7a1b5d2f4a6c8e0b1d3f5a7c9e02";
const TREASURY: &str = "trs_4d8a2c6e0b1f47a3c5e7d9b1a3c5e7f9";
const ENDPOINT: &str = "we_9e1c3a5b7d2f40c6e8a0b2d4f6a8c0e1";
const EVENT: &str = "evt_2b4d6f8a0c1e43b5d7f9a1c3e5b7d9f0";
const API_KEY: &str = "key_6a8c0e2b4d1f43a5c7e9b1d3f5a7c9e1";
#[cfg(test)]
const REQUEST: &str = "req_4f2a9c1e7b3d45a6c8e0b2d4f6a8c1e3";
const SWEEP: &str = "sw_1e3c5a7b9d0f42e4c6a8b0d2f4e6a8c0";
const FORWARDER: &str = "fwd_5c7e9a1b3d2f44c6e8a0b2d4f6c8e0a2";

const PHA: &str = "0x6c5ba91642f10282b576d91922ae6448c9d52f4e";
const FACTORY: &str = "0x9e5f1d3c7a2b4e6f8a0c1d3e5f7a9b2c4d6e8f01";
const TREASURY_ADDRESS: &str = "0x936c1991f8da9a919fa11b557a3514719f5a4504";
const QUOTE_ADDRESS: &str = "0x2f3e91325b2288bce392711f85f5359661062a91";
const CUSTOMER_ADDRESS: &str = "0x0f45147a02e4c9d91aff20024e22095536fd5053";
const PAYER: &str = "0x1775c1326aa633546b0b5634ae2bef0ba7cbfc9a";
const TX: &str = "0x7d3c1e5a9b2f4d6c8e0a1b3d5f7c9e2a4b6d8f0c1e3a5b7d9f1c3e5a7b9d1f3e";
const BLOCK_HASH: &str = "0x9a1c3e5b7d0f2a4c6e8b0d2f4a6c8e0b2d4f6a8c0e2b4d6f8a0c2e4b6d8f0a2c";
const REFUND_TX: &str = "0x4b6d8f0a2c4e6a8c0e2b4d6f8a0c2e4b6d8f0a2c4e6b8d0f2a4c6e8b0d2f4a6c";
const SALT: &str = "0x4e9767dd0c2ab5b953a305c3f10dc1e0d1f7c9d3cbab8463509d2edb06ca4b52";

/// When the example payment was made, Unix seconds (2026-09-28).
const CREATED: i64 = 1_790_553_600;

/// A runtime server's restricted key: quotes, deposit addresses, deposits, events, and refund
/// reads, the set the integration guide recommends; each `write` includes its `read`.
const RUNTIME_PERMISSIONS: [&str; 8] = [
    "account.read",
    "deposit_addresses.read",
    "deposit_addresses.write",
    "deposits.read",
    "events.read",
    "quotes.read",
    "quotes.write",
    "refunds.read",
];

/// The example of the component schema `name`, if it has one.
pub(super) fn schema(name: &str) -> Option<Value> {
    Some(match name {
        "SubmitQuoteTransactionRequest" => json!({"transaction_hash": TX}),
        "SubmitDepositAddressTransactionRequest" => {
            json!({"transaction_hash": TX, "chain_id":84532})
        }
        "TransactionSubmission" => {
            json!({"object":"transaction_submission", "transaction_hash": TX, "status":"received"})
        }
        "Quote" => quote(),
        "QuoteView" => quote(),
        "ClientQuote" => client_quote(),
        "QuoteList" => list("/v1/quotes", quote()),
        "InstancePauseRequest" => {
            json!({"owner":"37215125136-1", "reason":"planned upgrade", "duration_seconds":900})
        }
        "InstancePauseResponse" => {
            json!({"owner":"37215125136-1", "paused_scopes":["mutations"], "expires_at":1790000900})
        }
        "CreateQuoteRequest" => json!({
            "client_reference_id": "team-42",
            "amount": 2500,
            "currency": "usd",
            "chain_id": 1,
            "asset": "usdc",
            "metadata": {"order_id": "ord_1001"},
        }),
        "Deposit" => deposit(),
        "DepositList" => list("/v1/deposits", deposit()),
        "DepositAddress" => deposit_address(),
        "DepositAddressView" => deposit_address(),
        "DepositAddressList" => list("/v1/deposit_addresses", deposit_address()),
        "CreateDepositAddressRequest" => json!({
            "client_reference_id": "team-42",
            "metadata": {"plan": "pro"},
        }),
        "Refund" => refund(),
        "RefundList" => list("/v1/refunds", refund()),
        "CreateRefundRequest" => json!({
            "deposit": DEPOSIT,
            "amount_atomic": "202510000000000000000",
            "destination_address": PAYER,
            "metadata": {"ticket": "support-311"},
        }),
        "MarkRefundPaidRequest" => json!({"transaction_hash": REFUND_TX, "receipt_log_index": 0}),
        "UpdateMetadataRequest" => json!({"metadata": {"order_id": "ord_1001", "note": ""}}),
        "Balance" => json!({
            "object": "balance",
            "livemode": false,
            "unswept": [{
                "chain_id": 1,
                "token": PHA,
                "asset": "PHA",
                "amount_atomic": "202510000000000000000",
                "final_amount_atomic": "202510000000000000000",
            }],
        }),
        "SweepList" => list(
            "/v1/sweeps",
            json!({
                "id": SWEEP,
                "object": "sweep",
                "livemode": false,
                "chain_id": 1,
                "forwarder": FORWARDER,
                "address": QUOTE_ADDRESS,
                "treasury": TREASURY_ADDRESS,
                "token": PHA,
                "asset": "PHA",
                "amount_atomic": "202510000000000000000",
                "tx_hash": REFUND_TX,
                "block_number": 21_000_420,
                "log_index": 7,
                "created": CREATED + 7_200,
            }),
        ),
        "ForwarderList" => list(
            "/v1/forwarders",
            json!({
                "id": FORWARDER,
                "object": "forwarder",
                "livemode": false,
                "chain_id": 1,
                "address": QUOTE_ADDRESS,
                "factory": FACTORY,
                "salt": SALT,
                "treasury": TREASURY_ADDRESS,
                "quote": QUOTE,
                "deposit_address": null,
                "superseded_at": null,
            }),
        ),
        "Treasury" => treasury(),
        "TreasuryList" => list("/v1/treasuries", treasury()),
        "TreasuryChallenge" => json!({
            "object": "treasury_challenge",
            "livemode": false,
            "chain_id": 1,
            "address": TREASURY_ADDRESS,
            "nonce": "Kq3nV8xZt2mP6wRa",
            "message": treasury_message(),
            "expires_at": CREATED + 600,
        }),
        "CreateTreasuryChallengeRequest" => json!({"chain_id": 1, "address": TREASURY_ADDRESS}),
        "CreateTreasuryRequest" => json!({
            "chain_id": 1,
            "message": treasury_message(),
            "signature": format!("0x{}1b", "5e".repeat(64)),
        }),
        "EventObjectResponse" => event(),
        "EventList" => list("/v1/events", event()),
        "ResendEventRequest" => json!({"webhook_endpoint": ENDPOINT}),
        "WebhookEndpointObject" => webhook_endpoint(),
        "WebhookEndpointList" => list("/v1/webhook_endpoints", webhook_endpoint()),
        "CreateWebhookEndpointRequest" => json!({
            "url": "https://example.com/phala-pay/webhooks",
            "enabled_events": ["deposit.credited", "deposit.reversed", "refund.failed"],
            "description": "Order fulfillment",
            "metadata": {"team": "payments"},
        }),
        "UpdateWebhookEndpointRequest" => json!({
            "enabled_events": ["*"],
            "disabled": false,
        }),
        "DeletedWebhookEndpoint" => json!({
            "id": ENDPOINT,
            "object": "webhook_endpoint",
            "deleted": true,
        }),
        "ApiKeyObject" => api_key(),
        "ApiKeyList" => list("/v1/api_keys", api_key()),
        "CreateApiKeyRequest" => json!({
            "name": "fulfillment worker",
            "type": "restricted",
            "permissions": ["quotes.write", "deposit_addresses.write", "deposits.read",
                            "events.read", "refunds.read"],
        }),
        "RollApiKeyRequest" => json!({"expires_in": 86_400}),
        "AccountObject" => json!({
            "id": ACCOUNT,
            "object": "account",
            "livemode": false,
            "name": "Example Cloud",
            "charges_enabled": true,
            "paused_scopes": [],
            "webhook_keys": [{"version": 2, "expires_at": null}, {"version": 1, "expires_at": CREATED + 86_400}],
            "created": CREATED - 2_592_000,
        }),
        "PaymentSettingsObject" => payment_settings(),
        "UpdatePaymentSettingsRequest" => json!({
            "chains": [{
                "chain_id": 1,
                "confirmations": "12",
                "assets": [{"asset": "PHA", "quote_spread_bps": 100}, {"asset": "USDT"}],
            }],
        }),
        "AccountSelfPauseRequest" => json!({"scopes": ["quotes"]}),
        "RollWebhookKeyRequest" => json!({"expires_in": 172_800}),
        "AttestationResponse" => json!({
            "object": "attestation",
            "account": ACCOUNT,
            "livemode": false,
            "webhook_keys": [{
                "version": 1,
                "public_key": "whpk_O2onvM62pC1io6jQKm8Nc2UyFXcd4kOmOsBIoYtZ2ik=",
                "expires_at": null,
            }],
            "report_data": "9f2c4e6a8b0d1f3a5c7e9b1d3f5a7c9e2b4d6f8a0c1e3a5b7d9f1c3e5a7b9d1f",
            "tdx_quote": "040002008100000000000000939a7233f79c4ca9940a0db3957f0607",
        }),
        "Config" => json!({
            "object": "config",
            "livemode": false,
            "currency": "usd",
            "max_open_quotes": 100,
            "max_open_amount_per_account": 1_000_000,
            "max_open_amount_per_customer": 500_000,
            "quote_creations_per_customer_per_minute": 10,
            "assets": [{
                "chain_id": 1,
                "asset": "PHA",
                "contract": PHA,
                "decimals": 18,
                "pricing": "spot",
                "confirmations": "12",
                "typical_credit_seconds": 150,
                "typical_finality_seconds": 900,
                "min_amount": 100,
                "min_deposit_atomic": "0",
                "max_deposit_atomic": "1000000000000000000000000",
                "min_refund_atomic": "1000000000000000000",
                "quote_ttl_seconds": 900,
                "quote_spread_bps": 100,
                "quote_tolerance_bps": 100,
                "quote_amount_decimals": 4,
            }],
        }),
        "ErrorResponse" => json!({
            "error": {
                "type": "invalid_request_error",
                "code": "deposit_not_final",
                "message": "the deposit is not final yet; request the refund once it is (about 15 minutes after its block on Ethereum)",
                "doc_url": format!("{}deposit_not_final", super::error::DOCS_URL),
            }
        }),
        // The admin API.
        "CreateAccountRequest" => json!({
            "name": "Example Cloud",
            "contact": {"name": "Ada Lovelace", "email": "payments@example.com"},
            "due_diligence": {
                "reference": "DD-2026-017",
                "reviewed_by": "operator@phala.network",
                "reviewed_at": "2026-09-20",
            },
            "charges_enabled": false,
            "reason": "onboarding after due diligence DD-2026-017",
        }),
        "UpdateAccountRequest" => json!({
            "charges_enabled": true,
            "reason": "live mode approved after legal sign-off",
        }),
        "AccountResponse" => json!({
            "id": ACCOUNT,
            "object": "account",
            "name": "Example Cloud",
            "contact": {"name": "Ada Lovelace", "email": "payments@example.com"},
            "due_diligence": {
                "reference": "DD-2026-017",
                "reviewed_by": "operator@phala.network",
                "reviewed_at": "2026-09-20",
            },
            "charges_enabled": false,
            "restricted": false,
            "paused_scopes": [],
            "max_unfinalized_credit": 100_000,
            "limits": {
                "live": {
                    "max_open_quotes": 1_000,
                    "max_open_amount_per_account": 5_000_000,
                    "max_open_amount_per_customer": 500_000,
                    "max_active_deposit_addresses": 100_000,
                },
                "test": {
                    "max_open_quotes": 100,
                    "max_open_amount_per_account": 1_000_000,
                    "max_open_amount_per_customer": 500_000,
                    "max_active_deposit_addresses": 1_000,
                },
            },
            "payment_settings": {
                "live": unconfigured_settings(true),
                "test": payment_settings(),
            },
            "created": CREATED - 2_592_000,
            "api_keys": [first_key()],
        }),
        "IssueApiKeyRequest" => json!({
            "livemode": true,
            "name": "recovery",
            "revoke_existing": true,
            "reason": "contact verified by phone after a lost key, ticket OPS-88",
        }),
        "AccountPauseRequest" => json!({
            "scopes": ["quotes", "settlement"],
            "reason": "treasury on a sanctions list; review OPS-91",
        }),
        "CustomerPauseRequest" => json!({"livemode": true, "scopes": ["settlement"]}),
        "PauseRequest" => json!({"scopes": ["quotes"]}),
        "PauseResponse" => json!({"paused_scopes": ["quotes", "settlement"]}),
        "RoutePauseResponse" => json!({
            "route": "phala-cloud-ethereum-pha-usd",
            "paused_scopes": ["quotes"],
        }),
        "AdminReasonRequest" => json!({"reason": "providers agree again; verified OPS-93"}),
        "AdminTreasuryPauseRequest" => json!({
            "reason": "former treasury key reported compromised by the contact; OPS-97",
        }),
        "NudgeResponse" => json!({
            "deposit_id": DEPOSIT,
            "next_attempt_at": "2026-09-28T12:00:00Z",
        }),
        "ReconciliationBlockLiftResponse" => json!({
            "block_key": "chain:1",
            "lifted_at": "2026-09-28T12:00:00Z",
        }),
        "ManualEntryRequest" => {
            json!({"address":PAYER,"reason":"reviewed designation","source_ref":"UK entity reference"})
        }
        "ManualEntryResponse" => json!({"address":PAYER,"active":true}),
        "ManualEntry" => manual_entry(),
        "ManualEntryList" => json!({"entries": [manual_entry()]}),
        "DailyReportResponse" => json!({
            "sanctions_manual_entries": 0,
            "sanctions_snapshot": null,
            "generated_at": "2026-09-28T12:00:00Z",
            "exposure_minor": "12500",
            "routes": [{
                "route": "phala-cloud-ethereum-pha-usd",
                "chain_id": 1,
                "asset_contract": PHA,
                "deposits_by_state": {"credited": 41, "swept": 1204, "rejected": 2},
                "refunds_by_status": {"succeeded": 3},
                "age_in_state_max_seconds": {"credited": 3_600},
                "credited_undelivered": 0,
                "credited_undelivered_max_age_seconds": 0,
                "open_rate_lock_exposure_atomic": "101255000000000000000",
                "rejected_holds_atomic": "0",
                "unflushed_balance_atomic": "8303010000000000000000",
            }],
            "reconciliation": null,
            "reconciliation_blocks": [],
            "failing_for_hours": 24,
            "failing_webhook_endpoints": [{
                "id": ENDPOINT,
                "account": ACCOUNT,
                "livemode": true,
                "url": "https://example.com/phala-pay/webhooks",
                "pending_deliveries": 37,
                "oldest_pending_at": "2026-09-26T08:14:00Z",
                "last_attempt_at": "2026-09-28T11:41:07Z",
                "last_attempt_status": 503,
            }],
        }),
        // The restore reconciliation (deploy/runbooks/restore.md).
        "RestoreStatus" => json!({
            "object": "restore_status",
            "frozen": true,
            "restore": restore(),
            "rescan": [{
                "chain_id": 1,
                "restored_block": 23_401_200,
                "scanned_block": 23_401_917,
                "scanned_block_time": CREATED + 3_600,
                "pending_backfills": 0,
                "blocked": false,
                "complete": true,
            }],
            "delivered_events": {
                "imported": 1,
                "findings": [{
                    "event": EVENT,
                    "type": "deposit.credited",
                    "deposit": DEPOSIT,
                    "status": "pending",
                    "delivered_amount_atomic": "202510000000000000000",
                    "delivered_amount": "2500",
                    "ledger_amount_atomic": null,
                    "ledger_amount": null,
                }],
            },
        }),
        "RestoreObject" => restore(),
        "RestoreApiKeyRevokeRequest" => json!({
            "account": ACCOUNT,
            "prefix": "ppay_sk_live_",
            "last4": "9Yz2",
            "reason": "revoked by the merchant at 10:02 PDT, after the restore point; INC-12",
        }),
        "RestoreTreasuryVerifyRequest" => json!({
            "account": ACCOUNT,
            "livemode": true,
            "treasuries": [{
                "id": TREASURY,
                "status": "canceled",
                "chain_id": 1,
                "address": TREASURY_ADDRESS,
                "crediting_paused_by": [],
            }],
            "reapply": true,
            "reason": "the merchant's treasury events since the restore point; INC-12",
        }),
        "RestoreTreasuryVerifyResponse" => json!({
            "object": "list",
            "data": [{
                "id": TREASURY,
                "received_status": "canceled",
                "status": "canceled",
                "result": "canceled",
                "crediting": "matches",
            }],
        }),
        "RestoreTreasuryApplyRequest" => {
            let body = json!({
                "id": EVENT,
                "object": "event",
                "account": ACCOUNT,
                "livemode": false,
                "type": "treasury.updated",
                "created": CREATED + 1_760,
                "actor": "system",
                "request": null,
                "data": {"object": treasury(), "previous_attributes": {"status": "pending"}},
            });
            json!({
                "delivery": {
                    "webhook_id": EVENT,
                    "webhook_timestamp": (CREATED + 1_761).to_string(),
                    "webhook_signature": "v1a,0thypM6abf9ly803QGttAKGQfPFKHiwgpxF+b4zWDUCycKswAoJ848WmI7VKQBw8NIWO74zYeRvd7vw/cGOZBw==",
                    "body": body.to_string(),
                },
                "reason": "applied after the restore point, from the merchant's receiver; INC-12",
            })
        }
        "RestoreTreasuryApplyResponse" => json!({
            "applied": true,
            "treasury": treasury(),
        }),
        "RestoreWebhookEndpointDeleteRequest" => json!({
            "account": ACCOUNT,
            "livemode": true,
            "id": ENDPOINT,
            "reason": "deleted by the merchant after the restore point; INC-12",
        }),
        "RestoreDepositAddressRequest" => json!({
            "account": ACCOUNT,
            "livemode": false,
            "client_reference_id": "team-42",
            "address": CUSTOMER_ADDRESS,
            "id": DEPOSIT_ADDRESS,
            "reason": "issued after the restore point, from the merchant's records; INC-12",
        }),
        "RestoreDepositAddressResponse" => json!({
            "reissued": true,
            "deposit_address": deposit_address(),
        }),
        "RestoreQuoteRequest" => json!({
            "account": ACCOUNT,
            "livemode": false,
            "id": QUOTE,
            "client_reference_id": "team-42",
            "chain_id": 1,
            "asset": "PHA",
            "amount": 2500,
            "amount_atomic": "202510000000000000000",
            "exchange_rate": "0.12345679",
            "address": QUOTE_ADDRESS,
            "created": CREATED,
            "expires_at": CREATED + 900,
            "metadata": {"order_id": "ord_1001"},
            "client_secret": format!("{QUOTE}_secret_9f8e7d6c5b4a39281706f5e4d3c2b1a0f9e8d7c6b5a4938271605f4e3d2c1b0a"),
            "reason": "created after the restore point, from the merchant's records; INC-12",
        }),
        "RestoreQuoteResponse" => {
            let mut quote = quote();
            quote["payment"] = Value::Null;
            quote["client_secret"] = Value::Null;
            quote["expires_at"] = json!(CREATED + 1_800);
            json!({"reissued": true, "quote": quote})
        }
        "RestoreEventsImportRequest" => {
            let body = json!({
                "id": EVENT,
                "object": "event",
                "account": ACCOUNT,
                "livemode": false,
                "type": "deposit.credited",
                "created": CREATED,
                "actor": "system",
                "request": null,
                "data": {"object": deposit()},
            });
            json!({
                "deliveries": [{
                    "webhook_id": EVENT,
                    "webhook_timestamp": (CREATED + 31).to_string(),
                    "webhook_signature": "v1a,0thypM6abf9ly803QGttAKGQfPFKHiwgpxF+b4zWDUCycKswAoJ848WmI7VKQBw8NIWO74zYeRvd7vw/cGOZBw==",
                    "body": body.to_string(),
                }],
                "reason": "delivered after the restore point, from the merchant's receiver; INC-12",
            })
        }
        "RestoreDeliveredCreditDiscardRequest" => json!({
            "deposit": DEPOSIT,
            "reason": "INC-12: the chain shows another amount; settled with the merchant",
        }),
        "RestoreDeliveredCreditDiscardResponse" => json!({
            "deposit": DEPOSIT,
            "discarded": true,
        }),
        "RestoreEventsImportResponse" => json!({
            "object": "list",
            "data": [{"id": EVENT, "result": "imported", "reversed_deposit": null}],
        }),
        "RestoreUnfreezeRequest" => json!({
            "reason": "INC-12 reconciled with every merchant; signed off by the on-call lead",
            "security_changes_reapplied": true,
            "deposit_addresses_reissued": true,
            "quotes_reissued": true,
            "delivered_events_imported": true,
        }),
        _ => return None,
    })
}

fn restore() -> Value {
    json!({
        "id": "0b8e2f4a-6c1d-4e3b-9a5f-7d2c4e6a8b1f",
        "object": "restore",
        "detected_at": CREATED + 1_800,
        "detected_by": "restore_check",
        "timeline_id": 2,
        "restore_point": CREATED + 1_740,
        "restored_cursors": {"1": 23_401_200},
        "unfrozen_at": null,
        "unfrozen_by": null,
        "unfreeze_reason": null,
    })
}

fn list(url: &str, item: Value) -> Value {
    json!({"object": "list", "url": url, "has_more": false, "data": [item]})
}

fn quote() -> Value {
    json!({
        "id": QUOTE,
        "object": "quote",
        "livemode": false,
        "client_reference_id": "team-42",
        "amount": 2500,
        "currency": "usd",
        "chain_id": 1,
        "asset": "PHA",
        "amount_atomic": "202510000000000000000",
        "exchange_rate": "0.12345679",
        "address": QUOTE_ADDRESS,
        "treasury": TREASURY_ADDRESS,
        "payment_uri": format!("ethereum:{PHA}@1/transfer?address={QUOTE_ADDRESS}&uint256=202510000000000000000"),
        "status": "open",
        "expires_at": CREATED + 900,
        "created": CREATED,
        "payment": {
            "status": "seen",
            "chain_id": 1,
            "asset": "PHA",
            "tx_hash": TX,
            "amount_atomic": "202510000000000000000",
            "confirmations": 1,
            "estimated_final_at": CREATED + 972,
            "matches_quote": true,
            "deposit": DEPOSIT,
        },
        "deposit": null,
        "client_secret": format!("{QUOTE}_secret_9f8e7d6c5b4a39281706f5e4d3c2b1a0f9e8d7c6b5a4938271605f4e3d2c1b0a"),
        "terms": {
            "quote_ttl_seconds": 900,
            "quote_spread_bps": 100,
            "quote_tolerance_bps": 100,
            "quote_amount_decimals": 4,
            "min_amount": 100,
            "min_deposit_atomic": "0",
            "max_deposit_atomic": "1000000000000000000000000",
            "min_refund_atomic": "1000000000000000000",
            "confirmations": "12",
        },
        "metadata": {"order_id": "ord_1001"},
    })
}

/// The catalog of the example's test mode: PHA and USDT on Ethereum, with their bounds.
fn available(accepted: bool) -> Value {
    let asset = |asset: &str, contract: &str, decimals: u8, pricing: &str| {
        json!({
            "asset": asset,
            "contract": contract,
            "decimals": decimals,
            "pricing": pricing,
            "quote_amount_decimals": 4,
            "accepted": accepted,
            "enabled": accepted,
            "quote_ttl_seconds": {"default": 900, "min": 30, "max": 3600},
            "quote_spread_bps": {"default": 50, "min": 0, "max": 500},
            "quote_tolerance_bps": {"default": 100, "min": 0, "max": 500},
            "min_amount": {"default": 100, "min": 100, "max": u64::MAX},
            "min_deposit_atomic": {"default": "0", "min": "0", "max": U256_MAX},
            "max_deposit_atomic": {
                "default": "1000000000000000000000000",
                "min": "0",
                "max": "1000000000000000000000000",
            },
            "min_refund_atomic": {
                "default": "1000000000000000000",
                "min": "1000000000000000000",
                "max": "1000000000000000000",
            },
        })
    };
    json!([{
        "chain_id": 1,
        "status": if accepted { "active" } else { "not_configured" },
        "confirmations": {"floor": "2", "default": "2"},
        "assets": [
            asset("PHA", PHA, 18, "spot"),
            asset("USDT", "0xdac17f958d2ee523a2206206994597c13d831ec7", 6, "stablecoin"),
        ],
    }])
}

/// The largest base-unit amount, as a decimal string.
const U256_MAX: &str =
    "115792089237316195423570985008687907853269984665640564039457584007913129639935";

fn payment_settings() -> Value {
    json!({
        "object": "payment_settings",
        "livemode": false,
        "status": "configured",
        "revision": "psrev_5b0e4f1a9c3d4e7f8a2b6c1d0e9f8a7b",
        "updated": CREATED - 86_400,
        "quote_creations_per_customer_per_minute": null,
        "chains": [{
            "chain_id": 1,
            "confirmations": "12",
            "assets": [
                {"asset": "PHA", "quote_spread_bps": 100},
                {"asset": "USDT"},
            ],
        }],
        "available": available(true),
    })
}

fn unconfigured_settings(livemode: bool) -> Value {
    json!({
        "object": "payment_settings",
        "livemode": livemode,
        "status": "unconfigured",
        "revision": "psrev_0c1d2e3f4a5b4c6d8e7f9a0b1c2d3e4f",
        "updated": CREATED - 2_592_000,
        "quote_creations_per_customer_per_minute": null,
        "chains": [],
        "available": available(false),
    })
}

fn client_quote() -> Value {
    json!({
        "id": QUOTE,
        "object": "quote",
        "livemode": false,
        "status": "open",
        "amount": 2500,
        "currency": "usd",
        "asset": "PHA",
        "decimals": 18,
        "chain_id": 1,
        "amount_atomic": "202510000000000000000",
        "address": QUOTE_ADDRESS,
        "payment_uri": format!("ethereum:{PHA}@1/transfer?address={QUOTE_ADDRESS}&uint256=202510000000000000000"),
        "expires_at": CREATED + 900,
        "payment_status": "seen",
        "confirmations": 1,
        "amount_credited": null,
        "typical_credit_seconds": 30,
    })
}

fn deposit() -> Value {
    json!({
        "id": DEPOSIT,
        "object": "deposit",
        "livemode": false,
        "client_reference_id": "team-42",
        "quote": QUOTE,
        "deposit_address": null,
        "status": "credited",
        "final": true,
        "final_at": CREATED + 972,
        "swept": false,
        "rejection_reason": null,
        "chain_id": 1,
        "asset": "PHA",
        "asset_contract": PHA,
        "amount_atomic": "202510000000000000000",
        "amount": 2500,
        "currency": "usd",
        "exchange_rate": "0.12345679",
        "price_source": "quote",
        "valued_at": CREATED + 30,
        "address": QUOTE_ADDRESS,
        "from_address": PAYER,
        "tx_hash": TX,
        "receipt_log_index": 0,
        "revision": 0,
        "log_index": 212,
        "block_number": 21_000_000,
        "block_hash": BLOCK_HASH,
        "block_time": CREATED + 12,
        "amount_refunded_atomic": "0",
        "refunded": false,
        "amount_refunded": 0,
        "amount_reversed": 0,
        "replaces": null,
        "replaced_by": null,
        "created": CREATED + 24,
        "metadata": {"order_id": "ord_1001"},
    })
}

fn deposit_address() -> Value {
    json!({
        "id": DEPOSIT_ADDRESS,
        "object": "deposit_address",
        "livemode": false,
        "client_reference_id": "team-42",
        "version": 1,
        "status": "active",
        "address": CUSTOMER_ADDRESS,
        "salt": SALT,
        "networks": [{
            "chain_id": 1,
            "address": CUSTOMER_ADDRESS,
            "treasury": TREASURY_ADDRESS,
            "assets": [{
                "asset": "PHA",
                "contract": PHA,
                "decimals": 18,
                "payment_uri": format!("ethereum:{PHA}@1/transfer?address={CUSTOMER_ADDRESS}"),
            }],
        }],
        "payments": [],
        "client_secret": format!("{DEPOSIT_ADDRESS}_secret_0a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f9"),
        "retired_at": null,
        "created": CREATED,
        "metadata": {"plan": "pro"},
    })
}

fn refund() -> Value {
    json!({
        "id": REFUND,
        "object": "refund",
        "livemode": false,
        "deposit": DEPOSIT,
        "amount_atomic": "202510000000000000000",
        "destination_address": PAYER,
        "treasury": TREASURY_ADDRESS,
        "status": "pending",
        "failure_reason": null,
        "transaction_hash": REFUND_TX,
        "receipt_log_index": 0,
        "created": CREATED + 3_600,
        "metadata": {"ticket": "support-311"},
    })
}

fn treasury() -> Value {
    json!({
        "id": TREASURY,
        "object": "treasury",
        "livemode": false,
        "chain_id": 1,
        "address": TREASURY_ADDRESS,
        "kind": "contract",
        "status": "active",
        "effective_at": CREATED - 86_400,
        "created": CREATED - 86_400,
        "replaced_at": null,
        "canceled_at": null,
        "cancellation_reason": null,
        "crediting_paused": false,
        "crediting_paused_by": [],
    })
}

fn treasury_message() -> String {
    format!(
        "pay-api.phala.com wants you to sign in with your Ethereum account:\n\
         0x936c1991f8dA9a919fa11b557a3514719f5A4504\n\n\
         Set this address as the test mode treasury of {ACCOUNT} on Phala Pay.\n\n\
         URI: https://pay-api.phala.com\nVersion: 1\nChain ID: 1\nNonce: Kq3nV8xZt2mP6wRa\n\
         Issued At: 2026-09-28T12:00:00Z\nExpiration Time: 2026-09-28T12:10:00Z"
    )
}

fn event() -> Value {
    let mut deposit = deposit();
    deposit["final"] = json!(false);
    json!({
        "id": EVENT,
        "object": "event",
        "account": ACCOUNT,
        "livemode": false,
        "type": "deposit.credited",
        "created": CREATED + 30,
        "actor": "system",
        "request": null,
        "data": {"object": deposit},
        "pending_webhooks": 0,
    })
}

fn webhook_endpoint() -> Value {
    json!({
        "id": ENDPOINT,
        "object": "webhook_endpoint",
        "livemode": false,
        "url": "https://example.com/phala-pay/webhooks",
        "enabled_events": ["deposit.credited", "deposit.reversed", "refund.failed"],
        "status": "enabled",
        "disabled_reason": null,
        "description": "Order fulfillment",
        "metadata": {"team": "payments"},
        "created": CREATED - 86_400,
        "pending_deliveries": 2,
        "oldest_pending_at": CREATED + 30,
        "last_attempt": {"at": CREATED + 95, "status_code": 503},
    })
}

/// The first secret key the operator issues with an account, with its secret.
fn first_key() -> Value {
    json!({
        "id": API_KEY,
        "object": "api_key",
        "livemode": false,
        "type": "secret",
        "name": "",
        "permissions": null,
        "secret": "ppay_sk_test_51Ab3Cd5Ef7Gh9Jk2Lm4Np6Qr8St0Uv2Wx4Yz",
        "redacted": "ppay_sk_test_…x4Yz",
        "status": "active",
        "created": CREATED - 86_400,
        "expires_at": null,
        "last_used": null,
    })
}

fn api_key() -> Value {
    json!({
        "id": API_KEY,
        "object": "api_key",
        "livemode": false,
        "type": "restricted",
        "name": "fulfillment worker",
        "permissions": RUNTIME_PERMISSIONS,
        "redacted": "ppay_rk_test_…Yz4x",
        "status": "active",
        "created": CREATED - 86_400,
        "expires_at": null,
        "last_used": CREATED,
    })
}

/// An `*.updated` event with the request that caused it, for the events guide.
#[cfg(test)]
pub(super) fn updated_event() -> Value {
    json!({
        "id": EVENT,
        "object": "event",
        "account": ACCOUNT,
        "livemode": false,
        "type": "webhook_endpoint.updated",
        "created": CREATED,
        "actor": API_KEY,
        "request": {"id": REQUEST, "idempotency_key": "f1a2b3c4-endpoint-update"},
        "data": {
            "object": webhook_endpoint(),
            "previous_attributes": {"enabled_events": ["deposit.credited"]},
        },
        "pending_webhooks": 1,
    })
}

fn manual_entry() -> Value {
    json!({"address":PAYER,"reason":"reviewed designation","source_ref":"UK entity reference",
           "created_by":"admin:admin/test-v1","created_at":"2026-09-28T12:00:00Z"})
}
