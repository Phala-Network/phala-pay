//! Direct row writes and reads that set up and inspect test databases. Production writes these
//! rows through the API repository and the scanner.

use alloy_primitives::{Address as EvmAddress, B256, address};
use sqlx::PgPool;
use topup::api_keys::{self, KeyKind};
use topup::db::{self, Account, Address, Customer};
use uuid::Uuid;

/// The treasury tests set for their accounts ([`set_treasury`]), which seeded addresses pay.
pub const FIXTURE_TREASURY: EvmAddress = address!("0x0000000000000000000000000000000000007EA5");

/// Values used to create a merchant account enabled for live mode and, when `webhook_url` is not
/// empty, one webhook endpoint in `livemode`. [`create_api_key`] gives it a key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewAccount {
    pub id: Uuid,
    pub name: String,
    pub livemode: bool,
    pub webhook_url: String,
    pub paused_scopes: Vec<String>,
}

impl NewAccount {
    /// A live account without API keys or a webhook endpoint.
    pub fn named(name: &str) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.to_owned(),
            livemode: true,
            webhook_url: String::new(),
            paused_scopes: Vec::new(),
        }
    }
}

/// Values used to create an account's customer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewCustomer {
    pub id: Uuid,
    pub account_id: Uuid,
    pub livemode: bool,
    pub client_reference_id: String,
    pub paused_scopes: Vec<String>,
}

/// Values used to insert a customer's forwarder address. The address gets a canceled quote on
/// `route`, so payments to it are valued at spot, as for any address without an open quote.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewAddress {
    pub id: Uuid,
    pub customer_id: Uuid,
    pub chain_id: u64,
    pub route: String,
    pub salt: B256,
    pub address: EvmAddress,
}

/// Writes `document` as the account's payment settings in `livemode`, as `POST
/// /v1/payment_settings` does; a new account accepts nothing until it is configured.
pub async fn configure_payments(
    pool: &PgPool,
    account_id: Uuid,
    livemode: bool,
    document: &topup::payment_config::Document,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    topup::payment_config::write(
        &mut transaction,
        topup::tenancy::Scope::new(account_id, livemode),
        document,
        "test",
    )
    .await?;
    transaction.commit().await
}

/// Configures the account to accept the asset of each of `routes` in `livemode`, on the
/// operator's defaults.
pub async fn accept_routes(
    pool: &PgPool,
    account_id: Uuid,
    livemode: bool,
    routes: &[&topup_core::route::RouteFile],
) -> Result<(), sqlx::Error> {
    configure_payments(
        pool,
        account_id,
        livemode,
        &topup::payment_config::Document::accepting(routes.iter().copied()),
    )
    .await
}

/// Configures the account to accept `assets` on `chain_id` in `livemode`, on the operator's
/// defaults.
pub async fn accept_assets(
    pool: &PgPool,
    account_id: Uuid,
    livemode: bool,
    chain_id: u64,
    assets: &[&str],
) -> Result<(), sqlx::Error> {
    configure_payments(
        pool,
        account_id,
        livemode,
        &topup::payment_config::Document {
            quote_creations_per_customer_per_minute: None,
            chains: vec![topup::payment_config::ChainChoice {
                chain_id,
                confirmations: None,
                assets: assets
                    .iter()
                    .map(|asset| topup::payment_config::AssetChoice::on_defaults(*asset))
                    .collect(),
            }],
        },
    )
    .await
}

/// Configures the account to accept every current route of `livemode` in `routes`, on the
/// operator's defaults.
pub async fn accept_all(
    pool: &PgPool,
    account_id: Uuid,
    livemode: bool,
    routes: &topup::routes::RouteSet,
) -> Result<(), sqlx::Error> {
    configure_payments(
        pool,
        account_id,
        livemode,
        &topup::payment_config::Document::accepting_all(routes, livemode),
    )
    .await
}

/// Inserts a secret key of the account in the given mode and returns the key.
pub async fn create_api_key(
    pool: &PgPool,
    account_id: Uuid,
    livemode: bool,
) -> Result<String, sqlx::Error> {
    let key = api_keys::generate(KeyKind::Secret, livemode)
        .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
    sqlx::query(
        r#"
        INSERT INTO api_keys (id, account_id, livemode, kind, prefix, last4, key_hash, created_by)
        VALUES ($1, $2, $3, 'secret', $4, $5, $6, 'admin')
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(account_id)
    .bind(livemode)
    .bind(api_keys::prefix(KeyKind::Secret, livemode))
    .bind(&key[key.len() - 4..])
    .bind(api_keys::hash(&key).as_slice())
    .execute(pool)
    .await?;
    Ok(key.as_str().to_owned())
}

/// Creates the account; see [`NewAccount`].
pub async fn create_account(pool: &PgPool, account: &NewAccount) -> Result<Account, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO accounts (id, name, paused_scopes, charges_enabled) \
         VALUES ($1, $2, $3, true)",
    )
    .bind(account.id)
    .bind(&account.name)
    .bind(&account.paused_scopes)
    .execute(&mut *transaction)
    .await?;
    if !account.webhook_url.is_empty() {
        sqlx::query(
            "INSERT INTO webhook_endpoints (id, account_id, livemode, url) VALUES ($1, $2, $3, $4)",
        )
        .bind(Uuid::new_v4())
        .bind(account.id)
        .bind(account.livemode)
        .bind(&account.webhook_url)
        .execute(&mut *transaction)
        .await?;
    }
    transaction.commit().await?;
    db::get_account(pool, account.id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)
}

pub async fn create_customer(
    pool: &PgPool,
    customer: &NewCustomer,
) -> Result<Customer, sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO customers (id, account_id, livemode, client_reference_id, paused_scopes)
        VALUES ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(customer.id)
    .bind(customer.account_id)
    .bind(customer.livemode)
    .bind(&customer.client_reference_id)
    .bind(&customer.paused_scopes)
    .execute(pool)
    .await?;
    db::get_customer(pool, customer.id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)
}

/// Creates an account and one customer of it in the account's mode.
pub async fn create_account_and_customer(
    pool: &PgPool,
    account: &NewAccount,
    client_reference_id: &str,
) -> Result<(Account, Customer), sqlx::Error> {
    let account_row = create_account(pool, account).await?;
    let customer = create_customer(
        pool,
        &NewCustomer {
            id: Uuid::new_v4(),
            account_id: account.id,
            livemode: account.livemode,
            client_reference_id: client_reference_id.to_owned(),
            paused_scopes: Vec::new(),
        },
    )
    .await?;
    Ok((account_row, customer))
}

/// The terms a seeded quote is stored with: the fixture route's defaults. Seeded quotes are
/// canceled, so their terms are only shown.
pub fn fixture_terms() -> topup::payment_config::Terms {
    let route: topup_core::route::RouteFile =
        serde_saphyr::from_str(include_str!("../fixtures/phala-cloud-pha.yaml"))
            .expect("route fixture");
    topup::payment_config::Terms::defaults(&route)
}

pub async fn insert_address(pool: &PgPool, address: &NewAddress) -> Result<Address, sqlx::Error> {
    let chain_id = i64::try_from(address.chain_id).map_err(|error| encode_error(&error))?;
    let quote_id = Uuid::new_v4();
    let mut transaction = pool.begin().await?;
    sqlx::query(
        r#"
        INSERT INTO quotes (
            id, account_id, livemode, customer_id, route, amount_atomic, price_scaled,
            credit_minor, expires_at, status, closed_at, route_version, settings_revision_id,
            terms
        )
        SELECT $1, customer.account_id, customer.livemode, customer.id, $3, 1, 100000000, 1,
               now(), 'cancelled', now(), 1, settings.current_revision_id, $4
        FROM customers AS customer
        JOIN payment_settings_state AS settings
            ON settings.account_id = customer.account_id AND settings.livemode = customer.livemode
        WHERE customer.id = $2
        "#,
    )
    .bind(quote_id)
    .bind(address.customer_id)
    .bind(&address.route)
    .bind(sqlx::types::Json(fixture_terms()))
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO addresses
            (id, account_id, livemode, chain_id, quote_id, salt, treasury, address)
        SELECT $1, account_id, livemode, $2, id, $3, $4, $5
        FROM quotes
        WHERE id = $6
        "#,
    )
    .bind(address.id)
    .bind(chain_id)
    .bind(format!("{:#x}", address.salt))
    .bind(format!("{FIXTURE_TREASURY:#x}"))
    .bind(format!("{:#x}", address.address))
    .bind(quote_id)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    db::get_address(pool, address.id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)
}

pub async fn set_customer_paused_scopes(
    pool: &PgPool,
    id: Uuid,
    paused_scopes: &[String],
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE customers SET paused_scopes = $2 WHERE id = $1")
        .bind(id)
        .bind(paused_scopes)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_account_paused_scopes(
    pool: &PgPool,
    id: Uuid,
    paused_scopes: &[String],
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE accounts SET paused_scopes = $2 WHERE id = $1")
        .bind(id)
        .bind(paused_scopes)
        .execute(pool)
        .await?;
    Ok(())
}

fn encode_error(error: &std::num::TryFromIntError) -> sqlx::Error {
    sqlx::Error::Encode(format!("value is outside PostgreSQL bigint: {error}").into())
}

/// Makes `treasury` the account's current treasury of `chain_id` in `livemode`, as a proven and
/// applied treasury would be, replacing the former one. Production sets it with a signed proof
/// through `POST /v1/treasuries`.
pub async fn set_treasury(
    pool: &PgPool,
    account_id: Uuid,
    livemode: bool,
    chain_id: u64,
    treasury: EvmAddress,
) -> Result<(), sqlx::Error> {
    let chain_id = i64::try_from(chain_id).map_err(|error| encode_error(&error))?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "UPDATE treasuries SET replaced_at = now() \
         WHERE account_id = $1 AND livemode = $2 AND chain_id = $3 \
           AND applied_at IS NOT NULL AND replaced_at IS NULL",
    )
    .bind(account_id)
    .bind(livemode)
    .bind(chain_id)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO treasuries (
            id, account_id, livemode, chain_id, address, kind, proof_message, proof_signature,
            verified_at, effective_at, screened_at, applied_at, created_by
        )
        VALUES ($1, $2, $3, $4, $5, 'eoa', 'seeded', '0x', now(), now(), now(), now(),
                'key_00000000000000000000000000000000')
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(account_id)
    .bind(livemode)
    .bind(chain_id)
    .bind(format!("{treasury:#x}"))
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

/// Records a proven change of the account's treasury of `chain_id` to `treasury`, pending until
/// `effective_at`, as `POST /v1/treasuries` records a later live change; `topup::treasuries::
/// apply_due` applies it.
pub async fn schedule_treasury(
    pool: &PgPool,
    account_id: Uuid,
    livemode: bool,
    chain_id: u64,
    treasury: EvmAddress,
    effective_at: chrono::DateTime<chrono::Utc>,
) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO treasuries (
            id, account_id, livemode, chain_id, address, kind, proof_message, proof_signature,
            verified_at, effective_at, screened_at, created_by
        )
        VALUES ($1, $2, $3, $4, $5, 'eoa', 'seeded', '0x', now(), $6, now(),
                'key_00000000000000000000000000000000')
        "#,
    )
    .bind(id)
    .bind(account_id)
    .bind(livemode)
    .bind(i64::try_from(chain_id).map_err(|error| encode_error(&error))?)
    .bind(format!("{treasury:#x}"))
    .bind(effective_at)
    .execute(pool)
    .await?;
    Ok(id)
}

/// Initializes a fixture chain as startup would, without marking any address backfilled.
pub async fn initialize_dual_chain(pool: &PgPool, chain: u64) -> Result<(), sqlx::Error> {
    topup::db::chain_reads::initialize_coverage(
        pool,
        chain,
        topup::db::chain_reads::Boundary {
            number: 0,
            hash: B256::ZERO,
            time: chrono::DateTime::UNIX_EPOCH,
        },
    )
    .await?;
    topup::db::chain_reads::advance_checkpoint(
        pool,
        chain,
        topup::db::chain_reads::Boundary {
            number: 0,
            hash: B256::ZERO,
            time: chrono::DateTime::UNIX_EPOCH,
        },
    )
    .await
}
