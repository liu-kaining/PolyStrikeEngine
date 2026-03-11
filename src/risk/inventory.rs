//! In-memory inventory state (memory-first, hot path never blocks on I/O).
//!
//! CRITICAL: Uses Decimal internally for precision. Public API accepts f64 for convenience
//! but converts to Decimal for storage to avoid cumulative precision loss.
//! All Decimal->f64 conversions use ToPrimitive (zero-allocation) instead of to_string().parse().

use std::time::Instant;

use dashmap::DashMap;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;

use crate::models::types::OrderSide;

#[inline]
fn dec_to_f64(d: Decimal) -> f64 {
    d.to_f64().unwrap_or(0.0)
}

/// Per-market exposure snapshot for API responses.
#[derive(Debug, Clone, Serialize)]
#[allow(dead_code)]
pub struct MarketExposure {
    pub yes_qty: f64,
    pub no_qty: f64,
    pub pending_yes_buy_notional: f64,
    pub pending_no_buy_notional: f64,
    pub realized_pnl: f64,
    pub last_local_fill_elapsed_secs: f64,
}

#[derive(Debug, Clone)]
struct InternalExposure {
    yes_qty: Decimal,
    no_qty: Decimal,
    pending_yes_buy_notional: Decimal,
    pending_no_buy_notional: Decimal,
    realized_pnl: Decimal,
    /// Monotonic timestamp of last local fill (immune to NTP drift).
    last_local_fill_at: Option<Instant>,
}

impl Default for InternalExposure {
    fn default() -> Self {
        Self {
            yes_qty: Decimal::ZERO,
            no_qty: Decimal::ZERO,
            pending_yes_buy_notional: Decimal::ZERO,
            pending_no_buy_notional: Decimal::ZERO,
            realized_pnl: Decimal::ZERO,
            last_local_fill_at: None,
        }
    }
}

impl Default for MarketExposure {
    fn default() -> Self {
        Self {
            yes_qty: 0.0,
            no_qty: 0.0,
            pending_yes_buy_notional: 0.0,
            pending_no_buy_notional: 0.0,
            realized_pnl: 0.0,
            last_local_fill_elapsed_secs: f64::MAX,
        }
    }
}

impl From<InternalExposure> for MarketExposure {
    fn from(inner: InternalExposure) -> Self {
        Self {
            yes_qty: dec_to_f64(inner.yes_qty),
            no_qty: dec_to_f64(inner.no_qty),
            pending_yes_buy_notional: dec_to_f64(inner.pending_yes_buy_notional),
            pending_no_buy_notional: dec_to_f64(inner.pending_no_buy_notional),
            realized_pnl: dec_to_f64(inner.realized_pnl),
            last_local_fill_elapsed_secs: inner
                .last_local_fill_at
                .map(|t| t.elapsed().as_secs_f64())
                .unwrap_or(f64::MAX),
        }
    }
}

/// Lock-free in-memory ledger: market_id -> InternalExposure.
/// Read path (engine tick): memory only. Write path (fills): memory first.
pub struct InventoryManager {
    exposures: DashMap<String, InternalExposure>,
}

impl Default for InventoryManager {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)]
impl InventoryManager {
    pub fn new() -> Self {
        Self {
            exposures: DashMap::new(),
        }
    }

    fn f64_to_decimal(v: f64) -> Decimal {
        Decimal::from_f64_retain(v).unwrap_or(Decimal::ZERO)
    }

    /// Record a fill in memory (memory-first; no DB in hot path).
    pub fn apply_fill(
        &self,
        market_id: &str,
        is_yes: bool,
        side: OrderSide,
        filled_size: f64,
        fill_price: f64,
    ) {
        let size_dec = Self::f64_to_decimal(filled_size);
        let price_dec = Self::f64_to_decimal(fill_price);

        let mut entry = self
            .exposures
            .entry(market_id.to_string())
            .or_default();

        match side {
            OrderSide::Buy => {
                if is_yes {
                    entry.yes_qty += size_dec;
                } else {
                    entry.no_qty += size_dec;
                }
                entry.realized_pnl -= price_dec * size_dec;
            }
            OrderSide::Sell => {
                if is_yes {
                    entry.yes_qty -= size_dec;
                } else {
                    entry.no_qty -= size_dec;
                }
                entry.realized_pnl += price_dec * size_dec;
            }
        }
        entry.last_local_fill_at = Some(Instant::now());
    }

    /// Convenience: record fill by token outcome only (no price/pnl).
    pub fn add_fill(&self, token_id: &str, is_yes: bool, side: OrderSide, qty: f64) {
        let delta = Self::f64_to_decimal(qty);

        let mut entry = self
            .exposures
            .entry(token_id.to_string())
            .or_default();

        if is_yes {
            match side {
                OrderSide::Buy => entry.yes_qty += delta,
                OrderSide::Sell => entry.yes_qty -= delta,
            }
        } else {
            match side {
                OrderSide::Buy => entry.no_qty += delta,
                OrderSide::Sell => entry.no_qty -= delta,
            }
        }
        entry.last_local_fill_at = Some(Instant::now());
    }

    /// Total exposure across all markets.
    pub fn get_global_exposure(&self) -> f64 {
        let total = self.exposures.iter().fold(Decimal::ZERO, |acc, r| {
            acc + r.yes_qty + r.no_qty
                + r.pending_yes_buy_notional
                + r.pending_no_buy_notional
        });
        dec_to_f64(total)
    }

    /// Total exposure excluding one market.
    pub fn get_global_exposure_excluding(&self, market_id: &str) -> f64 {
        let total = self
            .exposures
            .iter()
            .filter(|r| r.key() != market_id)
            .fold(Decimal::ZERO, |acc, r| {
                acc + r.yes_qty + r.no_qty
                    + r.pending_yes_buy_notional
                    + r.pending_no_buy_notional
            });
        dec_to_f64(total)
    }

    /// Total realized P&L across all markets (in USDC).
    pub fn get_total_realized_pnl(&self) -> f64 {
        let total = self
            .exposures
            .iter()
            .fold(Decimal::ZERO, |acc, r| acc + r.realized_pnl);
        dec_to_f64(total)
    }

    /// Update pending BUY notional for a token.
    pub fn update_pending_buy_notional(&self, market_id: &str, is_yes: bool, notional: f64) {
        let n = Self::f64_to_decimal(notional).max(Decimal::ZERO);

        let mut entry = self
            .exposures
            .entry(market_id.to_string())
            .or_default();

        if is_yes {
            entry.pending_yes_buy_notional = n;
        } else {
            entry.pending_no_buy_notional = n;
        }
    }

    /// Seconds elapsed since last local fill (monotonic).
    /// Returns f64::MAX if no fill has been recorded.
    pub fn get_last_local_fill_elapsed_secs(&self, market_id: &str) -> f64 {
        self.exposures
            .get(market_id)
            .and_then(|e| e.last_local_fill_at.map(|t| t.elapsed().as_secs_f64()))
            .unwrap_or(f64::MAX)
    }

    /// Overwrite in-memory yes/no from reconciliation.
    pub fn apply_reconciliation_snapshot(
        &self,
        market_id: &str,
        yes_exposure: f64,
        no_exposure: f64,
    ) {
        let mut entry = self
            .exposures
            .entry(market_id.to_string())
            .or_default();
        entry.yes_qty = Self::f64_to_decimal(yes_exposure);
        entry.no_qty = Self::f64_to_decimal(no_exposure);
    }

    /// Net exposure for the market: Yes positive, No negative.
    pub fn get_net_exposure(&self, token_id: &str) -> f64 {
        self.exposures
            .get(token_id)
            .map(|e| dec_to_f64(e.yes_qty - e.no_qty))
            .unwrap_or(0.0)
    }

    /// Zero-allocation hot-path read: returns (yes_qty, no_qty) as f64.
    pub fn get_exposure_qty(&self, token_id: &str) -> (f64, f64) {
        self.exposures
            .get(token_id)
            .map(|e| (dec_to_f64(e.yes_qty), dec_to_f64(e.no_qty)))
            .unwrap_or((0.0, 0.0))
    }

    pub fn get_exposure(&self, token_id: &str) -> MarketExposure {
        self.exposures
            .get(token_id)
            .map(|r| MarketExposure::from((*r).clone()))
            .unwrap_or_default()
    }

    pub fn market_ids(&self) -> Vec<String> {
        self.exposures.iter().map(|r| r.key().clone()).collect()
    }

    pub fn snapshot_all(&self) -> Vec<(String, MarketExposure)> {
        self.exposures
            .iter()
            .map(|r| (r.key().clone(), MarketExposure::from((*r).clone())))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::types::OrderSide;
    use std::{sync::Arc, thread, time::Duration};

    #[test]
    fn test_apply_fill_accumulates_and_pnl() {
        let inventory = InventoryManager::new();
        let market = "mkt-test";

        inventory.apply_fill(market, true, OrderSide::Buy, 10.0, 0.5);
        inventory.apply_fill(market, true, OrderSide::Buy, 5.0, 0.6);
        inventory.apply_fill(market, true, OrderSide::Sell, 8.0, 0.7);

        let exposure = inventory.get_exposure(market);

        let expected_yes = 10.0 + 5.0 - 8.0;
        let expected_pnl = -5.0 - 3.0 + 5.6;

        assert!((exposure.yes_qty - expected_yes).abs() < 1e-9);
        assert!((exposure.no_qty - 0.0).abs() < 1e-9);
        assert!((exposure.realized_pnl - expected_pnl).abs() < 1e-9);
    }

    #[test]
    fn test_last_local_fill_elapsed_updates() {
        let inventory = InventoryManager::new();
        let market = "mkt-ts";

        let e0 = inventory.get_last_local_fill_elapsed_secs(market);
        assert_eq!(e0, f64::MAX);

        inventory.add_fill(market, true, OrderSide::Buy, 1.0);
        let e1 = inventory.get_last_local_fill_elapsed_secs(market);
        assert!(e1 < 1.0);

        thread::sleep(Duration::from_millis(10));
        inventory.add_fill(market, true, OrderSide::Buy, 1.0);
        let e2 = inventory.get_last_local_fill_elapsed_secs(market);
        assert!(e2 < e1 + 1.0);
    }

    #[test]
    fn test_concurrent_add_fill_is_consistent() {
        let inventory = Arc::new(InventoryManager::new());
        let market = "mkt-concurrent";
        let threads = 4;
        let per_thread = 100;

        let mut handles = Vec::new();
        for _ in 0..threads {
            let inv = inventory.clone();
            let m = market.to_string();
            handles.push(thread::spawn(move || {
                for _ in 0..per_thread {
                    inv.add_fill(&m, true, OrderSide::Buy, 1.0);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let exposure = inventory.get_exposure(market);
        let expected_yes = (threads * per_thread) as f64;
        assert!((exposure.yes_qty - expected_yes).abs() < 1e-6);
        assert!((exposure.no_qty - 0.0).abs() < 1e-6);
    }
}
