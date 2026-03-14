use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use tokio::sync::{watch, RwLock};
use tokio_util::sync::CancellationToken;

use crate::api::{
    models::{StartEventRadarReq, StartStrategyReq},
    strategy_registry::StrategyRegistry,
};
use crate::discovery::radar;
use crate::engine;
use crate::execution::poly_client::PolyClient;
use crate::models::types::BookTicker;
use crate::risk::{InventoryManager, MarketExposure, RiskGuard};
use tracing::{error, info};

/// Shared handle to the current event slug (set by /event/start or auto-pilot).
pub type CurrentEventSlug = Arc<RwLock<Option<String>>>;

#[derive(Clone)]
struct ApiState {
    inventory: Arc<InventoryManager>,
    strategies: Arc<StrategyRegistry>,
    risk_guard: Arc<RiskGuard>,
    poly_client: Option<Arc<PolyClient>>,
    binance_rx: watch::Receiver<BookTicker>,
    dry_run: bool,
    global_cancel: CancellationToken,
    current_event_slug: CurrentEventSlug,
}

#[derive(Serialize)]
struct StatusResponse {
    status: &'static str,
}

#[derive(Serialize)]
struct MessageResponse {
    message: String,
}

async fn status_handler() -> Json<StatusResponse> {
    Json(StatusResponse { status: "running" })
}

async fn exposure_handler(
    State(state): State<ApiState>,
) -> Json<HashMap<String, MarketExposure>> {
    let mut out = HashMap::new();
    for (market_id, exposure) in state.inventory.snapshot_all() {
        out.insert(market_id, exposure);
    }
    Json(out)
}

async fn strategy_start_handler(
    State(state): State<ApiState>,
    Json(req): Json<StartStrategyReq>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<MessageResponse>)> {
    let cancel_token = match state.strategies.try_register(req.token_id.clone()) {
        Some(t) => t,
        None => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(MessageResponse {
                    message: format!("Strategy for token {} already running", req.token_id),
                }),
            ));
        }
    };
    let token_id = req.token_id.clone();
    let strike_price = req.strike_price;
    let snipe_size = req.snipe_size;
    let expiry_timestamp = req.expiry_timestamp;
    let volatility = req.volatility;
    let dry_run = state.dry_run;
    let inventory = state.inventory.clone();
    let risk_guard = state.risk_guard.clone();
    let strategies = state.strategies.clone();
    let poly_client = state.poly_client.clone();
    let binance_rx = state.binance_rx.clone();
    let child_cancel = state.global_cancel.child_token();

    // Combine per-strategy cancel with global shutdown
    let combined_cancel = CancellationToken::new();
    let cc = combined_cancel.clone();
    let ct = cancel_token.clone();
    let gc = child_cancel.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = ct.cancelled() => { cc.cancel(); }
            _ = gc.cancelled() => { cc.cancel(); }
        }
    });

    tokio::spawn(async move {
        engine::run_sniper_task(
            token_id, strike_price, snipe_size, expiry_timestamp, volatility,
            false,
            None,
            dry_run, inventory, risk_guard, poly_client, binance_rx,
            combined_cancel, strategies,
        ).await;
    });
    Ok(Json(MessageResponse {
        message: format!("Strategy started for token {}", req.token_id),
    }))
}

async fn strategy_stop_handler(
    State(state): State<ApiState>,
    Json(req): Json<crate::api::models::StopStrategyReq>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<MessageResponse>)> {
    if !state.strategies.cancel(&req.token_id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(MessageResponse {
                message: format!("No running strategy for token {}", req.token_id),
            }),
        ));
    }
    Ok(Json(MessageResponse {
        message: format!("Strategy stopped for token {}", req.token_id),
    }))
}

/// Internal: start event radar for a slug (used by HTTP handler and auto-pilot).
/// Returns number of markets started.
pub async fn start_event_radar(
    slug: &str,
    snipe_size: f64,
    volatility: f64,
    dry_run: bool,
    inventory: Arc<InventoryManager>,
    risk_guard: Arc<RiskGuard>,
    strategies: Arc<StrategyRegistry>,
    poly_client: Option<Arc<PolyClient>>,
    binance_rx: watch::Receiver<BookTicker>,
    global_cancel: CancellationToken,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let markets = radar::fetch_event_markets(slug).await.map_err(|e| {
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, e.to_string());
        Box::new(io_err) as Box<dyn std::error::Error + Send + Sync>
    })?;
    let mut started = 0usize;
    for market in markets {
        let Some(cancel_token) = strategies.try_register(market.token_id.clone()) else {
            continue;
        };
        let token_id = market.token_id;
        let strike_price = market.strike_price;
        let expiry_timestamp = market.expiry_timestamp;
        let is_relative_strike = market.is_relative_strike;
        let strike_timestamp = market.strike_timestamp;
        let inventory = inventory.clone();
        let risk_guard = risk_guard.clone();
        let strategies = strategies.clone();
        let poly_client = poly_client.clone();
        let binance_rx = binance_rx.clone();
        let child_cancel = global_cancel.child_token();
        let combined_cancel = CancellationToken::new();
        let cc = combined_cancel.clone();
        let ct = cancel_token.clone();
        let gc = child_cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = ct.cancelled() => { cc.cancel(); }
                _ = gc.cancelled() => { cc.cancel(); }
            }
        });
        tokio::spawn(async move {
            engine::run_sniper_task(
                token_id, strike_price, snipe_size, expiry_timestamp, volatility,
                is_relative_strike, strike_timestamp,
                dry_run, inventory, risk_guard, poly_client, binance_rx,
                combined_cancel, strategies,
            )
            .await;
        });
        started += 1;
    }
    Ok(started)
}

async fn event_start_handler(
    State(state): State<ApiState>,
    Json(req): Json<StartEventRadarReq>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<MessageResponse>)> {
    let started = start_event_radar(
        &req.event_slug,
        req.snipe_size,
        req.volatility,
        state.dry_run,
        state.inventory.clone(),
        state.risk_guard.clone(),
        state.strategies.clone(),
        state.poly_client.clone(),
        state.binance_rx.clone(),
        state.global_cancel.clone(),
    )
    .await
    .map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            Json(MessageResponse {
                message: format!(
                    "Failed to discover markets for slug {}: {}",
                    req.event_slug, e
                ),
            }),
        )
    })?;
    *state.current_event_slug.write().await = Some(req.event_slug.clone());
    Ok(Json(MessageResponse {
        message: format!(
            "Event radar started for slug {}: {} markets launched.",
            req.event_slug, started
        ),
    }))
}

pub async fn start_api_server(
    inventory: Arc<InventoryManager>,
    risk_guard: Arc<RiskGuard>,
    strategies: Arc<StrategyRegistry>,
    poly_client: Option<Arc<PolyClient>>,
    binance_rx: watch::Receiver<BookTicker>,
    dry_run: bool,
    global_cancel: CancellationToken,
    current_event_slug: CurrentEventSlug,
    port: u16,
) {
    let app_state = ApiState {
        inventory,
        strategies,
        risk_guard,
        poly_client,
        binance_rx,
        dry_run,
        global_cancel,
        current_event_slug,
    };
    let app = Router::new()
        .route("/status", get(status_handler))
        .route("/exposure", get(exposure_handler))
        .route("/strategy/start", post(strategy_start_handler))
        .route("/strategy/stop", post(strategy_stop_handler))
        .route("/event/start", post(event_start_handler))
        .with_state(app_state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!("[API] SaaS gateway listening on http://{}", addr);

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            error!("[API] Failed to bind {}: {}", addr, e);
            return;
        }
    };
    if let Err(e) = axum::serve(listener, app).await {
        error!("[API] Server error: {}", e);
    }
}
