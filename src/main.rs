mod api;
mod config;
mod engine;
mod execution;
mod models;
mod oracle;
mod risk;
mod strategy;

use std::sync::Arc;

use risk::InventoryManager;

fn main() -> anyhow::Result<()> {
    // Required by rustls 0.23+ when used via tokio-tungstenite (wss://).
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install rustls default crypto provider");

    let _ = dotenvy::dotenv();
    println!("[PolyStrike Engine] Starting...");

    let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
    rt.block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
    let inventory = Arc::new(InventoryManager::new());

    // Spawn API server in background
    let api_handle = tokio::spawn(api::server::start_api_server(inventory.clone(), 3333));

    // Spawn engine in background so we can handle shutdown signal
    let engine_handle = tokio::spawn(engine::run(inventory.clone()));

    // ============================================================
    // GRACEFUL SHUTDOWN: Wait for Ctrl+C or engine termination
    // ============================================================
    tokio::select! {
        // Handle Ctrl+C (SIGINT)
        _ = tokio::signal::ctrl_c() => {
            println!("\n");
            println!("╔════════════════════════════════════════════════════════════╗");
            println!("║  🛑 Shutdown signal received (Ctrl+C)                      ║");
            println!("║  Initiating graceful shutdown...                           ║");
            println!("╚════════════════════════════════════════════════════════════╝");
        }
        
        // Handle engine termination (error or completion)
        result = engine_handle => {
            match result {
                Ok(Ok(())) => println!("[System] Engine completed normally."),
                Ok(Err(e)) => eprintln!("[System] Engine error: {}", e),
                Err(e) => eprintln!("[System] Engine task panicked: {}", e),
            }
        }
    }

    // ============================================================
    // P&L SNAPSHOT: Print final positions before exit
    // ============================================================
    print_shutdown_snapshot(&inventory);

    // Abort background tasks
    api_handle.abort();
    
    println!("[System] Shutdown complete. Goodbye!");
    Ok(())
}

/// Print a final P&L snapshot of all market positions.
fn print_shutdown_snapshot(inventory: &InventoryManager) {
    println!("\n");
    println!("╔════════════════════════════════════════════════════════════╗");
    println!("║                    📊 FINAL P&L SNAPSHOT                   ║");
    println!("╠════════════════════════════════════════════════════════════╣");
    
    let snapshot = inventory.snapshot_all();
    
    if snapshot.is_empty() {
        println!("║  No positions recorded.                                    ║");
    } else {
        let mut total_yes: f64 = 0.0;
        let mut total_no: f64 = 0.0;
        let mut total_pnl: f64 = 0.0;
        
        for (market_id, exposure) in snapshot {
            let net = exposure.yes_qty - exposure.no_qty;
            total_yes += exposure.yes_qty;
            total_no += exposure.no_qty;
            total_pnl += exposure.realized_pnl;
            
            println!("║                                                            ║");
            println!("║  Market: {}...", &market_id[..market_id.len().min(24)]);
            println!("║    YES Qty:     {:>10.2}", exposure.yes_qty);
            println!("║    NO Qty:      {:>10.2}", exposure.no_qty);
            println!("║    Net Exposure:{:>10.2}", net);
            println!("║    Realized P&L:${:>10.2}", exposure.realized_pnl);
        }
        
        println!("╠════════════════════════════════════════════════════════════╣");
        println!("║                      AGGREGATE TOTALS                      ║");
        println!("║  Total YES:     {:>12.2}", total_yes);
        println!("║  Total NO:      {:>12.2}", total_no);
        println!("║  Net Position:  {:>12.2}", total_yes - total_no);
        println!("║  Total P&L:     ${:>11.2}", total_pnl);
    }
    
    let global_exposure = inventory.get_global_exposure();
    println!("╠════════════════════════════════════════════════════════════╣");
    println!("║  Global Exposure: ${:>10.2}", global_exposure);
    println!("╚════════════════════════════════════════════════════════════╝");
    println!();
}