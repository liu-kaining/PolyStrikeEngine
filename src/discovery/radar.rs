use std::error::Error;
use std::sync::OnceLock;

use chrono::DateTime;
use reqwest::Client;
use serde_json::Value;
use tracing::warn;

const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

#[derive(Debug, Clone)]
pub struct MarketInfo {
    pub token_id: String,
    pub strike_price: f64,
    pub expiry_timestamp: i64,
}

static RADAR_CLIENT: OnceLock<Client> = OnceLock::new();

fn build_client(proxy_url: Option<&str>) -> Result<Client, Box<dyn Error>> {
    let mut builder = Client::builder().user_agent(USER_AGENT);
    if let Some(url) = proxy_url {
        builder = builder.proxy(reqwest::Proxy::https(url)?);
    }
    Ok(builder.build()?)
}

pub fn init_radar_client(proxy_url: Option<&str>) -> Result<(), Box<dyn Error>> {
    let client = build_client(proxy_url)?;
    let _ = RADAR_CLIENT.set(client);
    Ok(())
}

fn get_client() -> &'static Client {
    RADAR_CLIENT.get_or_init(|| build_client(None).expect("default reqwest client"))
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
    if let Some(arr) = v.get("clobTokenIds").and_then(|x| x.as_array()) {
        return arr.first()?.as_str().map(|s| s.to_string());
    }

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
    let resp = get_client().get(&url).send().await?;

    let status = resp.status();
    if status == reqwest::StatusCode::FORBIDDEN {
        warn!(
            "[Radar] ⚠️ Gamma API returned 403 Forbidden for slug '{}'. \
             Possible IP block — check network node or proxy config (HTTPS_PROXY).",
            slug
        );
        return Err(format!("403 Forbidden: IP may be blocked for slug '{}'", slug).into());
    }
    let payload: Value = resp.error_for_status()?.json().await?;

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
            let Some(mut strike_price) = strike_price else {
                continue;
            };
            if strike_price < 1000.0 && strike_price > 10.0 {
                strike_price *= 1000.0;
            }

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

/// Parses expiry timestamp from slug (e.g. "btc-updown-5m-1734567890" -> 1734567890).
/// Returns None if slug does not end with a unix timestamp.
fn parse_expiry_from_slug(slug: &str) -> Option<i64> {
    let last = slug.rsplit('-').next()?;
    if last.len() == 10 && last.bytes().all(|b| b.is_ascii_digit()) {
        return last.parse::<i64>().ok();
    }
    None
}

/// Fetches one page of events from Gamma (no slug filter). Used by 5m radar.
async fn fetch_events_page(limit: u32, offset: u32) -> Result<Vec<Value>, Box<dyn Error>> {
    let url = "https://gamma-api.polymarket.com/events";
    let limit_s = limit.to_string();
    let offset_s = offset.to_string();
    let resp = get_client()
        .get(url)
        .query(&[
            ("limit", limit_s.as_str()),
            ("offset", offset_s.as_str()),
            ("active", "true"),
        ])
        .send()
        .await?;
    if resp.status() == reqwest::StatusCode::FORBIDDEN {
        return Err("Gamma API 403".into());
    }
    let payload: Value = resp.error_for_status()?.json().await?;
    let events = payload.as_array().ok_or("gamma events response is not an array")?;
    Ok(events.clone())
}

/// 5 分钟高频雷达：拉取包含 "updown-5m" 的活跃事件，只保留 1～3 分钟内到期的市场。
/// 供外部每 1 分钟定时调用（如 interval tick）。
pub async fn fetch_updown_5m_markets_near_expiry() -> Result<Vec<MarketInfo>, Box<dyn Error>> {
    let now = chrono::Utc::now().timestamp();
    let min_expiry = now + 60;   // 至少 1 分钟后到期
    let max_expiry = now + 180;  // 最多 3 分钟后到期

    let mut out = Vec::new();
    let limit = 100u32;
    let mut offset = 0u32;

    loop {
        let events = fetch_events_page(limit, offset).await?;
        if events.is_empty() {
            break;
        }
        for event in &events {
            let slug = event.get("slug").and_then(|v| v.as_str()).unwrap_or("");
            if !slug.contains("updown-5m") {
                continue;
            }
            let expiry_ts = parse_expiry_from_slug(slug)
                .or_else(|| {
                    event
                        .get("endDate")
                        .and_then(|v| v.as_str())
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.timestamp())
                });
            let Some(exp) = expiry_ts else { continue };
            if exp < min_expiry || exp > max_expiry {
                continue;
            }
            let Some(markets) = event.get("markets").and_then(|m| m.as_array()) else {
                continue;
            };
            for market in markets {
                let active = market.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
                let closed = market.get("closed").and_then(|v| v.as_bool()).unwrap_or(true);
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
                let Some(mut strike_price) = strike_price else {
                    continue;
                };
                if strike_price < 1000.0 && strike_price > 10.0 {
                    strike_price *= 1000.0;
                }
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
        if (events.len() as u32) < limit {
            break;
        }
        offset += limit;
    }
    Ok(out)
}

/// Checks if the Polymarket event for the given slug exists (API returns 200 with events).
/// Returns Ok(false) on 404 or empty response so caller can retry later.
pub async fn check_event_exists(slug: &str) -> Result<bool, Box<dyn Error>> {
    let url = format!("https://gamma-api.polymarket.com/events?slug={slug}");
    let resp = get_client().get(&url).send().await?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(false);
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        warn!(
            "[Radar] Gamma API 403 for slug '{}', treating as unavailable.",
            slug
        );
        return Ok(false);
    }
    let payload: Value = resp.error_for_status()?.json().await?;
    let events = payload.as_array().ok_or("gamma events response is not an array")?;
    Ok(!events.is_empty())
}
