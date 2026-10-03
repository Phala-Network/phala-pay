//! The CLI preserves sanitized member failures on stderr, leaving stdout for JSON.
use anyhow::{Context, Result, ensure};
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use std::future::IntoFuture;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn rpc_check_reports_every_wrong_chain_member_without_retry_or_secrets() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let server = tokio::spawn(
        axum::serve(
            listener,
            Router::new().route(
                "/secret-key",
                post(move |Json(request): Json<Value>| {
                    let count = count.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        Json(json!({"jsonrpc":"2.0","id":request["id"],"result":"0x1"}))
                    }
                }),
            ),
        )
        .into_future(),
    );
    let result = async {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../deploy/environments/phala-network/staging/topup/topup.yaml");
        let mut config: Value = serde_json::from_str(
            &topup::config::Config::load(&path)
                .map_err(anyhow::Error::msg)?
                .resolved_json()
                .map_err(anyhow::Error::msg)?,
        )?;
        config["rpc_companies"] =
            json!({"local-a":{"domains":["localhost"]},"local-b":{"domains":["127.0.0.1"]}});
        for (id, group) in config["rpc_groups"].as_object_mut().context("groups")? {
            let a = id.ends_with('a');
            let host = if a { "localhost" } else { "127.0.0.1" };
            let member = group["members"][0].clone();
            group["members"] = json!([member]);
            group["members"][0]["company"] = json!(if a { "local-a" } else { "local-b" });
            group["members"][0]["url"] = json!(format!("http://{host}:{port}/secret-key"));
        }
        let encoded = serde_json::to_vec(&config)?;
        let output = tokio::task::spawn_blocking(move || -> Result<_> {
            use std::io::Write;
            let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_topup"))
                .args(["rpc", "check", "--config", "/dev/stdin"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()?;
            child.stdin.take().context("stdin")?.write_all(&encoded)?;
            Ok(child.wait_with_output()?)
        })
        .await??;
        ensure!(!output.status.success());
        ensure!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr)?;
        for member in [
            "base-sepolia-a",
            "base-sepolia-b",
            "provider-a",
            "provider-b",
        ] {
            ensure!(
                stderr.contains(&format!(
                    "{member}: chain id: RPC member identity invalid (mismatch)"
                )),
                "{stderr}"
            );
        }
        ensure!(
            !stderr.contains("secret-key") && !stderr.contains("http://"),
            "{stderr}"
        );
        ensure!(
            calls.load(Ordering::SeqCst) == 4,
            "wrong chain must never retry"
        );
        Ok(())
    }
    .await;
    server.abort();
    result
}
