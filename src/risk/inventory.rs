//! In-memory inventory state (memory-first, hot path never blocks on I/O).
//! Design aligned with [PolyMatrixEngine](https://github.com/liu-kaining/PolyMatrixEngine) `app/core/inventory_state.py`.
//!
//! CRITICAL: Uses Decimal internally for precision. Public API accepts f64 for convenience
//! but converts to Decimal for storage to avoid cumulative precision loss.

use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use rust_decimal::Decimal;
use serde::Serialize;

use crate::models::types::OrderSide;

/// Per-market exposure snapshot: positions + pending buy notional + reconciliation metadata.
/// All quantities stored as Decimal for exact arithmetic.
#[derive(Debug, Clone, Serialize)]
#[allow(dead_code)]
pub struct MarketExposure {
    pub yes_qty: f64,          // Serialized as f64 for API compatibility
    pub no_qty: f64,
    pub pending_yes_buy_notional: f64,
    pub pending_no_buy_notional: f64,
    pub realized_pnl: f64,
    pub last_local_fill_timestamp: f64,
}

/// Internal representation with Decimal precision.
#[derive(Debug, Clone, Default)]
struct InternalExposure {
    yes_qty: Decimal,
    no_qty: Decimal,
    pending_yes_buy_notional: Decimal,
    pending_no_buy_notional: Decimal,
    realized_pnl: Decimal,
    last_local_fill_timestamp: f64,
}

impl Default for MarketExposure {
    fn default() -> Self {
        Self {
            yes_qty: 0.0,
            no_qty: 0.0,
            pending_yes_buy_notional: 0.0,
            pending_no_buy_notional: 0.0,
            realized_pnl: 0.0,
            last_local_fill_timestamp: 0.0,
        }
    }
}

impl From<InternalExposure> for MarketExposure {
    fn from(inner: InternalExposure) -> Self {
        Self {
            yes_qty: inner.yes_qty.to_string().parse().unwrap_or(0.0),
            no_qty: inner.no_qty.to_string().parse().unwrap_or(0.0),
            pending_yes_buy_notional: inner.pending_yes_buy_notional.to_string().parse().unwrap_or(0.0),
            pending_no_buy_notional: inner.pending_no_buy_notional.to_string().parse().unwrap_or(0.0),
            realized_pnl: inner.realized_pnl.to_string().parse().unwrap_or(0.0),
            last_local_fill_timestamp: inner.last_local_fill_timestamp,
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

    fn now_secs() -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    }

    fn f64_to_decimal(v: f64) -> Decimal {
        Decimal::from_f64_retain(v).unwrap_or(Decimal::ZERO)
    }

    /// Record a fill in memory (memory-first; no DB in hot path).
    /// Updates yes/no exposure, realized_pnl, and last_local_fill_timestamp.
    pub fn apply_fill(
        &self,
        market_id: &str,
        is_yes: bool,
        side: OrderSide,
        filled_size: f64,
        fill_price: f64,
    ) {
        let now = Self::now_secs();
        let size_dec = Self::f64_to_decimal(filled_size);
        let price_dec = Self::f64_to_decimal(fill_price);
        
        let mut entry = self
            .exposures
            .entry(market_id.to_string())
            .or_insert_with(InternalExposure::default);

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
        entry.last_local_fill_timestamp = now;
    }

    /// Convenience: record fill by token outcome only (no price/pnl). Prefer `apply_fill` when you have price.
    pub fn add_fill(&self, token_id: &str, is_yes: bool, side: OrderSide, qty: f64) {
        let delta = Self::f64_to_decimal(qty);
        let now = Self::now_secs();
        
        let mut entry = self
            .exposures
            .entry(token_id.to_string())
            .or_insert_with(InternalExposure::default);
            
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
        entry.last_local_fill_timestamp = now;
    }

    /// Total exposure across all markets (yes + no + pending buy notionals).
    /// Used for global budget check (align with PolyMatrix get_global_exposure).
    pub fn get_global_exposure(&self) -> f64 {
        self.exposures.iter().fold(Decimal::ZERO, |acc, r| {
            acc + r.yes_qty + r.no_qty
                + r.pending_yes_buy_notional
                + r.pending_no_buy_notional
        }).to_string().parse().unwrap_or(0.0)
    }

    /// Total exposure excluding one market (e.g. when evaluating adding a new market).
    pub fn get_global_exposure_excluding(&self, market_id: &str) -> f64 {
        self.exposures
            .iter()
            .filter(|r| r.key() != market_id)
            .fold(Decimal::ZERO, |acc, r| {
                acc + r.yes_qty + r.no_qty
                    + r.pending_yes_buy_notional
                    + r.pending_no_buy_notional
            }).to_string().parse().unwrap_or(0.0)
    }

    /// Update pending BUY notional for a token (for balance precheck before placing orders).
    pub fn update_pending_buy_notional(&self, market_id: &str, is_yes: bool, notional: f64) {
        let n = Self::f64_to_decimal(notional).max(Decimal::ZERO);
        
        let mut entry = self
            .exposures
            .entry(market_id.to_string())
            .or_insert_with(InternalExposure::default);
            
        if is_yes {
            entry.pending_yes_buy_notional = n;
        } else {
            entry.pending_no_buy_notional = n;
        }
    }

    /// Seconds since epoch of last local fill; watchdog uses this to skip overwrite shortly after fill.
    pub fn get_last_local_fill_timestamp(&self, market_id: &str) -> f64 {
        self.exposures
            .get(market_id)
            .map(|e| e.last_local_fill_timestamp)
            .unwrap_or(0.0)
    }

    /// Overwrite in-memory yes/no from reconciliation (e.g. REST positions); keep last_local_fill_timestamp.
    pub fn apply_reconciliation_snapshot(
        &self,
        market_id: &str,
        yes_exposure: f64,
        no_exposure: f64,
    ) {
        let mut entry = self
            .exposures
            .entry(market_id.to_string())
            .or_insert_with(InternalExposure::default);
        entry.yes_qty = Self::f64_to_decimal(yes_exposure);
        entry.no_qty = Self::f64_to_decimal(no_exposure);
    }

    /// Net exposure for the market: Yes positive, No negative.
    pub fn get_net_exposure(&self, token_id: &str) -> f64 {
        self.exposures
            .get(token_id)
            .map(|e| (e.yes_qty - e.no_qty).to_string().parse().unwrap_or(0.0))
            .unwrap_or(0.0)
    }

    pub fn get_exposure(&self, token_id: &str) -> MarketExposure {
        self.exposures
            .get(token_id)
            .map(|r| MarketExposure::from((*r).clone()))
            .unwrap_or_default()
    }

    /// All market ids currently in the ledger (for watchdog iteration).
    pub fn market_ids(&self) -> Vec<String> {
        self.exposures.iter().map(|r| r.key().clone()).collect()
    }

    /// Snapshot all market exposures for API responses.
    pub fn snapshot_all(&self) -> Vec<(String, MarketExposure)> {
        self.exposures
            .iter()
            .map(|r| (r.key().clone(), MarketExposure::from((*r).clone())))
            .collect()
    }
}