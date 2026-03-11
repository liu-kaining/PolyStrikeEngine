use std::error::Error;

use chrono::DateTime;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct MarketInfo {
    pub token_id: String,
    pub strike_price: f64,
    pub expiry_timestamp: i64,
}

fn parse_f64_field(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    v.as_str()?.parse::<f64>().ok()
}

fn parse_number_from_text(text: &str) -> Option<f64> {
    let mut started = false;
    let mut buf = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() || ch == '.' || (ch == '-' && !started) {
            started = true;
            buf.push(ch);
        } else if started {
            break;
        }
    }
    if buf.is_empty() || buf == "-" || buf == "." {
        return None;
    }
    buf.parse::<f64>().ok()
}

fn extract_yes_token_id(v: &Value) -> Option<String> {
    // Common case: clobTokenIds is a JSON array
    if let Some(arr) = v.get("clobTokenIds").and_then(|x| x.as_array()) {
        return arr.first()?.as_str().map(|s| s.to_string());
    }

    // Some payloads may encode it as a stringified JSON array
    let raw = v.get("clobTokenIds").and_then(|x| x.as_str())?;
    let parsed: Value = serde_json::from_str(raw).ok()?;
    parsed
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
}

pub async fn fetch_event_markets(slug: &str) -> Result<Vec<MarketInfo>, Box<dyn Error>> {
    let url = format!("https://gamma-api.polymarket.com/events?slug={slug}");
    let payload: Value = reqwest::get(url).await?.error_for_status()?.json().await?;

    let events = payload
        .as_array()
        .ok_or("gamma events response is not an array")?;

    let mut out = Vec::new();
    for event in events {
        let Some(markets) = event.get("markets").and_then(|m| m.as_array()) else {
            continue;
        };

        for market in markets {
            let active = market
                .get("active")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let closed = market
                .get("closed")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            if !active || closed {
                continue;
            }

            let Some(token_id) = extract_yes_token_id(market) else {
                continue;
            };

            let strike_price = parse_f64_field(market.get("line")).or_else(|| {
                market
                    .get("groupItemTitle")
                    .and_then(|v| v.as_str())
                    .and_then(parse_number_from_text)
            });
            let Some(strike_price) = strike_price else {
                continue;
            };

            let Some(end_date) = market.get("endDate").and_then(|v| v.as_str()) else {
                continue;
            };
            let expiry_timestamp = DateTime::parse_from_rfc3339(end_date)?.timestamp();

            out.push(MarketInfo {
                token_id,
                strike_price,
                expiry_timestamp,
            });
        }
    }

    Ok(out)
}
