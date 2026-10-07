//! The two OpenAPI documents: the merchant API (`openapi.json`, what the SDKs are generated from
//! and the API reference is built from) and the operator's admin API (`openapi.admin.json`).
//!
//! Each is utoipa's document finished with what the handlers' annotations do not carry: the
//! reference's introduction (authentication, request ids, idempotency, pagination, events, and one
//! section per error code, which error objects' `doc_url` points to), the servers, the tags, a
//! single-value `enum` on every object's `object` field (Stripe's `"object": "quote"`), and an
//! example of every object and request body. `status`-like fields stay plain strings: a value
//! added later must not break a generated client.

use serde_json::{Value, json};
use utoipa::openapi::OpenApi;

use super::error::ERROR_CODES;
use super::examples;

/// The merchant API's servers.
const SERVERS: &[(&str, &str)] = &[
    (
        "https://pay-api.phala.com",
        "Production: Ethereum Mainnet, live and test mode",
    ),
    (
        "https://pay-api-staging.phala.com",
        "Staging: Sepolia, for integration tests",
    ),
];

/// The merchant API's tags, in the reference's order.
const MERCHANT_TAGS: &[(&str, &str)] = &[
    (
        "quotes",
        "A locked price and a single-use address for one payment.",
    ),
    (
        "deposit_addresses",
        "A customer's persistent address, credited at spot on every chain and asset.",
    ),
    (
        "deposits",
        "Payments recorded on chain: credited, rejected, or reversed, and when they are final.",
    ),
    (
        "refunds",
        "Refunds you pay from your treasury and attach with `mark_paid`.",
    ),
    ("balance", "Unswept amounts per chain and token."),
    (
        "sweeps",
        "Finalized sweeps of forwarders to your treasuries.",
    ),
    (
        "forwarders",
        "Every issued address, with what you need to sweep it.",
    ),
    (
        "treasuries",
        "Where each chain's forwarders pay, proven and time-locked.",
    ),
    (
        "events",
        "Notifications and the audit log: every change, as it was when it happened.",
    ),
    (
        "webhook_endpoints",
        "Where events are delivered, with delivery health.",
    ),
    (
        "api_keys",
        "Your secret and restricted keys: create, roll, revoke.",
    ),
    (
        "account",
        "Your account: settings, pauses, webhook signing keys.",
    ),
    (
        "attestation",
        "TDX evidence binding your webhook signing keys.",
    ),
    (
        "config",
        "The payable assets and their terms in the key's mode.",
    ),
];

/// The merchant document as served and committed.
pub(super) fn merchant(openapi: &OpenApi) -> Value {
    let mut document = finish(openapi, "Phala Pay API", &merchant_description());
    // Every merchant request counts toward the account's rate limit.
    for_each_operation(&mut document, |_, operation| {
        let responses = &mut operation["responses"];
        if responses.get("403").is_none() {
            responses["403"] = error_response(
                "`permission_denied`: the key lacks this permission, or `testmode_charges_only`: live mode is not enabled",
            );
        }
        if responses.get("429").is_none() {
            responses["429"] = error_response("`rate_limit`: retry after `Retry-After` seconds");
        }
    });
    document["tags"] = tags(MERCHANT_TAGS);
    document["components"]["securitySchemes"] = json!({
        "api_key": {
            "type": "http",
            "scheme": "bearer",
            "description": "A restricted key, `Authorization: Bearer ppay_rk_test_…` or \
                            `ppay_rk_live_…`, or a secret key, `ppay_sk_test_…` or \
                            `ppay_sk_live_…`; the key selects the account and the mode. HTTP Basic \
                            is not accepted.",
        }
    });
    response_headers(&mut document);
    document
}

/// The admin document as served and committed.
pub(super) fn admin(openapi: &OpenApi) -> Value {
    let mut document = finish(
        openapi,
        "Phala Pay admin API",
        "The operator's API (design D8): accounts, recovery keys, pauses, and platform health. \
         Requests use RFC 9421 signatures. Configured maintenance keys authorize only POST \
         /v1/admin/instance/pause and /v1/admin/instance/resume; every other admin route requires \
         the operator's full admin key. Scope denials are audited 403 permission_denied. The \
         merchant API is `openapi.json`.",
    );
    document["tags"] = tags(&[("admin", "The operator's actions and reports.")]);
    document["components"]["securitySchemes"] = json!({
        "http_message_signature": {
            "type": "apiKey",
            "in": "header",
            "name": "Signature",
            "description": "An RFC 9421 ed25519 signature over `@method`, `@target-uri`, \
                            `content-digest`, and `idempotency-key` when sent. `@target-uri` is the \
                            service's configured public origin (`public_origin`) followed by \
                            the request path and query, so sign the public URL you call; `Host` and \
                            `X-Forwarded-*` headers are ignored.",
        }
    });
    response_headers(&mut document);
    document
}

fn finish(openapi: &OpenApi, title: &str, description: &str) -> Value {
    let mut document = serde_json::to_value(openapi).unwrap_or_else(|error| {
        // utoipa's document is plain data; it always serializes.
        unreachable!("OpenAPI serialization failed: {error}")
    });
    document["info"] = json!({
        "title": title,
        "version": env!("CARGO_PKG_VERSION"),
        "description": description,
        "license": {"name": "Apache-2.0", "identifier": "Apache-2.0"},
    });
    document["servers"] = SERVERS
        .iter()
        .map(|(url, description)| json!({"url": url, "description": description}))
        .collect();
    for_each_operation(&mut document, |method, operation| {
        let responses = &mut operation["responses"];
        if responses.get("503").is_none() {
            responses["503"] = error_response(
                "`unavailable`: temporary service or database unavailability; `service_restoring`: restore reconciliation is in progress. Retry-After, when present, is the minimum delay in seconds",
            );
        }
        if method == "post" && responses.get("409").is_none() {
            responses["409"] = error_response(
                "`idempotency_key_in_use`: a request with this `Idempotency-Key` is still \
                 running; retry with the same key",
            );
        }
    });
    if let Some(schemas) = document
        .pointer_mut("/components/schemas")
        .and_then(Value::as_object_mut)
    {
        for (name, schema) in schemas.iter_mut() {
            object_enum(schema);
            if let (Some(example), Some(schema)) = (examples::schema(name), schema.as_object_mut())
            {
                schema.insert("example".to_owned(), example);
            }
        }
    }
    document
}

fn response_headers(document: &mut Value) {
    for_each_operation(document, |_, operation| {
        for (status, response) in operation["responses"].as_object_mut().into_iter().flatten() {
            let headers = &mut response["headers"];
            headers["Cache-Control"] = json!({
                "description": "Tenant data and credentials must never be stored, including errors",
                "required": true,
                "schema": {"type": "string", "enum": ["no-store"]},
            });
            headers["Request-Id"] = json!({
                "description": "The request's id, `req_…` (https://docs.stripe.com/api/request_ids)",
                "schema": {"type": "string"},
            });
            if status == "429" || status == "503" {
                headers["Retry-After"] = json!({
                    "description": if status == "429" {
                        "Required: minimum seconds to wait before retrying"
                    } else {
                        "Optional: minimum seconds to wait before retrying; sent for database admission failures and service_restoring, and may be absent for other unavailable errors"
                    },
                    "required": status == "429",
                    "schema": {"type": "integer", "minimum": 1},
                });
            }
        }
    });
}

/// Calls `edit` with the method and the operation object of every operation of the document.
fn for_each_operation(document: &mut Value, mut edit: impl FnMut(&str, &mut Value)) {
    for item in document["paths"]
        .as_object_mut()
        .into_iter()
        .flatten()
        .map(|(_, item)| item)
    {
        for (method, operation) in item.as_object_mut().into_iter().flatten() {
            if operation.is_object() {
                edit(method, operation);
            }
        }
    }
}

/// An error response described by `description`.
fn error_response(description: &str) -> Value {
    json!({
        "description": description,
        "content": {
            "application/json": {"schema": {"$ref": "#/components/schemas/ErrorResponse"}}
        },
    })
}

/// Gives an `object` property documented as "Always `x`." the single-value `enum: ["x"]`.
fn object_enum(schema: &mut Value) {
    let Some(object) = schema.pointer_mut("/properties/object") else {
        return;
    };
    let name = object
        .get("description")
        .and_then(Value::as_str)
        .and_then(|description| description.strip_prefix("Always `"))
        .and_then(|rest| rest.split_once('`'))
        .map(|(name, _)| name.to_owned());
    if let (Some(name), Some(object)) = (name, object.as_object_mut()) {
        object.insert("enum".to_owned(), json!([name]));
    }
}

fn tags(tags: &[(&str, &str)]) -> Value {
    tags.iter()
        .map(|(name, description)| json!({"name": name, "description": description}))
        .collect()
}

/// The reference's introduction: its `#` headings are sections of the reference, and each error
/// code's `##` heading is the anchor `section/Errors/<code>` of its `doc_url`.
fn merchant_description() -> String {
    let mut text = String::from(
        "Phala Pay is a non-custodial crypto payments API for merchants: quotes and deposit \
         addresses, deposits credited at a small confirmation depth and watched to finality, \
         refunds you pay from your treasury, and webhooks signed with a key bound to a TDX \
         attestation. The [integration guide](https://github.com/Phala-Network/phala-pay/blob/main/docs/integration.md) \
         walks through an integration end to end.\n\n\
         # Authentication\n\n\
         Send an API key as `Authorization: Bearer ppay_rk_test_…` (test mode) or \
         `ppay_rk_live_…` (live mode). A restricted key (`ppay_rk_`, \
         [Stripe](https://docs.stripe.com/keys#limit-access)) holds only the permissions it was \
         created with and never manages keys, treasuries, webhook endpoints, webhook keys, or \
         account settings: run production with one. A secret key (`ppay_sk_`) holds every \
         permission; keep it offline, for administration. The key selects the account and the \
         mode; every object carries `livemode`, and a key never sees the other mode's \
         objects.\n\n\
         # Request ids\n\n\
         Every response carries `Request-Id: req_…` \
         ([Stripe](https://docs.stripe.com/api/request_ids)). Quote it when you contact support. \
         An event caused by an API request names it in `request.id`, with the \
         `Idempotency-Key` the request sent.\n\n\
         # Idempotent requests\n\n\
         Every `POST` accepts an `Idempotency-Key` of up to 255 characters \
         ([Stripe](https://docs.stripe.com/api/idempotent_requests)). For 24 hours a repeat of the \
         same request returns the first response, with `Idempotent-Replayed: true`, whatever it \
         was, including a `500`, so a retry never runs a request twice; a repeat with another \
         request is `400 idempotency_key_reused`, and one while the first still runs is \
         `409 idempotency_key_in_use`. A request that did not execute is not saved and runs again \
         on a retry: one that failed validation (`parameter_*`), was rate limited (`429`), or met \
         `503 unavailable`.\n\n\
         # Pagination\n\n\
         Lists are newest first with Stripe's cursor pagination \
         ([Stripe](https://docs.stripe.com/api/pagination)): `limit` (1 to 100, default 10), and \
         `starting_after` or `ending_before`, the id of an object of the list; `has_more` says \
         whether more follow. Lists with a `created` filter take `created[gt]`, `created[gte]`, \
         `created[lt]`, and `created[lte]` in Unix seconds.\n\n\
         # Events\n\n\
         Every change is an event, delivered to your webhook endpoints and listed by \
         `GET /v1/events` ([Stripe](https://docs.stripe.com/api/events/object)). `data.object` is \
         the object as it was when the event happened, rendered with the change and never \
         changed afterwards; `*.updated` events add `data.previous_attributes`. Deliveries are \
         retried until delivered and never given up on; a webhook endpoint's \
         `pending_deliveries`, `oldest_pending_at`, and `last_attempt` show whether it is \
         keeping up, and `GET /v1/events?delivery_success=false` lists what it has not \
         received.\n\n\
         # Errors\n\n\
         Errors are Stripe's error object ([Stripe](https://docs.stripe.com/api/errors)): \
         `{\"error\": {\"type\", \"code\", \"message\", \"param\", \"doc_url\"}}`. `400` means the \
         request cannot succeed as sent or in the objects' current state, `401` authentication, \
         `403` permission, `404` a missing object, `409` only an `Idempotency-Key` still in use, \
         `429` too many requests (with `Retry-After` in seconds), and `5xx` the service. Branch \
         on `code`; `message` may change.\n",
    );
    for (code, status, description) in ERROR_CODES {
        text.push_str(&format!("\n## {code}\n\n`{status}`. {description}\n"));
    }
    text
}

/// The operations of a document, `METHOD path`, for the tests.
#[cfg(test)]
fn operations(document: &Value) -> Vec<(String, &serde_json::Map<String, Value>)> {
    document["paths"]
        .as_object()
        .into_iter()
        .flatten()
        .flat_map(|(path, item)| {
            item.as_object()
                .into_iter()
                .flatten()
                .filter_map(move |(method, operation)| {
                    operation
                        .as_object()
                        .map(|operation| (format!("{} {path}", method.to_uppercase()), operation))
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn documents() -> (Value, Value) {
        let (merchant, admin) = super::super::documents();
        (super::merchant(&merchant), super::admin(&admin))
    }

    /// Resolves a `$ref` to its component schema.
    fn resolve<'a>(document: &'a Value, schema: &'a Value) -> &'a Value {
        match schema.get("$ref").and_then(Value::as_str) {
            Some(reference) => document
                .pointer(&reference.replacen('#', "", 1))
                .unwrap_or(&Value::Null),
            None => schema,
        }
    }

    /// Checks `example` against `schema`: every required property present, no property the schema
    /// does not have, recursively through objects, arrays, and `$ref`s.
    fn conforms(document: &Value, schema: &Value, example: &Value, path: &str) {
        let schema = resolve(document, schema);
        if let Some(options) = schema
            .get("oneOf")
            .or_else(|| schema.get("anyOf"))
            .and_then(Value::as_array)
        {
            if !example.is_null() && options.len() == 1 {
                conforms(document, &options[0], example, path);
            }
            return;
        }
        match example {
            Value::Object(fields) => {
                let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
                    return;
                };
                for required in schema
                    .get("required")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    assert!(fields.contains_key(required), "{path} lacks {required}");
                }
                for (name, value) in fields {
                    let property = properties
                        .get(name)
                        .unwrap_or_else(|| panic!("{path}.{name} is not in the schema"));
                    conforms(document, property, value, &format!("{path}.{name}"));
                }
            }
            Value::Array(items) => {
                if let Some(item) = schema.get("items") {
                    for (index, value) in items.iter().enumerate() {
                        conforms(document, item, value, &format!("{path}[{index}]"));
                    }
                }
            }
            _ => {}
        }
    }

    #[test]
    fn every_object_names_itself_with_a_single_value_enum() {
        let (merchant, admin) = documents();
        for document in [&merchant, &admin] {
            for (name, schema) in document["components"]["schemas"]
                .as_object()
                .into_iter()
                .flatten()
            {
                // `EventData.object` is the event's object itself, not its name.
                if let Some(object) = schema
                    .pointer("/properties/object")
                    .filter(|object| object["type"] == "string")
                {
                    let values = object["enum"].as_array();
                    assert_eq!(values.map(Vec::len), Some(1), "{name}.object");
                }
                // A status is a string any value of which a client accepts.
                if let Some(status) = schema.pointer("/properties/status") {
                    assert!(status.get("enum").is_none(), "{name}.status");
                }
            }
        }
    }

    #[test]
    fn every_request_and_response_body_has_a_conforming_example() {
        let (merchant, admin) = documents();
        for document in [&merchant, &admin] {
            for (operation, item) in operations(document) {
                let bodies = item.get("requestBody").into_iter().chain(
                    item["responses"]
                        .as_object()
                        .into_iter()
                        .flatten()
                        .map(|(_, r)| r),
                );
                for body in bodies {
                    let Some(schema) = body.pointer("/content/application~1json/schema") else {
                        continue;
                    };
                    let resolved = resolve(document, schema);
                    let example = resolved
                        .get("example")
                        .unwrap_or_else(|| panic!("{operation}: a body without an example"));
                    conforms(document, resolved, example, &operation);
                }
            }
        }
    }

    #[test]
    fn an_updated_event_example_conforms() {
        let (merchant, _) = documents();
        let schema = &merchant["components"]["schemas"]["EventObjectResponse"];
        conforms(
            &merchant,
            schema,
            &examples::updated_event(),
            "updated_event",
        );
    }

    #[test]
    fn documents_are_split_and_describe_every_error_code() {
        let (merchant, admin) = documents();
        let merchant_operations = operations(&merchant);
        assert!(!merchant_operations.is_empty());
        assert!(
            merchant_operations
                .iter()
                .all(|(operation, _)| !operation.contains("/v1/admin/"))
        );
        assert!(
            operations(&admin)
                .iter()
                .all(|(operation, _)| operation.contains("/v1/admin/"))
        );
        let description = merchant["info"]["description"].as_str().unwrap_or_default();
        for (code, _, _) in ERROR_CODES {
            assert!(description.contains(&format!("\n## {code}\n")), "{code}");
        }
        for (name, _) in MERCHANT_TAGS {
            assert!(
                merchant_operations.iter().any(|(_, operation)| {
                    operation["tags"]
                        .as_array()
                        .is_some_and(|tags| tags.contains(&json!(name)))
                }),
                "tag {name} has no operation"
            );
        }
        assert_eq!(
            merchant["servers"].as_array().map(Vec::len),
            Some(SERVERS.len())
        );
    }
}
