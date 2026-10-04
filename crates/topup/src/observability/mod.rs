//! Tracing conventions, secret redaction, and optional Sentry reporting.

mod backup;
mod business;
mod capacity;
mod logging;
pub mod metrics;
mod redaction;
mod reporting;
mod request;
mod spans;
mod status;

pub use backup::monitor_backup;
pub use business::{emit_alert, monitor_business};
pub use logging::log_subscriber;
pub use redaction::{Redacted, RedactedTransportError};
pub use reporting::{CronMonitor, ReportingError, init_reporting, require_sentry_dsn};
pub use request::request_context;
pub use spans::{deposit_step_span, outbox_delivery_span, scanner_window_span};
pub use status::{ReconciliationStatus, reconciliation, record_reconciliation};

/// Price metrics and alert integration.
pub mod price_metrics;
