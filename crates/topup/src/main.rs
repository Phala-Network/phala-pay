//! Command-line entry point for the Phala Pay service.

mod route;

use std::collections::HashMap;
use std::future::{Future, IntoFuture as _};
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, bail};
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand};
use serde_json::json;
use sqlx::PgPool;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use topup::pump::{AgeAlertConfig, AgeAlerter, Pump, PumpConfig, StepSet};
use topup::steps::confirm::ConfirmStep;
use topup::steps::screen::ScreenStep;
use topup_adapters::attestation::AttestedWebhookKey;
use topup_adapters::attestation::DstackAttestor;
#[cfg(feature = "dev-signer")]
use topup_adapters::attestation::report_data;
#[cfg(feature = "dev-signer")]
use topup_adapters::signer::DevSigner;
use topup_adapters::signer::actor::SignerHandle;
use topup_adapters::signer::dstack::DstackSigner;
#[cfg(feature = "dev-signer")]
use topup_core::SecretKey32;
use topup_core::{CLIENT_SECRET_KEY_DOMAIN, DB_APP_KEY_DOMAIN, DB_OWNER_KEY_DOMAIN};
#[cfg(feature = "dev-signer")]
use topup_core::{Signer as _, WebhookKeyId};
use tracing_subscriber::util::SubscriberInitExt as _;

#[derive(Parser)]
#[command(name = "topup", version, about = "Phala Pay service")]
struct Cli {
    #[command(subcommand)]
    command: TopupCommand,
}

#[derive(Subcommand)]
enum TopupCommand {
    /// Send one synthetic business alert to staging Sentry; requires SENTRY_DSN.
    AlertTest {
        #[arg(long, value_enum)]
        alert: SyntheticAlert,
        #[arg(long, default_value = "warning", value_parser = ["warning", "critical"])]
        severity: String,
    },
    Run(RunArgs),
    /// Apply the database migrations as the database owner.
    Migrate,
    Route {
        #[command(subcommand)]
        command: RouteCommand,
    },
    /// Typed read/verify endpoint preflight.
    Rpc {
        #[command(subcommand)]
        command: RpcCommand,
    },
    /// Run one reconciliation pass and exit.
    Reconcile(ReconcileArgs),
    /// Print attestation evidence binding a nonce to an account's webhook keys in one mode, as
    /// `GET /v1/attestation` returns it to that account.
    Attest(AttestArgs),
    /// Derive the backup key and database credentials from dstack into a shared tmpfs.
    Keys(KeysArgs),
    Heartbeat(HeartbeatArgs),
    /// Exit zero only when the local API answers `GET /healthz` with 200, for container health.
    Healthcheck(HealthcheckArgs),
    /// Validate a restored database and run post-restore reconciliation.
    ///
    /// Run only while the service, heartbeat, and backup processes are stopped: the post-restore
    /// round holds the lease-owner lock and may repair the restored ledger; it asks the product
    /// nothing. It writes its report to --report when that is given.
    RestoreCheck(RestoreCheckArgs),
    /// Validate or print a service configuration file.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Subcommand)]
enum RpcCommand {
    /// Probe every endpoint with its sealed credential; print only validated endpoint ids.
    Check {
        #[arg(long)]
        config: PathBuf,
    },
}

#[derive(Clone, clap::ValueEnum)]
#[value(rename_all = "verbatim")]
enum SyntheticAlert {
    #[value(name = "TopupOutboxBacklog")]
    OutboxBacklog,
    #[value(name = "TopupOutboxStalled")]
    OutboxStalled,
    #[value(name = "TopupOutboxInternalFailure")]
    OutboxInternalFailure,
    #[value(name = "TopupHeartbeatStale")]
    HeartbeatStale,
    #[value(name = "TopupTreasuryProgressAge")]
    TreasuryProgressAge,
    #[value(name = "TopupRefundProgressAge")]
    RefundProgressAge,
    #[value(name = "TopupCertificateExpiry")]
    CertificateExpiry,
    #[value(name = "TopupCertificateProbeFailed")]
    CertificateProbeFailed,
    #[value(name = "TopupBusinessProbeFailed")]
    BusinessProbeFailed,
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Validate the file without any secret; with --secrets, also check each provider's sealed
    /// key (`TOPUP_RPC_<ID>_KEY`) against its URL. Nothing secret is printed.
    Check {
        #[arg(long)]
        secrets: bool,
        /// Require a present, valid SENTRY_DSN for production preflight.
        #[arg(long)]
        require_sentry: bool,
        file: PathBuf,
    },
    /// Print the resolved configuration as JSON (also a valid configuration file): every route
    /// default written out, and each keyed provider URL with its `{key}`.
    Show { file: PathBuf },
}

#[derive(Args)]
struct RunArgs {
    /// The service configuration file (docs/configuration.md).
    #[arg(long, value_name = "FILE")]
    config: PathBuf,
    /// API socket address; defaults to the deployment port on all interfaces.
    #[arg(long, default_value = "0.0.0.0:8080")]
    bind: std::net::SocketAddr,
    /// The egress proxy every webhook delivery goes through (the smokescreen sidecar); required
    /// unless the public origin is `http` (local stacks).
    #[arg(long, value_name = "URL")]
    webhook_proxy: Option<reqwest::Url>,
    /// Serve only the read API of a database restored from backup (deploy/RESTORE.md): no loop,
    /// no lease-owner lock, and every write refused.
    #[arg(long)]
    read_only: bool,
    /// An origin replacing the configured one: the restore instance's own (deploy/RESTORE.md), or a
    /// local stack's. deploy/compose-policy.jq keeps it out of the service's attested compose.
    #[arg(long, value_name = "URL", conflicts_with = "public_origin_host_env")]
    public_origin: Option<String>,
    /// The environment variable that holds the public origin's host, served as `https://HOST`,
    /// when the configuration leaves `public_origin` out: the Phala Cloud template's
    /// `DSTACK_APP_DOMAIN` (deploy/compose.template.yaml). The host must be a lowercase DNS name.
    #[arg(long, value_name = "NAME")]
    public_origin_host_env: Option<String>,
    /// The environment variable that holds the admin public key (standard base64 ed25519) when the
    /// configuration leaves `admin_key.public_key` out: the Phala Cloud template's
    /// `TOPUP_ADMIN_PUBLIC_KEY`.
    #[arg(long, value_name = "NAME")]
    admin_public_key_env: Option<String>,
    /// The restore-check report served on /healthz.
    #[arg(long, value_name = "FILE", requires = "read_only")]
    restore_report: Option<PathBuf>,
    /// Delay before retrying an expected wait outcome.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..))]
    wait_interval_s: u64,
    /// Least delay between reconciliation rounds; a round runs only after `finalized` advanced.
    #[arg(long, default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..))]
    reconcile_interval_s: u64,
}

/// Concurrent deposit pumps in one service process.
const PUMPS: usize = 1;
/// Interval between deposit state-age scans.
const AGE_ALERT_INTERVAL: Duration = Duration::from_secs(60);
/// Interval between prunings of the idempotency keys older than 24 hours.
const IDEMPOTENCY_PRUNE_INTERVAL: Duration = Duration::from_secs(600);

#[derive(Args)]
struct ReconcileArgs {
    /// The service configuration file.
    #[arg(long, value_name = "FILE")]
    config: PathBuf,
}

#[derive(Args)]
struct AttestArgs {
    #[arg(long, value_name = "HEX")]
    nonce: String,
    /// The account, `acct_…`.
    #[arg(long, value_name = "ACCOUNT")]
    account: String,
    /// Attest the live-mode keys instead of the test-mode keys.
    #[arg(long)]
    live: bool,
    /// Key versions to attest, current first; repeat during a rotation.
    #[arg(long = "version", value_name = "N", default_value = "1")]
    versions: Vec<u32>,
    #[cfg(feature = "dev-signer")]
    #[arg(long, help = "Use development keys without a hardware quote")]
    dev: bool,
}

#[derive(Args)]
struct KeysArgs {
    /// Directory (tmpfs) for backup.key, which WAL-G reads.
    #[arg(long, value_name = "DIR")]
    backup_dir: PathBuf,
    /// Directory (tmpfs) for the owner login's postgres.password and postgres.pgpass.
    #[arg(long, value_name = "DIR")]
    owner_dir: PathBuf,
    /// Directory (tmpfs) for the application login's topup_service.pgpass.
    #[arg(long, value_name = "DIR")]
    app_dir: PathBuf,
    /// Keep the process alive so the shared tmpfs remains mounted.
    #[arg(long, conflicts_with = "check")]
    hold: bool,
    /// Check only that the files were atomically published with safe metadata.
    #[arg(long, conflicts_with = "hold")]
    check: bool,
}

#[derive(Args)]
struct RestoreCheckArgs {
    /// Externally recorded failure instant used to measure RPO. Omitted at boot after a
    /// restore from backup: the report is then `unanchored` and the operator compares its
    /// `restored_heartbeat_at` with their own external anchor.
    #[arg(long, value_name = "RFC3339")]
    failure_at: Option<DateTime<Utc>>,
    /// Source WAL location from the same heartbeat log line. Omit only as a declared incident
    /// exception; RPO is then proven by the heartbeat timestamp alone and flagged in the report.
    #[arg(long, value_name = "PG_LSN", requires = "failure_at")]
    expected_lsn: Option<String>,
    /// The service configuration file.
    #[arg(long, value_name = "FILE")]
    config: PathBuf,
    /// Where to publish the report, for the read-only service's /healthz.
    #[arg(long, value_name = "FILE")]
    report: Option<PathBuf>,
}

#[derive(Args)]
struct HeartbeatArgs {
    /// Seconds between persisted RPO heartbeats.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..))]
    interval_s: u64,
}

#[derive(Args)]
struct HealthcheckArgs {
    /// Health endpoint of the API listener in this container.
    #[arg(long, default_value = "http://127.0.0.1:8080/healthz")]
    url: reqwest::Url,
}

#[derive(Subcommand)]
enum RouteCommand {
    Validate {
        /// Permit zero factory and implementation placeholders in deployment templates.
        #[arg(long)]
        template: bool,
        file: PathBuf,
    },
    /// Print the resolved route as JSON (also a valid route file): every code default written out.
    Show {
        /// Permit zero factory and implementation placeholders in deployment templates.
        #[arg(long)]
        template: bool,
        file: PathBuf,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    // `run` reports under its configuration's environment; the file is read again below, where a
    // failure is logged.
    let environment = match &cli.command {
        TopupCommand::Run(args) => topup::config::Config::load(&args.config)
            .ok()
            .map(|config| (config.environment, args.read_only)),
        TopupCommand::AlertTest { .. } => Some(("staging".to_owned(), false)),
        _ => None,
    };
    // Before the subscriber, which adds the Sentry layer only when reporting is enabled. The
    // guard flushes queued events when `main` returns.
    let reporting = match topup::observability::init_reporting(
        environment
            .as_ref()
            .map(|(environment, read_only)| (environment.as_str(), *read_only)),
    ) {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let logging = if matches!(
        &cli.command,
        TopupCommand::Rpc {
            command: RpcCommand::Check { .. }
        }
    ) {
        // Machine-readable RPC results and diagnostics must use separate streams.
        topup::observability::log_subscriber(std::io::stderr).try_init()
    } else {
        topup::observability::log_subscriber(std::io::stdout).try_init()
    };
    if let Err(error) = logging {
        eprintln!("failed to initialize tracing: {error}");
        return ExitCode::FAILURE;
    }

    let result = match cli.command {
        TopupCommand::AlertTest { alert, severity } => {
            if reporting.is_none() {
                eprintln!("alert-test requires the staging SENTRY_DSN");
                return ExitCode::FAILURE;
            }
            let name = clap::ValueEnum::to_possible_value(&alert).expect("alert has a CLI value");
            topup::observability::emit_alert(name.get_name(), "synthetic", &severity, 1, 0);
            Ok(ExitCode::SUCCESS)
        }
        TopupCommand::Run(args) => {
            // Only `run` reports the configured reporting mode at startup.
            tracing::info!(
                sentry_enabled = reporting.is_some(),
                "error reporting configured"
            );
            run(&args).await
        }
        TopupCommand::Config {
            command:
                ConfigCommand::Check {
                    secrets,
                    require_sentry,
                    file,
                },
        } => return check_config(&file, secrets, require_sentry),
        TopupCommand::Config {
            command: ConfigCommand::Show { file },
        } => return show_config(&file),
        TopupCommand::Migrate => migrate().await,
        TopupCommand::Route {
            command: RouteCommand::Validate { template, file },
        } => return validate_route(&file, template),
        TopupCommand::Route {
            command: RouteCommand::Show { template, file },
        } => return show_route(&file, template),
        TopupCommand::Reconcile(args) => reconcile(&args).await,
        TopupCommand::Rpc { command } => rpc_command(command).await,
        TopupCommand::Attest(args) => {
            return match attest(&args).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(message) => {
                    eprintln!("{message}");
                    ExitCode::FAILURE
                }
            };
        }
        TopupCommand::Keys(args) => keys(&args).await,
        TopupCommand::Heartbeat(args) => heartbeat(&args).await,
        TopupCommand::Healthcheck(args) => return healthcheck(&args).await,
        TopupCommand::RestoreCheck(args) => restore_check(&args).await,
    };

    result.unwrap_or_else(|error| {
        // The outermost context is the message; the error it wraps, if any, the `error` field.
        match error.source() {
            Some(cause) => tracing::error!(error = %cause, "{error}"),
            None => tracing::error!("{error}"),
        }
        ExitCode::FAILURE
    })
}

async fn keys(args: &KeysArgs) -> anyhow::Result<ExitCode> {
    if args.check {
        let checked = topup::keys::check(&args.backup_dir, &args.owner_dir, &args.app_dir);
        return Ok(if checked.is_ok() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }

    let signer = DstackSigner::new();
    let (Ok(backup), Ok(owner), Ok(app)) = tokio::join!(
        signer.derive_backup_key(),
        signer.derive_secret(DB_OWNER_KEY_DOMAIN),
        signer.derive_secret(DB_APP_KEY_DOMAIN),
    ) else {
        bail!("failed to derive the backup key and database credentials");
    };
    let backup_key = args.backup_dir.join(topup::keys::BACKUP_KEY_FILE);
    topup::keys::write_backup_key(&backup_key, &backup)
        .with_context(|| format!("failed to write backup key file {}", backup_key.display()))?;
    topup::keys::write_database_credentials(&args.owner_dir, &args.app_dir, &owner, &app)
        .context("failed to write database credential files")?;
    tracing::info!("key files are ready");
    if args.hold {
        wait_for_shutdown_signal()
            .await
            .context("failed to listen for key holder shutdown signal")?;
        tracing::info!("key holder stopped");
    }
    Ok(ExitCode::SUCCESS)
}

async fn heartbeat(args: &HeartbeatArgs) -> anyhow::Result<ExitCode> {
    let pool = connect("heartbeat", 1)
        .await
        .context("failed to connect to database")?;
    let mut interval = tokio::time::interval(Duration::from_secs(args.interval_s));
    let shutdown = wait_for_shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let record = topup::heartbeat::record(&pool)
                    .await
                    .context("failed to record restore heartbeat")?;
                tracing::info!(
                    heartbeat_id = record.id,
                    // RFC 3339, so the operator can compare it with the failure instant.
                    recorded_at = %record
                        .recorded_at
                        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
                    rpo_seconds = record.rpo_seconds,
                    wal_lsn = %record.wal_lsn,
                    "restore heartbeat recorded"
                );
            }
            signal = &mut shutdown => {
                signal.context("failed to listen for heartbeat shutdown signal")?;
                tracing::info!("heartbeat stopped");
                return Ok(ExitCode::SUCCESS);
            }
        }
    }
}

// The exit status is the health signal; failures are logged below error level so a probe every
// 30 s during an outage does not duplicate the service's own error reports.
async fn healthcheck(args: &HealthcheckArgs) -> ExitCode {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(%error, "failed to build the health check client");
            return ExitCode::FAILURE;
        }
    };
    match client.get(args.url.clone()).send().await {
        Ok(response) if response.status() == reqwest::StatusCode::OK => ExitCode::SUCCESS,
        Ok(response) => {
            tracing::warn!(status = %response.status(), "health check failed");
            ExitCode::FAILURE
        }
        Err(error) => {
            tracing::warn!(%error, "health check request failed");
            ExitCode::FAILURE
        }
    }
}

async fn restore_check(args: &RestoreCheckArgs) -> anyhow::Result<ExitCode> {
    let result = run_restore_check(args).await;
    let encoded = match &result {
        Ok(report) => {
            serde_json::to_value(report).context("failed to encode restore check report")?
        }
        Err(error) => json!({ "status": "failed", "failures": [error.to_string()] }),
    };
    if let Some(path) = &args.report {
        write_restore_report(path, &encoded)
            .with_context(|| format!("failed to write restore check report {}", path.display()))?;
    }
    let report = result?;
    println!("{encoded}");
    Ok(if report.status == "ok" {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Runs the post-restore gate; each error's outermost context is the report's failure.
async fn run_restore_check(
    args: &RestoreCheckArgs,
) -> anyhow::Result<topup::restore::RestoreReport> {
    let config = load_config(&args.config)?;
    let routes = config
        .route_set()
        .map_err(anyhow::Error::msg)
        .context("failed to load the route configuration")?;
    // The post-restore gate reads and repairs with owner credentials, never the service login.
    let pool = connect_owner("restore-check", 4)
        .await
        .context("failed to connect to the restored database")?;
    topup::rpc_runtime::preflight(&routes)
        .await
        .map_err(anyhow::Error::msg)?;
    let reconciler = topup::reconciler::Reconciler::from_routes(pool.clone(), Arc::new(routes))
        .context("failed to configure the post-restore reconciler")?;
    let expectations = topup::restore::RestoreExpectations {
        failure_at: args.failure_at,
        expected_lsn: args.expected_lsn.clone(),
    };
    topup::restore::check(&pool, &expectations, &reconciler)
        .await
        .map_err(anyhow::Error::msg)
}

/// Publishes the report atomically, so a reader never sees a partial file.
fn write_restore_report(path: &Path, report: &serde_json::Value) -> std::io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(".tmp.{}", std::process::id()));
    let temporary = PathBuf::from(temporary);
    std::fs::write(&temporary, report.to_string())?;
    std::fs::rename(&temporary, path)
}

async fn attest(args: &AttestArgs) -> Result<(), &'static str> {
    let nonce = parse_nonce(&args.nonce)?;
    if args.versions.is_empty()
        || args.versions.iter().any(|&version| {
            topup_core::WebhookKeyId::new(&args.account, args.live, version).is_none()
        })
    {
        return Err("account must be an acct_ id and every version at least 1");
    }

    #[cfg(feature = "dev-signer")]
    if args.dev {
        let signer = DevSigner::derive(&SecretKey32::new([1; 32]));
        let mut keys = Vec::with_capacity(args.versions.len());
        for &version in &args.versions {
            let id = WebhookKeyId::new(&args.account, args.live, version)
                .ok_or("account must be an acct_ id")?;
            let public_key = signer
                .webhook_public_key(&id)
                .await
                .map_err(|_| "development webhook key is invalid")?;
            keys.push(AttestedWebhookKey {
                version,
                public_key,
            });
        }
        let report_data = report_data(&nonce, &args.account, args.live, &keys)
            .ok_or("nonce or account is too long")?;
        return print_attestation(args, &keys, &report_data, &[], &[], &[]);
    }

    let evidence = DstackAttestor::new()
        .attest(&nonce, &args.account, args.live, &args.versions)
        .await
        .map_err(|_| "failed to collect dstack attestation")?;
    print_attestation(
        args,
        &evidence.webhook_keys,
        &evidence.report_data,
        &evidence.quote,
        &evidence.info.app_id,
        &evidence.info.compose_hash,
    )
}

fn parse_nonce(value: &str) -> Result<Vec<u8>, &'static str> {
    if value.is_empty() {
        return Err("nonce must be non-empty hexadecimal");
    }
    if value.len() > 64 {
        return Err("nonce must be at most 32 bytes (64 hexadecimal characters)");
    }
    hex::decode(value).map_err(|_| "nonce must be valid hexadecimal")
}

fn print_attestation(
    args: &AttestArgs,
    keys: &[AttestedWebhookKey],
    report_data: &[u8; 32],
    quote: &[u8],
    app_id: &[u8],
    compose_hash: &[u8],
) -> Result<(), &'static str> {
    let webhook_keys: Vec<_> = keys
        .iter()
        .map(|key| {
            json!({
                "version": key.version,
                "public_key": topup::webhook_keys::standard_webhooks_public_key(&key.public_key),
            })
        })
        .collect();
    let output = json!({
        "object": "attestation",
        "account": args.account,
        "livemode": args.live,
        "webhook_keys": webhook_keys,
        "report_data": hex::encode(report_data),
        "tdx_quote": hex::encode(quote),
        "app_id": if app_id.is_empty() { String::new() } else { format!("0x{}", hex::encode(app_id)) },
        "compose_hash": if compose_hash.is_empty() { String::new() } else { format!("0x{}", hex::encode(compose_hash)) },
    });
    let encoded = serde_json::to_string(&output).map_err(|_| "failed to encode attestation")?;
    println!("{encoded}");
    Ok(())
}

async fn check_payment_settings_cutover(pool: &PgPool) -> anyhow::Result<()> {
    if topup::payment_config::cutover_incomplete(&mut *pool.acquire().await?)
        .await
        .context("failed to read the payment settings cutover")?
    {
        bail!("payment settings cutover is incomplete; upgrade through 0.9.x first");
    }
    Ok(())
}

async fn run(args: &RunArgs) -> anyhow::Result<ExitCode> {
    let config = load_config(&args.config)?;
    let routes = config
        .route_set()
        .map_err(anyhow::Error::msg)
        .context("failed to load the route configuration")?;
    let age_config =
        AgeAlertConfig::from_routes(routes.routes()).context("invalid age alert configuration")?;
    let pump_config = PumpConfig {
        step_timeout: topup::pump::STEP_TIMEOUT,
        wait_interval: Duration::from_secs(args.wait_interval_s),
        ..PumpConfig::default()
    };
    // Checked before the on-chain contract check; `connect` reads it again below.
    database_url("run").context("missing runtime configuration")?;
    let environment = |name: &str| std::env::var(name).ok();
    let admin_key = config
        .runtime_admin_key(args.admin_public_key_env.as_deref(), environment)
        .map_err(anyhow::Error::msg)
        .context("invalid admin key")?;
    let public_origin = match &args.public_origin {
        Some(origin) => {
            topup::api::PublicOrigin::parse(origin).context("invalid --public-origin")?
        }
        None => config
            .runtime_public_origin(args.public_origin_host_env.as_deref(), environment)
            .map_err(anyhow::Error::msg)
            .context("invalid public origin")?,
    };
    if args.read_only {
        const READ_ONLY_CONNECTIONS: u32 = 4;
        let pool = connect("run", READ_ONLY_CONNECTIONS)
            .await
            .context("failed to connect to database")?;
        check_payment_settings_cutover(&pool).await?;
        let state = topup::api::AppState {
            pool,
            routes: Arc::new(routes),
            admin_key,
            maintenance_keys: config.maintenance_keys.clone(),
            public_origin,
            attestor: Arc::new(DstackAttestor::new()),
            rate_lock_quotes: Arc::new(topup::locks::UnavailableQuoteProvider),
            client_reads: Arc::new(topup::api::ClientReadLimiter::new(
                client_secret_key().await?,
                READ_ONLY_CONNECTIONS,
            )),
            rate_limits: Arc::default(),
            screening: Arc::new(topup::refunds::UnavailableDestinationScreener),
            contract_signatures: Arc::new(topup::treasuries::UnavailableContractSignatures),
        };
        return serve_read_only(args.bind, state, args.restore_report.clone()).await;
    }
    let routes = Arc::new(routes);
    let scanner_count = routes.chain_ids().count();
    let connection_count = u32::try_from(PUMPS)
        .ok()
        .zip(u32::try_from(scanner_count).ok())
        .and_then(|(pumps, scanners)| pumps.checked_add(scanners))
        // The API, and each mode's outbox worker, whose concurrent deliveries each hold a
        // connection only for a statement at a time, never during a request.
        .and_then(|count| count.checked_add(10))
        .context("route count is too large")?;
    let pool = connect("run", connection_count)
        .await
        .context("failed to connect to database")?;
    check_payment_settings_cutover(&pool).await?;
    let pricing_runtimes = topup::locks::pricing::PricingRuntime::build_all(&routes, pool.clone())
        .map_err(anyhow::Error::msg)
        .context("invalid rate-lock pricing configuration")?;
    let price_provider = Arc::new(topup::locks::ConfiguredQuoteProvider::from_runtimes(
        Arc::clone(&pricing_runtimes),
    ));
    let rate_lock_quotes: Arc<dyn topup::locks::QuoteProvider> = price_provider.clone();
    // A restore from backup freezes the service until the operator reconciles it; one that booted
    // straight into this compose is found by its new PostgreSQL timeline.
    let restore = topup::restore_mode::detect(&pool)
        .await
        .context("failed to check for a restore from backup")?;
    if let Some(restore) = &restore {
        tracing::error!(
            restore_id = %restore.id,
            "frozen after a restore from backup: merchant requests with an API key answer 503 \
             service_restoring, and crediting, settlement, quote expiry, treasury changes, refund \
             verification, and event delivery wait for POST /v1/admin/restore/unfreeze \
             (deploy/RESTORE.md)"
        );
    }
    topup::payment_config::report_invalid(&pool, &routes)
        .await
        .context("failed to check the accounts' payment settings")?;
    let Some(lease_owner) = wait_for_lease_owner_lock(&pool).await? else {
        return Ok(ExitCode::SUCCESS);
    };
    topup::db::rpc::check_start(&pool, &routes.chain_ids().collect::<Vec<_>>()).await?;
    if let Err(error) = topup::rpc_runtime::preflight(&routes).await {
        tracing::warn!(%error,"RPC self-test failed; affected endpoints remain not-ready while the API starts");
    }
    topup::scanner::initialize_cursors(&pool, &routes)
        .await
        .context("failed to initialize chain coverage")?;
    let signer = Arc::new(spawn_signer().context("failed to start signer actor")?);
    let delivery_config = topup::outbox::DeliveryConfig {
        proxy: webhook_proxy(args.webhook_proxy.as_ref(), &public_origin)?,
        ..topup::outbox::DeliveryConfig::default()
    };
    // Test and live events have separate workers, so test traffic cannot delay live deliveries.
    let delivery_workers = [false, true].map(|livemode| {
        topup::outbox::DeliveryWorker::new(
            pool.clone(),
            Arc::clone(&signer),
            livemode,
            delivery_config.clone(),
        )
    });
    let [test_delivery, live_delivery] = delivery_workers;
    let test_delivery = test_delivery.context("failed to configure webhook delivery")?;
    let live_delivery = live_delivery.context("failed to configure webhook delivery")?;
    let frozen = topup::reconciler::frozen_chains(&pool, &routes)
        .await
        .context("failed to load reconciliation blocks")?;
    for chain_id in frozen {
        tracing::error!(
            chain_id,
            "reconciliation froze configured chain; its scanner, pumps, finality watch, and \
             quote creation stay paused until the block is lifted"
        );
    }
    let reconciler = Arc::new(
        topup::reconciler::Reconciler::from_routes(pool.clone(), Arc::clone(&routes))
            .context("failed to configure reconciler")?,
    );
    let listener = tokio::net::TcpListener::bind(args.bind)
        .await
        .with_context(|| format!("failed to bind API listener on {}", args.bind))?;
    let confirm_step =
        ConfirmStep::from_routes(pool.clone(), &routes, Arc::clone(&pricing_runtimes))
            .context("invalid confirm-step configuration")?;
    let screen_step = ScreenStep::from_routes(pool.clone(), &routes)
        .context("failed to configure screening step")?;
    let steps = Arc::new(StepSet::new(Box::new(confirm_step), Box::new(screen_step)));
    let pump = Pump::new(
        pool.clone(),
        Arc::clone(&routes),
        Arc::<StepSet>::clone(&steps),
        pump_config,
    )
    .context("invalid pump configuration")?;
    let refund_reader = |index| {
        topup::refunds::EvmRefundChainReader::from_routes(pool.clone(), &routes, index)
            .context("failed to configure refund verification chain reader")
    };
    let refund_worker = topup::refunds::RefundVerificationWorker::new(
        pool.clone(),
        Arc::clone(&routes),
        refund_reader(0)?,
        refund_reader(1)?,
        topup::refunds::RefundVerificationConfig::default(),
    );
    let finality_watch =
        topup::finality::FinalityWatch::from_routes(pool.clone(), Arc::clone(&routes))
            .map_err(anyhow::Error::msg)
            .context("failed to configure the finality watch")?;
    let mut tasks = ServiceTasks::new();
    let screening = Arc::new(topup::refunds::CachedDestinationScreener::new(Arc::new(
        topup::refunds::OracleDestinationScreener::new(Arc::clone(&routes)),
    )));
    let state = topup::api::AppState {
        pool: pool.clone(),
        routes: Arc::clone(&routes),
        admin_key,
        maintenance_keys: config.maintenance_keys.clone(),
        public_origin,
        attestor: Arc::new(DstackAttestor::new()),
        rate_lock_quotes,
        client_reads: Arc::new(topup::api::ClientReadLimiter::new(
            client_secret_key().await?,
            connection_count,
        )),
        rate_limits: Arc::default(),
        screening: Arc::clone(&screening) as Arc<dyn topup::refunds::DestinationScreener>,
        contract_signatures: Arc::new(
            topup::treasuries::EvmContractSignatures::from_routes(pool.clone(), &routes)
                .map_err(anyhow::Error::msg)
                .context("failed to configure treasury proof checks")?,
        ),
    };
    let monitored_origin = state.public_origin.to_string();
    let (application, _) = topup::api::booting_router(state);
    tasks.spawn("API server", |cancellation| {
        axum::serve(
            topup::api::DeadlineListener::new(listener).with_connect_info(),
            application.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(cancellation.cancelled_owned())
        .into_future()
    });
    tracing::info!(bind = %args.bind, "API listening");

    let price_routes = Arc::clone(&routes);
    tasks.spawn("TWAP price sampler", |cancellation| async move {
        price_provider
            .sample_twaps(&price_routes, cancellation)
            .await;
    });
    let scanner_pool = pool.clone();
    let scanner_routes = Arc::clone(&routes);
    let scan_config = topup::scanner::ScanConfig;
    let finalized_heads = topup::scanner::FinalizedHeads::default();
    let scanner_heads = finalized_heads.clone();
    tasks.spawn("scanner", |cancellation| async move {
        topup::scanner::run(
            scanner_pool,
            &scanner_routes,
            scan_config,
            scanner_heads,
            cancellation,
        )
        .await
    });
    let watch_heads = finalized_heads.clone();
    // The scanner and the reconciler run while frozen after a restore: they rescan the chain from
    // the restored cursor. Every task that credits, settles, or announces waits for the unfreeze.
    tasks.spawn("finality watch", |cancellation| {
        after_unfreeze(pool.clone(), cancellation, |cancellation| async move {
            finality_watch.run(watch_heads, cancellation).await;
        })
    });
    tasks.spawn("refund verification worker", |cancellation| {
        after_unfreeze(pool.clone(), cancellation, |cancellation| async move {
            refund_worker.run(cancellation).await;
        })
    });
    for worker in 0..PUMPS {
        let worker_pump = pump.clone();
        tasks.spawn(format!("deposit pump {worker}"), |cancellation| {
            after_unfreeze(pool.clone(), cancellation, move |cancellation| async move {
                tracing::info!(worker, "deposit pump started");
                worker_pump
                    .run_with_instance(worker.to_string(), cancellation)
                    .await;
            })
        });
    }
    let age_alerter = AgeAlerter::new(pool.clone(), age_config, AGE_ALERT_INTERVAL);
    tasks.spawn("age alerter", |cancellation| {
        after_unfreeze(pool.clone(), cancellation, |cancellation| async move {
            age_alerter.run(cancellation).await;
        })
    });
    tasks.spawn("backup monitor", topup::observability::monitor_backup);
    let business_pool = pool.clone();
    tasks.spawn("business health monitor", |cancellation| {
        after_unfreeze(
            business_pool.clone(),
            cancellation,
            |cancellation| async move {
                topup::observability::monitor_business(
                    business_pool,
                    monitored_origin,
                    cancellation,
                )
                .await;
            },
        )
    });
    // The freeze keeps the restored state as it is; pruning waits, and a claim replaces an
    // expired key itself meanwhile.
    let idempotency_pruner =
        topup::api::IdempotencyKeyPruner::new(pool.clone(), IDEMPOTENCY_PRUNE_INTERVAL);
    tasks.spawn("idempotency key pruner", |cancellation| {
        after_unfreeze(pool.clone(), cancellation, |cancellation| async move {
            idempotency_pruner.run(cancellation).await;
        })
    });
    let expiry_worker =
        topup::locks::ExpiryWorker::new(pool.clone(), Arc::clone(&routes), Duration::from_secs(5));
    tasks.spawn("rate-lock expiry worker", |cancellation| {
        after_unfreeze(pool.clone(), cancellation, |cancellation| async move {
            expiry_worker.run(cancellation).await;
        })
    });
    let treasury_worker = topup::treasuries::TreasuryWorker::new(
        pool.clone(),
        Arc::clone(&routes),
        Arc::clone(&screening) as Arc<dyn topup::refunds::DestinationScreener>,
        Duration::from_secs(30),
    );
    tasks.spawn("treasury time-lock worker", |cancellation| {
        after_unfreeze(pool.clone(), cancellation, |cancellation| async move {
            treasury_worker.run(cancellation).await;
        })
    });
    tasks.spawn("test webhook delivery worker", |cancellation| {
        after_unfreeze(pool.clone(), cancellation, |cancellation| async move {
            test_delivery.run(cancellation).await;
        })
    });
    tasks.spawn("live webhook delivery worker", |cancellation| {
        after_unfreeze(pool.clone(), cancellation, |cancellation| async move {
            live_delivery.run(cancellation).await;
        })
    });
    let reconcile_interval = Duration::from_secs(args.reconcile_interval_s);
    tasks.spawn("reconciler", |cancellation| async move {
        reconciler
            .run_loop(reconcile_interval, finalized_heads, cancellation)
            .await;
    });

    tracing::info!(
        pumps = PUMPS,
        scanners = scanner_count,
        "topup service started"
    );
    // The watch cancels every task when the lock connection fails; it is reported below.
    let mut lease_owner_task = tokio::spawn(lease_owner.watch(LEASE_OWNER_PING, tasks.token()));
    let mut clean_shutdown = true;
    let mut lease_owner_finished = None;
    tokio::select! {
        signal = wait_for_shutdown_signal() => {
            if let Err(error) = signal {
                tracing::error!(%error, "failed to listen for shutdown signal");
                clean_shutdown = false;
            }
        }
        () = tasks.first_exit() => clean_shutdown = false,
        result = &mut lease_owner_task => {
            lease_owner_finished = Some(result);
        }
    }
    tracing::info!("shutdown requested; finishing in-flight service work");
    match tasks.shutdown().await {
        Ok(true) => {}
        Ok(false) => clean_shutdown = false,
        Err(_tasks) => {
            // Keep the lease-owner lock alive: returning would drop it while task futures may
            // still hold money-path resources. Process termination stops all threads first.
            tracing::error!(
                "service tasks did not stop after abort; terminating without unlocking"
            );
            std::process::exit(1);
        }
    }
    if !cleanup_service(
        lease_owner_task,
        lease_owner_finished,
        &pool,
        Duration::from_secs(15),
    )
    .await
    {
        clean_shutdown = false;
    }
    tracing::info!("topup service stopped");

    Ok(if clean_shutdown {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Bounded cleanup after all lease-holding tasks have stopped.
async fn cleanup_service(
    mut lease_owner_task: tokio::task::JoinHandle<
        Result<topup::reconciler::LeaseOwnerLock, topup::reconciler::ReconciliationError>,
    >,
    lease_owner_finished: Option<
        Result<
            Result<topup::reconciler::LeaseOwnerLock, topup::reconciler::ReconciliationError>,
            tokio::task::JoinError,
        >,
    >,
    pool: &PgPool,
    timeout: Duration,
) -> bool {
    let mut clean_shutdown = true;
    // Release the lease-owner lock only after every lease-holding task has stopped.
    let lease_owner = match tokio::time::timeout(timeout, async {
        match lease_owner_finished {
            Some(result) => result,
            None => (&mut lease_owner_task).await,
        }
    })
    .await
    {
        Ok(result) => Some(result),
        Err(_) => {
            tracing::error!("lease-owner task drain deadline exceeded");
            lease_owner_task.abort();
            clean_shutdown = false;
            None
        }
    };
    match lease_owner {
        Some(Ok(Ok(lock))) => {
            if let Err(error) = tokio::time::timeout(timeout, lock.release())
                .await
                .unwrap_or_else(|_| {
                    Err(topup::reconciler::ReconciliationError::LeaseOwnerLock(
                        "lease unlock deadline exceeded",
                    ))
                })
            {
                tracing::error!(%error, "failed to release the lease-owner lock");
                clean_shutdown = false;
            }
        }
        Some(Ok(Err(error))) => {
            tracing::error!(
                %error,
                "lease-owner lock connection failed; deposit processing was stopped"
            );
            clean_shutdown = false;
        }
        Some(Err(error)) => {
            tracing::error!(%error, "lease-owner lock task failed");
            clean_shutdown = false;
        }
        None => {}
    }
    if tokio::time::timeout(timeout, pool.close()).await.is_err() {
        tracing::error!("database pool close deadline exceeded");
        clean_shutdown = false;
    }
    clean_shutdown
}

/// Runs `task` once the service is not frozen after a restore (`topup::restore_mode`); nothing when
/// shutdown comes first.
async fn after_unfreeze<F>(
    pool: sqlx::PgPool,
    cancellation: CancellationToken,
    task: impl FnOnce(CancellationToken) -> F,
) where
    F: Future<Output = ()>,
{
    if topup::restore_mode::wait_until_unfrozen(
        &pool,
        topup::restore_mode::UNFREEZE_POLL_INTERVAL,
        &cancellation,
    )
    .await
    {
        task(cancellation).await;
    }
}

/// Serves only the read API of a database restored from backup (`deploy/RESTORE.md`), so the
/// operator can verify it through the product-signed lookups: no lease-owner lock, scanner, pump,
/// webhook delivery, reconciler, or signer runs, and every non-GET request is refused.
async fn serve_read_only(
    bind: std::net::SocketAddr,
    state: topup::api::AppState,
    report: Option<PathBuf>,
) -> anyhow::Result<ExitCode> {
    let pool = state.pool.clone();
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind API listener on {bind}"))?;
    let application = topup::api::read_only_router(state, report);
    tracing::warn!(%bind, "API listening read-only (--read-only)");
    serve_read_only_until(
        listener,
        application,
        pool,
        wait_for_shutdown_signal(),
        Duration::from_secs(300),
        Duration::from_secs(15),
    )
    .await
}

async fn serve_read_only_until(
    listener: tokio::net::TcpListener,
    application: axum::Router,
    pool: PgPool,
    shutdown: impl Future<Output = std::io::Result<()>>,
    drain_timeout: Duration,
    close_timeout: Duration,
) -> anyhow::Result<ExitCode> {
    let cancellation = CancellationToken::new();
    let served = axum::serve(
        topup::api::DeadlineListener::new(listener).with_connect_info(),
        application.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(cancellation.clone().cancelled_owned())
    .into_future();
    tokio::pin!(served);
    let mut signal_failed = false;
    let result = tokio::select! {
        result = &mut served => Some(result),
        signal = shutdown => {
            if let Err(error) = signal {
                signal_failed = true;
                tracing::error!(%error, "failed to listen for shutdown signal");
            }
            cancellation.cancel();
            match tokio::time::timeout(drain_timeout, &mut served).await {
                Ok(result) => Some(result),
                Err(_) => {
                    tracing::error!("read-only API drain deadline exceeded");
                    None
                }
            }
        }
    };
    let closed = tokio::time::timeout(close_timeout, pool.close()).await;
    if closed.is_err() {
        tracing::error!("read-only database pool close deadline exceeded");
    }
    match result {
        Some(result) => result.context("read-only API failed")?,
        None => return Ok(ExitCode::FAILURE),
    }
    if closed.is_err() || signal_failed {
        return Ok(ExitCode::FAILURE);
    }
    tracing::info!("read-only API stopped");
    Ok(ExitCode::SUCCESS)
}

async fn rpc_command(command: RpcCommand) -> anyhow::Result<ExitCode> {
    let RpcCommand::Check { config: file } = command;
    let config = load_config(&file)?;
    config
        .check_secrets(|name| std::env::var(name).ok())
        .map_err(anyhow::Error::msg)?;
    let routes = config.route_set().map_err(anyhow::Error::msg)?;
    let ids = topup::rpc_runtime::preflight(&routes)
        .await
        .map_err(anyhow::Error::msg)?;
    println!("{}", serde_json::to_string(&ids)?);
    Ok(ExitCode::SUCCESS)
}

async fn reconcile(args: &ReconcileArgs) -> anyhow::Result<ExitCode> {
    let config = load_config(&args.config)?;
    let routes = config
        .route_set()
        .map_err(anyhow::Error::msg)
        .context("failed to load the route configuration")?;
    let pool = connect("reconcile", 4)
        .await
        .context("failed to connect to database")?;
    check_payment_settings_cutover(&pool).await?;
    topup::rpc_runtime::preflight(&routes)
        .await
        .map_err(anyhow::Error::msg)?;
    let reconciler = topup::reconciler::Reconciler::from_routes(pool.clone(), Arc::new(routes))
        .context("failed to configure reconciler")?;
    let result = match topup::reconciler::hold_lease_owner_lock(&pool).await {
        Ok(lease_owner) => {
            let result = reconciler.run_once().await;
            if let Err(error) = lease_owner.release().await {
                tracing::warn!(%error, "failed to release the lease-owner lock");
            }
            result
        }
        Err(error) => Err(error),
    };
    pool.close().await;
    let report = result.context("reconciliation failed")?;
    if !report.succeeded() {
        tracing::error!(
            findings = report.findings.len(),
            failed_checks = report.failed_checks.len(),
            "reconciliation completed with failed checks"
        );
        return Ok(ExitCode::FAILURE);
    }
    tracing::info!(findings = report.findings.len(), "reconciliation completed");
    Ok(ExitCode::SUCCESS)
}

/// Long-running service tasks sharing one cancellation token.
///
/// A task that exits before shutdown stops the service: `run` then cancels the rest.
struct ServiceTasks {
    set: JoinSet<Result<(), String>>,
    names: HashMap<tokio::task::Id, String>,
    cancellation: CancellationToken,
}

/// Result of a service task, normalized for reporting.
trait TaskOutcome {
    fn into_outcome(self) -> Result<(), String>;
}

impl TaskOutcome for () {
    fn into_outcome(self) -> Result<(), String> {
        Ok(())
    }
}

impl<E: std::fmt::Display> TaskOutcome for Result<(), E> {
    fn into_outcome(self) -> Result<(), String> {
        self.map_err(|error| error.to_string())
    }
}

impl ServiceTasks {
    fn new() -> Self {
        Self {
            set: JoinSet::new(),
            names: HashMap::new(),
            cancellation: CancellationToken::new(),
        }
    }

    fn token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    fn spawn<F>(&mut self, name: impl Into<String>, task: impl FnOnce(CancellationToken) -> F)
    where
        F: Future + Send + 'static,
        F::Output: TaskOutcome,
    {
        let task = task(self.cancellation.clone());
        let handle = self.set.spawn(async move { task.await.into_outcome() });
        self.names.insert(handle.id(), name.into());
    }

    fn name(&self, id: tokio::task::Id) -> &str {
        self.names.get(&id).map_or("unknown", String::as_str)
    }

    /// Waits for the first task to exit and reports it.
    async fn first_exit(&mut self) {
        let Some(result) = self.set.join_next_with_id().await else {
            return std::future::pending().await;
        };
        match result {
            // After a cancellation (a failed lease-owner lock) exits are expected; the cause is
            // reported separately.
            Ok((_, Ok(()))) if self.cancellation.is_cancelled() => {}
            Ok((id, Ok(()))) => {
                tracing::error!(task = self.name(id), "service task stopped before shutdown");
            }
            Ok((id, Err(error))) => {
                tracing::error!(task = self.name(id), %error, "service task failed");
            }
            Err(error) => {
                tracing::error!(task = self.name(error.id()), %error, "service task failed to join");
            }
        }
    }

    /// Returns cleanliness only after every task future has dropped. On failure to join, the
    /// caller must terminate without releasing the lease-owner lock.
    async fn shutdown(self) -> Result<bool, Self> {
        // 285s graceful + 15s abort/join + 45s lock/pool cleanup fits the 360s Compose grace.
        self.shutdown_with_deadlines(Duration::from_secs(285), Duration::from_secs(15))
            .await
    }

    async fn shutdown_with_deadlines(
        mut self,
        drain_timeout: Duration,
        abort_timeout: Duration,
    ) -> Result<bool, Self> {
        self.cancellation.cancel();
        let mut clean = true;
        let drain = async {
            while let Some(result) = self.set.join_next_with_id().await {
                match result {
                    Ok((_, Ok(()))) => {}
                    Ok((id, Err(error))) => {
                        tracing::error!(
                            task = self.name(id),
                            %error,
                            "service task failed during shutdown"
                        );
                        clean = false;
                    }
                    Err(error) => {
                        tracing::error!(
                            task = self.name(error.id()),
                            %error,
                            "service task failed to join during shutdown"
                        );
                        clean = false;
                    }
                }
            }
        };
        if tokio::time::timeout(drain_timeout, drain).await.is_err() {
            tracing::error!("shutdown drain deadline exceeded; aborting service tasks");
            self.set.abort_all();
            // Abort is a request, not proof of completion. Joining proves that task futures and
            // their held resources have dropped before the advisory lock can be released.
            let join = async { while self.set.join_next().await.is_some() {} };
            if tokio::time::timeout(abort_timeout, join).await.is_err() {
                tracing::error!("service task abort/join deadline exceeded");
                return Err(self);
            }
            clean = false;
        }
        Ok(clean)
    }
}

/// Interval between liveness pings on the lease-owner lock connection.
const LEASE_OWNER_PING: Duration = Duration::from_secs(5);

/// Takes the lease-owner lock, retrying with backoff while the post-restore gate holds it;
/// `None` means shutdown was requested first.
///
/// Waiting instead of exiting keeps a restart policy from crash-looping during a restore.
async fn wait_for_lease_owner_lock(
    pool: &sqlx::PgPool,
) -> anyhow::Result<Option<topup::reconciler::LeaseOwnerLock>> {
    let mut delay = Duration::from_secs(1);
    let shutdown = wait_for_shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        match topup::reconciler::hold_lease_owner_lock(pool).await {
            Ok(lock) => return Ok(Some(lock)),
            Err(topup::reconciler::ReconciliationError::LeaseOwnerLock(reason)) => {
                tracing::warn!(
                    reason,
                    retry_in_s = delay.as_secs(),
                    "waiting for the lease-owner lock before processing deposits"
                );
            }
            Err(error) => return Err(error).context("failed to take the lease-owner lock"),
        }
        tokio::select! {
            signal = &mut shutdown => {
                signal.context("failed to listen for shutdown signal")?;
                return Ok(None);
            }
            () = tokio::time::sleep(delay) => {}
        }
        delay = delay.saturating_mul(2).min(Duration::from_secs(60));
    }
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};

    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        received = terminate.recv() => received
            .ok_or_else(|| Error::new(ErrorKind::BrokenPipe, "SIGTERM listener closed")),
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}

fn load_config(path: &Path) -> anyhow::Result<topup::config::Config> {
    topup::config::Config::load(path)
        .map_err(anyhow::Error::msg)
        .context("failed to load the service configuration")
}

/// The egress proxy of webhook deliveries, `--webhook-proxy`: the smokescreen sidecar, the only
/// filter of the addresses a merchant's URL may reach (design §8). It is required unless the
/// service's own origin is `http`, which only local stacks use.
fn webhook_proxy(
    proxy: Option<&reqwest::Url>,
    public_origin: &topup::api::PublicOrigin,
) -> anyhow::Result<Option<reqwest::Url>> {
    match proxy {
        Some(proxy) => Ok(Some(proxy.clone())),
        None if public_origin.to_string().starts_with("http://") => Ok(None),
        None => bail!(
            "--webhook-proxy is required for run: webhooks reach merchants only through the \
             egress proxy"
        ),
    }
}

/// `DATABASE_URL`, the database login of this container (libpq reads its password from
/// `PGPASSFILE`).
fn database_url(command: &'static str) -> anyhow::Result<String> {
    std::env::var("DATABASE_URL")
        .ok()
        .filter(|value| !value.is_empty())
        .with_context(|| format!("DATABASE_URL is required for {command}"))
}

/// Connects a pool of at most `max_connections` to `DATABASE_URL`.
async fn connect(command: &'static str, max_connections: u32) -> anyhow::Result<PgPool> {
    let url = database_url(command)?;
    Ok(topup::db::connect(&url, command, max_connections).await?)
}

/// [`connect`] for the commands that create roles and repair the ledger (`migrate`,
/// `restore-check`): the login must own the database or be a superuser, so a container given the
/// application login fails here rather than part-way through.
async fn connect_owner(command: &'static str, max_connections: u32) -> anyhow::Result<PgPool> {
    let pool = connect(command, max_connections).await?;
    let (user, owner): (String, bool) = sqlx::query_as(
        "SELECT current_user::text, \
                pg_catalog.pg_get_userbyid(d.datdba) = current_user \
                    OR (SELECT rolsuper FROM pg_catalog.pg_roles WHERE rolname = current_user) \
         FROM pg_catalog.pg_database d WHERE d.datname = current_database()",
    )
    .fetch_one(&pool)
    .await
    .context("failed to read the database login's role")?;
    if !owner {
        pool.close().await;
        bail!("{command} requires the database owner; DATABASE_URL logs in as `{user}`");
    }
    Ok(pool)
}

/// Queue depth of every dstack signer actor.
const SIGNER_QUEUE: NonZeroUsize =
    NonZeroUsize::new(32).expect("constant signer queue is non-zero");
/// Maximum duration of one dstack signing request.
const SIGNER_TIMEOUT: Duration = Duration::from_secs(15);

/// Starts the dstack webhook signer actor.
/// The key of quotes' and deposit addresses' client secrets, derived from dstack like the
/// service's other keys, so every release and CVM of the application checks the same secrets.
async fn client_secret_key() -> anyhow::Result<topup::client_secret::ClientSecretKey> {
    DstackSigner::new()
        .derive_secret(CLIENT_SECRET_KEY_DOMAIN)
        .await
        .map(topup::client_secret::ClientSecretKey::new)
        .map_err(|_| anyhow::anyhow!("failed to derive the client-secret key"))
}

fn spawn_signer() -> std::io::Result<SignerHandle> {
    SignerHandle::spawn(DstackSigner::new(), SIGNER_QUEUE, SIGNER_TIMEOUT)
}

/// Refuses old or unfinished cutovers before the migration runner can change the database.
async fn check_payment_settings_migration(pool: &PgPool) -> anyhow::Result<()> {
    let mut connection = pool.acquire().await?;
    let (has_migrations, has_cutover): (bool, bool) = sqlx::query_as(
        "SELECT to_regclass('public._sqlx_migrations') IS NOT NULL, \
                to_regclass('public.payment_settings_cutover') IS NOT NULL",
    )
    .fetch_one(&mut *connection)
    .await
    .context("failed to read the database migration tables")?;
    if has_migrations {
        let needs_cutover: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM _sqlx_migrations WHERE success) \
                    AND NOT EXISTS (SELECT 1 FROM _sqlx_migrations \
                                    WHERE version = 20261024000000 AND success)",
        )
        .fetch_one(&mut *connection)
        .await
        .context("failed to read the payment settings migration status")?;
        if needs_cutover {
            bail!("payment settings cutover is incomplete; upgrade through 0.9.x first");
        }
    }
    if has_cutover
        && topup::payment_config::cutover_incomplete(&mut connection)
            .await
            .context("failed to read the payment settings cutover")?
    {
        bail!("payment settings cutover is incomplete; upgrade through 0.9.x first");
    }
    Ok(())
}

async fn migrate() -> anyhow::Result<ExitCode> {
    let pool = connect_owner("migrate", 1)
        .await
        .context("failed to connect to database")?;
    check_payment_settings_migration(&pool).await?;
    topup::db::migrate(&pool)
        .await
        .context("failed to apply database migrations")?;
    tracing::info!("database migrations applied");
    Ok(ExitCode::SUCCESS)
}

fn show_route(file: &Path, template: bool) -> ExitCode {
    let resolved = std::fs::read_to_string(file)
        .map_err(|error| format!("failed to read route file `{}`: {error}", file.display()))
        .and_then(|yaml| {
            route::parse_and_validate(&yaml, template)
                .map_err(|error| format!("route file `{}` is invalid: {error}", file.display()))
        })
        .and_then(|route| route::resolved_json(&route));
    match resolved {
        Ok(json) => {
            print!("{json}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn check_config(file: &Path, secrets: bool, require_sentry: bool) -> ExitCode {
    if require_sentry
        && topup::observability::require_sentry_dsn(std::env::var("SENTRY_DSN").ok().as_deref())
            .is_err()
    {
        eprintln!("production preflight requires a present, valid SENTRY_DSN");
        return ExitCode::FAILURE;
    }
    match topup::config::Config::load(file) {
        Ok(config) => {
            if secrets && let Err(error) = config.check_environment_secrets() {
                eprintln!("{error}");
                return ExitCode::FAILURE;
            }
            println!(
                "configuration `{}` is valid: {} routes, {} RPC providers{}{}",
                file.display(),
                config.routes.len(),
                config.rpc.len().saturating_mul(2),
                if secrets {
                    ", every provider key fits its URL"
                } else {
                    "; provider keys were not checked"
                },
                if config.public_origin.is_none() || config.admin_key.is_none() {
                    "; the origin or the admin key is left to `topup run`'s environment"
                } else {
                    ""
                }
            );
            for route in &config.routes {
                println!(
                    "route {} price {:?}; staging opt-in: {}",
                    route.route, route.pricing.mode, route.pricing.allow_unclear_sources
                );
                if !route.livemode {
                    println!(
                        "  testnet valuation: mainnet feeds and exchange markets; test tokens have no market"
                    );
                }
                for (role, sources) in route.pricing.roles() {
                    for (order, source) in sources.iter().enumerate() {
                        println!(
                            "  {role}[{order}]: {}",
                            serde_json::to_string(source)
                                .unwrap_or_else(|_| "invalid source".into())
                        );
                        if let topup_core::price::Source::Chainlink {
                            feed,
                            chain_id,
                            observation_chain_id,
                            ..
                        } = source
                            && let Some(metadata) = topup_core::price::feed(feed, *chain_id)
                        {
                            println!(
                                "    address={} decimals={} heartbeat_s={} margin_s={} deviation_bps={} observation_chain_id={:?}",
                                metadata.address,
                                metadata.decimals,
                                metadata.heartbeat_s,
                                metadata.margin_s,
                                metadata.deviation_bps,
                                observation_chain_id
                            );
                        }
                        if let topup_core::price::Source::UniswapV2Twap { twap, .. } = source {
                            println!(
                                "    TWAP={twap:?}; sample_interval_s={}; ETH/USD={:?}",
                                if config.environment == "staging" {
                                    topup_adapters::pricing::uniswap_v2::SAMPLE_INTERVAL_S
                                } else {
                                    60
                                },
                                topup_core::price::feed("ETH_USD", 1)
                            );
                        }
                        if let Some(provider) = topup_core::price::provider(source.company()) {
                            println!(
                                "    licensing={:?}; owner={}; rate_limit={}",
                                provider.verdict, provider.legal_owner, provider.rate_limit
                            );
                        }
                    }
                }
                if let Some(sequencer) = &route.pricing.sequencer_uptime {
                    println!(
                        "  sequencer: {:?}; {:?}",
                        sequencer,
                        topup_core::price::feed(&sequencer.feed, 8453)
                    );
                }
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn show_config(file: &Path) -> ExitCode {
    match topup::config::Config::load(file).and_then(|config| config.resolved_json()) {
        Ok(json) => {
            print!("{json}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn validate_route(file: &Path, template: bool) -> ExitCode {
    let yaml = match std::fs::read_to_string(file) {
        Ok(yaml) => yaml,
        Err(error) => {
            eprintln!("failed to read route file `{}`: {error}", file.display());
            return ExitCode::FAILURE;
        }
    };
    match route::parse_and_validate(&yaml, template) {
        Ok(_) => {
            let kind = if template {
                "route template"
            } else {
                "route file"
            };
            println!(
                "{kind} `{}` is valid at schema level; on-chain deployment and Safe control were not checked",
                file.display()
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("route file `{}` is invalid: {error}", file.display());
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{ServiceTasks, parse_nonce};

    #[test]
    fn synthetic_alert_cli_accepts_every_business_alert_and_rejects_unknown_names() {
        use clap::Parser;
        for alert in <super::SyntheticAlert as clap::ValueEnum>::value_variants() {
            let value = clap::ValueEnum::to_possible_value(alert).unwrap();
            assert!(
                super::Cli::try_parse_from([
                    "topup",
                    "alert-test",
                    "--alert",
                    value.get_name(),
                    "--severity",
                    "critical"
                ])
                .is_ok()
            );
        }
        assert!(super::Cli::try_parse_from(["topup", "alert-test", "--alert", "unknown"]).is_err());
        assert!(
            super::Cli::try_parse_from([
                "topup",
                "config",
                "check",
                "--require-sentry",
                "topup.yaml"
            ])
            .is_ok()
        );
    }

    #[tokio::test]
    async fn read_only_shutdown_bounds_a_stalled_request_and_closes_the_pool() {
        use std::time::Duration;
        use tokio_util::sync::CancellationToken;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let entered = CancellationToken::new();
        let handler_entered = entered.clone();
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(move || {
                let entered = handler_entered.clone();
                async move {
                    entered.cancel();
                    std::future::pending::<&'static str>().await
                }
            }),
        );
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1/unused")
            .unwrap();
        let observed_pool = pool.clone();
        let server = tokio::spawn(super::serve_read_only_until(
            listener,
            app,
            pool,
            async move {
                entered.cancelled().await;
                Ok(())
            },
            Duration::from_millis(20),
            Duration::from_millis(20),
        ));
        let request = tokio::spawn(async move { reqwest::get(format!("http://{address}/")).await });
        let result = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(result, std::process::ExitCode::FAILURE);
        assert!(observed_pool.is_closed());
        request.abort();
    }

    #[tokio::test]
    async fn lease_owner_drain_timeout_still_closes_the_pool() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1/unused")
            .unwrap();
        let task = tokio::spawn(std::future::pending());
        let aborted = task.abort_handle();
        assert!(
            !super::cleanup_service(task, None, &pool, std::time::Duration::from_millis(20)).await
        );
        assert!(pool.is_closed());
        tokio::task::yield_now().await;
        assert!(aborted.is_finished());
    }

    #[tokio::test]
    async fn read_only_shutdown_bounds_pool_close_with_an_outstanding_connection() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            assert!(
                std::env::var("CI").is_err(),
                "DATABASE_URL is required in CI"
            );
            return;
        };
        let pool = topup::db::connect(&url, "run", 1).await.unwrap();
        let connection = pool.acquire().await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            super::serve_read_only_until(
                listener,
                axum::Router::new(),
                pool.clone(),
                async { Ok(()) },
                std::time::Duration::from_millis(20),
                std::time::Duration::from_millis(20),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result, std::process::ExitCode::FAILURE);
        assert!(pool.is_closed());
        drop(connection);
    }

    struct TaskCleanup {
        completed: Arc<AtomicBool>,
        delay: std::time::Duration,
    }

    impl Drop for TaskCleanup {
        fn drop(&mut self) {
            // Model a task whose resource cleanup continues after abort is requested.
            std::thread::sleep(self.delay);
            self.completed.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timed_out_tasks_finish_cleanup_before_unlock_is_allowed() {
        use std::time::Duration;
        let lease_owner = if let Ok(url) = std::env::var("DATABASE_URL") {
            let pool = topup::db::connect(&url, "run", 2).await.unwrap();
            let lock = topup::reconciler::hold_lease_owner_lock(&pool)
                .await
                .unwrap();
            Some((pool, lock))
        } else {
            assert!(
                std::env::var("CI").is_err(),
                "DATABASE_URL is required in CI"
            );
            None
        };
        let mut tasks = ServiceTasks::new();
        let completed = Arc::new(AtomicBool::new(false));
        let cleanup = TaskCleanup {
            completed: completed.clone(),
            delay: Duration::from_millis(50),
        };
        let entered = tokio_util::sync::CancellationToken::new();
        let started = entered.clone();
        tasks.spawn("ignores cancellation", |_| async move {
            let _cleanup = cleanup;
            started.cancel();
            std::future::pending::<()>().await;
        });
        entered.cancelled().await;
        let result = tasks
            .shutdown_with_deadlines(Duration::from_millis(20), Duration::from_secs(1))
            .await;
        // This is the same gate as run(): only a successful join authorizes lock cleanup.
        match result {
            Ok(clean) => {
                assert!(!clean, "forced abort is an unclean shutdown");
                assert!(
                    completed.load(Ordering::SeqCst),
                    "task resources must finish dropping before unlock"
                );
                if let Some((pool, lock)) = lease_owner {
                    // Exercise the same explicit advisory unlock and pool-close path as run(),
                    // with the lock held throughout the task's delayed resource cleanup.
                    let watch = tokio::spawn(async move { Ok(lock) });
                    assert!(
                        super::cleanup_service(watch, None, &pool, Duration::from_secs(1)).await
                    );
                    assert!(pool.is_closed());
                }
            }
            Err(_) => panic!("the aborted task should have finished cleanup"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unjoined_task_cannot_authorize_lock_cleanup() {
        use std::time::Duration;
        let mut tasks = ServiceTasks::new();
        let completed = Arc::new(AtomicBool::new(false));
        let cleanup = TaskCleanup {
            completed: completed.clone(),
            delay: Duration::from_millis(200),
        };
        let entered = tokio_util::sync::CancellationToken::new();
        let started = entered.clone();
        tasks.spawn("slow cleanup", |_| async move {
            let _cleanup = cleanup;
            started.cancel();
            std::future::pending::<()>().await;
        });
        entered.cancelled().await;
        let mut retained = match tasks
            .shutdown_with_deadlines(Duration::from_millis(20), Duration::from_millis(20))
            .await
        {
            Err(tasks) => tasks,
            Ok(_) => panic!("unjoined resources must not authorize lease unlock"),
        };
        assert!(!completed.load(Ordering::SeqCst));
        // Production terminates without dropping the lock here. The test joins its task to leave
        // no running work in the test runtime.
        tokio::time::timeout(Duration::from_secs(2), retained.set.join_next())
            .await
            .unwrap();
        assert!(completed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn read_only_signal_error_fails_after_bounded_cleanup() {
        use std::time::Duration;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1/unused")
            .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            super::serve_read_only_until(
                listener,
                axum::Router::new(),
                pool.clone(),
                async { Err(std::io::Error::other("signal setup failed")) },
                Duration::from_millis(20),
                Duration::from_millis(20),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result, std::process::ExitCode::FAILURE);
        assert!(pool.is_closed());
    }

    #[test]
    fn nonce_policy_accepts_one_through_thirty_two_bytes() {
        assert_eq!(parse_nonce("00"), Ok(vec![0]));
        assert_eq!(parse_nonce(&"ab".repeat(32)), Ok(vec![0xab; 32]));
    }

    #[test]
    fn nonce_policy_rejects_empty_oversized_and_invalid_values() {
        assert_eq!(parse_nonce(""), Err("nonce must be non-empty hexadecimal"));
        assert_eq!(
            parse_nonce(&"ab".repeat(33)),
            Err("nonce must be at most 32 bytes (64 hexadecimal characters)")
        );
        assert_eq!(parse_nonce("0"), Err("nonce must be valid hexadecimal"));
        assert_eq!(parse_nonce("zz"), Err("nonce must be valid hexadecimal"));
    }

    #[tokio::test]
    async fn an_early_task_exit_is_reported_and_shutdown_stops_every_task() {
        let mut tasks = ServiceTasks::new();
        let stopped = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&stopped);
        tasks.spawn("long-running", |cancellation| async move {
            cancellation.cancelled().await;
            observed.store(true, Ordering::SeqCst);
        });
        tasks.spawn("failing", |_| async { Err::<(), _>("boom") });

        tokio::time::timeout(std::time::Duration::from_secs(5), tasks.first_exit())
            .await
            .expect("the failing task exits first");
        assert!(!stopped.load(Ordering::SeqCst));
        assert!(
            matches!(tasks.shutdown().await, Ok(true)),
            "the remaining task stops cleanly"
        );
        assert!(stopped.load(Ordering::SeqCst));

        let mut tasks = ServiceTasks::new();
        tasks.spawn("failing on shutdown", |cancellation| async move {
            cancellation.cancelled().await;
            Err::<(), _>("shutdown failure")
        });
        assert!(
            matches!(tasks.shutdown().await, Ok(false)),
            "a failure during shutdown is unclean"
        );
    }
}
