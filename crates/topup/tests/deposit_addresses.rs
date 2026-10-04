//! Deposit addresses on PostgreSQL (docs/design/multi-tenant.md "Deposit addresses"): one
//! address per customer and mode for every supported token on every supported chain; creation is
//! idempotent; rotation retires and issues on every chain; a chain whose treasury differs has its
//! own address; the address is the one the merchant recomputes; caps, the rotation limit, and
//! tenancy hold; a deposit to one names it.

mod support;

use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use anyhow::{Context, Result, ensure};
use axum::Router;
use axum::body::to_bytes;
use axum::http::{Method, StatusCode};
use chrono::Utc;
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use topup::api::{AppState, PublicOrigin, VerificationKey};
use topup::db::{self, NewDeposit};
use topup::deposit_addresses::MAX_ROTATIONS_PER_HOUR;
use topup::payment_config::{AssetChoice, ChainChoice, Document};
use topup_adapters::attestation::DstackAttestor;
use topup_core::address::{deposit_address_salt, forwarder_address};
use topup_core::deposit::{DepositState, RejectReason};
use topup_core::identity::deposit_id;
use topup_core::money::AtomicAmount;
use topup_core::route::{ChainFamily, Confirmations, RouteFile};
use tower::ServiceExt;
use uuid::Uuid;

use support::seed::{self, NewAccount};
use support::{TEST_ORIGIN, merchant_request, public_key_base64, with_database};

const TEST_CHAIN: u64 = 11_155_111;
/// A second live chain (OP Mainnet) beside Ethereum's chain 1.
const OTHER_CHAIN: u64 = 10;
const USDC: &str = "0x00000000000000000000000000000000000000c1";

#[tokio::test]
async fn one_address_for_every_asset_and_chain_is_idempotent_per_customer_and_mode() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            let fixture = Fixture::new(pool).await?;
            let first = fixture.create(&fixture.live_key, "team-42").await?;
            ensure!(first["object"] == "deposit_address" && first["status"] == "active");
            ensure!(first["livemode"] == true && first["client_reference_id"] == "team-42");
            ensure!(first["version"] == 1 && first["retired_at"].is_null());
            let id = first["id"].as_str().context("id")?;
            ensure!(id.starts_with("da_") && id.len() == 35);
            // The same address on both live chains, whose treasury is the same.
            let address = first["address"].as_str().context("shared address")?;
            ensure!(chain_ids(&first) == vec![1, OTHER_CHAIN], "{first}");
            for network in networks(&first)? {
                ensure!(network["address"] == address);
                ensure!(network["treasury"] == format!("{:#x}", fixture.account_treasury()));
            }
            // Chain 1 takes both of its tokens, each with an EIP-681 URI without an amount.
            let ethereum = &networks(&first)?[0];
            let assets = ethereum["assets"].as_array().context("assets")?;
            ensure!(assets.len() == 2, "{ethereum}");
            let pha = assets
                .iter()
                .find(|asset| asset["asset"] == "pha")
                .context("pha")?;
            ensure!(pha["contract"] == format!("{:#x}", fixture.live_route.asset.contract));
            ensure!(pha["decimals"] == 18);
            ensure!(
                pha["payment_uri"]
                    == format!(
                        "ethereum:{:#x}@1/transfer?address={address}",
                        fixture.live_route.asset.contract
                    )
            );
            let usdc = assets
                .iter()
                .find(|asset| asset["asset"] == "usdc")
                .context("usdc")?;
            ensure!(usdc["payment_uri"] == format!("ethereum:{USDC}@1/transfer?address={address}"));
            ensure!(networks(&first)?[1]["assets"].as_array().map(Vec::len) == Some(1));

            // The same request returns the same address, without an idempotency key.
            let again = fixture.create(&fixture.live_key, "team-42").await?;
            ensure!(again == first, "{again} != {first}");
            // Another customer, and the same customer in test mode, get their own.
            let other = fixture.create(&fixture.live_key, "team-43").await?;
            ensure!(other["address"] != first["address"]);
            let test = fixture.create(&fixture.test_key, "team-42").await?;
            ensure!(test["livemode"] == false && test["address"] != first["address"]);
            // Test mode lists only the test chain.
            ensure!(chain_ids(&test) == vec![TEST_CHAIN], "{test}");
            // The request names no chain or asset.
            let (status, body) = fixture
                .request(
                    Method::POST,
                    "/v1/deposit_addresses",
                    &fixture.live_key,
                    json!({"client_reference_id": "team-42", "chain_id": 1, "asset": "pha"}),
                )
                .await?;
            ensure!(status == StatusCode::BAD_REQUEST, "{body}");
            let (status, body) = fixture
                .request(
                    Method::POST,
                    "/v1/deposit_addresses",
                    &fixture.live_key,
                    json!({"client_reference_id": ""}),
                )
                .await?;
            ensure!(status == StatusCode::BAD_REQUEST, "{body}");
            ensure!(body["error"]["param"] == "client_reference_id");

            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM deposit_addresses")
                .fetch_one(pool)
                .await?;
            ensure!(count == 3);
            // One forwarder row per address and chain, in that chain's issued-address set.
            let owners: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM addresses \
                 WHERE deposit_address_id IS NOT NULL AND quote_id IS NULL",
            )
            .fetch_one(pool)
            .await?;
            ensure!(owners == 5);
            for chain_id in [1, OTHER_CHAIN] {
                let watched = db::list_scan_addresses(pool, chain_id).await?;
                ensure!(
                    watched
                        .iter()
                        .any(|watched| format!("{:#x}", watched.address) == address)
                );
            }
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn the_address_is_the_one_the_merchant_recomputes() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let fixture = Fixture::new(&database.app_pool).await?;
            let created = fixture.create(&fixture.live_key, "客户 42").await?;
            let rotated = fixture.rotate(&fixture.live_key, &created).await?;
            for (object, version) in [(&created, 1), (&rotated, 2)] {
                ensure!(object["version"] == version);
                let salt =
                    deposit_address_salt(&fixture.account.public_id, true, "客户 42", version);
                ensure!(object["salt"] == format!("{salt:#x}"));
                let contracts = &fixture.live_route.chain.contracts;
                let address = forwarder_address(
                    contracts.forwarder_factory,
                    contracts.implementation,
                    fixture.account_treasury(),
                    salt,
                );
                ensure!(object["address"] == format!("{address:#x}"));
                for network in networks(object)? {
                    ensure!(network["address"] == format!("{address:#x}"));
                }
            }
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn rotate_retires_the_address_and_issues_the_next_version_on_every_chain() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            let fixture = Fixture::new(pool).await?;
            let first = fixture.create(&fixture.live_key, "team-42").await?;
            let first_id = first["id"].as_str().context("id")?;
            let second = fixture.rotate(&fixture.live_key, &first).await?;
            ensure!(second["status"] == "active" && second["version"] == 2);
            ensure!(second["id"] != first["id"] && second["address"] != first["address"]);
            ensure!(chain_ids(&second) == chain_ids(&first));
            for (old, new) in networks(&first)?.iter().zip(networks(&second)?) {
                ensure!(old["address"] != new["address"]);
            }

            let (status, retired) = fixture
                .request(
                    Method::GET,
                    &format!("/v1/deposit_addresses/{first_id}"),
                    &fixture.live_key,
                    Value::Null,
                )
                .await?;
            ensure!(status == StatusCode::OK);
            ensure!(retired["status"] == "retired" && retired["retired_at"].is_i64());
            ensure!(retired["networks"] == first["networks"]);
            // Retired addresses stay in every chain's issued-address set, which the scanner reads.
            for chain_id in [1, OTHER_CHAIN] {
                let watched = db::list_scan_addresses(pool, chain_id).await?;
                for object in [&first, &second] {
                    ensure!(watched.iter().any(|watched| {
                        Some(format!("{:#x}", watched.address).as_str())
                            == object["address"].as_str()
                    }));
                }
            }
            // Creation now returns the new address.
            ensure!(fixture.create(&fixture.live_key, "team-42").await? == second);
            // A retired address cannot be rotated again.
            let (status, body) = fixture
                .request(
                    Method::POST,
                    &format!("/v1/deposit_addresses/{first_id}/rotate"),
                    &fixture.live_key,
                    Value::Null,
                )
                .await?;
            ensure!(status == StatusCode::BAD_REQUEST, "{body}");
            ensure!(body["error"]["code"] == "deposit_address_retired");
            let audited: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM audit WHERE action = 'deposit_address.rotate' \
                 AND subject = $1",
            )
            .bind(format!("deposit_address:{first_id}"))
            .fetch_one(pool)
            .await?;
            ensure!(audited == 1);

            // Lists filter by customer and status, newest first.
            fixture.create(&fixture.live_key, "team-43").await?;
            let (_, list) = fixture
                .request(
                    Method::GET,
                    "/v1/deposit_addresses?client_reference_id=team-42",
                    &fixture.live_key,
                    Value::Null,
                )
                .await?;
            ensure!(list["object"] == "list" && list["has_more"] == false);
            let data = list["data"].as_array().context("data")?;
            ensure!(data.len() == 2 && data[0]["id"] == second["id"], "{list}");
            ensure!(data[1]["networks"] == first["networks"], "{list}");
            let (_, list) = fixture
                .request(
                    Method::GET,
                    "/v1/deposit_addresses?status=active&limit=1",
                    &fixture.live_key,
                    Value::Null,
                )
                .await?;
            ensure!(
                list["has_more"] == true && list["data"][0]["client_reference_id"] == "team-43"
            );
            let after = list["data"][0]["id"].as_str().context("id")?;
            let (_, list) = fixture
                .request(
                    Method::GET,
                    &format!("/v1/deposit_addresses?status=active&starting_after={after}"),
                    &fixture.live_key,
                    Value::Null,
                )
                .await?;
            ensure!(list["has_more"] == false && list["data"][0]["id"] == second["id"]);
            let (status, _) = fixture
                .request(
                    Method::GET,
                    "/v1/deposit_addresses?status=open",
                    &fixture.live_key,
                    Value::Null,
                )
                .await?;
            ensure!(status == StatusCode::BAD_REQUEST);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn other_accounts_and_modes_see_no_deposit_address() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            let fixture = Fixture::new(pool).await?;
            let created = fixture.create(&fixture.live_key, "team-42").await?;
            let id = created["id"].as_str().context("id")?;
            let stranger = seed::create_account(pool, &NewAccount::named("stranger")).await?;
            let stranger_key = seed::create_api_key(pool, stranger.id, true).await?;
            seed::set_treasury(pool, stranger.id, true, 1, seed::FIXTURE_TREASURY).await?;
            seed::accept_routes(pool, stranger.id, true, &[&fixture.live_route]).await?;
            for key in [&stranger_key, &fixture.test_key] {
                for (method, path) in [
                    (Method::GET, format!("/v1/deposit_addresses/{id}")),
                    (Method::POST, format!("/v1/deposit_addresses/{id}/rotate")),
                ] {
                    let (status, body) = fixture.request(method, &path, key, Value::Null).await?;
                    ensure!(status == StatusCode::NOT_FOUND, "{path}: {body}");
                }
                let (_, list) = fixture
                    .request(Method::GET, "/v1/deposit_addresses", key, Value::Null)
                    .await?;
                ensure!(list["data"] == json!([]), "{list}");
                let (status, _) = fixture
                    .request(
                        Method::GET,
                        &format!("/v1/deposit_addresses?starting_after={id}"),
                        key,
                        Value::Null,
                    )
                    .await?;
                ensure!(status == StatusCode::BAD_REQUEST);
            }
            // The same client_reference_id at another account is another customer.
            let theirs = fixture.create(&stranger_key, "team-42").await?;
            ensure!(theirs["address"] != created["address"]);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn caps_rotation_limit_and_pauses_bound_issuance() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            let fixture = Fixture::new(pool).await?;
            sqlx::query(
                "INSERT INTO account_limits (account_id, livemode, max_open_quotes, \
                 max_open_minor_account, max_open_minor_customer, max_active_deposit_addresses) \
                 VALUES ($1, true, 1000, 0, 0, 2)",
            )
            .bind(fixture.account.id)
            .execute(pool)
            .await?;
            let first = fixture.create(&fixture.live_key, "team-1").await?;
            fixture.create(&fixture.live_key, "team-2").await?;
            let (status, body) = fixture
                .request(
                    Method::POST,
                    "/v1/deposit_addresses",
                    &fixture.live_key,
                    json!({"client_reference_id": "team-3"}),
                )
                .await?;
            ensure!(status == StatusCode::BAD_REQUEST, "{body}");
            ensure!(body["error"]["code"] == "deposit_address_cap_exceeded");
            // At the cap, existing addresses are still returned and rotated: rotation keeps the
            // count of active addresses.
            ensure!(fixture.create(&fixture.live_key, "team-1").await? == first);
            // Test mode has its own default cap.
            fixture.create(&fixture.test_key, "team-3").await?;

            let mut current = first;
            for _ in 0..MAX_ROTATIONS_PER_HOUR {
                current = fixture.rotate(&fixture.live_key, &current).await?;
            }
            let id = current["id"].as_str().context("id")?;
            let (status, body) = fixture
                .request(
                    Method::POST,
                    &format!("/v1/deposit_addresses/{id}/rotate"),
                    &fixture.live_key,
                    Value::Null,
                )
                .await?;
            ensure!(status == StatusCode::TOO_MANY_REQUESTS, "{body}");
            ensure!(body["error"]["code"] == "customer_rate_limit");
            // Another customer's rotations are not limited by this one's.
            let second = fixture.create(&fixture.live_key, "team-2").await?;
            fixture.rotate(&fixture.live_key, &second).await?;

            // A `quotes` pause stops new addresses; reads keep working.
            seed::set_account_paused_scopes(pool, fixture.account.id, &["quotes".to_owned()])
                .await?;
            let (status, body) = fixture
                .request(
                    Method::POST,
                    "/v1/deposit_addresses",
                    &fixture.live_key,
                    json!({"client_reference_id": "team-1"}),
                )
                .await?;
            ensure!(status == StatusCode::BAD_REQUEST && body["error"]["code"] == "paused");
            let (status, _) = fixture
                .request(
                    Method::GET,
                    &format!("/v1/deposit_addresses/{id}"),
                    &fixture.live_key,
                    Value::Null,
                )
                .await?;
            ensure!(status == StatusCode::OK);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_frozen_chain_gets_no_new_network() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            let fixture = Fixture::new(pool).await?;
            let existing = fixture.create(&fixture.live_key, "team-1").await?;
            freeze(pool, OTHER_CHAIN).await?;
            // An existing address keeps its network on the frozen chain.
            ensure!(fixture.create(&fixture.live_key, "team-1").await? == existing);
            // A new one, or a rotation, is issued on the other chains only.
            let new = fixture.create(&fixture.live_key, "team-2").await?;
            ensure!(chain_ids(&new) == vec![1], "{new}");
            let rotated = fixture.rotate(&fixture.live_key, &existing).await?;
            ensure!(chain_ids(&rotated) == vec![1], "{rotated}");
            // With every chain frozen, nothing new is issued.
            freeze(pool, 1).await?;
            let (status, body) = fixture
                .request(
                    Method::POST,
                    "/v1/deposit_addresses",
                    &fixture.live_key,
                    json!({"client_reference_id": "team-3"}),
                )
                .await?;
            ensure!(status == StatusCode::BAD_REQUEST, "{body}");
            ensure!(body["error"]["code"] == "chain_frozen");
            ensure!(fixture.create(&fixture.live_key, "team-2").await? == new);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn metadata_is_set_merged_carried_by_rotation_and_scoped() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            let fixture = Fixture::new(pool).await?;
            let create = |metadata: Value| {
                fixture.request(
                    Method::POST,
                    "/v1/deposit_addresses",
                    &fixture.live_key,
                    json!({"client_reference_id": "team-42", "metadata": metadata}),
                )
            };
            let (status, first) = create(json!({"team": "42", "plan": "pro"})).await?;
            ensure!(status == StatusCode::OK, "{first}");
            ensure!(first["metadata"] == json!({"team": "42", "plan": "pro"}));
            // Creation returns the active address with the request's metadata merged in.
            let (_, again) = create(json!({"plan": "", "region": "eu"})).await?;
            ensure!(again["id"] == first["id"]);
            ensure!(
                again["metadata"] == json!({"team": "42", "region": "eu"}),
                "{again}"
            );
            let (status, body) = create(json!({"bad[key]": "x"})).await?;
            ensure!(status == StatusCode::BAD_REQUEST, "{body}");
            ensure!(body["error"]["param"] == "metadata[bad[key]]");

            let id = first["id"].as_str().context("id")?;
            let (status, updated) = fixture
                .request(
                    Method::POST,
                    &format!("/v1/deposit_addresses/{id}"),
                    &fixture.live_key,
                    json!({"metadata": {"region": "", "tier": "1"}}),
                )
                .await?;
            ensure!(status == StatusCode::OK, "{updated}");
            ensure!(updated["metadata"] == json!({"team": "42", "tier": "1"}));
            // Rotation carries the metadata to the next version.
            let rotated = fixture.rotate(&fixture.live_key, &updated).await?;
            ensure!(rotated["metadata"] == updated["metadata"]);
            // A retired address is still updatable; `""` unsets every key.
            let (status, cleared) = fixture
                .request(
                    Method::POST,
                    &format!("/v1/deposit_addresses/{id}"),
                    &fixture.live_key,
                    json!({"metadata": ""}),
                )
                .await?;
            ensure!(
                status == StatusCode::OK && cleared["metadata"] == json!({}),
                "{cleared}"
            );
            ensure!(cleared["status"] == "retired");
            // Another mode or account cannot update it.
            let (status, _) = fixture
                .request(
                    Method::POST,
                    &format!("/v1/deposit_addresses/{id}"),
                    &fixture.test_key,
                    json!({"metadata": {"x": "y"}}),
                )
                .await?;
            ensure!(status == StatusCode::NOT_FOUND);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_changed_treasury_on_one_chain_changes_only_that_chains_address() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            let fixture = Fixture::new(pool).await?;
            let old = fixture.create(&fixture.live_key, "team-42").await?;
            let shared = old["address"]
                .as_str()
                .context("shared address")?
                .to_owned();
            // A treasury change that applied without replacing the networks (as if its
            // replacement had been missed) is repaired by the next creation.
            let new_treasury = Address::repeat_byte(0x7e);
            seed::set_treasury(pool, fixture.account.id, true, OTHER_CHAIN, new_treasury).await?;
            let new = fixture.create(&fixture.live_key, "team-42").await?;
            // The same deposit address and version; only chain 10's network changed.
            ensure!(new["id"] == old["id"] && new["version"] == 1);
            ensure!(new["address"].is_null(), "{new}");
            let [ethereum, optimism] = networks(&new)? else {
                anyhow::bail!("two networks: {new}");
            };
            ensure!(ethereum["address"] == shared.as_str());
            ensure!(optimism["address"] != shared.as_str());
            ensure!(optimism["treasury"] == format!("{new_treasury:#x}"));
            let salt = deposit_address_salt(&fixture.account.public_id, true, "team-42", 1);
            let contracts = &fixture.other_route.chain.contracts;
            let expected = forwarder_address(
                contracts.forwarder_factory,
                contracts.implementation,
                new_treasury,
                salt,
            );
            ensure!(optimism["address"] == format!("{expected:#x}"));
            // The old chain-10 forwarder is superseded, still watched, and still pays the old
            // treasury.
            let superseded: (String, bool) = sqlx::query_as(
                "SELECT treasury, superseded_at IS NOT NULL FROM addresses \
                 WHERE chain_id = $1 AND address = $2",
            )
            .bind(i64::try_from(OTHER_CHAIN)?)
            .bind(&shared)
            .fetch_one(pool)
            .await?;
            ensure!(superseded == (format!("{:#x}", fixture.account_treasury()), true));
            let watched = db::list_scan_addresses(pool, OTHER_CHAIN).await?;
            ensure!(watched.len() == 2, "{watched:?}");
            // Changing the treasury back makes the first forwarder current again.
            seed::set_treasury(
                pool,
                fixture.account.id,
                true,
                OTHER_CHAIN,
                fixture.account_treasury(),
            )
            .await?;
            let back = fixture.create(&fixture.live_key, "team-42").await?;
            ensure!(back == old, "{back} != {old}");
            Ok(())
        })
    })
    .await
}

/// The public view states each network's typical credit time at the confirmation its payments
/// wait for, as `GET /v1/config` does: two blocks on Ethereum and three 2-second blocks on Base,
/// then finality on Base alone once the account's policy requires it there.
#[tokio::test]
async fn the_public_view_states_each_networks_credit_time() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            const BASE: u64 = 8_453;
            let pool = &database.app_pool;
            let fixture = Fixture::new(pool).await?;
            seed::set_treasury(pool, fixture.account.id, true, BASE, seed::FIXTURE_TREASURY)
                .await?;
            let mut ethereum = fixture.live_route.clone();
            ethereum.chain.confirmations = Confirmations::Depth(2);
            let mut base = fixture.other_route.clone();
            base.route = "phala-cloud-base-pha-usd".to_owned();
            base.chain.chain_id = BASE;
            base.pricing.sequencer_uptime = Some(topup_core::price::Sequencer {feed:"BASE_SEQUENCER_UPTIME".into(),grace_s:3600,rpc_group:"a".into(),rpc_group_b:"b".into()});
            base.chain.confirmations = ChainFamily::OpStack.default_confirmations();
            let fixture = fixture.with_routes(vec![ethereum, base, fixture.test_route.clone()])?;
            let (object, secret) = fixture
                .create_with_secret(&fixture.live_key, "team-42")
                .await?;
            let id = object["id"].as_str().context("id")?;
            let credit_times = || async {
                let (status, view) = fixture.client_read(id, &secret).await?;
                ensure!(status == StatusCode::OK, "{view}");
                networks(&view)?
                    .iter()
                    .map(|network| {
                        Ok((
                            network["chain_id"].as_u64().context("chain_id")?,
                            network["typical_credit_seconds"]
                                .as_u64()
                                .context("typical_credit_seconds")?,
                        ))
                    })
                    .collect::<Result<Vec<_>>>()
            };
            ensure!(credit_times().await? == vec![(1, 30), (BASE, 7)]);

            let (status, updated) = fixture
                .request(
                    Method::POST,
                    "/v1/payment_settings",
                    &fixture.live_key,
                    json!({"chains": [
                        {"chain_id": 1, "assets": [{"asset": "pha"}]},
                        {"chain_id": BASE, "confirmations": "finalized", "assets": [{"asset": "pha"}]},
                    ]}),
                )
                .await?;
            ensure!(status == StatusCode::OK, "{updated}");
            ensure!(credit_times().await? == vec![(1, 30), (BASE, 900)]);
            // The merchant's view, which has `GET /v1/config`, does not repeat it.
            let (status, merchant) = fixture
                .request(
                    Method::GET,
                    &format!("/v1/deposit_addresses/{id}"),
                    &fixture.live_key,
                    Value::Null,
                )
                .await?;
            ensure!(status == StatusCode::OK, "{merchant}");
            ensure!(networks(&merchant)?.iter().all(|network| network.get("typical_credit_seconds").is_none()));
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_chain_added_later_gets_the_same_address_on_the_next_creation() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            let fixture = Fixture::new(pool).await?;
            let ethereum_only = fixture
                .with_routes(vec![fixture.live_route.clone(), fixture.test_route.clone()])?;
            let before = ethereum_only
                .create(&ethereum_only.live_key, "team-42")
                .await?;
            ensure!(chain_ids(&before) == vec![1], "{before}");
            let after = fixture.create(&fixture.live_key, "team-42").await?;
            ensure!(after["id"] == before["id"]);
            ensure!(chain_ids(&after) == vec![1, OTHER_CHAIN], "{after}");
            ensure!(after["address"] == before["address"]);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn deposits_to_active_and_retired_addresses_on_any_chain_name_the_deposit_address()
-> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            let fixture = Fixture::new(pool).await?;
            let first = fixture.create(&fixture.live_key, "team-42").await?;
            let second = fixture.rotate(&fixture.live_key, &first).await?;
            for (number, object, chain_id) in [(1_u8, &first, 1), (2, &second, OTHER_CHAIN)] {
                // Each deposit starts with a copy of its address's metadata.
                let id = object["id"].as_str().context("id")?;
                let (status, _) = fixture
                    .request(
                        Method::POST,
                        &format!("/v1/deposit_addresses/{id}"),
                        &fixture.live_key,
                        json!({"metadata": {"version": number.to_string()}}),
                    )
                    .await?;
                ensure!(status == StatusCode::OK);
                let da = topup::ids::parse(
                    topup::ids::DEPOSIT_ADDRESS,
                    object["id"].as_str().context("id")?,
                )
                .context("da id")?;
                let address_id: Uuid = sqlx::query_scalar(
                    "SELECT id FROM addresses WHERE deposit_address_id = $1 AND chain_id = $2",
                )
                .bind(da)
                .bind(i64::try_from(chain_id)?)
                .fetch_one(pool)
                .await?;
                let tx_hash = B256::repeat_byte(number);
                ensure!(
                    db::insert_deposit(
                        pool,
                        &NewDeposit {
                            chain_id,
                            tx_hash,
                            receipt_log_index: 0,
                            log_index: 0,
                            block_number: 10,
                            block_hash: B256::repeat_byte(0xbb),
                            block_time: Utc::now(),
                            address_id,
                            route: None,
                            route_version: None,
                            asset_contract: Address::repeat_byte(0x73),
                            from_address: Address::repeat_byte(0x74),
                            amount_atomic: AtomicAmount::new(U256::from(5_u64)),
                            state: DepositState::Rejected,
                            reason: Some(RejectReason::UnsupportedAsset),
                            next_attempt_at: Utc::now(),
                            tx_from: Address::repeat_byte(0x74),
                            tx_nonce: 0,
                            is_final: false,
                        },
                    )
                    .await?
                );
                let deposit =
                    topup::ids::format(topup::ids::DEPOSIT, deposit_id(chain_id, tx_hash, 0));
                let (status, body) = fixture
                    .request(
                        Method::GET,
                        &format!("/v1/deposits/{deposit}"),
                        &fixture.live_key,
                        Value::Null,
                    )
                    .await?;
                ensure!(status == StatusCode::OK, "{body}");
                ensure!(body["deposit_address"] == object["id"] && body["quote"].is_null());
                ensure!(body["chain_id"] == chain_id && body["address"] == object["address"]);
                ensure!(
                    body["metadata"] == json!({"version": number.to_string()}),
                    "{body}"
                );
                // The copy is independent of the address afterwards.
                let (status, _) = fixture
                    .request(
                        Method::POST,
                        &format!("/v1/deposit_addresses/{id}"),
                        &fixture.live_key,
                        json!({"metadata": ""}),
                    )
                    .await?;
                ensure!(status == StatusCode::OK);
                let (_, body) = fixture
                    .request(
                        Method::GET,
                        &format!("/v1/deposits/{deposit}"),
                        &fixture.live_key,
                        Value::Null,
                    )
                    .await?;
                ensure!(body["metadata"] == json!({"version": number.to_string()}));
                ensure!(body["client_reference_id"] == "team-42");
                let (_, list) = fixture
                    .request(
                        Method::GET,
                        &format!(
                            "/v1/deposits?deposit_address={}",
                            object["id"].as_str().context("id")?
                        ),
                        &fixture.live_key,
                        Value::Null,
                    )
                    .await?;
                let data = list["data"].as_array().context("data")?;
                ensure!(data.len() == 1 && data[0]["id"] == deposit, "{list}");
            }
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_payment_is_seen_within_a_block_and_readable_by_client_secret() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            let fixture = Fixture::new(pool).await?;
            let (object, secret) = fixture
                .create_with_secret(&fixture.live_key, "team-42")
                .await?;
            let id = object["id"].as_str().context("id")?;
            ensure!(object["payments"] == json!([]), "{object}");
            // A second create issues another secret; both read the address.
            let (_, second) = fixture
                .create_with_secret(&fixture.live_key, "team-42")
                .await?;
            ensure!(second != secret);
            for secret in [&secret, &second] {
                let (status, view) = fixture.client_read(id, secret).await?;
                ensure!(status == StatusCode::OK, "{view}");
                ensure!(view["payments"] == json!([]) && view["livemode"] == true);
            }

            // The head scan sees a transfer one block deep, before the route's confirmation.
            let address_id: Uuid = sqlx::query_scalar(
                "SELECT id FROM addresses WHERE deposit_address_id = $1 AND chain_id = 1",
            )
            .bind(topup::ids::parse(topup::ids::DEPOSIT_ADDRESS, id).context("da_ id")?)
            .fetch_one(pool)
            .await?;
            let tx_hash = B256::repeat_byte(0x5e);
            db::commit_head_scan(
                pool,
                1,
                100,
                100,
                &[db::NewPendingTransfer {
                    chain_id: 1,
                    tx_hash,
                    receipt_log_index: 0,
                    log_index: 3,
                    block_number: 100,
                    block_hash: B256::repeat_byte(0xb1),
                    block_time: Utc::now(),
                    address_id,
                    asset_contract: fixture.live_route.asset.contract,
                    from_address: Address::repeat_byte(0x74),
                    amount_atomic: AtomicAmount::new(U256::from(7_u64)),
                }],
            )
            .await?;
            let (status, merchant) = fixture
                .request(
                    Method::GET,
                    &format!("/v1/deposit_addresses/{id}"),
                    &fixture.live_key,
                    Value::Null,
                )
                .await?;
            ensure!(status == StatusCode::OK, "{merchant}");
            let deposit = topup::ids::format(topup::ids::DEPOSIT, deposit_id(1, tx_hash, 0));
            ensure!(
                merchant["payments"]
                    == json!([{
                        "status": "seen", "chain_id": 1, "asset": "pha", "amount_atomic": "7",
                        "tx_hash": format!("{tx_hash:#x}"), "confirmations": 1,
                        "estimated_final_at": merchant["payments"][0]["estimated_final_at"],
                        "matches_quote": null, "deposit": deposit,
                    }]),
                "{merchant}"
            );
            ensure!(merchant["payments"][0]["estimated_final_at"].is_i64());
            let (_, view) = fixture.client_read(id, &secret).await?;
            let payments = view["payments"].as_array().context("payments")?;
            ensure!(payments.len() == 1, "{view}");
            ensure!(payments[0]["status"] == "seen" && payments[0]["confirmations"] == 1);
            ensure!(payments[0]["asset"] == "pha" && payments[0]["decimals"] == 18);
            ensure!(payments[0]["amount_atomic"] == "7");
            // The public view carries no merchant fields.
            for field in ["client_reference_id", "metadata", "salt", "client_secret"] {
                ensure!(view.get(field).is_none(), "{field} in {view}");
            }
            ensure!(view["networks"][0].get("treasury").is_none());

            // Recorded as a deposit, it is `recorded` and the page shows the deposit's progress.
            ensure!(
                db::insert_deposit(
                    pool,
                    &NewDeposit {
                        chain_id: 1,
                        tx_hash,
                        receipt_log_index: 0,
                        log_index: 3,
                        block_number: 100,
                        block_hash: B256::repeat_byte(0xb1),
                        block_time: Utc::now(),
                        address_id,
                        route: Some(fixture.live_route.route.clone()),
                        route_version: Some(fixture.live_route.version),
                        asset_contract: fixture.live_route.asset.contract,
                        from_address: Address::repeat_byte(0x74),
                        amount_atomic: AtomicAmount::new(U256::from(7_u64)),
                        state: DepositState::Detected,
                        reason: None,
                        next_attempt_at: Utc::now(),
                        tx_from: Address::repeat_byte(0x74),
                        tx_nonce: 0,
                        is_final: false,
                    },
                )
                .await?
            );
            let (_, merchant) = fixture
                .request(
                    Method::GET,
                    &format!("/v1/deposit_addresses/{id}"),
                    &fixture.live_key,
                    Value::Null,
                )
                .await?;
            ensure!(
                merchant["payments"][0]["status"] == "recorded",
                "{merchant}"
            );
            ensure!(merchant["payments"][0]["deposit"] == deposit, "{merchant}");
            let (_, view) = fixture.client_read(id, &secret).await?;
            ensure!(view["payments"][0]["status"] == "confirming", "{view}");
            sqlx::query("UPDATE deposits SET state = 'reversed' WHERE tx_hash = $1")
                .bind(format!("{tx_hash:#x}"))
                .execute(pool)
                .await?;
            let (_, view) = fixture.client_read(id, &secret).await?;
            ensure!(view["payments"][0]["status"] == "reversed", "{view}");

            // Another address's secret, a made-up secret, and a missing one read nothing.
            let (other, other_secret) = fixture
                .create_with_secret(&fixture.live_key, "team-43")
                .await?;
            ensure!(other["id"] != object["id"]);
            for secret in [
                other_secret.replace(other["id"].as_str().context("id")?, id),
                format!("{id}_secret_{}", "0".repeat(64)),
            ] {
                let (status, _) = fixture.client_read(id, &secret).await?;
                ensure!(status == StatusCode::NOT_FOUND, "{secret}");
            }
            let response = fixture
                .app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(format!("/v1/deposit_addresses/{id}"))
                        .body(axum::body::Body::empty())?,
                )
                .await?;
            ensure!(response.status() == StatusCode::UNAUTHORIZED);
            // Only the newest ten secrets of an address stay valid.
            for _ in 0..10 {
                fixture
                    .create_with_secret(&fixture.live_key, "team-42")
                    .await?;
            }
            let (status, _) = fixture.client_read(id, &secret).await?;
            ensure!(status == StatusCode::NOT_FOUND);
            Ok(())
        })
    })
    .await
}

struct Fixture {
    app: Router,
    pool: sqlx::PgPool,
    account: db::Account,
    live_key: String,
    test_key: String,
    live_route: RouteFile,
    usdc_route: RouteFile,
    other_route: RouteFile,
    test_route: RouteFile,
}

impl Fixture {
    async fn new(pool: &sqlx::PgPool) -> Result<Self> {
        let account = seed::create_account(pool, &NewAccount::named("merchant")).await?;
        let live_key = seed::create_api_key(pool, account.id, true).await?;
        let test_key = seed::create_api_key(pool, account.id, false).await?;
        for (livemode, chain_id) in [(true, 1), (true, OTHER_CHAIN), (false, TEST_CHAIN)] {
            seed::set_treasury(pool, account.id, livemode, chain_id, seed::FIXTURE_TREASURY)
                .await?;
        }
        let live_route: RouteFile =
            serde_saphyr::from_str(include_str!("fixtures/phala-cloud-pha.yaml"))?;
        let test_route: RouteFile = serde_saphyr::from_str(
            &include_str!("fixtures/phala-cloud-pha.yaml")
                .replace(
                    "route: phala-cloud-ethereum-pha-usd",
                    "route: phala-cloud-sepolia-pha",
                )
                .replace("chain_id: 1", &format!("chain_id: {TEST_CHAIN}"))
                .replace("livemode: true", "livemode: false"),
        )?;
        let mut usdc_route: RouteFile = serde_saphyr::from_str(
            &include_str!("fixtures/phala-cloud-pha.yaml")
                .replace(
                    "route: phala-cloud-ethereum-pha-usd",
                    "route: phala-cloud-ethereum-usdc-usd",
                )
                .replace("symbol: pha", "symbol: usdc")
                .replace("0x6c5bA91642F10282b576d91922Ae6448C9d52f4E", USDC),
        )?;
        usdc_route.pricing.mode = topup_core::route::PricingMode::Stablecoin;
        usdc_route.pricing.primary.clear();
        usdc_route.pricing.check.clear();
        usdc_route.pricing.fx.clear();
        usdc_route.pricing.sources = vec![
            topup_core::price::Source::Chainlink {
                feed: "USDC_USD".into(),
                chain_id: 1,
                rpc_group: "a".into(),
                rpc_group_b: None,
                observation_chain_id: None,
            },
            topup_core::price::Source::Kraken {
                symbol: "USDCUSD".into(),
                company: "kraken".into(),
            },
        ];
        let other_route: RouteFile = serde_saphyr::from_str(
            &include_str!("fixtures/phala-cloud-pha.yaml")
                .replace(
                    "route: phala-cloud-ethereum-pha-usd",
                    "route: phala-cloud-optimism-pha-usd",
                )
                .replace("chain_id: 1", &format!("chain_id: {OTHER_CHAIN}")),
        )?;
        // The merchant accepts every route of its fixture, and PHA on Base, which a test adds.
        let mut live = Document::accepting([&live_route, &usdc_route, &other_route]);
        live.chains.push(ChainChoice {
            chain_id: 8_453,
            confirmations: None,
            assets: vec![AssetChoice::on_defaults("pha")],
        });
        seed::configure_payments(pool, account.id, true, &live).await?;
        seed::accept_routes(pool, account.id, false, &[&test_route]).await?;
        Ok(Self {
            app: app(
                pool,
                vec![
                    live_route.clone(),
                    usdc_route.clone(),
                    other_route.clone(),
                    test_route.clone(),
                ],
            )?,
            pool: pool.clone(),
            account,
            live_key,
            test_key,
            live_route,
            usdc_route,
            other_route,
            test_route,
        })
    }

    /// The same account and keys served with other routes.
    fn with_routes(&self, routes: Vec<RouteFile>) -> Result<Self> {
        Ok(Self {
            app: app(&self.pool, routes)?,
            pool: self.pool.clone(),
            account: self.account.clone(),
            live_key: self.live_key.clone(),
            test_key: self.test_key.clone(),
            live_route: self.live_route.clone(),
            usdc_route: self.usdc_route.clone(),
            other_route: self.other_route.clone(),
            test_route: self.test_route.clone(),
        })
    }

    fn account_treasury(&self) -> Address {
        seed::FIXTURE_TREASURY
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

    /// The customer's address without its `client_secret`, which every create issues anew.
    async fn create(&self, key: &str, customer: &str) -> Result<Value> {
        Ok(self.create_with_secret(key, customer).await?.0)
    }

    async fn create_with_secret(&self, key: &str, customer: &str) -> Result<(Value, String)> {
        let (status, body) = self
            .request(
                Method::POST,
                "/v1/deposit_addresses",
                key,
                json!({"client_reference_id": customer}),
            )
            .await?;
        ensure!(status == StatusCode::OK, "{status}: {body}");
        without_secret(body)
    }

    /// An anonymous `GET` of the address's public view by `client_secret`.
    async fn client_read(&self, id: &str, client_secret: &str) -> Result<(StatusCode, Value)> {
        let request = axum::http::Request::builder()
            .method(Method::GET)
            .uri(format!(
                "/v1/deposit_addresses/{id}?client_secret={client_secret}"
            ))
            .body(axum::body::Body::empty())?;
        let response = self.app.clone().oneshot(request).await?;
        let status = response.status();
        ensure!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_some_and(|origin| origin == "*"),
            "a public read allows any origin"
        );
        let bytes = to_bytes(response.into_body(), 1_048_576).await?;
        Ok((status, serde_json::from_slice(&bytes)?))
    }

    async fn rotate(&self, key: &str, address: &Value) -> Result<Value> {
        let id = address["id"].as_str().context("id")?;
        let (status, body) = self
            .request(
                Method::POST,
                &format!("/v1/deposit_addresses/{id}/rotate"),
                key,
                Value::Null,
            )
            .await?;
        ensure!(status == StatusCode::OK, "{status}: {body}");
        Ok(without_secret(body)?.0)
    }
}

/// Splits off the response's `client_secret`, `da_…_secret_…` of the returned address.
fn without_secret(mut body: Value) -> Result<(Value, String)> {
    let object = body.as_object_mut().context("object")?;
    let secret = object
        .remove("client_secret")
        .and_then(|secret| secret.as_str().map(str::to_owned))
        .context("client_secret")?;
    let id = object.get("id").and_then(Value::as_str).context("id")?;
    ensure!(secret.starts_with(&format!("{id}_secret_")) && secret.len() == id.len() + 72);
    Ok((body, secret))
}

fn app(pool: &sqlx::PgPool, routes: Vec<RouteFile>) -> Result<Router> {
    let admin_key = SigningKey::from_bytes(&[47; 32]);
    Ok(topup::api::router(AppState {
        pool: pool.clone(),
        routes: Arc::new(topup::routes::RouteSet::new(routes).map_err(anyhow::Error::msg)?),
        maintenance_keys: Vec::new(),
        admin_key: VerificationKey::from_base64(
            "admin/v1".to_owned(),
            &public_key_base64(&admin_key),
        )
        .map_err(anyhow::Error::msg)?,
        public_origin: PublicOrigin::parse(TEST_ORIGIN)?,
        attestor: Arc::new(DstackAttestor::new()),
        rate_lock_quotes: Arc::new(topup::locks::UnavailableQuoteProvider),
        client_reads: Arc::default(),
        rate_limits: Arc::default(),
        screening: Arc::new(topup::refunds::UnavailableDestinationScreener),
        contract_signatures: Arc::new(topup::treasuries::UnavailableContractSignatures),
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

async fn freeze(pool: &sqlx::PgPool, chain_id: u64) -> Result<()> {
    sqlx::query(
        "INSERT INTO reconciliation_blocks (block_key, scope, chain_id, check_name, reason) \
         VALUES ($1, 'chain', $2, 'address_derivation', 'test freeze')",
    )
    .bind(format!("chain:{chain_id}"))
    .bind(i64::try_from(chain_id)?)
    .execute(pool)
    .await?;
    Ok(())
}
