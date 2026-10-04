//! Architecture §4 startup contract checks against a real factory deployment on Anvil.

mod support;

use std::process::Command;
use std::str::FromStr;

use alloy_primitives::Address;
use anyhow::{Context, Result, ensure};
use support::chain::{Anvil, forge_create, run_checked};
use topup_core::route::RouteFile;

const TREASURY: &str = "0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc";
const FIXTURE: &str = include_str!("fixtures/phala-cloud-pha.yaml");

#[tokio::test]
async fn run_refuses_to_start_when_the_contracts_differ_from_the_route_or_build() -> Result<()> {
    let Some(anvil) = Anvil::start_if_available(&[]).await? else {
        return Ok(());
    };
    let rpc_url = anvil.rpc_url.clone();
    let factory = forge_create(&rpc_url, "src/ForwarderFactory.sol:ForwarderFactory", &[])?;
    let implementation = implementation_of(&rpc_url, factory)?;

    let yaml = route_yaml(&anvil, factory, TREASURY);
    let route: RouteFile = serde_saphyr::from_str(&yaml)?;
    route.validate().map_err(anyhow::Error::msg)?;
    ensure!(
        route.chain.contracts.implementation == implementation,
        "the default implementation must be the one the factory created"
    );
    topup::contracts::verify_routes(&route_set(&route)?)
        .await
        .map_err(anyhow::Error::msg)
        .context("the deployed contracts must match their own route")?;

    let mut wrong_implementation = route.clone();
    wrong_implementation.chain.contracts.implementation = Address::from_str(TREASURY)?;
    let error = topup::contracts::verify_routes(&route_set(&wrong_implementation)?)
        .await
        .expect_err("a wrong implementation must fail");
    ensure!(error.contains("implementation()"), "{error}");

    // Correct getters but different runtime code: one unreachable byte appended to each contract.
    for (contract, name) in [(factory, "factory"), (implementation, "implementation")] {
        let original = cast(&["code", &format!("{contract:#x}"), "--rpc-url", &rpc_url])?;
        set_code(&rpc_url, contract, &format!("{original}00"))?;
        ensure!(
            implementation_of(&rpc_url, factory)? == implementation,
            "the getters must still answer"
        );
        let error = topup::contracts::verify_routes(&route_set(&route)?)
            .await
            .expect_err("modified code must fail");
        ensure!(
            error.contains(name) && error.contains("differs from the recorded code hash"),
            "{error}"
        );
        set_code(&rpc_url, contract, &original)?;
    }
    topup::contracts::verify_routes(&route_set(&route)?)
        .await
        .map_err(anyhow::Error::msg)
        .context("restored code must pass again")?;

    // Balance and addressOf reads are aggregated through Multicall3: without the canonical
    // deployment the service must refuse to start rather than fail every read later.
    let multicall = Address::from_str("0xcA11bde05977b3631167028862bE2a173976CA11")?;
    let canonical = cast(&["code", &format!("{multicall:#x}"), "--rpc-url", &rpc_url])?;
    for (code, expected) in [
        ("0x", "has no code on chain"),
        ("0x00", "is not the canonical deployment"),
    ] {
        set_code(&rpc_url, multicall, code)?;
        let error = topup::contracts::verify_routes(&route_set(&route)?)
            .await
            .expect_err("a missing or different Multicall3 must fail");
        ensure!(
            error.contains("Multicall3") && error.contains(expected),
            "{error}"
        );
    }
    set_code(&rpc_url, multicall, &canonical)?;
    topup::contracts::verify_routes(&route_set(&route)?)
        .await
        .map_err(anyhow::Error::msg)
        .context("the canonical Multicall3 must pass again")?;

    // `topup run` refuses a factory whose code is not the recorded build.
    let original = cast(&["code", &format!("{factory:#x}"), "--rpc-url", &rpc_url])?;
    set_code(&rpc_url, factory, &format!("{original}00"))?;
    let path = std::env::temp_dir().join(format!("topup-startup-{}.yaml", uuid::Uuid::new_v4()));
    std::fs::write(&path, config_yaml(&anvil, factory, TREASURY))?;
    let output = Command::new(env!("CARGO_BIN_EXE_topup"))
        .args(["run", "--config"])
        .arg(&path)
        .env_clear()
        // Complete runtime configuration, with a database nobody listens on: the contract check
        // must refuse before the service connects to it.
        .env("DATABASE_URL", "postgres://topup_service@127.0.0.1:1/topup")
        .output();
    std::fs::remove_file(&path)?;
    let output = output.context("start topup run")?;
    let logs = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(!output.status.success(), "run must refuse to start: {logs}");
    ensure!(
        logs.contains("failed to connect to database"),
        "run must refuse without durable acceptance state: {logs}"
    );
    Ok(())
}

fn route_set(route: &RouteFile) -> Result<topup::routes::RouteSet> {
    topup::routes::RouteSet::new(vec![route.clone()]).map_err(anyhow::Error::msg)
}

fn route_yaml(anvil: &Anvil, factory: Address, treasury: &str) -> String {
    // Two distinct provider entries for the same node, as route validation requires.
    let primary = anvil.rpc_url.clone();
    let secondary = primary.replace("127.0.0.1", "localhost");
    FIXTURE
        .replace(
            "rpc_groups: { a: alchemy, b: quicknode }",
            &format!("rpc_groups: {{ a: \"{primary}\", b: \"{secondary}\" }}"),
        )
        .replace(
            "0xe8A9Ab1AbC7651A5b7C2ED5B662F2f80BF5C446d",
            &format!("{factory:#x}"),
        )
        .replace("0x0000000000000000000000000000000000007EA5", treasury)
}

/// The service configuration of `route_yaml`, its two providers configured by id.
fn config_yaml(anvil: &Anvil, factory: Address, treasury: &str) -> String {
    let primary = anvil.rpc_url.clone();
    let secondary = primary.replace("127.0.0.1", "localhost");
    let route = FIXTURE
        .replace(
            "0xe8A9Ab1AbC7651A5b7C2ED5B662F2f80BF5C446d",
            &format!("{factory:#x}"),
        )
        .replace("0x0000000000000000000000000000000000007EA5", treasury)
        .lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "environment: staging\npublic_origin: http://127.0.0.1:8080\nadmin_key:\n  id: admin/v1\n  \
         public_key: 11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=\n{rpc}\nroutes:\n  -\n{route}\n",
        rpc = include_str!("fixtures/rpc-groups.yaml")
            .replace("https://eth-mainnet.g.alchemy.com/v2/{key}", &primary)
            .replace("https://rpc.example/eth", &secondary)
            .replace("alchemy.com", "127.0.0.1")
            .replace("rpc.example", "localhost")
            .replace("        sealed_key: TOPUP_RPC_ALCHEMY_KEY\n", "")
            .replace("https://rpc.example/eth", &secondary)
    )
}

fn implementation_of(rpc_url: &str, factory: Address) -> Result<Address> {
    let output = cast(&[
        "call",
        &format!("{factory:#x}"),
        "implementation()(address)",
        "--rpc-url",
        rpc_url,
    ])?;
    Ok(Address::from_str(&output)?)
}

fn set_code(rpc_url: &str, contract: Address, code: &str) -> Result<()> {
    cast(&[
        "rpc",
        "--rpc-url",
        rpc_url,
        "anvil_setCode",
        &format!("{contract:#x}"),
        code,
    ])
    .map(drop)
}

fn cast(arguments: &[&str]) -> Result<String> {
    let output = run_checked("cast", arguments, None)?;
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[tokio::test]
async fn rpc_first_acceptance_tolerates_backup_and_restart_preserves_legacy_hash_anchor()
-> Result<()> {
    use support::TestDatabase;
    use topup::{config::Config, db, rpc_runtime};
    use topup_adapters::chain::evm::group::WatermarkStore;
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let Some(anvil) = Anvil::start_if_available(&[]).await? else {
            return Ok(());
        };
        let factory = forge_create(
            &anvil.rpc_url,
            "src/ForwarderFactory.sol:ForwarderFactory",
            &[],
        )?;
        let implementation = implementation_of(&anvil.rpc_url, factory)?;
        let token = forge_create(&anvil.rpc_url, "test/mocks/MockTokens.sol:MockERC20", &[])?;
        let oracle = forge_create(
            &anvil.rpc_url,
            "test/mocks/MockSanctionsOracle.sol:MockSanctionsOracle",
            &[],
        )?;
        // Anvil finalized/safe tags lag latest; capability calls use the finalized block.
        cast(&["rpc", "anvil_mine", "0x80", "--rpc-url", &anvil.rpc_url])?;
        let yaml = config_yaml(&anvil, factory, TREASURY);
        let mut config = Config::parse(&yaml).map_err(anyhow::Error::msg)?;
        for route in &mut config.routes {
            route.chain.chain_id = 31337;
            route.chain.contracts.implementation = implementation;
            route.livemode = false;
            route.asset.contract = token;
            route.screening.sanctions_oracle = oracle;
        }
        for group in config.rpc_groups.values_mut() {
            group.chain_id = 31337;
        }
        let backup = config.rpc_groups.get_mut("alchemy").context("A")?;
        backup.policy.attempt_timeout_ms = 1000;
        backup.policy.total_deadline_ms = 15_000;
        let mut member = backup.members[0].clone();
        member.id = "offline-backup".into();
        member.url = "http://127.0.0.1:1".into();
        backup.members.push(member);
        let routes = config.route_set().map_err(anyhow::Error::msg)?;
        // 0.6 stored only a height. 0.7 must acquire its first independent hash before progress.
        db::initialize_cursor(&database.app_pool, 31337, 1, chrono::Utc::now()).await?;
        let public = config.resolved_json().map_err(anyhow::Error::msg)?;
        rpc_runtime::accept(&database.app_pool, &routes, &public)
            .await
            .map_err(anyhow::Error::msg)?;
        let state = db::rpc::state(&database.app_pool, db::rpc::digest(&public));
        let a = state
            .load(31337, "alchemy", "cursor")
            .await?
            .context("A anchor")?;
        let b = state
            .load(31337, "quicknode", "cursor")
            .await?
            .context("B anchor")?;
        ensure!(a == b && a.number == 1);
        ensure!(
            routes
                .provider(31337, 0)?
                .group()
                .context("A client")?
                .eligible()
                == 1
        );
        // Accepted roles cannot be renamed, even with the same company and URLs.
        let mut renamed = config.clone();
        let group = renamed.rpc_groups.remove("alchemy").context("A group")?;
        renamed.rpc_groups.insert("renamed-a".into(),group);
        for route in &mut renamed.routes { route.chain.rpc_providers[0]="renamed-a".into(); }
        let renamed_routes=renamed.route_set().map_err(anyhow::Error::msg)?;
        let renamed_public=renamed.resolved_json().map_err(anyhow::Error::msg)?;
        let error=rpc_runtime::accept(&database.app_pool,&renamed_routes,&renamed_public).await.expect_err("role renaming cannot reset floors");
        ensure!(error.contains("identity cannot change"),"{error}");

        // Numeric poison alone is recoverable through the stopped-service owner entry point.
        // Retain a wrong-branch old review and a poisoned issuance/backfill position for audit.
        let (_, customer)=support::seed::create_account_and_customer(&database.app_pool,&support::seed::NewAccount::named("numeric recovery"),"recovery-customer").await?;
        let address_id=uuid::Uuid::new_v4();
        support::seed::insert_address(&database.app_pool,&support::seed::NewAddress{id:address_id,customer_id:customer.id,chain_id:31337,route:config.routes[0].route.clone(),salt:alloy_primitives::B256::repeat_byte(7),address:Address::repeat_byte(7)}).await?;
        // Populate real deposit and factory ledgers before poisoning derived progress.
        let treasury=format!("{:#x}",support::seed::FIXTURE_TREASURY);
        let salt=format!("{:#x}",alloy_primitives::B256::repeat_byte(7));
        let forwarder=cast(&["call",&format!("{factory:#x}"),"addressOf(address,bytes32)(address)",&treasury,&salt,"--rpc-url",&anvil.rpc_url])?.to_ascii_lowercase();
        sqlx::query("UPDATE addresses SET address=$2 WHERE id=$1").bind(address_id).bind(&forwarder).execute(&database.owner_pool).await?;
        let sender="0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266";
        cast(&["send",&format!("{token:#x}"),"mint(address,uint256)",&forwarder,"100","--unlocked","--from",sender,"--rpc-url",&anvil.rpc_url])?;
        cast(&["send",&format!("{factory:#x}"),"flush(address,bytes32[],address)",&treasury,&format!("[{salt}]"),&format!("{token:#x}"),"--unlocked","--from",sender,"--rpc-url",&anvil.rpc_url])?;
        cast(&["rpc","anvil_mine","0x80","--rpc-url",&anvil.rpc_url])?;
        let reader=topup_adapters::chain::evm::FinalizedReader::new(routes.provider(31337,0)?.clone());
        topup::scanner::scan_once(&database.app_pool,&reader,&topup::scanner::chain_routes(&routes)[0]).await?;
        let deposit_hash:String=sqlx::query_scalar("SELECT block_hash FROM deposits WHERE address_id=$1").bind(address_id).fetch_one(&database.app_pool).await?;
        let flush_hash:String=sqlx::query_scalar("SELECT block_hash FROM flushed WHERE address_id=$1").bind(address_id).fetch_one(&database.app_pool).await?;
        sqlx::query("UPDATE deposits SET state='credited',credit_minor=1 WHERE address_id=$1").bind(address_id).execute(&database.owner_pool).await?;
        let poison=1_000_000_i64;
        sqlx::query("UPDATE cursors SET scanned_block=$1,confirmed_block=$1 WHERE chain_id=31337").bind(poison).execute(&database.owner_pool).await?;
        sqlx::query("UPDATE addresses SET created_block=$1,backfilled=true,backfilled_through=$1 WHERE chain_id=31337").bind(poison).execute(&database.owner_pool).await?;
        sqlx::query("UPDATE rpc_watermarks SET number=$1 WHERE chain_id=31337").bind(poison).execute(&database.owner_pool).await?;
        let old=topup_adapters::chain::evm::window::WindowProof {group:"alchemy".into(),member:config.rpc_groups["alchemy"].members[0].id.clone(),request:topup_adapters::chain::evm::window::WindowRequest{from:0,to:1,recipients:vec![Address::repeat_byte(7)],tokens:vec![],factory:None,finalized:true,exclude_member:None},end_hash:format!("0x{}","77".repeat(32))};
        db::rpc::commit_window(&database.app_pool,31337,&[],&[],Some(&old),Default::default()).await?;
        // A failing first member advertises a huge head, then fails its log capability.
        // Its temporary head must not make the healthy backup stale during owner recovery.
        let target=anvil.rpc_url.clone();
        let probe_heads=std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probe_failures=std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let heads=probe_heads.clone();let failures=probe_failures.clone();
        let router=axum::Router::new().route("/",axum::routing::post(move |axum::Json(request):axum::Json<serde_json::Value>| {
            let target=target.clone();let heads=heads.clone();let failures=failures.clone();async move {
                if request["method"]=="eth_getLogs" {failures.fetch_add(1,std::sync::atomic::Ordering::SeqCst);return axum::Json(serde_json::json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32603,"message":"capability fails after heads"}}));}
                let mut response:serde_json::Value=reqwest::Client::new().post(target).json(&request).send().await.unwrap().json().await.unwrap();
                if request["method"]=="eth_getBlockByNumber" && ["latest","safe","finalized"].iter().any(|tag|request["params"][0]==*tag) {heads.fetch_add(1,std::sync::atomic::Ordering::SeqCst);response["result"]["number"]=serde_json::json!("0xf4240");
                    // These tags advertise one synthetic block: keep its hash consistent so
                    // rejection tests the failed log capability, rather than a snapshot fork.
                    response["result"]["hash"]=serde_json::json!(format!("0x{}","88".repeat(32)));
                    response["result"]["parentHash"]=serde_json::json!(format!("0x{}","99".repeat(32)));}
                axum::Json(response)
            }
        }));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let mut recovery_config=config.clone();
        let a=recovery_config.rpc_groups.get_mut("alchemy").context("recovery A")?;
        a.members[1]=a.members[0].clone();a.members[1].id="healthy-backup".into();
        a.members[0].url=format!("http://{}",listener.local_addr()?);
        let proxy=tokio::spawn(async move {axum::serve(listener,router).await.unwrap()});
        // Abort on every exit path, including test failures.
        struct Proxy(tokio::task::JoinHandle<()>);
        impl Drop for Proxy {fn drop(&mut self){self.0.abort();}}
        let _proxy=Proxy(proxy);
        let fresh=recovery_config.route_set().map_err(anyhow::Error::msg)?;
        rpc_runtime::recover_watermark(&database.owner_pool,&fresh,31337,1,"test-owner","numeric poison regression").await.map_err(anyhow::Error::msg)?;
        ensure!(probe_heads.load(std::sync::atomic::Ordering::SeqCst)>=3 && probe_failures.load(std::sync::atomic::Ordering::SeqCst)>0,"first probe must accept high heads before failing capability");
        let pending:bool=sqlx::query_scalar("SELECT frozen AND recovery_pending FROM rpc_chain_state WHERE chain_id=31337").fetch_one(&database.app_pool).await?;
        ensure!(pending,"authorized numeric recovery must freeze before replay");
        let created:i64=sqlx::query_scalar("SELECT created_block FROM addresses WHERE id=$1").bind(address_id).fetch_one(&database.app_pool).await?;
        ensure!(created==0,"poisoned derived address progress must be repaired");
        // Recovery must validate both existing ledgers and leave the chain frozen on conflict.
        let wrong=format!("0x{}","aa".repeat(32));
        sqlx::query("UPDATE deposits SET block_hash=$2 WHERE address_id=$1").bind(address_id).bind(&wrong).execute(&database.owner_pool).await?;
        ensure!(rpc_runtime::resume_recovery(&database.owner_pool,&fresh,31337,16).await.is_err());
        sqlx::query("UPDATE deposits SET block_hash=$2 WHERE address_id=$1").bind(address_id).bind(&deposit_hash).execute(&database.owner_pool).await?;
        sqlx::query("UPDATE flushed SET block_hash=$2 WHERE address_id=$1").bind(address_id).bind(&wrong).execute(&database.owner_pool).await?;
        ensure!(rpc_runtime::resume_recovery(&database.owner_pool,&fresh,31337,16).await.is_err());
        sqlx::query("UPDATE flushed SET block_hash=$2 WHERE address_id=$1").bind(address_id).bind(&flush_hash).execute(&database.owner_pool).await?;
        ensure!(rpc_runtime::resume_recovery(&database.owner_pool,&fresh,31337,16).await.map_err(anyhow::Error::msg)?,"full recovery must finish");
        let credit:String=sqlx::query_scalar("SELECT credit_minor::text FROM deposits WHERE address_id=$1").bind(address_id).fetch_one(&database.app_pool).await?;ensure!(credit=="1");
        let flushed:String=sqlx::query_scalar("SELECT amount_atomic::text FROM flushed WHERE address_id=$1").bind(address_id).fetch_one(&database.app_pool).await?;ensure!(flushed=="100");
        let old_count:i64=sqlx::query_scalar("SELECT count(*) FROM rpc_window_reviews WHERE epoch=0 AND end_hash=$1").bind(&old.end_hash).fetch_one(&database.app_pool).await?;
        ensure!(old_count==1,"wrong-branch evidence stays immutable for audit");
        let due=db::rpc::due_reviews(&database.app_pool,31337).await?;
        for (id,_,_) in due { let epoch:i64=sqlx::query_scalar("SELECT epoch FROM rpc_window_reviews WHERE id=$1").bind(id).fetch_one(&database.app_pool).await?; ensure!(epoch==1,"only new epoch evidence can be replayed"); }
        // Reattach fresh runtime clients after recovery and run the production scanner again.
        let recovered=config.route_set().map_err(anyhow::Error::msg)?;
        rpc_runtime::accept(&database.app_pool,&recovered,&public).await.map_err(anyhow::Error::msg)?;
        let reader=topup_adapters::chain::evm::FinalizedReader::new(recovered.provider(31337,0)?.clone());
        topup::scanner::scan_once(&database.app_pool,&reader,&topup::scanner::chain_routes(&recovered)[0]).await?;
        let frozen:bool=sqlx::query_scalar("SELECT frozen FROM rpc_chain_state WHERE chain_id=31337").fetch_one(&database.app_pool).await?;
        ensure!(!frozen,"old wrong-branch evidence must not re-freeze the new epoch");
        let recovered_cursor=db::get_cursor(&database.app_pool,31337).await?;
        let recovered_anchor=state.load(31337,"alchemy","cursor").await?;
        drop(anvil);
        // All members are down, but the SAME accepted config can restart degraded without
        // lowering an anchor. A first acceptance of a new digest still fails.
        let restarted = config.route_set().map_err(anyhow::Error::msg)?;
        rpc_runtime::accept(&database.app_pool, &restarted, &public)
            .await
            .map_err(anyhow::Error::msg)?;
        ensure!(
            restarted
                .provider(31337, 0)?
                .group()
                .context("A")?
                .eligible()
                == 0
        );
        ensure!(db::get_cursor(&database.app_pool, 31337).await? == recovered_cursor);
        ensure!(state.load(31337, "alchemy", "cursor").await? == recovered_anchor);
        ensure!(
            rpc_runtime::accept(&database.app_pool, &restarted, &format!("{public} "))
                .await
                .is_err()
        );
        Ok(())
    }
    .await;
    database.cleanup().await?;
    result
}
