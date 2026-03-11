//! API request/response DTOs for SaaS control plane.

use serde::Deserialize;

/// Request body for starting a sniper strategy on a market.
#[derive(Debug, Clone, Deserialize)]
pub struct StartStrategyReq {
    pub token_id: String,
    pub strike_price: f64,
    pub snipe_size: f64,
}

/// Request body for stopping a sniper strategy by market token.
#[derive(Debug, Clone, Deserialize)]
pub struct StopStrategyReq {
    pub token_id: String,
}
