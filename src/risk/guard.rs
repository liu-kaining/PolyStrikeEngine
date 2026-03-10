//! Global budget breaker and per-market exposure cap.
//! Aligned with [PolyMatrixEngine](https://github.com/liu-kaining/PolyMatrixEngine) GLOBAL_MAX_BUDGET and MAX_EXPOSURE_PER_MARKET.

use std::sync::RwLock;

/// Global budget breaker + optional per-market exposure cap.
pub struct RiskGuard {
    current_total_spend: RwLock<f64>,
    max_budget: f64,
    /// Per-market cap (absolute yes/no exposure); 0 = disabled.
    max_exposure_per_market: f64,
}

#[allow(dead_code)]
impl RiskGuard {
    pub fn new(max_budget: f64) -> Self {
        Self {
            current_total_spend: RwLock::new(0.0),
            max_budget,
            max_exposure_per_market: 0.0,
        }
    }

    /// With per-market exposure cap (e.g. MAX_EXPOSURE_PER_MARKET).
    pub fn with_max_exposure_per_market(mut self, max_per_market: f64) -> Self {
        self.max_exposure_per_market = max_per_market;
        self
    }

    /// Check and reserve budget for an order. Returns Err if budget would be exceeded.
    pub fn check_and_reserve_budget(&self, price: f64, size: f64) -> Result<(), &'static str> {
        let cost = price * size;
        let mut spend = self
            .current_total_spend
            .write()
            .map_err(|_| "RiskGuard: lock poisoned")?;
        if *spend + cost > self.max_budget {
            return Err("RiskGuard: Global budget exceeded!");
        }
        *spend += cost;
        Ok(())
    }

    /// Per-market exposure check (kill-switch logic). Returns Err if yes or no exceeds cap.
    pub fn check_market_exposure(
        &self,
        yes_qty: f64,
        no_qty: f64,
    ) -> Result<(), &'static str> {
        if self.max_exposure_per_market <= 0.0 {
            return Ok(());
        }
        if yes_qty.abs() > self.max_exposure_per_market {
            return Err("RiskGuard: Per-market YES exposure exceeded!");
        }
        if no_qty.abs() > self.max_exposure_per_market {
            return Err("RiskGuard: Per-market NO exposure exceeded!");
        }
        Ok(())
    }

    pub fn max_budget(&self) -> f64 {
        self.max_budget
    }

    pub fn max_exposure_per_market(&self) -> f64 {
        self.max_exposure_per_market
    }
}
