use std::sync::atomic::{AtomicU64, Ordering};

/// Global atomic BTC mid price, stored as f64 bits in an AtomicU64.
static GLOBAL_BTC_MID_BITS: AtomicU64 = AtomicU64::new(0);

/// Update the global BTC mid; used by all sniper tasks.
pub fn set_btc_mid(price: f64) {
    GLOBAL_BTC_MID_BITS.store(price.to_bits(), Ordering::Relaxed);
}

/// Read the latest BTC mid seen by any task.
pub fn get_btc_mid() -> f64 {
    f64::from_bits(GLOBAL_BTC_MID_BITS.load(Ordering::Relaxed))
}


