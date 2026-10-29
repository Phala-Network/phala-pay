//! Real tracing-to-database coverage, independent of Sentry configuration.
mod support;

use anyhow::{Context, Result, ensure};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use topup::alerts::AlertRecorder;

#[tokio::test]
async fn shared_tracing_recorder_captures_legacy_and_business_alerts_without_sentry() -> Result<()>
{
    support::with_database(|database| {
        Box::pin(async move {
            ensure!(sentry::Hub::current().client().is_none());
            let recorder = AlertRecorder::initialize(database.app_pool.clone()).await?;
            let cancel = CancellationToken::new();
            let worker_cancel = cancel.clone();
            let worker = tokio::spawn(recorder.run(worker_cancel));
            let result = async {
                tracing::subscriber::with_default(
                    topup::observability::log_subscriber(std::io::sink),
                    || {
                        tracing::warn!(
                            tags.alert = "TopupRpcDisagreement",
                            tags.chain_id = 1,
                            "existing legacy alert"
                        );
                        topup::observability::emit_alert(
                            "TopupHeartbeatStale",
                            "pipeline",
                            "warning",
                            180,
                            180,
                        );
                        topup::observability::emit_alert(
                            "TopupHeartbeatStale",
                            "pipeline",
                            "critical",
                            240,
                            180,
                        );
                    },
                );
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let rows: Vec<(String, String, String)> = sqlx::query_as(
                            "SELECT name,severity,runbook FROM alerts ORDER BY name",
                        )
                        .fetch_all(&database.app_pool)
                        .await?;
                        if rows.len() == 2 && rows[0].1 == "critical" {
                            ensure!(
                                rows[0].0 == "TopupHeartbeatStale"
                                    && rows[0].2.ends_with("business-health.md")
                            );
                            ensure!(
                                rows[1].0 == "TopupRpcDisagreement"
                                    && rows[1].2.ends_with("provider-disagreement.md")
                            );
                            break Ok::<_, anyhow::Error>(());
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .context("alerts were not persisted")??;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            cancel.cancel();
            worker.await.context("alert worker stopped")?;
            result
        })
    })
    .await
}
