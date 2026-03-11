use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;

use crate::api::{models::StartStrategyReq, strategy_registry::StrategyRegistry};
use crate::engine;
use crate::risk::{InventoryManager, MarketExposure, RiskGuard};

/// Global app state: inventory + active strategy registry (token_id -> CancellationToken).
#[derive(Clone)]
struct ApiState {
    inventory: Arc<InventoryManager>,
    strategies: Arc<StrategyRegistry>,
    risk_guard: Arc<RiskGuard>,
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
    let inventory = state.inventory.clone();
    let risk_guard = state.risk_guard.clone();
    let strategies = state.strategies.clone();
    tokio::spawn(async move {
        engine::run_sniper_task(
            token_id,
            strike_price,
            snipe_size,
            inventory,
            risk_guard,
            cancel_token,
            strategies,
        )
        .await;
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

pub async fn start_api_server(
    inventory: Arc<InventoryManager>,
    risk_guard: Arc<RiskGuard>,
    strategies: Arc<StrategyRegistry>,
    port: u16,
) {
    let app_state = ApiState {
        inventory,
        strategies,
        risk_guard,
    };
    let app = Router::new()
        .route("/status", get(status_handler))
        .route("/exposure", get(exposure_handler))
        .route("/strategy/start", post(strategy_start_handler))
        .route("/strategy/stop", post(strategy_stop_handler))
        .with_state(app_state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!("[API] SaaS gateway listening on http://{}", addr);

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[API] Failed to bind {}: {}", addr, e);
            return;
        }
    };
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("[API] Server error: {}", e);
    }
}
