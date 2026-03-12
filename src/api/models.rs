//! API request/response DTOs for SaaS control plane.

use serde::Deserialize;

fn default_volatility() -> f64 {
    0.80
}

/// Request body for starting a sniper strategy on a market.
#[derive(Debug, Clone, Deserialize)]
pub struct StartStrategyReq {
    pub token_id: String,
    pub strike_price: f64,
    pub snipe_size: f64,
    pub expiry_timestamp: i64,
    #[serde(default = "default_volatility")]
    pub volatility: f64,
}

/// Request body for stopping a sniper strategy by market token.
#[derive(Debug, Clone, Deserialize)]
pub struct StopStrategyReq {
    pub token_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StartEventRadarReq {
    pub event_slug: String,
    #[serde(default = "default_volatility")]
    pub volatility: f64,
    pub snipe_size: f64,
}
