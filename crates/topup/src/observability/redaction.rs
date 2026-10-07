pub use topup_adapters::redaction::{Redacted, RedactedTransportError};

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use topup_adapters::chain::evm::EvmClient;
    use tracing_test::traced_test;

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
    async fn failing_rpc_provider_is_logged_without_url_credentials() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        let secret = "rpc-secret-token";
        let client = EvmClient::with_timeout(
            &format!("http://user:{secret}@{address}/rpc?api_key={secret}"),
            Duration::from_secs(1),
        )
        .unwrap()
        .with_provider("read-test");
        let error = client.latest_head().await.unwrap_err();
        tracing::warn!(%error, "chain read waits after transport error");
        server.abort();
        logs_assert(|lines: &[&str]| {
            if !lines.iter().any(|line| {
                line.contains("chain read waits after transport error")
                    && line.contains("read-test")
            }) {
                return Err("missing redacted transport error".into());
            }
            if lines.iter().any(|line| {
                [" INFO ", " WARN ", " ERROR "]
                    .iter()
                    .any(|level| line.contains(level))
                    && (line.contains(secret) || line.contains("api_key"))
            }) {
                return Err("URL credentials reached production logs".into());
            }
            Ok(())
        });
    }
}
