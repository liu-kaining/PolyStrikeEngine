use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use futures_util::future::join_all;
use rust_decimal::prelude::ToPrimitive;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::api::strategy_registry::StrategyRegistry;
use crate::execution::poly_client::PolyClient;
use crate::models::types::OrderSide;
use crate::models::{btc_price, types::BookTicker};
use crate::oracle::poly_ws;
use crate::risk::{BudgetReservation, InventoryManager, RiskGuard};
use crate::strategy::SniperStrategy;

// Default engine parameters (can be overridden via ENV at runtime).
const ENTER_THRESHOLD_DEFAULT: f64 = 0.001;   // 0.1% edge to enter
const EXIT_THRESHOLD_DEFAULT: f64 = 0.002;    // 0.2% edge to exit
const MIN_PROFIT_DEFAULT: f64 = 0.0005;       // 0.05% profit to take
const ORDER_TIMEOUT_SECS_DEFAULT: u64 = 5;
const PENDING_WATCHDOG_SECS_DEFAULT: u64 = 8;
const ORDER_FAIL_COOLDOWN_SECS_DEFAULT: u64 = 2;
const EDGE_MONITOR_INTERVAL_DEFAULT: u64 = 5000;
/// 买价硬上限：超过此价格绝不开枪，防止高位接盘（可 ENV MAX_BUY_PRICE 覆盖，默认 0.90）
const MAX_BUY_PRICE_DEFAULT: f64 = 0.90;
/// 相对价/开盘价盘口的基准价差保护：在此类市场上自动提高 ENTER_THRESHOLD（默认 +0.001）
const RELATIVE_MARKET_OFFSET_THRESHOLD_DEFAULT: f64 = 0.001;
/// 超过此金额（USD）的买单拆成多笔小单（冰山委托）
const ICEBERG_THRESHOLD_USD: f64 = 5.0;
const ICEBERG_CHUNK_MIN_USD: f64 = 3.0;
const ICEBERG_CHUNK_MAX_USD: f64 = 5.0;
const ICEBERG_WINDOW_SECS: u64 = 1;

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

fn enter_threshold() -> f64 {
    env_f64("ENTER_THRESHOLD", ENTER_THRESHOLD_DEFAULT)
}

fn exit_threshold() -> f64 {
    env_f64("EXIT_THRESHOLD", EXIT_THRESHOLD_DEFAULT)
}

fn min_profit() -> f64 {
    env_f64("MIN_PROFIT", MIN_PROFIT_DEFAULT)
}

fn order_timeout_secs() -> u64 {
    env_u64("ORDER_TIMEOUT_SECS", ORDER_TIMEOUT_SECS_DEFAULT)
}

fn pending_watchdog_secs() -> u64 {
    env_u64("PENDING_WATCHDOG_SECS", PENDING_WATCHDOG_SECS_DEFAULT)
}

fn order_fail_cooldown_secs() -> u64 {
    env_u64("ORDER_FAIL_COOLDOWN_SECS", ORDER_FAIL_COOLDOWN_SECS_DEFAULT)
}

fn edge_monitor_interval() -> u64 {
    env_u64("EDGE_MONITOR_INTERVAL", EDGE_MONITOR_INTERVAL_DEFAULT)
}

fn max_buy_price() -> f64 {
    env_f64("MAX_BUY_PRICE", MAX_BUY_PRICE_DEFAULT)
}

fn relative_market_offset_threshold() -> f64 {
    env_f64("RELATIVE_MARKET_OFFSET_THRESHOLD", RELATIVE_MARKET_OFFSET_THRESHOLD_DEFAULT)
}

/// 将总 size 按 $3～$5 一档拆成多笔（冰山委托），返回每笔的 size。
fn iceberg_chunk_sizes(total_size: f64, price: f64) -> Vec<f64> {
    if price <= 0.0 || total_size <= 0.0 {
        return vec![];
    }
    let total_usd = total_size * price;
    if total_usd <= ICEBERG_THRESHOLD_USD {
        return vec![total_size];
    }
    let n = (total_usd / ICEBERG_CHUNK_MAX_USD).ceil() as usize;
    let n = n.max(1);
    let chunk_usd = total_usd / (n as f64);
    let n = if chunk_usd < ICEBERG_CHUNK_MIN_USD {
        (total_usd / ICEBERG_CHUNK_MIN_USD).ceil() as usize
    } else {
        n
    };
    let n = n.max(1);
    let chunk_size = total_size / (n as f64);
    let mut sizes = vec![chunk_size; n];
    // 最后一块微调以保证 sum = total_size
    let sum: f64 = sizes.iter().sum();
    if let Some(last) = sizes.last_mut() {
        *last += total_size - sum;
    }
    sizes.retain(|s| *s > 0.0);
    if sizes.is_empty() {
        vec![total_size]
    } else {
        sizes
    }
}

#[derive(Debug, Clone)]
struct ExecutedOrderUpdate {
    token_id: String,
    side: OrderSide,
    price: f64,
    size: f64,
    synced_position: f64,
    edge_hint: f64,
    success: bool,
    reservation: Option<Arc<BudgetReservation>>,
}

struct StrategyGuardDrop {
    strategies: Arc<StrategyRegistry>,
    market_key: String,
}

impl Drop for StrategyGuardDrop {
    fn drop(&mut self) {
        self.strategies.deregister(&self.market_key);
    }
}

enum PositionState {
    Empty,
    PendingBuy { #[allow(dead_code)] token_id: String, entered_at: Instant },
    Holding { token_id: String, buy_price: f64, amount: f64 },
    PendingSell {
        token_id: String,
        buy_price: f64,
        amount: f64,
        amount_trying_to_sell: f64,
        entered_at: Instant,
    },
}

/// Runs a single sniper loop for one market (both Up and Down tokens).
/// Listens to both orderbooks and buys whichever side has the best edge.
pub async fn run_sniper_task(
    yes_token_id: String,
    no_token_id: String,
    strike_price: f64,
    snipe_size: f64,
    expiry_timestamp: i64,
    volatility: f64,
    is_relative_strike: bool,
    strike_timestamp: Option<i64>,
    basis_adjustment: f64,
    binance_kline_time: Option<i64>,
    binance_kline_close: Option<f64>,
    dry_run: bool,
    inventory: Arc<InventoryManager>,
    risk_guard: Arc<RiskGuard>,
    poly_client: Option<Arc<PolyClient>>,
    binance_rx: watch::Receiver<BookTicker>,
    cancel_token: CancellationToken,
    strategies: Arc<StrategyRegistry>,
) {
    let market_key = yes_token_id.clone();
    let _guard = StrategyGuardDrop {
        strategies: strategies.clone(),
        market_key: market_key.clone(),
    };

    let sniper_strategy =
        SniperStrategy::new(strike_price, snipe_size, expiry_timestamp, volatility);

    let mut binance_rx = binance_rx;

    let mut poly_yes_rx = match poly_ws::spawn_poly_orderbook_stream(&yes_token_id) {
        Ok(rx) => rx,
        Err(e) => {
            error!("[Engine] {} Poly stream failed: {}", yes_token_id, e);
            return;
        }
    };
    let mut poly_no_rx = match poly_ws::spawn_poly_orderbook_stream(&no_token_id) {
        Ok(rx) => rx,
        Err(e) => {
            error!("[Engine] {} Poly stream failed: {}", no_token_id, e);
            return;
        }
    };

    let mut last_price = 0.0_f64;
    let mut first_tick = true;
    let wait_duration = Duration::from_secs(5);
    let pos_yes = sync_current_position(yes_token_id.as_str(), inventory.as_ref(), poly_client.as_deref()).await;
    let pos_no = sync_current_position(no_token_id.as_str(), inventory.as_ref(), poly_client.as_deref()).await;

    let mut state = if pos_yes > 0.0 {
        PositionState::Holding { token_id: yes_token_id.clone(), buy_price: 0.0, amount: pos_yes }
    } else if pos_no > 0.0 {
        PositionState::Holding { token_id: no_token_id.clone(), buy_price: 0.0, amount: pos_no }
    } else {
        PositionState::Empty
    };
    #[allow(unused_assignments)]
    let mut current_position = 0.0_f64;

    let (fill_tx, mut fill_rx) = mpsc::channel::<ExecutedOrderUpdate>(1024);
    let mut last_order_fail: Option<Instant> = None;
    let mut tick_count: u64 = 0;

    let relative_tag = if is_relative_strike {
        strike_timestamp
            .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
            .map(|dt| format!(" [Relative Strike from {}]", dt.format("%H:%M:%S")))
            .unwrap_or_else(|| " [Relative Strike]".to_string())
    } else {
        String::new()
    };
    if basis_adjustment != 0.0 {
        info!(
            "[Engine] 🎯 Monitoring K={:.0} (Binance {:.0} + Adjustment {:.1}){}",
            strike_price,
            strike_price - basis_adjustment,
            basis_adjustment,
            relative_tag
        );
    } else {
        info!("[Engine] 🎯 Monitoring K={:.0}{}", strike_price, relative_tag);
    }
    if let (Some(kline_sec), Some(close)) = (binance_kline_time, binance_kline_close) {
        let time_str = chrono::DateTime::from_timestamp(kline_sec, 0)
            .map(|dt| dt.format("%H:%M:%S").to_string())
            .unwrap_or_else(|| kline_sec.to_string());
        debug!(
            "[Debug] Binance 1m Kline time: {}, Close: {:.2}",
            time_str, close
        );
    }

    loop {
        tokio::select! {
            Some(update) = fill_rx.recv() => {
                current_position = update.synced_position.max(0.0);

                state = match (state, update.side, update.success) {
                    (PositionState::PendingBuy { .. }, OrderSide::Buy, true)
                    | (PositionState::Empty, OrderSide::Buy, true) => {
                        if let Some(ref res) = update.reservation {
                            risk_guard.refund_budget_delta(res, update.price, update.size);
                        }
                        let total_pnl = inventory.get_total_realized_pnl();
                        risk_guard.update_circuit_breaker(total_pnl);
                        last_order_fail = None;
                        let side_tag = if update.token_id == yes_token_id { "Up" } else { "Down" };
                        info!(
                            "[HFT-ENTER] ⚡ K={:.0} | {} | Bought at {:.3} | Edge:{:.4} | Size:{:.2}",
                            strike_price, side_tag, update.price, update.edge_hint, update.size,
                        );
                        PositionState::Holding {
                            token_id: update.token_id,
                            buy_price: update.price,
                            amount: update.size,
                        }
                    }
                    (PositionState::PendingBuy { .. }, OrderSide::Buy, false) => {
                        if update.reservation.is_some() || update.size > 0.0 {
                            info!(
                                "[HFT-ENTER] ❌ K={:.0} | Buy failed at {:.3} | Edge:{:.4} | Cooldown {}s",
                                strike_price, update.price, update.edge_hint, order_fail_cooldown_secs(),
                            );
                        }
                        last_order_fail = Some(Instant::now());
                        risk_guard.release_token_buy_slot(&market_key);
                        PositionState::Empty
                    }
                    (PositionState::PendingSell { token_id, buy_price, .. }, OrderSide::Sell, true) => {
                        let profit = update.price - buy_price;
                        let total_pnl = inventory.get_total_realized_pnl();
                        risk_guard.update_circuit_breaker(total_pnl);
                        last_order_fail = None;
                        let side_tag = if token_id == yes_token_id { "Up" } else { "Down" };
                        info!(
                            "[HFT-EXIT] 💰 K={:.0} | {} | Sold at {:.3} | Buy:{:.3} | Profit:{:.4}",
                            strike_price, side_tag, update.price, buy_price, profit,
                        );
                        if current_position > 0.0 {
                            PositionState::Holding { token_id, buy_price, amount: current_position }
                        } else {
                            PositionState::Empty
                        }
                    }
                    (PositionState::PendingSell { token_id, buy_price, amount, amount_trying_to_sell, .. }, OrderSide::Sell, false) => {
                        info!(
                            "[HFT-EXIT] ❌ K={:.0} | Sell failed at {:.3} | Buy:{:.3} | Size:{:.2} | Cooldown {}s",
                            strike_price, update.price, buy_price, amount_trying_to_sell, order_fail_cooldown_secs(),
                        );
                        last_order_fail = Some(Instant::now());
                        PositionState::Holding { token_id, buy_price, amount }
                    }
                    (s, _, _) => s,
                };
            }

            _ = cancel_token.cancelled() => {
                info!("[Engine] Market K={:.0} stopping — cancelling open orders...", strike_price);
                if let Some(ref client) = poly_client {
                    let _ = client.cancel_all_orders(&yes_token_id).await;
                    let _ = client.cancel_all_orders(&no_token_id).await;
                }
                break;
            }

            _ = tokio::time::sleep(wait_duration) => {
                let ticker = *binance_rx.borrow();
                let binance_mid = (ticker.bid_price + ticker.ask_price) * 0.5;
                if binance_mid > 0.0 {
                    btc_price::set_btc_mid(binance_mid);
                }

                if first_tick {
                    first_tick = false;
                    info!("[Engine] K={:.0} waiting for Poly WS first tick...", strike_price);
                }
                match &state {
                    PositionState::PendingBuy { entered_at, .. } => {
                        if entered_at.elapsed() > Duration::from_secs(pending_watchdog_secs()) {
                            warn!(
                                "[Watchdog] ⏰ K={:.0} PendingBuy stuck for {}s, force-unlocking to Empty",
                                strike_price, entered_at.elapsed().as_secs()
                            );
                            risk_guard.release_token_buy_slot(&market_key);
                            state = PositionState::Empty;
                        }
                    }
                    PositionState::PendingSell { token_id, buy_price, amount, entered_at, .. } => {
                        if entered_at.elapsed() > Duration::from_secs(pending_watchdog_secs()) {
                            warn!(
                                "[Watchdog] ⏰ K={:.0} PendingSell stuck for {}s, force-unlocking to Holding",
                                strike_price, entered_at.elapsed().as_secs()
                            );
                            state = PositionState::Holding {
                                token_id: token_id.clone(),
                                buy_price: *buy_price,
                                amount: *amount,
                            };
                        }
                    }
                    _ => {}
                }
            }

            changed = binance_rx.changed() => {
                if changed.is_err() {
                    error!("[Engine] K={:.0} Binance WS sender dropped.", strike_price);
                    break;
                }
                let ticker = *binance_rx.borrow();
                let binance_mid = (ticker.bid_price + ticker.ask_price) * 0.5;
                btc_price::set_btc_mid(binance_mid);
                if first_tick {
                    first_tick = false;
                }
                if (binance_mid - last_price).abs() > f64::EPSILON {
                    last_price = binance_mid;
                }
                let book_yes = *poly_yes_rx.borrow();
                let book_no = *poly_no_rx.borrow();
                current_position = match &state {
                    PositionState::Holding { token_id, .. } if token_id == &yes_token_id => {
                        sync_current_position(&yes_token_id, inventory.as_ref(), poly_client.as_deref()).await
                    }
                    PositionState::Holding { token_id, .. } if token_id == &no_token_id => {
                        sync_current_position(&no_token_id, inventory.as_ref(), poly_client.as_deref()).await
                    }
                    _ => 0.0,
                };

                tick_count += 1;
                evaluate_and_act(
                    &mut state, &sniper_strategy, binance_mid,
                    book_yes.best_ask, book_yes.best_bid,
                    book_no.best_ask, book_no.best_bid,
                    current_position, strike_price, is_relative_strike, dry_run,
                    &yes_token_id, &no_token_id, &market_key, &risk_guard, &inventory, &poly_client, &fill_tx,
                    &last_order_fail, tick_count,
                ).await;
            }

            changed = poly_yes_rx.changed() => {
                if changed.is_err() {
                    error!("[Engine] K={:.0} Poly Yes WS dropped.", strike_price);
                    break;
                }
                let ticker = *binance_rx.borrow();
                let binance_mid = (ticker.bid_price + ticker.ask_price) * 0.5;
                let book_yes = *poly_yes_rx.borrow();
                let book_no = *poly_no_rx.borrow();
                current_position = match &state {
                    PositionState::Holding { token_id, .. } if token_id == &yes_token_id => {
                        sync_current_position(&yes_token_id, inventory.as_ref(), poly_client.as_deref()).await
                    }
                    PositionState::Holding { token_id, .. } if token_id == &no_token_id => {
                        sync_current_position(&no_token_id, inventory.as_ref(), poly_client.as_deref()).await
                    }
                    _ => 0.0,
                };

                tick_count += 1;
                evaluate_and_act(
                    &mut state, &sniper_strategy, binance_mid,
                    book_yes.best_ask, book_yes.best_bid,
                    book_no.best_ask, book_no.best_bid,
                    current_position, strike_price, is_relative_strike, dry_run,
                    &yes_token_id, &no_token_id, &market_key, &risk_guard, &inventory, &poly_client, &fill_tx,
                    &last_order_fail, tick_count,
                ).await;
            }

            changed = poly_no_rx.changed() => {
                if changed.is_err() {
                    error!("[Engine] K={:.0} Poly No WS dropped.", strike_price);
                    break;
                }
                let ticker = *binance_rx.borrow();
                let binance_mid = (ticker.bid_price + ticker.ask_price) * 0.5;
                let book_yes = *poly_yes_rx.borrow();
                let book_no = *poly_no_rx.borrow();
                current_position = match &state {
                    PositionState::Holding { token_id, .. } if token_id == &yes_token_id => {
                        sync_current_position(&yes_token_id, inventory.as_ref(), poly_client.as_deref()).await
                    }
                    PositionState::Holding { token_id, .. } if token_id == &no_token_id => {
                        sync_current_position(&no_token_id, inventory.as_ref(), poly_client.as_deref()).await
                    }
                    _ => 0.0,
                };

                tick_count += 1;
                evaluate_and_act(
                    &mut state, &sniper_strategy, binance_mid,
                    book_yes.best_ask, book_yes.best_bid,
                    book_no.best_ask, book_no.best_bid,
                    current_position, strike_price, is_relative_strike, dry_run,
                    &yes_token_id, &no_token_id, &market_key, &risk_guard, &inventory, &poly_client, &fill_tx,
                    &last_order_fail, tick_count,
                ).await;
            }
        }
    }
}

fn enter_threshold_effective(is_relative_strike: bool) -> f64 {
    let base = enter_threshold();
    if is_relative_strike {
        base + relative_market_offset_threshold()
    } else {
        base
    }
}

/// Dual-side edges: fv_yes (binary call), fv_no = 1 - fv_yes; edge_yes, edge_no (buy), sell_edge_yes, sell_edge_no.
fn compute_edges_dual(
    strategy: &SniperStrategy,
    binance_mid: f64,
    best_ask_yes: f64,
    best_bid_yes: f64,
    best_ask_no: f64,
    best_bid_no: f64,
) -> (f64, f64, f64, f64, f64, f64) {
    let now = Utc::now().timestamp();
    let remaining_seconds = (strategy.expiry_timestamp - now).max(0);
    let time_to_expiry_years = remaining_seconds as f64 / (365.0 * 24.0 * 3600.0);
    let fv_yes = SniperStrategy::calculate_binary_call_price(
        binance_mid,
        strategy.strike_price,
        time_to_expiry_years,
        strategy.volatility,
    );
    let fv_no = 1.0 - fv_yes;
    let edge_yes = if best_ask_yes > 0.0 { fv_yes - best_ask_yes } else { f64::MIN };
    let edge_no = if best_ask_no > 0.0 { fv_no - best_ask_no } else { f64::MIN };
    let sell_edge_yes = if best_bid_yes > 0.0 { best_bid_yes - fv_yes } else { f64::MIN };
    let sell_edge_no = if best_bid_no > 0.0 { best_bid_no - fv_no } else { f64::MIN };
    (fv_yes, fv_no, edge_yes, edge_no, sell_edge_yes, sell_edge_no)
}

#[allow(clippy::too_many_arguments)]
async fn evaluate_and_act(
    state: &mut PositionState,
    sniper_strategy: &SniperStrategy,
    binance_mid: f64,
    best_ask_yes: f64,
    best_bid_yes: f64,
    best_ask_no: f64,
    best_bid_no: f64,
    current_position: f64,
    strike_price: f64,
    is_relative_strike: bool,
    dry_run: bool,
    yes_token_id: &str,
    no_token_id: &str,
    market_key: &str,
    risk_guard: &Arc<RiskGuard>,
    inventory: &Arc<InventoryManager>,
    poly_client: &Option<Arc<PolyClient>>,
    fill_tx: &mpsc::Sender<ExecutedOrderUpdate>,
    last_order_fail: &Option<Instant>,
    tick_count: u64,
) {
    if matches!(state, PositionState::Empty) {
        let max_budget = risk_guard.max_budget_f64();
        if max_budget > 0.0 {
            let spent = risk_guard.current_spent();
            let available_cash = (max_budget - spent.to_f64().unwrap_or(0.0)).max(0.0);
            if available_cash < 1.0 {
                if tick_count % 1000 == 0 {
                    info!(
                        "[System] Balance < $1.0, entering hibernation mode. Waiting for fills..."
                    );
                }
                return;
            }
        }
    }

    if let Some(t) = last_order_fail {
        let remaining = Duration::from_secs(order_fail_cooldown_secs()).saturating_sub(t.elapsed());
        if !remaining.is_zero() {
            if tick_count % edge_monitor_interval() == 0 {
                debug!(
                    "[Cooldown] K={:.0} | {}s remaining",
                    strike_price, remaining.as_secs()
                );
            }
            return;
        }
    }

    let (fv_yes, fv_no, edge_yes, edge_no, sell_edge_yes, sell_edge_no) =
        compute_edges_dual(sniper_strategy, binance_mid, best_ask_yes, best_bid_yes, best_ask_no, best_bid_no);

    let is_interesting = edge_yes > -0.01 || edge_no > -0.01;
    if tick_count % edge_monitor_interval() == 0 && is_interesting {
        let state_tag = match state {
            PositionState::Empty => "EMPTY",
            PositionState::PendingBuy { .. } => "P-BUY",
            PositionState::Holding { .. } => "HOLD",
            PositionState::PendingSell { .. } => "P-SELL",
        };
        let thr = enter_threshold_effective(is_relative_strike);
        info!(
            "[Monitor] K={:.0} | FV_yes:{:.4} FV_no:{:.4} | AskY:{:.3} BidY:{:.3} AskN:{:.3} BidN:{:.3} | EdgeY:{:.4} EdgeN:{:.4} | Thr:{:.3} | State:{} | BTC:{:.1}",
            strike_price, fv_yes, fv_no, best_ask_yes, best_bid_yes, best_ask_no, best_bid_no,
            edge_yes, edge_no, thr, state_tag, binance_mid,
        );
    }

    match state {
        PositionState::Empty => {
            if risk_guard.is_frozen() {
                if tick_count % edge_monitor_interval() == 0 {
                    debug!("[Skipped] K={:.0} | Reason: circuit breaker frozen", strike_price);
                }
                return;
            }

            let thr_eff = enter_threshold_effective(is_relative_strike);
            let (buy_token_id, best_ask, buy_edge, fair_val) = if edge_yes >= edge_no && edge_yes > thr_eff && best_ask_yes > 0.0 && best_ask_yes <= max_buy_price() {
                (yes_token_id, best_ask_yes, edge_yes, fv_yes)
            } else if edge_no > thr_eff && best_ask_no > 0.0 && best_ask_no <= max_buy_price() {
                (no_token_id, best_ask_no, edge_no, fv_no)
            } else {
                return;
            };

            let budget_per_trade = sniper_strategy.snipe_size;
            let shares_to_buy = (budget_per_trade / best_ask).floor();
            if shares_to_buy < 1.0 {
                if tick_count % edge_monitor_interval() == 0 {
                    debug!(
                        "[Skipped] K={:.0} | Reason: budget per trade too small | Budget:{:.4} Ask:{:.4}",
                        strike_price, budget_per_trade, best_ask,
                    );
                }
                return;
            }
            let notional = shares_to_buy * best_ask;
            if notional < 1.0 {
                if tick_count % edge_monitor_interval() == 0 {
                    debug!(
                        "[Skipped] K={:.0} | Reason: notional < $1.0 | Notional:{:.4}",
                        strike_price, notional,
                    );
                }
                return;
            }

            let yes_qty = inventory.get_exposure_qty(yes_token_id).0;
            let no_qty = inventory.get_exposure_qty(no_token_id).1;
            let (add_yes, add_no) = if buy_token_id == yes_token_id {
                (shares_to_buy, 0.0)
            } else {
                (0.0, shares_to_buy)
            };
            if risk_guard
                .check_market_exposure(yes_qty + add_yes, no_qty + add_no)
                .is_err()
            {
                if tick_count % edge_monitor_interval() == 0 {
                    debug!(
                        "[Skipped] K={:.0} | Reason: market exposure limit | yes:{:.2} no:{:.2}",
                        strike_price, yes_qty + add_yes, no_qty + add_no,
                    );
                }
                return;
            }

            if !risk_guard.try_acquire_global_buy_slot() {
                if tick_count % edge_monitor_interval() == 0 {
                    debug!(
                        "[Skipped] K={:.0} | Reason: global PendingBuy in another market",
                        strike_price
                    );
                }
                return;
            }
            if !risk_guard.try_acquire_token_buy_slot(market_key) {
                risk_guard.release_global_buy_slot();
                if tick_count % edge_monitor_interval() == 0 {
                    debug!(
                        "[Skipped] K={:.0} | Reason: market already has PendingBuy",
                        strike_price
                    );
                }
                return;
            }

            if !risk_guard.can_afford(1.0, sniper_strategy.snipe_size) {
                risk_guard.release_global_buy_slot();
                risk_guard.release_token_buy_slot(market_key);
                if tick_count % edge_monitor_interval() == 0 {
                    let spent = risk_guard.current_spent();
                    debug!(
                        "[Skipped] K={:.0} | Reason: budget | Need:{:.2} Spent:{} Max:{}",
                        strike_price, sniper_strategy.snipe_size, spent, risk_guard.max_budget_f64(),
                    );
                }
                return;
            }

            let side_tag = if buy_token_id == yes_token_id { "Up" } else { "Down" };
            info!(
                "[TRIGGER] K={:.0} | {} | Edge:{:.4} > Thr:{:.3} | Ask:{:.3} FV:{:.4} | Spawning order...",
                strike_price, side_tag, buy_edge, thr_eff, best_ask, fair_val,
            );
            let signal = crate::strategy::SnipeSignal {
                side: OrderSide::Buy,
                target_price: best_ask,
                size: shares_to_buy,
            };
            tokio::spawn(try_fire(
                risk_guard.clone(), inventory.clone(), poly_client.clone(),
                buy_token_id.to_string(), market_key.to_string(), strike_price, signal,
                dry_run, fill_tx.clone(), buy_edge,
            ));
            *state = PositionState::PendingBuy {
                token_id: buy_token_id.to_string(),
                entered_at: Instant::now(),
            };
        }
        PositionState::Holding { token_id, buy_price, amount } => {
            let (best_bid, sell_edge) = if token_id == yes_token_id {
                (best_bid_yes, sell_edge_yes)
            } else {
                (best_bid_no, sell_edge_no)
            };
            if best_bid > 0.0 {
                let profit = best_bid - *buy_price;
                if sell_edge > exit_threshold() || profit > min_profit() {
                    let sell_size = (*amount).min(current_position).max(0.0);
                    if sell_size > 0.0 {
                        let side_tag = if token_id == yes_token_id { "Up" } else { "Down" };
                        info!(
                            "[TRIGGER-SELL] K={:.0} | {} | SellEdge:{:.4} Profit:{:.4} | Bid:{:.3} BuyP:{:.3}",
                            strike_price, side_tag, sell_edge, profit, best_bid, buy_price,
                        );
                        let signal = crate::strategy::SnipeSignal {
                            side: OrderSide::Sell,
                            target_price: best_bid,
                            size: sell_size,
                        };
                        let tid = token_id.clone();
                        let bp = *buy_price;
                        let amt = *amount;
                        tokio::spawn(try_fire(
                            risk_guard.clone(), inventory.clone(), poly_client.clone(),
                            tid.clone(), market_key.to_string(), strike_price, signal,
                            dry_run, fill_tx.clone(), sell_edge,
                        ));
                        *state = PositionState::PendingSell {
                            token_id: tid,
                            buy_price: bp,
                            amount: amt,
                            amount_trying_to_sell: sell_size,
                            entered_at: Instant::now(),
                        };
                    }
                }
            }
        }
        PositionState::PendingBuy { .. } | PositionState::PendingSell { .. } => {}
    }
}

async fn sync_current_position(
    token_id: &str,
    inventory: &InventoryManager,
    poly_client: Option<&PolyClient>,
) -> f64 {
    if let Some(client) = poly_client {
        match client.sync_token_position(token_id).await {
            Ok(p) => return p,
            Err(e) => {
                warn!(
                    "[Engine] {} position sync failed, falling back to local inventory: {}",
                    token_id, e
                );
            }
        }
    }
    inventory.get_net_exposure(token_id).max(0.0)
}

// INVARIANT: Every exit path MUST send an ExecutedOrderUpdate to fill_tx.

#[allow(clippy::too_many_arguments)]
async fn try_fire(
    risk_guard: Arc<RiskGuard>,
    inventory: Arc<InventoryManager>,
    poly_client: Option<Arc<PolyClient>>,
    token_id: String,
    market_key: String,
    strike_price: f64,
    signal: crate::strategy::SnipeSignal,
    dry_run: bool,
    fill_tx: mpsc::Sender<ExecutedOrderUpdate>,
    edge_hint: f64,
) {
    let fail_update = |reservation: Option<Arc<BudgetReservation>>| ExecutedOrderUpdate {
        token_id: token_id.clone(),
        side: signal.side,
        price: signal.target_price,
        size: 0.0,
        synced_position: inventory.get_net_exposure(token_id.as_str()).max(0.0),
        edge_hint,
        success: false,
        reservation,
    };

    let reservation: Option<Arc<BudgetReservation>> = if signal.side == OrderSide::Buy {
        match risk_guard.reserve_budget(signal.target_price, signal.size) {
            Ok(r) => Some(Arc::new(r)),
            Err(e) => {
                debug!("[RiskGuard] ⛔ Budget rejected (race): {}", e);
                if matches!(signal.side, OrderSide::Buy) {
                    risk_guard.release_global_buy_slot();
                    risk_guard.release_token_buy_slot(&market_key);
                }
                let _ = fill_tx.send(fail_update(None)).await;
                return;
            }
        }
    } else {
        None
    };

    if dry_run {
        info!(
            "[HFT][K={:.0}] [DRY RUN] {:?} {:.4} @ {:.4}",
            strike_price, signal.side, signal.size, signal.target_price
        );
        if let Some(ref res) = reservation {
            risk_guard.release_budget(res.as_ref().clone_for_release());
        }
        if matches!(signal.side, OrderSide::Buy) {
            risk_guard.release_global_buy_slot();
            risk_guard.release_token_buy_slot(&market_key);
        }
        let _ = fill_tx.send(fail_update(None)).await;
        return;
    }

    debug!("[FIRE][K={:.0}] Triggering snipe order: {:?}", strike_price, signal);

    let Some(client) = poly_client else {
        error!("[FIRE] PolyClient not available, releasing budget.");
        if let Some(ref res) = reservation {
            risk_guard.release_budget(res.as_ref().clone_for_release());
        }
        if matches!(signal.side, OrderSide::Buy) {
            risk_guard.release_global_buy_slot();
            risk_guard.release_token_buy_slot(&market_key);
        }
        let _ = fill_tx.send(fail_update(None)).await;
        return;
    };

    let client = client.clone();
    let token_id_clone = token_id.clone();
    let inv = inventory.clone();

    let before = client
        .sync_token_position(token_id_clone.as_str())
        .await
        .unwrap_or_else(|_| inv.get_net_exposure(token_id_clone.as_str()).max(0.0));

    let total_usd = signal.size * signal.target_price;
    let iceberg = signal.side == OrderSide::Buy && total_usd > ICEBERG_THRESHOLD_USD;
    let place_ok = if iceberg {
        let chunks = iceberg_chunk_sizes(signal.size, signal.target_price);
        let futures: Vec<_> = chunks
            .into_iter()
            .map(|sz| {
                let c = client.clone();
                let t = token_id_clone.clone();
                async move {
                    c.execute_snipe_order(&t, signal.side, signal.target_price, sz)
                        .await
                }
            })
            .collect();
        match tokio::time::timeout(
            Duration::from_secs(ICEBERG_WINDOW_SECS),
            join_all(futures),
        )
        .await
        {
            Ok(results) => {
                let ok_count = results.iter().filter(|r| r.is_ok()).count();
                let err_count = results.len() - ok_count;
                if err_count > 0 {
                    debug!(
                        "[FIRE][K={:.0}] Iceberg: {} ok, {} failed",
                        strike_price, ok_count, err_count
                    );
                }
                ok_count > 0
            }
            Err(_) => {
                warn!(
                    "[HFT-WARN] ⚠️ 冰山委托在 {}s 内未全部返回。 K={:.0}",
                    ICEBERG_WINDOW_SECS, strike_price
                );
                false
            }
        }
    } else {
        match tokio::time::timeout(
            Duration::from_secs(order_timeout_secs()),
            client.execute_snipe_order(
                token_id_clone.as_str(),
                signal.side,
                signal.target_price,
                signal.size,
            ),
        )
        .await
        {
            Ok(Ok(_)) => true,
            Ok(Err(e)) => {
                error!(
                    "[FIRE][K={:.0}] Order failed: {:#?}, releasing budget",
                    strike_price, e
                );
                false
            }
            Err(_) => {
                warn!(
                    "[HFT-WARN] ⚠️ execute_snipe_order 在 {}s 内未返回。 side={:?}, K={:.0}",
                    order_timeout_secs(),
                    signal.side,
                    strike_price
                );
                false
            }
        }
    };

    if !place_ok {
        if let Some(ref res) = reservation {
            risk_guard.release_budget(res.as_ref().clone_for_release());
        }
        if matches!(signal.side, OrderSide::Buy) {
            risk_guard.release_global_buy_slot();
            risk_guard.release_token_buy_slot(&market_key);
        }
        let _ = fill_tx.send(fail_update(None)).await;
        return;
    }

    info!(
        "[FIRE][K={:.0}] Order(s) placed (iceberg={})",
        strike_price, iceberg
    );

    // High-frequency mode: wait briefly for a fill snapshot. If position
    // hasn't changed after ~1.5s, treat as no-fill and free the global BUY slot.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let after = client
        .sync_token_position(token_id_clone.as_str())
        .await
        .unwrap_or(before);

    let actual_filled = match signal.side {
        OrderSide::Buy => (after - before).max(0.0),
        OrderSide::Sell => (before - after).max(0.0),
    };

    if actual_filled <= f64::EPSILON {
        info!(
            "[HFT-WARN] ⚠️ 订单超时或未成交，状态机已安全回滚锁。 side={:?}, K={:.0}",
            signal.side, strike_price
        );
        if let Some(ref res) = reservation {
            risk_guard.release_budget(res.as_ref().clone_for_release());
        }
        if matches!(signal.side, OrderSide::Buy) {
            risk_guard.release_global_buy_slot();
            risk_guard.release_token_buy_slot(&market_key);
        }
        let _ = fill_tx.send(ExecutedOrderUpdate {
            token_id: token_id.clone(),
            side: signal.side,
            price: signal.target_price,
            size: 0.0,
            synced_position: after.max(0.0),
            edge_hint,
            success: false,
            reservation: None,
        })
        .await;
        return;
    }

    inventory.apply_fill(
        token_id.as_str(),
        true,
        signal.side,
        actual_filled,
        signal.target_price,
    );

    if matches!(signal.side, OrderSide::Sell) {
        risk_guard.refund_budget_on_sell(signal.target_price, actual_filled);
    }

    let _ = fill_tx
        .send(ExecutedOrderUpdate {
            token_id: token_id.clone(),
            side: signal.side,
            price: signal.target_price,
            size: actual_filled,
            synced_position: after.max(0.0),
            edge_hint,
            success: true,
            reservation: reservation.clone(),
        })
        .await;
    if matches!(signal.side, OrderSide::Buy) {
        risk_guard.release_global_buy_slot();
        risk_guard.release_token_buy_slot(&market_key);
    }
}
