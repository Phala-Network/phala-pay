//! SIWE challenges and EOA/EIP-1271 ownership proofs.
use super::{Challenge, ERC6492_MAGIC_SUFFIX, Kind, MessageOrigin, TreasuryError};
use crate::{routes::RouteSet, tenancy::Scope};
use alloy_primitives::{Address, B256, Bytes, Signature, eip191_hash_message};
use async_trait::async_trait;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rand::{TryRng as _, rngs::SysRng};
use sqlx::{PgExecutor, PgPool};
use std::{collections::BTreeMap, str::FromStr, sync::Arc};
use topup_adapters::chain::evm::EvmClient;

/// The EIP-4361 statement of a challenge: what the signer agrees to.
pub(super) fn statement(account_public_id: &str, livemode: bool) -> String {
    let mode = if livemode { "live" } else { "test" };
    format!("Set this address as the {mode} mode treasury of {account_public_id} on Phala Pay.")
}

fn timestamp(time: DateTime<Utc>) -> Result<siwe::TimeStamp, TreasuryError> {
    time.to_rfc3339_opts(SecondsFormat::Millis, true)
        .parse()
        .map_err(|_| TreasuryError::DatabaseInvariant)
}

/// Renders the EIP-4361 message of a challenge with `statement`, issued at `issued_at` and
/// expiring at `expires_at`.
pub(super) fn render_message(
    origin: &MessageOrigin,
    statement: String,
    chain_id: u64,
    address: Address,
    nonce: &str,
    issued_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
) -> Result<String, TreasuryError> {
    let message = siwe::Message {
        domain: origin
            .domain
            .parse()
            .map_err(|_| TreasuryError::DatabaseInvariant)?,
        address: address.into_array(),
        statement: Some(statement),
        uri: origin
            .uri
            .parse()
            .map_err(|_| TreasuryError::DatabaseInvariant)?,
        version: siwe::Version::V1,
        chain_id,
        nonce: nonce.to_owned(),
        issued_at: timestamp(issued_at)?,
        expiration_time: Some(timestamp(expires_at)?),
        not_before: None,
        request_id: None,
        resources: Vec::new(),
    };
    Ok(message.to_string())
}

/// Issues a challenge to prove `address` as the scope's treasury on `chain_id`, valid for `ttl`:
/// [`super::CHALLENGE_TTL`] for an EOA, [`super::CONTRACT_CHALLENGE_TTL`] for an address that holds code.
pub async fn create_challenge<'e>(
    executor: impl PgExecutor<'e>,
    scope: Scope,
    account_public_id: &str,
    origin: &MessageOrigin,
    chain_id: u64,
    address: Address,
    ttl: Duration,
) -> Result<Challenge, TreasuryError> {
    let now = Utc::now();
    let mut random = [0_u8; 16];
    SysRng.try_fill_bytes(&mut random).map_err(|error| {
        tracing::error!(%error, "OS RNG failed; no treasury challenge issued");
        TreasuryError::EntropyUnavailable
    })?;
    // EIP-4361 nonces are at least 8 alphanumeric characters; 128 random bits as hex.
    let nonce = hex::encode(random);
    let expires_at = now
        .checked_add_signed(ttl)
        .ok_or(TreasuryError::DatabaseInvariant)?;
    let message = render_message(
        origin,
        statement(account_public_id, scope.livemode()),
        chain_id,
        address,
        &nonce,
        now,
        expires_at,
    )?;
    sqlx::query(
        r#"
        INSERT INTO treasury_challenges
            (nonce, account_id, livemode, chain_id, address, message, expires_at, created_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
    )
    .bind(&nonce)
    .bind(scope.account_id())
    .bind(scope.livemode())
    .bind(i64::try_from(chain_id).map_err(|_| TreasuryError::DatabaseInvariant)?)
    .bind(format!("{address:#x}"))
    .bind(&message)
    .bind(expires_at)
    .bind(now)
    .execute(executor)
    .await?;
    Ok(Challenge {
        nonce,
        chain_id,
        address,
        message,
        expires_at,
    })
}

/// Finds the scope's challenge `message` answers, for a submission to `chain_id`, without using
/// it: the message must be the challenge's exactly, unexpired, and unused.
pub async fn find_challenge(
    pool: &PgPool,
    scope: Scope,
    chain_id: u64,
    message: &str,
    now: DateTime<Utc>,
) -> Result<Challenge, TreasuryError> {
    let parsed = siwe::Message::from_str(message)
        .map_err(|_| TreasuryError::MessageInvalid("message is not an EIP-4361 message"))?;
    if parsed.chain_id != chain_id {
        return Err(TreasuryError::ChainMismatch);
    }
    let row = sqlx::query_as::<_, (i64, String, String, DateTime<Utc>, Option<DateTime<Utc>>)>(
        r#"
        SELECT chain_id, address, message, expires_at, used_at
        FROM treasury_challenges
        WHERE nonce = $1 AND account_id = $2 AND livemode = $3
        "#,
    )
    .bind(&parsed.nonce)
    .bind(scope.account_id())
    .bind(scope.livemode())
    .fetch_optional(pool)
    .await?;
    let Some((stored_chain, address, stored_message, expires_at, used_at)) = row else {
        return Err(TreasuryError::ChallengeUnknown);
    };
    if stored_message != message {
        return Err(TreasuryError::MessageInvalid(
            "message differs from the challenge's; sign it exactly as issued",
        ));
    }
    if used_at.is_some() {
        return Err(TreasuryError::ChallengeUsed);
    }
    if expires_at <= now {
        return Err(TreasuryError::ChallengeExpired);
    }
    let stored_chain = u64::try_from(stored_chain).map_err(|_| TreasuryError::DatabaseInvariant)?;
    let address = Address::from_str(&address).map_err(|_| TreasuryError::DatabaseInvariant)?;
    // The message is the stored one, so its fields are the challenge's; checked all the same.
    if stored_chain != chain_id || Address::from(parsed.address) != address {
        return Err(TreasuryError::DatabaseInvariant);
    }
    Ok(Challenge {
        nonce: parsed.nonce,
        chain_id,
        address,
        message: stored_message,
        expires_at,
    })
}

/// A contract's answer to an EIP-1271 check, as both providers agree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContractAnswer {
    /// A contract is deployed and returned the magic value.
    Valid,
    /// A contract is deployed and did not return the magic value.
    Invalid,
    /// No contract is deployed at the address.
    NotDeployed,
    /// A read failed or the providers disagree.
    Unavailable,
}

/// EIP-1271 checks at the chain's `finalized` block.
#[async_trait]
pub trait ContractSignatures: Send + Sync {
    /// Whether `account` on `chain_id` holds code at provider A's latest block, which gives its
    /// challenge the longer [`super::CONTRACT_CHALLENGE_TTL`]; `false` when the read fails.
    async fn has_code(&self, chain_id: u64, account: Address) -> bool;

    /// Whether the contract at `account` on `chain_id` accepts `signature` of `hash`.
    async fn verify(
        &self,
        chain_id: u64,
        account: Address,
        hash: B256,
        signature: Bytes,
    ) -> ContractAnswer;
}

/// EIP-1271 checks on both RPC providers of each chain, each at its own `finalized` block.
pub struct EvmContractSignatures {
    clients: BTreeMap<u64, [Arc<EvmClient>; 2]>,
    pool: PgPool,
}

impl EvmContractSignatures {
    /// Checks through providers A and B of every chain of `routes`.
    pub fn from_routes(pool: PgPool, routes: &RouteSet) -> Result<Self, String> {
        let mut clients = BTreeMap::new();
        for chain_id in routes.chain_ids() {
            let provider = |index| {
                routes
                    .provider(chain_id, index)
                    .map(Arc::clone)
                    .map_err(|error| error.to_string())
            };
            clients.insert(chain_id, [provider(0)?, provider(1)?]);
        }
        Ok(Self { clients, pool })
    }

    /// Checks through explicit per-chain providers A and B.
    #[must_use]
    pub const fn new(pool: PgPool, clients: BTreeMap<u64, [Arc<EvmClient>; 2]>) -> Self {
        Self { clients, pool }
    }
}

async fn provider_answer(
    client: &EvmClient,
    account: Address,
    hash: B256,
    signature: Bytes,
    block: alloy::eips::BlockId,
) -> ContractAnswer {
    match client.code_at_id(account, block).await {
        Ok(code) if code.is_empty() => return ContractAnswer::NotDeployed,
        Ok(_) => {}
        Err(_) => return ContractAnswer::Unavailable,
    }
    match client
        .is_valid_signature(account, hash, signature, block)
        .await
    {
        Ok(true) => ContractAnswer::Valid,
        Ok(false) => ContractAnswer::Invalid,
        Err(_) => ContractAnswer::Unavailable,
    }
}

#[async_trait]
impl ContractSignatures for EvmContractSignatures {
    async fn has_code(&self, chain_id: u64, account: Address) -> bool {
        let Some([primary, _]) = self.clients.get(&chain_id) else {
            return false;
        };
        match primary.code_at(account).await {
            Ok(code) => !code.is_empty(),
            Err(error) => {
                tracing::warn!(chain_id, %error, "treasury challenge code read failed");
                false
            }
        }
    }

    async fn verify(
        &self,
        chain_id: u64,
        account: Address,
        hash: B256,
        signature: Bytes,
    ) -> ContractAnswer {
        let Some([primary, secondary]) = self.clients.get(&chain_id) else {
            return ContractAnswer::Unavailable;
        };
        let block = match crate::db::chain_reads::checkpoint(&self.pool, chain_id).await {
            Ok(Some(boundary)) => alloy::eips::BlockId::hash_canonical(boundary.hash),
            _ => return ContractAnswer::Unavailable,
        };
        let (a, b) = tokio::join!(
            provider_answer(primary, account, hash, signature.clone(), block),
            provider_answer(secondary, account, hash, signature, block)
        );
        match (a, b) {
            (a, b) if a == b => a,
            (ContractAnswer::Unavailable, _) | (_, ContractAnswer::Unavailable) => {
                ContractAnswer::Unavailable
            }
            // A provider whose contract refuses the signature: not proven.
            (ContractAnswer::Invalid, _) | (_, ContractAnswer::Invalid) => ContractAnswer::Invalid,
            _ => ContractAnswer::Unavailable,
        }
    }
}

/// EIP-1271 checks that are never available, for an instance that sets no treasuries.
pub struct UnavailableContractSignatures;

#[async_trait]
impl ContractSignatures for UnavailableContractSignatures {
    async fn has_code(&self, _: u64, _: Address) -> bool {
        false
    }

    async fn verify(&self, _: u64, _: Address, _: B256, _: Bytes) -> ContractAnswer {
        ContractAnswer::Unavailable
    }
}

/// Checks that `signature` of the challenge's message proves its address (module docs): an EOA's
/// EIP-191 signature, or a deployed contract's EIP-1271 approval on both providers at
/// `finalized`.
pub async fn verify_signature(
    contracts: &dyn ContractSignatures,
    challenge: &Challenge,
    signature: &[u8],
) -> Result<Kind, TreasuryError> {
    if signature.ends_with(&ERC6492_MAGIC_SUFFIX) {
        return Err(TreasuryError::Erc6492);
    }
    let message = challenge.message.as_bytes();
    if let Ok(parsed) = Signature::try_from(signature)
        && parsed.recover_address_from_msg(message).ok() == Some(challenge.address)
    {
        return Ok(Kind::Eoa);
    }
    match contracts
        .verify(
            challenge.chain_id,
            challenge.address,
            eip191_hash_message(message),
            Bytes::copy_from_slice(signature),
        )
        .await
    {
        ContractAnswer::Valid => Ok(Kind::Contract),
        ContractAnswer::Invalid => Err(TreasuryError::SignatureInvalid),
        ContractAnswer::NotDeployed => Err(TreasuryError::NotDeployed),
        ContractAnswer::Unavailable => Err(TreasuryError::Unavailable),
    }
}
