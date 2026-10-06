//! Executable per-member acceptance, recovery probing and height-only cursor anchoring.
use crate::{db, routes::RouteSet};
use serde_json::json;
use sqlx::PgPool;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use topup_adapters::chain::evm::{
    EvmClient,
    group::{HeadAnchor, RpcGroup, WatermarkStore},
};
use topup_core::route::RouteFile;

async fn probe(
    group: &Arc<RpcGroup>,
    index: usize,
    routes: &[&RouteFile],
) -> Result<String, String> {
    let copy = group.probe_copy().map_err(|e| e.to_string())?;
    let result = tokio::time::timeout(Duration::from_millis(group.policy.probe.deadline), async {
        let hash = probe_inner(&copy, index, routes).await?;
        group
            .validate_probe(index, &copy)
            .await
            .map_err(|e| format!("persisted head/genesis anchor: {e}"))?;
        Ok(hash)
    })
    .await
    .map_err(|_| "probe total deadline: RPC deadline expired".to_owned())?;
    if copy.quarantined(index) {
        group.failed(index, topup_adapters::chain::evm::group::Failure::Identity);
    }
    result
}
async fn probe_inner(
    group: &Arc<RpcGroup>,
    index: usize,
    routes: &[&RouteFile],
) -> Result<String, String> {
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(group.policy.probe.deadline))
        .unwrap_or_else(Instant::now);
    let chain = group
        .send(
            index,
            &json!({"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}),
            deadline,
        )
        .await
        .map_err(|e| format!("chain id: {e}"))?;
    if chain.get("result").and_then(serde_json::Value::as_str)
        != Some(format!("0x{:x}", group.chain).as_str())
    {
        group.failed(index, topup_adapters::chain::evm::group::Failure::Identity);
        return Err("chain id: RPC member identity invalid (mismatch)".to_owned());
    }
    let genesis = block(group, index, 0, deadline)
        .await
        .map_err(|e| format!("genesis: {e}"))?;
    let client = EvmClient::from_group(group.clone(), Some(index)).map_err(|e| e.to_string())?;
    let mut contracts = std::collections::BTreeSet::new();
    for route in routes {
        if contracts.insert((
            route.chain.contracts.forwarder_factory,
            route.chain.contracts.implementation,
        )) {
            crate::contracts::verify_on(&client, route)
                .await
                .map_err(|e| format!("route {} contracts/multicall: {e}", route.route))?;
        }
    }
    group
        .head(index, "latest", deadline)
        .await
        .map_err(|e| format!("latest head: {e}"))?;
    // Use one finalized snapshot for every numeric capability/log check. Tagged heads
    // from a load-balanced endpoint can regress between redundant reads.
    let head = group
        .head(index, "finalized", deadline)
        .await
        .map_err(|e| format!("finalized head: {e}"))?;
    group
        .head(index, "safe", deadline)
        .await
        .map_err(|e| format!("safe head: {e}"))?;
    // Observation-only groups need typed feed calls, not payment receipt/log capabilities.
    if routes.is_empty() {
        let name = if group.chain == 8453 {
            "BASE_SEQUENCER_UPTIME"
        } else {
            "USDT_USD"
        };
        let feed = topup_core::price::feed(name, group.chain)
            .ok_or("unsupported observation-only feed chain")?;
        let address = feed
            .address
            .parse()
            .map_err(|_| "invalid pinned feed address")?;
        // The price reader uses latest state. Historical state is not a required capability.
        let decimals = client
            .call(
                "RPC price capability",
                address,
                alloy_primitives::Bytes::from_static(&[0x31, 0x3c, 0xe5, 0x67]),
                Some(alloy::eips::BlockId::latest()),
            )
            .await
            .map_err(|e| format!("price decimals capability: {e}"))?;
        if decimals.len() != 32
            || alloy_primitives::U256::from_be_slice(&decimals)
                != alloy_primitives::U256::from(feed.decimals)
        {
            return Err("price decimals capability: RPC malformed response".into());
        }
        return Ok(genesis.hash);
    }
    group.send(index,&json!({"jsonrpc":"2.0","id":1,"method":"eth_getTransactionReceipt","params":[format!("0x{}","00".repeat(32))]}),deadline).await.map_err(|e|format!("receipt capability: {e}"))?;
    if routes
        .iter()
        .any(|r| r.chain.rpc_providers.first() == Some(&group.id))
    {
        let logs = json!({"jsonrpc":"2.0","id":1,"method":"eth_getLogs","params":[{"fromBlock":format!("0x{:x}",head.number.saturating_sub(1999)),"toBlock":format!("0x{:x}",head.number),"topics":["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",null,[format!("0x{}","00".repeat(32))]]}]});
        group
            .send(index, &logs, deadline)
            .await
            .map_err(|e| format!("address-less transfer logs (2000 blocks): {e}"))?;
    } else {
        for route in routes {
            let logs = json!({"jsonrpc":"2.0","id":1,"method":"eth_getLogs","params":[{
                "fromBlock":format!("0x{:x}",head.number.saturating_sub(99)),
                "toBlock":format!("0x{:x}",head.number), "address":format!("{:#x}",route.asset.contract),
                "topics":["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",null,[format!("0x{}","00".repeat(32))]]
            }]});
            group.send(index, &logs, deadline).await.map_err(|e| {
                format!(
                    "route {} recent-range transfer logs (100 blocks): {e}",
                    route.route
                )
            })?;
        }
    }
    for route in routes {
        for contract in [route.asset.contract, route.screening.sanctions_oracle] {
            if client
                .code_at(contract)
                .await
                .map_err(|e| format!("route {} contract code {contract:#x}: {e}", route.route))?
                .is_empty()
            {
                return Err(format!(
                    "route {} contract code {contract:#x}: RPC capability unavailable (missing code)",
                    route.route
                ));
            }
        }
    }
    for route in routes {
        let decimals = client
            .call(
                "RPC token capability",
                route.asset.contract,
                alloy_primitives::Bytes::from_static(&[0x31, 0x3c, 0xe5, 0x67]),
                Some(head.number.into()),
            )
            .await
            .map_err(|e| format!("route {} token decimals capability: {e}", route.route))?;
        if decimals.len() != 32
            || alloy_primitives::U256::from_be_slice(&decimals)
                != alloy_primitives::U256::from(route.asset.decimals)
        {
            return Err(format!(
                "route {} token decimals capability: RPC member identity invalid (mismatch)",
                route.route
            ));
        }
        let hash = alloy_primitives::keccak256("isSanctioned(address)");
        let mut data = hash
            .as_slice()
            .get(..4)
            .ok_or("oracle selector missing")?
            .to_vec();
        data.extend_from_slice(&[0u8; 32]);
        let answer = client
            .call(
                "RPC oracle capability",
                route.screening.sanctions_oracle,
                data.into(),
                Some(head.number.into()),
            )
            .await
            .map_err(|e| format!("route {} oracle capability: {e}", route.route))?;
        if answer.len() != 32
            || alloy_primitives::U256::from_be_slice(&answer) > alloy_primitives::U256::from(1)
        {
            return Err(format!(
                "route {} oracle capability: RPC malformed response",
                route.route
            ));
        }
    }
    Ok(genesis.hash)
}
async fn block(
    group: &RpcGroup,
    index: usize,
    number: u64,
    deadline: Instant,
) -> Result<HeadAnchor, String> {
    let v=group.send(index,&json!({"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":[format!("0x{number:x}"),false]}),deadline).await.map_err(|e|e.to_string())?;
    let anchor =
        HeadAnchor::parse(v.get("result").ok_or("missing block")?).map_err(|e| e.to_string())?;
    if anchor.number != number {
        return Err("RPC returned a different numeric block".into());
    }
    Ok(anchor)
}
type Groups<'a> = BTreeMap<String, (Arc<RpcGroup>, Vec<&'a RouteFile>)>;
fn groups(routes: &RouteSet) -> Result<Groups<'_>, String> {
    let mut result: BTreeMap<String, (Arc<RpcGroup>, Vec<&RouteFile>)> = BTreeMap::new();
    for route in routes.routes() {
        for role in 0..2 {
            let client = routes
                .provider(route.chain.chain_id, role)
                .map_err(|e| e.to_string())?;
            if let Some(group) = client.group() {
                result
                    .entry(group.id.clone())
                    .or_insert_with(|| (group.clone(), Vec::new()))
                    .1
                    .push(route);
            }
        }
    }
    for (id, client) in routes.groups() {
        if let Some(group) = client.group() {
            result
                .entry(id.clone())
                .or_insert_with(|| (group.clone(), Vec::new()));
        }
    }
    Ok(result)
}

/// Binds one durable safety store to every group the runtime constructs
/// (route A/B roles and observation-only groups), keyed by the accepted config digest.
pub fn bind_durable_state(
    pool: &PgPool,
    routes: &RouteSet,
    public_config: &str,
) -> Result<Arc<dyn WatermarkStore>, String> {
    let state = db::rpc::state(pool, db::rpc::digest(public_config));
    for (group, _) in groups(routes)?.values() {
        group.set_store(state.clone());
    }
    Ok(state)
}

/// First acceptance needs one fully verified member in each group; offline backups do not veto it.
/// The exact accepted digest may restart with no serving members, without issuing or crediting on
/// unavailable evidence. Every returning member still undergoes complete verification.
pub async fn accept(pool: &PgPool, routes: &RouteSet, public_config: &str) -> Result<(), String> {
    let digest = db::rpc::digest(public_config);
    let accepted: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM rpc_config_acceptances WHERE config_digest=$1)",
    )
    .bind(&digest)
    .fetch_one(pool)
    .await
    .map_err(|e| e.to_string())?;
    let groups = groups(routes)?;
    check_roles(pool, routes).await?;
    let state = bind_durable_state(pool, routes, public_config)?;
    let mut genesis = BTreeMap::new();
    let mut validations = Vec::new();
    for (group, route_files) in groups.values() {
        for index in 0..group.members.len() {
            match probe(group, index, route_files).await {
                Ok(hash) => {
                    let previous: Option<String> = sqlx::query_scalar(
                        "SELECT genesis_hash FROM rpc_member_validations WHERE chain_id=$1 LIMIT 1",
                    )
                    .bind(i64::try_from(group.chain).map_err(|e| e.to_string())?)
                    .fetch_optional(pool)
                    .await
                    .map_err(|e| e.to_string())?;
                    if previous.as_ref().is_some_and(|h| h != &hash)
                        || genesis.get(&group.chain).is_some_and(|h| h != &hash)
                    {
                        group.failed(index, topup_adapters::chain::evm::group::Failure::Identity);
                        tracing::warn!(group=%group.id,member=%group.members.get(index).map(|m|m.id.as_str()).unwrap_or("unknown"), "RPC genesis mismatch; member quarantined");
                        continue;
                    }
                    genesis.insert(group.chain, hash.clone());
                    group.verified(index, true);
                    validations.push((group.clone(), index, hash));
                }
                Err(error) => {
                    group.verified(index, false);
                    tracing::warn!(group=%group.id,member=%group.members.get(index).map(|m|m.id.as_str()).unwrap_or("unknown"),%error,"RPC member preflight failed");
                }
            }
        }
        if !accepted && group.eligible() == 0 {
            return Err(format!(
                "RPC group {} has no verified member for first acceptance",
                group.id
            ));
        }
    }
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    sqlx::query(
        "INSERT INTO rpc_config_acceptances(config_digest) VALUES($1) ON CONFLICT DO NOTHING",
    )
    .bind(&digest)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    for route in routes.routes() {
        for (role, id) in ["a", "b"].iter().zip(&route.chain.rpc_providers) {
            let _stored: Option<String> = sqlx::query_scalar("INSERT INTO rpc_role_bindings(chain_id,role,group_id) VALUES($1,$2,$3) ON CONFLICT(chain_id,role) DO NOTHING RETURNING group_id")
                .bind(i64::try_from(route.chain.chain_id).map_err(|e|e.to_string())?).bind(role).bind(id).fetch_optional(&mut *tx).await.map_err(|e|e.to_string())?;
            let stored: String = sqlx::query_scalar(
                "SELECT group_id FROM rpc_role_bindings WHERE chain_id=$1 AND role=$2",
            )
            .bind(i64::try_from(route.chain.chain_id).map_err(|e| e.to_string())?)
            .bind(role)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
            if &stored != id {
                return Err("accepted RPC role/group identity cannot change".into());
            }
        }
    }
    for (group, index, hash) in validations {
        let member = group.members.get(index).ok_or("missing RPC member")?;
        sqlx::query("INSERT INTO rpc_member_validations(config_digest,group_id,member_id,chain_id,genesis_hash) VALUES($1,$2,$3,$4,$5) ON CONFLICT(config_digest,group_id,member_id) DO UPDATE SET validated_at=now()").bind(&digest).bind(&group.id).bind(&member.id).bind(i64::try_from(group.chain).map_err(|e|e.to_string())?).bind(hash).execute(&mut *tx).await.map_err(|e|e.to_string())?;
    }
    tx.commit().await.map_err(|e| e.to_string())?;
    anchor_cursors(pool, routes, state.as_ref()).await?;
    Ok(())
}
async fn check_roles(pool: &PgPool, routes: &RouteSet) -> Result<(), String> {
    for route in routes.routes() {
        for (role, id) in ["a", "b"].iter().zip(&route.chain.rpc_providers) {
            let old: Option<String> = sqlx::query_scalar(
                "SELECT group_id FROM rpc_role_bindings WHERE chain_id=$1 AND role=$2",
            )
            .bind(i64::try_from(route.chain.chain_id).map_err(|e| e.to_string())?)
            .bind(role)
            .fetch_optional(pool)
            .await
            .map_err(|e| e.to_string())?;
            if old.as_ref().is_some_and(|old| old != id) {
                return Err("accepted RPC role/group identity cannot change".into());
            }
        }
    }
    Ok(())
}

/// Standalone reconciliation/restore must establish the same trusted A/B cursor floors.
pub async fn ensure_anchors(pool: &PgPool, routes: &RouteSet) -> Result<(), String> {
    if !routes.has_rpc_groups() {
        return Ok(());
    }
    let configured = groups(routes)?;
    let mut state = None;
    for (id, (group, _)) in &configured {
        let store = group
            .watermark_store()
            .ok_or_else(|| format!("RPC durable safety store missing for group {id}"))?;
        state.get_or_insert(store);
    }
    let Some(state) = state else {
        return Ok(());
    };
    check_roles(pool, routes).await?;
    anchor_cursors(pool, routes, state.as_ref()).await
}
async fn anchor_cursors(
    pool: &PgPool,
    routes: &RouteSet,
    state: &dyn WatermarkStore,
) -> Result<(), String> {
    for chain in routes.chain_ids() {
        let a = routes
            .provider(chain, 0)
            .map_err(|e| e.to_string())?
            .group()
            .ok_or("RPC A group missing")?;
        let b = routes
            .provider(chain, 1)
            .map_err(|e| e.to_string())?
            .group()
            .ok_or("RPC B group missing")?;
        let saved_a = state.load(chain, &a.id, "cursor").await;
        let saved_b = state.load(chain, &b.id, "cursor").await;
        match (saved_a, saved_b) {
            (Ok(Some(a)), Ok(Some(b))) if a == b => {
                sqlx::query("UPDATE rpc_chain_state SET awaiting_anchor=false WHERE chain_id=$1")
                    .bind(i64::try_from(chain).map_err(|e| e.to_string())?)
                    .execute(pool)
                    .await
                    .map_err(|e| e.to_string())?;
                continue;
            }
            (Err(topup_adapters::chain::evm::group::Failure::Fork), _)
            | (_, Err(topup_adapters::chain::evm::group::Failure::Fork)) => continue,
            (Err(e), _) | (_, Err(e)) => return Err(e.to_string()),
            _ => {}
        }
        sqlx::query("INSERT INTO rpc_chain_state(chain_id,awaiting_anchor) VALUES($1,true) ON CONFLICT(chain_id) DO UPDATE SET awaiting_anchor=true").bind(i64::try_from(chain).map_err(|e|e.to_string())?).execute(pool).await.map_err(|e|e.to_string())?;
        let (Ok(ai), Ok(bi)) = (
            a.select(&Default::default(), None),
            b.select(&Default::default(), None),
        ) else {
            sqlx::query("INSERT INTO rpc_chain_state(chain_id,awaiting_anchor) VALUES($1,true) ON CONFLICT(chain_id) DO UPDATE SET awaiting_anchor=true").bind(i64::try_from(chain).map_err(|e|e.to_string())?).execute(pool).await.map_err(|e|e.to_string())?;
            continue;
        };
        let ap = a.probe_copy().map_err(|e| e.to_string())?;
        let bp = b.probe_copy().map_err(|e| e.to_string())?;
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(
                a.policy.total_deadline_ms.min(b.policy.total_deadline_ms),
            ))
            .unwrap_or_else(Instant::now);
        let ahead = ap
            .head(ai, "finalized", deadline)
            .await
            .map_err(|e| e.to_string())?;
        let bhead = bp
            .head(bi, "finalized", deadline)
            .await
            .map_err(|e| e.to_string())?;
        let old = db::get_cursor(pool, chain)
            .await
            .map_err(|e| e.to_string())?;
        let restoring = crate::restore_mode::detect(pool)
            .await
            .map_err(|e| e.to_string())?
            .is_some();
        let height = old.unwrap_or(if restoring {
            0
        } else {
            ahead.number.min(bhead.number)
        });
        if height > ahead.number.min(bhead.number) {
            state.freeze(chain).await.map_err(|e| e.to_string())?;
            return Err(
                "legacy cursor exceeds agreed finalized evidence; audited recovery required"
                    .to_owned(),
            );
        }
        let ah = block(&ap, ai, height, deadline).await?;
        let bh = block(&bp, bi, height, deadline).await?;
        if ah != bh || ah.number != height {
            state.freeze(chain).await.map_err(|e| e.to_string())?;
            return Err("height-only cursor has no agreed A/B hash anchor".to_owned());
        }
        if old.is_none() {
            let v=ap.send(ai,&json!({"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":[format!("0x{height:x}"),false]}),deadline).await.map_err(|e|e.to_string())?;
            let time = v
                .get("result")
                .and_then(|v| v.get("timestamp"))
                .and_then(serde_json::Value::as_str)
                .and_then(|s| u64::from_str_radix(s.strip_prefix("0x")?, 16).ok())
                .and_then(|n| i64::try_from(n).ok())
                .and_then(|n| chrono::DateTime::from_timestamp(n, 0))
                .ok_or("malformed cursor timestamp")?;
            db::initialize_cursor(pool, chain, height, time)
                .await
                .map_err(|e| e.to_string())?;
        }
        for (group, index) in [(a, ai), (b, bi)] {
            state
                .accept(
                    chain,
                    &group.id,
                    "cursor",
                    &group.members.get(index).ok_or("RPC member missing")?.id,
                    &ah,
                )
                .await
                .map_err(|e| e.to_string())?;
        }
        sqlx::query("UPDATE rpc_chain_state SET awaiting_anchor=false WHERE chain_id=$1")
            .bind(i64::try_from(chain).map_err(|e| e.to_string())?)
            .execute(pool)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}
/// Cooldown expiry only schedules probes; full identity/contracts/heads are checked before readmit.
pub async fn recover_members(
    pool: PgPool,
    routes: Arc<RouteSet>,
    cancellation: CancellationToken,
    config_digest: String,
) -> Result<(), String> {
    recover_members_every(
        pool,
        routes,
        cancellation,
        config_digest,
        Duration::from_secs(5),
    )
    .await
}

async fn recover_members_every(
    pool: PgPool,
    routes: Arc<RouteSet>,
    cancellation: CancellationToken,
    config_digest: String,
    period: Duration,
) -> Result<(), String> {
    let state = db::rpc::state(&pool, config_digest);
    let groups = groups(&routes)?;
    let watched = groups
        .values()
        .map(|(group, _)| group.clone())
        .collect::<Vec<_>>();
    let recovery = async {
        loop {
            tokio::select! {()=cancellation.cancelled()=>return Ok(()),()=tokio::time::sleep(period)=>{}}
            refresh_metrics_safely(db::rpc::refresh_metrics(&pool)).await;
            for (group, route_files) in groups.values() {
                for index in 0..group.members.len() {
                    if group.probe_due(index) {
                        let result = probe(group, index, route_files).await;
                        let known = sqlx::query_scalar::<_, String>(
                            r#"
                            SELECT genesis_hash
                            FROM rpc_member_validations
                            WHERE chain_id = $1
                            LIMIT 1
                            "#,
                        )
                        .bind(i64::try_from(group.chain).map_err(|e| e.to_string())?)
                        .fetch_optional(&pool)
                        .await;
                        let known = match known {
                            Ok(known) => known,
                            Err(_) => {
                                group.probe_result(index, false);
                                tracing::warn!(
                                    tags.alert = "TopupRpcRecoveryUnavailable",
                                    tags.group = %group.id,
                                    tags.chain = group.chain,
                                    "RPC recovery identity unavailable; member remains unverified"
                                );
                                continue;
                            }
                        };
                        group.probe_result(
                            index,
                            result.is_ok_and(|hash| known.as_ref() == Some(&hash)),
                        );
                    }
                }
            }
            if anchor_cursors(&pool, &routes, state.as_ref())
                .await
                .is_err()
            {
                tracing::warn!(
                    tags.alert = "TopupRpcAnchorUnavailable",
                    "RPC cursor anchor unavailable; safety gate remains closed"
                );
            }
        }
    };
    with_availability_monitor(&watched, &cancellation, recovery).await
}

/// Per-member network preflight, returning only stable ids. An offline backup is reported and
/// left unverified; at least one fully validated member must serve every group.
pub async fn preflight(routes: &RouteSet) -> Result<Vec<String>, String> {
    let mut healthy = Vec::new();
    let mut genesis = BTreeMap::new();
    let mut failures = Vec::new();
    for (group, files) in groups(routes)?.values() {
        let mut reasons = Vec::new();
        for index in 0..group.members.len() {
            group.verified(index, false);
            let member = group.members.get(index).ok_or("missing member")?;
            let result = probe(group, index, files).await.and_then(|hash| {
                if genesis.get(&group.chain).is_some_and(|old| old != &hash) {
                    group.failed(index, topup_adapters::chain::evm::group::Failure::Identity);
                    return Err("genesis: RPC member identity invalid (mismatch)".to_owned());
                }
                Ok(hash)
            });
            match result {
                Ok(hash) => {
                    genesis.insert(group.chain, hash);
                    group.verified(index, true);
                    healthy.push(member.id.clone());
                }
                Err(error) => {
                    tracing::warn!(group=%group.id, member=%member.id, %error, "RPC member preflight failed");
                    reasons.push(format!("{}: {error}", member.id));
                }
            }
        }
        if group.eligible() == 0 {
            failures.push(format!(
                "RPC group {} has no verified member [{}]",
                group.id,
                reasons.join("; ")
            ));
        }
    }
    if !failures.is_empty() {
        return Err(failures.join("; "));
    }
    Ok(healthy)
}
/// Pins a selected member's genesis to the database identity before standalone/recovery reads.
pub async fn verify_persisted_genesis(pool: &PgPool, routes: &RouteSet) -> Result<(), String> {
    for (group, _) in groups(routes)?.values() {
        let known: Option<String> = sqlx::query_scalar(
            "SELECT genesis_hash FROM rpc_member_validations WHERE chain_id=$1 LIMIT 1",
        )
        .bind(i64::try_from(group.chain).map_err(|e| e.to_string())?)
        .fetch_optional(pool)
        .await
        .map_err(|e| e.to_string())?;
        if let Some(known) = known {
            let index = group
                .select(&Default::default(), None)
                .map_err(|e| e.to_string())?;
            let deadline = Instant::now()
                .checked_add(Duration::from_millis(group.policy.total_deadline_ms))
                .unwrap_or_else(Instant::now);
            if block(group, index, 0, deadline).await?.hash != known {
                group.failed(index, topup_adapters::chain::evm::group::Failure::Identity);
                return Err("RPC genesis differs from the persisted chain identity".into());
            }
        }
    }
    Ok(())
}
/// Audited owner recovery verifies an agreed lower finalized anchor and repairs derived floors.
/// No runtime operation can lower a persisted watermark; this entry requires stopped writers.
pub async fn recover_watermark(
    pool: &PgPool,
    routes: &RouteSet,
    chain: u64,
    height: u64,
    actor: &str,
    reason: &str,
) -> Result<(), String> {
    let lock = crate::reconciler::store::exclusive_lease_owner_lock(pool)
        .await
        .map_err(|e| e.to_string())?;
    let result = async {
        check_roles(pool, routes).await?;
        let a = routes
            .provider(chain, 0)
            .map_err(|e| e.to_string())?
            .group()
            .ok_or("A group missing")?;
        let b = routes
            .provider(chain, 1)
            .map_err(|e| e.to_string())?
            .group()
            .ok_or("B group missing")?;
        let files = routes
            .routes()
            .iter()
            .filter(|r| r.chain.chain_id == chain)
            .collect::<Vec<_>>();
        let expected: Option<String> = sqlx::query_scalar(
            "SELECT genesis_hash FROM rpc_member_validations WHERE chain_id=$1 LIMIT 1",
        )
        .bind(i64::try_from(chain).map_err(|e| e.to_string())?)
        .fetch_optional(pool)
        .await
        .map_err(|e| e.to_string())?;
        // Probe below poisoned floors without publishing evidence. Each member gets a fresh
        // full-probe deadline; anchor operations start only after both groups finish probing.
        let valid_a = probe_recovery_members(a, &files, expected.as_deref()).await;
        let valid_b = probe_recovery_members(b, &files, expected.as_deref()).await;
        let ah = recovery_anchor(a, &valid_a, b, &valid_b, height).await?;
        db::rpc::recover(pool, chain, &ah, actor, reason)
            .await
            .map_err(|e| e.to_string())
    }
    .await;
    lock.release().await.map_err(|e| e.to_string())?;
    result
}
async fn probe_recovery_members(
    group: &Arc<RpcGroup>,
    files: &[&RouteFile],
    expected: Option<&str>,
) -> Vec<usize> {
    let mut verified = Vec::new();
    for index in 0..group.members.len() {
        let result = async {
            let copy = group.probe_copy().map_err(|e| e.to_string())?;
            let deadline = copy.probe_deadline().ok_or("missing probe deadline")?;
            tokio::time::timeout_at(deadline, probe_inner(&copy, index, files))
                .await
                .map_err(|_| "probe total deadline: RPC deadline expired".to_owned())?
        }
        .await;
        match result {
            Ok(hash) if expected.is_none_or(|v| v == hash) => verified.push(index),
            Ok(_) => {
                tracing::warn!(group=%group.id, member=%group.members[index].id, "recovery genesis mismatch")
            }
            Err(error) => {
                tracing::warn!(group=%group.id, member=%group.members[index].id, %error, "RPC recovery member probe failed")
            }
        }
    }
    verified
}

async fn recovery_anchor(
    a: &Arc<RpcGroup>,
    valid_a: &[usize],
    b: &Arc<RpcGroup>,
    valid_b: &[usize],
    height: u64,
) -> Result<HeadAnchor, String> {
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(
            a.policy.total_deadline_ms.min(b.policy.total_deadline_ms),
        ))
        .unwrap_or_else(Instant::now);
    let a = a.isolated_copy(deadline).map_err(|e| e.to_string())?;
    let b = b.isolated_copy(deadline).map_err(|e| e.to_string())?;
    for index in valid_a {
        a.verified(*index, true);
    }
    for index in valid_b {
        b.verified(*index, true);
    }
    let ai = a
        .select(&Default::default(), None)
        .map_err(|e| e.to_string())?;
    let bi = b
        .select(&Default::default(), None)
        .map_err(|e| e.to_string())?;
    tokio::time::timeout_at(deadline, async {
        if a.head(ai, "finalized", deadline)
            .await
            .map_err(|e| e.to_string())?
            .number
            < height
            || b.head(bi, "finalized", deadline)
                .await
                .map_err(|e| e.to_string())?
                .number
                < height
        {
            return Err("recovery anchor above finalized evidence".into());
        }
        let ah = block(&a, ai, height, deadline).await?;
        let bh = block(&b, bi, height, deadline).await?;
        if ah != bh || ah.number != height {
            return Err("RPC recovery requires A/B anchor agreement".into());
        }
        Ok(ah)
    })
    .await
    .map_err(|_| "recovery anchor deadline: RPC deadline expired".to_owned())?
}

/// Bounded stopped-service replay. Repeated invocations retain atomic address progress. Unfreeze
/// occurs only after all address backfills and every credited deposit's canonical block check.
pub async fn resume_recovery(
    pool: &PgPool,
    routes: &RouteSet,
    chain: u64,
    max_windows: u32,
) -> Result<bool, String> {
    use sqlx::Row;
    use topup_adapters::chain::evm::{ChainReader, FinalizedReader};
    let lock = crate::reconciler::store::exclusive_lease_owner_lock(pool)
        .await
        .map_err(|e| e.to_string())?;
    let result=async {
        let pending:bool=sqlx::query_scalar("SELECT frozen AND recovery_pending FROM rpc_chain_state WHERE chain_id=$1").bind(i64::try_from(chain).map_err(|e|e.to_string())?).fetch_one(pool).await.map_err(|e|e.to_string())?;
        if !pending {return Err("an audited recovery must be pending before replay/resume".to_owned());}
        preflight(routes).await?;
        verify_persisted_genesis(pool, routes).await?;
        let a=routes.provider(chain,0).map_err(|e|e.to_string())?.group().ok_or("A group missing")?;
        let b=routes.provider(chain,1).map_err(|e|e.to_string())?.group().ok_or("B group missing")?;
        let ai=a.select(&Default::default(),None).map_err(|e|e.to_string())?;let bi=b.select(&Default::default(),None).map_err(|e|e.to_string())?;
        let chain_db=i64::try_from(chain).map_err(|e|e.to_string())?;
        let evidence:serde_json::Value=sqlx::query_scalar("SELECT evidence FROM rpc_recoveries WHERE chain_id=$1 ORDER BY epoch DESC LIMIT 1").bind(chain_db).fetch_one(pool).await.map_err(|e|e.to_string())?;
        let anchor:HeadAnchor=serde_json::from_value(evidence.get("anchor").ok_or("missing audited recovery anchor")?.clone()).map_err(|e|e.to_string())?;
        let deadline=Instant::now().checked_add(Duration::from_millis(a.policy.total_deadline_ms.min(b.policy.total_deadline_ms))).unwrap_or_else(Instant::now);
        if block(a,ai,anchor.number,deadline).await?!=anchor||block(b,bi,anchor.number,deadline).await?!=anchor {return Err("audited anchor no longer agrees with A/B".into());}
        let reader=FinalizedReader::new(routes.provider(chain,0).map_err(|e|e.to_string())?.clone());
        let chain_routes=crate::scanner::chain_routes(routes).into_iter().find(|r|r.chain.chain_id==chain).ok_or("chain routes missing")?;
        let addresses=db::list_scan_addresses(pool,chain).await.map_err(|e|e.to_string())?;
        let from=addresses.iter().filter(|a|!a.backfilled).map(crate::db::ScanAddress::backfill_start).min();
        if let Some(mut from)=from {for _ in 0..max_windows {
            if from>anchor.number {break;}
            let to=from.saturating_add(1999).min(anchor.number);
            let selected=addresses.iter().filter(|a|!a.backfilled&&a.backfill_start()<=to).cloned().collect::<Vec<_>>();
            let request=crate::scanner::window_request(&chain_routes,&selected,from,to,true);
            let window=reader.read_window(&request).await.map_err(|e|e.to_string())?;
            let deposits=crate::scanner::resolve_logs_for_reconciliation(window.transfers,&selected,&chain_routes).map_err(|e|e.to_string())?;
            db::rpc::recovery_window(pool,chain,&deposits,&window.factory_logs,window.proof.as_ref().ok_or("missing replay provenance")?,&selected.iter().map(|a|a.id).collect::<Vec<_>>(),to).await.map_err(|e|e.to_string())?;
            from=to.checked_add(1).ok_or("replay block overflow")?;
        }}
        let incomplete:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM addresses WHERE chain_id=$1 AND NOT backfilled AND (backfilled_through IS NULL OR backfilled_through<$2))").bind(chain_db).bind(i64::try_from(anchor.number).map_err(|e|e.to_string())?).fetch_one(pool).await.map_err(|e|e.to_string())?;
        if incomplete {return Ok(false);}
        for row in sqlx::query("SELECT d.block_number,d.block_hash,d.block_time,d.tx_hash,d.receipt_log_index,d.tx_nonce::text AS tx_nonce,d.asset_contract,d.from_address,d.amount_atomic::text AS amount_atomic,a.address FROM deposits d JOIN addresses a ON a.id=d.address_id WHERE d.chain_id=$1 AND d.credit_minor IS NOT NULL AND d.state<>'reversed'").bind(chain_db).fetch_all(pool).await.map_err(|e|e.to_string())? {
            let number=u64::try_from(row.try_get::<i64,_>("block_number").map_err(|e|e.to_string())?).map_err(|e|e.to_string())?;
            let hash:String=row.try_get("block_hash").map_err(|e|e.to_string())?;
            let deadline=Instant::now().checked_add(Duration::from_millis(a.policy.total_deadline_ms.min(b.policy.total_deadline_ms))).unwrap_or_else(Instant::now);
            let ah=block(a,ai,number,deadline).await?;let bh=block(b,bi,number,deadline).await?;
            if ah!=bh||ah.hash!=hash {return Err("credited deposit branch requires explicit ledger reconciliation; chain stays frozen".into());}
            let tx_hash: alloy_primitives::B256 = row.try_get::<String,_>("tx_hash").map_err(|e|e.to_string())?.parse().map_err(|_|"invalid stored transaction hash")?;
            let receipt_index=u64::try_from(row.try_get::<i64,_>("receipt_log_index").map_err(|e|e.to_string())?).map_err(|e|e.to_string())?;
            let known=topup_adapters::chain::evm::KnownTransfer {block_hash:hash.parse().map_err(|_|"invalid stored block hash")?,block_time:row.try_get("block_time").map_err(|e|e.to_string())?,tx_nonce:row.try_get::<String,_>("tx_nonce").map_err(|e|e.to_string())?.parse().map_err(|_|"invalid stored transaction nonce")?};
            let mut evidence=None;
            for (group,index) in [(a,ai),(b,bi)] {
                if group.head(index,"finalized",deadline).await.map_err(|e|e.to_string())?.number<number {return Err("credited deposit not finalized on both groups".into());}
                let client=Arc::new(EvmClient::from_group(group.clone(),Some(index)).map_err(|e|e.to_string())?);
                let lookup=FinalizedReader::new(client).receipt_transfer_known(tx_hash,receipt_index,known).await.map_err(|e|e.to_string())?;
                let transfer=lookup.transfer().ok_or("credited deposit receipt transfer missing; chain stays frozen")?;
                if transfer.block_number!=number || transfer.block_hash!=known.block_hash
                    || format!("{:#x}",transfer.token)!=row.try_get::<String,_>("asset_contract").map_err(|e|e.to_string())?
                    || format!("{:#x}",transfer.from)!=row.try_get::<String,_>("from_address").map_err(|e|e.to_string())?
                    || format!("{:#x}",transfer.to)!=row.try_get::<String,_>("address").map_err(|e|e.to_string())?
                    || transfer.amount.value().to_string()!=row.try_get::<String,_>("amount_atomic").map_err(|e|e.to_string())?
                    || evidence.as_ref().is_some_and(|old| old!=transfer) { return Err("credited deposit receipt disagrees; chain stays frozen".into()); }
                evidence=Some(transfer.clone());
            }

        }
        for row in sqlx::query("SELECT block_number,block_hash FROM flushed WHERE chain_id=$1 UNION SELECT block_number,block_hash FROM flush_failures WHERE chain_id=$1").bind(chain_db).fetch_all(pool).await.map_err(|e|e.to_string())? {
            let number=u64::try_from(row.try_get::<i64,_>("block_number").map_err(|e|e.to_string())?).map_err(|e|e.to_string())?;
            let hash:String=row.try_get("block_hash").map_err(|e|e.to_string())?;
            let deadline=Instant::now().checked_add(Duration::from_millis(a.policy.total_deadline_ms.min(b.policy.total_deadline_ms))).unwrap_or_else(Instant::now);
            let ah=block(a,ai,number,deadline).await?;let bh=block(b,bi,number,deadline).await?;
            if ah!=bh||ah.hash!=hash {return Err("factory ledger branch requires explicit reconciliation; chain stays frozen".into());}
        }
        let mut tx=pool.begin().await.map_err(|e|e.to_string())?;
        sqlx::query("UPDATE addresses SET backfilled=true WHERE chain_id=$1").bind(chain_db).execute(&mut *tx).await.map_err(|e|e.to_string())?;
        for (group,index) in [(a,ai),(b,bi)] {
            sqlx::query("INSERT INTO rpc_watermarks(chain_id,group_id,tag,number,hash,parent_hash,member_id,config_digest,epoch) SELECT $1,$2,'cursor',$3,$4,$5,$6,$7,epoch FROM rpc_chain_state WHERE chain_id=$1")
                .bind(chain_db).bind(&group.id).bind(i64::try_from(anchor.number).map_err(|e|e.to_string())?).bind(&anchor.hash).bind(&anchor.parent_hash)
                .bind(&group.members.get(index).ok_or("missing recovery member")?.id).bind("audited-recovery").execute(&mut *tx).await.map_err(|e|e.to_string())?;
        }
        sqlx::query("UPDATE rpc_chain_state SET frozen=false,recovery_pending=false,awaiting_anchor=false,reason=NULL WHERE chain_id=$1 AND frozen AND recovery_pending").bind(chain_db).execute(&mut *tx).await.map_err(|e|e.to_string())?;
        tx.commit().await.map_err(|e|e.to_string())?;Ok(true)
    }.await;
    lock.release().await.map_err(|e| e.to_string())?;
    result
}

// Metric collection is observational: preserve the last snapshot and continue recovery.
pub(crate) async fn refresh_metrics_safely(refresh: impl Future<Output = Result<(), sqlx::Error>>) {
    if refresh.await.is_err() {
        tracing::warn!(
            tags.alert = "TopupRpcMetricsRefreshFailed",
            "RPC metrics refresh failed; recovery continues"
        );
    }
}

// Poll recovery and health independently; cancellation drops blocked DB/HTTP work as well.
// This owned future pair avoids detached tasks and shares the worker's lifetime.
pub(crate) async fn with_availability_monitor(
    groups: &[Arc<RpcGroup>],
    cancellation: &CancellationToken,
    recovery: impl Future<Output = Result<(), String>>,
) -> Result<(), String> {
    let availability = async {
        let mut unavailable_since = BTreeMap::new();
        let mut ticker = tokio::time::interval(Duration::from_secs(5));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = cancellation.cancelled() => return,
                _ = ticker.tick() => {
                    for group in groups {
                        report_group_availability(&group.id, group.chain, group.serving_members() > 0,
                            &mut unavailable_since, Instant::now());
                    }
                }
            }
        }
    };
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Ok(()),
        () = availability => Ok(()),
        result = recovery => result,
    }
}

/// Pages through Sentry only after a group has no serving candidate for one minute.
pub(crate) fn report_group_availability(
    group: &str,
    chain: u64,
    serving: bool,
    unavailable_since: &mut BTreeMap<String, Instant>,
    now: Instant,
) {
    if serving {
        unavailable_since.remove(group);
        return;
    }
    let since = unavailable_since.entry(group.to_owned()).or_insert(now);
    if now.saturating_duration_since(*since) >= Duration::from_secs(60) {
        tracing::warn!(
            tags.alert = "TopupRpcGroupUnavailable",
            tags.group = group,
            tags.chain = chain,
            "RPC group has had no serving member for one minute"
        );
    }
}

#[cfg(test)]
mod observability_tests {
    use super::*;
    #[tokio::test]
    async fn failed_metrics_refresh_keeps_recovery_running_until_cancellation() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1/unused")
            .unwrap();
        pool.close().await;
        let snapshot = db::rpc::metrics();
        let routes = Arc::new(RouteSet::with_providers(Vec::new(), &BTreeMap::new()).unwrap());
        let cancellation = CancellationToken::new();
        let cancel = cancellation.clone();
        let task = tokio::spawn(recover_members_every(
            pool,
            routes,
            cancellation,
            "test".into(),
            Duration::from_millis(5),
        ));
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(
            !task.is_finished(),
            "metrics failure stopped the recovery worker"
        );
        cancel.cancel();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .is_ok()
        );
        assert_eq!(db::rpc::metrics(), snapshot);
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;
    use alloy::providers::bindings::IMulticall3::{Result as Call3Result, aggregate3Call};
    use alloy::sol_types::SolCall;
    use alloy_primitives::{Address, Bytes, U256};
    use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::post};
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use topup_adapters::{
        chain::evm::{
            MULTICALL3,
            group::{
                GroupPolicy, Member,
                budget::{BudgetSpec, Budgets},
            },
        },
        redaction::Redacted,
    };

    struct MockProbe {
        routes: Vec<RouteFile>,
        finalized_reads: AtomicUsize,
        oracle_reads: AtomicUsize,
        logs: Mutex<Vec<serde_json::Value>>,
        numeric_calls: Mutex<Vec<serde_json::Value>>,
        regress_finalized: bool,
        throttle_oracle: bool,
        reject_wide_logs: bool,
        block_delay: Duration,
        price_only: bool,
        price_call_failure: bool,
        receipt_reads: AtomicUsize,
    }
    impl MockProbe {
        fn new() -> Self {
            let config = crate::config::Config::parse(include_str!(
                "../../../deploy/environments/phala-network/staging/topup/topup.yaml"
            ))
            .unwrap();
            Self {
                routes: config
                    .routes
                    .into_iter()
                    .filter(|r| r.chain.chain_id == 11155111)
                    .collect(),
                finalized_reads: AtomicUsize::new(0),
                oracle_reads: AtomicUsize::new(0),
                logs: Mutex::new(Vec::new()),
                numeric_calls: Mutex::new(Vec::new()),
                regress_finalized: false,
                throttle_oracle: false,
                reject_wide_logs: false,
                block_delay: Duration::ZERO,
                price_only: false,
                price_call_failure: false,
                receipt_reads: AtomicUsize::new(0),
            }
        }
        fn response(&self, request: &serde_json::Value) -> axum::response::Response {
            let first = &self.routes[0];
            let contracts = &first.chain.contracts;
            let params = &request["params"];
            let result = match request["method"].as_str().unwrap() {
                "eth_chainId" => json!(if self.price_only { "0x1" } else { "0xaa36a7" }),
                "eth_getTransactionReceipt" => {
                    self.receipt_reads.fetch_add(1, Ordering::SeqCst);
                    if self.price_only {
                        return StatusCode::FORBIDDEN.into_response();
                    }
                    serde_json::Value::Null
                }
                "eth_getBlockByNumber" => {
                    let tag = params[0].as_str().unwrap();
                    let number = match tag {
                        "finalized" => {
                            let read = self.finalized_reads.fetch_add(1, Ordering::SeqCst);
                            if self.regress_finalized && read > 0 {
                                1999
                            } else {
                                2000
                            }
                        }
                        "latest" => 3000,
                        "safe" => 2500,
                        _ => u64::from_str_radix(tag.strip_prefix("0x").unwrap(), 16).unwrap(),
                    };
                    let mut block = serde_json::to_value(alloy::rpc::types::Block::<
                        alloy::rpc::types::Transaction,
                    >::default())
                    .unwrap();
                    block["number"] = json!(format!("0x{number:x}"));
                    block["hash"] = json!(format!("0x{}", "11".repeat(32)));
                    block["parentHash"] = json!(format!("0x{}", "22".repeat(32)));
                    block
                }
                "eth_getCode" => {
                    let address: Address = serde_json::from_value(params[0].clone()).unwrap();
                    if address == MULTICALL3 {
                        let recorded: serde_json::Value = serde_json::from_str(include_str!(
                            "../../../deploy/contracts/multicall3.json"
                        ))
                        .unwrap();
                        recorded["runtime_code"].clone()
                    } else if address == contracts.forwarder_factory
                        || address == contracts.implementation
                    {
                        // Recorded Foundry runtime templates; production verification checks their
                        // audited hashes and the exact immutable addresses before any capability.
                        let recorded: serde_json::Value = serde_json::from_str(include_str!(
                            "../tests/fixtures/rpc-probe-contract-code.json"
                        ))
                        .unwrap();
                        let (name, immutable, offsets): (&str, Address, &[usize]) =
                            if address == contracts.forwarder_factory {
                                (
                                    "ForwarderFactory",
                                    contracts.implementation,
                                    &[105, 480, 606, 887],
                                )
                            } else {
                                ("Forwarder", contracts.forwarder_factory, &[208, 319])
                            };
                        let mut code = hex::decode(
                            recorded[name].as_str().unwrap().strip_prefix("0x").unwrap(),
                        )
                        .unwrap();
                        for offset in offsets {
                            code[*offset..*offset + 32]
                                .copy_from_slice(immutable.into_word().as_slice());
                        }
                        json!(format!("0x{}", hex::encode(code)))
                    } else {
                        json!("0x6000")
                    }
                }
                "eth_getLogs" => {
                    self.logs.lock().unwrap().push(params[0].clone());
                    if self.reject_wide_logs {
                        return Json(json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32005,"message":"block range too wide"}})).into_response();
                    }
                    json!([])
                }
                "eth_call" => {
                    let to: Address = serde_json::from_value(params[0]["to"].clone()).unwrap();
                    if self.price_only {
                        if params[1] != "latest" {
                            return StatusCode::FORBIDDEN.into_response();
                        }
                        assert_eq!(
                            to.to_string().to_lowercase(),
                            topup_core::price::feed("USDT_USD", 1)
                                .unwrap()
                                .address
                                .to_lowercase()
                        );
                        if self.price_call_failure {
                            return StatusCode::SERVICE_UNAVAILABLE.into_response();
                        }
                        json!(format!(
                            "0x{}",
                            hex::encode(U256::from(8).to_be_bytes::<32>())
                        ))
                    } else if to == MULTICALL3 {
                        let input: Bytes = serde_json::from_value(
                            params[0].get("input").unwrap_or(&params[0]["data"]).clone(),
                        )
                        .unwrap();
                        let calls = aggregate3Call::abi_decode(&input).unwrap().calls;
                        assert_eq!(calls.len(), 1);
                        assert_eq!(calls[0].target, contracts.forwarder_factory);
                        let derived = topup_core::address::forwarder_address(
                            contracts.forwarder_factory,
                            contracts.implementation,
                            crate::contracts::sample_treasury(),
                            crate::contracts::sample_salt(),
                        );
                        json!(Bytes::from(aggregate3Call::abi_encode_returns(&vec![
                            Call3Result {
                                success: true,
                                returnData: derived.into_word().as_slice().to_vec().into()
                            }
                        ])))
                    } else if to == contracts.forwarder_factory {
                        json!(contracts.implementation.into_word())
                    } else if to == contracts.implementation {
                        json!(contracts.forwarder_factory.into_word())
                    } else {
                        self.numeric_calls.lock().unwrap().push(params[1].clone());
                        let route = self
                            .routes
                            .iter()
                            .find(|r| r.asset.contract == to || r.screening.sanctions_oracle == to)
                            .unwrap();
                        let value = if to == route.screening.sanctions_oracle {
                            if self.oracle_reads.fetch_add(1, Ordering::SeqCst) == 0
                                && self.throttle_oracle
                            {
                                return (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "1")], Json(json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32005,"message":"quota exceeded"}}))).into_response();
                            }
                            0
                        } else {
                            route.asset.decimals
                        };
                        json!(format!(
                            "0x{}",
                            hex::encode(U256::from(value).to_be_bytes::<32>())
                        ))
                    }
                }
                _ => panic!("unexpected method"),
            };
            Json(json!({"jsonrpc":"2.0","id":request["id"],"result":result})).into_response()
        }
    }
    async fn server(mock: Arc<MockProbe>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/",
                    post(move |Json(request): Json<serde_json::Value>| {
                        let mock = mock.clone();
                        async move {
                            if request["method"] == "eth_getBlockByNumber" {
                                tokio::time::sleep(mock.block_delay).await;
                            }
                            mock.response(&request)
                        }
                    }),
                ),
            )
            .await
            .unwrap();
        });
        (url, task)
    }
    fn group(url: &str, id: &str, rps: u32, policy: GroupPolicy) -> Arc<RpcGroup> {
        group_on_chain(url, id, rps, policy, 11155111)
    }
    fn group_on_chain(
        url: &str,
        id: &str,
        rps: u32,
        policy: GroupPolicy,
        chain: u64,
    ) -> Arc<RpcGroup> {
        let spec = BudgetSpec {
            requests_per_second: rps,
            burst: if rps == 2 { 2 } else { 100 },
        };
        let budgets = Arc::new(
            Budgets::new(&BTreeMap::from([
                ("account".into(), spec.clone()),
                ("key".into(), spec),
            ]))
            .unwrap(),
        );
        RpcGroup::new(
            id.into(),
            chain,
            policy,
            vec![Member {
                id: "one".into(),
                company: "company".into(),
                endpoint: Redacted::parse(url).unwrap(),
                account: "account".into(),
                key: "key".into(),
                priority: 0,
                weight: 1,
            }],
            budgets,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn ensure_anchors_names_an_unbound_observation_group() {
        let pool = PgPool::connect_lazy("postgres://unused:unused@127.0.0.1/unused").unwrap();
        let state = db::rpc::state(&pool, db::rpc::digest("cfg"));
        let a = group(
            "http://127.0.0.1:1",
            "provider-a",
            100,
            GroupPolicy::default(),
        );
        let b = group(
            "http://127.0.0.1:1",
            "provider-b",
            100,
            GroupPolicy::default(),
        );
        let observation = group_on_chain(
            "http://127.0.0.1:1",
            "base-mainnet-a",
            100,
            GroupPolicy::default(),
            8453,
        );
        a.set_store(state.clone());
        b.set_store(state);
        let mut route: RouteFile =
            serde_saphyr::from_str(include_str!("../tests/fixtures/phala-cloud-pha.yaml")).unwrap();
        route.chain.chain_id = 11155111;
        route.livemode = false;
        route.chain.rpc_providers = vec![a.id.clone(), b.id.clone()];
        let routes = RouteSet::with_groups(
            vec![route],
            BTreeMap::from([
                (
                    a.id.clone(),
                    Arc::new(EvmClient::from_group(a, None).unwrap()),
                ),
                (
                    b.id.clone(),
                    Arc::new(EvmClient::from_group(b, None).unwrap()),
                ),
                (
                    observation.id.clone(),
                    Arc::new(EvmClient::from_group(observation, None).unwrap()),
                ),
            ]),
        )
        .unwrap();
        assert_eq!(
            ensure_anchors(&pool, &routes).await.unwrap_err(),
            "RPC durable safety store missing for group base-mainnet-a"
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn observation_groups_require_feed_calls_but_not_receipt_access() {
        for price_call_failure in [false, true] {
            let mock = Arc::new(MockProbe {
                price_only: true,
                price_call_failure,
                ..MockProbe::new()
            });
            let (url, task) = server(mock.clone()).await;
            let group = group_on_chain(&url, "mainnet-price", 100, GroupPolicy::default(), 1);
            let result = probe(&group, 0, &[]).await;
            task.abort();
            assert_eq!(result.is_ok(), !price_call_failure, "{result:?}");
            assert_eq!(mock.receipt_reads.load(Ordering::SeqCst), 0);
            assert!(mock.logs.lock().unwrap().is_empty());
        }
    }
    #[tokio::test]
    async fn acceptance_uses_one_finalized_snapshot_for_numeric_capabilities() {
        let mock = Arc::new(MockProbe {
            regress_finalized: true,
            ..MockProbe::new()
        });
        let (url, task) = server(mock.clone()).await;
        let group = group(&url, "provider-a", 100, GroupPolicy::default());
        let files = mock.routes.iter().collect::<Vec<_>>();
        let result = probe(&group, 0, &files).await;
        task.abort();
        assert_eq!(result.unwrap(), format!("0x{}", "11".repeat(32)));
        assert_eq!(mock.finalized_reads.load(Ordering::SeqCst), 1);
        let calls = mock.numeric_calls.lock().unwrap();
        assert_eq!(calls.len(), 6);
        assert!(calls.iter().all(|tag| tag == "0x7d0"));
        let logs = mock.logs.lock().unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0]["fromBlock"], "0x1");
        assert_eq!(logs[0]["toBlock"], "0x7d0");
        assert!(logs[0].get("address").is_none());
        assert_eq!(group.eligible(), 0);
    }
    #[tokio::test]
    async fn real_route_oracle_throttle_retries_until_valid_numeric_result() {
        let mock = Arc::new(MockProbe {
            throttle_oracle: true,
            ..MockProbe::new()
        });
        let (url, task) = server(mock.clone()).await;
        let group = group(&url, "provider-b", 100, GroupPolicy::default());
        let result = probe(&group, 0, &mock.routes.iter().collect::<Vec<_>>()).await;
        task.abort();
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(mock.oracle_reads.load(Ordering::SeqCst), 4);
        assert_eq!(mock.logs.lock().unwrap().len(), 3);
        assert!(
            mock.numeric_calls
                .lock()
                .unwrap()
                .iter()
                .all(|tag| tag == "0x7d0")
        );
    }
    #[tokio::test]
    async fn real_route_a_rejects_unsplit_log_range_without_retry_or_admission() {
        let mock = Arc::new(MockProbe {
            reject_wide_logs: true,
            ..MockProbe::new()
        });
        let (url, task) = server(mock.clone()).await;
        let group = group(&url, "provider-a", 100, GroupPolicy::default());
        let result = probe(&group, 0, &mock.routes.iter().collect::<Vec<_>>()).await;
        task.abort();
        assert!(
            result
                .unwrap_err()
                .contains("address-less transfer logs (2000 blocks): RPC log window too large")
        );
        assert_eq!(mock.logs.lock().unwrap().len(), 1);
        assert_eq!(mock.oracle_reads.load(Ordering::SeqCst), 0);
        assert_eq!(group.eligible(), 0);
    }
    #[tokio::test]
    async fn slow_recovery_has_fresh_member_and_anchor_deadlines() {
        let mock = Arc::new(MockProbe::new());
        let (url, task) = server(mock.clone()).await;
        // Three real routes at ethPandaOps's 2 RPS/burst 2 exceed the old 10 s wrapper.
        let a = group(&url, "provider-a", 100, GroupPolicy::default());
        let b = group(&url, "provider-b", 2, GroupPolicy::default());
        let files = mock.routes.iter().collect::<Vec<_>>();
        let start = Instant::now();
        let valid_a = probe_recovery_members(&a, &files, None).await;
        let valid_b = probe_recovery_members(&b, &files, None).await;
        assert!(start.elapsed() > Duration::from_secs(10));
        assert_eq!(valid_a, vec![0]);
        assert_eq!(valid_b, vec![0]);
        assert_eq!(
            recovery_anchor(&a, &valid_a, &b, &valid_b, 2000)
                .await
                .unwrap()
                .number,
            2000
        );
        // Also outlive the original copies' probe deadlines before anchor creation.
        let mut short = GroupPolicy::default();
        short.probe.deadline = 1000;
        short.attempt_timeout_ms = 100;
        let fresh_a = group(&url, "provider-a", 100, short.clone());
        let fresh_b = group(&url, "provider-b", 100, short);
        let expired_a = fresh_a.probe_copy().unwrap();
        let expired_b = fresh_b.probe_copy().unwrap();
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let anchor = recovery_anchor(&expired_a, &valid_a, &expired_b, &valid_b, 2000)
            .await
            .unwrap();
        assert_eq!(anchor.number, 2000);
        assert_eq!(anchor.hash, format!("0x{}", "11".repeat(32)));
        task.abort();
    }
    #[tokio::test]
    async fn recovery_anchor_uses_full_operation_budget_beyond_probe_deadline() {
        let mock = Arc::new(MockProbe {
            block_delay: Duration::from_millis(350),
            ..MockProbe::new()
        });
        let (url, task) = server(mock).await;
        let mut policy = GroupPolicy::default();
        policy.probe.deadline = 1000;
        policy.attempt_timeout_ms = 800;
        // The four sequential anchor RPCs take >= 1.4 s, within the independent 10 s budget.
        let a = group(&url, "provider-a", 100, policy.clone());
        let b = group(&url, "provider-b", 100, policy);
        let started = Instant::now();
        let result = recovery_anchor(&a, &[0], &b, &[0], 2000).await;
        task.abort();
        assert!(started.elapsed() > Duration::from_secs(1));
        assert_eq!(result.unwrap().number, 2000);
    }
}
