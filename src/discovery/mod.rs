pub mod radar;

use chrono::{Datelike, Duration, Utc};

/// Predicts the next day's event slug in format `bitcoin-above-on-{month}-{day}` (UTC).
/// Handles month rollover and leap years via chrono.
pub fn predict_next_slug() -> String {
    let tomorrow = (Utc::now() + Duration::days(1)).date_naive();
    format!(
        "bitcoin-above-on-{}-{}",
        tomorrow.month(),
        tomorrow.day()
    )
}

/// 推算当前应锁定的 5 分钟市场 slug：下一个 5 分钟整点时间戳对应的 btc-updown-5m-{ts}。
/// next_5m_timestamp = ((current_timestamp / 300) + 1) * 300
pub fn predict_current_5m_slug() -> String {
    let now = Utc::now().timestamp();
    let next_5m = ((now / 300) + 1) * 300;
    format!("btc-updown-5m-{}", next_5m)
}
