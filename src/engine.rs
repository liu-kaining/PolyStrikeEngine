use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::api::strategy_registry::StrategyRegistry;
use crate::execution::poly_client::PolyClient;
use crate::models::types::OrderSide;
use crate::models::{btc_price, types::BookTicker};
use crate::oracle::poly_ws;
use crate::risk::{BudgetReservation, InventoryManager, RiskGuard};
use crate::strategy::SniperStrategy;

const ENTER_THRESHOLD: f64 = 0.01;
const EXIT_THRESHOLD: f64 = 0.01;
const MIN_PROFIT: f64 = 0.015;
const ORDER_TIMEOUT_SECS: u64 = 5;
const PENDING_WATCHDOG_SECS: u64 = 8;

#[derive(Debug, Clone)]
struct ExecutedOrderUpdate {
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
    token_id: String,
}

impl Drop for StrategyGuardDrop {
    fn drop(&mut self) {
        self.strategies.deregister(&self.token_id);
    }
}

enum PositionState {
    Empty,
    PendingBuy { entered_at: Instant },
    Holding { buy_price: f64, amount: f64 },
    PendingSell {
        buy_price: f64,
        amount: f64,
        amount_trying_to_sell: f64,
        entered_at: Instant,
    },
}

/// Runs a single sniper loop for one market.
/// All expensive resources (PolyClient, Binance WS) are injected as singletons.
pub async fn run_sniper_task(
    token_id: String,
    strike_price: f64,
    snipe_size: f64,
    expiry_timestamp: i64,
    volatility: f64,
    dry_run: bool,
    inventory: Arc<InventoryManager>,
    risk_guard: Arc<RiskGuard>,
    poly_client: Option<Arc<PolyClient>>,
    binance_rx: watch::Receiver<BookTicker>,
    cancel_token: CancellationToken,
    strategies: Arc<StrategyRegistry>,
) {
    let _guard = StrategyGuardDrop {
        strategies: strategies.clone(),
        token_id: token_id.clone(),
    };

    let sniper_strategy =
        SniperStrategy::new(strike_price, snipe_size, expiry_timestamp, volatility);

    let mut binance_rx = binance_rx;

    let mut poly_rx = match poly_ws::spawn_poly_orderbook_stream(&token_id) {
        Ok(rx) => rx,
        Err(e) => {
            error!("[Engine] {} Polymarket stream failed: {}", token_id, e);
            return;
        }
    };

    let mut last_price = 0.0_f64;
    let mut first_tick = true;
    let wait_duration = Duration::from_secs(5);
    let mut current_position = sync_current_position(
        token_id.as_str(),
        inventory.as_ref(),
        poly_client.as_deref(),
    )
    .await;

    let mut state = if current_position > 0.0 {
        PositionState::Holding {
            buy_price: 0.0,
            amount: current_position,
        }
    } else {
        PositionState::Empty
    };

    let (fill_tx, mut fill_rx) = mpsc::channel::<ExecutedOrderUpdate>(1024);

    info!("[Engine] 🎯 Monitoring K={:.0}", strike_price);

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
                        info!(
                            "[HFT-ENTER] ⚡ K={:.0} | Bought at {:.3} | Edge:{:.4} | Size:{:.2}",
                            strike_price, update.price, update.edge_hint, update.size,
                        );
                        PositionState::Holding {
                            buy_price: update.price,
                            amount: update.size,
                        }
                    }
                    (PositionState::PendingBuy { .. }, OrderSide::Buy, false) => {
                        info!(
                            "[HFT-ENTER] ❌ K={:.0} | Buy failed at {:.3} | Edge:{:.4}",
                            strike_price, update.price, update.edge_hint,
                        );
                        PositionState::Empty
                    }
                    (PositionState::PendingSell { buy_price, .. }, OrderSide::Sell, true) => {
                        let profit = update.price - buy_price;
                        let total_pnl = inventory.get_total_realized_pnl();
                        risk_guard.update_circuit_breaker(total_pnl);
                        info!(
                            "[HFT-EXIT] 💰 K={:.0} | Sold at {:.3} | Buy:{:.3} | Profit:{:.4}",
                            strike_price, update.price, buy_price, profit,
                        );
                        if current_position > 0.0 {
                            PositionState::Holding { buy_price, amount: current_position }
                        } else {
                            PositionState::Empty
                        }
                    }
                    (PositionState::PendingSell { buy_price, amount, amount_trying_to_sell, .. }, OrderSide::Sell, false) => {
                        info!(
                            "[HFT-EXIT] ❌ K={:.0} | Sell failed at {:.3} | Buy:{:.3} | Size:{:.2}",
                            strike_price, update.price, buy_price, amount_trying_to_sell,
                        );
                        PositionState::Holding { buy_price, amount }
                    }
                    (s, _, _) => s,
                };
            }

            _ = cancel_token.cancelled() => {
                info!("[Engine] Strategy for {} stopping — cancelling open orders...", token_id);
                // Best-effort cancel all open orders for this token before exiting
                if let Some(ref client) = poly_client {
                    if let Err(e) = client.cancel_all_orders(&token_id).await {
                        warn!("[Engine] {} cancel_all_orders failed: {}", token_id, e);
                    } else {
                        info!("[Engine] {} open orders cancelled.", token_id);
                    }
                }
                break;
            }

            _ = tokio::time::sleep(wait_duration) => {
                if first_tick {
                    info!("[Engine] {} Still waiting for first tick...", token_id);
                }
                match &state {
                    PositionState::PendingBuy { entered_at } => {
                        if entered_at.elapsed() > Duration::from_secs(PENDING_WATCHDOG_SECS) {
                            warn!(
                                "[Watchdog] ⏰ K={:.0} PendingBuy stuck for {}s, force-unlocking to Empty",
                                strike_price, entered_at.elapsed().as_secs()
                            );
                            state = PositionState::Empty;
                        }
                    }
                    PositionState::PendingSell { buy_price, amount, entered_at, .. } => {
                        if entered_at.elapsed() > Duration::from_secs(PENDING_WATCHDOG_SECS) {
                            warn!(
                                "[Watchdog] ⏰ K={:.0} PendingSell stuck for {}s, force-unlocking to Holding",
                                strike_price, entered_at.elapsed().as_secs()
                            );
                            state = PositionState::Holding {
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
                    error!("[Engine] {} Binance WS sender dropped, suspending task.", token_id);
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
                let poly_book = *poly_rx.borrow();

                evaluate_and_act(
                    &mut state, &sniper_strategy, binance_mid,
                    poly_book.best_ask, poly_book.best_bid,
                    current_position, strike_price, dry_run,
                    &token_id, &risk_guard, &inventory, &poly_client, &fill_tx,
                ).await;
            }

            changed = poly_rx.changed() => {
                if changed.is_err() {
                    error!("[Engine] {} Poly WS sender dropped, suspending task.", token_id);
                    break;
                }
                let ticker = *binance_rx.borrow();
                let binance_mid = (ticker.bid_price + ticker.ask_price) * 0.5;
                btc_price::set_btc_mid(binance_mid);
                let poly_book = *poly_rx.borrow();

                evaluate_and_act(
                    &mut state, &sniper_strategy, binance_mid,
                    poly_book.best_ask, poly_book.best_bid,
                    current_position, strike_price, dry_run,
                    &token_id, &risk_guard, &inventory, &poly_client, &fill_tx,
                ).await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn evaluate_and_act(
    state: &mut PositionState,
    sniper_strategy: &SniperStrategy,
    binance_mid: f64,
    best_ask: f64,
    best_bid: f64,
    current_position: f64,
    strike_price: f64,
    dry_run: bool,
    token_id: &str,
    risk_guard: &Arc<RiskGuard>,
    inventory: &Arc<InventoryManager>,
    poly_client: &Option<Arc<PolyClient>>,
    fill_tx: &mpsc::Sender<ExecutedOrderUpdate>,
) {
    let (_fair_value, buy_edge, sell_edge) =
        compute_edges(sniper_strategy, binance_mid, best_ask, best_bid);

    match state {
        PositionState::Empty => {
            if risk_guard.is_frozen() {
                return;
            }

            let (yes_qty, no_qty) = inventory.get_exposure_qty(token_id);
            if risk_guard
                .check_market_exposure(yes_qty + sniper_strategy.snipe_size, no_qty)
                .is_err()
            {
                return;
            }

            if best_ask > 0.0 && buy_edge > ENTER_THRESHOLD {
                let signal = crate::strategy::SnipeSignal {
                    side: OrderSide::Buy,
                    target_price: best_ask,
                    size: sniper_strategy.snipe_size,
                };
                tokio::spawn(try_fire(
                    risk_guard.clone(), inventory.clone(), poly_client.clone(),
                    token_id.to_string(), strike_price, signal,
                    dry_run, fill_tx.clone(), buy_edge,
                ));
                *state = PositionState::PendingBuy { entered_at: Instant::now() };
            }
        }
        PositionState::Holding { buy_price, amount } => {
            if best_bid > 0.0 {
                let profit = best_bid - *buy_price;
                if sell_edge > EXIT_THRESHOLD || profit > MIN_PROFIT {
                    let sell_size = (*amount).min(current_position).max(0.0);
                    if sell_size > 0.0 {
                        let signal = crate::strategy::SnipeSignal {
                            side: OrderSide::Sell,
                            target_price: best_bid,
                            size: sell_size,
                        };
                        let bp = *buy_price;
                        let amt = *amount;
                        tokio::spawn(try_fire(
                            risk_guard.clone(), inventory.clone(), poly_client.clone(),
                            token_id.to_string(), strike_price, signal,
                            dry_run, fill_tx.clone(), sell_edge,
                        ));
                        *state = PositionState::PendingSell {
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

fn compute_edges(
    strategy: &SniperStrategy,
    binance_mid: f64,
    best_ask: f64,
    best_bid: f64,
) -> (f64, f64, f64) {
    let now = Utc::now().timestamp();
    let remaining_seconds = (strategy.expiry_timestamp - now).max(0);
    let time_to_expiry_years = remaining_seconds as f64 / (365.0 * 24.0 * 3600.0);
    let fair_value = SniperStrategy::calculate_binary_call_price(
        binance_mid,
        strategy.strike_price,
        time_to_expiry_years,
        strategy.volatility,
    );
    let buy_edge = if best_ask > 0.0 { fair_value - best_ask } else { f64::MIN };
    let sell_edge = if best_bid > 0.0 { best_bid - fair_value } else { f64::MIN };
    (fair_value, buy_edge, sell_edge)
}

// INVARIANT: Every exit path MUST send an ExecutedOrderUpdate to fill_tx.

#[allow(clippy::too_many_arguments)]
async fn try_fire(
    risk_guard: Arc<RiskGuard>,
    inventory: Arc<InventoryManager>,
    poly_client: Option<Arc<PolyClient>>,
    token_id: String,
    strike_price: f64,
    signal: crate::strategy::SnipeSignal,
    dry_run: bool,
    fill_tx: mpsc::Sender<ExecutedOrderUpdate>,
    edge_hint: f64,
) {
    let fail_update = |reservation: Option<Arc<BudgetReservation>>| ExecutedOrderUpdate {
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
                info!("[RiskGuard] ⛔ 触发风控拦截：超出预算或单市场敞口。 原因: {}", e);
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
        let _ = fill_tx.send(fail_update(None)).await;
        return;
    }

    info!("[FIRE][K={:.0}] Triggering snipe order: {:?}", strike_price, signal);

    let Some(client) = poly_client else {
        error!("[FIRE] PolyClient not available, releasing budget.");
        if let Some(ref res) = reservation {
            risk_guard.release_budget(res.as_ref().clone_for_release());
        }
        let _ = fill_tx.send(fail_update(None)).await;
        return;
    };

    let op = {
        let client = client.clone();
        let token_id = token_id.clone();
        let inv = inventory.clone();
        async move {
            let before = client
                .sync_token_position(token_id.as_str())
                .await
                .unwrap_or_else(|_| inv.get_net_exposure(token_id.as_str()).max(0.0));

            let order_id = client
                .execute_snipe_order(token_id.as_str(), signal.side, signal.target_price, signal.size)
                .await?;

            info!("[FIRE][K={:.0}] Order placed: {}", strike_price, order_id);

            tokio::time::sleep(Duration::from_millis(800)).await;

            let after = client
                .sync_token_position(token_id.as_str())
                .await
                .unwrap_or(before);

            Ok::<(f64, f64), anyhow::Error>((before, after))
        }
    };

    let timed = tokio::time::timeout(Duration::from_secs(ORDER_TIMEOUT_SECS), op).await;

    match timed {
        Ok(Ok((before, after))) => {
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
                let _ = fill_tx.send(ExecutedOrderUpdate {
                    side: signal.side, price: signal.target_price, size: 0.0,
                    synced_position: after.max(0.0), edge_hint, success: false, reservation: None,
                }).await;
                return;
            }

            inventory.apply_fill(token_id.as_str(), true, signal.side, actual_filled, signal.target_price);

            if matches!(signal.side, OrderSide::Sell) {
                risk_guard.refund_budget_on_sell(signal.target_price, actual_filled);
            }

            let _ = fill_tx.send(ExecutedOrderUpdate {
                side: signal.side, price: signal.target_price, size: actual_filled,
                synced_position: after.max(0.0), edge_hint, success: true,
                reservation: reservation.clone(),
            }).await;
        }
        Ok(Err(e)) => {
            error!("[FIRE][K={:.0}] Order failed: {}, releasing budget", strike_price, e);
            if let Some(ref res) = reservation {
                risk_guard.release_budget(res.as_ref().clone_for_release());
            }
            let _ = fill_tx.send(fail_update(None)).await;
        }
        Err(_) => {
            warn!(
                "[HFT-WARN] ⚠️ 下单流程在 {}s 超时内未完成，状态机将回滚。 side={:?}, K={:.0}",
                ORDER_TIMEOUT_SECS, signal.side, strike_price
            );
            if let Some(ref res) = reservation {
                risk_guard.release_budget(res.as_ref().clone_for_release());
            }
            let _ = fill_tx.send(fail_update(None)).await;
        }
    }
}
