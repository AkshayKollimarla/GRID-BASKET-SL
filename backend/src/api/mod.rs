use crate::engine::{spawn_engine, EngineHandle};
use crate::exchanges::instruments;
use crate::models::AgentConfig;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use parking_lot::RwLock;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};

pub struct AppState {
    pub engine: RwLock<Option<Arc<EngineHandle>>>,
    /// Persistent store of saved agent configs (= inactive agents that the
    /// user wants to keep around). Loaded from disk on startup, written
    /// back on every save/delete. Keyed by `AgentConfig.name`.
    pub saved_agents: RwLock<Vec<AgentConfig>>,
    /// Path to the on-disk JSON file. `agents.json` in the backend's
    /// working directory by default; override via `AGENTS_FILE` env var.
    pub agents_path: PathBuf,
}

impl AppState {
    fn load_saved_agents(path: &PathBuf) -> Vec<AgentConfig> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        match serde_json::from_str::<Vec<AgentConfig>>(&raw) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(?e, ?path, "could not parse saved agents file — starting empty");
                Vec::new()
            }
        }
    }

    fn persist(&self) {
        let agents = self.saved_agents.read().clone();
        match serde_json::to_string_pretty(&agents) {
            Ok(s) => {
                if let Err(e) = std::fs::write(&self.agents_path, s) {
                    tracing::warn!(?e, ?self.agents_path, "could not write saved-agents file");
                }
            }
            Err(e) => tracing::warn!(?e, "could not serialize saved agents"),
        }
    }
}

pub fn router() -> Router {
    let agents_path: PathBuf = std::env::var("AGENTS_FILE")
        .unwrap_or_else(|_| "agents.json".into())
        .into();
    let saved = AppState::load_saved_agents(&agents_path);
    tracing::info!(loaded = saved.len(), ?agents_path, "saved agents loaded");
    let state = Arc::new(AppState {
        engine: RwLock::new(None),
        saved_agents: RwLock::new(saved),
        agents_path,
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
        .route("/api/force_flatten", post(force_flatten))
        // Saved-agents CRUD — used by the sidebar to list inactive agents,
        // re-load their configs into the form, and delete them.
        .route("/api/agents", get(list_agents).post(save_agent))
        .route("/api/agents/:name", delete(delete_agent))
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
    // Auto-save / upsert this config in the saved-agents store so it shows
    // up in the sidebar even after stop, and so the user can re-edit it
    // without retyping every field.
    upsert_saved(&state, cfg.clone());
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

/// Replace any existing saved agent with the same name, else append.
/// Persists to disk on every change.
fn upsert_saved(state: &AppState, cfg: AgentConfig) {
    {
        let mut g = state.saved_agents.write();
        if let Some(slot) = g.iter_mut().find(|c| c.name == cfg.name) {
            *slot = cfg;
        } else {
            g.push(cfg);
        }
    }
    state.persist();
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

/// List every saved agent config (the "Inactive Agents" sidebar
/// reads this). Also tells the UI which one is currently running by
/// name so it can mark it as ACTIVE instead of INACTIVE.
async fn list_agents(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let agents = state.saved_agents.read().clone();
    let active_name = {
        let g = state.engine.read();
        g.as_ref().and_then(|eng| {
            if eng.running.load(std::sync::atomic::Ordering::Relaxed) {
                Some(eng.config.name.clone())
            } else {
                None
            }
        })
    };
    (
        StatusCode::OK,
        Json(json!({ "agents": agents, "active": active_name })),
    )
}

/// Upsert a saved agent (used by the UI's "Save" button on the form,
/// also called implicitly on Start). Body = full AgentConfig.
async fn save_agent(
    State(state): State<Arc<AppState>>,
    Json(cfg): Json<AgentConfig>,
) -> impl IntoResponse {
    let trimmed_name = cfg.name.trim().to_string();
    if trimmed_name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "name cannot be empty" })),
        );
    }
    let mut to_save = cfg;
    to_save.name = trimmed_name.clone();
    upsert_saved(&state, to_save);
    (
        StatusCode::OK,
        Json(json!({ "status": "saved", "name": trimmed_name })),
    )
}

/// Remove a saved agent from the sidebar list.
async fn delete_agent(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    {
        let mut g = state.saved_agents.write();
        let before = g.len();
        g.retain(|c| c.name != name);
        if before == g.len() {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "no saved agent with that name" })),
            );
        }
    }
    state.persist();
    (StatusCode::OK, Json(json!({ "status": "deleted", "name": name })))
}

/// Operator-triggered emergency flatten. Cancels every order, slices every
/// basket flat, runs a residual mop-up against any leftover exchange
/// position, then verifies the exchange-side position is zero. Returns
/// `{ ok, message }` so the UI can show what happened.
async fn force_flatten(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let eng_opt = { state.engine.read().clone() };
    match eng_opt {
        Some(eng) => {
            let (ok, msg) = eng.force_flatten().await;
            (
                StatusCode::OK,
                Json(json!({ "ok": ok, "message": msg })),
            )
        }
        None => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "message": "engine not started" })),
        ),
    }
}
