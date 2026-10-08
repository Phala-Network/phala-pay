//! Real published N-1 workers and current workers share only an expand-only database.
mod support;

use alloy_primitives::{Address, B256};
use anyhow::{Context, Result, ensure};
use axum::{Json, Router, extract::State, routing::post};
use chrono::Utc;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use support::{
    TestDatabase,
    chain::{ANVIL_PRIVATE_KEY, Anvil, forge_create, run_checked},
    seed::{self, NewAccount},
};
use topup::{
    audit::Actor,
    deposit_addresses::{ChainContracts, ReissueTarget},
    routes::RouteSet,
};
use topup_core::{
    money::AtomicAmount,
    route::{Bounded, Confirmations, RouteFile},
};
use uuid::Uuid;

struct FixtureDirectory(PathBuf);
impl FixtureDirectory {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("chain-reads-rollback-{}", Uuid::new_v4()));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for FixtureDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct Task(tokio::task::JoinHandle<()>);
impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}
struct Service {
    child: Child,
    container: Option<String>,
    log: PathBuf,
}
impl Service {
    fn stop(mut self) -> Result<()> {
        self.terminate();
        let _ = self.child.wait()?;
        Ok(())
    }
    fn terminate(&mut self) {
        if let Some(name) = &self.container {
            let _ = Command::new("docker")
                .args(["stop", "--time", "2", name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = self.child.kill();
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.terminate();
        let _ = self.child.wait();
        if let Some(name) = &self.container {
            let _ = Command::new("docker")
                .args(["rm", "-f", name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

#[derive(Clone)]
struct Relay {
    upstream: String,
    client: reqwest::Client,
    gate: Arc<AtomicBool>,
    observed: Arc<AtomicBool>,
    scanner_gate: Arc<AtomicBool>,
    scanner_observed: Arc<AtomicBool>,
}
async fn relay(State(s): State<Relay>, Json(request): Json<Value>) -> Json<Value> {
    // Block all scanner range queries, including fast detection, while allowing startup's
    // 1000-recipient capability test and block-hash receipt checks to finish normally.
    if s.scanner_gate.load(Ordering::SeqCst)
        && request["method"] == "eth_getLogs"
        && !request["params"][0]["fromBlock"].is_null()
        && !request["params"][0]["toBlock"].is_null()
        && !request["params"][0]["topics"][2]
            .as_array()
            .is_some_and(|recipients| recipients.len() == 1000)
    {
        s.scanner_observed.store(true, Ordering::SeqCst);
        while s.scanner_gate.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    if s.gate.load(Ordering::SeqCst)
        && request["method"] == "eth_getLogs"
        && request["params"][0]["topics"][2]
            .as_array()
            .is_some_and(|recipients| recipients.len() < 1000)
        && request["params"][0]["topics"][0]
            .as_array()
            .is_some_and(|topics| topics.len() == 4)
    {
        s.observed.store(true, Ordering::SeqCst);
        while s.gate.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let response = async {
        s.client
            .post(&s.upstream)
            .json(&request)
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await
    }
    .await;
    Json(response.unwrap_or_else(|_|json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":"local chain unavailable"}})))
}
async fn serve(app: Router) -> Result<(String, Task)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://{}", listener.local_addr()?);
    Ok((
        origin,
        Task(tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("local fixture server");
        })),
    ))
}
fn cast(anvil: &Anvil, args: &[&str]) -> Result<Value> {
    let mut arguments = args.to_vec();
    arguments.extend(["--rpc-url", anvil.rpc_url.as_str()]);
    let output = run_checked("cast", &arguments, None)?;
    Ok(serde_json::from_slice(&output.stdout)?)
}
fn send(anvil: &Anvil, to: Address, signature: &str, args: &[&str]) -> Result<B256> {
    let address = format!("{to:#x}");
    let mut arguments = vec![
        "send",
        "--private-key",
        ANVIL_PRIVATE_KEY,
        "--json",
        &address,
        signature,
    ];
    arguments.extend_from_slice(args);
    serde_json::from_value(cast(anvil, &arguments)?["transactionHash"].clone())
        .context("sent transaction hash")
}
async fn wait_until<F, Fut>(label: &str, mut check: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    let end = tokio::time::Instant::now() + Duration::from_secs(150);
    loop {
        if check().await? {
            return Ok(());
        }
        ensure!(tokio::time::Instant::now() < end, "timed out: {label}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
async fn merchant(origin: &str, key: &str, path: &str, body: Value) -> Result<Value> {
    let response = reqwest::Client::new()
        .post(format!("{origin}{path}"))
        .bearer_auth(key)
        .header("Idempotency-Key", Uuid::new_v4().to_string())
        .json(&body)
        .send()
        .await?;
    let status = response.status();
    let value = response.json::<Value>().await?;
    ensure!(status.is_success(), "{path}: {status} {value}");
    Ok(value)
}
fn new_config(route: &RouteFile, tls: &support::tls::RpcTlsProxy) -> Result<Value> {
    let raw = json!({"environment":"local","public_origin":"http://topup:8080","admin_key":{"id":"drill/admin","public_key":support::public_key_base64(&ed25519_dalek::SigningKey::from_bytes(&[41;32]))},
        "rpc":[{"chain_id":1,"read":{"id":"ankr-drill","url":format!("{}/{{key}}",tls.read_url),"sealed_key":"TOPUP_RPC_ANKR_KEY","max_log_blocks":3000},"verify":{"id":"infura-drill","url":format!("{}/{{key}}",tls.verify_url),"sealed_key":"TOPUP_RPC_INFURA_KEY","max_log_blocks":3000}}],"routes":[route]});
    let config = topup::config::Config::parse(&raw.to_string()).map_err(anyhow::Error::msg)?;
    Ok(serde_json::from_str(
        &config.resolved_json().map_err(anyhow::Error::msg)?,
    )?)
}
fn previous_config(mut config: Value, anvil: &Anvil) -> Value {
    let port = anvil.rpc_url.rsplit(':').next().unwrap();
    config.as_object_mut().unwrap().remove("rpc");
    // N-1 keeps its own strict configuration schema and deprecated oracle settings.
    config.as_object_mut().unwrap().remove("sanctions");
    config["rpc_companies"] =
        json!({"read":{"domains":["read-drill.test"]},"verify":{"domains":["verify-drill.test"]}});
    config["rpc_budgets"] = json!({"read-account":{"requests_per_second":100,"burst":100},"read-key":{"requests_per_second":100,"burst":100},"verify-account":{"requests_per_second":100,"burst":100},"verify-key":{"requests_per_second":100,"burst":100}});
    config["rpc_groups"] = json!({"read":{"chain_id":1,"members":[{"id":"read","company":"read","url":format!("http://read-drill.test:{port}"),"account_budget":"read-account","key_budget":"read-key"}]},"verify":{"chain_id":1,"members":[{"id":"verify","company":"verify","url":format!("http://verify-drill.test:{port}"),"account_budget":"verify-account","key_budget":"verify-key"}]}});
    config["routes"][0]["chain"]["rpc_groups"] = json!({"a":"read","b":"verify"});
    config["routes"][0]["price"]["sources"][0]["rpc_group"] = json!("read");
    config["routes"][0]["price"]["sources"][0]["rpc_group_b"] = json!("verify");
    config
}
fn start_service(
    image: Option<&str>,
    config: &Path,
    database: &TestDatabase,
    kms: &str,
    port: u16,
    tls: &support::tls::RpcTlsProxy,
    directory: &Path,
) -> Result<Service> {
    let container = image.map(|_| format!("chain-reads-previous-{}", Uuid::new_v4()));
    let log = directory.join(format!("{}.log", Uuid::new_v4()));
    let file = std::fs::File::create(&log)?;
    let mut command = if let Some(image) = image {
        let mut c = Command::new("docker");
        c.args([
            "run",
            "--rm",
            "--network",
            "host",
            "--name",
            container.as_deref().unwrap(),
            "--add-host",
            "read-drill.test:127.0.0.1",
            "--add-host",
            "verify-drill.test:127.0.0.1",
            "-e",
            &format!("DATABASE_URL={}", database.app_url),
            "-e",
            &format!("DSTACK_SIMULATOR_ENDPOINT={kms}"),
            "-v",
            &format!("{}:/etc/drill.json:ro", config.display()),
            image,
            "topup",
        ]);
        c
    } else {
        let mut c = Command::new(env!("CARGO_BIN_EXE_topup"));
        c.env(
            "TOPUP_TEST_SLS_ORIGIN",
            std::fs::read_to_string(directory.join("sls-origin"))?,
        )
        .env("DATABASE_URL", &database.app_url)
        .env("DSTACK_SIMULATOR_ENDPOINT", kms)
        .env("SSL_CERT_FILE", &tls.certificate)
        .env("TOPUP_RPC_ANKR_KEY", "local-drill-key")
        .env("TOPUP_RPC_INFURA_KEY", "local-drill-key");
        c
    };
    command.args([
        "run",
        "--config",
        if image.is_some() {
            Path::new("/etc/drill.json")
        } else {
            config
        }
        .to_str()
        .unwrap(),
        "--bind",
        &format!("127.0.0.1:{port}"),
        "--public-origin",
        "http://topup:8080",
        "--wait-interval-s",
        "1",
        "--reconcile-interval-s",
        "1",
    ]);
    let child = command.stdout(file.try_clone()?).stderr(file).spawn()?;
    Ok(Service {
        child,
        container,
        log,
    })
}
async fn ready(service: &mut Service, origin: &str) -> Result<()> {
    let end = tokio::time::Instant::now() + Duration::from_secs(150);
    loop {
        ensure!(
            service.child.try_wait()?.is_none(),
            "worker exited: {}",
            std::fs::read_to_string(&service.log)?
        );
        if reqwest::get(format!("{origin}/healthz"))
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return Ok(());
        }
        ensure!(
            tokio::time::Instant::now() < end,
            "service health: {}",
            std::fs::read_to_string(&service.log)?
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
async fn credited(database: &TestDatabase, tx: B256) -> Result<()> {
    wait_until("payment credited",||async {Ok(sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM deposits WHERE tx_hash=$1 AND state IN ('credited','swept'))").bind(format!("{tx:#x}")).fetch_one(&database.app_pool).await?)}).await
}
fn reconcile(
    image: Option<&str>,
    config: &Path,
    database: &TestDatabase,
    tls: &support::tls::RpcTlsProxy,
) -> Result<()> {
    let mut command = if let Some(image) = image {
        let mut command = Command::new("docker");
        command.args([
            "run",
            "--rm",
            "--network",
            "host",
            "--add-host",
            "read-drill.test:127.0.0.1",
            "--add-host",
            "verify-drill.test:127.0.0.1",
            "-e",
            &format!("DATABASE_URL={}", database.app_url),
            "-v",
            &format!("{}:/etc/drill.json:ro", config.display()),
            image,
            "topup",
        ]);
        command
    } else {
        let mut command = Command::new(env!("CARGO_BIN_EXE_topup"));
        command
            .env("DATABASE_URL", &database.app_url)
            .env("SSL_CERT_FILE", &tls.certificate)
            .env("TOPUP_RPC_ANKR_KEY", "local-drill-key")
            .env("TOPUP_RPC_INFURA_KEY", "local-drill-key");
        command
    };
    command
        .args(["reconcile", "--config"])
        .arg(if image.is_some() {
            Path::new("/etc/drill.json")
        } else {
            config
        });
    let output = command.output()?;
    ensure!(
        output.status.success(),
        "reconciliation failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
/// A real N API submission. Rollback must preserve positive rows and abandon pending memory tasks.
async fn submit_hint(origin: &str, key: &str, address: &str, hash: B256) -> Result<()> {
    let response = reqwest::Client::new()
        .post(format!(
            "{origin}/v1/deposit_addresses/{address}/transactions"
        ))
        .bearer_auth(key)
        .json(&json!({"transaction_hash":format!("{hash:#x}"),"chain_id":1}))
        .send()
        .await?;
    ensure!(response.status().as_u16() == 202);
    ensure!(
        response.json::<Value>().await?
            == json!({"object":"transaction_submission","transaction_hash":format!("{hash:#x}"),"status":"received"})
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires verified published N-1 image; mandatory deploy rollback CI gate"]
async fn published_image_round_trip() -> Result<()> {
    ensure!(
        cfg!(feature = "test-support"),
        "rollback worker drill requires test-support; official OFAC fetch is forbidden"
    );
    let image = std::env::var("TOPUP_ROLLBACK_IMAGE").context("published N-1 image required")?;
    ensure!(image.contains("@sha256:"));
    let database = TestDatabase::create_with_migrations(false)
        .await?
        .context("local Postgres required")?;
    let directory = FixtureDirectory::new()?;
    let result=async {
        let sls = support::sanctions::Fixture::new(include_str!("fixtures/sdn.xml")).await?;
        std::fs::write(directory.path().join("sls-origin"), &sls.origin)?;
        let anvil=Anvil::start(1,&["--slots-in-an-epoch","4"]).await?;
        let factory=forge_create(&anvil.rpc_url,"src/ForwarderFactory.sol:ForwarderFactory",&[])?;
        let oracle=forge_create(&anvil.rpc_url,"test/mocks/MockSanctionsOracle.sol:MockSanctionsOracle",&[])?;
        let token=forge_create(&anvil.rpc_url,"test/mocks/MockTokens.sol:MockERC20",&[])?;
        send(&anvil,token,"mint(address,uint256)",&["0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266","1000000000000000000000"])?;
        let contracts=PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contracts");
        let code=run_checked("forge",&["inspect","test/mocks/PriceFixtures.sol:MockPriceAggregator","deployedBytecode"],Some(&contracts))?;
        let code=String::from_utf8(code.stdout)?;let feed=topup_core::price::feed("USDC_USD",1).unwrap().address;
        cast(&anvil,&["rpc","anvil_setCode",feed,code.trim()])?;
        for (slot,value) in [(0,8_u64),(1,100000000),(2,1),(3,u64::try_from(Utc::now().timestamp())?),(4,u64::try_from(Utc::now().timestamp())?),(5,1)] {
            cast(&anvil,&["rpc","anvil_setStorageAt",feed,&format!("0x{slot:064x}"),&format!("0x{value:064x}")])?;
        }
        anvil.mine(16)?;
        let gate=Arc::new(AtomicBool::new(false));let observed=Arc::new(AtomicBool::new(false));
        let scanner_gate=Arc::new(AtomicBool::new(false));let scanner_observed=Arc::new(AtomicBool::new(false));
        let (proxy,_proxy_task)=serve(Router::new().route("/{*path}",post(relay)).with_state(Relay {upstream:anvil.rpc_url.clone(),client:reqwest::Client::new(),gate:gate.clone(),observed:observed.clone(),scanner_gate:scanner_gate.clone(),scanner_observed:scanner_observed.clone()})).await?;
        let tls=support::tls::RpcTlsProxy::start(&proxy)?;
        let (kms,_kms_task)=serve(Router::new().route("/GetKey",post(||async {Json(json!({"key":"01".repeat(32),"signature_chain":[]}))}))).await?;
        let mut route:RouteFile=serde_saphyr::from_str(include_str!("fixtures/phala-cloud-pha.yaml"))?;
        route.livemode=true;route.route="rollback-ethereum-usdc-usd".into();route.chain.contracts.forwarder_factory=factory;
        route.chain.contracts.implementation=topup_core::route::factory_implementation(factory);
        route.screening.sanctions_oracle=oracle;route.chain.confirmations=Confirmations::Depth(2);route.asset.contract=token;route.asset.symbol="usdc".into();
        route.pricing.mode=topup_core::route::PricingMode::Stablecoin;route.pricing.primary.clear();route.pricing.check.clear();route.pricing.fx.clear();
        route.pricing.sources=vec![topup_core::price::Source::Chainlink {feed:"USDC_USD".into(),chain_id:1,observation_chain_id:None}];
        route.merchant.min_amount=Bounded::at(1);route.merchant.min_deposit_atomic=Bounded::at(AtomicAmount::new(alloy_primitives::U256::ZERO));
        route.validate()?;
        let current=new_config(&route,&tls)?;let old=previous_config(current.clone(),&anvil);
        let current_path=directory.path().join("current.json");let old_path=directory.path().join("previous.json");
        std::fs::write(&current_path,current.to_string())?;std::fs::write(&old_path,old.to_string())?;
        let migration=Command::new("docker").args(["run","--rm","--network","host","-e",&format!("DATABASE_URL={}",database.owner_url),&image,"topup","migrate"]).output()?;
        ensure!(migration.status.success(),"published N-1 migrate: {}",String::from_utf8_lossy(&migration.stderr));
        let account=seed::create_account(&database.app_pool,&NewAccount {livemode:true,..NewAccount::named("rollback merchant")}).await?;
        let key=seed::create_api_key(&database.app_pool,account.id,true).await?;
        let treasury:Address="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266".parse()?;
        seed::set_treasury(&database.app_pool,account.id,true,1,treasury).await?;seed::accept_routes(&database.app_pool,account.id,true,&[&route]).await?;
        let socket=std::net::TcpListener::bind("127.0.0.1:0")?;let port=socket.local_addr()?.port();drop(socket);let origin=format!("http://127.0.0.1:{port}");
        let mut previous=start_service(Some(&image),&old_path,&database,&kms,port,&tls,directory.path())?;ready(&mut previous,&origin).await?;
        let address=merchant(&origin,&key,"/v1/deposit_addresses",json!({"client_reference_id":"round-trip"})).await?;
        let forwarder:Address=address["networks"][0]["address"].as_str().context("issued network")?.parse()?;
        let da_id=topup::ids::parse(topup::ids::DEPOSIT_ADDRESS,address["id"].as_str().unwrap()).unwrap();
        let first=send(&anvil,token,"transfer(address,uint256)",&[&format!("{forwarder:#x}"),"1000000000000000000"])?;anvil.mine(16)?;credited(&database,first).await?;
        wait_until("N-1 finalized payment",||async {Ok(sqlx::query_scalar::<_,bool>("SELECT final_at IS NOT NULL FROM deposits WHERE tx_hash=$1").bind(format!("{first:#x}")).fetch_one(&database.app_pool).await?)}).await?;
        previous.stop()?;
        reconcile(Some(&image),&old_path,&database,&tls)?;
        // Restore fixture: version 2 was previously issued but its DB row was lost.
        // Its historical payment stays absent until real reissue restores the version and
        // lowers the newly inserted created_block below the current coverage boundary.
        let historical_salt=topup_core::address::deposit_address_salt(&account.public_id,true,"round-trip",2);
        let historical_forwarder=topup_core::address::forwarder_address(factory,route.chain.contracts.implementation,treasury,historical_salt);
        let historical=send(&anvil,token,"transfer(address,uint256)",&[&format!("{historical_forwarder:#x}"),"1000000000000000000"])?;
        anvil.mine(16)?;
        let historical_receipt=cast(&anvil,&["receipt",&format!("{historical:#x}"),"--json"])?;
        let historical_block=u64::from_str_radix(historical_receipt["blockNumber"].as_str().context("historical block")?.trim_start_matches("0x"),16)?;
        topup::db::migrate(&database.owner_pool).await?;
        scanner_gate.store(true,Ordering::SeqCst);
        let mut current_service=start_service(None,&current_path,&database,&kms,port,&tls,directory.path())?;ready(&mut current_service,&origin).await?;
        wait_until("scanner held off before hint-only phase",||async {Ok(scanner_observed.load(Ordering::SeqCst))}).await?;
        let hint_boundary:(i64,i64)=sqlx::query_as("SELECT through_block,(SELECT scanned_block FROM cursors WHERE chain_id=1) FROM chain_coverage WHERE chain_id=1").fetch_one(&database.app_pool).await?;
        let second=send(&anvil,token,"transfer(address,uint256)",&[&format!("{forwarder:#x}"),"1000000000000000000"])?;anvil.mine(16)?;
        submit_hint(&origin,&key,address["id"].as_str().unwrap(),second).await?;
        wait_until("hint recorded dual-verified positive deposit",||async {Ok(sqlx::query_scalar::<_,bool>("SELECT dual_verified_at=created_at FROM deposits WHERE tx_hash=$1").bind(format!("{second:#x}")).fetch_optional(&database.app_pool).await?==Some(true))}).await?;
        ensure!(sqlx::query_scalar::<_,i32>("SELECT used FROM daily_budgets WHERE name='hints'").fetch_one(&database.app_pool).await?==1);

        let after_hint:(i64,i64)=sqlx::query_as("SELECT through_block,(SELECT scanned_block FROM cursors WHERE chain_id=1) FROM chain_coverage WHERE chain_id=1").fetch_one(&database.app_pool).await?;
        ensure!(hint_boundary==after_hint,"scanner advanced during the hint-only phase");

        // RPC no longer holds the chain lock, so the pump can claim the hint row even
        // while scanning is blocked. Finish it before the deliberate restart; killing an
        // in-flight step would leave its five-minute lease beyond this drill's wait bound.
        credited(&database,second).await?;
        let after_credit:(i64,i64)=sqlx::query_as("SELECT through_block,(SELECT scanned_block FROM cursors WHERE chain_id=1) FROM chain_coverage WHERE chain_id=1").fetch_one(&database.app_pool).await?;
        ensure!(hint_boundary==after_credit,"scanner advanced before hint payment processing finished");

        // Restart at a scheduled-round boundary without changing production cadences.
        current_service.stop()?;scanner_gate.store(false,Ordering::SeqCst);current_service=start_service(None,&current_path,&database,&kms,port,&tls,directory.path())?;ready(&mut current_service,&origin).await?;credited(&database,second).await?;
        wait_until("N finalized second payment",||async {Ok(sqlx::query_scalar::<_,bool>("SELECT final_at IS NOT NULL FROM deposits WHERE tx_hash=$1").bind(format!("{second:#x}")).fetch_one(&database.app_pool).await?)}).await?;
        let first_salt:String=sqlx::query_scalar("SELECT salt FROM addresses WHERE deposit_address_id=$1").bind(da_id).fetch_one(&database.app_pool).await?;
        let n_flush=send(&anvil,factory,"flush(address,bytes32[],address)",&[&format!("{treasury:#x}"),&format!("[{first_salt}]"),&format!("{token:#x}")])?;
        anvil.mine(16)?;
        current_service.stop()?;current_service=start_service(None,&current_path,&database,&kms,port,&tls,directory.path())?;ready(&mut current_service,&origin).await?;
        wait_until("N recorded flush and swept second payment",||async {Ok(sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM flushed WHERE tx_hash=$1) AND EXISTS(SELECT 1 FROM deposits WHERE tx_hash=$2 AND final_at IS NOT NULL AND state='swept')").bind(format!("{n_flush:#x}")).bind(format!("{second:#x}")).fetch_one(&database.app_pool).await?)}).await?;
        current_service.stop()?;
        // The typed self-test needs a transaction/receipt/log at the finalized Anvil boundary.
        // Genuine zero transfers to the treasury pay no issued address and change no balance.
        for _ in 0..16 {send(&anvil,token,"transfer(address,uint256)",&[&format!("{treasury:#x}"),"0"])?;}
        reconcile(None,&current_path,&database,&tls)?;
        ensure!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM reconciliation_blocks").fetch_one(&database.app_pool).await?==0,"N reconciliation must be clean before rollback");
        current_service=start_service(None,&current_path,&database,&kms,port,&tls,directory.path())?;ready(&mut current_service,&origin).await?;
        let cancel=merchant(&origin,&key,"/v1/quotes",json!({"client_reference_id":"cancel","amount":100,"currency":"usd","chain_id":1,"asset":"usdc"})).await?;
        let canceled=merchant(&origin,&key,&format!("/v1/quotes/{}/cancel",cancel["id"].as_str().unwrap()),json!({})).await?;
        ensure!(canceled["status"]=="open" && canceled["cancel_requested_at"].is_number());
        let expiry=merchant(&origin,&key,"/v1/quotes",json!({"client_reference_id":"expiry","amount":100,"currency":"usd","chain_id":1,"asset":"usdc"})).await?;
        let expiry_id=topup::ids::parse(topup::ids::QUOTE,expiry["id"].as_str().unwrap()).unwrap();
        sqlx::query("UPDATE quotes SET expires_at=now()-interval '1 second' WHERE id=$1").bind(expiry_id).execute(&database.owner_pool).await?;
        ensure!(!sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM deposits WHERE tx_hash=$1)")
            .bind(format!("{historical:#x}")).fetch_one(&database.app_pool).await?,"restored history was recorded before reissue");
        let pending_hint=B256::repeat_byte(0x79);
        // Use a deposit-address object, whose endpoint requires the explicit network.
        submit_hint(&origin,&key,address["id"].as_str().unwrap(),pending_hint).await?;
        wait_until("N pending hint task started",||async {Ok(sqlx::query_scalar::<_,i32>("SELECT used FROM daily_budgets WHERE name='hints'").fetch_one(&database.app_pool).await?==2)}).await?;
        current_service.stop()?;
        ensure!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM deposits WHERE tx_hash=$1").bind(format!("{pending_hint:#x}")).fetch_one(&database.app_pool).await?==0);
        let historic_block:i64=sqlx::query_scalar("SELECT block_number FROM deposits WHERE tx_hash=$1").bind(format!("{first:#x}")).fetch_one(&database.app_pool).await?;
        let (reissued,created)=topup::deposit_addresses::reissue(&database.app_pool,&account,true,"round-trip",&[ChainContracts::of(&route)],ReissueTarget {version:Some(2),address:Some(historical_forwarder)},None,None,Some(Utc::now()-chrono::TimeDelta::hours(1)),&std::collections::BTreeMap::from([(1,u64::try_from(historic_block.saturating_sub(16))?)]),&Actor::system("rollback-drill"),"historical payment reissue").await?;
        ensure!(created && reissued.version==2);
        ensure!(sqlx::query_scalar::<_,bool>("SELECT dual_covered_through IS NULL AND created_block < $2 FROM addresses WHERE deposit_address_id=$1").bind(reissued.id).bind(i64::try_from(historical_block)?).fetch_one(&database.app_pool).await?);
        let salts:Vec<String>=sqlx::query_scalar("SELECT salt FROM addresses WHERE deposit_address_id=ANY($1) ORDER BY deposit_address_id").bind(vec![da_id,reissued.id]).fetch_all(&database.app_pool).await?;
        ensure!(salts.len()==2);let salts=format!("[{}]",salts.join(","));
        send(&anvil,factory,"flush(address,bytes32[],address)",&[&format!("{treasury:#x}"),&salts,&format!("{token:#x}")])?;
        anvil.mine(16)?;
        let before:(i64,i64)=sqlx::query_as("SELECT through_block,(SELECT scanned_block FROM cursors WHERE chain_id=1) FROM chain_coverage WHERE chain_id=1").fetch_one(&database.app_pool).await?;
        gate.store(true,Ordering::SeqCst);observed.store(false,Ordering::SeqCst);
        let mut interrupted=start_service(None,&current_path,&database,&kms,port,&tls,directory.path())?;
        ready(&mut interrupted,&origin).await?;
        wait_until("coverage request interrupted",||async {Ok(observed.load(Ordering::SeqCst))}).await?;
        interrupted.stop()?;gate.store(false,Ordering::SeqCst);
        let after:(i64,i64)=sqlx::query_as("SELECT through_block,(SELECT scanned_block FROM cursors WHERE chain_id=1) FROM chain_coverage WHERE chain_id=1").fetch_one(&database.app_pool).await?;ensure!(before==after,"interrupted coverage advanced");
        topup::db::chain_reads::freeze(&database.app_pool,1,"contract_code_mismatch").await?;
        let mut previous=start_service(Some(&image),&old_path,&database,&kms,port,&tls,directory.path())?;ready(&mut previous,&origin).await?;
        let frozen=reqwest::Client::new().post(format!("{origin}/v1/quotes")).bearer_auth(&key).header("Idempotency-Key",Uuid::new_v4().to_string()).json(&json!({"client_reference_id":"frozen","amount":100,"currency":"usd","chain_id":1,"asset":"usdc"})).send().await?;
        ensure!(frozen.status().as_u16()==400 && frozen.json::<Value>().await?["error"]["code"]=="chain_frozen","N-1 ignored an unknown chain-scope freeze");
        // Only after a fresh passing dual contract check, use the audited lift boundary.
        let pair=topup::chain_rpc::ChainRpc {read:Arc::new(topup_adapters::chain::evm::EvmClient::new(&anvil.rpc_url)?),verify:Arc::new(topup_adapters::chain::evm::EvmClient::new(&anvil.rpc_url)?)};
        let routes=RouteSet::with_rpc(vec![route.clone()],std::collections::BTreeMap::from([(1,pair)])).map_err(anyhow::Error::msg)?;
        let (app, _)=topup::api::router(topup::api::AppState {
            pool:database.app_pool.clone(), routes:Arc::new(routes),
            max_attached_pending_refunds: std::num::NonZeroU32::new(2).expect("positive refund limit"),
            admin_key:topup::api::VerificationKey::from_base64("drill/admin".into(),&support::public_key_base64(&ed25519_dalek::SigningKey::from_bytes(&[41;32]))).unwrap(),
            maintenance_keys:Vec::new(),public_origin:topup::api::PublicOrigin::parse(support::TEST_ORIGIN).unwrap(),
            attestor:Arc::new(topup_adapters::attestation::DstackAttestor::new()),
            rate_lock_quotes:Arc::new(topup::locks::UnavailableQuoteProvider),client_reads:Arc::default(),rate_limits:Arc::default(), hint_limits: Arc::default(), transaction_hints: Arc::default(),
            sanctions_rescreen: Arc::default(),
            screening:Arc::new(topup::refunds::UnavailableDestinationScreener),contract_signatures:Arc::new(topup::treasuries::UnavailableContractSignatures),
        });
        use tower::ServiceExt;
        let lifted=app.oneshot(support::signed_request(axum::http::Method::POST,"/v1/admin/reconciliation_blocks/chain:1/lift",serde_json::to_vec(&json!({"reason":"fresh dual check then audited rollback lift"}))?,"drill/admin",&ed25519_dalek::SigningKey::from_bytes(&[41;32]),Utc::now().timestamp())).await?;
        ensure!(lifted.status().is_success(),"fresh checked audited lift failed");
        let third=send(&anvil,token,"transfer(address,uint256)",&[&format!("{historical_forwarder:#x}"),"1000000000000000000"])?;anvil.mine(16)?;
        previous.stop()?;previous=start_service(Some(&image),&old_path,&database,&kms,port,&tls,directory.path())?;ready(&mut previous,&origin).await?;credited(&database,third).await?;credited(&database,historical).await?;
        send(&anvil,factory,"flush(address,bytes32[],address)",&[&format!("{treasury:#x}"),&salts,&format!("{token:#x}")])?;
        anvil.mine(16)?;
        previous.stop()?;previous=start_service(Some(&image),&old_path,&database,&kms,port,&tls,directory.path())?;ready(&mut previous,&origin).await?;
        wait_until("N-1 finality and sweep",||async {Ok(sqlx::query_scalar::<_,bool>("SELECT count(*)=4 AND bool_and(final_at IS NOT NULL AND state='swept') FROM deposits").fetch_one(&database.app_pool).await?)}).await?;
        ensure!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM deposits WHERE tx_hash=$1").bind(format!("{historical:#x}")).fetch_one(&database.app_pool).await?==1);
        wait_until("N-1 indexed flush",||async {Ok(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM flushed").fetch_one(&database.app_pool).await?>0)}).await?;
        previous.stop()?;
        ensure!(sqlx::query_scalar::<_,i32>("SELECT used FROM daily_budgets WHERE name='hints'").fetch_one(&database.app_pool).await?==2,"N-1 touched pending hint state");
        ensure!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM deposits WHERE tx_hash=$1").bind(format!("{pending_hint:#x}")).fetch_one(&database.app_pool).await?==0,"N-1 materialized an unmined hint");
        ensure!(sqlx::query_scalar::<_,bool>("SELECT dual_verified_at=created_at FROM deposits WHERE tx_hash=$1").bind(format!("{second:#x}")).fetch_one(&database.app_pool).await?,"N-1 discarded N's hint verification marker");
        reconcile(Some(&image),&old_path,&database,&tls)?;
        let unverified:bool=sqlx::query_scalar("SELECT dual_verified_at IS NULL FROM deposits WHERE tx_hash=$1").bind(format!("{third:#x}")).fetch_one(&database.app_pool).await?;ensure!(unverified);
        let mut final_current=start_service(None,&current_path,&database,&kms,port,&tls,directory.path())?;ready(&mut final_current,&origin).await?;
        wait_until("N reverified N-1 deposits and address backfill",||async {Ok(sqlx::query_scalar::<_,bool>("SELECT NOT EXISTS(SELECT 1 FROM deposits WHERE dual_verified_at IS NULL) AND NOT EXISTS(SELECT 1 FROM addresses a JOIN chain_coverage c USING(chain_id) WHERE a.dual_covered_through IS DISTINCT FROM c.through_block)").fetch_one(&database.app_pool).await?)}).await?;
        wait_until("coverage cancellation and expiry",||async {Ok(sqlx::query_scalar::<_,bool>("SELECT bool_and(status IN ('expired','cancelled')) FROM quotes WHERE id=ANY($1)").bind(vec![expiry_id,topup::ids::parse(topup::ids::QUOTE,cancel["id"].as_str().unwrap()).unwrap()]).fetch_one(&database.app_pool).await?)}).await?;
        ensure!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM deposits").fetch_one(&database.app_pool).await?==4);
        ensure!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM events WHERE type='deposit.credited'").fetch_one(&database.app_pool).await?==4);
        ensure!(sqlx::query_scalar::<_,bool>("SELECT bool_and(final_at IS NOT NULL AND state='swept') FROM deposits").fetch_one(&database.app_pool).await?);
        ensure!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM reconciliation_blocks").fetch_one(&database.app_pool).await?==0);
        final_current.stop()?;
        // Anvil mines empty blocks for the shortened finality lag. Give the typed startup
        // self-test a genuine transaction/receipt/log in the finalized block as a real
        // active chain does. These zero transfers pay no issued address and change no balance.
        for _ in 0..16 {
            send(&anvil,token,"transfer(address,uint256)",&[&format!("{treasury:#x}"),"0"])?;
        }
        reconcile(None,&current_path,&database,&tls)?;
        Ok(())
    }.await;
    if let Err(error) = &result {
        eprintln!("rollback drill failed: {error:#}");
        for entry in std::fs::read_dir(directory.path())? {
            let path = entry?.path();
            if path.extension().is_some_and(|extension| extension == "log") {
                let log = std::fs::read_to_string(&path)?;
                let lines: Vec<_> = log.lines().collect();
                eprintln!(
                    "--- {} ---\n{}",
                    path.display(),
                    lines[lines.len().saturating_sub(80)..].join("\n")
                );
            }
        }
        let deposits: Vec<(String, String, Option<String>)> =
            sqlx::query_as("SELECT tx_hash,state,reason FROM deposits ORDER BY created_at")
                .fetch_all(&database.app_pool)
                .await?;
        eprintln!("deposit outcomes: {deposits:?}");
    }
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}
