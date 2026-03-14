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
