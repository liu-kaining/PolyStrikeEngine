mod api;
mod config;
mod engine;
mod execution;
mod models;
mod oracle;
mod risk;
mod strategy;

use std::sync::Arc;

use config::AppConfig;
use api::strategy_registry::StrategyRegistry;
use risk::{InventoryManager, RiskGuard};

fn main() -> anyhow::Result<()> {
    // Required by rustls 0.23+ when used via tokio-tungstenite (wss://).
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install rustls default crypto provider");

    let _ = dotenvy::dotenv();
    println!("[PolyStrike Engine] Starting...");

    let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
    rt.block_on(async {
        let inventory = Arc::new(InventoryManager::new());
        let cfg = AppConfig::from_env();
        if let Err(e) = cfg.validate() {
            eprintln!("[Config] Invalid configuration: {}", e);
            return;
        }
        let risk_guard = Arc::new(
            RiskGuard::new(cfg.max_budget)
                .with_max_exposure_per_market(cfg.max_exposure_per_market),
        );
        let strategies = Arc::new(StrategyRegistry::new());

        tokio::spawn(api::server::start_api_server(
            inventory.clone(),
            risk_guard,
            strategies,
            3333,
        ));
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for ctrl_c");
        println!("[System] Shutting down gracefully...");

        let snapshot = inventory.snapshot_all();
        if snapshot.is_empty() {
            println!("[System] No market exposure recorded.");
        } else {
            println!("[System] Final exposure snapshot:");
            for (market_id, exposure) in snapshot {
                let net = exposure.yes_qty - exposure.no_qty;
                println!(
                    "  - {} | yes: {:.4}, no: {:.4}, net: {:.4}, pnl: {:.4}",
                    market_id, exposure.yes_qty, exposure.no_qty, net, exposure.realized_pnl
                );
            }
        }
    });
    Ok(())
}

