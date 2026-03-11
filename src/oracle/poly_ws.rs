//! Polymarket WebSocket orderbook stream with robust reconnection logic.

use std::time::Duration;

use futures_util::StreamExt;
use polymarket_client_sdk::clob::ws::{Client as WsClient, types::response::BookUpdate};
use tokio::sync::watch;

/// Best bid/ask snapshot for a single token (from Polymarket CLOB).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PolyBookTicker {
    pub best_bid: f64,
    pub best_ask: f64,
}

/// Maximum number of consecutive connection failures before giving up.
const MAX_RETRIES: u32 = 10;
/// Backoff duration between retries.
const INITIAL_BACKOFF_MS: u64 = 1000;
const MAX_BACKOFF_MS: u64 = 30_000;

fn decimal_to_f64(d: &polymarket_client_sdk::types::Decimal) -> f64 {
    d.to_string().parse::<f64>().unwrap_or(0.0)
}

fn book_update_to_ticker(book: &BookUpdate) -> PolyBookTicker {
    let best_bid = book
        .bids
        .first()
        .map(|l| decimal_to_f64(&l.price))
        .unwrap_or(0.0);
    let best_ask = book
        .asks
        .first()
        .map(|l| decimal_to_f64(&l.price))
        .unwrap_or(0.0);
    PolyBookTicker { best_bid, best_ask }
}

/// Spawns a task that subscribes to Polymarket orderbook for `token_id` and sends
/// the latest best bid/ask via a watch channel.
///
/// # Panics
/// Panics after `MAX_RETRIES` consecutive connection failures to alert the main engine.
pub fn spawn_poly_orderbook_stream(
    token_id: &str,
) -> anyhow::Result<watch::Receiver<PolyBookTicker>> {
    let (tx, rx) = watch::channel(PolyBookTicker::default());
    let token_id = token_id.to_string();

    tokio::spawn(async move {
        let mut backoff_ms = INITIAL_BACKOFF_MS;
        let mut consecutive_failures: u32 = 0;

        loop {
            let client = WsClient::default();
            
            match client.subscribe_orderbook(vec![token_id.clone()]) {
                Ok(stream) => {
                    eprintln!("[Poly WS] Connected to orderbook for token {}...", &token_id[..token_id.len().min(12)]);
                    
                    // Reset backoff and failure counter on successful connection
                    backoff_ms = INITIAL_BACKOFF_MS;
                    consecutive_failures = 0;
                    
                    let mut stream = Box::pin(stream);
                    
                    while let Some(result) = stream.next().await {
                        match result {
                            Ok(book) => {
                                let ticker = book_update_to_ticker(&book);
                                let _ = tx.send(ticker);
                            }
                            Err(e) => {
                                eprintln!("[Poly WS] Stream error: {:?}, reconnecting...", e);
                                break;
                            }
                        }
                    }
                    
                    // Stream ended, will retry below
                    eprintln!("[Poly WS] Stream ended, reconnecting...");
                }
                Err(e) => {
                    consecutive_failures += 1;
                    eprintln!(
                        "[Poly WS] Connection failed ({}/{}): {:?}, retrying in {}ms",
                        consecutive_failures, MAX_RETRIES, e, backoff_ms
                    );
                    
                    // CRITICAL: Panic after max retries to alert main engine
                    if consecutive_failures >= MAX_RETRIES {
                        panic!(
                            "[Poly WS] CRITICAL: Max retries ({}) exceeded for token {}. \
                             Check Polymarket API status or token validity!",
                            MAX_RETRIES, &token_id[..token_id.len().min(20)]
                        );
                    }
                }
            }

            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(MAX_BACKOFF_MS);
        }
    });

    Ok(rx)
}