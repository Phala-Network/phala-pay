//! Service boundary modules for the Phala Pay binary.

#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

// Match the development signer guard: fixture endpoints must never ship in release builds.
#[cfg(all(feature = "test-support", not(debug_assertions)))]
compile_error!("the test-support feature must not be enabled in release builds");

pub mod api;
pub mod api_keys;
pub mod audit;
pub mod chain_rpc;
pub mod checkpoint;
pub mod client_secret;
pub mod config;
pub mod contracts;
pub mod db;
pub mod deposit_addresses;
pub mod finality;
pub mod heartbeat;
pub mod hints;
pub mod ids;
pub mod jitter;
pub mod keys;
pub mod limits;
pub mod locks;
pub mod observability;
pub mod outbox;
mod pause;
pub mod payment_config;
pub mod pump;
pub mod reconciler;
pub mod refunds;
pub mod restore;
pub mod restore_mode;
pub mod routes;
pub mod rpc_provider;
pub mod sanctions;
pub mod scanner;
pub mod steps;
pub mod tenancy;
pub mod treasuries;
pub mod webhook_endpoints;
pub mod webhook_keys;

/// Typed read/verify endpoint self-tests and checkpoint initialization.
pub mod rpc_runtime;

#[cfg(test)]
extern crate self as topup;
#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod test_support;
