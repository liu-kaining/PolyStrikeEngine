use std::time::Duration;

use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::watch;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

use crate::models::types::BookTicker;

/// Raw Binance `@bookTicker` payload for a single symbol.
/// Note: combined stream (e.g. btcusdt@bookTicker) may omit "E" (event_time).
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

/// Spawns a dedicated task that maintains a low-latency Binance `@bookTicker` stream.
///
/// Returns a `watch::Receiver<BookTicker>` that always holds the latest tick.
/// Subscribers get notified only when a new, valid tick is received.
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

        loop {
            match connect_async(&url).await {
                Ok((ws_stream, _)) => {
                    eprintln!("[Binance WS] Connected: {}", url);
                    let (_write, mut read) = ws_stream.split();
                    backoff_ms = 500;

                    let mut first_msg = true;
                    let mut parse_fail_logged = 0u32;
                    while let Some(msg_result) = read.next().await {
                        let msg = match msg_result {
                            Ok(m) => m,
                            Err(_) => break,
                        };

                        let Message::Text(text) = msg else {
                            continue;
                        };

                        if first_msg {
                            first_msg = false;
                            let preview = text.chars().take(120).collect::<String>();
                            eprintln!("[Binance WS] First raw message: {}...", preview);
                        }

                        let parsed: BinanceBookTicker = match serde_json::from_str(&text) {
                            Ok(v) => v,
                            Err(_) => {
                                if parse_fail_logged < 3 && text.len() < 300 {
                                    eprintln!("[Binance WS] Parse skip (not bookTicker?): {}...", &text[..text.len().min(150)]);
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
                }
                Err(e) => {
                    eprintln!("[Binance WS] Disconnected or error: {:?}, reconnecting in {}ms", e, backoff_ms);
                }
            }

            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(10_000);
        }
    });

    Ok(rx)
}

