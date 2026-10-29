//! Prefixed public object ids, Stripe's convention: a type prefix and the 32 lowercase hex digits
//! of the object's UUID, such as `qt_0c6e1d0a9b3f4c2e8d7a6b5c4d3e2f10`.

use uuid::Uuid;

/// Account ids (`accounts.public_id`).
pub const ACCOUNT: &str = "acct_";
/// API key ids.
pub const API_KEY: &str = "key_";
/// Webhook endpoint ids.
pub const WEBHOOK_ENDPOINT: &str = "we_";
/// Quote ids.
pub const QUOTE: &str = "qt_";
/// Deposit address ids.
pub const DEPOSIT_ADDRESS: &str = "da_";
/// Forwarder ids (`GET /v1/forwarders`): an issued address.
pub const FORWARDER: &str = "fwd_";
/// Sweep ids (`GET /v1/sweeps`): a finalized `Flushed` event.
pub const SWEEP: &str = "sw_";
/// Deposit ids; the UUID is the deterministic deposit id (architecture §0).
pub const DEPOSIT: &str = "dep_";
/// Refund ids.
pub const REFUND: &str = "re_";
/// Treasury ids.
pub const TREASURY: &str = "trs_";
/// Event ids; the UUID is the event's derived id (architecture §11).
pub const EVENT: &str = "evt_";
/// Durable alert instance ids.
pub const ALERT: &str = "al_";

/// Formats `id` with `prefix`.
#[must_use]
pub fn format(prefix: &str, id: Uuid) -> String {
    format!("{prefix}{}", id.simple())
}

/// Parses an id of `prefix`; anything but the prefix and 32 lowercase hex digits is `None`.
#[must_use]
pub fn parse(prefix: &str, value: &str) -> Option<Uuid> {
    let hex = value.strip_prefix(prefix)?;
    let canonical = hex.len() == 32
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    canonical.then(|| Uuid::try_parse(hex).ok()).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_reject_other_forms() {
        let id = Uuid::from_u128(0x0c6e_1d0a_9b3f_4c2e_8d7a_6b5c_4d3e_2f10);
        let quote = format(QUOTE, id);
        assert_eq!(quote, "qt_0c6e1d0a9b3f4c2e8d7a6b5c4d3e2f10");
        assert_eq!(parse(QUOTE, &quote), Some(id));
        for invalid in [
            "dep_0c6e1d0a9b3f4c2e8d7a6b5c4d3e2f10",
            "qt_0C6E1D0A9B3F4C2E8D7A6B5C4D3E2F10",
            "qt_0c6e1d0a-9b3f-4c2e-8d7a-6b5c4d3e2f10",
            "qt_0c6e1d0a9b3f4c2e8d7a6b5c4d3e2f1",
            "0c6e1d0a9b3f4c2e8d7a6b5c4d3e2f10",
        ] {
            assert_eq!(parse(QUOTE, invalid), None, "{invalid}");
        }
    }
}
