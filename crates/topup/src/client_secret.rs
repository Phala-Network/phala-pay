//! Self-authenticating `client_secret`s of quotes and deposit addresses (architecture §12).
//!
//! A secret is `{id}_secret_{nonce}{tag}`: `id` is the object's public id (`qt_…` or `da_…`),
//! `nonce` 8 random bytes and the 8-byte owner tag, and `tag` the first 16 bytes of HMAC-SHA256,
//! under the key derived from dstack at [`topup_core::CLIENT_SECRET_KEY_DOMAIN`] like the
//! service's other keys, of everything before it; all are lowercase hex. The owner tag is the
//! first 8 bytes of HMAC-SHA256 of `owner:{account}:{id}:{random}` (the account's `acct_` id,
//! the object's id, and the random bytes as they appear in the secret, lowercase hex) under the
//! owner subkey, HMAC-SHA256 of [`OWNER_SUBKEY_LABEL`] under the key. So a secret also proves
//! which account the service issued the object to ([`ClientSecretKey::verify_owner`]), which a
//! deposit-address recovery checks after a restore.
//! The tag is checked in memory in constant time, so a forged or malformed secret is refused
//! without touching the database. The database still stores each issued secret's SHA-256: a
//! secret is valid only while its row is, which lets a deposit address retire its old secrets.

use hmac::{Hmac, KeyInit, Mac as _};
use rand::TryRng as _;
use rand::rngs::SysRng;
use sha2::Sha256;
use topup_core::SecretKey32;

const SEPARATOR: &str = "_secret_";
/// Derives the owner tag's subkey from the key, so owner tags and tags never share a key.
pub const OWNER_SUBKEY_LABEL: &str = "client-secret/owner/v1";
/// Labels the owner tag's message.
const OWNER_PREFIX: &str = "owner:";
const RANDOM_BYTES: usize = 8;
const OWNER_TAG_BYTES: usize = 8;
const NONCE_BYTES: usize = RANDOM_BYTES + OWNER_TAG_BYTES;
const TAG_BYTES: usize = 16;

/// The key that tags and checks client secrets, and its owner subkey.
pub struct ClientSecretKey {
    key: SecretKey32,
    owner: SecretKey32,
}

impl std::fmt::Debug for ClientSecretKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ClientSecretKey(..)")
    }
}

/// The OS RNG failed, so no secret was issued.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("OS RNG failed; no client secret issued")]
pub struct EntropyUnavailable;

impl ClientSecretKey {
    /// A key from its 32 bytes, as derived at [`topup_core::CLIENT_SECRET_KEY_DOMAIN`].
    #[must_use]
    pub fn new(key: SecretKey32) -> Self {
        let owner = SecretKey32::from_slice(
            &keyed(key.expose_secret(), OWNER_SUBKEY_LABEL)
                .finalize()
                .into_bytes(),
        )
        .expect("HMAC-SHA256 is 32 bytes");
        Self { key, owner }
    }

    /// A random key of this process alone, for tests: secrets it issues die with the process.
    ///
    /// # Panics
    ///
    /// When the OS RNG fails, as a UUID v4 would.
    #[must_use]
    pub fn ephemeral() -> Self {
        let mut key = [0_u8; 32];
        SysRng
            .try_fill_bytes(&mut key)
            .expect("the OS RNG provides a client-secret key");
        Self::new(SecretKey32::new(key))
    }

    /// Issues a new secret of the object whose public id is `id`, of the account whose public id
    /// is `account`.
    pub fn issue(&self, account: &str, id: &str) -> Result<String, EntropyUnavailable> {
        let mut random = [0_u8; RANDOM_BYTES];
        SysRng
            .try_fill_bytes(&mut random)
            .map_err(|_| EntropyUnavailable)?;
        let random = hex::encode(random);
        let owner = self.owner_mac(account, id, &random).finalize().into_bytes();
        let signed = format!(
            "{id}{SEPARATOR}{random}{}",
            hex::encode(&owner[..OWNER_TAG_BYTES])
        );
        let tag = self.mac(&signed).finalize().into_bytes();
        Ok(format!("{signed}{}", hex::encode(&tag[..TAG_BYTES])))
    }

    /// Whether `secret` is well formed for the object whose public id is `id` and carries this
    /// key's tag. It does not say whether the secret is still valid: the database does.
    #[must_use]
    pub fn verify(&self, id: &str, secret: &str) -> bool {
        self.nonce(id, secret).is_some()
    }

    /// Whether `secret` [verifies](Self::verify) for `id` and the service issued it to the
    /// account whose public id is `account`: the tag proves the service issued the id, the owner
    /// tag which account it issued it to.
    #[must_use]
    pub fn verify_owner(&self, account: &str, id: &str, secret: &str) -> bool {
        let Some(nonce) = self.nonce(id, secret) else {
            return false;
        };
        let (random, owner) = nonce.split_at(2 * RANDOM_BYTES);
        hex::decode(owner).is_ok_and(|owner| {
            self.owner_mac(account, id, random)
                .verify_truncated_left(&owner)
                .is_ok()
        })
    }

    /// The hex nonce of `secret` when it is well formed for `id` and carries this key's tag.
    fn nonce<'secret>(&self, id: &str, secret: &'secret str) -> Option<&'secret str> {
        let rest = secret.strip_prefix(id)?.strip_prefix(SEPARATOR)?;
        let lowercase_hex = rest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if rest.len() != 2 * (NONCE_BYTES + TAG_BYTES) || !lowercase_hex {
            return None;
        }
        let (signed, tag) = secret.split_at(secret.len() - 2 * TAG_BYTES);
        let tag = hex::decode(tag).ok()?;
        self.mac(signed).verify_truncated_left(&tag).ok()?;
        Some(&rest[..2 * NONCE_BYTES])
    }

    /// The owner tag's HMAC under the owner subkey; `random` is the hex of the random bytes.
    fn owner_mac(&self, account: &str, id: &str, random: &str) -> Hmac<Sha256> {
        keyed(
            self.owner.expose_secret(),
            &format!("{OWNER_PREFIX}{account}:{id}:{random}"),
        )
    }

    fn mac(&self, message: &str) -> Hmac<Sha256> {
        keyed(self.key.expose_secret(), message)
    }
}

fn keyed(key: &[u8], message: &str) -> Hmac<Sha256> {
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(message.as_bytes());
    mac
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "qt_0123456789abcdef0123456789abcdef";
    const ACCOUNT: &str = "acct_0123456789abcdef0123456789abcdef";

    fn key(byte: u8) -> ClientSecretKey {
        ClientSecretKey::new(SecretKey32::new([byte; 32]))
    }

    #[test]
    fn an_issued_secret_verifies_for_its_object_and_key_only() {
        let secret = key(1).issue(ACCOUNT, ID).expect("secret");
        assert_eq!(secret.len(), ID.len() + SEPARATOR.len() + 64);
        assert!(key(1).verify(ID, &secret));
        assert!(!key(2).verify(ID, &secret));
        let other = "qt_fedcba9876543210fedcba9876543210";
        assert!(!key(1).verify(other, &secret.replacen(ID, other, 1)));
        assert_ne!(
            key(1).issue(ACCOUNT, ID).expect("secret"),
            secret,
            "nonces differ"
        );
    }

    #[test]
    fn a_secret_proves_the_account_it_was_issued_to() {
        let secret = key(1).issue(ACCOUNT, ID).expect("secret");
        assert!(key(1).verify_owner(ACCOUNT, ID, &secret));
        assert!(!key(1).verify_owner("acct_fedcba9876543210fedcba9876543210", ID, &secret));
        assert!(!key(2).verify_owner(ACCOUNT, ID, &secret));
        // The owner tag as architecture §12 and the restore drill compute it.
        let nonce = &secret[ID.len() + SEPARATOR.len()..][..2 * NONCE_BYTES];
        let (random, owner) = nonce.split_at(2 * RANDOM_BYTES);
        let subkey = keyed(&[1; 32], "client-secret/owner/v1")
            .finalize()
            .into_bytes();
        let expected = keyed(&subkey, &format!("owner:{ACCOUNT}:{ID}:{random}"))
            .finalize()
            .into_bytes();
        assert_eq!(owner, hex::encode(&expected[..OWNER_TAG_BYTES]));
    }

    #[test]
    fn a_forged_or_malformed_secret_is_refused() {
        let secret = key(1).issue(ACCOUNT, ID).expect("secret");
        let mut flipped = secret.clone().into_bytes();
        let last = flipped.len() - 1;
        flipped[last] = if flipped[last] == b'0' { b'1' } else { b'0' };
        let flipped = String::from_utf8(flipped).expect("ascii");
        for forged in [
            flipped,
            secret.to_uppercase(),
            secret[..secret.len() - 2].to_owned(),
            format!("{secret}00"),
            format!("{ID}{SEPARATOR}{}", "0".repeat(48)),
            String::new(),
        ] {
            assert!(!key(1).verify(ID, &forged), "{forged}");
        }
    }
}
