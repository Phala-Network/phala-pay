#![allow(dead_code)]

pub mod chain;
pub mod sanctions;
pub mod seed;
pub mod tls;

use std::env;
use std::future::{Future, poll_fn};
use std::panic::{self, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::Poll;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::body::Body;
use axum::http::{Method, Request};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer as _, SigningKey};
use sfv::{DictSerializer, Integer, KeyRef, ListSerializer, StringRef};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, Executor, PgPool};
use url::Url;
use uuid::Uuid;

/// Waits out transient `max_connections` exhaustion when many test databases share one
/// server under load; sqlx's 30 s default turns that into spurious `PoolTimedOut` failures.
pub const DB_ACQUIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

pub type TestFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + 'a>>;

/// An isolated, migrated database with an owner connection and a `topup_app` login.
pub struct TestDatabase {
    admin_pool: PgPool,
    /// Connects as the migration owner; only for migrations and fault injection.
    pub owner_pool: PgPool,
    pub app_pool: PgPool,
    pub owner_url: String,
    pub app_url: String,
    database_name: String,
    app_role: String,
}

/// Creates the cluster-wide `topup_app` role before any per-test database is migrated.
///
/// Roles are shared by every database in the cluster, so parallel per-test migrations of
/// `20260922000000` would otherwise race on `CREATE ROLE` on a fresh cluster. The advisory lock
/// key is shared by every test binary; the migration then finds the role and skips creating it.
pub async fn ensure_app_role(admin_pool: &PgPool) -> Result<()> {
    let mut transaction = admin_pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(704_200_000)")
        .execute(&mut *transaction)
        .await?;
    transaction
        .execute(
            "DO $$ BEGIN \
             IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'topup_app') THEN \
             CREATE ROLE topup_app NOLOGIN; \
             END IF; \
             END $$",
        )
        .await?;
    transaction
        .commit()
        .await
        .context("create topup_app role")?;
    Ok(())
}

impl TestDatabase {
    pub async fn create() -> Result<Option<Self>> {
        Self::create_with_migrations(true).await
    }

    /// The published-image drill starts with N-1's own migration entrypoint.
    pub async fn create_with_migrations(migrate: bool) -> Result<Option<Self>> {
        let Some(owner_template) = required_url("OWNER_DATABASE_URL")? else {
            return Ok(None);
        };
        let Some(app_template) = required_url("DATABASE_URL")? else {
            return Ok(None);
        };

        let mut admin_url = Url::parse(&owner_template)?;
        admin_url.set_path("/postgres");
        let admin_pool = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(DB_ACQUIRE_TIMEOUT)
            .connect(admin_url.as_str())
            .await?;
        ensure_app_role(&admin_pool).await?;
        sqlx::query("SELECT pg_advisory_lock(704_209_001)")
            .execute(&admin_pool)
            .await?;

        let suffix = Uuid::new_v4().simple().to_string();
        let database_name = format!("topup_test_{suffix}");
        let app_role = format!("topup_test_app_{suffix}");
        let password = format!("test_{suffix}");
        admin_pool
            .execute(AssertSqlSafe(format!(
                "CREATE DATABASE \"{database_name}\""
            )))
            .await?;

        let mut owner_url = Url::parse(&owner_template)?;
        owner_url.set_path(&format!("/{database_name}"));
        let owner_pool = topup::db::connect(owner_url.as_str(), "migrate", 4).await?;
        if migrate {
            topup::db::migrate(&owner_pool).await?;
        }

        admin_pool
            .execute(AssertSqlSafe(format!(
                "CREATE ROLE \"{app_role}\" LOGIN PASSWORD '{password}' IN ROLE topup_app"
            )))
            .await?;
        let mut app_url = Url::parse(&app_template)?;
        app_url
            .set_username(&app_role)
            .map_err(|()| anyhow::anyhow!("DATABASE_URL cannot accept a username"))?;
        app_url
            .set_password(Some(&password))
            .map_err(|()| anyhow::anyhow!("DATABASE_URL cannot accept a password"))?;
        app_url.set_path(&format!("/{database_name}"));
        let app_pool = topup::db::connect(app_url.as_str(), "run", 8).await?;

        sqlx::query("SELECT pg_advisory_unlock(704_209_001)")
            .execute(&admin_pool)
            .await?;
        Ok(Some(Self {
            admin_pool,
            owner_pool,
            app_pool,
            owner_url: owner_url.into(),
            app_url: app_url.into(),
            database_name,
            app_role,
        }))
    }

    pub async fn cleanup(self) -> Result<()> {
        self.app_pool.close().await;
        self.owner_pool.close().await;
        self.admin_pool
            .execute(AssertSqlSafe(format!(
                "DROP DATABASE \"{}\" WITH (FORCE)",
                self.database_name
            )))
            .await
            .context("drop test database")?;
        self.admin_pool
            .execute(AssertSqlSafe(format!("DROP ROLE \"{}\"", self.app_role)))
            .await
            .context("drop test role")?;
        self.admin_pool.close().await;
        Ok(())
    }
}

/// Runs `test` against a fresh [`TestDatabase`] and drops it afterwards, or skips when the
/// database URLs are not set. A panicking test still drops its database and role before the
/// panic resumes.
pub async fn with_database<F>(test: F) -> Result<()>
where
    F: for<'a> FnOnce(&'a TestDatabase) -> TestFuture<'a>,
{
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let outcome = {
        let mut future = test(&database);
        poll_fn(|context| {
            match panic::catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(context))) {
                Ok(Poll::Pending) => Poll::Pending,
                Ok(Poll::Ready(result)) => Poll::Ready(Ok(result)),
                Err(payload) => Poll::Ready(Err(payload)),
            }
        })
        .await
    };
    let cleanup = database.cleanup().await;
    match outcome {
        Ok(result) => result.and(cleanup),
        Err(payload) => panic::resume_unwind(payload),
    }
}

/// Gives the quote `quote` a client secret tagged with `client_reads`' key, as `POST /v1/quotes`
/// does, so a test reads the payer's view of a quote it seeded; returns the secret.
pub async fn issue_client_secret(
    pool: &PgPool,
    client_reads: &topup::api::ClientReadLimiter,
    quote: Uuid,
) -> Result<String> {
    let secret = client_reads
        .key()
        .issue("acct_test", &topup::locks::quote_id(quote))
        .map_err(|_| anyhow::anyhow!("no entropy for a client secret"))?;
    sqlx::query("UPDATE quotes SET client_secret_hash = $2 WHERE id = $1")
        .bind(quote)
        .bind(Sha256::digest(secret.as_bytes()).as_slice())
        .execute(pool)
        .await?;
    Ok(secret)
}

/// The payer's view of the quote whose client secret is `secret`, read without credentials.
pub async fn client_quote(app: &axum::Router, secret: &str) -> Result<serde_json::Value> {
    use tower::ServiceExt as _;
    let id = secret
        .split("_secret_")
        .next()
        .context("quote client secret")?;
    let response = app
        .clone()
        .oneshot(
            Request::get(format!("/v1/quotes/{id}?client_secret={secret}")).body(Body::empty())?,
        )
        .await?;
    anyhow::ensure!(
        response.status() == axum::http::StatusCode::OK,
        "{}",
        response.status()
    );
    let body = axum::body::to_bytes(response.into_body(), 1_048_576).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// Public origin the test routers are configured with and requests are signed for by default.
pub const TEST_ORIGIN: &str = "http://api.test";

/// A merchant request authenticated with `Authorization: Bearer {api_key}`.
pub fn merchant_request(method: Method, path: &str, body: Vec<u8>, api_key: &str) -> Request<Body> {
    merchant_request_builder(method, path, api_key)
        .body(Body::from(body))
        .expect("test request must be valid")
}

/// A merchant request that also sends `Idempotency-Key`.
pub fn merchant_request_with_key(
    method: Method,
    path: &str,
    body: Vec<u8>,
    api_key: &str,
    idempotency_key: &str,
) -> Request<Body> {
    merchant_request_builder(method, path, api_key)
        .header("idempotency-key", idempotency_key)
        .body(Body::from(body))
        .expect("test request must be valid")
}

fn merchant_request_builder(
    method: Method,
    path: &str,
    api_key: &str,
) -> axum::http::request::Builder {
    Request::builder()
        .method(method)
        .uri(format!("{TEST_ORIGIN}{path}"))
        .header("host", "api.test")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {api_key}"))
}

pub fn public_key_base64(key: &SigningKey) -> String {
    STANDARD.encode(key.verifying_key().as_bytes())
}

pub fn signed_request(
    method: Method,
    path: &str,
    body: Vec<u8>,
    kid: &str,
    key: &SigningKey,
    created: i64,
) -> Request<Body> {
    signed_request_with_options(
        method,
        path,
        body,
        kid,
        key,
        created,
        &SignatureOptions::default(),
    )
}

/// A default-signed request that also sends and covers `Idempotency-Key`.
pub fn signed_request_with_key(
    method: Method,
    path: &str,
    body: Vec<u8>,
    kid: &str,
    key: &SigningKey,
    created: i64,
    idempotency_key: &str,
) -> Request<Body> {
    signed_request_with_options(
        method,
        path,
        body,
        kid,
        key,
        created,
        &SignatureOptions {
            idempotency_key: Some(idempotency_key.to_owned()),
            ..SignatureOptions::default()
        },
    )
}

#[derive(Clone, Debug)]
pub enum SignatureParameter {
    Created,
    KeyId,
    Algorithm(String),
}

#[derive(Clone, Debug)]
pub struct SignatureOptions {
    pub label: String,
    pub parameters: Vec<SignatureParameter>,
    pub origin_form: bool,
    pub idempotency_key: Option<String>,
    /// Origin the signer addressed; `@target-uri` is this origin plus `path`.
    pub origin: String,
}

impl Default for SignatureOptions {
    fn default() -> Self {
        Self {
            label: "sig1".to_owned(),
            parameters: vec![
                SignatureParameter::Created,
                SignatureParameter::KeyId,
                SignatureParameter::Algorithm("ed25519".to_owned()),
            ],
            origin_form: false,
            idempotency_key: None,
            origin: TEST_ORIGIN.to_owned(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn signed_request_with_options(
    method: Method,
    path: &str,
    body: Vec<u8>,
    kid: &str,
    key: &SigningKey,
    created: i64,
    options: &SignatureOptions,
) -> Request<Body> {
    let target_uri = format!("{}{path}", options.origin);
    let digest = STANDARD.encode(Sha256::digest(&body));
    let content_digest = format!("sha-256=:{digest}:");
    let signature_parameters = signature_parameters(
        kid,
        created,
        &options.parameters,
        options.idempotency_key.is_some(),
    );
    let mut base = format!(
        "\"@method\": {}\n\"@target-uri\": {target_uri}\n\"content-digest\": {content_digest}",
        method.as_str()
    );
    if let Some(idempotency_key) = &options.idempotency_key {
        base.push_str("\n\"idempotency-key\": ");
        base.push_str(idempotency_key);
    }
    base.push_str("\n\"@signature-params\": ");
    base.push_str(&signature_parameters);
    let signature = key.sign(base.as_bytes()).to_bytes();
    let label = KeyRef::from_str(&options.label).expect("signature label must be an SFV key");
    let signature_input = format!("{label}={signature_parameters}");
    let mut signature_serializer = DictSerializer::new();
    let _ = signature_serializer
        .bare_item(label, signature.as_slice())
        .finish();
    let signature_header = signature_serializer
        .finish()
        .expect("signature dictionary must not be empty");
    let request_target = if options.origin_form {
        path
    } else {
        &target_uri
    };
    let mut request = Request::builder()
        .method(method)
        .uri(request_target)
        .header("host", "api.test")
        .header("content-type", "application/json")
        .header("content-digest", content_digest)
        .header("signature-input", signature_input)
        .header("signature", signature_header);
    if let Some(idempotency_key) = &options.idempotency_key {
        request = request.header("idempotency-key", idempotency_key);
    }
    request
        .body(Body::from(body))
        .expect("test request must be valid")
}

fn signature_parameters(
    kid: &str,
    created: i64,
    order: &[SignatureParameter],
    include_idempotency_key: bool,
) -> String {
    let mut serializer = ListSerializer::new();
    {
        let mut inner = serializer.inner_list();
        for component in ["@method", "@target-uri", "content-digest"] {
            let _ = inner
                .bare_item(
                    StringRef::from_str(component)
                        .expect("signature component must be an SFV string"),
                )
                .finish();
        }
        if include_idempotency_key {
            let _ = inner
                .bare_item(
                    StringRef::from_str("idempotency-key")
                        .expect("signature component must be an SFV string"),
                )
                .finish();
        }
        let mut parameters = inner.finish();
        for parameter in order {
            parameters = match parameter {
                SignatureParameter::Created => parameters.parameter(
                    KeyRef::from_str("created").expect("created must be an SFV key"),
                    Integer::try_from(created).expect("created must fit an SFV integer"),
                ),
                SignatureParameter::KeyId => parameters.parameter(
                    KeyRef::from_str("keyid").expect("keyid must be an SFV key"),
                    StringRef::from_str(kid).expect("kid must be an SFV string"),
                ),
                SignatureParameter::Algorithm(algorithm) => parameters.parameter(
                    KeyRef::from_str("alg").expect("alg must be an SFV key"),
                    StringRef::from_str(algorithm).expect("algorithm must be an SFV string"),
                ),
            };
        }
        let _ = parameters.finish();
    }
    serializer
        .finish()
        .expect("signature parameter inner list must not be empty")
}

fn required_url(name: &str) -> Result<Option<String>> {
    match env::var(name).ok().filter(|value| !value.is_empty()) {
        Some(value) => Ok(Some(value)),
        None => skip(&format!("{name} is not set")).map(|()| None),
    }
}

/// Prints a skip message, or fails when `CI=true` (set by GitHub Actions), where every
/// integration test must run.
pub fn skip(reason: &str) -> Result<()> {
    if env::var("CI").is_ok_and(|value| value == "true") {
        anyhow::bail!("CI=true but {reason}; integration tests must not skip in CI");
    }
    eprintln!("skipping integration test: {reason}");
    Ok(())
}

/// Refund destination screening that clears every address.
pub struct ClearScreener;

#[async_trait::async_trait]
impl topup::refunds::DestinationScreener for ClearScreener {
    async fn screen(
        &self,
        _route: &topup_core::route::RouteFile,
        _destination: alloy_primitives::Address,
    ) -> topup::refunds::DestinationScreening {
        topup::refunds::DestinationScreening::Clear
    }
}

/// A rate limiter's clock that stands still until the test advances it, so whether a request is
/// within a limit depends on the test's steps and not on how fast the machine answers them.
#[derive(Clone, Debug)]
pub struct ManualClock(Arc<Mutex<Instant>>);

impl ManualClock {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }

    pub fn now(&self) -> Instant {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn advance(&self, by: Duration) {
        let mut now = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        *now += by;
    }

    /// A limiter enforcing `limits` on this clock.
    pub fn rate_limiter(&self, limits: topup::api::RateLimits) -> topup::api::ApiRateLimiter {
        let clock = self.clone();
        topup::api::ApiRateLimiter::with_clock(limits, move || clock.now())
    }

    /// A limiter of reads by `client_secret`, with an ephemeral key, on this clock.
    pub fn client_read_limiter(&self) -> topup::api::ClientReadLimiter {
        let clock = self.clone();
        topup::api::ClientReadLimiter::with_clock(move || clock.now())
    }
}
