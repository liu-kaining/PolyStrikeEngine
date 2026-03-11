//! Binance WebSocket book ticker stream with robust reconnection logic.
//! On max retries exceeded, drops the watch::Sender to signal downstream consumers
//! instead of panicking the process.

use std::time::Duration;

use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::watch;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{error, info};

use crate::models::types::BookTicker;

#[derive(Debug, Deserialize)]
struct BinanceBookTicker {
    #[serde(rename = "u")]
    #[allow(dead_code)]
    update_id: u64,
    #[serde(rename = "E", default)]
    event_time: u64,
    #[serde(rename = "b")]
    bid_price: String,
    #[serde(rename = "B")]
    bid_qty: String,
    #[serde(rename = "a")]
    ask_price: String,
    #[serde(rename = "A")]
    ask_qty: String,
}

impl TryFrom<BinanceBookTicker> for BookTicker {
    type Error = ();

    fn try_from(raw: BinanceBookTicker) -> Result<Self, Self::Error> {
        let bid_price = raw.bid_price.parse::<f64>().map_err(|_| ())?;
        let bid_qty = raw.bid_qty.parse::<f64>().map_err(|_| ())?;
        let ask_price = raw.ask_price.parse::<f64>().map_err(|_| ())?;
        let ask_qty = raw.ask_qty.parse::<f64>().map_err(|_| ())?;

        Ok(BookTicker {
            bid_price,
            bid_qty,
            ask_price,
            ask_qty,
            event_time_ms: raw.event_time,
        })
    }
}

const MAX_RETRIES: u32 = 10;
const MAX_BACKOFF_MS: u64 = 30_000;

/// Spawns a dedicated task that maintains a low-latency Binance `@bookTicker` stream.
///
/// Returns a `watch::Receiver<BookTicker>` that always holds the latest tick.
/// If MAX_RETRIES consecutive connection failures occur, the task exits gracefully
/// (dropping the Sender, which causes all Receivers to get `Err` on `changed()`).
pub async fn spawn_binance_book_ticker_stream(
    symbol: &str,
) -> anyhow::Result<watch::Receiver<BookTicker>> {
    let (tx, rx) = watch::channel(BookTicker::default());

    let symbol = symbol.to_lowercase();
    let url = format!(
        "wss://stream.binance.com:9443/ws/{}@bookTicker",
        symbol
    );

    tokio::spawn(async move {
        let mut backoff_ms = 500u64;
        let mut consecutive_failures: u32 = 0;

        loop {
            match connect_async(&url).await {
                Ok((ws_stream, _)) => {
                    let (_write, mut read) = ws_stream.split();

                    backoff_ms = 500;
                    consecutive_failures = 0;
                    info!("[Binance WS] Reconnected successfully");

                    let mut parse_fail_logged = 0u32;

                    while let Some(msg_result) = read.next().await {
                        let msg = match msg_result {
                            Ok(m) => m,
                            Err(e) => {
                                error!("[Binance WS] Stream error: {:?}", e);
                                break;
                            }
                        };

                        let Message::Text(text) = msg else {
                            continue;
                        };

                        let parsed: BinanceBookTicker = match serde_json::from_str(&text) {
                            Ok(v) => v,
                            Err(_) => {
                                if parse_fail_logged < 3 && text.len() < 300 {
                                    error!(
                                        "[Binance WS] Parse skip (not bookTicker?): {}...",
                                        &text[..text.len().min(150)]
                                    );
                                    parse_fail_logged += 1;
                                }
                                continue;
                            }
                        };

                        let ticker = match BookTicker::try_from(parsed) {
                            Ok(t) => t,
                            Err(_) => continue,
                        };

                        let _ = tx.send(ticker);
                    }

                    error!("[Binance WS] Stream ended, reconnecting...");
                }
                Err(e) => {
                    consecutive_failures += 1;
                    error!(
                        "[Binance WS] Connection failed ({}/{}): {:?}, retrying in {}ms",
                        consecutive_failures, MAX_RETRIES, e, backoff_ms
                    );

                    if consecutive_failures >= MAX_RETRIES {
                        error!(
                            "[Binance WS] CRITICAL: Max retries ({}) exceeded. \
                             Dropping sender to signal downstream tasks.",
                            MAX_RETRIES
                        );
                        // Drop tx by returning — all receivers will see Err on changed()
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
