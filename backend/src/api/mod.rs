use crate::engine::{spawn_engine, EngineHandle};
use crate::exchanges::instruments;
use crate::models::AgentConfig;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use parking_lot::RwLock;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};

pub struct AppState {
    pub engine: RwLock<Option<Arc<EngineHandle>>>,
}

pub fn router() -> Router {
    let state = Arc::new(AppState {
        engine: RwLock::new(None),
    });
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);
    Router::new()
        .route("/api/health", get(health))
        .route("/api/config/default", get(default_config))
        .route("/api/instruments", get(instruments_handler))
        .route("/api/start", post(start))
        .route("/api/stop", post(stop))
        .route("/api/snapshot", get(snapshot))
        .route("/api/kill", post(kill))
        .route("/api/reset", post(reset))
        .with_state(state)
        .layer(cors)
}

#[derive(Debug, Deserialize)]
struct InstrumentsQuery {
    exchange: String,
}

async fn instruments_handler(Query(q): Query<InstrumentsQuery>) -> impl IntoResponse {
    let result: Result<Vec<String>, String> = match q.exchange.to_lowercase().as_str() {
        "deribit" => instruments::fetch_deribit_perps()
            .await
            .map_err(|e| e.to_string()),
        "hyperliquid" => instruments::fetch_hyperliquid_perps()
            .await
            .map_err(|e| e.to_string()),
        "mock" => Ok(vec![
            "BTC-MOCK".into(),
            "ETH-MOCK".into(),
            "SOL-MOCK".into(),
        ]),
        other => Err(format!("unknown exchange: {}", other)),
    };
    match result {
        Ok(symbols) => (StatusCode::OK, Json(json!({ "symbols": symbols }))),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e, "symbols": [] })),
        ),
    }
}

async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

async fn default_config() -> impl IntoResponse {
    Json(AgentConfig::default_demo())
}

async fn start(
    State(state): State<Arc<AppState>>,
    Json(cfg): Json<AgentConfig>,
) -> impl IntoResponse {
    {
        let g = state.engine.read();
        if let Some(eng) = g.as_ref() {
            if eng.running.load(std::sync::atomic::Ordering::Relaxed) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "engine already running" })),
                );
            }
        }
    }
    match EngineHandle::new(cfg).await {
        Ok((handle, fills_rx)) => {
            let handle = Arc::new(handle);
            spawn_engine(handle.clone(), fills_rx);
            *state.engine.write() = Some(handle);
            (StatusCode::OK, Json(json!({ "status": "started" })))
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("{}", e) })),
        ),
    }
}

async fn stop(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if let Some(eng) = state.engine.read().as_ref() {
        eng.running
            .store(false, std::sync::atomic::Ordering::Relaxed);
        eng.log_line("Engine stopped by user.".to_string());
    }
    Json(json!({ "status": "stopped" }))
}

async fn snapshot(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let eng_opt = { state.engine.read().clone() };
    match eng_opt {
        Some(eng) => Json(json!(eng.snapshot().await)).into_response(),
        None => Json(json!({ "running": false, "message": "engine not started" })).into_response(),
    }
}

async fn kill(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let eng_opt = { state.engine.read().clone() };
    if let Some(eng) = eng_opt {
        eng.kill_switch.trip("manual kill from UI".into()).await;
    }
    Json(json!({ "status": "kill_switch_tripped" }))
}

async fn reset(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if let Some(eng) = state.engine.read().as_ref() {
        eng.kill_switch.manual_reset();
    }
    Json(json!({ "status": "reset" }))
}
