mod api;
mod config;
mod discovery;
mod engine;
mod execution;
mod models;
mod oracle;
mod risk;
mod strategy;

use std::sync::Arc;
use std::time::Duration;

use api::strategy_registry::StrategyRegistry;
use config::AppConfig;
use execution::poly_client::PolyClient;
use models::btc_price;
use oracle::binance_ws::spawn_binance_book_ticker_stream;
use risk::{InventoryManager, RiskGuard};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_appender::rolling;
use tracing_subscriber::{
    fmt,
    layer::SubscriberExt,
    util::SubscriberInitExt,
    EnvFilter,
    Layer,
};

fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install rustls default crypto provider");

    let _ = dotenvy::dotenv();

    let file_appender = rolling::daily("logs", "engine.log");
    let (file_writer, _file_guard) = tracing_appender::non_blocking(file_appender);
    let (stdout_writer, _stdout_guard) = tracing_appender::non_blocking(std::io::stdout());

    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let console_layer = fmt::layer()
        .with_writer(stdout_writer)
        .with_ansi(true)
        .with_target(false)
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_filter(env_filter);

    let file_layer = fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false)
        .with_target(true)
        .with_filter(EnvFilter::new("polystrike_engine=info"));

    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .init();

    info!("[PolyStrike Engine] Starting...");

    let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
    rt.block_on(async {
        let cfg = AppConfig::from_env();
        if let Err(e) = cfg.validate() {
            error!(error = %e, "[Config] Invalid configuration");
            return;
        }

        let inventory = Arc::new(InventoryManager::new());
        let risk_guard = Arc::new(
            RiskGuard::new(cfg.max_budget)
                .with_max_exposure_per_market(cfg.max_exposure_per_market),
        );
        let strategies = Arc::new(StrategyRegistry::new());
        let strategies_for_status = strategies.clone();
        let global_cancel = CancellationToken::new();

        // -- Singleton PolyClient (one auth, one rate limiter, shared by all tasks) --
        let poly_client: Option<Arc<PolyClient>> = if !cfg.dry_run {
            match PolyClient::new_from_env().await {
                Ok(c) => {
                    info!("[System] PolyClient authenticated (singleton).");
                    Some(Arc::new(c))
                }
                Err(e) => {
                    error!("[System] PolyClient init failed (snipe disabled): {}", e);
                    None
                }
            }
        } else {
            info!("[System] DRY_RUN=true — PolyClient not initialized.");
            None
        };

        // -- Singleton Binance WS (one connection, cloned receiver for all tasks) --
        let binance_rx = match spawn_binance_book_ticker_stream(&cfg.binance_symbol).await {
            Ok(rx) => {
                info!("[System] Binance WS connected (singleton for {}).", cfg.binance_symbol);
                rx
            }
            Err(e) => {
                error!("[System] CRITICAL: Binance WS failed to start: {}. Aborting.", e);
                return;
            }
        };

        // -- API server (carries singletons via ApiState) --
        let inventory_for_shutdown = inventory.clone();
        tokio::spawn(api::server::start_api_server(
            inventory.clone(),
            risk_guard.clone(),
            strategies.clone(),
            poly_client.clone(),
            binance_rx.clone(),
            cfg.dry_run,
            global_cancel.clone(),
            3333,
        ));

        // -- 30s heartbeat --
        let heartbeat_binance_rx = binance_rx.clone();
        tokio::spawn(async move {
            let rx = heartbeat_binance_rx;
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let ticker = *rx.borrow();
                let btc_mid = (ticker.bid_price + ticker.ask_price) * 0.5;
                if btc_mid > 0.0 {
                    btc_price::set_btc_mid(btc_mid);
                }
                let markets = strategies_for_status.len();
                info!(
                    "[System] Radar active. Monitoring {} markets. Last BTC: {:.1}",
                    markets, btc_mid
                );
            }
        });

        // -- Wait for shutdown signal (SIGINT or SIGTERM) --
        wait_for_shutdown_signal().await;

        info!("[System] Shutting down gracefully — cancelling all strategies...");
        global_cancel.cancel();

        // Give tasks time to cancel open orders and exit
        tokio::time::sleep(Duration::from_secs(3)).await;

        let snapshot = inventory_for_shutdown.snapshot_all();
        if snapshot.is_empty() {
            info!("[System] No market exposure recorded.");
        } else {
            info!("[System] Final exposure snapshot:");
            for (market_id, exposure) in snapshot {
                let net = exposure.yes_qty - exposure.no_qty;
                info!(
                    "Market {} | yes: {:.4}, no: {:.4}, net: {:.4}, pnl: {:.4}",
                    market_id, exposure.yes_qty, exposure.no_qty, net, exposure.realized_pnl
                );
            }
        }
        info!("[System] Shutdown complete.");
    });
    Ok(())
}

/// Wait for either Ctrl+C (SIGINT) or SIGTERM.
async fn wait_for_shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for ctrl_c");
    };

    #[cfg(unix)]
    let sigterm = async {
        use tokio::signal::unix::{signal, SignalKind};
        let mut stream = signal(SignalKind::terminate()).expect("failed to listen for SIGTERM");
        stream.recv().await;
    };

    #[cfg(not(unix))]
    let sigterm = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => { info!("[System] Received SIGINT (Ctrl+C)."); }
        _ = sigterm => { info!("[System] Received SIGTERM."); }
    }
}
