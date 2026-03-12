//! Global budget breaker and per-market exposure cap with hysteresis circuit breaker.
//!
//! CRITICAL: Uses Decimal for all monetary calculations to avoid f64 precision loss.
//! All Decimal->f64 conversions use ToPrimitive (zero-allocation) instead of to_string().parse().

use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

const FREEZE_THRESHOLD: f64 = 0.20;
const THAW_THRESHOLD: f64 = 0.15;

#[inline]
fn dec_to_f64(d: Decimal) -> f64 {
    d.to_f64().unwrap_or(0.0)
}

pub struct RiskGuard {
    current_total_spend: RwLock<Decimal>,
    max_budget: Decimal,
    max_exposure_per_market: Decimal,
    frozen: RwLock<bool>,
    /// Global flag: whether there is a BUY order in-flight anywhere in the system.
    /// Used to ensure we only have one outstanding BUY at a time (for tiny accounts).
    pending_buy_in_flight: AtomicBool,
}

#[allow(dead_code)]
impl RiskGuard {
    pub fn new(max_budget: f64) -> Self {
        Self {
            current_total_spend: RwLock::new(Decimal::ZERO),
            max_budget: Decimal::from_f64_retain(max_budget).unwrap_or(Decimal::MAX),
            max_exposure_per_market: Decimal::ZERO,
            frozen: RwLock::new(false),
            pending_buy_in_flight: AtomicBool::new(false),
        }
    }

    pub fn with_max_exposure_per_market(mut self, max_per_market: f64) -> Self {
        self.max_exposure_per_market =
            Decimal::from_f64_retain(max_per_market).unwrap_or(Decimal::ZERO);
        self
    }

    pub fn reserve_budget(
        &self,
        price: f64,
        size: f64,
    ) -> Result<BudgetReservation, &'static str> {
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

    pub fn release_budget(&self, reservation: BudgetReservation) {
        if let Ok(mut spend) = self.current_total_spend.write() {
            *spend = (*spend - reservation.amount).max(Decimal::ZERO);
        }
    }

    pub fn refund_budget_on_sell(&self, price: f64, actual_filled_size: f64) {
        let price_dec = match Decimal::from_f64_retain(price) {
            Some(p) => p,
            None => return,
        };
        let size_dec = match Decimal::from_f64_retain(actual_filled_size) {
            Some(s) => s,
            None => return,
        };
        let notional = price_dec * size_dec;
        if let Ok(mut spend) = self.current_total_spend.write() {
            *spend = (*spend - notional).max(Decimal::ZERO);
        }
    }

    pub fn refund_budget_delta(&self, reserved: &BudgetReservation, actual_price: f64, actual_size: f64) {
        let actual_cost = Decimal::from_f64_retain(actual_price)
            .unwrap_or(Decimal::ZERO)
            * Decimal::from_f64_retain(actual_size).unwrap_or(Decimal::ZERO);
        let overpay = (reserved.amount - actual_cost).max(Decimal::ZERO);
        if overpay > Decimal::ZERO {
            if let Ok(mut spend) = self.current_total_spend.write() {
                *spend = (*spend - overpay).max(Decimal::ZERO);
            }
        }
    }

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

    pub fn update_circuit_breaker(&self, total_realized_pnl: f64) -> bool {
        let max_budget_f64 = self.max_budget_f64();
        if max_budget_f64 <= 0.0 {
            return false;
        }
        let drawdown_ratio = (-total_realized_pnl) / max_budget_f64;

        let mut frozen = self.frozen.write().unwrap_or_else(|e| e.into_inner());
        if *frozen {
            if drawdown_ratio <= THAW_THRESHOLD {
                *frozen = false;
            }
        } else if drawdown_ratio >= FREEZE_THRESHOLD {
            *frozen = true;
        }
        *frozen
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen
            .read()
            .map(|f| *f)
            .unwrap_or(false)
    }

    /// Fast read-only affordability check (no budget mutation).
    /// Returns true if the budget has room for `price * size`.
    pub fn can_afford(&self, price: f64, size: f64) -> bool {
        let cost = match (Decimal::from_f64_retain(price), Decimal::from_f64_retain(size)) {
            (Some(p), Some(s)) => p * s,
            _ => return false,
        };
        self.current_total_spend
            .read()
            .map(|spend| *spend + cost <= self.max_budget)
            .unwrap_or(false)
    }

    pub fn max_budget_f64(&self) -> f64 {
        dec_to_f64(self.max_budget)
    }

    pub fn max_exposure_per_market_f64(&self) -> f64 {
        dec_to_f64(self.max_exposure_per_market)
    }

    pub fn current_spent(&self) -> Decimal {
        self.current_total_spend
            .read()
            .map(|r| *r)
            .unwrap_or(Decimal::ZERO)
    }

    /// Try to acquire the global BUY slot. Returns true if we successfully
    /// became the single in-flight BUY; false if another BUY is already running.
    pub fn try_acquire_global_buy_slot(&self) -> bool {
        // swap returns previous value; only succeed if it was false.
        !self.pending_buy_in_flight.swap(true, Ordering::AcqRel)
    }

    /// Release the global BUY slot, allowing another market to fire.
    pub fn release_global_buy_slot(&self) {
        self.pending_buy_in_flight.store(false, Ordering::Release);
    }

    /// Read-only check for diagnostics.
    pub fn is_global_buy_in_flight(&self) -> bool {
        self.pending_buy_in_flight.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone)]
pub struct BudgetReservation {
    amount: Decimal,
}

impl BudgetReservation {
    #[inline]
    #[cfg(test)]
    pub fn amount(&self) -> Decimal {
        self.amount
    }

    pub fn clone_for_release(&self) -> Self {
        Self {
            amount: self.amount,
        }
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
        let res = guard.reserve_budget(10.0, 5.0).unwrap();
        assert_eq!(res.amount(), dec!(50));
        assert_eq!(guard.current_spent(), dec!(50));
        guard.release_budget(res);
        assert_eq!(guard.current_spent(), Decimal::ZERO);
    }

    #[test]
    fn test_budget_exceeded() {
        let guard = RiskGuard::new(100.0);
        let _ = guard.reserve_budget(50.0, 1.0).unwrap();
        assert!(guard.reserve_budget(60.0, 1.0).is_err());
    }

    #[test]
    fn test_precision() {
        let guard = RiskGuard::new(1.0);
        let _ = guard.reserve_budget(0.1, 1.0).unwrap();
        let _ = guard.reserve_budget(0.2, 1.0).unwrap();
        let spent = guard.current_spent();
        let diff = (spent - dec!(0.3)).abs();
        assert!(diff < dec!(0.0000000001));
    }

    #[test]
    fn test_market_exposure_limits() {
        let guard = RiskGuard::new(0.0).with_max_exposure_per_market(10.0);
        assert!(guard.check_market_exposure(9.9, 0.0).is_ok());
        assert!(guard.check_market_exposure(0.0, -9.9).is_ok());
        assert!(guard.check_market_exposure(10.0, 0.0).is_ok());
        assert!(guard.check_market_exposure(0.0, -10.0).is_ok());
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
                for _ in 0..25 {
                    let _ = g.reserve_budget(1.0, 1.0);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let spent = guard.current_spent();
        assert!(spent <= dec!(100));
        assert!(spent >= Decimal::ZERO);
    }

    #[test]
    fn test_hysteresis_circuit_breaker() {
        let guard = RiskGuard::new(100.0);
        assert!(!guard.update_circuit_breaker(0.0));
        assert!(!guard.is_frozen());
        assert!(!guard.update_circuit_breaker(-19.0));
        assert!(guard.update_circuit_breaker(-20.0));
        assert!(guard.is_frozen());
        assert!(guard.update_circuit_breaker(-16.0));
        assert!(guard.is_frozen());
        assert!(!guard.update_circuit_breaker(-15.0));
        assert!(!guard.is_frozen());
    }

    #[test]
    fn test_refund_budget_delta_partial_fill() {
        let guard = RiskGuard::new(100.0);
        let reservation = guard.reserve_budget(1.0, 10.0).unwrap();
        assert_eq!(guard.current_spent(), dec!(10));
        guard.refund_budget_delta(&reservation, 1.0, 6.0);
        assert_eq!(guard.current_spent(), dec!(6));
    }
}
