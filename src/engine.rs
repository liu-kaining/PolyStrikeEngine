use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::execution::poly_client::PolyClient;
use crate::models::types::BookTicker;
use crate::oracle::binance_ws::spawn_binance_book_ticker_stream;
use crate::oracle::poly_ws;
use crate::risk::{InventoryManager, RiskGuard};
use crate::strategy::SniperStrategy;

/// Runs a single sniper loop for one market. Exits when `cancel_token` is cancelled.
pub async fn run_sniper_task(
    token_id: String,
    strike_price: f64,
    snipe_size: f64,
    inventory: Arc<InventoryManager>,
    cancel_token: CancellationToken,
) {
    let config = AppConfig::from_env();
    let symbol = config.binance_symbol.clone();

    let risk_guard = RiskGuard::new(config.max_budget)
        .with_max_exposure_per_market(config.max_exposure_per_market);

    let poly_client = match PolyClient::new_from_env().await {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("[Engine] {} PolyClient init failed (snipe disabled): {}", token_id, e);
            None
        }
    };

    let sniper_strategy = SniperStrategy {
        strike_price,
        snipe_size,
    };

    println!("[Engine] {} Subscribing to Binance {}@bookTicker...", token_id, symbol);
    let mut binance_rx: watch::Receiver<BookTicker> = match spawn_binance_book_ticker_stream(&symbol).await {
        Ok(rx) => rx,
        Err(e) => {
            eprintln!("[Engine] {} Binance stream failed: {}", token_id, e);
            return;
        }
    };

    println!("[Engine] {} Subscribing to Polymarket orderbook...", &token_id[..token_id.len().min(20)]);
    let mut poly_rx = match poly_ws::spawn_poly_orderbook_stream(&token_id) {
        Ok(rx) => rx,
        Err(e) => {
            eprintln!("[Engine] {} Polymarket stream failed: {}", token_id, e);
            return;
        }
    };

    println!("[Engine] {} Sniper loop running (strike={}, size={})", token_id, strike_price, snipe_size);

    let mut last_price = 0.0_f64;
    let mut first_tick = true;
    let mut wait_duration = Duration::from_secs(5);

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                println!("[Engine] Strategy for {} stopped.", token_id);
                break;
            }
            _ = tokio::time::sleep(wait_duration) => {
                if first_tick {
                    println!("[Engine] {} Still waiting for first Binance tick...", token_id);
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
                        "[Engine] {} Binance {} first tick: mid=${:.2}",
                        token_id, symbol, binance_mid
                    );
                }
                if (binance_mid - last_price).abs() > f64::EPSILON {
                    last_price = binance_mid;
                }
                let poly_book = *poly_rx.borrow();
                if let Some(signal) = sniper_strategy.evaluate(binance_mid, &poly_book) {
                    try_fire(&risk_guard, inventory.as_ref(), poly_client.as_ref(), &token_id, &signal).await;
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
                    try_fire(&risk_guard, inventory.as_ref(), poly_client.as_ref(), &token_id, &signal).await;
                }
            }
        }
    }
}

async fn try_fire(
    risk_guard: &RiskGuard,
    inventory: &InventoryManager,
    poly_client: Option<&PolyClient>,
    token_id: &str,
    signal: &crate::strategy::SnipeSignal,
) {
    if token_id.is_empty() {
        return;
    }
    if let Err(e) = risk_guard.check_and_reserve_budget(signal.target_price, signal.size) {
        println!("[RiskGuard] Blocked before fire: {}", e);
        return;
    }
    println!("[FIRE] Triggering snipe order: {:?}", signal);

    let Some(client) = poly_client else {
        eprintln!("[FIRE] PolyClient not available, skip.");
        return;
    };

    match client
        .execute_snipe_order(token_id, signal.side, signal.target_price, signal.size)
        .await
    {
        Ok(order_id) => {
            println!("[FIRE] Order placed: {}", order_id);
            inventory.add_fill(token_id, true, signal.side, signal.size);
        }
        Err(e) => {
            eprintln!("[FIRE] Order failed: {}", e);
        }
    }
}
