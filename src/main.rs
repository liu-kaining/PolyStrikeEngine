mod api;
mod config;
mod engine;
mod execution;
mod models;
mod oracle;
mod risk;
mod strategy;

use std::sync::Arc;

use api::strategy_registry::StrategyRegistry;
use risk::InventoryManager;

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
        let strategies = Arc::new(StrategyRegistry::new());

        tokio::spawn(api::server::start_api_server(
            inventory.clone(),
            strategies,
            3333,
        ));
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for ctrl_c");
        println!("[PolyStrike Engine] Shutting down...");
    });
    Ok(())
}

