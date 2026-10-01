mod api;
mod config;
mod json_stream;
mod model;
mod nbt;
mod persist;
mod state;
mod sync;
mod upstream;

use std::{env, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "skyblock_price_service=info".into()),
        )
        .init();
    let config = config::Config::load()?;
    let token = env::var("PRICE_API_TOKEN").context("PRICE_API_TOKEN must be set")?;
    ensure!(
        (24..=256).contains(&token.len())
            && token.is_ascii()
            && !token.chars().any(char::is_whitespace),
        "PRICE_API_TOKEN must be 24 to 256 ASCII characters without whitespace"
    );
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    let state = Arc::new(state::State::new(config, token));
    let worker_state = state.clone();
    let (worker_error_tx, mut worker_error_rx) = tokio::sync::oneshot::channel();
    let worker = std::thread::Builder::new()
        .name("skyblock-sync".into())
        .spawn(move || {
            let result = sync::run(worker_state.clone());
            if let Err(error) = &result {
                error!(error = %error, "sync worker stopped");
                worker_state.stop();
            }
            let _ = worker_error_tx.send(());
            result
        })?;
    let shutdown_state = state.clone();
    let shutdown = async move {
        tokio::select! { _ = shutdown_signal() => {}, _ = &mut worker_error_rx => {} }
        shutdown_state.stop();
        info!("shutdown requested");
    };
    info!(bind = %state.config.bind, "price API listening");
    let served = axum::serve(listener, api::router(state.clone()))
        .with_graceful_shutdown(shutdown)
        .await;
    state.stop();
    let joined = tokio::time::timeout(
        Duration::from_secs(state.config.request_timeout_secs + 5),
        tokio::task::spawn_blocking(move || worker.join()),
    )
    .await;
    served?;
    match joined {
        Ok(Ok(Ok(result))) => result,
        Ok(Ok(Err(_))) => anyhow::bail!("sync worker panicked"),
        Ok(Err(error)) => Err(error.into()),
        Err(_) => anyhow::bail!("sync worker shutdown deadline exceeded"),
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("cannot install SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
