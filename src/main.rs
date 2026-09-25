//! Binary entry point: wires the store, worker, and HTTP API together and
//! shuts everything down cleanly on Ctrl-C / SIGTERM.

use std::process::ExitCode;

use claude_job_runner::claude::CliRunner;
use claude_job_runner::config::Config;
use claude_job_runner::http;
use claude_job_runner::store::JobStore;
use claude_job_runner::worker::Worker;
use claude_job_runner::workspace::WorkspaceRoot;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // A missing .env is fine; a malformed one is not worth dying over either.
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!(error = %err, "fatal");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    if let Some(path) = &config.claude.jobctl_mcp {
        info!(path = %path.display(), "jobs get the jobctl MCP server");
    } else {
        info!("jobctl-mcp not found; jobs run without MCP tools");
    }

    let store = JobStore::connect(&config.database_url, config.db_max_connections).await?;
    if config.requeue_pending_on_start {
        let requeued = store.requeue_pending().await?;
        if requeued > 0 {
            info!(requeued, "reset orphaned pending jobs");
        }
    }

    let root = WorkspaceRoot::prepare(&config.workspace_root).await?;
    let swept = root.sweep().await?;
    if swept > 0 {
        info!(swept, "removed stale workspaces");
    }

    let shutdown = CancellationToken::new();
    let worker = Worker::new(
        store.clone(),
        CliRunner::new(config.claude.clone()),
        root,
        config.poll_interval,
        config.max_concurrent_jobs,
    );
    let worker_task = tokio::spawn({
        let shutdown = shutdown.clone();
        async move { worker.run(shutdown).await }
    });
    let listener = http::bind(config.http_addr).await?;
    let http_task = tokio::spawn(http::serve(store, listener, shutdown.clone()));

    wait_for_signal().await;
    info!("shutdown requested");
    shutdown.cancel();

    worker_task.await?;
    http_task.await??;
    Ok(())
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(term) => term,
            Err(err) => {
                error!(error = %err, "could not listen for SIGTERM; only Ctrl-C will stop the runner");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
