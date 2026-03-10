use std::env;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AppConfig {
    pub binance_symbol: String,
    /// Polymarket token (asset) id for the YES outcome to snipe (U256 as string).
    pub poly_token_id: Option<String>,
    pub chain_id: u64,
    pub funder_address: Option<String>,
    /// Max total spend (budget cap) for RiskGuard (GLOBAL_MAX_BUDGET).
    pub max_budget: f64,
    /// Per-market exposure cap; watchdog kill-switch (MAX_EXPOSURE_PER_MARKET).
    pub max_exposure_per_market: f64,
    /// Seconds after last local fill to skip REST overwrite during reconciliation.
    pub reconciliation_buffer_seconds: f64,
}

impl AppConfig {
    pub fn from_env() -> Self {
        let binance_symbol = env::var("BINANCE_SYMBOL").unwrap_or_else(|_| "btcusdt".to_string());
        let poly_token_id = env::var("POLY_TOKEN_ID").ok();
        let chain_id = env::var("CHAIN_ID")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(137); // default to Polygon mainnet

        let funder_address = env::var("FUNDER_ADDRESS").ok();

        let max_budget = env::var("MAX_BUDGET")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(500.0);

        let max_exposure_per_market = env::var("MAX_EXPOSURE_PER_MARKET")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0);

        let reconciliation_buffer_seconds = env::var("RECONCILIATION_BUFFER_SECONDS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(8.0);

        Self {
            binance_symbol,
            poly_token_id,
            chain_id,
            funder_address,
            max_budget,
            max_exposure_per_market,
            reconciliation_buffer_seconds,
        }
    }
}
