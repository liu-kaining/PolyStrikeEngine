use std::error::Error;
use std::sync::OnceLock;

use chrono::DateTime;
use reqwest::Client;
use serde_json::Value;
use tracing::{info, warn};

use crate::models::btc_price;

const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// Chainlink 与 Binance 的基差校准：final_strike = binance_strike + BASIS_ADJUSTMENT（默认 0）。
fn basis_adjustment() -> f64 {
    std::env::var("BASIS_ADJUSTMENT")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0)
}

#[derive(Debug, Clone)]
pub struct MarketInfo {
    pub token_id: String,
    pub strike_price: f64,
    pub expiry_timestamp: i64,
    /// 是否由“开盘价”补位得到的 strike（相对价盘口）
    pub is_relative_strike: bool,
    /// 当 is_relative_strike 时，用于取整点价格的 target_timestamp（便于日志显示 16:10:00）
    pub strike_timestamp: Option<i64>,
    /// 已加上的基差校准值（Chainlink - Binance），用于日志显示
    pub basis_adjustment: f64,
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

/// 提取数字（忽略逗号），用于 groupItemTitle 等。先去掉逗号再解析。
fn parse_number_from_text(text: &str) -> Option<f64> {
    let normalized = text.replace(',', "");
    let mut started = false;
    let mut buf = String::new();
    for ch in normalized.chars() {
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

/// 从 5 分钟市场描述中提取 strike：e.g. "Will the price of Bitcoin be above $70,540.2 at 11:35 AM ET?"
/// 先去掉逗号再匹配 $ 后的数字。
fn parse_strike_from_description(description: &str) -> Option<f64> {
    let normalized = description.replace(',', "");
    let after_dollar = normalized.split('$').nth(1)?;
    let num_str: String = after_dollar
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if num_str.is_empty() {
        return None;
    }
    num_str.parse::<f64>().ok()
}

/// 万能价格提取：用 $ 后紧跟 [0-9,.]+ 的 pattern，在 title/description/question 里找第一个匹配，去逗号转 f64。
fn strike_from_dollar_pattern(market: &Value) -> Option<f64> {
    let fields = ["title", "description", "question"];
    for key in fields {
        let s = match market.get(key).and_then(|v| v.as_str()) {
            Some(s) => s,
            None => continue,
        };
        let after_dollar = match s.split('$').nth(1) {
            Some(a) => a,
            None => continue,
        };
        let num_str: String = after_dollar
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
            .collect();
        let normalized = num_str.replace(',', "");
        if normalized.is_empty() {
            continue;
        }
        if let Ok(n) = normalized.parse::<f64>() {
            return Some(n);
        }
    }
    None
}

/// Gamma API 可能返回数组或字符串形式的 JSON 数组（带转义），统一解析为 Vec<String>。
fn get_string_array_from_field(obj: &Value, field: &str) -> Option<Vec<String>> {
    let val = obj.get(field)?;
    if let Some(arr) = val.as_array() {
        let v: Vec<String> = arr
            .iter()
            .filter_map(|e| e.as_str().map(String::from))
            .collect();
        return if v.is_empty() { None } else { Some(v) };
    }
    if let Some(s) = val.as_str() {
        let parsed: Value = serde_json::from_str(s).ok()?;
        let arr = parsed.as_array()?;
        let v: Vec<String> = arr
            .iter()
            .filter_map(|e| e.as_str().map(String::from))
            .collect();
        return if v.is_empty() { None } else { Some(v) };
    }
    None
}

/// 兼容 Yes/No 与 Up/Down：Yes 或 Up 视为看涨（CALL），取对应 token 作为 YES。
/// clobTokenIds / outcomes 支持直接数组或字符串形式的 JSON 数组。
fn extract_yes_token_id(v: &Value) -> Option<String> {
    let ids: Vec<String> = get_string_array_from_field(v, "clobTokenIds")?;
    let outcomes = get_string_array_from_field(v, "outcomes").unwrap_or_default();
    for (i, out) in outcomes.iter().enumerate() {
        let lower = out.to_lowercase();
        if lower == "yes" || lower == "up" {
            if let Some(tok) = ids.get(i) {
                return Some(tok.clone());
            }
        }
    }
    // 强制绑定：找不到 Yes/Up 时认定 index 0 = YES，index 1 = NO
    ids.into_iter().next()
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
            if let Ok(json_str) = serde_json::to_string(market) {
                info!("[Radar] market raw JSON: {}", json_str);
            }

            // 强制放宽：暂时移除 active/closed 过滤，slug 匹配就进解析
            let Some(token_id) = extract_yes_token_id(market) else {
                continue;
            };

            let strike_from_api = parse_f64_field(market.get("line"))
                .or_else(|| {
                    market
                        .get("groupItemTitle")
                        .and_then(|v| v.as_str())
                        .and_then(parse_number_from_text)
                })
                .or_else(|| strike_from_dollar_pattern(market))
                .or_else(|| {
                    market
                        .get("description")
                        .and_then(|v| v.as_str())
                        .and_then(parse_strike_from_description)
                })
                .or_else(|| {
                    market
                        .get("question")
                        .and_then(|v| v.as_str())
                        .and_then(parse_strike_from_description)
                })
                .or_else(|| {
                    market
                        .get("title")
                        .and_then(|v| v.as_str())
                        .and_then(parse_strike_from_description)
                });
            let target_ts = parse_expiry_from_slug(slug);
            let mut strike_price = if let Some(s) = strike_from_api {
                s
            } else {
                let from_cache = target_ts.and_then(btc_price::get_btc_at_timestamp);
                if let Some(p) = from_cache {
                    p
                } else if let Some(ts) = target_ts {
                    btc_price::fetch_historic_price_from_binance(ts)
                        .await
                        .unwrap_or_else(btc_price::get_btc_mid)
                } else {
                    btc_price::get_btc_mid()
                }
            };
            if strike_price < 1000.0 && strike_price > 10.0 {
                strike_price *= 1000.0;
            }
            let adj = basis_adjustment();
            strike_price += adj;
            if strike_price <= 0.0 {
                warn!(
                    "[Radar] Strike price is 0, skipping market to prevent false triggers. token_id={}",
                    token_id
                );
                continue;
            }
            let is_relative_strike = strike_from_api.is_none();
            let strike_timestamp = if is_relative_strike { target_ts } else { None };

            let Some(end_date) = market.get("endDate").and_then(|v| v.as_str()) else {
                continue;
            };
            let expiry_timestamp = DateTime::parse_from_rfc3339(end_date)?.timestamp();

            out.push(MarketInfo {
                token_id,
                strike_price,
                expiry_timestamp,
                is_relative_strike,
                strike_timestamp,
                basis_adjustment: adj,
            });
        }
    }

    Ok(out)
}

/// Parses expiry timestamp from slug (e.g. "btc-updown-5m-1734567890" -> 1734567890).
/// Returns None if slug does not end with a unix timestamp.
pub fn parse_expiry_from_slug(slug: &str) -> Option<i64> {
    let last = slug.rsplit('-').next()?;
    if last.len() == 10 && last.bytes().all(|b| b.is_ascii_digit()) {
        return last.parse::<i64>().ok();
    }
    None
}

/// Fetches one page of events from Gamma (no slug filter). Used by 5m radar.
#[allow(dead_code)]
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
#[allow(dead_code)]
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
                let strike_price = parse_f64_field(market.get("line"))
                    .or_else(|| {
                        market
                            .get("groupItemTitle")
                            .and_then(|v| v.as_str())
                            .and_then(parse_number_from_text)
                    })
                    .or_else(|| {
                        market
                            .get("description")
                            .and_then(|v| v.as_str())
                            .and_then(parse_strike_from_description)
                    })
                    .or_else(|| {
                        market
                            .get("question")
                            .and_then(|v| v.as_str())
                            .and_then(parse_strike_from_description)
                    })
                    .or_else(|| {
                        market
                            .get("title")
                            .and_then(|v| v.as_str())
                            .and_then(parse_strike_from_description)
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
                let adj = basis_adjustment();
                out.push(MarketInfo {
                    token_id,
                    strike_price: strike_price + adj,
                    expiry_timestamp,
                    is_relative_strike: false,
                    strike_timestamp: None,
                    basis_adjustment: adj,
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
#[allow(dead_code)]
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
