//! Polymarket WebSocket orderbook stream; publishes best bid/ask via watch.

use futures_util::StreamExt;
use polymarket_client_sdk::clob::ws::{Client as WsClient, types::response::BookUpdate};
use tokio::sync::watch;

/// Best bid/ask snapshot for a single token (from Polymarket CLOB).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PolyBookTicker {
    pub best_bid: f64,
    pub best_ask: f64,
}

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
pub fn spawn_poly_orderbook_stream(
    token_id: &str,
) -> anyhow::Result<watch::Receiver<PolyBookTicker>> {
    let (tx, rx) = watch::channel(PolyBookTicker::default());
    let token_id = token_id.to_string();

    tokio::spawn(async move {
        let client = WsClient::default();
        let mut stream = match client.subscribe_orderbook(vec![token_id.clone()]) {
            Ok(s) => Box::pin(s),
            Err(_) => return,
        };
        while let Some(result) = stream.next().await {
            match result {
                Ok(book) => {
                    let ticker = book_update_to_ticker(&book);
                    let _ = tx.send(ticker);
                }
                Err(_) => break,
            }
        }
    });

    Ok(rx)
}
