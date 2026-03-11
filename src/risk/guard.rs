//! Global budget breaker and per-market exposure cap.
//! Aligned with [PolyMatrixEngine](https://github.com/liu-kaining/PolyMatrixEngine) GLOBAL_MAX_BUDGET and MAX_EXPOSURE_PER_MARKET.
//!
//! CRITICAL: Uses Decimal for all monetary calculations to avoid f64 precision loss.

use std::sync::RwLock;

use rust_decimal::Decimal;

/// Global budget breaker + optional per-market exposure cap.
/// All monetary values are stored as Decimal to prevent precision loss.
pub struct RiskGuard {
    /// Current total spent (in USDC). Decimal for exact arithmetic.
    current_total_spend: RwLock<Decimal>,
    /// Maximum allowed budget (in USDC).
    max_budget: Decimal,
    /// Per-market cap (absolute yes/no exposure); 0 = disabled.
    max_exposure_per_market: Decimal,
}

#[allow(dead_code)]
impl RiskGuard {
    pub fn new(max_budget: f64) -> Self {
        Self {
            current_total_spend: RwLock::new(Decimal::ZERO),
            max_budget: Decimal::from_f64_retain(max_budget).unwrap_or(Decimal::MAX),
            max_exposure_per_market: Decimal::ZERO,
        }
    }

    /// With per-market exposure cap (e.g. MAX_EXPOSURE_PER_MARKET).
    pub fn with_max_exposure_per_market(mut self, max_per_market: f64) -> Self {
        self.max_exposure_per_market = Decimal::from_f64_retain(max_per_market).unwrap_or(Decimal::ZERO);
        self
    }

    /// Check and reserve budget for an order. Returns a `BudgetReservation` on success.
    /// The caller MUST call `release_budget` if the order fails to execute.
    ///
    /// # Example
    /// ```ignore
    /// let reservation = risk_guard.reserve_budget(price, size)?;
    /// match client.execute_order(...).await {
    ///     Ok(_) => { /* budget consumed, reservation dropped */ }
    ///     Err(_) => {
    ///         risk_guard.release_budget(reservation); // CRITICAL: must release on failure!
    ///     }
    /// }
    /// ```
    pub fn reserve_budget(&self, price: f64, size: f64) -> Result<BudgetReservation, &'static str> {
        let price_dec = Decimal::from_f64_retain(price).ok_or("RiskGuard: invalid price")?;
        let size_dec = Decimal::from_f64_retain(size).ok_or("RiskGuard: invalid size")?;
        let cost = price_dec * size_dec;

        let mut spend = self
            .current_total_spend
            .write()
            .map_err(|_| "RiskGuard: lock poisoned")?;
        
        if *spend + cost > self.max_budget {
            return Err("RiskGuard: Global budget exceeded!");
        }
        
        *spend += cost;
        
        Ok(BudgetReservation { amount: cost })
    }

    /// Legacy method for backward compatibility. Prefer `reserve_budget` + `release_budget`.
    #[deprecated(note = "Use reserve_budget() + release_budget() for atomic budget management")]
    pub fn check_and_reserve_budget(&self, price: f64, size: f64) -> Result<(), &'static str> {
        self.reserve_budget(price, size).map(|_| ())
    }

    /// Release reserved budget back when an order fails to execute.
    /// This is CRITICAL for preventing budget leakage on failed orders.
    pub fn release_budget(&self, reservation: BudgetReservation) {
        if let Ok(mut spend) = self.current_total_spend.write() {
            *spend = (*spend - reservation.amount).max(Decimal::ZERO);
        }
    }

    /// Per-market exposure check (kill-switch logic). Returns Err if yes or no exceeds cap.
    pub fn check_market_exposure(
        &self,
        yes_qty: f64,
        no_qty: f64,
    ) -> Result<(), &'static str> {
        if self.max_exposure_per_market <= Decimal::ZERO {
            return Ok(());
        }
        
        let yes_dec = Decimal::from_f64_retain(yes_qty.abs()).unwrap_or(Decimal::ZERO);
        let no_dec = Decimal::from_f64_retain(no_qty.abs()).unwrap_or(Decimal::ZERO);
        
        if yes_dec > self.max_exposure_per_market {
            return Err("RiskGuard: Per-market YES exposure exceeded!");
        }
        if no_dec > self.max_exposure_per_market {
            return Err("RiskGuard: Per-market NO exposure exceeded!");
        }
        Ok(())
    }

    pub fn max_budget(&self) -> f64 {
        self.max_budget.to_string().parse().unwrap_or(0.0)
    }

    pub fn max_exposure_per_market(&self) -> f64 {
        self.max_exposure_per_market.to_string().parse().unwrap_or(0.0)
    }

    /// Get current total spent (for monitoring/debugging).
    pub fn current_spent(&self) -> Decimal {
        self.current_total_spend.read()
            .map(|r| *r)
            .unwrap_or(Decimal::ZERO)
    }
}

/// A reservation of budget that must be released if the order fails.
/// Dropping this without executing the order will leak budget.
/// Use `risk_guard.release_budget(reservation)` to release on failure.
#[derive(Debug)]
pub struct BudgetReservation {
    amount: Decimal,
}

impl BudgetReservation {
    /// Get the reserved amount.
    #[inline]
    #[allow(dead_code)]
    pub fn amount(&self) -> Decimal {
        self.amount
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_reserve_and_release() {
        let guard = RiskGuard::new(100.0);
        
        // Reserve budget
        let res = guard.reserve_budget(10.0, 5.0).unwrap();
        assert_eq!(res.amount(), dec!(50));
        assert_eq!(guard.current_spent(), dec!(50));
        
        // Release on failure
        guard.release_budget(res);
        assert_eq!(guard.current_spent(), Decimal::ZERO);
    }

    #[test]
    fn test_budget_exceeded() {
        let guard = RiskGuard::new(100.0);
        
        // Should succeed
        let _ = guard.reserve_budget(50.0, 1.0).unwrap();
        
        // Should fail - would exceed budget
        assert!(guard.reserve_budget(60.0, 1.0).is_err());
    }

    #[test]
    fn test_precision() {
        let guard = RiskGuard::new(1.0);
        
        // f64 precision issue: 0.1 + 0.2 != 0.3
        // But with Decimal, we get exact arithmetic
        let _ = guard.reserve_budget(0.1, 1.0).unwrap();
        let _ = guard.reserve_budget(0.2, 1.0).unwrap();
        let spent = guard.current_spent();
        let diff = (spent - dec!(0.3)).abs();
        // Decimal should keep us within a very tight bound around 0.3
        assert!(diff < dec!(0.0000000001));
    }

    #[test]
    fn test_market_exposure_limits() {
        let guard = RiskGuard::new(0.0).with_max_exposure_per_market(10.0);

        // Below limit should be ok
        assert!(guard.check_market_exposure(9.9, 0.0).is_ok());
        assert!(guard.check_market_exposure(0.0, -9.9).is_ok());

        // At limit should be ok
        assert!(guard.check_market_exposure(10.0, 0.0).is_ok());
        assert!(guard.check_market_exposure(0.0, -10.0).is_ok());

        // Above limit must be rejected
        assert!(guard.check_market_exposure(10.1, 0.0).is_err());
        assert!(guard.check_market_exposure(0.0, -10.1).is_err());
    }

    #[test]
    fn test_concurrent_reserve_budget_never_exceeds_max() {
        let guard = Arc::new(RiskGuard::new(100.0));

        let mut handles = Vec::new();
        for _ in 0..4 {
            let g = guard.clone();
            handles.push(thread::spawn(move || {
                // Each thread tries to reserve up to 25 units of budget
                for _ in 0..25 {
                    let _ = g.reserve_budget(1.0, 1.0);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // Total reserved must never exceed global max_budget (100)
        let spent = guard.current_spent();
        assert!(spent <= dec!(100));
        assert!(spent >= Decimal::ZERO);
    }
}