//! Sniper strategy: compare Binance (oracle) price to Polymarket book and emit fire signals.

use std::sync::OnceLock;

use statrs::distribution::{ContinuousCDF, Normal};

use crate::models::types::OrderSide;

/// Cached standard normal distribution N(0,1) — constructed once, used on every tick.
fn std_normal() -> &'static Normal {
    static INSTANCE: OnceLock<Normal> = OnceLock::new();
    INSTANCE.get_or_init(|| Normal::new(0.0, 1.0).unwrap())
}

/// Signal to execute a snipe order (side, price, size).
#[derive(Debug, Clone)]
pub struct SnipeSignal {
    pub side: OrderSide,
    pub target_price: f64,
    pub size: f64,
}

pub struct SniperStrategy {
    pub strike_price: f64,
    pub snipe_size: f64,
    pub expiry_timestamp: i64,
    pub volatility: f64,
}

impl Default for SniperStrategy {
    fn default() -> Self {
        Self {
            strike_price: 72_000.0,
            snipe_size: 100.0,
            expiry_timestamp: 0,
            volatility: 0.5,
        }
    }
}

impl SniperStrategy {
    pub fn new(
        strike_price: f64,
        snipe_size: f64,
        expiry_timestamp: i64,
        volatility: f64,
    ) -> Self {
        Self {
            strike_price,
            snipe_size,
            expiry_timestamp,
            volatility,
        }
    }

    /// Black-Scholes-style fair probability for a European digital call (binary YES option).
    /// Uses a cached N(0,1) distribution — zero allocation on the hot path.
    pub(crate) fn calculate_binary_call_price(
        spot: f64,
        strike: f64,
        time_to_expiry_years: f64,
        volatility: f64,
    ) -> f64 {
        if strike <= 0.0 || volatility <= 0.0 {
            return if spot >= strike { 1.0 } else { 0.0 };
        }
        if time_to_expiry_years <= 0.0 {
            return if spot >= strike { 1.0 } else { 0.0 };
        }
        let d2 = ((spot / strike).ln() - (volatility * volatility / 2.0) * time_to_expiry_years)
            / (volatility * time_to_expiry_years.sqrt());
        std_normal().cdf(d2)
    }
}
