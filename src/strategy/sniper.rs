//! Sniper strategy: compare Binance (oracle) price to Polymarket book and emit fire signals.

use crate::models::types::OrderSide;
use crate::oracle::poly_ws::PolyBookTicker;

/// Signal to execute a snipe order (side, price, size).
#[derive(Debug, Clone)]
pub struct SnipeSignal {
    pub side: OrderSide,
    pub target_price: f64,
    pub size: f64,
}

/// Placeholder strategy: when Binance price exceeds target, look for cheap YES on Polymarket.
pub struct SniperStrategy {
    /// BTC price above which we consider YES to be near 1.0 (e.g. 100_000.0).
    pub target_btc_price: f64,
    /// Max ask price we're willing to pay for YES when condition holds (e.g. 0.90).
    pub max_ask_for_yes_snipe: f64,
    /// Size to fire (e.g. 100.0).
    pub snipe_size: f64,
}

impl Default for SniperStrategy {
    fn default() -> Self {
        Self {
            target_btc_price: 100_000.0,
            max_ask_for_yes_snipe: 0.90,
            snipe_size: 100.0,
        }
    }
}

impl SniperStrategy {
    /// Evaluate: if binance_price > target_btc_price and poly best_ask < threshold, return Buy YES signal.
    pub fn evaluate(&self, binance_price: f64, poly_book: &PolyBookTicker) -> Option<SnipeSignal> {
        if binance_price <= self.target_btc_price {
            return None;
        }
        if poly_book.best_ask >= self.max_ask_for_yes_snipe || poly_book.best_ask <= 0.0 {
            return None;
        }
        Some(SnipeSignal {
            side: OrderSide::Buy,
            target_price: poly_book.best_ask,
            size: self.snipe_size,
        })
    }
}
