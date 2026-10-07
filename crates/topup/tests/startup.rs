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
    let mut route: RouteFile = serde_saphyr::from_str(&yaml)?;
    route.chain.rpc_providers = vec![
        anvil.rpc_url.clone(),
        anvil.rpc_url.replace("127.0.0.1", "localhost"),
    ];
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
    std::fs::write(&path, config_yaml(factory, TREASURY, None))?;
    let output = Command::new(env!("CARGO_BIN_EXE_topup"))
        .args(["run", "--config"])
        .arg(&path)
        .env_clear()
        // Complete runtime configuration, with a database nobody listens on: the contract check
        // must refuse before the service connects to it.
        .env("DATABASE_URL", "postgres://topup_service@127.0.0.1:1/topup")
        .env("TOPUP_RPC_ANKR_KEY", "test-key")
        .env("TOPUP_RPC_INFURA_KEY", "test-key")
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

/// Both service modes reject a cutover that must still be completed by 0.9.x.
#[tokio::test]
async fn run_refuses_an_incomplete_payment_settings_cutover() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            sqlx::query("UPDATE payment_settings_cutover SET recording_resumed_at = NULL")
                .execute(&database.owner_pool)
                .await?;
            let route = FIXTURE.lines().map(|line| format!("    {line}")).collect::<Vec<_>>().join("\n");
            let config = format!(
                "environment: staging\npublic_origin: http://127.0.0.1:8080\nadmin_key:\n  id: admin/v1\n  \
                 public_key: 11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=\n{rpc}\nroutes:\n  -\n{route}\n",
                rpc = include_str!("fixtures/chain-rpc.yaml")
            );
            // backfilled_at remains set: completing the backfill alone is insufficient.
            for read_only in [false, true] {
                let mut service = StartupService::start(
                    &config,
                    &database.app_url,
                    None,
                    read_only,
                    Some("test-sealed-0123456789"),
                    None,
                )?;
                let status = tokio::time::timeout(std::time::Duration::from_secs(20), async {
                    loop {
                        if let Some(status) = service.child.try_wait()? {
                            return anyhow::Ok(status);
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                })
                .await
                .context("incomplete cutover did not refuse startup")??;
                let logs = service.logs()?;
                ensure!(!status.success(), "{logs}");
                ensure!(logs.contains("payment settings cutover is incomplete; upgrade through 0.9.x first"), "{logs}");
                ensure!(!logs.contains("deposit pump started"), "{logs}");
            }
            Ok(())
        })
    })
    .await
}

/// A migrated database starts the pumps and advances its scanner without an operator action.
#[tokio::test]
async fn recording_starts_immediately_on_a_migrated_database() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let Some(anvil) = Anvil::start_if_available(&[]).await? else {
                return Ok(());
            };
            let factory = forge_create(
                &anvil.rpc_url,
                "src/ForwarderFactory.sol:ForwarderFactory",
                &[],
            )?;
            let token = forge_create(&anvil.rpc_url, "test/mocks/MockTokens.sol:MockERC20", &[])?;
            let oracle = forge_create(
                &anvil.rpc_url,
                "test/mocks/MockSanctionsOracle.sol:MockSanctionsOracle",
                &[],
            )?;
            cast(&["rpc", "anvil_mine", "0x80", "--rpc-url", &anvil.rpc_url])?;
            let tls = support::tls::RpcTlsProxy::start(&anvil.rpc_url)?;
            let config = config_yaml(factory, TREASURY, Some(&tls))
                .replace("chain_id: 1", "chain_id: 31337")
                .replace("livemode: true", "livemode: false")
                .replace(
                    "0x6c5bA91642F10282b576d91922Ae6448C9d52f4E",
                    &format!("{token:#x}"),
                )
                .replace(
                    "0x40C57923924B5c5c5455c48D93317139ADDaC8fb",
                    &format!("{oracle:#x}"),
                );
            // The guest-agent stub uses the same documented /GetKey contract as dstack_domains.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let endpoint = format!("http://{}", listener.local_addr()?);
            let guest = axum::Router::new().route(
                "/GetKey",
                axum::routing::post(|| async {
                    axum::Json(
                        serde_json::json!({"key": hex::encode([1; 32]), "signature_chain": []}),
                    )
                }),
            );
            let guest_task = tokio::spawn(async move { axum::serve(listener, guest).await });
            let result = async {
                let mut service = StartupService::start(
                    &config,
                    &database.app_url,
                    Some(&endpoint),
                    false,
                    Some("test-key"),
                    Some(&tls.certificate),
                )?;
                tokio::time::timeout(std::time::Duration::from_secs(30), async {
                    loop {
                        let logs = service.logs()?;
                        ensure!(
                            service.child.try_wait()?.is_none(),
                            "service exited: {logs}"
                        );
                        if logs.contains("deposit pump started") {
                            return anyhow::Ok(());
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                })
                .await
                .with_context(|| {
                    format!(
                        "recording did not start: {}",
                        service.logs().unwrap_or_default()
                    )
                })??;
                let ready: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM chain_coverage c JOIN chain_checkpoints p USING(chain_id) WHERE c.chain_id=31337 AND c.through_block>0 AND c.through_block=p.block_number)")
                    .fetch_one(&database.app_pool).await?;
                ensure!(ready,"coverage and checkpoint initialized before recording: {}", service.logs()?);
                Ok(())
            }
            .await;
            guest_task.abort();
            let _ = guest_task.await;
            result
        })
    })
    .await
}

/// Owns one service process and its temporary files, including cleanup on assertion failure.
struct StartupService {
    child: std::process::Child,
    config: std::path::PathBuf,
    log: std::path::PathBuf,
}

impl StartupService {
    fn start(
        config: &str,
        database_url: &str,
        endpoint: Option<&str>,
        read_only: bool,
        rpc_key: Option<&str>,
        certificate: Option<&std::path::Path>,
    ) -> Result<Self> {
        let prefix = std::env::temp_dir().join(format!("topup-startup-{}", uuid::Uuid::new_v4()));
        let config_path = prefix.with_extension("yaml");
        let log = prefix.with_extension("log");
        let result = (|| {
            std::fs::write(&config_path, config)?;
            let output = std::fs::File::create(&log)?;
            let mut command = Command::new(env!("CARGO_BIN_EXE_topup"));
            command
                .args(["run", "--config"])
                .arg(&config_path)
                .args(["--bind", "127.0.0.1:0"])
                .env_clear()
                .env("DATABASE_URL", database_url)
                .stdout(output.try_clone()?)
                .stderr(output);
            if let Some(certificate) = certificate {
                command.env("SSL_CERT_FILE", certificate);
            }
            if read_only {
                command.arg("--read-only");
            }
            if let Some(endpoint) = endpoint {
                command.env("DSTACK_SIMULATOR_ENDPOINT", endpoint);
            }
            if let Some(key) = rpc_key {
                command
                    .env("TOPUP_RPC_ANKR_KEY", key)
                    .env("TOPUP_RPC_INFURA_KEY", key);
            }
            command.spawn().context("start service")
        })();
        match result {
            Ok(child) => Ok(Self {
                child,
                config: config_path,
                log,
            }),
            Err(error) => {
                let _ = std::fs::remove_file(config_path);
                let _ = std::fs::remove_file(log);
                Err(error)
            }
        }
    }

    fn logs(&self) -> Result<String> {
        std::fs::read_to_string(&self.log).context("read startup logs")
    }
}

impl Drop for StartupService {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.config);
        let _ = std::fs::remove_file(&self.log);
    }
}

fn route_set(route: &RouteFile) -> Result<topup::routes::RouteSet> {
    topup::routes::RouteSet::new(vec![route.clone()]).map_err(anyhow::Error::msg)
}

fn route_yaml(anvil: &Anvil, factory: Address, treasury: &str) -> String {
    let _ = anvil;
    FIXTURE
        .replace(
            "0xe8A9Ab1AbC7651A5b7C2ED5B662F2f80BF5C446d",
            &format!("{factory:#x}"),
        )
        .replace("0x0000000000000000000000000000000000007EA5", treasury)
}

/// The service configuration of `route_yaml`, its two providers configured by id.
fn config_yaml(
    factory: Address,
    treasury: &str,
    tls: Option<&support::tls::RpcTlsProxy>,
) -> String {
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
    let mut rpc = include_str!("fixtures/chain-rpc.yaml").to_owned();
    if let Some(tls) = tls {
        rpc = rpc
            .replace(
                "https://rpc.ankr.com/eth/{key}",
                &format!("{}/?key={{key}}", tls.read_url),
            )
            .replace(
                "https://mainnet.infura.io/v3/{key}",
                &format!("{}/?key={{key}}", tls.verify_url),
            );
    }
    format!(
        "environment: staging\npublic_origin: http://127.0.0.1:8080\nadmin_key:\n  id: admin/v1\n  public_key: 11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=\n{rpc}\nroutes:\n  -\n{route}\n"
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
