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
    /// Strike price for the event market (e.g. BTC >= 72_000 at expiry).
    pub strike_price: f64,
    /// Size to fire (e.g. 100.0).
    pub snipe_size: f64,
}

impl Default for SniperStrategy {
    fn default() -> Self {
        Self {
            strike_price: 72_000.0,
            snipe_size: 100.0,
        }
    }
}

impl SniperStrategy {
    /// Simplified probability oracle:
    /// map distance to strike into [0, 1] using a sigmoid.
    fn calculate_fair_value(binance_price: f64, strike_price: f64) -> f64 {
        if strike_price <= 0.0 {
            return 0.5;
        }
        let x = (binance_price - strike_price) / (strike_price * 0.02); // +/-2% scale
        1.0 / (1.0 + (-x).exp())
    }

    /// Evaluate current edge. If fair_value - best_ask > 0.05, fire Buy YES.
    pub fn evaluate(&self, binance_price: f64, poly_book: &PolyBookTicker) -> Option<SnipeSignal> {
        if poly_book.best_ask <= 0.0 {
            return None;
        }
        let fair_value = Self::calculate_fair_value(binance_price, self.strike_price);
        if fair_value - poly_book.best_ask <= 0.05 {
            return None;
        }
        Some(SnipeSignal {
            side: OrderSide::Buy,
            target_price: poly_book.best_ask,
            size: self.snipe_size,
        })
    }
}
