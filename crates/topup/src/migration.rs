//! One-time logical migration between app identities. Only ciphertext leaves the CVM.
//!
//! PostgreSQL tools run in the pinned postgres-walg image. The distroless topup jobs exchange
//! the archive over a private tmpfs, then verify the restored database before normal migrations.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, bail, ensure};
use ed25519_dalek::{Signature, VerifyingKey};
use futures_util::TryStreamExt as _;
use object_store::aws::AmazonS3Builder;
use object_store::buffered::BufWriter;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{AssertSqlSafe, PgPool};
use tokio::io::AsyncWriteExt as _;
use topup_adapters::attestation::DstackAttestor;
use topup_adapters::signer::dstack::DstackSigner;
use topup_core::SecretKey32;
use zeroize::Zeroizing;

const MANIFEST_DOMAIN: &[u8] = b"phala-pay/migration/manifest/v1\0";
/// Commitment domain for the import recipient's canonical age string (UTF-8).
pub const RECIPIENT_DOMAIN: &[u8] = b"phala-pay/migration/import/v1\0";
/// Commitment domain for the export public key, preceded by one-byte nonce length and nonce.
pub const EXPORT_KEY_DOMAIN: &[u8] = b"phala-pay/migration/export-key/v1\0";
const MAX_METADATA: u64 = 1_048_576;
const MAX_ARCHIVE: u64 = 32 * 1024 * 1024 * 1024;

/// Attested, immutable configuration mounted only by migration jobs.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Export settings; mutually exclusive with import.
    pub migration_export: Option<ExportConfig>,
    /// Import settings; mutually exclusive with export.
    pub migration_import: Option<ImportConfig>,
}

/// The operator pins the verified new-app recipient, never an object-store supplied key.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportConfig {
    /// Canonical age X25519 recipient verified against the new app's attestation.
    pub recipient: String,
    /// Unique S3 prefix, e.g. s3://bucket/migrations/staging-2026-11-06.
    pub object: String,
}

/// The destination pins both the old app identity and its attested export verification key.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportConfig {
    /// Source dstack app ID, lowercase hexadecimal.
    pub source_app_id: String,
    /// Ed25519 export key, 32 bytes as lowercase hexadecimal.
    pub export_public_key: String,
    /// The same unique S3 prefix as the source.
    pub object: String,
}

impl Config {
    /// Parses strict YAML and validates the pinned trust inputs before network or DB access.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let config: Self = serde_saphyr::from_str(&fs::read_to_string(path)?)
            .context("invalid migration configuration")?;
        match (&config.migration_export, &config.migration_import) {
            (Some(export), None) => {
                let recipient: age::x25519::Recipient = export
                    .recipient
                    .parse()
                    .map_err(|_| anyhow::anyhow!("invalid X25519 recipient"))?;
                ensure!(
                    recipient.to_string() == export.recipient,
                    "recipient must be canonical"
                );
                parse_object(&export.object)?;
            }
            (None, Some(import)) => {
                ensure!(
                    !import.source_app_id.is_empty() && import.source_app_id.len() <= 64,
                    "invalid source app id"
                );
                ensure!(
                    hex::encode(hex::decode(&import.source_app_id)?) == import.source_app_id,
                    "source app id must be lowercase hex"
                );
                verification_key(&import.export_public_key)?;
                parse_object(&import.object)?;
            }
            _ => bail!("configure exactly one of migration_export or migration_import"),
        }
        Ok(config)
    }
}

fn verification_key(value: &str) -> anyhow::Result<VerifyingKey> {
    let bytes: [u8; 32] = hex::decode(value)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid export public key length"))?;
    ensure!(
        hex::encode(bytes) == value,
        "export key must be lowercase hex"
    );
    Ok(VerifyingKey::from_bytes(&bytes)?)
}

fn parse_object(value: &str) -> anyhow::Result<(String, ObjectPath)> {
    let url = url::Url::parse(value)?;
    ensure!(
        url.scheme() == "s3"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "invalid migration object"
    );
    let bucket = url
        .host_str()
        .context("migration object needs a bucket")?
        .to_owned();
    let prefix = url.path().trim_matches('/');
    ensure!(
        !prefix.is_empty() && !value.contains('%') && !value.contains(".."),
        "migration object needs a unique literal prefix"
    );
    Ok((bucket, ObjectPath::parse(prefix)?))
}

/// Non-secret evidence published for verification with the official dstack verifier.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct KeyEvidence {
    /// Canonical age recipient, or hexadecimal Ed25519 verification key.
    pub public_key: String,
    /// SHA-256 commitment as hexadecimal, zero-padded by dstack to 64 quote bytes.
    pub report_data: String,
    /// Versioned dstack quote as hexadecimal.
    pub tdx_quote: String,
    /// Attested dstack application ID as hexadecimal.
    pub app_id: String,
    /// Attested compose hash as hexadecimal.
    pub compose_hash: String,
}

async fn evidence(
    attestor: &DstackAttestor,
    public_key: String,
    report_data: [u8; 32],
) -> anyhow::Result<KeyEvidence> {
    let (quote, info) = attestor.attest_report_data(report_data).await?;
    Ok(KeyEvidence {
        public_key,
        report_data: hex::encode(report_data),
        tdx_quote: hex::encode(quote),
        app_id: hex::encode(info.app_id),
        compose_hash: hex::encode(info.compose_hash),
    })
}

/// Export key evidence. Binding: SHA256(domain || len(nonce):u8 || nonce || raw_key).
pub async fn export_key(
    signer: &DstackSigner,
    attestor: &DstackAttestor,
    nonce: &[u8],
) -> anyhow::Result<KeyEvidence> {
    ensure!(
        !nonce.is_empty() && nonce.len() <= 32,
        "nonce must be 1 to 32 bytes"
    );
    let key = signer.migration_public_key().await?;
    let mut hasher = Sha256::new();
    hasher.update(EXPORT_KEY_DOMAIN);
    hasher.update([u8::try_from(nonce.len())?]);
    hasher.update(nonce);
    hasher.update(key.0);
    evidence(attestor, hex::encode(key.0), hasher.finalize().into()).await
}

/// Derives the stable age identity from v0 GetKey's 32-byte migration/import/v1 secret.
pub async fn import_identity(signer: &DstackSigner) -> anyhow::Result<age::x25519::Identity> {
    identity_from_secret(&signer.derive_secret("migration/import/v1").await?)
}

fn identity_from_secret(secret: &SecretKey32) -> anyhow::Result<age::x25519::Identity> {
    // Use age's supported Bech32 parsing interface; no private-format or cryptographic code.
    let hrp = bech32::Hrp::parse("AGE-SECRET-KEY-")?;
    let encoded = Zeroizing::new(
        bech32::encode::<bech32::Bech32>(hrp, secret.expose_secret())?.to_uppercase(),
    );
    encoded
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid derived migration identity"))
}

/// Creates evidence binding the canonical age public recipient to the destination app.
pub async fn recipient_evidence(
    identity: &age::x25519::Identity,
    attestor: &DstackAttestor,
) -> anyhow::Result<KeyEvidence> {
    let public_key = identity.to_public().to_string();
    let mut hasher = Sha256::new();
    hasher.update(RECIPIENT_DOMAIN);
    hasher.update(public_key.as_bytes());
    evidence(attestor, public_key, hasher.finalize().into()).await
}

/// Database invariants collected while all source writers are fenced off.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DatabaseState {
    /// Exact counts for every non-system table, keyed by quoted schema and table.
    pub row_counts: BTreeMap<String, i64>,
    /// Latest successfully applied SQLx migration.
    pub latest_migration: i64,
    /// Last persisted heartbeat timestamp, or null if no heartbeat was ever recorded.
    pub last_heartbeat: Option<chrono::DateTime<chrono::Utc>>,
}

/// Signed, versioned description of the ciphertext, its pinned recipient, source, and DB state.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Fixed protocol version.
    pub version: u32,
    /// Lowercase SHA-256 of the entire ciphertext.
    pub ciphertext_sha256: String,
    /// Canonical pinned age X25519 recipient.
    pub recipient: String,
    /// Attested source app ID.
    pub source_app_id: String,
    /// Unique migration object prefix, preventing cross-prefix replay.
    pub object: String,
    /// Expected restored database contents.
    pub database: DatabaseState,
}

/// Signature envelope. The verification key always comes from the attested configuration.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignedManifest {
    /// Canonical typed manifest, serialized as compact JSON for signing.
    pub manifest: Manifest,
    /// Ed25519 signature over domain || compact manifest JSON, lowercase hex.
    pub signature: String,
}

fn signing_bytes(manifest: &Manifest) -> anyhow::Result<Vec<u8>> {
    let mut bytes = MANIFEST_DOMAIN.to_vec();
    bytes.extend(serde_json::to_vec(manifest)?);
    Ok(bytes)
}

/// Verifies the pinned signer, app, destination recipient and object before decryption.
pub fn verify_manifest(
    signed: &SignedManifest,
    config: &ImportConfig,
    recipient: &str,
) -> anyhow::Result<()> {
    let key = verification_key(&config.export_public_key)?;
    let signature = Signature::from_slice(&hex::decode(&signed.signature)?)?;
    key.verify_strict(&signing_bytes(&signed.manifest)?, &signature)
        .context("migration manifest signature is invalid")?;
    let manifest = &signed.manifest;
    ensure!(
        manifest.version == 1
            && manifest.source_app_id == config.source_app_id
            && manifest.recipient == recipient
            && manifest.object == config.object,
        "migration manifest does not match the pinned source, recipient, object or version"
    );
    ensure!(
        manifest.ciphertext_sha256.len() == 64
            && hex::decode(&manifest.ciphertext_sha256)?.len() == 32,
        "invalid ciphertext hash"
    );
    Ok(())
}

/// Collects exact counts, latest migration, and heartbeat without changing schema or data.
pub async fn database_state(pool: &PgPool) -> anyhow::Result<DatabaseState> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    // format('%I') quotes identifiers in PostgreSQL, including embedded quotes.
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT format('%I.%I', schemaname, tablename) FROM pg_tables \
         WHERE schemaname NOT IN ('pg_catalog', 'information_schema', 'topup_migration') \
         AND schemaname NOT LIKE 'pg_toast%' ORDER BY 1",
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut row_counts = BTreeMap::new();
    for table in tables {
        let count = sqlx::query_scalar(AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
            .fetch_one(&mut *tx)
            .await?;
        row_counts.insert(table, count);
    }
    let latest_migration =
        sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations WHERE success")
            .fetch_one(&mut *tx)
            .await?;
    let last_heartbeat = sqlx::query_scalar("SELECT max(recorded_at) FROM heartbeat")
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(DatabaseState {
        row_counts,
        latest_migration,
        last_heartbeat,
    })
}

/// Refuses a nonempty destination, including any completed import marker.
pub async fn require_fresh(pool: &PgPool) -> anyhow::Result<()> {
    let empty: bool = sqlx::query_scalar(
        "SELECT NOT EXISTS (SELECT 1 FROM pg_tables WHERE schemaname NOT IN \
         ('pg_catalog', 'information_schema') AND schemaname NOT LIKE 'pg_toast%')",
    )
    .fetch_one(pool)
    .await?;
    ensure!(
        empty,
        "migration import requires a fresh database; a second import is refused"
    );
    Ok(())
}

/// Checks all signed DB invariants and atomically writes a marker outside the application schema.
pub async fn verify_database(pool: &PgPool, signed: &SignedManifest) -> anyhow::Result<()> {
    let marked: bool =
        sqlx::query_scalar("SELECT to_regclass('topup_migration.imported') IS NOT NULL")
            .fetch_one(pool)
            .await?;
    ensure!(!marked, "a second import is refused");
    ensure!(
        database_state(pool).await? == signed.manifest.database,
        "restored database does not match migration manifest"
    );
    let mut tx = pool.begin().await?;
    // No IF NOT EXISTS: concurrent verifiers cannot both authorize startup.
    sqlx::query("CREATE SCHEMA topup_migration AUTHORIZATION postgres")
        .execute(&mut *tx)
        .await?;
    sqlx::query("REVOKE ALL ON SCHEMA topup_migration FROM PUBLIC")
        .execute(&mut *tx)
        .await?;
    sqlx::query("CREATE TABLE topup_migration.imported (manifest jsonb NOT NULL, imported_at timestamptz NOT NULL DEFAULT now())")
        .execute(&mut *tx).await?;
    sqlx::query("INSERT INTO topup_migration.imported (manifest) VALUES ($1)")
        .bind(serde_json::to_value(signed)?)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

fn private_file(path: &Path) -> anyhow::Result<File> {
    Ok(OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?)
}

fn copy_plaintext(reader: &mut impl Read, writer: &mut impl Write) -> anyhow::Result<()> {
    let mut buffer = Zeroizing::new(vec![0_u8; 65_536]);
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        writer.write_all(buffer.get(..read).context("invalid read length")?)?;
    }
}

fn encrypt(input: &Path, output: &Path, recipient: &str) -> anyhow::Result<()> {
    let recipient: age::x25519::Recipient = recipient
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid pinned recipient"))?;
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))?;
    let mut encrypted = encryptor.wrap_output(private_file(output)?)?;
    copy_plaintext(&mut File::open(input)?, &mut encrypted)?;
    encrypted.finish()?.sync_all()?;
    Ok(())
}

fn decrypt(input: &Path, output: &Path, identity: &age::x25519::Identity) -> anyhow::Result<()> {
    let result = (|| {
        let decryptor = age::Decryptor::new(File::open(input)?)?;
        let mut plaintext = decryptor.decrypt(std::iter::once(identity as &dyn age::Identity))?;
        let mut file = private_file(output)?;
        copy_plaintext(&mut plaintext, &mut file)?;
        file.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(output);
    }
    result
}

fn hash_file(path: &Path) -> anyhow::Result<String> {
    let mut hash = Sha256::new();
    let mut input = File::open(path)?;
    let mut buffer = vec![0_u8; 65_536];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(buffer.get(..read).context("invalid read length")?);
    }
    Ok(hex::encode(hash.finalize()))
}

/// S3 transfer bounded to a unique prefix. Only credentials come from sealed AWS environment.
pub struct Transfer {
    store: Arc<dyn ObjectStore>,
    prefix: ObjectPath,
}

impl Transfer {
    /// Uses the standard S3 client and the existing sealed AWS_* credentials, over verified TLS.
    pub fn s3(object: &str) -> anyhow::Result<Self> {
        let (bucket, prefix) = parse_object(object)?;
        // Explicit credentials prevent fallback to instance metadata or profile files.
        let access = std::env::var("AWS_ACCESS_KEY_ID").context("AWS_ACCESS_KEY_ID is required")?;
        let secret =
            std::env::var("AWS_SECRET_ACCESS_KEY").context("AWS_SECRET_ACCESS_KEY is required")?;
        ensure!(
            !access.is_empty() && !secret.is_empty(),
            "S3 credentials are required"
        );
        let mut builder = AmazonS3Builder::new()
            .with_bucket_name(bucket)
            .with_access_key_id(access)
            .with_secret_access_key(secret)
            .with_region(std::env::var("AWS_REGION").unwrap_or_else(|_| "auto".to_owned()));
        if let Ok(endpoint) = std::env::var("AWS_ENDPOINT") {
            let url = url::Url::parse(&endpoint)?;
            ensure!(
                url.scheme() == "https" && url.username().is_empty() && url.password().is_none(),
                "migration S3 endpoint must use HTTPS"
            );
            builder = builder.with_endpoint(endpoint);
        }
        Ok(Self {
            store: Arc::new(builder.build()?),
            prefix,
        })
    }

    /// Publishes public recipient evidence. The exporter never trusts it without an operator pin.
    pub async fn publish_recipient(&self, evidence: &KeyEvidence) -> anyhow::Result<()> {
        self.store
            .put(
                &self.prefix.clone().join("recipient.json"),
                serde_json::to_vec_pretty(evidence)?.into(),
            )
            .await?;
        Ok(())
    }

    async fn upload(&self, path: &Path, name: &str) -> anyhow::Result<()> {
        let mut writer = BufWriter::new(self.store.clone(), self.prefix.clone().join(name))
            .with_max_concurrency(2);
        let mut file = tokio::fs::File::open(path).await?;
        let result = async {
            tokio::io::copy(&mut file, &mut writer).await?;
            writer.shutdown().await
        }
        .await;
        if result.is_err() {
            let _ = writer.abort().await;
        }
        result.context("migration upload failed")
    }

    async fn download(&self, name: &str, path: &Path, limit: u64) -> anyhow::Result<()> {
        let result = self.store.get(&self.prefix.clone().join(name)).await?;
        ensure!(
            result.meta.size <= limit,
            "migration object exceeds size limit"
        );
        let mut stream = result.into_stream();
        let mut file = tokio::fs::File::from_std(private_file(path)?);
        let mut written = 0_u64;
        while let Some(chunk) = stream.try_next().await? {
            written = written
                .checked_add(u64::try_from(chunk.len())?)
                .context("migration object is too large")?;
            ensure!(written <= limit, "migration object exceeds size limit");
            file.write_all(&chunk).await?;
        }
        file.sync_all().await?;
        Ok(())
    }

    /// Encrypts a fenced source's custom-format dump, signs metadata, uploads manifest last.
    /// Plaintext is removed on both success and failure; it is never uploaded.
    pub async fn export(
        &self,
        config: &ExportConfig,
        pool: &PgPool,
        signer: &DstackSigner,
        attestor: &DstackAttestor,
        directory: &Path,
    ) -> anyhow::Result<()> {
        let dump = directory.join("dump.pg");
        let encrypted = directory.join("dump.age");
        let result = async {
            let mut header = [0_u8; 5];
            File::open(&dump)?.read_exact(&mut header)?;
            ensure!(&header == b"PGDMP", "export requires pg_dump custom format");
            let database = database_state(pool).await?;
            let public = signer.migration_public_key().await?;
            let source = evidence(
                attestor,
                hex::encode(public.0),
                Sha256::digest(MANIFEST_DOMAIN).into(),
            )
            .await?;
            encrypt(&dump, &encrypted, &config.recipient)?;
            let manifest = Manifest {
                version: 1,
                ciphertext_sha256: hash_file(&encrypted)?,
                recipient: config.recipient.clone(),
                source_app_id: source.app_id,
                object: config.object.clone(),
                database,
            };
            let signature = hex::encode(signer.sign_migration(&signing_bytes(&manifest)?).await?.0);
            self.upload(&encrypted, "dump.age").await?;
            let signed = SignedManifest {
                manifest,
                signature,
            };
            self.store
                .put(
                    &self.prefix.clone().join("manifest.json"),
                    serde_json::to_vec(&signed)?.into(),
                )
                .await?;
            Ok(())
        }
        .await;
        let removed = fs::remove_file(&dump);
        let _ = fs::remove_file(&encrypted);
        result?;
        removed.context("failed to remove plaintext export")
    }

    /// Publishes recipient and waits for a signed manifest. Prepares plaintext only after all
    /// cryptographic checks; restore and verification jobs must succeed before migrate starts.
    pub async fn import(
        &self,
        config: &ImportConfig,
        pool: &PgPool,
        identity: &age::x25519::Identity,
        attestor: &DstackAttestor,
        directory: &Path,
        wait: Duration,
    ) -> anyhow::Result<()> {
        require_fresh(pool).await?;
        self.publish_recipient(&recipient_evidence(identity, attestor).await?)
            .await?;
        let manifest_path = directory.join("manifest.json");
        let encrypted = directory.join("dump.age");
        let partial = directory.join("dump.partial");
        let dump = directory.join("dump.pg");
        ensure!(
            !dump.exists() && !partial.exists() && !manifest_path.exists() && !encrypted.exists(),
            "migration working directory is not empty"
        );
        let result = async {
            tokio::time::timeout(wait, async {
                loop {
                    match self
                        .download("manifest.json", &manifest_path, MAX_METADATA)
                        .await
                    {
                        Ok(()) => return Ok(()),
                        Err(error)
                            if error.downcast_ref::<object_store::Error>().is_some_and(
                                |error| matches!(error, object_store::Error::NotFound { .. }),
                            ) =>
                        {
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                        Err(error) => return Err(error),
                    }
                }
            })
            .await
            .context("timed out waiting for migration export")??;
            let signed: SignedManifest = serde_json::from_reader(File::open(&manifest_path)?)?;
            verify_manifest(&signed, config, &identity.to_public().to_string())?;
            self.download("dump.age", &encrypted, MAX_ARCHIVE).await?;
            ensure!(
                hash_file(&encrypted)? == signed.manifest.ciphertext_sha256,
                "migration ciphertext hash mismatch"
            );
            decrypt(&encrypted, &partial, identity)?;
            fs::rename(&partial, &dump)?;
            Ok(())
        }
        .await;
        let _ = fs::remove_file(&encrypted);
        if result.is_err() {
            let _ = fs::remove_file(&partial);
            let _ = fs::remove_file(&dump);
            let _ = fs::remove_file(&manifest_path);
        }
        result
    }
}

/// Final startup gate after pg_restore; re-verifies staged metadata against attested pins.
pub async fn verify_import(
    config: &ImportConfig,
    pool: &PgPool,
    identity: &age::x25519::Identity,
    directory: &Path,
) -> anyhow::Result<()> {
    // Remove plaintext even when verification fails. The DB is left stopped for investigation.
    let dump = directory.join("dump.pg");
    if dump.exists() {
        fs::remove_file(&dump).context("failed to remove plaintext import")?;
    }
    let signed: SignedManifest =
        serde_json::from_reader(File::open(directory.join("manifest.json"))?)?;
    verify_manifest(&signed, config, &identity.to_public().to_string())?;
    verify_database(pool, &signed).await?;
    fs::remove_file(directory.join("manifest.json"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer as _, SigningKey};

    fn fixture() -> (SignedManifest, ImportConfig, age::x25519::Identity) {
        let key = SigningKey::from_bytes(&[41; 32]);
        let identity = identity_from_secret(&SecretKey32::new([42; 32])).unwrap();
        let config = ImportConfig {
            source_app_id: "ab".repeat(20),
            export_public_key: hex::encode(key.verifying_key().as_bytes()),
            object: "s3://migration-bucket/test-unique-cutover".into(),
        };
        let manifest = Manifest {
            version: 1,
            ciphertext_sha256: "cd".repeat(32),
            source_app_id: config.source_app_id.clone(),
            object: config.object.clone(),
            recipient: identity.to_public().to_string(),
            database: DatabaseState {
                row_counts: BTreeMap::from([("public.accounts".into(), 3)]),
                latest_migration: 20261101000000,
                last_heartbeat: None,
            },
        };
        let signature = hex::encode(key.sign(&signing_bytes(&manifest).unwrap()).to_bytes());
        (
            SignedManifest {
                manifest,
                signature,
            },
            config,
            identity,
        )
    }

    #[test]
    fn forged_manifest_wrong_signature_and_wrong_recipient_are_refused() {
        let (signed, mut config, identity) = fixture();
        let recipient = identity.to_public().to_string();
        verify_manifest(&signed, &config, &recipient).unwrap();
        let mut forged = signed.clone();
        forged
            .manifest
            .database
            .row_counts
            .insert("public.accounts".into(), 99);
        assert!(verify_manifest(&forged, &config, &recipient).is_err());
        let wrong = identity_from_secret(&SecretKey32::new([43; 32])).unwrap();
        assert!(verify_manifest(&signed, &config, &wrong.to_public().to_string()).is_err());
        config.source_app_id = "cd".repeat(20);
        assert!(verify_manifest(&signed, &config, &recipient).is_err());
        config.source_app_id = signed.manifest.source_app_id.clone();
        config.object.push_str("-replay");
        assert!(verify_manifest(&signed, &config, &recipient).is_err());
        config.object = signed.manifest.object.clone();
        config.export_public_key =
            hex::encode(SigningKey::from_bytes(&[44; 32]).verifying_key().as_bytes());
        assert!(verify_manifest(&signed, &config, &recipient).is_err());
    }

    #[test]
    fn streamed_age_roundtrip_rejects_wrong_recipient_and_tampered_ciphertext() {
        let dir = std::env::temp_dir().join(format!("topup-migration-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let result = (|| -> anyhow::Result<()> {
            let (_, _, identity) = fixture();
            let input = dir.join("input");
            let encrypted = dir.join("encrypted");
            let decrypted = dir.join("decrypted");
            let wrong = identity_from_secret(&SecretKey32::new([43; 32]))?;
            let original = vec![71_u8; 131_072]; // Crosses multiple age payload chunks.
            fs::write(&input, &original)?;
            encrypt(&input, &encrypted, &identity.to_public().to_string())?;
            assert!(decrypt(&encrypted, &decrypted, &wrong).is_err());
            assert!(!decrypted.exists());
            decrypt(&encrypted, &decrypted, &identity)?;
            assert_eq!(fs::read(&decrypted)?, original);
            fs::remove_file(&decrypted)?;
            let original_hash = hash_file(&encrypted)?;
            let mut tampered = fs::read(&encrypted)?;
            *tampered.last_mut().context("ciphertext")? ^= 1;
            fs::write(&encrypted, tampered)?;
            assert_ne!(hash_file(&encrypted)?, original_hash);
            assert!(decrypt(&encrypted, &decrypted, &identity).is_err());
            assert!(
                !decrypted.exists(),
                "partial unauthenticated plaintext must be removed"
            );
            Ok(())
        })();
        fs::remove_dir_all(&dir).unwrap();
        result.unwrap();
    }

    #[test]
    fn configuration_rejects_missing_pins_and_unsafe_object_locations() {
        for object in [
            "http://bucket/path",
            "s3://bucket",
            "s3://user@bucket/path",
            "s3://bucket/path?key=secret",
            "s3://bucket/a%2fb",
            "s3://bucket/path#fragment",
        ] {
            assert!(parse_object(object).is_err(), "{object}");
        }
        assert!(parse_object("s3://bucket/migrations/unique-cutover").is_ok());
        let directory =
            std::env::temp_dir().join(format!("topup-migration-config-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("config.json");
        for value in [
            serde_json::json!({}),
            serde_json::json!({"migration_import": {"source_app_id": "ab".repeat(20), "object": "s3://bucket/cutover"}}),
            serde_json::json!({"migration_export": {"recipient": "unverified", "object": "s3://bucket/cutover"}}),
            serde_json::json!({"migration_import": null, "migration_export": null, "unknown": true}),
        ] {
            fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            assert!(Config::load(&path).is_err());
        }
        fs::remove_dir_all(directory).unwrap();
    }

    /// Opt-in real two-cluster roundtrip, with the pinned dstack simulator and standard PG tools.
    /// deploy/tests/migration-roundtrip.sh supplies isolated containers; no staging or production.
    #[tokio::test]
    async fn simulator_two_postgres_roundtrip_and_row_count_mismatch() -> anyhow::Result<()> {
        let Ok(endpoint) = std::env::var("MIGRATION_DSTACK_ENDPOINT") else {
            eprintln!(
                "skipped: run deploy/tests/migration-roundtrip.sh for the full migration rehearsal"
            );
            return Ok(());
        };
        let source_url = std::env::var("MIGRATION_SOURCE_URL")?;
        let destination_url = std::env::var("MIGRATION_DESTINATION_URL")?;
        let source_container = std::env::var("MIGRATION_SOURCE_CONTAINER")?;
        let destination_container = std::env::var("MIGRATION_DESTINATION_CONTAINER")?;
        let source = crate::db::connect(&source_url, "migration-test-source", 2).await?;
        let destination =
            crate::db::connect(&destination_url, "migration-test-destination", 2).await?;
        crate::db::migrate(&source).await?;
        crate::heartbeat::record(&source).await?;
        for pool in [&source, &destination] {
            sqlx::raw_sql("DO $$ BEGIN IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname='topup_app') THEN CREATE ROLE topup_app NOLOGIN; END IF; END $$; CREATE ROLE topup_service LOGIN IN ROLE topup_app;")
                .execute(pool).await?;
        }
        let directory =
            std::env::temp_dir().join(format!("migration-roundtrip-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory)?;
        let export_dir = directory.join("export");
        let import_dir = directory.join("import");
        fs::create_dir(&export_dir)?;
        fs::create_dir(&import_dir)?;
        let result = async {
            let signer = DstackSigner::with_endpoint(endpoint.clone());
            let attestor = DstackAttestor::with_endpoint(endpoint);
            let identity = import_identity(&signer).await?;
            let public = export_key(&signer, &attestor, b"migration-roundtrip").await?;
            assert_ne!(
                public.report_data,
                export_key(&signer, &attestor, b"another-nonce")
                    .await?
                    .report_data
            );
            let dump = std::process::Command::new("docker")
                .args([
                    "exec",
                    &source_container,
                    "pg_dump",
                    "-U",
                    "postgres",
                    "-d",
                    "topup",
                    "-Fc",
                    "--exclude-schema=topup_migration",
                ])
                .output()?;
            ensure!(dump.status.success(), "test pg_dump failed");
            fs::write(export_dir.join("dump.pg"), dump.stdout)?;
            let export_config = ExportConfig {
                recipient: identity.to_public().to_string(),
                object: "s3://test/unique-migration".into(),
            };
            let import_config = ImportConfig {
                source_app_id: public.app_id,
                export_public_key: public.public_key,
                object: export_config.object.clone(),
            };
            let transfer = Transfer {
                store: Arc::new(object_store::memory::InMemory::new()),
                prefix: ObjectPath::parse("unique-migration")?,
            };
            transfer
                .export(&export_config, &source, &signer, &attestor, &export_dir)
                .await?;
            assert!(!export_dir.join("dump.pg").exists());
            let manifest_object = transfer.prefix.clone().join("manifest.json");
            let dump_object = transfer.prefix.clone().join("dump.age");
            let original_manifest = transfer.store.get(&manifest_object).await?.bytes().await?;
            let original_dump = transfer.store.get(&dump_object).await?.bytes().await?;
            let mut forged: SignedManifest = serde_json::from_slice(&original_manifest)?;
            forged.manifest.database.latest_migration = 1;
            transfer
                .store
                .put(&manifest_object, serde_json::to_vec(&forged)?.into())
                .await?;
            assert!(
                transfer
                    .import(
                        &import_config,
                        &destination,
                        &identity,
                        &attestor,
                        &import_dir,
                        Duration::from_secs(30)
                    )
                    .await
                    .is_err()
            );
            assert!(!import_dir.join("dump.pg").exists());
            transfer
                .store
                .put(&manifest_object, original_manifest.clone().into())
                .await?;
            let wrong_identity = identity_from_secret(&topup_core::SecretKey32::new([73; 32]))?;
            assert!(
                transfer
                    .import(
                        &import_config,
                        &destination,
                        &wrong_identity,
                        &attestor,
                        &import_dir,
                        Duration::from_secs(30)
                    )
                    .await
                    .is_err()
            );
            assert!(!import_dir.join("dump.pg").exists());
            transfer
                .store
                .put(&dump_object, vec![0_u8; 32].into())
                .await?;
            assert!(
                transfer
                    .import(
                        &import_config,
                        &destination,
                        &identity,
                        &attestor,
                        &import_dir,
                        Duration::from_secs(30)
                    )
                    .await
                    .is_err()
            );
            assert!(!import_dir.join("dump.pg").exists());
            transfer
                .store
                .put(&dump_object, original_dump.into())
                .await?;
            transfer
                .import(
                    &import_config,
                    &destination,
                    &identity,
                    &attestor,
                    &import_dir,
                    Duration::from_secs(30),
                )
                .await?;
            let plaintext = File::open(import_dir.join("dump.pg"))?;
            let restore = std::process::Command::new("docker")
                .args([
                    "exec",
                    "-i",
                    &destination_container,
                    "pg_restore",
                    "-U",
                    "postgres",
                    "-d",
                    "topup",
                    "--single-transaction",
                    "--exit-on-error",
                ])
                .stdin(plaintext)
                .output()?;
            ensure!(
                restore.status.success(),
                "test pg_restore failed: {}",
                String::from_utf8_lossy(&restore.stderr)
            );
            let signed: SignedManifest =
                serde_json::from_reader(File::open(import_dir.join("manifest.json"))?)?;
            let mut mismatch = signed.clone();
            mismatch
                .manifest
                .database
                .row_counts
                .insert("public.heartbeat".into(), 99);
            assert!(verify_database(&destination, &mismatch).await.is_err());
            assert!(
                sqlx::query_scalar::<_, bool>(
                    "SELECT to_regclass('topup_migration.imported') IS NULL"
                )
                .fetch_one(&destination)
                .await?
            );
            assert_eq!(
                database_state(&source).await?,
                database_state(&destination).await?
            );
            // ACLs travel with the archive; the destination login retains its NEW cluster identity.
            let granted: bool = sqlx::query_scalar(
                "SELECT has_table_privilege('topup_service', 'public.heartbeat', 'SELECT')",
            )
            .fetch_one(&destination)
            .await?;
            ensure!(
                granted,
                "restored ACLs must grant the application role access"
            );
            verify_import(&import_config, &destination, &identity, &import_dir).await?;
            assert!(!import_dir.join("dump.pg").exists());
            assert!(require_fresh(&destination).await.is_err());
            assert!(verify_database(&destination, &signed).await.is_err());
            Ok(())
        }
        .await;
        source.close().await;
        destination.close().await;
        fs::remove_dir_all(directory)?;
        result
    }
}
