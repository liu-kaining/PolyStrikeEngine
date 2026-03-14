use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use chrono::Utc;

/// Global atomic BTC mid price, stored as f64 bits in an AtomicU64.
static GLOBAL_BTC_MID_BITS: AtomicU64 = AtomicU64::new(0);

/// 整点快照缓存：最近约 15 分钟 (ts_sec, price)，用于 5m 开盘价补位。
const CACHE_MAX_AGE_SECS: i64 = 900;
static PRICE_CACHE: RwLock<Vec<(i64, f64)>> = RwLock::new(Vec::new());

/// Update the global BTC mid; used by all sniper tasks.
/// Also records (timestamp, price) for get_btc_at_timestamp lookups (e.g. 5m opening price).
pub fn set_btc_mid(price: f64) {
    let ts = Utc::now().timestamp();
    GLOBAL_BTC_MID_BITS.store(price.to_bits(), Ordering::Relaxed);
    if let Ok(mut cache) = PRICE_CACHE.write() {
        cache.push((ts, price));
        let cutoff = ts.saturating_sub(CACHE_MAX_AGE_SECS);
        cache.retain(|(t, _)| *t >= cutoff);
    }
}

/// Read the latest BTC mid seen by any task.
#[allow(dead_code)]
pub fn get_btc_mid() -> f64 {
    f64::from_bits(GLOBAL_BTC_MID_BITS.load(Ordering::Relaxed))
}

/// 根据整点时间戳取缓存的 BTC 价格：优先该秒的精准价格，否则返回最接近 target_timestamp 的那条。
/// 用于 5m 相对价盘口的 strike 补位（与官方开盘价对齐）。
pub fn get_btc_at_timestamp(target_timestamp: i64) -> Option<f64> {
    let cache = PRICE_CACHE.read().ok()?;
    if cache.is_empty() {
        return None;
    }
    let (_, best_price) = cache
        .iter()
        .min_by_key(|(ts, _)| (*ts - target_timestamp).abs())
        .copied()?;
    Some(best_price)
}
