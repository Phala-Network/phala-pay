use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signature, VerifyingKey};
use topup_core::{Ed25519PublicKey, Signer, SignerError, WebhookKeyId};

/// Standard Webhooks metadata and asymmetric `v1a` signature.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignedWebhook {
    /// Stable event identifier, the `evt_` id.
    pub id: String,
    /// Attempt timestamp as integer Unix seconds.
    pub timestamp: String,
    /// One `v1a,<base64>` entry per key, space-delimited: during a key rotation the receiver
    /// accepts the delivery if any entry verifies with a key it pinned.
    pub signature: String,
}

impl SignedWebhook {
    /// Signs `{id}.{timestamp}.{body}` exactly as sent on the wire with each of `keys`, which
    /// must not be empty.
    pub async fn new(
        signer: &impl Signer,
        keys: &[WebhookKeyId],
        event_id: &str,
        timestamp: i64,
        body: &[u8],
    ) -> Result<Self, SignerError> {
        let id = event_id.to_owned();
        let timestamp = timestamp.to_string();
        let content = signed_content(&id, &timestamp, body);
        if keys.is_empty() {
            return Err(SignerError::KeyUnavailable);
        }
        let mut entries = Vec::with_capacity(keys.len());
        for key in keys {
            let signature = signer.sign_webhook(key, &content).await.inspect_err(|_| {
                crate::observability::emit_alert(
                    "TopupOutboxInternalFailure",
                    "signing_failed",
                    "critical",
                    1,
                    0,
                );
            })?;
            entries.push(format!("v1a,{}", STANDARD.encode(signature.0)));
        }

        Ok(Self {
            id,
            timestamp,
            signature: entries.join(" "),
        })
    }

    /// Whether an entry of `signature`, a `webhook-signature` header, verifies
    /// `{id}.{timestamp}.{body}` with one of `keys`: the check a receiver makes (Standard
    /// Webhooks), without its timestamp tolerance, so a delivery kept in a merchant's records
    /// still verifies later.
    #[must_use]
    pub fn verifies(
        keys: &[Ed25519PublicKey],
        id: &str,
        timestamp: &str,
        body: &[u8],
        signature: &str,
    ) -> bool {
        let content = signed_content(id, timestamp, body);
        let keys: Vec<VerifyingKey> = keys
            .iter()
            .filter_map(|key| VerifyingKey::from_bytes(&key.0).ok())
            .collect();
        signature
            .split(' ')
            .filter_map(|entry| entry.strip_prefix("v1a,"))
            .filter_map(|encoded| STANDARD.decode(encoded).ok())
            .filter_map(|bytes| Signature::from_slice(&bytes).ok())
            .any(|signature| {
                keys.iter()
                    .any(|key| key.verify_strict(&content, &signature).is_ok())
            })
    }
}

/// `{id}.{timestamp}.{body}`, the signed content.
fn signed_content(id: &str, timestamp: &str, body: &[u8]) -> Vec<u8> {
    let mut content = Vec::with_capacity(id.len() + timestamp.len() + body.len() + 2);
    content.extend_from_slice(id.as_bytes());
    content.push(b'.');
    content.extend_from_slice(timestamp.as_bytes());
    content.push(b'.');
    content.extend_from_slice(body);
    content
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer as _, SigningKey};
    use topup_core::{Ed25519PublicKey, Ed25519Signature, Signer, SignerError, WebhookKeyId};

    use uuid::Uuid;

    use super::*;

    /// Signs with `[7; 32]` for version 1 and `[8; 32]` for any other version.
    struct FixedSigner;

    impl FixedSigner {
        fn key(key: &WebhookKeyId) -> SigningKey {
            SigningKey::from_bytes(&[if key.version() == 1 { 7 } else { 8 }; 32])
        }
    }

    impl Signer for FixedSigner {
        async fn sign_webhook(
            &self,
            key: &WebhookKeyId,
            content: &[u8],
        ) -> Result<Ed25519Signature, SignerError> {
            Ok(Ed25519Signature(Self::key(key).sign(content).to_bytes()))
        }

        async fn webhook_public_key(
            &self,
            key: &WebhookKeyId,
        ) -> Result<Ed25519PublicKey, SignerError> {
            Ok(Ed25519PublicKey(Self::key(key).verifying_key().to_bytes()))
        }
    }

    struct UnavailableSigner;

    impl Signer for UnavailableSigner {
        async fn sign_webhook(
            &self,
            _: &WebhookKeyId,
            _: &[u8],
        ) -> Result<Ed25519Signature, SignerError> {
            Err(SignerError::KeyUnavailable)
        }
        async fn webhook_public_key(
            &self,
            _: &WebhookKeyId,
        ) -> Result<Ed25519PublicKey, SignerError> {
            Err(SignerError::KeyUnavailable)
        }
    }

    #[test]
    fn signer_failure_reaches_sentry_without_key_or_payload() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let events = sentry::test::with_captured_events(|| {
            tracing::subscriber::with_default(
                crate::observability::log_subscriber(std::io::sink),
                || {
                    let result = runtime.block_on(SignedWebhook::new(
                        &UnavailableSigner,
                        &[key(1)],
                        "evt_private",
                        1,
                        b"private payload",
                    ));
                    assert_eq!(result, Err(SignerError::KeyUnavailable));
                },
            );
        });
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tags["alert"], "TopupOutboxInternalFailure");
        let rendered = format!("{events:?}");
        assert!(!rendered.contains("private"));
        assert!(!rendered.contains("acct_a"));
    }

    fn key(version: u32) -> WebhookKeyId {
        WebhookKeyId::new("acct_a", false, version).expect("valid key id")
    }

    #[tokio::test]
    async fn asymmetric_header_matches_fixed_standard_webhooks_vector() {
        let signer = FixedSigner;
        let event_id = crate::outbox::webhook_id(
            Uuid::parse_str("018d5f8e-8a7b-7d65-bc44-2c4f5f0a6d31")
                .expect("fixed UUID should parse"),
        );
        let body = br#"{"type":"deposit.confirmed","data":{"deposit_id":"dep_123"}}"#;
        let signed = SignedWebhook::new(&signer, &[key(1)], &event_id, 1_674_087_231, body)
            .await
            .expect("fixed signer should sign");

        assert_eq!(signed.id, event_id);
        assert_eq!(signed.timestamp, "1674087231");
        assert_eq!(
            signed.signature,
            "v1a,YuPb4kzXzDJqX8EcTFjrfDziMBFmzlPS3V/ISzdG/7R3KS7G1TVLRBF7DOJGnAtOjjvfeFm1G32KO67JiiY0BQ=="
        );
    }

    #[tokio::test]
    async fn every_key_signs_during_a_rotation_and_none_is_refused() {
        let signer = FixedSigner;
        let rotating = SignedWebhook::new(&signer, &[key(2), key(1)], "evt_1", 1, b"{}")
            .await
            .expect("fixed signer should sign");
        let mut single = Vec::new();
        for version in [2, 1] {
            single.push(
                SignedWebhook::new(&signer, &[key(version)], "evt_1", 1, b"{}")
                    .await
                    .expect("fixed signer should sign")
                    .signature,
            );
        }
        assert_eq!(rotating.signature, single.join(" "));
        assert_eq!(
            SignedWebhook::new(&signer, &[], "evt_1", 1, b"{}").await,
            Err(SignerError::KeyUnavailable)
        );
    }

    #[tokio::test]
    async fn a_delivery_verifies_with_any_key_that_signed_it_and_only_as_signed() {
        let signer = FixedSigner;
        let public =
            |version| Ed25519PublicKey(FixedSigner::key(&key(version)).verifying_key().to_bytes());
        let signed = SignedWebhook::new(&signer, &[key(2), key(1)], "evt_1", 7, b"{}")
            .await
            .expect("fixed signer should sign");
        let verifies = |keys: &[Ed25519PublicKey], id: &str, timestamp: &str, body: &[u8]| {
            SignedWebhook::verifies(keys, id, timestamp, body, &signed.signature)
        };
        assert!(verifies(&[public(1)], "evt_1", "7", b"{}"));
        assert!(verifies(&[public(2)], "evt_1", "7", b"{}"));
        assert!(!verifies(&[public(1)], "evt_2", "7", b"{}"));
        assert!(!verifies(&[public(1)], "evt_1", "8", b"{}"));
        assert!(!verifies(&[public(1)], "evt_1", "7", b"{ }"));
        assert!(!verifies(&[], "evt_1", "7", b"{}"));
        assert!(!SignedWebhook::verifies(
            &[public(1)],
            "evt_1",
            "7",
            b"{}",
            "v1,abc"
        ));
    }
}
