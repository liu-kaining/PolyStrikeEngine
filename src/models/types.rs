use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct BookTicker {
    pub bid_price: f64,
    pub bid_qty: f64,
    pub ask_price: f64,
    pub ask_qty: f64,
    pub event_time_ms: u64,
}

impl Default for BookTicker {
    fn default() -> Self {
        Self {
            bid_price: 0.0,
            bid_qty: 0.0,
            ask_price: 0.0,
            ask_qty: 0.0,
            event_time_ms: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[allow(dead_code)]
pub enum OrderSide {
    Buy,
    Sell,
}
