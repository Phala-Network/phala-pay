//! The fixed-address quote migration requires a pre-upgrade restore to run v0.12.x again.
mod support;

use std::process::{Command, Stdio};

use anyhow::{Context, Result, ensure};
use support::TestDatabase;
use uuid::Uuid;

const POSTGRES: &str =
    "postgres:18.6-trixie@sha256:86c951e05bf56c93d95d397747fb8820ac76cc3bedb78f43abd83eedbe3666ae";

fn migrate(image: &str, database: &TestDatabase) -> Result<std::process::Output> {
    Command::new("docker")
        .args(["run", "--rm", "--network", "host", "-e"])
        .arg(format!("DATABASE_URL={}", database.owner_url))
        .args([image, "topup", "migrate"])
        .output()
        .context("published image migration")
}

#[tokio::test]
#[ignore = "requires verified published N-1 image; restore-only rollback drill"]
async fn published_image_requires_pre_upgrade_restore() -> Result<()> {
    let image = std::env::var("TOPUP_ROLLBACK_IMAGE").context("published N-1 image required")?;
    ensure!(image.contains("@sha256:"));
    let database = TestDatabase::create_with_migrations(false)
        .await?
        .context("local Postgres required")?;
    let restored = TestDatabase::create_with_migrations(false)
        .await?
        .context("local Postgres required")?;
    let result = async {
        ensure!(migrate(&image, &database)?.status.success(), "N-1 migration failed");
        // Seed an N-1-shaped quote/address before the backup, without using current writers.
        let account = Uuid::new_v4();
        let customer = Uuid::new_v4();
        let quote = Uuid::new_v4();
        let address = Uuid::new_v4();
        let mut tx = database.owner_pool.begin().await?;
        sqlx::query("INSERT INTO accounts(id,name) VALUES($1,'restore-only drill')")
            .bind(account).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO customers(id,account_id,livemode,client_reference_id) VALUES($1,$2,true,'fixed')")
            .bind(customer).bind(account).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO quotes(id,account_id,livemode,customer_id,route,amount_atomic,price_scaled,credit_minor,expires_at,route_version,settings_revision_id,terms)
            SELECT $1,$2,true,$3,'historical',100,100000000,100,now(),1,current_revision_id,$4
            FROM payment_settings_state WHERE account_id=$2 AND livemode")
            .bind(quote).bind(account).bind(customer).bind(sqlx::types::Json(support::seed::fixture_terms()))
            .execute(&mut *tx).await?;
        sqlx::query("INSERT INTO addresses(id,account_id,livemode,chain_id,quote_id,salt,treasury,address)
            VALUES($1,$2,true,1,$3,$4,$5,$6)")
            .bind(address).bind(account).bind(quote).bind(format!("0x{}","11".repeat(32)))
            .bind(format!("0x{}","22".repeat(20))).bind(format!("0x{}","33".repeat(20)))
            .execute(&mut *tx).await?;
        let reversed = Uuid::new_v4();
        sqlx::query("INSERT INTO deposits(id,account_id,livemode,customer_id,chain_id,tx_hash,
            receipt_log_index,log_index,block_number,block_hash,block_time,address_id,route,route_version,
            asset_contract,from_address,amount_atomic,state,next_attempt_at,settings_revision_id,
            tx_from,tx_nonce,valuation_at,price_scaled,credit_minor,price_source)
            SELECT $1,$2,true,$3,1,$5,0,0,1,$5,now(),$4,'historical',1,$6,$6,100,'reversed',now(),
            current_revision_id,$6,0,now(),100000000,100,'lock'
            FROM payment_settings_state WHERE account_id=$2 AND livemode")
            .bind(reversed).bind(account).bind(customer).bind(address)
            .bind(format!("0x{}","44".repeat(32))).bind(format!("0x{}","55".repeat(20)))
            .execute(&mut *tx).await?;
        tx.commit().await?;
        let backup = Command::new("docker")
            .args(["run","--rm","--network","host",POSTGRES,"pg_dump","--format=custom","--no-owner","--dbname"])
            .arg(&database.owner_url).output().context("pre-upgrade backup")?;
        ensure!(backup.status.success(), "pre-upgrade backup failed");
        topup::db::migrate(&database.owner_pool).await?;
        let backfilled: Uuid = sqlx::query_scalar("SELECT address_id FROM quotes WHERE id=$1")
            .bind(quote).fetch_one(&database.app_pool).await?;
        ensure!(backfilled==address, "historical address was not preserved");
        let historical: Option<Uuid> = sqlx::query_scalar("SELECT quote_id FROM deposits WHERE id=$1")
            .bind(reversed).fetch_one(&database.owner_pool).await?;
        ensure!(historical==Some(quote), "reversed locked credit lost its quote association");
        let rejected = migrate(&image, &database)?;
        ensure!(!rejected.status.success(), "N-1 accepted the breaking schema");
        let log = format!("{}{}",String::from_utf8_lossy(&rejected.stdout),String::from_utf8_lossy(&rejected.stderr));
        ensure!(log.contains("missing") || log.contains("migration"), "N-1 did not reject migration startup: {log}");
        // Stream the actual pre-upgrade backup into a separate clean database, then run N-1.
        let mut restore = Command::new("docker")
            .args(["run","--rm","-i","--network","host",POSTGRES,"pg_restore","--exit-on-error","--no-owner","--dbname"])
            .arg(&restored.owner_url).stdin(Stdio::piped()).stdout(Stdio::null())
            .stderr(Stdio::piped()).spawn().context("restore pre-upgrade backup")?;
        let input = restore.stdin.take().context("restore input")?;
        let writer = std::thread::spawn(move || {
            use std::io::Write;
            let mut input = input;
            input.write_all(&backup.stdout)
        });
        let output = restore.wait_with_output()?;
        writer.join().map_err(|_|anyhow::anyhow!("backup stream failed"))??;
        ensure!(output.status.success(), "backup restore failed: {}",String::from_utf8_lossy(&output.stderr));
        ensure!(migrate(&image, &restored)?.status.success(), "N-1 failed after pre-upgrade restore");
        let watched: Uuid = sqlx::query_scalar("SELECT id FROM addresses WHERE quote_id=$1")
            .bind(quote).fetch_one(&restored.owner_pool).await?;
        ensure!(watched==address, "restore changed the watched address");
        Ok(())
    }.await;
    let cleanup = database.cleanup().await.and(restored.cleanup().await);
    result.and(cleanup)
}
