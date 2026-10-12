//! Stripe's `metadata` (<https://docs.stripe.com/api/metadata>,
//! <https://docs.stripe.com/metadata>) on quotes, deposits, and refunds: up to 50 string key/value
//! pairs, keys of up to 40 characters without square brackets, values of up to 500 characters.
//! Deposit addresses carry it too; a deposit starts with a copy of its quote's or its deposit
//! address's.
//!
//! A request's `metadata` is merged into the object's: a key with a value sets it, a key whose
//! value is `""` unsets it, and `metadata: ""` unsets every key. The service never reads metadata.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer};
use serde_json::Value;
use sqlx::types::Json;
use sqlx::{Acquire, Postgres};
use uuid::Uuid;

use crate::tenancy::Scope;

use super::error::ApiError;

/// An object's metadata as stored and returned: never an empty value.
pub type Metadata = BTreeMap<String, String>;

const MAX_KEYS: usize = 50;
const MAX_KEY_CHARS: usize = 40;
const MAX_VALUE_CHARS: usize = 500;

/// A request's `metadata` parameter, validated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetadataUpdate {
    /// `metadata: ""`: unset every key.
    Clear,
    /// Set each key with `Some`, unset each with `None` (a `""` value).
    Merge(BTreeMap<String, Option<String>>),
}

impl MetadataUpdate {
    /// Parses a request's `metadata`: an object of strings, or `""`.
    pub fn parse(value: &Value) -> Result<Self, ApiError> {
        let object = match value {
            Value::String(text) if text.is_empty() => return Ok(Self::Clear),
            Value::Object(object) => object,
            _ => {
                return Err(ApiError::invalid_param(
                    "metadata",
                    "metadata must be an object of string values, or \"\" to unset every key",
                ));
            }
        };
        let mut pairs = BTreeMap::new();
        for (key, value) in object {
            let param = || format!("metadata[{key}]");
            if key.is_empty() || key.chars().count() > MAX_KEY_CHARS {
                return Err(ApiError::invalid_param(
                    param(),
                    format!("metadata keys must contain 1 to {MAX_KEY_CHARS} characters"),
                ));
            }
            if key.contains(['[', ']']) {
                return Err(ApiError::invalid_param(
                    param(),
                    "metadata keys cannot contain square brackets",
                ));
            }
            let Value::String(value) = value else {
                return Err(ApiError::invalid_param(
                    param(),
                    "metadata values must be strings",
                ));
            };
            if value.chars().count() > MAX_VALUE_CHARS {
                return Err(ApiError::invalid_param(
                    param(),
                    format!("metadata values can contain up to {MAX_VALUE_CHARS} characters"),
                ));
            }
            pairs.insert(key.clone(), (!value.is_empty()).then(|| value.clone()));
        }
        Ok(Self::Merge(pairs))
    }

    /// The metadata an object holds after the update: `current` merged with it.
    pub fn apply(&self, mut current: Metadata) -> Result<Metadata, ApiError> {
        match self {
            Self::Clear => current.clear(),
            Self::Merge(pairs) => {
                for (key, value) in pairs {
                    match value {
                        Some(value) => current.insert(key.clone(), value.clone()),
                        None => current.remove(key),
                    };
                }
            }
        }
        if current.len() > MAX_KEYS {
            return Err(ApiError::invalid_param(
                "metadata",
                format!("metadata can have up to {MAX_KEYS} keys"),
            ));
        }
        Ok(current)
    }
}

/// A create request's metadata: the parameter applied to none.
pub fn on_create(value: Option<&Value>) -> Result<Metadata, ApiError> {
    value.map_or_else(
        || Ok(Metadata::new()),
        |value| MetadataUpdate::parse(value)?.apply(Metadata::new()),
    )
}

/// An object that carries metadata.
#[derive(Clone, Copy, Debug)]
pub enum Object {
    /// A `quotes` row.
    Quote,
    /// A `deposits` row.
    Deposit,
    /// A `refunds` row.
    Refund,
    /// A `deposit_addresses` row.
    DepositAddress,
}

macro_rules! statements {
    ($table:literal) => {
        (
            concat!(
                "SELECT metadata FROM ",
                $table,
                " WHERE id = $1 AND account_id = $2 AND livemode = $3 FOR UPDATE"
            ),
            concat!("UPDATE ", $table, " SET metadata = $2 WHERE id = $1"),
        )
    };
}

impl Object {
    /// The `*.updated` event a change of the object is announced as, for the objects that have
    /// one.
    const fn updated_event(self, id: Uuid) -> Option<(&'static str, crate::db::EventObject)> {
        match self {
            Self::Refund => Some(("refund.updated", crate::db::EventObject::Refund(id))),
            Self::Quote | Self::Deposit | Self::DepositAddress => None,
        }
    }
}

/// Applies a `POST /v1/{object}/{id}` update's `metadata` to the scope's object `id`, and returns
/// whether the object exists in the scope. Without `metadata` nothing changes. A change of an
/// object with a `*.updated` event (a refund) is announced by it, caused by `actor`.
pub async fn update<'c>(
    db: impl Acquire<'c, Database = Postgres>,
    routes: &crate::routes::RouteSet,
    object: Object,
    scope: Scope,
    id: Uuid,
    metadata: Option<&Value>,
    actor: &crate::audit::Actor,
) -> Result<bool, ApiError> {
    let update = metadata.map(MetadataUpdate::parse).transpose()?;
    let (select, write) = match object {
        Object::Quote => statements!("quotes"),
        Object::Deposit => statements!("deposits"),
        Object::Refund => statements!("refunds"),
        Object::DepositAddress => statements!("deposit_addresses"),
    };
    let mut transaction = db.begin().await?;
    let Some(Json(current)) = sqlx::query_scalar::<_, Json<Metadata>>(select)
        .bind(id)
        .bind(scope.account_id())
        .bind(scope.livemode())
        .fetch_optional(&mut *transaction)
        .await?
    else {
        transaction.rollback().await?;
        return Ok(false);
    };
    if let Some(update) = update {
        let merged = match update.apply(current.clone()) {
            Ok(merged) => merged,
            Err(error) => {
                // Awaited, so the object is unlocked before the refusal is answered.
                transaction.rollback().await?;
                return Err(error);
            }
        };
        if merged != current {
            let announced = match object.updated_event(id) {
                Some((event_type, event_object)) => {
                    let before =
                        crate::db::render(&mut transaction, routes, scope, event_object).await?;
                    Some((event_type, event_object, before))
                }
                None => None,
            };
            sqlx::query(write)
                .bind(id)
                .bind(Json(&merged))
                .execute(&mut *transaction)
                .await?;
            if let Some((event_type, event_object, before)) = announced {
                let event = crate::db::NewOutboxEvent::new(event_type, scope, event_object, actor);
                crate::db::enqueue_in(&mut transaction, routes, &event, Some(&before)).await?;
            }
        }
    }
    transaction.commit().await?;
    Ok(true)
}

/// Deserializes a present `metadata` as `Some`, including `null`, so that [`MetadataUpdate::parse`]
/// rejects `null` instead of reading it as absent.
pub fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn param(value: &Value) -> Option<String> {
        match MetadataUpdate::parse(value).and_then(|update| update.apply(Metadata::new())) {
            Ok(_) => None,
            Err(error) => Some(error.param().unwrap_or_default().to_owned()),
        }
    }

    #[test]
    fn limits_follow_stripe() {
        let fifty: serde_json::Map<String, Value> =
            (0..50).map(|n| (format!("k{n}"), json!("v"))).collect();
        assert_eq!(param(&Value::Object(fifty.clone())), None);
        let mut fifty_one = fifty;
        fifty_one.insert("k50".to_owned(), json!("v"));
        assert_eq!(
            param(&Value::Object(fifty_one)),
            Some("metadata".to_owned())
        );

        assert_eq!(param(&json!({ "k".repeat(40): "v" })), None);
        let long_key = "k".repeat(41);
        assert_eq!(
            param(&json!({ long_key.clone(): "v" })),
            Some(format!("metadata[{long_key}]"))
        );
        // Characters, not bytes.
        assert_eq!(param(&json!({ "é".repeat(40): "é".repeat(500) })), None);
        assert_eq!(
            param(&json!({ "order": "v".repeat(501) })),
            Some("metadata[order]".to_owned())
        );
        for key in ["a[b]", "a[", "]", ""] {
            assert_eq!(
                param(&json!({ key: "v" })),
                Some(format!("metadata[{key}]"))
            );
        }
        for value in [json!(1), json!(true), json!(null), json!({}), json!(["v"])] {
            assert_eq!(
                param(&json!({ "order": value })),
                Some("metadata[order]".to_owned())
            );
        }
        for metadata in [json!(null), json!("x"), json!(1), json!(["v"])] {
            assert_eq!(param(&metadata), Some("metadata".to_owned()));
        }
    }

    #[test]
    fn empty_values_unset_and_an_empty_string_unsets_everything() {
        let current = Metadata::from([
            ("a".to_owned(), "1".to_owned()),
            ("b".to_owned(), "2".to_owned()),
        ]);
        let merged = MetadataUpdate::parse(&json!({ "a": "", "c": "3" }))
            .and_then(|update| update.apply(current.clone()))
            .ok();
        assert_eq!(
            merged,
            Some(Metadata::from([
                ("b".to_owned(), "2".to_owned()),
                ("c".to_owned(), "3".to_owned()),
            ]))
        );
        let cleared = MetadataUpdate::parse(&json!(""))
            .and_then(|update| update.apply(current.clone()))
            .ok();
        assert_eq!(cleared, Some(Metadata::new()));
        // An empty object merges nothing.
        let unchanged = MetadataUpdate::parse(&json!({}))
            .and_then(|update| update.apply(current.clone()))
            .ok();
        assert_eq!(unchanged, Some(current));
        // The key limit applies to the merged result, so an update may replace keys at the limit.
        let full: Metadata = (0..50).map(|n| (format!("k{n}"), "v".to_owned())).collect();
        let swapped = MetadataUpdate::parse(&json!({ "k0": "", "new": "v" }))
            .and_then(|update| update.apply(full.clone()));
        assert!(swapped.is_ok());
        let over =
            MetadataUpdate::parse(&json!({ "new": "v" })).and_then(|update| update.apply(full));
        assert!(over.is_err());
    }

    #[test]
    fn a_create_without_metadata_has_none() {
        assert_eq!(on_create(None).ok(), Some(Metadata::new()));
        assert_eq!(
            on_create(Some(&json!({ "order": "6735", "gone": "" }))).ok(),
            Some(Metadata::from([("order".to_owned(), "6735".to_owned())]))
        );
    }
}
