use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::config::AppConfig;
use crate::execution::poly_client::PolyClient;
use crate::models::types::BookTicker;
use crate::oracle::binance_ws::spawn_binance_book_ticker_stream;
use crate::oracle::poly_ws::{self, PolyBookTicker};
use crate::risk::{InventoryManager, RiskGuard};
use crate::strategy::SniperStrategy;

pub async fn run(inventory: Arc<InventoryManager>) -> anyhow::Result<()> {
    let config = AppConfig::from_env();
    
    // Validate configuration before starting
    if let Err(e) = config.validate() {
        anyhow::bail!("Invalid configuration: {}", e);
    }
    
    let symbol = config.binance_symbol.clone();
    let dry_run = config.dry_run;

    // SAFETY: Always print dry_run status at startup
    if dry_run {
        println!("\n");
        println!("╔════════════════════════════════════════════════════════════╗");
        println!("║  🔸 DRY RUN MODE ENABLED (PAPER TRADING)                   ║");
        println!("║  No real orders will be placed. Set DRY_RUN=false for live.║");
        println!("╚════════════════════════════════════════════════════════════╝");
        println!();
    } else {
        println!("\n");
        println!("╔════════════════════════════════════════════════════════════╗");
        println!("║  ⚠️  LIVE TRADING MODE - REAL MONEY AT STAKE!              ║");
        println!("║  Ensure you have reviewed all risk parameters.            ║");
        println!("╚════════════════════════════════════════════════════════════╝");
        println!();
    }

    let risk_guard = RiskGuard::new(config.max_budget)
        .with_max_exposure_per_market(config.max_exposure_per_market);

    let poly_client = if dry_run {
        // In dry_run mode, we still initialize the client to verify credentials,
        // but won't use it for actual orders.
        println!("[Sniper Engine] Dry run mode: PolyClient initialization skipped (no real API calls).");
        None
    } else {
        match PolyClient::new_from_env().await {
            Ok(c) => {
                println!("[Sniper Engine] PolyClient initialized successfully.");
                Some(c)
            }
            Err(e) => {
                eprintln!("[Sniper Engine] PolyClient init failed (snipe disabled): {}", e);
                None
            }
        }
    };

    let sniper_strategy = SniperStrategy::default();

    println!("[Sniper Engine] Subscribing to Binance {}@bookTicker...", symbol);
    let mut binance_rx: watch::Receiver<BookTicker> =
        spawn_binance_book_ticker_stream(&symbol).await?;

    let (mut poly_rx, poly_token_id) = match &config.poly_token_id {
        Some(token_id) => {
            println!("[Sniper Engine] Subscribing to Polymarket orderbook for token {}...", &token_id[..token_id.len().min(20)]);
            let rx = poly_ws::spawn_poly_orderbook_stream(token_id)?;
            (Some(rx), token_id.clone())
        }
        None => {
            eprintln!("[Sniper Engine] POLY_TOKEN_ID not set; Polymarket stream disabled.");
            (None, String::new())
        }
    };

    println!("[Sniper Engine] Main loop running, waiting for ticks...");

    let mut last_price = 0.0_f64;
    let mut first_tick = true;
    let mut wait_duration = Duration::from_secs(5);

    loop {
        if let Some(ref mut poly_rx) = poly_rx {
            tokio::select! {
                _ = tokio::time::sleep(wait_duration) => {
                    if first_tick {
                        println!("[Sniper Engine] Still waiting for first Binance tick...");
                    }
                    wait_duration = Duration::from_secs(5);
                }
                changed = binance_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let ticker = *binance_rx.borrow();
                    let binance_mid = (ticker.bid_price + ticker.ask_price) * 0.5;
                    if first_tick {
                        first_tick = false;
                        println!(
                            "[Sniper Engine] Binance {} connected, first tick: mid=${:.2} (bid={:.2}, ask={:.2})",
                            symbol, binance_mid, ticker.bid_price, ticker.ask_price
                        );
                    }
                    if (binance_mid - last_price).abs() > f64::EPSILON {
                        last_price = binance_mid;
                        println!(
                            "[Sniper Engine] Binance {} price moved to ${:.2}, evaluating snipe condition...",
                            symbol, binance_mid
                        );
                    }
                    let poly_book = *poly_rx.borrow();
                    if let Some(signal) = sniper_strategy.evaluate(binance_mid, &poly_book) {
                        try_fire(&risk_guard, inventory.as_ref(), poly_client.as_ref(), &poly_token_id, &signal, dry_run).await;
                    }
                }
                changed = poly_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let ticker = *binance_rx.borrow();
                    let binance_mid = (ticker.bid_price + ticker.ask_price) * 0.5;
                    let poly_book = *poly_rx.borrow();
                    if let Some(signal) = sniper_strategy.evaluate(binance_mid, &poly_book) {
                        try_fire(&risk_guard, inventory.as_ref(), poly_client.as_ref(), &poly_token_id, &signal, dry_run).await;
                    }
                }
            }
        } else {
            tokio::select! {
                _ = tokio::time::sleep(wait_duration) => {
                    if first_tick {
                        println!("[Sniper Engine] Still waiting for first Binance tick...");
                    }
                    wait_duration = Duration::from_secs(5);
                }
                changed = binance_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let ticker = *binance_rx.borrow();
                    let binance_mid = (ticker.bid_price + ticker.ask_price) * 0.5;
                    if first_tick {
                        first_tick = false;
                        println!(
                            "[Sniper Engine] Binance {} connected, first tick: mid=${:.2} (bid={:.2}, ask={:.2})",
                            symbol, binance_mid, ticker.bid_price, ticker.ask_price
                        );
                    }
                    if (binance_mid - last_price).abs() > f64::EPSILON {
                        last_price = binance_mid;
                        println!(
                            "[Sniper Engine] Binance {} price moved to ${:.2}, evaluating snipe condition...",
                            symbol, binance_mid
                        );
                    }
                    let poly_book = PolyBookTicker::default();
                    if let Some(signal) = sniper_strategy.evaluate(binance_mid, &poly_book) {
                        try_fire(&risk_guard, inventory.as_ref(), poly_client.as_ref(), &poly_token_id, &signal, dry_run).await;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Execute or simulate a snipe order.
/// 
/// # Arguments
/// * `dry_run` - If true, only simulate the order (paper trading), no real API calls.
/// 
/// # Budget Management
/// - Budget is reserved BEFORE any order attempt
/// - Budget is RELEASED if order fails or in dry_run mode
/// - Budget is consumed only on successful live order execution
async fn try_fire(
    risk_guard: &RiskGuard,
    inventory: &InventoryManager,
    poly_client: Option<&PolyClient>,
    token_id: &str,
    signal: &crate::strategy::SnipeSignal,
    dry_run: bool,
) {
    if token_id.is_empty() {
        return;
    }
    
    // CRITICAL: Reserve budget atomically. Must release on failure!
    let reservation = match risk_guard.reserve_budget(signal.target_price, signal.size) {
        Ok(r) => r,
        Err(e) => {
            println!("[RiskGuard] Blocked before fire: {}", e);
            return;
        }
    };

    // ============================================================
    // DRY RUN MODE: Simulate order without real API calls
    // ============================================================
    if dry_run {
        // Simulate successful order for testing downstream logic
        println!(
            "\n═══════════════════════════════════════════════════════════");
        println!("🔸 [DRY RUN] Would have executed order:");
        println!("   Side:     {:?}", signal.side);
        println!("   Price:    ${:.4}", signal.target_price);
        println!("   Size:     {:.2}", signal.size);
        println!("   Notional: ${:.2}", signal.target_price * signal.size);
        println!("   Token:    {}...", &token_id[..token_id.len().min(20)]);
        println!("═══════════════════════════════════════════════════════════\n");
        
        // Simulate fill for inventory tracking (paper trading)
        inventory.add_fill(token_id, true, signal.side, signal.size);
        
        // Release budget since no real money was spent in dry_run mode
        risk_guard.release_budget(reservation);
        return;
    }

    // ============================================================
    // LIVE MODE: Execute real order
    // ============================================================
    println!("[FIRE] Triggering snipe order: {:?}", signal);

    let Some(client) = poly_client else {
        eprintln!("[FIRE] PolyClient not available, releasing budget.");
        risk_guard.release_budget(reservation);
        return;
    };

    match client
        .execute_snipe_order(token_id, signal.side, signal.target_price, signal.size)
        .await
    {
        Ok(order_id) => {
            println!("[FIRE] ✅ Order placed: {}", order_id);
            inventory.add_fill(token_id, true, signal.side, signal.size);
            // Budget consumed successfully, reservation dropped without release
        }
        Err(e) => {
            eprintln!("[FIRE] ❌ Order failed: {}, releasing budget", e);
            // CRITICAL: Release budget on failure to prevent budget leakage!
            risk_guard.release_budget(reservation);
        }
    }
}