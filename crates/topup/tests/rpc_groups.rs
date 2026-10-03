//! Durable coverage, atomic progress and wrong-watermark recovery acceptance tests.
mod support;
use alloy_primitives::{Address, B256};
use anyhow::{Context, Result, ensure};
use sqlx::Row;
use support::{TestDatabase, seed};
use topup::{db, routes::RouteSet};
use topup_adapters::chain::evm::{
    group::{HeadAnchor, WatermarkStore},
    window::{WindowProof, WindowRequest},
};
use uuid::Uuid;
fn proof(member: &str, from: u64, to: u64) -> WindowProof {
    WindowProof {
        group: "a".into(),
        member: member.into(),
        request: WindowRequest {
            from,
            to,
            recipients: Vec::new(),
            tokens: Vec::new(),
            factory: None,
            finalized: true,
            exclude_member: None,
        },
        end_hash: format!("0x{}", "11".repeat(32)),
    }
}
#[tokio::test]
async fn historical_coverage_survives_a_moving_tail_and_progress_is_atomic() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        let (_, customer) = seed::create_account_and_customer(
            pool,
            &seed::NewAccount::named("historical review"),
            "customer",
        )
        .await?;
        let address_id = Uuid::new_v4();
        let recipient = Address::repeat_byte(3);
        seed::insert_address(
            pool,
            &seed::NewAddress {
                id: address_id,
                customer_id: customer.id,
                chain_id: 1,
                route: "phala-cloud-pha-usd".into(),
                salt: B256::repeat_byte(3),
                address: recipient,
            },
        )
        .await?;
        let mut old = proof("old", 1, 2000);
        old.request.recipients.push(recipient);
        let tail = proof("new", 100_001, 102_000);
        db::rpc::commit_window(
            pool,
            1,
            &[],
            &[],
            Some(&old),
            db::rpc::WindowProgress {
                scanned: Some((2000, None)),
                reconciliation: Some((None, 2001)),
                ..Default::default()
            },
        )
        .await?;
        db::rpc::commit_window(
            pool,
            1,
            &[],
            &[],
            Some(&tail),
            db::rpc::WindowProgress {
                scanned: Some((102_000, None)),
                ..Default::default()
            },
        )
        .await?;
        let pending = db::rpc::due_reviews(pool, 1).await?;
        ensure!(
            pending.iter().any(|(_, r, m)| r.from == 1 && m == "old"),
            "old catch-up window lost review coverage"
        );
        let id = pending
            .iter()
            .find(|(_, r, _)| r.from == 1)
            .expect("old coverage")
            .0;
        let mut reviewed = old.clone();
        reviewed.member = "independent".into();
        reviewed.request.exclude_member = Some("old".into());
        let missing = db::NewDeposit {
            chain_id: 1,
            tx_hash: B256::repeat_byte(4),
            receipt_log_index: 0,
            log_index: 1,
            block_number: 100,
            block_hash: B256::repeat_byte(0x11),
            block_time: chrono::Utc::now(),
            address_id,
            route: Some("phala-cloud-pha-usd".into()),
            route_version: Some(1),
            asset_contract: Address::repeat_byte(5),
            from_address: Address::ZERO,
            amount_atomic: topup_core::money::AtomicAmount::new(alloy_primitives::U256::from(500)),
            state: topup_core::deposit::DepositState::Detected,
            reason: None,
            next_attempt_at: chrono::Utc::now(),
            tx_from: Address::ZERO,
            tx_nonce: 0,
            is_final: false,
        };
        db::rpc::commit_window(
            pool,
            1,
            &[missing],
            &[],
            Some(&reviewed),
            db::rpc::WindowProgress {
                reviewed: Some(id),
                ..Default::default()
            },
        )
        .await?;
        ensure!(
            !db::rpc::due_reviews(pool, 1)
                .await?
                .iter()
                .any(|(i, _, _)| i == &id)
        );
        let found: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM deposits WHERE chain_id=1 AND block_number=100",
        )
        .fetch_one(pool)
        .await?;
        ensure!(
            found == 1,
            "missed deposit in old historical window was not repaired"
        );
        ensure!(
            db::get_cursor(pool, 1).await? == Some(102_000),
            "review must not rewind forward progress"
        );
        let cursor = db::get_cursor(pool, 1).await?;
        let refused = db::rpc::commit_window(
            pool,
            1,
            &[],
            &[],
            Some(&proof("new", 102_001, 104_000)),
            db::rpc::WindowProgress {
                scanned: Some((104_000, None)),
                reconciliation: Some((Some(99), 104_001)),
                ..Default::default()
            },
        )
        .await;
        ensure!(refused.is_err());
        ensure!(
            db::get_cursor(pool, 1).await? == cursor,
            "failed atomic window advanced scanner"
        );
        let recon: i64 = sqlx::query_scalar(
            "SELECT next_block FROM reconciliation_deposit_cursors WHERE chain_id=1",
        )
        .fetch_one(pool)
        .await?;
        ensure!(recon == 2001, "failed atomic window advanced reconciler");
        Ok(())
    }
    .await;
    database.cleanup().await?;
    result
}
#[tokio::test]
async fn recovery_repairs_poisoned_address_progress_and_preserves_audit() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result=async {
        let pool=&database.app_pool;
        let (_,customer)=seed::create_account_and_customer(pool,&seed::NewAccount::named("rpc recovery"),"customer").await?;
        let id=Uuid::new_v4();seed::insert_address(pool,&seed::NewAddress {id,customer_id:customer.id,chain_id:1,route:"phala-cloud-pha-usd".into(),salt:B256::repeat_byte(3),address:Address::repeat_byte(3)}).await?;
        sqlx::query("UPDATE addresses SET created_block=9000,backfilled=true,backfilled_through=10000 WHERE id=$1").bind(id).execute(pool).await?;
        db::initialize_cursor(pool,1,10_000,chrono::Utc::now()).await?;
        let state=db::rpc::state(pool,"config".into());let wrong=HeadAnchor {number:10_000,hash:format!("0x{}","22".repeat(32)),parent_hash:format!("0x{}","33".repeat(32))};
        state.accept(1,"a","finalized","bad",&wrong).await?;state.freeze(1).await?;
        let correct=HeadAnchor {number:100,hash:format!("0x{}","11".repeat(32)),parent_hash:format!("0x{}","00".repeat(32))};
        db::rpc::recover(&database.owner_pool,1,&correct,"reviewed operator","wrong accepted head").await?;
        let row=sqlx::query("SELECT created_block,backfilled,backfilled_through FROM addresses WHERE id=$1").bind(id).fetch_one(pool).await?;
        ensure!(row.try_get::<i64,_>("created_block")?==0);ensure!(!row.try_get::<bool,_>("backfilled")?);ensure!(row.try_get::<Option<i64>,_>("backfilled_through")?.is_none());
        ensure!(db::get_confirmed_cursor(pool,1).await?.is_none());
        let time:Option<chrono::DateTime<chrono::Utc>>=sqlx::query_scalar("SELECT scanned_block_time FROM cursors WHERE chain_id=1").fetch_one(pool).await?;
        ensure!(time.is_none());ensure!(state.blocked(1).await.is_err());
        let evidence:serde_json::Value=sqlx::query_scalar("SELECT evidence FROM rpc_recoveries WHERE chain_id=1").fetch_one(pool).await?;
        ensure!(evidence.to_string().contains("9000"));
        // Runtime cannot mutate/delete the recovery audit.
        ensure!(sqlx::query("DELETE FROM rpc_recoveries").execute(pool).await.is_err());
        Ok(())
    }.await;
    database.cleanup().await?;
    result
}
#[test]
fn redundant_staging_preserves_roles_and_rejects_old_schema() -> Result<()> {
    let config = topup::config::Config::parse(include_str!(
        "../../../deploy/environments/phala-network/staging/topup/topup.yaml"
    ))
    .map_err(anyhow::Error::msg)?;
    for route in &config.routes {
        for (role, id) in route.chain.rpc_providers.iter().enumerate() {
            let group = &config.rpc_groups[id];
            if id == "base-sepolia-b" {
                // No independently operated backup passed the genesis/capability requirements.
                ensure!(group.members.len() == 1);
            } else {
                ensure!(group.members.len() >= 2);
            }
            ensure!(
                group
                    .members
                    .iter()
                    .all(|member| member.sealed_key.is_none())
            );
            ensure!(group.members[0].company == if role == 0 { "tenderly" } else { "publicnode" });
        }
    }
    let route: topup_core::route::RouteFile =
        serde_saphyr::from_str(include_str!("fixtures/phala-cloud-pha.yaml"))?;
    let mut bad = route.clone();
    bad.chain.rpc_providers.push("silently-dropped".into());
    ensure!(RouteSet::new(vec![bad]).is_err());
    let legacy = include_str!("fixtures/phala-cloud-pha.yaml").replace(
        "rpc_groups: { a: alchemy, b: quicknode }",
        "rpc_providers: [alchemy, quicknode, spare]",
    );
    ensure!(serde_saphyr::from_str::<topup_core::route::RouteFile>(&legacy).is_err());
    Ok(())
}

#[tokio::test]
async fn lagging_empty_and_rpc_error_windows_leave_both_cursors_until_complete_answer() -> Result<()>
{
    use axum::{Json, Router, routing::post};
    use std::collections::BTreeMap;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use topup_adapters::{
        chain::evm::{
            ChainReader, EvmClient, FinalizedReader,
            group::{
                GroupPolicy, Member, RpcGroup,
                budget::{BudgetSpec, Budgets},
            },
        },
        redaction::Redacted,
    };
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let mode = Arc::new(AtomicUsize::new(0));
    let log_sends = Arc::new(AtomicUsize::new(0));
    let block: serde_json::Value = serde_json::from_str(include_str!(
        "../../adapters/tests/fixtures/base-sepolia/block-47297199.json"
    ))?;
    let logs: serde_json::Value = serde_json::from_str(include_str!(
        "../../adapters/tests/fixtures/base-sepolia/bridge-mint-logs.json"
    ))?;
    let receipt: serde_json::Value = serde_json::from_str(include_str!(
        "../../adapters/tests/fixtures/base-sepolia/bridge-mint-receipt.json"
    ))?;
    let number = u64::from_str_radix(
        block["number"]
            .as_str()
            .context("number")?
            .trim_start_matches("0x"),
        16,
    )?;
    let response_mode = mode.clone();
    let hits = log_sends.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let router=Router::new().route("/",post(move |Json(request):Json<serde_json::Value>| {
        let mode=response_mode.clone();let hits=hits.clone();let mut block=block.clone();let logs=logs.clone();let receipt=receipt.clone();
        async move {
            let result=match request["method"].as_str().unwrap() {
                "eth_getBlockByNumber"|"eth_getBlockByHash"=> {if mode.load(Ordering::SeqCst)==0 || request["params"][0]==serde_json::json!(format!("0x{:x}",number-1)) {
                    block["number"]=serde_json::json!(format!("0x{:x}",number-1));block["hash"]=block["parentHash"].clone();
                    block["parentHash"]=serde_json::json!(format!("0x{}","22".repeat(32)));
                }block},
                "eth_getLogs"=>{hits.fetch_add(1,Ordering::SeqCst);if mode.load(Ordering::SeqCst)==1 {return Json(serde_json::json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32603,"message":"internal"}}));}
                if mode.load(Ordering::SeqCst)==0 {serde_json::json!([])} else {logs}},
                "eth_getTransactionReceipt"=>receipt,
                other=>panic!("unexpected method {other}"),
            };
            Json(serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let result = async {
        let pool = &database.app_pool;
        let (_, customer) = seed::create_account_and_customer(
            pool,
            &seed::NewAccount::named("atomic window"),
            "customer",
        )
        .await?;
        let id = Uuid::new_v4();
        let recipient: Address = "0x3c0fe91b38c2f708d360f5724208fa7ecaa6ed34".parse()?;
        seed::insert_address(
            pool,
            &seed::NewAddress {
                id,
                customer_id: customer.id,
                chain_id: 84532,
                route: "phala-cloud-pha-usd".into(),
                salt: B256::repeat_byte(3),
                address: recipient,
            },
        )
        .await?;
        db::initialize_cursor(pool, 84532, number - 1, chrono::Utc::now()).await?;
        sqlx::query(
            "INSERT INTO reconciliation_deposit_cursors(chain_id,next_block) VALUES(84532,$1)",
        )
        .bind(i64::try_from(number)?)
        .execute(pool)
        .await?;
        let state = db::rpc::state(pool, "test".into());
        let request = WindowRequest {
            from: number,
            to: number,
            recipients: vec![recipient],
            tokens: vec![],
            factory: None,
            finalized: true,
            exclude_member: None,
        };
        for current in 0..3 {
            mode.store(current, Ordering::SeqCst);
            let budgets = Arc::new(
                Budgets::new(&BTreeMap::from([
                    (
                        "account".into(),
                        BudgetSpec {
                            requests_per_second: 1000,
                            burst: 100,
                        },
                    ),
                    (
                        "key".into(),
                        BudgetSpec {
                            requests_per_second: 1000,
                            burst: 100,
                        },
                    ),
                ]))
                .map_err(anyhow::Error::msg)?,
            );
            let group = RpcGroup::new(
                "atomic-window-test".into(),
                84532,
                GroupPolicy {
                    max_attempts: 1,
                    ..Default::default()
                },
                vec![Member {
                    id: "member".into(),
                    company: "simulated".into(),
                    endpoint: Redacted::parse(&endpoint)?,
                    account: "account".into(),
                    key: "key".into(),
                    priority: 0,
                    weight: 1,
                }],
                budgets,
            )?;
            group.verified(0, true);
            group.set_store(state.clone());
            let reader = FinalizedReader::new(Arc::new(EvmClient::from_group(group, None)?));
            let answer = reader.read_window(&request).await;
            if current < 2 {
                ensure!(answer.is_err());
                ensure!(db::get_cursor(pool, 84532).await? == Some(number - 1));
                let cursor: i64 = sqlx::query_scalar(
                    "SELECT next_block FROM reconciliation_deposit_cursors WHERE chain_id=84532",
                )
                .fetch_one(pool)
                .await?;
                ensure!(cursor == i64::try_from(number)?);
                if current == 0 {
                    ensure!(log_sends.load(Ordering::SeqCst) == 0);
                }
            } else {
                let window = answer?;
                ensure!(window.transfers.len() == 1);
                let log = &window.transfers[0];
                let deposit = db::NewDeposit {
                    chain_id: 84532,
                    tx_hash: log.tx_hash,
                    receipt_log_index: log.receipt_log_index,
                    log_index: log.log_index,
                    block_number: log.block_number,
                    block_hash: log.block_hash,
                    block_time: log.block_time,
                    address_id: id,
                    route: Some("phala-cloud-pha-usd".into()),
                    route_version: Some(1),
                    asset_contract: log.token,
                    from_address: log.from,
                    amount_atomic: log.amount,
                    state: topup_core::deposit::DepositState::Detected,
                    reason: None,
                    next_attempt_at: chrono::Utc::now(),
                    tx_from: log.tx_from,
                    tx_nonce: log.tx_nonce,
                    is_final: false,
                };
                db::rpc::commit_window(
                    pool,
                    84532,
                    &[deposit],
                    &[],
                    window.proof.as_ref(),
                    db::rpc::WindowProgress {
                        scanned: Some((number, None)),
                        reconciliation: Some((Some(number), number + 1)),
                        ..Default::default()
                    },
                )
                .await?;
                ensure!(db::get_cursor(pool, 84532).await? == Some(number));
            }
        }
        Ok(())
    }
    .await;
    server.abort();
    database.cleanup().await?;
    result
}

#[test]
fn repeated_templates_allow_distinct_credentials_and_aliases_share_quota_scopes() -> Result<()> {
    use std::collections::BTreeMap;
    let mut config = topup::config::Config::parse(include_str!(
        "../../../deploy/environments/phala-network/staging/topup/topup.yaml"
    ))
    .map_err(anyhow::Error::msg)?;
    let group = config
        .rpc_groups
        .get_mut("provider-a")
        .context("Tenderly A")?;
    let original_members = group.members.len();
    let member = group.members.first_mut().context("member")?;
    member.url.push_str("/{key}");
    member.sealed_key = Some("TOPUP_RPC_FIRST_KEY".into());
    let mut second = member.clone();
    second.id = "second-credential".into();
    second.sealed_key = Some("TOPUP_RPC_SECOND_KEY".into());
    second.key_budget = "second-key-budget".into();
    config.rpc_budgets.insert(
        second.key_budget.clone(),
        config.rpc_budgets[&member.key_budget].clone(),
    );
    group.members.push(second);
    topup::rpc_groups::validate(
        &config.routes,
        &config.rpc_groups,
        &config.rpc_companies,
        &config.rpc_budgets,
    )
    .map_err(anyhow::Error::msg)?;
    let secrets = BTreeMap::from([
        ("TOPUP_RPC_FIRST_KEY", "first-test-key"),
        ("TOPUP_RPC_SECOND_KEY", "second-test-key"),
    ]);
    let clients = topup::rpc_groups::clients(&config.rpc_groups, &config.rpc_budgets, |name| {
        secrets.get(name).map(|s| (*s).into())
    })
    .map_err(anyhow::Error::msg)?;
    ensure!(
        clients["provider-a"]
            .group()
            .context("group")?
            .members
            .len()
            == original_members.saturating_add(1)
    );
    let aliases = topup::rpc_groups::clients(&config.rpc_groups, &config.rpc_budgets, |_| {
        Some("same-test-credential".into())
    });
    ensure!(
        aliases.is_err(),
        "same credential under different sealed names cannot get extra key quota"
    );
    Ok(())
}

#[test]
fn reviewed_company_aliases_cannot_split_one_registrable_domain_across_roles() -> Result<()> {
    let original =
        include_str!("../../../deploy/environments/phala-network/staging/topup/topup.yaml");
    let config = topup::config::Config::parse(original).map_err(anyhow::Error::msg)?;
    let mut groups = config.rpc_groups.clone();
    let mut companies = config.rpc_companies.clone();
    companies.get_mut("tenderly").context("tenderly")?.domains = vec!["a.vendor.co.uk".into()];
    companies
        .get_mut("publicnode")
        .context("publicnode")?
        .domains = vec!["b.vendor.co.uk".into()];
    for group in groups.values_mut() {
        for member in &mut group.members {
            let host = if member.company == "tenderly" {
                "a.vendor.co.uk"
            } else if member.company == "publicnode" {
                "b.vendor.co.uk"
            } else {
                continue;
            };
            let path = if member.sealed_key.is_some() {
                "/{key}"
            } else {
                "/"
            };
            member.url = format!("https://{host}{path}");
        }
    }
    let mut independent_companies = companies.clone();
    independent_companies
        .get_mut("publicnode")
        .context("publicnode")?
        .domains = vec!["b.independent.co.uk".into()];
    let mut independent_groups = groups.clone();
    for group in independent_groups.values_mut() {
        for member in &mut group.members {
            if member.company == "publicnode" {
                member.url = member.url.replace("b.vendor.co.uk", "b.independent.co.uk");
            }
        }
    }
    topup::rpc_groups::validate(
        &config.routes,
        &independent_groups,
        &independent_companies,
        &config.rpc_budgets,
    )
    .map_err(anyhow::Error::msg)
    .context("the control config must be valid")?;
    let error =
        topup::rpc_groups::validate(&config.routes, &groups, &companies, &config.rpc_budgets)
            .expect_err("PSL identity must reject sibling-domain aliases");
    ensure!(error.contains("domain ownership"), "{error}");
    Ok(())
}
