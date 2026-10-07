pub use topup_adapters::redaction::{Redacted, RedactedTransportError};

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    use topup_adapters::chain::evm::{EvmClient, FinalizedReader};
    use topup_core::route::RouteFile;
    use tracing_test::traced_test;

    use crate::reconciler::{CheckName, Reconciler, ReconciliationChain};
    use crate::routes::RouteSet;

    use super::Redacted;

    #[test]
    fn env_derived_provider_url_is_redacted_in_errors_and_logs() {
        let secret = "rpc-secret-token";
        let value = format!("https://user:{secret}@rpc.example/v1?api_key={secret}");
        let provider = Redacted::parse(&value).expect("valid provider URL");
        let message = format!("provider {provider:?} failed: {provider}");

        assert_eq!(message, "provider [REDACTED URL] failed: [REDACTED URL]");
        assert!(!message.contains(secret));
        assert!(!message.contains("user"));
    }

    #[traced_test]
    #[tokio::test]
    async fn failing_rpc_provider_is_logged_by_production_code_without_url_credentials() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("local listener binds");
        let address = listener.local_addr().expect("listener address");
        // Every connection closes before a response, so the real HTTP transport fails.
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        let secret = "rpc-secret-token";
        let rpc_url = format!("http://user:{secret}@{address}/rpc?api_key={secret}");
        let route: RouteFile =
            serde_saphyr::from_str(include_str!("../../tests/fixtures/phala-cloud-pha.yaml"))
                .expect("route fixture parses");
        let chain_id = route.chain.chain_id;
        let chain = FinalizedReader::new(Arc::new(
            EvmClient::with_timeout(&rpc_url, Duration::from_secs(5))
                .expect("production adapter accepts URL")
                .with_provider("provider-a"),
        ));
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1/unused")
            .expect("lazy pool URL is valid");
        let reconciler = Reconciler::with_dependencies(
            pool,
            Arc::new(RouteSet::new(vec![route]).expect("route loads")),
            BTreeMap::from([(chain_id, Arc::new(chain) as Arc<dyn ReconciliationChain>)]),
        );

        reconciler
            .check(CheckName::CustodyBalance)
            .await
            .expect_err("closed connections fail the finalized-head request");
        server.abort();

        logs_assert(|lines: &[&str]| {
            let line = lines
                .iter()
                .find(|line| line.contains("custody balance check failed for chain"))
                .ok_or_else(|| "missing production reconciler error log".to_owned())?;
            if !line.contains("finalized head fetch failed for provider `provider-a` (transport)") {
                return Err(format!("adapter error was not redacted: {line}"));
            }
            // This capture enables every level; `log_subscriber` drops alloy's DEBUG transport
            // span, which records the raw URL, so only production-visible levels are checked.
            let production_levels = [" INFO ", " WARN ", " ERROR "];
            if let Some(line) = lines.iter().find(|line| {
                production_levels.iter().any(|level| line.contains(level))
                    && (line.contains(secret) || line.contains("api_key"))
            }) {
                return Err(format!("provider URL credentials reached the logs: {line}"));
            }
            Ok(())
        });
    }
}
