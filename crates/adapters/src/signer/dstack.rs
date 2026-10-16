//! dstack-backed signing adapter.
//!
//! Keys come from the `GetKey` method of the dstack v0 guest API (supported by 0.6.1) on `/var/run/dstack.sock`. That
//! agent derives the key from the domain (its `path`) and the app key alone: the algorithm only
//! selects the public key it signs, so one domain has one secret. The pinned SDK always requests
//! `secp256k1`; a webhook key's secret (`settlement/{account}/{live|test}/v{n}`, design D11) is used
//! as an ed25519 seed. Every domain here has one
//! fixed algorithm, which [`DerivedKey`] checks locally because the response carries no public key.
//!
//! The pinned SDK deserializes RPC JSON into ordinary response buffers before returning them. Those
//! internal JSON and response allocations are outside this adapter's zeroization control. The hex
//! key string this module receives is zeroized after decoding, and the decoded `Vec` is moved into
//! [`Zeroizing`] and copied directly into [`SecretKey32`]; all owned buffers are zeroized on drop.

use std::time::Duration;

use dstack_sdk::dstack_client::DstackClient;
use tokio::time::timeout;
use topup_core::{
    BACKUP_KEY_DOMAIN, Ed25519PublicKey, Ed25519Signature, SecretKey32, Signer, SignerError,
    WebhookKeyId,
};
use zeroize::{Zeroize as _, Zeroizing};

use super::{ed25519_public_key, sign_ed25519, validate_secp256k1};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// A signer which derives a fresh key for every operation through the dstack guest agent.
#[derive(Clone, Debug)]
pub struct DstackSigner {
    endpoint: Option<String>,
    timeout: Duration,
}

impl Default for DstackSigner {
    fn default() -> Self {
        Self::new()
    }
}

impl DstackSigner {
    /// Uses the dstack socket selected by the SDK and a ten-second call timeout.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            endpoint: None,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Uses the dstack socket selected by the SDK and the provided call timeout.
    #[must_use]
    pub const fn with_timeout(timeout: Duration) -> Self {
        Self {
            endpoint: None,
            timeout,
        }
    }

    /// Uses an explicit dstack or simulator endpoint and a ten-second call timeout.
    #[must_use]
    pub fn with_endpoint(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: Some(endpoint.into()),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Uses an explicit endpoint and timeout for every dstack SDK call.
    #[must_use]
    pub fn with_endpoint_and_timeout(endpoint: impl Into<String>, timeout: Duration) -> Self {
        Self {
            endpoint: Some(endpoint.into()),
            timeout,
        }
    }

    /// Derives the WAL-G backup-encryption key from [`BACKUP_KEY_DOMAIN`].
    pub async fn derive_backup_key(&self) -> Result<SecretKey32, SignerError> {
        self.derive_secret(BACKUP_KEY_DOMAIN).await
    }

    /// Derives the secp256k1 key for a secret domain such as `backup/v1` or `db/owner/v1`.
    ///
    /// Every secret is a secp256k1 key, so a secret domain never needs a second algorithm.
    pub async fn derive_secret(&self, domain: &str) -> Result<SecretKey32, SignerError> {
        Ok(self
            .derive_key(domain, KeyAlgorithm::Secp256k1)
            .await?
            .secret)
    }

    async fn derive_key(
        &self,
        domain: &str,
        algorithm: KeyAlgorithm,
    ) -> Result<DerivedKey, SignerError> {
        let client = DstackClient::new(self.endpoint.as_deref());
        let mut response = timeout(self.timeout, client.get_key(Some(domain.to_owned()), None))
            .await
            .map_err(|_| SignerError::KeyUnavailable)?
            .map_err(|_| SignerError::KeyUnavailable)?;
        let key = response.decode_key().map_err(|_| SignerError::InvalidKey);
        response.key.zeroize();
        DerivedKey::from_bytes(key?, algorithm)
    }
}

impl Signer for DstackSigner {
    async fn sign_webhook(
        &self,
        key: &WebhookKeyId,
        payload: &[u8],
    ) -> Result<Ed25519Signature, SignerError> {
        let key = self
            .derive_key(&key.domain(), KeyAlgorithm::Ed25519)
            .await?;
        Ok(sign_ed25519(&key.secret, payload))
    }

    async fn webhook_public_key(
        &self,
        key: &WebhookKeyId,
    ) -> Result<Ed25519PublicKey, SignerError> {
        let key = self
            .derive_key(&key.domain(), KeyAlgorithm::Ed25519)
            .await?;
        Ok(ed25519_public_key(&key.secret))
    }
}

#[derive(Clone, Copy)]
pub(crate) enum KeyAlgorithm {
    Secp256k1,
    Ed25519,
}

pub(crate) struct DerivedKey {
    pub(crate) secret: SecretKey32,
}

impl DerivedKey {
    /// Accepts 32 key bytes that are valid for `algorithm`: any 32 bytes for ed25519, a non-zero
    /// scalar below the curve order for secp256k1.
    pub(crate) fn from_bytes(key: Vec<u8>, algorithm: KeyAlgorithm) -> Result<Self, SignerError> {
        let key = Zeroizing::new(key);
        let secret = SecretKey32::from_slice(key.as_slice()).ok_or(SignerError::InvalidKey)?;
        if let KeyAlgorithm::Secp256k1 = algorithm {
            validate_secp256k1(&secret)?;
        }
        Ok(Self { secret })
    }
}

#[cfg(test)]
mod tests {
    use super::{DerivedKey, KeyAlgorithm};
    use topup_core::SignerError;

    #[test]
    fn rejects_wrong_private_key_length() {
        for algorithm in [KeyAlgorithm::Secp256k1, KeyAlgorithm::Ed25519] {
            assert_eq!(
                DerivedKey::from_bytes(vec![1; 31], algorithm).err(),
                Some(SignerError::InvalidKey)
            );
        }
    }

    #[test]
    fn rejects_invalid_secp256k1_scalars() {
        let curve_order =
            hex::decode("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141")
                .expect("curve order vector is valid hex");
        for key in [vec![0; 32], curve_order] {
            assert_eq!(
                DerivedKey::from_bytes(key, KeyAlgorithm::Secp256k1).err(),
                Some(SignerError::InvalidKey)
            );
        }
    }
}
