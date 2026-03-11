use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use axum::{extract::State, routing::get, Json, Router};
use serde::Serialize;

use crate::risk::{InventoryManager, MarketExposure};

#[derive(Clone)]
struct ApiState {
    inventory: Arc<InventoryManager>,
}

#[derive(Serialize)]
struct StatusResponse {
    status: &'static str,
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

pub async fn start_api_server(inventory: Arc<InventoryManager>, port: u16) {
    let app_state = ApiState { inventory };
    let app = Router::new()
        .route("/status", get(status_handler))
        .route("/exposure", get(exposure_handler))
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
