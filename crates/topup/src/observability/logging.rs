use tracing::Subscriber;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt as _;

/// Targets whose spans or events can carry raw provider URLs, including credentials.
const PROVIDER_TRANSPORT_TARGETS: [&str; 3] = ["alloy_transport_http", "reqwest", "hyper_util"];

/// Builds the production JSON log subscriber, with the Sentry layer when reporting is enabled.
///
/// The INFO default and silenced transport targets are a redaction boundary: alloy's DEBUG
/// `ReqwestTransport` span records the credentialed provider URL. The target filter is a global
/// layer, so a later `EnvFilter` or `RUST_LOG` cannot re-enable those targets, and the Sentry
/// layer sees only the lines the JSON log shows. Start reporting first
/// ([`super::init_reporting`]).
pub fn log_subscriber<W>(writer: W) -> impl Subscriber + Send + Sync
where
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    let targets = PROVIDER_TRANSPORT_TARGETS.into_iter().fold(
        Targets::new().with_default(LevelFilter::INFO),
        |targets, target| targets.with_target(target, LevelFilter::OFF),
    );
    tracing_subscriber::fmt()
        .json()
        .with_max_level(LevelFilter::DEBUG)
        .with_target(false)
        .with_writer(writer)
        .finish()
        .with(targets)
        .with(super::reporting::tracing_layer())
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::fmt::MakeWriter;

    use super::log_subscriber;

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("log buffer lock")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> MakeWriter<'writer> for Buffer {
        type Writer = Self;

        fn make_writer(&'writer self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn provider_transport_output_never_reaches_production_logs() {
        let buffer = Buffer::default();
        let url = "http://user:rpc-secret-token@rpc.example/v1?api_key=rpc-secret-token";
        tracing::subscriber::with_default(log_subscriber(buffer.clone()), || {
            tracing::debug!(target: "alloy_transport_http::reqwest_transport", url, "transport");
            tracing::warn!(target: "reqwest::connect", url, "reqwest connect");
            tracing::warn!(target: "hyper_util::client", url, "hyper connect");
            let span = tracing::debug_span!(
                target: "alloy_transport_http::reqwest_transport",
                "ReqwestTransport",
                url,
            );
            let _guard = span.enter();
            tracing::info!("service event inside the transport span");
        });

        let output = String::from_utf8(buffer.0.lock().expect("log buffer lock").clone())
            .expect("logs are UTF-8");
        assert!(output.contains("service event inside the transport span"));
        assert!(!output.contains("rpc-secret-token"), "{output}");
        assert!(!output.contains("ReqwestTransport"), "{output}");
    }
}
