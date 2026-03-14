pub mod radar;

use chrono::{Datelike, Duration, Utc};

/// Predicts the next day's event slug in format `bitcoin-above-on-{month}-{day}` (UTC).
/// Handles month rollover and leap years via chrono.
#[allow(dead_code)]
pub fn predict_next_slug() -> String {
    let tomorrow = (Utc::now() + Duration::days(1)).date_naive();
    format!(
        "bitcoin-above-on-{}-{}",
        tomorrow.month(),
        tomorrow.day()
    )
}

/// 推算当前应锁定的 5 分钟市场 slug：**当前** 5 分钟整点时间戳（不减 300，不提前到下一档）。
/// target_timestamp = (now / 300) * 300
pub fn predict_current_5m_slug() -> String {
    let now = Utc::now().timestamp();
    let target_timestamp = (now / 300) * 300;
    format!("btc-updown-5m-{}", target_timestamp)
}
