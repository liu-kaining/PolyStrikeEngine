//! In-memory inventory state (memory-first, hot path never blocks on I/O).
//! Design aligned with [PolyMatrixEngine](https://github.com/liu-kaining/PolyMatrixEngine) `app/core/inventory_state.py`.

use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashMap;

use crate::models::types::OrderSide;

/// Per-market exposure snapshot: positions + pending buy notional + reconciliation metadata.
#[derive(Debug, Default, Clone)]
#[allow(dead_code)]
pub struct MarketExposure {
    pub yes_qty: f64,
    pub no_qty: f64,
    /// Total notional of active BUY orders for Yes token (for balance precheck).
    pub pending_yes_buy_notional: f64,
    /// Total notional of active BUY orders for No token.
    pub pending_no_buy_notional: f64,
    pub realized_pnl: f64,
    /// Unix timestamp of last local fill; used by watchdog to skip stale REST overwrite.
    pub last_local_fill_timestamp: f64,
}

/// Lock-free in-memory ledger: market_id -> MarketExposure.
/// Read path (engine tick): memory only. Write path (fills): memory first.
pub struct InventoryManager {
    exposures: DashMap<String, MarketExposure>,
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
        let mut entry = self
            .exposures
            .entry(market_id.to_string())
            .or_insert_with(MarketExposure::default);

        match side {
            OrderSide::Buy => {
                if is_yes {
                    entry.yes_qty += filled_size;
                } else {
                    entry.no_qty += filled_size;
                }
                entry.realized_pnl -= fill_price * filled_size;
            }
            OrderSide::Sell => {
                if is_yes {
                    entry.yes_qty -= filled_size;
                } else {
                    entry.no_qty -= filled_size;
                }
                entry.realized_pnl += fill_price * filled_size;
            }
        }
        entry.last_local_fill_timestamp = now;
    }

    /// Convenience: record fill by token outcome only (no price/pnl). Prefer `apply_fill` when you have price.
    pub fn add_fill(&self, token_id: &str, is_yes: bool, side: OrderSide, qty: f64) {
        let delta = match side {
            OrderSide::Buy => qty,
            OrderSide::Sell => -qty,
        };
        let mut entry = self
            .exposures
            .entry(token_id.to_string())
            .or_insert_with(MarketExposure::default);
        if is_yes {
            entry.yes_qty += delta;
        } else {
            entry.no_qty += delta;
        }
        entry.last_local_fill_timestamp = Self::now_secs();
    }

    /// Total exposure across all markets (yes + no + pending buy notionals).
    /// Used for global budget check (align with PolyMatrix get_global_exposure).
    pub fn get_global_exposure(&self) -> f64 {
        self.exposures.iter().fold(0.0, |acc, r| {
            acc + r.yes_qty + r.no_qty
                + r.pending_yes_buy_notional
                + r.pending_no_buy_notional
        })
    }

    /// Total exposure excluding one market (e.g. when evaluating adding a new market).
    pub fn get_global_exposure_excluding(&self, market_id: &str) -> f64 {
        self.exposures
            .iter()
            .filter(|r| r.key() != market_id)
            .fold(0.0, |acc, r| {
                acc + r.yes_qty + r.no_qty
                    + r.pending_yes_buy_notional
                    + r.pending_no_buy_notional
            })
    }

    /// Update pending BUY notional for a token (for balance precheck before placing orders).
    pub fn update_pending_buy_notional(&self, market_id: &str, is_yes: bool, notional: f64) {
        let mut entry = self
            .exposures
            .entry(market_id.to_string())
            .or_insert_with(MarketExposure::default);
        let n = notional.max(0.0);
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
            .or_insert_with(MarketExposure::default);
        entry.yes_qty = yes_exposure;
        entry.no_qty = no_exposure;
    }

    /// Net exposure for the market: Yes positive, No negative.
    pub fn get_net_exposure(&self, token_id: &str) -> f64 {
        self.exposures
            .get(token_id)
            .map(|e| e.yes_qty - e.no_qty)
            .unwrap_or(0.0)
    }

    pub fn get_exposure(&self, token_id: &str) -> MarketExposure {
        self.exposures
            .get(token_id)
            .map(|r| (*r).clone())
            .unwrap_or_default()
    }

    /// All market ids currently in the ledger (for watchdog iteration).
    pub fn market_ids(&self) -> Vec<String> {
        self.exposures.iter().map(|r| r.key().clone()).collect()
    }
}
