//! Treasuries through the API (docs/design/multi-tenant.md D10): EIP-4361 proofs by an EOA or,
//! on Anvil, by a deployed EIP-1271 contract; refused proofs; the 48-hour time-lock of a live
//! change, its cancellation, and the deposit address networks it moves; tenancy.

mod support;

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;

use alloy::sol;
use alloy::sol_types::{Eip712Domain, SolCall as _, SolStruct as _};
use alloy_primitives::{Address, B256, Bytes, U256, eip191_hash_message, keccak256};
use alloy_signer::Signer as _;
use alloy_signer_local::PrivateKeySigner;
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use axum::Router;
use axum::body::to_bytes;
use axum::http::{Method, StatusCode};
use chrono::{Duration, Utc};
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use topup::api::{AppState, PublicOrigin, VerificationKey};
use topup::db;
use topup::locks::QuoteProvider;
use topup::locks::pricing::ValidatedQuote;
use topup::pump::{Step as _, StepResult};
use topup::refunds::{DestinationScreener, DestinationScreening};
use topup::routes::RouteSet;
use topup::steps::screen::{ScreenRoute, ScreenStep};
use topup::treasuries::{
    CHALLENGE_TTL, ContractAnswer, ContractSignatures, EvmContractSignatures, RESCREEN_INTERVAL,
    Rescreen, TIME_LOCK, apply_due, rescreen_due,
};
use topup_adapters::attestation::DstackAttestor;
use topup_adapters::chain::evm::EvmClient;
use topup_adapters::risk::oracle::SanctionsSource;
use topup_core::deposit::{StepOutcome, WaitReason};
use topup_core::money::{AtomicAmount, PRICE_SCALE, ScaledPrice};
use topup_core::route::RouteFile;
use topup_core::screening::{SanctionsResult, SanctionsVerdict};
use tower::ServiceExt;

use support::chain::{ANVIL_PRIVATE_KEY, Anvil, CHAIN_ID, forge_create_in_profile, run_checked};
use support::seed::{self, NewAccount};
use support::{
    TEST_ORIGIN, TestDatabase, merchant_request, public_key_base64, signed_request, with_database,
};

/// A second live chain (OP Mainnet) beside Ethereum's chain 1.
const OTHER_CHAIN: u64 = 10;
const SEPOLIA: u64 = 11_155_111;
/// The admin signing key of the test router.
const ADMIN_KEY: [u8; 32] = [53; 32];

#[tokio::test]
async fn an_eoa_proves_its_first_treasury_with_siwe_and_it_applies_at_once() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let fixture = Fixture::new(&database.app_pool, Contracts::None).await?;
            let signer = PrivateKeySigner::random();
            // No treasury yet: nothing can be issued.
            let (status, body) = fixture.quote(&fixture.live_key, 1).await?;
            ensure!(status == StatusCode::BAD_REQUEST, "{body}");
            ensure!(body["error"]["code"] == "treasury_not_set");

            let challenge = fixture
                .challenge(&fixture.live_key, 1, signer.address())
                .await?;
            let message = challenge["message"].as_str().context("message")?;
            let account = &fixture.account.public_id;
            let lines: Vec<&str> = message.lines().collect();
            ensure!(lines[0] == "api.test wants you to sign in with your Ethereum account:");
            ensure!(lines[1] == signer.address().to_checksum(None));
            ensure!(
                lines[3]
                    == format!(
                        "Set this address as the live mode treasury of {account} on Phala Pay."
                    )
            );
            ensure!(lines[5] == "URI: http://api.test" && lines[6] == "Version: 1");
            ensure!(lines[7] == "Chain ID: 1");
            ensure!(
                lines[8] == format!("Nonce: {}", challenge["nonce"].as_str().context("nonce")?)
            );
            ensure!(lines[10].starts_with("Expiration Time: "));
            let expires_in =
                challenge["expires_at"].as_i64().context("expires_at")? - Utc::now().timestamp();
            ensure!((590..=600).contains(&expires_in), "{expires_in}");

            let signature = sign(&signer, message).await?;
            let (status, treasury) = fixture
                .submit(&fixture.live_key, 1, message, &signature)
                .await?;
            ensure!(status == StatusCode::OK, "{treasury}");
            ensure!(treasury["object"] == "treasury" && treasury["livemode"] == true);
            ensure!(treasury["status"] == "active" && treasury["kind"] == "eoa");
            ensure!(treasury["address"] == format!("{:#x}", signer.address()));
            ensure!(treasury["effective_at"] == treasury["created"]);
            let id = treasury["id"].as_str().context("id")?;
            ensure!(id.starts_with("trs_"));

            // Quotes and deposit addresses pay it on its chain, and only there.
            let (status, quote) = fixture.quote(&fixture.live_key, 1).await?;
            ensure!(status == StatusCode::OK, "{quote}");
            ensure!(quote["treasury"] == format!("{:#x}", signer.address()));
            let (status, _) = fixture.quote(&fixture.live_key, OTHER_CHAIN).await?;
            ensure!(status == StatusCode::BAD_REQUEST);
            let address = fixture.deposit_address(&fixture.live_key, "team-1").await?;
            ensure!(chain_ids(&address) == vec![1], "{address}");

            // The account security event carries the account and mode and reaches an endpoint
            // that subscribes to other events only.
            // A first treasury is created active, with nothing to replace.
            let events = fixture.events("treasury.created").await?;
            ensure!(events == vec![(fixture.account.id, true, "treasury".to_owned())]);
            ensure!(fixture.deliveries("treasury.created").await? == 1);
            ensure!(fixture.events("treasury.updated").await?.is_empty());
            let data = fixture.event_data("treasury.created").await?;
            ensure!(data[0]["object"]["status"] == "active", "{data:?}");
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn proofs_are_refused_unless_they_answer_an_unused_live_challenge_of_the_scope() -> Result<()>
{
    with_database(|database| {
        Box::pin(async move {
            let fixture = Fixture::new(&database.app_pool, Contracts::None).await?;
            let signer = PrivateKeySigner::random();
            let refused = |status: StatusCode, body: &Value, code: &str, param: &str| {
                ensure!(status == StatusCode::BAD_REQUEST, "{body}");
                ensure!(body["error"]["code"] == code, "{body}");
                ensure!(body["error"]["param"] == param, "{body}");
                Ok(())
            };

            // Expired.
            let challenge = fixture
                .challenge(&fixture.live_key, 1, signer.address())
                .await?;
            let message = challenge["message"].as_str().context("message")?;
            sqlx::query(
                "UPDATE treasury_challenges SET expires_at = now() - interval '1 second' \
                 WHERE nonce = $1",
            )
            .bind(challenge["nonce"].as_str())
            .execute(&fixture.pool)
            .await?;
            let signature = sign(&signer, message).await?;
            let (status, body) = fixture
                .submit(&fixture.live_key, 1, message, &signature)
                .await?;
            refused(status, &body, "treasury_challenge_expired", "message")?;

            // The message's chain is not the request's.
            let challenge = fixture
                .challenge(&fixture.live_key, 1, signer.address())
                .await?;
            let message = challenge["message"].as_str().context("message")?;
            let signature = sign(&signer, message).await?;
            let (status, body) = fixture
                .submit(&fixture.live_key, OTHER_CHAIN, message, &signature)
                .await?;
            refused(status, &body, "treasury_proof_invalid", "chain_id")?;
            // Editing the chain (or anything else) in the message breaks it.
            let edited = message.replace("Chain ID: 1", &format!("Chain ID: {OTHER_CHAIN}"));
            let signature_edited = sign(&signer, &edited).await?;
            let (status, body) = fixture
                .submit(&fixture.live_key, OTHER_CHAIN, &edited, &signature_edited)
                .await?;
            refused(status, &body, "treasury_proof_invalid", "message")?;
            // Another key's signature: not an EOA's, and no contract is deployed there.
            let stranger = sign(&PrivateKeySigner::random(), message).await?;
            let (status, body) = fixture
                .submit(&fixture.live_key, 1, message, &stranger)
                .await?;
            refused(status, &body, "treasury_not_deployed", "signature")?;
            // An ERC-6492 wrapper (a counterfactual contract's signature) is refused outright.
            let wrapped = format!(
                "{}{}",
                signature, "6492649264926492649264926492649264926492649264926492649264926492"
            );
            let (status, body) = fixture
                .submit(&fixture.live_key, 1, message, &wrapped)
                .await?;
            refused(status, &body, "treasury_proof_invalid", "signature")?;
            ensure!(
                body["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("ERC-6492")),
                "{body}"
            );
            // A challenge of the other mode is not this mode's.
            let (status, body) = fixture
                .submit(&fixture.test_key, 1, message, &signature)
                .await?;
            ensure!(status == StatusCode::BAD_REQUEST, "{body}");

            // The right proof succeeds once; the same nonce is refused afterwards.
            let (status, body) = fixture
                .submit(&fixture.live_key, 1, message, &signature)
                .await?;
            ensure!(status == StatusCode::OK, "{body}");
            let (status, body) = fixture
                .submit(&fixture.live_key, 1, message, &signature)
                .await?;
            refused(status, &body, "treasury_challenge_used", "message")?;

            // Another account cannot use this account's challenge.
            let other = fixture.other_account().await?;
            let challenge = fixture
                .challenge(&fixture.live_key, 1, signer.address())
                .await?;
            let message = challenge["message"].as_str().context("message")?;
            let signature = sign(&signer, message).await?;
            let (status, body) = fixture.submit(&other, 1, message, &signature).await?;
            refused(status, &body, "treasury_proof_invalid", "message")?;
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_sanctioned_treasury_is_refused() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let mut fixture = Fixture::new(&database.app_pool, Contracts::None).await?;
            fixture.app = app(
                &fixture.pool,
                fixture.routes.clone(),
                Arc::new(Sanctioned),
                Arc::new(NoContracts),
            )?;
            let signer = PrivateKeySigner::random();
            let (status, body) = fixture.prove(&fixture.live_key, 1, &signer).await?;
            ensure!(status == StatusCode::BAD_REQUEST, "{body}");
            ensure!(body["error"]["code"] == "treasury_sanctioned");
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM treasuries")
                .fetch_one(&fixture.pool)
                .await?;
            ensure!(count == 0);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_live_change_waits_48_hours_then_moves_that_chains_deposit_addresses() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let fixture = Fixture::new(&database.app_pool, Contracts::None).await?;
            let first = PrivateKeySigner::random();
            let next = PrivateKeySigner::random();
            for chain_id in [1, OTHER_CHAIN] {
                let (status, body) = fixture.prove(&fixture.live_key, chain_id, &first).await?;
                ensure!(
                    status == StatusCode::OK && body["status"] == "active",
                    "{body}"
                );
            }
            let before = fixture.deposit_address(&fixture.live_key, "team-1").await?;
            let shared = before["address"]
                .as_str()
                .context("shared address")?
                .to_owned();
            let (_, old_quote) = fixture.quote(&fixture.live_key, 1).await?;

            // A later live change is pending for 48 hours.
            let (status, pending) = fixture.prove(&fixture.live_key, 1, &next).await?;
            ensure!(status == StatusCode::OK, "{pending}");
            ensure!(pending["status"] == "pending", "{pending}");
            let lock = pending["effective_at"].as_i64().context("effective_at")?
                - pending["created"].as_i64().context("created")?;
            ensure!(lock == TIME_LOCK.num_seconds(), "{lock}");
            // Two first treasuries created active, and the pending change.
            ensure!(fixture.events("treasury.created").await?.len() == 3);
            ensure!(fixture.deliveries("treasury.created").await? == 3);
            // Only one change waits per chain, and the current treasury is not a change.
            let (status, body) = fixture.prove(&fixture.live_key, 1, &next).await?;
            ensure!(status == StatusCode::BAD_REQUEST, "{body}");
            ensure!(body["error"]["code"] == "treasury_change_pending");
            let (status, body) = fixture
                .prove(&fixture.live_key, OTHER_CHAIN, &first)
                .await?;
            ensure!(body["error"]["code"] == "treasury_unchanged" && status.as_u16() == 400);

            // Until it applies, quotes and addresses keep paying the current treasury.
            let (_, quote) = fixture.quote(&fixture.live_key, 1).await?;
            ensure!(quote["treasury"] == format!("{:#x}", first.address()));
            let now = Utc::now();
            let routes = fixture.route_set()?;
            ensure!(
                apply_due(&fixture.pool, &routes, &Clear, now + Duration::hours(47)).await? == 0
            );
            ensure!(fixture.deposit_address(&fixture.live_key, "team-1").await? == before);

            // After 48 hours it applies, and the chain's network of every deposit address moves.
            ensure!(
                apply_due(
                    &fixture.pool,
                    &routes,
                    &Clear,
                    now + TIME_LOCK + Duration::minutes(1)
                )
                .await?
                    == 1
            );
            let id = pending["id"].as_str().context("id")?;
            let (_, applied) = fixture.get(&format!("/v1/treasuries/{id}")).await?;
            ensure!(applied["status"] == "active", "{applied}");
            let (_, list) = fixture.get("/v1/treasuries?chain_id=1").await?;
            let statuses: Vec<&str> = list["data"]
                .as_array()
                .context("data")?
                .iter()
                .filter_map(|treasury| treasury["status"].as_str())
                .collect();
            ensure!(statuses == vec!["active", "replaced"], "{list}");
            // The change applies, and the treasury it replaces is `replaced`, each with the
            // status it left in `previous_attributes`.
            let updated = fixture.event_data("treasury.updated").await?;
            ensure!(updated.len() == 2, "{updated:?}");
            let statuses: Vec<(&Value, &Value)> = updated
                .iter()
                .map(|data| {
                    (
                        &data["object"]["status"],
                        &data["previous_attributes"]["status"],
                    )
                })
                .collect();
            ensure!(
                statuses.contains(&(&json!("active"), &json!("pending")))
                    && statuses.contains(&(&json!("replaced"), &json!("active"))),
                "{updated:?}"
            );
            // The pending change's own `treasury.created` still shows it pending.
            let created = fixture.event_data("treasury.created").await?;
            ensure!(
                created
                    .iter()
                    .any(|data| data["object"]["id"] == pending["id"]
                        && data["object"]["status"] == "pending"),
                "{created:?}"
            );

            let after = fixture.deposit_address(&fixture.live_key, "team-1").await?;
            ensure!(
                after["id"] == before["id"] && after["address"].is_null(),
                "{after}"
            );
            let [ethereum, optimism] = networks(&after)? else {
                anyhow::bail!("two networks: {after}");
            };
            ensure!(ethereum["treasury"] == format!("{:#x}", next.address()));
            ensure!(ethereum["address"] != shared.as_str());
            ensure!(optimism["address"] == shared.as_str());
            // The old chain-1 forwarder is kept, watched, and pays the old treasury.
            let superseded: (String, bool) = sqlx::query_as(
                "SELECT treasury, superseded_at IS NOT NULL FROM addresses \
                 WHERE chain_id = 1 AND address = $1",
            )
            .bind(&shared)
            .fetch_one(&fixture.pool)
            .await?;
            ensure!(superseded == (format!("{:#x}", first.address()), true));
            let watched = db::list_scan_addresses(&fixture.pool, 1).await?;
            ensure!(
                watched
                    .iter()
                    .any(|watched| format!("{:#x}", watched.address) == shared)
            );

            // A quote created before keeps its address; new ones pay the new treasury.
            let (_, kept) = fixture
                .get(&format!(
                    "/v1/quotes/{}",
                    old_quote["id"].as_str().context("id")?
                ))
                .await?;
            ensure!(kept["address"] == old_quote["address"]);
            ensure!(kept["treasury"] == format!("{:#x}", first.address()));
            let (_, quote) = fixture.quote(&fixture.live_key, 1).await?;
            ensure!(quote["treasury"] == format!("{:#x}", next.address()));
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_pending_change_can_be_canceled_during_the_lock() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let fixture = Fixture::new(&database.app_pool, Contracts::None).await?;
            let first = PrivateKeySigner::random();
            let next = PrivateKeySigner::random();
            fixture.prove(&fixture.live_key, 1, &first).await?;
            let (_, pending) = fixture.prove(&fixture.live_key, 1, &next).await?;
            let id = pending["id"].as_str().context("id")?;
            let cancel = format!("/v1/treasuries/{id}/cancel");
            let (status, canceled) = fixture
                .post(&fixture.live_key, &cancel, Value::Null)
                .await?;
            ensure!(status == StatusCode::OK, "{canceled}");
            ensure!(canceled["status"] == "canceled" && canceled["canceled_at"].is_i64());
            ensure!(fixture.events("treasury.canceled").await?.len() == 1);
            ensure!(fixture.deliveries("treasury.canceled").await? == 1);
            let (status, body) = fixture
                .post(&fixture.live_key, &cancel, Value::Null)
                .await?;
            ensure!(status == StatusCode::BAD_REQUEST, "{body}");
            ensure!(body["error"]["code"] == "treasury_unexpected_state");
            // It never applies; the current treasury stays.
            let later = Utc::now() + TIME_LOCK + Duration::hours(1);
            ensure!(apply_due(&fixture.pool, &fixture.route_set()?, &Clear, later).await? == 0);
            let (_, quote) = fixture.quote(&fixture.live_key, 1).await?;
            ensure!(quote["treasury"] == format!("{:#x}", first.address()));
            // A new change can be requested after the cancellation.
            let (status, again) = fixture.prove(&fixture.live_key, 1, &next).await?;
            ensure!(
                status == StatusCode::OK && again["status"] == "pending",
                "{again}"
            );
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_change_to_a_treasury_sanctioned_by_its_effective_time_is_canceled_not_applied()
-> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let fixture = Fixture::new(&database.app_pool, Contracts::None).await?;
            let first = PrivateKeySigner::random();
            let next = PrivateKeySigner::random();
            fixture.prove(&fixture.live_key, 1, &first).await?;
            let (_, pending) = fixture.prove(&fixture.live_key, 1, &next).await?;
            ensure!(pending["status"] == "pending", "{pending}");
            let id = pending["id"].as_str().context("id")?;
            let routes = fixture.route_set()?;
            let due = Utc::now() + TIME_LOCK + Duration::minutes(1);

            // Screening that cannot answer leaves it pending, to be tried again.
            ensure!(apply_due(&fixture.pool, &routes, &Unavailable, due).await? == 0);
            let (_, still) = fixture.get(&format!("/v1/treasuries/{id}")).await?;
            ensure!(still["status"] == "pending", "{still}");

            // Listed by then: canceled with its reason, never applied.
            let listing = Listed(vec![next.address()]);
            ensure!(apply_due(&fixture.pool, &routes, &listing, due).await? == 0);
            let (_, canceled) = fixture.get(&format!("/v1/treasuries/{id}")).await?;
            ensure!(canceled["status"] == "canceled", "{canceled}");
            ensure!(
                canceled["cancellation_reason"] == "sanctioned",
                "{canceled}"
            );
            let events = fixture.events("treasury.canceled").await?;
            ensure!(events == vec![(fixture.account.id, true, "treasury".to_owned())]);
            ensure!(fixture.deliveries("treasury.canceled").await? == 1);
            let actor: String =
                sqlx::query_scalar("SELECT actor FROM events WHERE type = 'treasury.canceled'")
                    .fetch_one(&fixture.pool)
                    .await?;
            ensure!(actor == "system");
            // The current treasury stays; quotes keep paying it.
            let (_, quote) = fixture.quote(&fixture.live_key, 1).await?;
            ensure!(quote["treasury"] == format!("{:#x}", first.address()));
            ensure!(apply_due(&fixture.pool, &routes, &Clear, due).await? == 0);

            // A merchant's own cancellation says so.
            let (_, again) = fixture.prove(&fixture.live_key, 1, &next).await?;
            let again = again["id"].as_str().context("id")?;
            let (_, canceled) = fixture
                .post(
                    &fixture.live_key,
                    &format!("/v1/treasuries/{again}/cancel"),
                    Value::Null,
                )
                .await?;
            ensure!(canceled["cancellation_reason"] == "requested", "{canceled}");
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_daily_rescreen_pauses_quotes_and_settlement_of_an_account_with_a_sanctioned_treasury()
-> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let fixture = Fixture::new(&database.app_pool, Contracts::None).await?;
            let treasury = PrivateKeySigner::random();
            let (_, set) = fixture.prove(&fixture.live_key, 1, &treasury).await?;
            ensure!(set["status"] == "active", "{set}");
            let routes = fixture.route_set()?;
            let listing = Listed(vec![treasury.address()]);
            let now = Utc::now();

            // Screened when set: nothing is due within a day.
            let outcome =
                rescreen_due(&fixture.pool, &routes, &listing, now + Duration::hours(23)).await?;
            ensure!(outcome == Rescreen::default(), "{outcome:?}");
            // A day later it is screened again; clear, and not again for another day.
            let day = now + RESCREEN_INTERVAL + Duration::minutes(1);
            ensure!(
                rescreen_due(&fixture.pool, &routes, &Clear, day)
                    .await?
                    .clear
                    == 1
            );
            let outcome =
                rescreen_due(&fixture.pool, &routes, &listing, day + Duration::hours(1)).await?;
            ensure!(outcome == Rescreen::default(), "{outcome:?}");
            // Unavailable screening changes nothing and retries.
            let later = day + RESCREEN_INTERVAL + Duration::minutes(1);
            ensure!(
                rescreen_due(&fixture.pool, &routes, &Unavailable, later).await?
                    == Rescreen::default()
            );

            // Listed: the account's quotes and settlement pause, audited and announced.
            let outcome = rescreen_due(&fixture.pool, &routes, &listing, later).await?;
            ensure!(outcome.sanctioned == 1, "{outcome:?}");
            let scopes: Vec<String> =
                sqlx::query_scalar("SELECT paused_scopes FROM accounts WHERE id = $1")
                    .bind(fixture.account.id)
                    .fetch_one(&fixture.pool)
                    .await?;
            ensure!(scopes == vec!["quotes", "settlement"], "{scopes:?}");
            let audit: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM audit WHERE account_id = $1 AND action = 'pause' \
                 AND actor_type = 'system' AND reason LIKE '%sanctions list%'",
            )
            .bind(fixture.account.id)
            .fetch_one(&fixture.pool)
            .await?;
            ensure!(audit == 1);
            let updated = fixture.events("account.updated").await?;
            ensure!(updated.len() == 2, "{updated:?}");
            let (status, body) = fixture.quote(&fixture.live_key, 1).await?;
            ensure!(status == StatusCode::BAD_REQUEST && body["error"]["code"] == "paused");

            // The operator lifts the pause after review, through the admin API.
            let request = |path: &str| -> Result<_> {
                Ok(signed_request(
                    Method::POST,
                    path,
                    serde_json::to_vec(&json!({
                        "scopes": ["quotes", "settlement"],
                        "reason": "INC-9: the treasury was replaced and reviewed",
                    }))?,
                    "admin/v1",
                    &SigningKey::from_bytes(&ADMIN_KEY),
                    Utc::now().timestamp(),
                ))
            };
            let account = &fixture.account.public_id;
            let response = fixture
                .app
                .clone()
                .oneshot(request(&format!("/v1/admin/accounts/{account}/resume"))?)
                .await?;
            ensure!(response.status() == StatusCode::OK);
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 1_048_576).await?)?;
            ensure!(body["paused_scopes"] == json!([]), "{body}");
            let (status, body) = fixture.quote(&fixture.live_key, 1).await?;
            ensure!(status == StatusCode::OK, "{body}");
            Ok(())
        })
    })
    .await
}

/// In an incident, such as a compromised former treasury, the merchant and the operator each pause
/// crediting of the forwarders over one treasury: their deposits stay `confirmed` (`pending` in the
/// API) with no `deposit.credited` until both pauses are lifted, while deposits to another treasury
/// are credited. Each change is audited and announced as `treasury.updated`.
#[tokio::test]
async fn crediting_pauses_per_treasury_and_resumes() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let fixture = Fixture::new(&database.app_pool, Contracts::None).await?;
            let old = PrivateKeySigner::random();
            let new = PrivateKeySigner::random();
            let (_, old_treasury) = fixture.prove(&fixture.live_key, 1, &old).await?;
            let old_id = old_treasury["id"].as_str().context("id")?.to_owned();
            ensure!(old_treasury["crediting_paused"] == false);
            let (_, old_quote) = fixture.quote(&fixture.live_key, 1).await?;
            fixture.prove(&fixture.live_key, 1, &new).await?;
            let later = Utc::now() + TIME_LOCK + Duration::hours(1);
            ensure!(apply_due(&fixture.pool, &fixture.route_set()?, &Clear, later).await? == 1);
            let (_, new_quote) = fixture.quote(&fixture.live_key, 1).await?;
            ensure!(new_quote["treasury"] == format!("{:#x}", new.address()));
            let to_old = fixture.confirmed_deposit(&old_quote, 1).await?;
            let to_new = fixture.confirmed_deposit(&new_quote, 2).await?;
            let step = fixture.screen_step()?;
            let outcome = |deposit: uuid::Uuid| {
                let step = &step;
                let pool = &fixture.pool;
                async move {
                    let deposit = db::get_deposit(pool, deposit).await?.context("deposit")?;
                    anyhow::Ok(step.run(&deposit).await)
                }
            };
            let held = |result: &StepResult| {
                result.outcome
                    == StepOutcome::Wait {
                        reason: WaitReason::Paused,
                    }
                    && result.events.is_empty()
            };
            ensure!(outcome(to_old).await?.outcome == StepOutcome::Advance);

            // The merchant pauses the former treasury; only its forwarders are held.
            let pause = format!("/v1/treasuries/{old_id}/pause");
            let (status, paused) = fixture.post(&fixture.live_key, &pause, Value::Null).await?;
            ensure!(status == StatusCode::OK, "{paused}");
            ensure!(paused["status"] == "replaced" && paused["crediting_paused"] == true);
            ensure!(paused["crediting_paused_by"] == json!(["merchant"]));
            let result = outcome(to_old).await?;
            ensure!(held(&result), "{:?}", result.outcome);
            ensure!(result.evidence["pause_scopes"]["treasury"] == json!(["settlement"]));
            ensure!(outcome(to_new).await?.outcome == StepOutcome::Advance);
            let updated = fixture.event_data("treasury.updated").await?;
            let notice = updated.last().context("treasury.updated")?;
            ensure!(notice["object"]["crediting_paused_by"] == json!(["merchant"]));
            ensure!(
                notice["previous_attributes"]
                    == json!({"crediting_paused": false, "crediting_paused_by": []}),
                "{notice}"
            );
            // Pausing again changes nothing and announces nothing.
            let notices = updated.len();
            let (status, _) = fixture.post(&fixture.live_key, &pause, Value::Null).await?;
            ensure!(status == StatusCode::OK);
            ensure!(fixture.event_data("treasury.updated").await?.len() == notices);

            // The operator pauses it too; the merchant's resume does not lift the operator's.
            let account = &fixture.account.public_id;
            let admin = |action: &str| -> Result<_> {
                Ok(signed_request(
                    Method::POST,
                    &format!("/v1/admin/accounts/{account}/treasuries/{old_id}/{action}"),
                    serde_json::to_vec(&json!({"reason": "INC-12: former treasury compromised"}))?,
                    "admin/v1",
                    &SigningKey::from_bytes(&ADMIN_KEY),
                    Utc::now().timestamp(),
                ))
            };
            let response = fixture.app.clone().oneshot(admin("pause")?).await?;
            ensure!(response.status() == StatusCode::OK);
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 1_048_576).await?)?;
            ensure!(
                body["crediting_paused_by"] == json!(["merchant", "operator"]),
                "{body}"
            );
            let (status, resumed) = fixture
                .post(
                    &fixture.live_key,
                    &format!("/v1/treasuries/{old_id}/resume"),
                    Value::Null,
                )
                .await?;
            ensure!(status == StatusCode::OK, "{resumed}");
            ensure!(resumed["crediting_paused_by"] == json!(["operator"]));
            ensure!(held(&outcome(to_old).await?));

            // Once the operator resumes too, the held deposit is credited.
            let response = fixture.app.clone().oneshot(admin("resume")?).await?;
            ensure!(response.status() == StatusCode::OK);
            let result = outcome(to_old).await?;
            ensure!(result.outcome == StepOutcome::Advance);
            ensure!(
                result
                    .events
                    .iter()
                    .any(|event| event.event_type == "deposit.credited")
            );
            let audited: Vec<(String, String)> = sqlx::query_as(
                "SELECT actor_type, reason FROM audit \
                 WHERE account_id = $1 AND action = 'treasury.updated' ORDER BY created_at",
            )
            .bind(fixture.account.id)
            .fetch_all(&fixture.pool)
            .await?;
            let admin_rows = audited
                .iter()
                .filter(|(actor, reason)| actor == "admin" && reason.starts_with("INC-12"))
                .count();
            ensure!(admin_rows == 2, "{audited:?}");
            // Another account cannot reach the treasury.
            let other = fixture.other_account().await?;
            let (status, _) = fixture.post(&other, &pause, Value::Null).await?;
            ensure!(status == StatusCode::NOT_FOUND);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn test_mode_changes_apply_at_once() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let fixture = Fixture::new(&database.app_pool, Contracts::None).await?;
            let first = PrivateKeySigner::random();
            let next = PrivateKeySigner::random();
            let (_, body) = fixture.prove(&fixture.test_key, SEPOLIA, &first).await?;
            ensure!(
                body["status"] == "active" && body["livemode"] == false,
                "{body}"
            );
            let before = fixture.deposit_address(&fixture.test_key, "team-1").await?;
            let (status, body) = fixture.prove(&fixture.test_key, SEPOLIA, &next).await?;
            ensure!(
                status == StatusCode::OK && body["status"] == "active",
                "{body}"
            );
            let after = fixture.deposit_address(&fixture.test_key, "team-1").await?;
            ensure!(after["id"] == before["id"] && after["address"] != before["address"]);
            ensure!(networks(&after)?[0]["treasury"] == format!("{:#x}", next.address()));
            let events = fixture.events("treasury.created").await?;
            ensure!(events.len() == 2 && events.iter().all(|(_, livemode, _)| !livemode));
            let replaced = fixture.events("treasury.updated").await?;
            ensure!(replaced.len() == 1 && !replaced[0].1);
            // A test key does not reach live chains.
            let (status, body) = fixture
                .post(
                    &fixture.test_key,
                    "/v1/treasuries/challenge",
                    json!({"chain_id": 1, "address": format!("{:#x}", first.address())}),
                )
                .await?;
            ensure!(status == StatusCode::BAD_REQUEST && body["error"]["param"] == "chain_id");
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn treasuries_of_another_account_or_mode_are_not_found() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let fixture = Fixture::new(&database.app_pool, Contracts::None).await?;
            fixture
                .prove(&fixture.live_key, 1, &PrivateKeySigner::random())
                .await?;
            let (_, pending) = fixture
                .prove(&fixture.live_key, 1, &PrivateKeySigner::random())
                .await?;
            let id = pending["id"].as_str().context("id")?;
            let other = fixture.other_account().await?;
            for key in [&other, &fixture.test_key] {
                let (status, _) = fixture
                    .request(
                        Method::GET,
                        &format!("/v1/treasuries/{id}"),
                        key,
                        Value::Null,
                    )
                    .await?;
                ensure!(status == StatusCode::NOT_FOUND);
                let (status, _) = fixture
                    .post(key, &format!("/v1/treasuries/{id}/cancel"), Value::Null)
                    .await?;
                ensure!(status == StatusCode::NOT_FOUND);
                let (_, list) = fixture
                    .request(Method::GET, "/v1/treasuries", key, Value::Null)
                    .await?;
                ensure!(list["data"] == json!([]), "{list}");
            }
            let (_, list) = fixture.get("/v1/treasuries?status=pending").await?;
            ensure!(list["data"].as_array().map(Vec::len) == Some(1), "{list}");
            Ok(())
        })
    })
    .await
}

/// Safe v1.4.1's canonical runtime code (`safe-global/safe-deployments`, `src/assets/v1.4.1`), read
/// from Sepolia and hashed without its Solidity metadata, which names the build's source paths:
/// the vendored build (`contracts/lib/safe-smart-account` at tag v1.4.1, solc 0.7.6, optimizer
/// off, as its CHANGELOG records) must deploy exactly this code.
const CANONICAL_SAFE_CODE: [(&str, &str); 4] = [
    (
        "lib/safe-smart-account/contracts/Safe.sol:Safe",
        "0xc24f07894e481e749fee29336ba775cdf2df546259e23fd349c798e62fe43a69",
    ),
    (
        "lib/safe-smart-account/contracts/handler/CompatibilityFallbackHandler.sol:CompatibilityFallbackHandler",
        "0x9d69690da03ccc191bc3518df346bcf7076d69b702df79ddaf0003bcddffa70f",
    ),
    (
        "lib/safe-smart-account/contracts/proxies/SafeProxyFactory.sol:SafeProxyFactory",
        "0x69e98bb1aa0a54c54f3795d63d4f0be955a9df4e12bbfe69b453663164f85442",
    ),
    (
        "lib/safe-smart-account/contracts/libraries/SignMessageLib.sol:SignMessageLib",
        "0x33a1b7baaccde380049110a45a710c42d7fc2b728bcd412fc05ef693d12708b3",
    ),
];

sol! {
    function setup(
        address[] owners,
        uint256 threshold,
        address to,
        bytes data,
        address fallbackHandler,
        address paymentToken,
        uint256 payment,
        address paymentReceiver
    );
    function createProxyWithNonce(address singleton, bytes initializer, uint256 saltNonce)
        returns (address proxy);
    function signMessage(bytes data);
    function getTransactionHash(
        address to,
        uint256 value,
        bytes data,
        uint8 operation,
        uint256 safeTxGas,
        uint256 baseGas,
        uint256 gasPrice,
        address gasToken,
        address refundReceiver,
        uint256 nonce
    ) returns (bytes32);
    function execTransaction(
        address to,
        uint256 value,
        bytes data,
        uint8 operation,
        uint256 safeTxGas,
        uint256 baseGas,
        uint256 gasPrice,
        address gasToken,
        address refundReceiver,
        bytes signatures
    ) returns (bool);
    function getMessageHash(bytes message) returns (bytes32);

    /// The EIP-712 type the Safe{Core} SDK signs a message as, and CompatibilityFallbackHandler
    /// checks (`keccak256("SafeMessage(bytes message)")`).
    struct SafeMessage {
        bytes message;
    }
}

#[tokio::test]
async fn real_safe_v1_4_1_owners_prove_a_treasury_as_the_safe_sdk_signs() -> Result<()> {
    let Some(anvil) = Anvil::start_if_available(&[]).await? else {
        return Ok(());
    };
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let anvil = &anvil;
    let result = async {
        let client = Arc::new(EvmClient::new(&anvil.rpc_url)?);
        let contracts = EvmContractSignatures::new(
            database.app_pool.clone(),
            BTreeMap::from([(CHAIN_ID, [Arc::clone(&client), Arc::clone(&client)])]),
        );
        anvil.mine(70)?;
        let reader = topup_adapters::chain::evm::FinalizedReader::new(client.clone());
        topup::scanner::initialize_chain(&database.app_pool, CHAIN_ID, &reader, &reader).await?;
        let fixture =
            Fixture::new(&database.app_pool, Contracts::Anvil(Arc::new(contracts))).await?;
        let safe = SafeDeployment::deploy(anvil, &client).await?;
        let owners: Vec<PrivateKeySigner> = (0..3).map(|_| PrivateKeySigner::random()).collect();
        let [first, second, third] = [&owners[0], &owners[1], &owners[2]];
        let solo = safe.create(anvil, &client, &[first], 1, 1).await?;
        let shared = safe
            .create(anvil, &client, &[first, second, third], 2, 2)
            .await?;
        let approved = safe
            .create(anvil, &client, &[first, second, third], 2, 3)
            .await?;
        let refused = safe.create(anvil, &client, &[first], 1, 4).await?;
        // A Safe whose proxy is not deployed yet: its owner's signature cannot be checked.
        let counterfactual = safe.predict(&client, &[first], 1, 5).await?;

        // An on-chain approval: the owners execute a Safe transaction that delegatecalls
        // SignMessageLib.signMessage(hashSafeMessage(message)), as the Safe docs show.
        let challenge = fixture
            .challenge(&fixture.test_key, CHAIN_ID, approved)
            .await?;
        let approved_message = challenge["message"].as_str().context("message")?.to_owned();
        let expires_in =
            challenge["expires_at"].as_i64().context("expires_at")? - Utc::now().timestamp();
        ensure!(
            expires_in > CHALLENGE_TTL.num_seconds(),
            "a Safe's challenge lasts {expires_in} s"
        );
        safe.approve_message(anvil, &client, approved, &approved_message, &[first, third])
            .await?;
        // 64 blocks bring the Safes and the approval to Anvil's `finalized`.
        anvil.mine(70)?;
        let reader = topup_adapters::chain::evm::FinalizedReader::new(client.clone());
        topup::checkpoint::advance(&database.app_pool, CHAIN_ID, &reader, &reader).await?;
        // Deployed above `finalized`: not yet deployed as far as the proof is concerned.
        let unfinalized = safe.create(anvil, &client, &[first], 1, 6).await?;

        // 1-of-1 and 2-of-3: each owner signs the EIP-712 SafeMessage of the message's EIP-191
        // hash (Protocol Kit `signMessage` with ETH_SIGN_TYPED_DATA_V4), and the signatures are
        // concatenated in ascending owner order (`buildSignatureBytes`).
        for (treasury, signers) in [(solo, vec![first]), (shared, vec![third, second])] {
            let challenge = fixture
                .challenge(&fixture.test_key, CHAIN_ID, treasury)
                .await?;
            let message = challenge["message"].as_str().context("message")?;
            let signature = safe_signature(&client, treasury, message, &signers).await?;
            let (status, body) = fixture
                .submit(&fixture.test_key, CHAIN_ID, message, &signature)
                .await?;
            ensure!(status == StatusCode::OK, "{body}");
            ensure!(body["kind"] == "contract" && body["address"] == format!("{treasury:#x}"));
        }
        let (status, body) = fixture
            .submit(&fixture.test_key, CHAIN_ID, &approved_message, "0x")
            .await?;
        ensure!(
            status == StatusCode::OK && body["kind"] == "contract",
            "{body}"
        );

        let refused_proof = |status: StatusCode, body: &Value, code: &str| {
            ensure!(status == StatusCode::BAD_REQUEST, "{body}");
            ensure!(body["error"]["code"] == code, "{body}");
            Ok(())
        };
        // A signer that is not an owner; one owner of a 2-of-3; an owner's plain personal_sign of
        // the message (not the SafeMessage); and an unapproved message with `0x`.
        let stranger = PrivateKeySigner::random();
        for (treasury, signers, plain) in [
            (refused, vec![&stranger], false),
            (shared, vec![first], false),
            (refused, vec![first], true),
        ] {
            let challenge = fixture
                .challenge(&fixture.test_key, CHAIN_ID, treasury)
                .await?;
            let message = challenge["message"].as_str().context("message")?;
            let signature = if plain {
                sign(first, message).await?
            } else {
                safe_signature(&client, treasury, message, &signers).await?
            };
            let (status, body) = fixture
                .submit(&fixture.test_key, CHAIN_ID, message, &signature)
                .await?;
            refused_proof(status, &body, "treasury_proof_invalid")?;
        }
        let challenge = fixture
            .challenge(&fixture.test_key, CHAIN_ID, refused)
            .await?;
        let message = challenge["message"].as_str().context("message")?;
        let (status, body) = fixture
            .submit(&fixture.test_key, CHAIN_ID, message, "0x")
            .await?;
        refused_proof(status, &body, "treasury_proof_invalid")?;

        // Not deployed at `finalized`, counterfactual or recent: refused, whatever the owner signs.
        for treasury in [counterfactual, unfinalized] {
            let challenge = fixture
                .challenge(&fixture.test_key, CHAIN_ID, treasury)
                .await?;
            let message = challenge["message"].as_str().context("message")?;
            let hash = safe_message_hash(treasury, message);
            let signature = format!(
                "0x{}",
                hex::encode(first.sign_hash(&hash).await?.as_bytes())
            );
            let (status, body) = fixture
                .submit(&fixture.test_key, CHAIN_ID, message, &signature)
                .await?;
            refused_proof(status, &body, "treasury_not_deployed")?;
        }
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// Safe v1.4.1 deployed on Anvil from the vendored sources.
struct SafeDeployment {
    singleton: Address,
    factory: Address,
    handler: Address,
    sign_message_lib: Address,
}

impl SafeDeployment {
    /// Deploys the singleton, proxy factory, CompatibilityFallbackHandler, and SignMessageLib,
    /// and checks that their code is the canonical deployments' code.
    async fn deploy(anvil: &Anvil, client: &EvmClient) -> Result<Self> {
        let mut deployed = Vec::new();
        for (contract, canonical) in CANONICAL_SAFE_CODE {
            let address = forge_create_in_profile(&anvil.rpc_url, "safe", contract, &[])?;
            let code = client.code_at(address).await?;
            ensure!(
                keccak256(strip_solidity_metadata(&code)) == B256::from_str(canonical)?,
                "{contract} differs from the canonical Safe v1.4.1 deployment"
            );
            deployed.push(address);
        }
        let [singleton, handler, factory, sign_message_lib] = deployed[..] else {
            anyhow::bail!("four contracts");
        };
        Ok(Self {
            singleton,
            factory,
            handler,
            sign_message_lib,
        })
    }

    fn creation(
        owners: &[&PrivateKeySigner],
        threshold: u64,
        salt: u64,
        singleton: Address,
        handler: Address,
    ) -> Vec<u8> {
        let initializer = setupCall {
            owners: owners.iter().map(|owner| owner.address()).collect(),
            threshold: U256::from(threshold),
            to: Address::ZERO,
            data: Bytes::new(),
            fallbackHandler: handler,
            paymentToken: Address::ZERO,
            payment: U256::ZERO,
            paymentReceiver: Address::ZERO,
        }
        .abi_encode();
        createProxyWithNonceCall {
            singleton,
            initializer: initializer.into(),
            saltNonce: U256::from(salt),
        }
        .abi_encode()
    }

    /// The Safe proxy `create` would deploy.
    async fn predict(
        &self,
        client: &EvmClient,
        owners: &[&PrivateKeySigner],
        threshold: u64,
        salt: u64,
    ) -> Result<Address> {
        let call = Self::creation(owners, threshold, salt, self.singleton, self.handler);
        let output = client
            .call("createProxyWithNonce", self.factory, call.into(), None)
            .await?;
        Ok(createProxyWithNonceCall::abi_decode_returns(&output)?)
    }

    /// Deploys a Safe proxy with `owners`, `threshold`, and the CompatibilityFallbackHandler, as
    /// the Safe{Wallet} and the Safe{Core} SDK create one.
    async fn create(
        &self,
        anvil: &Anvil,
        client: &EvmClient,
        owners: &[&PrivateKeySigner],
        threshold: u64,
        salt: u64,
    ) -> Result<Address> {
        let safe = self.predict(client, owners, threshold, salt).await?;
        let call = Self::creation(owners, threshold, salt, self.singleton, self.handler);
        send(anvil, self.factory, &call)?;
        ensure!(
            !client.code_at(safe).await?.is_empty(),
            "the Safe was not created"
        );
        Ok(safe)
    }

    /// Approves `message` on chain: a Safe transaction, signed by `signers`, that delegatecalls
    /// `SignMessageLib.signMessage(hashSafeMessage(message))`.
    async fn approve_message(
        &self,
        anvil: &Anvil,
        client: &EvmClient,
        safe: Address,
        message: &str,
        signers: &[&PrivateKeySigner],
    ) -> Result<()> {
        let data: Bytes = signMessageCall {
            data: eip191_hash_message(message.as_bytes()).to_vec().into(),
        }
        .abi_encode()
        .into();
        let hash_call = getTransactionHashCall {
            to: self.sign_message_lib,
            value: U256::ZERO,
            data: data.clone(),
            operation: 1,
            safeTxGas: U256::ZERO,
            baseGas: U256::ZERO,
            gasPrice: U256::ZERO,
            gasToken: Address::ZERO,
            refundReceiver: Address::ZERO,
            nonce: U256::ZERO,
        };
        let output = client
            .call(
                "getTransactionHash",
                safe,
                hash_call.abi_encode().into(),
                None,
            )
            .await?;
        let transaction_hash = getTransactionHashCall::abi_decode_returns(&output)?;
        let signatures = concatenated_signatures(signers, transaction_hash).await?;
        let exec = execTransactionCall {
            to: self.sign_message_lib,
            value: U256::ZERO,
            data,
            operation: 1,
            safeTxGas: U256::ZERO,
            baseGas: U256::ZERO,
            gasPrice: U256::ZERO,
            gasToken: Address::ZERO,
            refundReceiver: Address::ZERO,
            signatures: signatures.into(),
        };
        send(anvil, safe, &exec.abi_encode())
    }
}

/// The EIP-712 hash an owner signs for `message` in the Safe{Core} SDK: `SafeMessage{message:
/// hashSafeMessage(message)}`, where `hashSafeMessage` of a string is its EIP-191 hash, over the
/// domain `{chainId, verifyingContract: safe}` of Safe ≥ 1.3.0.
fn safe_message_hash(safe: Address, message: &str) -> B256 {
    let domain = Eip712Domain::new(None, None, Some(U256::from(CHAIN_ID)), Some(safe), None);
    SafeMessage {
        message: eip191_hash_message(message.as_bytes()).to_vec().into(),
    }
    .eip712_signing_hash(&domain)
}

/// The Safe signature of `message` by `signers`, as `EthSafeMessage.encodedSignatures()` returns
/// it; the hash is first checked against the Safe's own `getMessageHash`.
async fn safe_signature(
    client: &EvmClient,
    safe: Address,
    message: &str,
    signers: &[&PrivateKeySigner],
) -> Result<String> {
    let hash = safe_message_hash(safe, message);
    let call = getMessageHashCall {
        message: eip191_hash_message(message.as_bytes()).to_vec().into(),
    };
    let output = client
        .call("getMessageHash", safe, call.abi_encode().into(), None)
        .await?;
    ensure!(getMessageHashCall::abi_decode_returns(&output)? == hash);
    Ok(format!(
        "0x{}",
        hex::encode(concatenated_signatures(signers, hash).await?)
    ))
}

/// 65-byte ECDSA signatures of `hash` (`v` 27 or 28) in ascending signer order, as Safe's
/// `checkSignatures` requires.
async fn concatenated_signatures(signers: &[&PrivateKeySigner], hash: B256) -> Result<Vec<u8>> {
    let mut ordered = signers.to_vec();
    ordered.sort_by_key(|signer| signer.address());
    let mut bytes = Vec::new();
    for signer in ordered {
        bytes.extend_from_slice(&signer.sign_hash(&hash).await?.as_bytes());
    }
    Ok(bytes)
}

/// Sends raw calldata from the Anvil deployer.
fn send(anvil: &Anvil, to: Address, calldata: &[u8]) -> Result<()> {
    run_checked(
        "cast",
        &[
            "send",
            "--rpc-url",
            &anvil.rpc_url,
            "--private-key",
            ANVIL_PRIVATE_KEY,
            &format!("{to:#x}"),
            &format!("0x{}", hex::encode(calldata)),
        ],
        None,
    )?;
    Ok(())
}

/// Runtime code without the CBOR metadata solc appends to it and to every creation code it embeds
/// (`a2 64 "ipfs" 58 22 <34 bytes> 64 "solc" 43 <3 bytes> 00 33`).
fn strip_solidity_metadata(code: &[u8]) -> Vec<u8> {
    const PREFIX: &[u8] = &[0xa2, 0x64, b'i', b'p', b'f', b's', 0x58, 0x22];
    const LENGTH: usize = 53;
    let mut stripped = Vec::with_capacity(code.len());
    let mut rest = code;
    while let Some(start) = rest
        .windows(PREFIX.len())
        .position(|window| window == PREFIX)
    {
        let end = start + LENGTH;
        if rest.len() < end || rest[end - 2..end] != [0x00, 0x33] {
            stripped.extend_from_slice(&rest[..start + PREFIX.len()]);
            rest = &rest[start + PREFIX.len()..];
            continue;
        }
        stripped.extend_from_slice(&rest[..start]);
        rest = &rest[end..];
    }
    stripped.extend_from_slice(rest);
    stripped
}

async fn sign(signer: &PrivateKeySigner, message: &str) -> Result<String> {
    let signature = signer.sign_message(message.as_bytes()).await?;
    Ok(format!("0x{}", hex::encode(signature.as_bytes())))
}

enum Contracts {
    /// No contract is deployed anywhere.
    None,
    /// Anvil's chain, with its test route.
    Anvil(Arc<dyn ContractSignatures>),
}

struct Fixture {
    app: Router,
    pool: sqlx::PgPool,
    routes: Vec<RouteFile>,
    account: db::Account,
    live_key: String,
    test_key: String,
}

impl Fixture {
    async fn new(pool: &sqlx::PgPool, contracts: Contracts) -> Result<Self> {
        seed::initialize_dual_chain(pool, 1).await?;
        let account = seed::create_account(
            pool,
            &NewAccount {
                webhook_url: "https://merchant.test/webhooks".to_owned(),
                ..NewAccount::named("merchant")
            },
        )
        .await?;
        // Two endpoints, live and test, that subscribe to deposits only: account security events
        // reach them anyway.
        sqlx::query(
            "INSERT INTO webhook_endpoints (id, account_id, livemode, url) \
             VALUES (gen_random_uuid(), $1, false, 'https://merchant.test/test-webhooks')",
        )
        .bind(account.id)
        .execute(pool)
        .await?;
        sqlx::query(
            "UPDATE webhook_endpoints SET enabled_events = ARRAY['deposit.credited'] \
             WHERE account_id = $1",
        )
        .bind(account.id)
        .execute(pool)
        .await?;
        let live_key = seed::create_api_key(pool, account.id, true).await?;
        let test_key = seed::create_api_key(pool, account.id, false).await?;
        let fixture_route = include_str!("fixtures/phala-cloud-pha.yaml");
        let route = |name: &str, chain_id: u64, livemode: bool| -> Result<RouteFile> {
            Ok(serde_saphyr::from_str(
                &fixture_route
                    .replace(
                        "route: phala-cloud-ethereum-pha-usd",
                        &format!("route: {name}"),
                    )
                    .replace("chain_id: 1", &format!("chain_id: {chain_id}"))
                    .replace("livemode: true", &format!("livemode: {livemode}")),
            )?)
        };
        let mut routes = vec![
            serde_saphyr::from_str(fixture_route)?,
            route("phala-cloud-optimism-pha-usd", OTHER_CHAIN, true)?,
            route("phala-cloud-sepolia-pha", SEPOLIA, false)?,
        ];
        let contracts: Arc<dyn ContractSignatures> = match contracts {
            Contracts::None => Arc::new(NoContracts),
            Contracts::Anvil(contracts) => {
                routes.push(route("anvil-pha", CHAIN_ID, false)?);
                contracts
            }
        };
        // Two-decimal tokens at 1 USD, so a quote of 100 cents is 1 token, 100 atomic units.
        for route in &mut routes {
            route.asset.decimals = 2;
            route.asset.quote_amount_decimals = 2;
            route.merchant.min_deposit_atomic =
                topup_core::route::Bounded::at(AtomicAmount::new(U256::from(1_u64)));
            route.merchant.max_deposit_atomic =
                topup_core::route::Bounded::at(AtomicAmount::new(U256::from(1_000_000_u64)));
            route.merchant.min_amount = topup_core::route::Bounded::at(1);
        }
        for route in &routes {
            seed::initialize_dual_chain(pool, route.chain.chain_id).await?;
        }
        let route_set = RouteSet::new(routes.clone()).map_err(anyhow::Error::msg)?;
        for livemode in [false, true] {
            seed::accept_all(pool, account.id, livemode, &route_set).await?;
        }
        Ok(Self {
            app: app(pool, routes.clone(), Arc::new(Clear), contracts)?,
            pool: pool.clone(),
            routes,
            account,
            live_key,
            test_key,
        })
    }

    fn route_set(&self) -> Result<RouteSet> {
        RouteSet::new(self.routes.clone()).map_err(anyhow::Error::msg)
    }

    /// A key of another live account.
    async fn other_account(&self) -> Result<String> {
        let other = seed::create_account(&self.pool, &NewAccount::named("other")).await?;
        Ok(seed::create_api_key(&self.pool, other.id, true).await?)
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        key: &str,
        body: Value,
    ) -> Result<(StatusCode, Value)> {
        let body = if body.is_null() {
            Vec::new()
        } else {
            serde_json::to_vec(&body)?
        };
        let response = self
            .app
            .clone()
            .oneshot(merchant_request(method, path, body, key))
            .await?;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1_048_576).await?;
        Ok((status, serde_json::from_slice(&bytes)?))
    }

    async fn post(&self, key: &str, path: &str, body: Value) -> Result<(StatusCode, Value)> {
        self.request(Method::POST, path, key, body).await
    }

    async fn get(&self, path: &str) -> Result<(StatusCode, Value)> {
        self.request(Method::GET, path, &self.live_key, Value::Null)
            .await
    }

    async fn challenge(&self, key: &str, chain_id: u64, address: Address) -> Result<Value> {
        let (status, body) = self
            .post(
                key,
                "/v1/treasuries/challenge",
                json!({"chain_id": chain_id, "address": format!("{address:#x}")}),
            )
            .await?;
        ensure!(status == StatusCode::OK, "{status}: {body}");
        ensure!(body["object"] == "treasury_challenge");
        Ok(body)
    }

    async fn submit(
        &self,
        key: &str,
        chain_id: u64,
        message: &str,
        signature: &str,
    ) -> Result<(StatusCode, Value)> {
        self.post(
            key,
            "/v1/treasuries",
            json!({"chain_id": chain_id, "message": message, "signature": signature}),
        )
        .await
    }

    /// Proves `signer` as the treasury of `chain_id` in `key`'s mode.
    async fn prove(
        &self,
        key: &str,
        chain_id: u64,
        signer: &PrivateKeySigner,
    ) -> Result<(StatusCode, Value)> {
        let challenge = self.challenge(key, chain_id, signer.address()).await?;
        let message = challenge["message"].as_str().context("message")?;
        let signature = sign(signer, message).await?;
        self.submit(key, chain_id, message, &signature).await
    }

    async fn quote(&self, key: &str, chain_id: u64) -> Result<(StatusCode, Value)> {
        self.post(
            key,
            "/v1/quotes",
            json!({"client_reference_id": "team-1", "amount": 100, "currency": "usd",
                   "chain_id": chain_id, "asset": "pha"}),
        )
        .await
    }

    /// The customer's deposit address, without the `client_secret` each create issues anew.
    async fn deposit_address(&self, key: &str, customer: &str) -> Result<Value> {
        let (status, mut body) = self
            .post(
                key,
                "/v1/deposit_addresses",
                json!({"client_reference_id": customer}),
            )
            .await?;
        ensure!(status == StatusCode::OK, "{status}: {body}");
        body.as_object_mut()
            .and_then(|object| object.remove("client_secret"))
            .context("client_secret")?;
        Ok(body)
    }

    /// Records a confirmed, valued deposit of 1 token to `quote`'s address, as the scanner and
    /// the confirm step would; `number` makes its transaction unique.
    async fn confirmed_deposit(&self, quote: &Value, number: u8) -> Result<uuid::Uuid> {
        let address_id: uuid::Uuid =
            sqlx::query_scalar("SELECT id FROM addresses WHERE address = $1")
                .bind(quote["address"].as_str().context("address")?)
                .fetch_one(&self.pool)
                .await?;
        let route = self.routes.first().context("route")?;
        let deposit = db::NewDeposit {
            chain_id: 1,
            tx_hash: B256::repeat_byte(number),
            log_index: 0,
            receipt_log_index: 0,
            tx_from: Address::ZERO,
            tx_nonce: 0,
            is_final: true,
            block_number: 100,
            block_hash: B256::repeat_byte(number.wrapping_add(1)),
            block_time: Utc::now(),
            address_id,
            route: Some(route.route.clone()),
            route_version: Some(route.version),
            asset_contract: route.asset.contract,
            from_address: Address::repeat_byte(0x11),
            amount_atomic: AtomicAmount::new(U256::from(1_u64)),
            state: topup_core::deposit::DepositState::Confirmed,
            reason: None,
            next_attempt_at: Utc::now(),
        };
        let id = topup_core::identity::deposit_id(1, deposit.tx_hash, 0);
        ensure!(db::insert_deposit(&self.pool, &deposit).await?);
        sqlx::query(
            "UPDATE deposits SET valuation_at = now(), price_scaled = 100000000, \
             price_source = 'spot', credit_minor = 100 WHERE id = $1",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// The screen step of the fixture's Ethereum route, with every payer clear.
    fn screen_step(&self) -> Result<ScreenStep> {
        let route = self.routes.first().context("route")?;
        Ok(ScreenStep::new(
            self.pool.clone(),
            [ScreenRoute::new(route.clone(), Arc::new(ClearPayers))],
        )?)
    }

    /// The account, mode, and object type of each recorded event of `event_type`.    /// The account, mode, and object type of each recorded event of `event_type`.
    async fn events(&self, event_type: &str) -> Result<Vec<(uuid::Uuid, bool, String)>> {
        Ok(sqlx::query_as(
            "SELECT account_id, livemode, object_type FROM events WHERE type = $1 ORDER BY created",
        )
        .bind(event_type)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The `data` of each `event_type` event, oldest first.
    async fn event_data(&self, event_type: &str) -> Result<Vec<Value>> {
        Ok(
            sqlx::query_scalar("SELECT data FROM events WHERE type = $1 ORDER BY created, id")
                .bind(event_type)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// Deliveries of `event_type` events, each to the endpoint of its own mode.
    async fn deliveries(&self, event_type: &str) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM webhook_deliveries AS delivery \
             JOIN events AS event ON event.id = delivery.event_id \
             JOIN webhook_endpoints AS endpoint ON endpoint.id = delivery.endpoint_id \
             WHERE event.type = $1 AND endpoint.livemode = event.livemode",
        )
        .bind(event_type)
        .fetch_one(&self.pool)
        .await?)
    }
}

fn app(
    pool: &sqlx::PgPool,
    routes: Vec<RouteFile>,
    screening: Arc<dyn DestinationScreener>,
    contract_signatures: Arc<dyn ContractSignatures>,
) -> Result<Router> {
    let admin_key = SigningKey::from_bytes(&ADMIN_KEY);
    Ok(topup::api::router(AppState {
        pool: pool.clone(),
        routes: Arc::new(RouteSet::new(routes).map_err(anyhow::Error::msg)?),
        maintenance_keys: Vec::new(),
        admin_key: VerificationKey::from_base64(
            "admin/v1".to_owned(),
            &public_key_base64(&admin_key),
        )
        .map_err(anyhow::Error::msg)?,
        public_origin: PublicOrigin::parse(TEST_ORIGIN)?,
        attestor: Arc::new(DstackAttestor::new()),
        rate_lock_quotes: Arc::new(FixedQuote),
        client_reads: Arc::default(),
        rate_limits: Arc::default(),
        hint_limits: Arc::default(),
        transaction_hints: Arc::default(),
        screening,
        sanctions_rescreen: Arc::default(),
        contract_signatures,
    })
    .0)
}

fn networks(object: &Value) -> Result<&[Value]> {
    Ok(object["networks"]
        .as_array()
        .context("networks")?
        .as_slice())
}

fn chain_ids(object: &Value) -> Vec<u64> {
    object["networks"]
        .as_array()
        .map(|networks| {
            networks
                .iter()
                .filter_map(|network| network["chain_id"].as_u64())
                .collect()
        })
        .unwrap_or_default()
}

/// A fixed price of 1 USD per token.
struct FixedQuote;

#[async_trait]
impl QuoteProvider for FixedQuote {
    async fn quote(
        &self,
        _route: &RouteFile,
        _deadline: tokio::time::Instant,
    ) -> Result<ValidatedQuote, Value> {
        Ok(ValidatedQuote {
            price: ScaledPrice::new(100_000_000, PRICE_SCALE).map_err(|_| json!("price"))?,
            evidence: json!({"mode": "spot"}),
        })
    }
}

/// Screening that clears every address.
struct Clear;

#[async_trait]
impl DestinationScreener for Clear {
    async fn screen(&self, _route: &RouteFile, _address: Address) -> DestinationScreening {
        DestinationScreening::Clear
    }
}

/// Screening that lists every address.
struct Sanctioned;

#[async_trait]
impl DestinationScreener for Sanctioned {
    async fn screen(&self, _route: &RouteFile, _address: Address) -> DestinationScreening {
        DestinationScreening::Sanctioned
    }
}

/// Screening that lists the given addresses.
struct Listed(Vec<Address>);

#[async_trait]
impl DestinationScreener for Listed {
    async fn screen(&self, _route: &RouteFile, address: Address) -> DestinationScreening {
        if self.0.contains(&address) {
            DestinationScreening::Sanctioned
        } else {
            DestinationScreening::Clear
        }
    }
}

/// Screening that cannot answer.
struct Unavailable;

#[async_trait]
impl DestinationScreener for Unavailable {
    async fn screen(&self, _route: &RouteFile, _address: Address) -> DestinationScreening {
        DestinationScreening::Unavailable
    }
}

/// A chain where no contract is deployed.
/// A sanctions source that names no payer.
struct ClearPayers;

#[async_trait]
impl SanctionsSource for ClearPayers {
    async fn sanctions(&self, _address: Address, _block_number: u64) -> SanctionsResult {
        topup_core::screening::SanctionsResult::new(SanctionsVerdict::Clear)
    }
}

struct NoContracts;

#[async_trait]
impl ContractSignatures for NoContracts {
    async fn has_code(&self, _: u64, _: Address) -> bool {
        false
    }

    async fn verify(&self, _: u64, _: Address, _: B256, _: Bytes) -> ContractAnswer {
        ContractAnswer::NotDeployed
    }
}

#[tokio::test]
async fn activated_list_rescreens_current_and_pending_before_timelock() -> Result<()> {
    with_database(|db|Box::pin(async move {
        let fixture=Fixture::new(&db.app_pool,Contracts::None).await?;
        let first=PrivateKeySigner::random();
        let next=PrivateKeySigner::random();
        fixture.prove(&fixture.live_key,1,&first).await?;
        let (_,pending)=fixture.prove(&fixture.live_key,1,&next).await?;
        ensure!(pending["status"]=="pending");
        let snapshot=uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO sanctions_list_snapshots(id,source,publish_date,sha256,record_count,address_count,fetched_at,activated_at,verified_at) VALUES($1,'ofac_sdn','2026-10-06',$2,1,2,now(),now(),now())").bind(snapshot).bind(vec![1_u8;32]).execute(&db.app_pool).await?;
        for address in [first.address(),next.address()] {
            sqlx::query("INSERT INTO sanctions_list_addresses(snapshot_id,sdn_uid,id_type,raw_value,evm_address) VALUES($1,1,'Digital Currency Address - USDT',$2,$3)")
                .bind(snapshot).bind(format!("{address:#x}")).bind(address.as_slice()).execute(&db.app_pool).await?;
        }
        let routes=fixture.route_set()?;
        let screener=topup::sanctions::ListScreener::new(db.app_pool.clone(),std::time::Duration::from_secs(86400));
        topup::sanctions::rescreen(&db.app_pool,&routes,&screener).await?;
        let (_,canceled)=fixture.get(&format!("/v1/treasuries/{}",pending["id"].as_str().unwrap())).await?;
        ensure!(canceled["status"]=="canceled" && canceled["cancellation_reason"]=="sanctioned");
        let scopes:Vec<String>=sqlx::query_scalar("SELECT paused_scopes FROM accounts WHERE id=$1").bind(fixture.account.id).fetch_one(&db.app_pool).await?;
        ensure!(scopes.contains(&"quotes".to_owned()) && scopes.contains(&"settlement".to_owned()));
        ensure!(fixture.events("treasury.canceled").await?.len()==1);
        Ok(())
    })).await
}
