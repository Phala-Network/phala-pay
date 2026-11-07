//! dstack attestation primitives.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dstack_sdk::dstack_client::DstackClient;
use sha2::{Digest, Sha256};
use tokio::time::timeout;
use topup_core::{Ed25519PublicKey, WebhookKeyId};
use zeroize::Zeroize as _;

use crate::signer::dstack::{DerivedKey, KeyAlgorithm};
use crate::signer::ed25519_public_key;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// dstack identity metadata returned alongside an attestation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttestationInfo {
    /// Application identifier.
    pub app_id: Vec<u8>,
    /// Hash of the deployed compose document.
    pub compose_hash: Vec<u8>,
    /// Verbatim compose document, when the SDK exposes it.
    pub app_compose: Option<String>,
}

/// One version of an account's webhook key, as bound into an attestation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttestedWebhookKey {
    /// Key version, from 1.
    pub version: u32,
    /// Raw ed25519 public key.
    pub public_key: Ed25519PublicKey,
}

/// An attestation binding a nonce to an account's webhook public keys in one mode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttestationBundle {
    /// The account's keys, in the order bound into the report data.
    pub webhook_keys: Vec<AttestedWebhookKey>,
    /// SHA-256 commitment passed as dstack report data.
    pub report_data: [u8; 32],
    /// Versioned dstack attestation bytes. On TDX this contains the quote and event log.
    pub quote: Vec<u8>,
    /// Application identity and compose metadata.
    pub info: AttestationInfo,
}

/// A failure while collecting dstack attestation evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AttestationError {
    /// The request named no key, an invalid account, or a nonce longer than 255 bytes.
    #[error("attestation request is invalid")]
    InvalidRequest,
    /// A webhook key returned by dstack was malformed.
    #[error("dstack returned an invalid webhook key")]
    InvalidWebhookKey,
    /// The dstack attestation or information call failed or timed out.
    #[error("dstack attestation is unavailable")]
    DstackUnavailable,
}

/// Computes the report data binding a nonce to an account's webhook keys in one mode (design
/// D11):
///
/// ```text
/// sha256(len(nonce) ‖ nonce ‖ len(account) ‖ account ‖ livemode ‖ (version ‖ public_key)*)
/// ```
///
/// Lengths are one byte, `account` is the UTF-8 `acct_` id, `livemode` is one byte (`1` live, `0`
/// test), and each key is its version as a 4-byte big-endian integer followed by its 32-byte raw
/// ed25519 public key, in the order given. `None` when the nonce or account exceeds 255 bytes.
#[must_use]
pub fn report_data(
    nonce: &[u8],
    account: &str,
    livemode: bool,
    keys: &[AttestedWebhookKey],
) -> Option<[u8; 32]> {
    let mut hasher = Sha256::new();
    hasher.update([u8::try_from(nonce.len()).ok()?]);
    hasher.update(nonce);
    hasher.update([u8::try_from(account.len()).ok()?]);
    hasher.update(account.as_bytes());
    hasher.update([u8::from(livemode)]);
    for key in keys {
        hasher.update(key.version.to_be_bytes());
        hasher.update(key.public_key.0);
    }
    Some(hasher.finalize().into())
}

/// Collects webhook-key attestations from the dstack guest agent.
#[derive(Clone, Debug)]
pub struct DstackAttestor {
    endpoint: Option<String>,
    timeout: Duration,
    info_cache: Arc<tokio::sync::Mutex<Option<CachedInfo>>>,
}

#[derive(Clone, Debug)]
struct CachedInfo {
    collected: Instant,
    value: Result<serde_json::Value, AttestationError>,
}

impl Default for DstackAttestor {
    fn default() -> Self {
        Self::new()
    }
}

impl DstackAttestor {
    /// Uses the dstack socket selected by the SDK and a ten-second call timeout.
    #[must_use]
    pub fn new() -> Self {
        Self {
            endpoint: None,
            timeout: DEFAULT_TIMEOUT,
            info_cache: Arc::default(),
        }
    }

    /// Uses the dstack socket selected by the SDK and the provided call timeout.
    #[must_use]
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            endpoint: None,
            timeout,
            info_cache: Arc::default(),
        }
    }

    /// Uses an explicit dstack or simulator endpoint and a ten-second call timeout.
    #[must_use]
    pub fn with_endpoint(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: Some(endpoint.into()),
            timeout: DEFAULT_TIMEOUT,
            info_cache: Arc::default(),
        }
    }

    /// Uses an explicit endpoint and timeout for every dstack SDK call.
    #[must_use]
    pub fn with_endpoint_and_timeout(endpoint: impl Into<String>, timeout: Duration) -> Self {
        Self {
            endpoint: Some(endpoint.into()),
            timeout,
            info_cache: Arc::default(),
        }
    }

    /// Returns only the guest's public v0 Info metadata, using the same socket as attestation.
    /// The SDK represents `tcb_info` as an object. This is untrusted transport: callers must
    /// verify `vm_config` against the quote with the official dstack verifier.
    /// Successful calls are cached for 60 seconds; failures for five seconds. The mutex also
    /// coalesces concurrent callers, bounding guest quote work during anonymous traffic.
    pub async fn guest_info(&self) -> Result<serde_json::Value, AttestationError> {
        let mut cache = self.info_cache.lock().await;
        if let Some(cached) = cache.as_ref() {
            let ttl = if cached.value.is_ok() { 60 } else { 5 };
            if cached.collected.elapsed() < Duration::from_secs(ttl) {
                return cached.value.clone();
            }
        }
        let client = DstackClient::new(self.endpoint.as_deref());
        let value = async {
            let info = timeout(self.timeout, client.info())
                .await
                .map_err(|_| AttestationError::DstackUnavailable)?
                .map_err(|_| AttestationError::DstackUnavailable)?;
            serde_json::to_value(info).map_err(|_| AttestationError::DstackUnavailable)
        }
        .await;
        *cache = Some(CachedInfo {
            collected: Instant::now(),
            value: value.clone(),
        });
        value
    }

    /// Derives the webhook keys of `account` in the given mode at `versions`, binds them and the
    /// account to `nonce` ([`report_data`]), and returns dstack evidence.
    pub async fn attest(
        &self,
        nonce: &[u8],
        account: &str,
        livemode: bool,
        versions: &[u32],
    ) -> Result<AttestationBundle, AttestationError> {
        if versions.is_empty() {
            return Err(AttestationError::InvalidRequest);
        }
        let client = DstackClient::new(self.endpoint.as_deref());
        let webhook_keys = self
            .derive_webhook_keys(&client, account, livemode, versions)
            .await?;

        let report_data = report_data(nonce, account, livemode, &webhook_keys)
            .ok_or(AttestationError::InvalidRequest)?;
        let (quote, info) = self.attest_report_data(report_data).await?;

        Ok(AttestationBundle {
            webhook_keys,
            report_data,
            quote,
            info,
        })
    }

    /// Collects versioned evidence for a domain-separated SHA-256 commitment, plus app identity.
    pub async fn attest_report_data(
        &self,
        report_data: [u8; 32],
    ) -> Result<(Vec<u8>, AttestationInfo), AttestationError> {
        let client = DstackClient::new(self.endpoint.as_deref());
        let response = timeout(self.timeout, client.attest(report_data.to_vec()))
            .await
            .map_err(|_| AttestationError::DstackUnavailable)?
            .map_err(|_| AttestationError::DstackUnavailable)?;
        let info = timeout(self.timeout, client.info())
            .await
            .map_err(|_| AttestationError::DstackUnavailable)?
            .map_err(|_| AttestationError::DstackUnavailable)?;
        let quote = response
            .decode_attestation()
            .map_err(|_| AttestationError::DstackUnavailable)?;
        let app_id = hex::decode(&info.app_id).map_err(|_| AttestationError::DstackUnavailable)?;
        let compose_hash =
            hex::decode(&info.compose_hash).map_err(|_| AttestationError::DstackUnavailable)?;
        let app_compose = info.tcb_info.app_compose;

        Ok((
            quote,
            AttestationInfo {
                app_id,
                compose_hash,
                app_compose: (!app_compose.is_empty()).then_some(app_compose),
            },
        ))
    }

    /// Derives the public webhook keys of `account` in the given mode at `versions`, in order,
    /// without attesting them: the keys that verify the account's deliveries.
    pub async fn webhook_keys(
        &self,
        account: &str,
        livemode: bool,
        versions: &[u32],
    ) -> Result<Vec<AttestedWebhookKey>, AttestationError> {
        let client = DstackClient::new(self.endpoint.as_deref());
        self.derive_webhook_keys(&client, account, livemode, versions)
            .await
    }

    async fn derive_webhook_keys(
        &self,
        client: &DstackClient,
        account: &str,
        livemode: bool,
        versions: &[u32],
    ) -> Result<Vec<AttestedWebhookKey>, AttestationError> {
        let mut webhook_keys = Vec::with_capacity(versions.len());
        for &version in versions {
            let id = WebhookKeyId::new(account, livemode, version)
                .ok_or(AttestationError::InvalidRequest)?;
            let mut key_response = timeout(self.timeout, client.get_key(Some(id.domain()), None))
                .await
                .map_err(|_| AttestationError::DstackUnavailable)?
                .map_err(|_| AttestationError::DstackUnavailable)?;
            let key = key_response.decode_key();
            key_response.key.zeroize();
            let key = key
                .map_err(|_| AttestationError::InvalidWebhookKey)
                .and_then(|key| {
                    DerivedKey::from_bytes(key, KeyAlgorithm::Ed25519)
                        .map_err(|_| AttestationError::InvalidWebhookKey)
                })?;
            webhook_keys.push(AttestedWebhookKey {
                version,
                public_key: ed25519_public_key(&key.secret),
            });
        }
        Ok(webhook_keys)
    }
}

#[cfg(test)]
mod tests {
    use topup_core::Ed25519PublicKey;

    use super::{AttestedWebhookKey, report_data};

    #[test]
    fn report_data_matches_the_published_vector() {
        let nonce = hex::decode("000102030405060708090a0b0c0d0e0f").expect("valid vector");
        let keys = [
            AttestedWebhookKey {
                version: 2,
                public_key: Ed25519PublicKey([0x42; 32]),
            },
            AttestedWebhookKey {
                version: 1,
                public_key: Ed25519PublicKey([0x24; 32]),
            },
        ];
        let account = "acct_0123456789abcdef0123456789abcdef";
        assert_eq!(
            report_data(&nonce, account, true, &keys).map(hex::encode),
            Some("86da5cb5cfe64def5578b7e416e6354930cd47cf85231f8227c8eb6e1748eda0".to_owned())
        );
        assert_eq!(
            report_data(&nonce, account, false, &keys[..1]).map(hex::encode),
            Some("919730430f98ac5d7dc845ded45abd8946e7ee4e38035cefb16567324fcbe622".to_owned())
        );
    }

    #[test]
    fn report_data_binds_the_account_mode_and_every_key() {
        let key = |version, byte| AttestedWebhookKey {
            version,
            public_key: Ed25519PublicKey([byte; 32]),
        };
        let base = report_data(b"n", "acct_a", true, &[key(1, 1)]);
        for other in [
            report_data(b"n", "acct_b", true, &[key(1, 1)]),
            report_data(b"n", "acct_a", false, &[key(1, 1)]),
            report_data(b"n", "acct_a", true, &[key(2, 1)]),
            report_data(b"n", "acct_a", true, &[key(1, 2)]),
            report_data(b"n", "acct_a", true, &[key(1, 1), key(2, 2)]),
            report_data(b"m", "acct_a", true, &[key(1, 1)]),
        ] {
            assert_ne!(base, other);
        }
        assert_eq!(report_data(&[0; 256], "acct_a", true, &[key(1, 1)]), None);
    }
}
