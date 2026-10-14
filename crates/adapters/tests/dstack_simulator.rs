//! Opt-in dstack 0.5.12 simulator coverage.

use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
use topup_adapters::attestation::{AttestedWebhookKey, DstackAttestor, report_data};
use topup_adapters::signer::dstack::DstackSigner;
use topup_core::{DB_APP_KEY_DOMAIN, DB_OWNER_KEY_DOMAIN, Signer as _, WebhookKeyId};

const ACCOUNT: &str = "acct_0123456789abcdef0123456789abcdef";

#[tokio::test]
async fn dstack_signing_and_attestation() -> Result<(), Box<dyn std::error::Error>> {
    let Ok(endpoint) = std::env::var("DSTACK_SIMULATOR_ENDPOINT") else {
        eprintln!("skipped: set DSTACK_SIMULATOR_ENDPOINT to run the dstack simulator test");
        return Ok(());
    };

    let signer = DstackSigner::with_endpoint(endpoint.clone());
    let payload = b"simulator webhook payload";
    let key = WebhookKeyId::new(ACCOUNT, false, 1).ok_or("valid key id")?;
    let public_key = signer.webhook_public_key(&key).await?;
    let signature = signer.sign_webhook(&key, payload).await?;
    let verifying_key = VerifyingKey::from_bytes(&public_key.0)?;
    verifying_key.verify(payload, &Signature::from_bytes(&signature.0))?;
    // Every account, mode, and version has its own key.
    for other in [
        WebhookKeyId::new(ACCOUNT, true, 1),
        WebhookKeyId::new(ACCOUNT, false, 2),
        WebhookKeyId::new("acct_fedcba9876543210fedcba9876543210", false, 1),
    ] {
        let other = other.ok_or("valid key id")?;
        assert_ne!(signer.webhook_public_key(&other).await?, public_key);
    }

    // Database passwords: stable per app id, one per login, and distinct from other domains.
    let owner = signer.derive_secret(DB_OWNER_KEY_DOMAIN).await?;
    let owner_again = DstackSigner::with_endpoint(endpoint.clone())
        .derive_secret(DB_OWNER_KEY_DOMAIN)
        .await?;
    let app = signer.derive_secret(DB_APP_KEY_DOMAIN).await?;
    let backup = signer.derive_backup_key().await?;
    assert_eq!(owner.expose_secret(), owner_again.expose_secret());
    assert_ne!(owner.expose_secret(), app.expose_secret());
    assert_ne!(app.expose_secret(), backup.expose_secret());

    let evidence = DstackAttestor::with_endpoint(endpoint)
        .attest(b"integration-test", ACCOUNT, false, &[1])
        .await?;
    let attested = [AttestedWebhookKey {
        version: 1,
        public_key,
    }];
    assert_eq!(evidence.webhook_keys, attested);
    // The attestation is msgpack whose byte fields are integer arrays; the TDX quote in it carries
    // report_data zero-padded to 64 bytes.
    let mut padded = evidence.report_data.to_vec();
    padded.resize(64, 0);
    let encoded = msgpack_uints(&padded);
    assert!(
        evidence
            .quote
            .windows(encoded.len())
            .any(|window| window == encoded)
    );
    assert_eq!(
        evidence.report_data,
        report_data(b"integration-test", ACCOUNT, false, &attested).ok_or("report data")?
    );
    assert!(!evidence.info.app_id.is_empty());
    assert_eq!(evidence.info.compose_hash.len(), 32);
    Ok(())
}

/// Encodes each byte as a msgpack unsigned integer: a positive fixint below 0x80, else `0xcc b`.
fn msgpack_uints(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .flat_map(|&byte| {
            if byte < 0x80 {
                vec![byte]
            } else {
                vec![0xcc, byte]
            }
        })
        .collect()
}
