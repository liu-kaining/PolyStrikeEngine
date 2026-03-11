//! Background risk monitor: per-market and global exposure checks.
//! Aligned with [PolyMatrixEngine](https://github.com/liu-kaining/PolyMatrixEngine) `app/risk/watchdog.py`.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::sleep;
use tracing::error;

use super::{InventoryManager, RiskGuard};

/// Real-time risk check based on in-memory state (no DB in loop).
/// Logs critical and can trigger kill-switch when exposure exceeds cap.
#[allow(dead_code)]
pub struct Watchdog {
    pub max_exposure_per_market: f64,
    pub global_max_budget: f64,
    /// Allow small leeway for race conditions (e.g. 1.05 = 5%).
    pub global_leeway: f64,
}

impl Default for Watchdog {
    fn default() -> Self {
        Self {
            max_exposure_per_market: 0.0,
            global_max_budget: 1000.0,
            global_leeway: 1.05,
        }
    }
}

#[allow(dead_code)]
impl Watchdog {
    pub fn new(max_exposure_per_market: f64, global_max_budget: f64) -> Self {
        Self {
            max_exposure_per_market,
            global_max_budget,
            global_leeway: 1.05,
        }
    }

    /// One-shot check: per-market breach and global budget breach.
    /// Returns true if any breach was detected (caller may suspend/cancel).
    pub fn check_exposure(
        &self,
        inventory: &InventoryManager,
        _risk_guard: &RiskGuard,
    ) -> bool {
        let mut breached = false;

        for market_id in inventory.market_ids() {
            let snap = inventory.get_exposure(&market_id);
            let yes_exp = snap.yes_qty.abs();
            let no_exp = snap.no_qty.abs();

            if self.max_exposure_per_market > 0.0
                && (yes_exp > self.max_exposure_per_market || no_exp > self.max_exposure_per_market)
            {
                error!(
                    "[Watchdog] RISK BREACH (Memory): Market {} exceeded limit ({})! YES: {:.2}, NO: {:.2}",
                    &market_id[..market_id.len().min(12)],
                    self.max_exposure_per_market,
                    yes_exp,
                    no_exp
                );
                breached = true;
            }
        }

        let global_exposure = inventory.get_global_exposure();
        let limit = self.global_max_budget * self.global_leeway;
        if global_exposure > limit {
            error!(
                "[Watchdog] GLOBAL RISK BREACH: Total exposure ${:.2} exceeds budget ${:.2} (with leeway)!",
                global_exposure, limit
            );
            breached = true;
        }

        breached
    }

    /// Run the watchdog loop (poll every interval). Pass Arc so it can be spawned.
    pub async fn run_loop(
        self: Arc<Self>,
        inventory: Arc<InventoryManager>,
        risk_guard: Arc<RiskGuard>,
        interval_secs: u64,
    ) {
        let mut interval = sleep(Duration::from_secs(interval_secs));
        loop {
            interval.await;
            self.check_exposure(&inventory, &risk_guard);
            interval = sleep(Duration::from_secs(interval_secs));
        }
    }
}
