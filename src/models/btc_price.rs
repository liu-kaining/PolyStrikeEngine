use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use chrono::Utc;

/// Global atomic BTC mid price, stored as f64 bits in an AtomicU64.
static GLOBAL_BTC_MID_BITS: AtomicU64 = AtomicU64::new(0);

/// 整点快照缓存：最近约 15 分钟 (ts_sec, price)，用于 5m 开盘价补位。
const CACHE_MAX_AGE_SECS: i64 = 900;
static PRICE_CACHE: RwLock<Vec<(i64, f64)>> = RwLock::new(Vec::new());

/// 向缓存写入一条 (ts, price)；供 set_btc_mid、warmup、fetch 使用。
fn record_price(ts: i64, price: f64) {
    if let Ok(mut cache) = PRICE_CACHE.write() {
        cache.push((ts, price));
        let cutoff = Utc::now().timestamp().saturating_sub(CACHE_MAX_AGE_SECS);
        cache.retain(|(t, _)| *t >= cutoff);
    }
}

/// Update the global BTC mid; used by all sniper tasks.
/// Also records (timestamp, price) for get_btc_at_timestamp lookups (e.g. 5m opening price).
pub fn set_btc_mid(price: f64) {
    let ts = Utc::now().timestamp();
    GLOBAL_BTC_MID_BITS.store(price.to_bits(), Ordering::Relaxed);
    record_price(ts, price);
}

/// Read the latest BTC mid seen by any task.
#[allow(dead_code)]
pub fn get_btc_mid() -> f64 {
    f64::from_bits(GLOBAL_BTC_MID_BITS.load(Ordering::Relaxed))
}

/// 根据整点时间戳取缓存的 BTC 价格：优先该秒的精准价格，否则返回最接近 target_timestamp 的那条。
/// 用于 5m 相对价盘口的 strike 补位（与官方开盘价对齐）。缓存为空时返回 None，不返回 0。
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

const BINANCE_KLINES_URL: &str = "https://api.binance.com/api/v3/klines";

/// 从 Binance REST 拉取指定时间所在那一分钟 K 线的开盘价；写入缓存并返回。
/// 用于机器人刚启动、内存缓存为空时的补位。
pub async fn fetch_historic_price_from_binance(ts: i64) -> Option<f64> {
    let minute_ts = (ts / 60) * 60;
    let start_ms = minute_ts * 1000;
    let url = format!(
        "{}?symbol=BTCUSDT&interval=1m&startTime={}&limit=1",
        BINANCE_KLINES_URL, start_ms
    );
    let client = reqwest::Client::new();
    let resp = client.get(&url).send().await.ok()?;
    let arr: Vec<serde_json::Value> = resp.json().await.ok()?;
    let row = arr.first()?;
    let open_str = row.get(1)?.as_str()?;
    let price: f64 = open_str.parse().ok()?;
    record_price(minute_ts, price);
    Some(price)
}

/// 预热：拉取过去 15 分钟的 1m K 线开盘价写入缓存（Engine/Radar 启动时调用）。
pub async fn warmup_btc_cache_for_past_15_min(anchor_ts: i64) {
    let minute_ts = (anchor_ts / 60) * 60;
    let start_ms = (minute_ts - 14 * 60) * 1000;
    let url = format!(
        "{}?symbol=BTCUSDT&interval=1m&startTime={}&limit=15",
        BINANCE_KLINES_URL, start_ms
    );
    let client = reqwest::Client::new();
    let Ok(resp) = client.get(&url).send().await else { return };
    let Ok(arr) = resp.json::<Vec<serde_json::Value>>().await else { return };
    for row in arr {
        let Some(open_time_ms) = row.get(0).and_then(|v| v.as_i64()) else { continue };
        let open_sec = open_time_ms / 1000;
        let Some(open_str) = row.get(1).and_then(|v| v.as_str()) else { continue };
        let Ok(price) = open_str.parse::<f64>() else { continue };
        record_price(open_sec, price);
    }
}
