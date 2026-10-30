//! Count actual SQLx statement events in a scoped async subscriber.
//! Scale tests use `traced_test` to initialize tracing before installing this scoped counter.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tracing_subscriber::{
    Layer, Registry,
    layer::{Context, SubscriberExt},
};

#[derive(Clone, Default)]
pub struct Queries(Arc<AtomicUsize>);

impl<S: tracing::Subscriber> Layer<S> for Queries {
    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::DEBUG)
    }
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        if event.metadata().target() == "sqlx::query" {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
}
impl Queries {
    pub fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync + use<> {
        Registry::default().with(self.clone())
    }
    pub fn count(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}
