//! Polymarket WebSocket orderbook stream with robust reconnection logic.
//! On max retries exceeded, drops the watch::Sender to signal downstream consumers
//! instead of panicking the process.

use std::time::Duration;

use futures_util::StreamExt;
use polymarket_client_sdk::clob::ws::{
    types::response::BookUpdate,
    Client as WsClient,
};
use rust_decimal::prelude::ToPrimitive;
use tokio::sync::watch;
use tracing::error;

/// Best bid/ask snapshot for a single token (from Polymarket CLOB).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PolyBookTicker {
    pub best_bid: f64,
    pub best_ask: f64,
}

const MAX_RETRIES: u32 = 10;
const INITIAL_BACKOFF_MS: u64 = 1000;
const MAX_BACKOFF_MS: u64 = 30_000;

#[inline]
fn decimal_to_f64(d: &polymarket_client_sdk::types::Decimal) -> f64 {
    d.to_f64().unwrap_or(0.0)
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
/// If MAX_RETRIES consecutive connection failures occur, the task exits gracefully
/// (dropping the Sender, which causes all Receivers to get `Err` on `changed()`).
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
                                error!("[Poly WS] Stream error: {:?}, reconnecting...", e);
                                break;
                            }
                        }
                    }

                    error!("[Poly WS] Stream ended, reconnecting...");
                }
                Err(e) => {
                    consecutive_failures += 1;
                    error!(
                        "[Poly WS] Connection failed ({}/{}): {:?}, retrying in {}ms",
                        consecutive_failures, MAX_RETRIES, e, backoff_ms
                    );

                    if consecutive_failures >= MAX_RETRIES {
                        error!(
                            "[Poly WS] CRITICAL: Max retries ({}) exceeded for token {}. \
                             Dropping sender to signal downstream task.",
                            MAX_RETRIES,
                            &token_id[..token_id.len().min(20)]
                        );
                        return;
                    }
                }
            }

            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(MAX_BACKOFF_MS);
        }
    });

    Ok(rx)
}
