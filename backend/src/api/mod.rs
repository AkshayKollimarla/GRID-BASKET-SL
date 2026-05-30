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
use dashmap::DashMap;
use parking_lot::RwLock;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};

pub struct AppState {
    /// Currently RUNNING engines, keyed by agent name. Multi-bot —
    /// each agent name maps to its own EngineHandle. Stopping an
    /// engine removes it from this map; the saved config in
    /// `saved_agents` stays so the user can restart later.
    pub engines: DashMap<String, Arc<EngineHandle>>,
    /// Persistent store of saved agent configs (= the sidebar list).
    /// Loaded from disk on startup, written back on every save/delete.
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

    /// Return the engine for `name` ONLY if it's still flagged running.
    /// If running is false (engine self-stopped after all_killed), the
    /// stale handle is removed from the map first.
    fn live_engine(&self, name: &str) -> Option<Arc<EngineHandle>> {
        let still_running = self
            .engines
            .get(name)
            .map(|e| e.value().running.load(std::sync::atomic::Ordering::Relaxed));
        match still_running {
            Some(true) => self.engines.get(name).map(|e| e.value().clone()),
            Some(false) => {
                self.engines.remove(name);
                None
            }
            None => None,
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
        engines: DashMap::new(),
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
        // Multi-bot endpoints — agent name in the URL path. The body of
        // /api/start still contains the full config so the engine can be
        // spawned freshly.
        .route("/api/start", post(start))
        .route("/api/stop/:name", post(stop))
        .route("/api/snapshot/:name", get(snapshot))
        .route("/api/kill/:name", post(kill))
        .route("/api/reset/:name", post(reset))
        .route("/api/force_flatten/:name", post(force_flatten))
        // Saved-agents CRUD — used by the sidebar to list all configs,
        // re-load them into the form, and delete them.
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

/// Start an engine for the named agent. Body = full AgentConfig.
/// Rejects if an engine with the same name is already running.
/// Auto-persists the config so it appears in the sidebar.
async fn start(
    State(state): State<Arc<AppState>>,
    Json(cfg): Json<AgentConfig>,
) -> impl IntoResponse {
    let name = cfg.name.trim().to_string();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "agent name cannot be empty" })),
        );
    }
    if state.live_engine(&name).is_some() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("agent '{}' is already running", name) })),
        );
    }
    // Persist the config (insert or update) so the sidebar shows it.
    let mut to_save = cfg.clone();
    to_save.name = name.clone();
    upsert_saved(&state, to_save.clone());

    match EngineHandle::new(to_save).await {
        Ok((handle, fills_rx)) => {
            let handle = Arc::new(handle);
            spawn_engine(handle.clone(), fills_rx);
            state.engines.insert(name.clone(), handle);
            (
                StatusCode::OK,
                Json(json!({ "status": "started", "name": name })),
            )
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

async fn stop(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    if let Some(eng) = state.live_engine(&name) {
        eng.running
            .store(false, std::sync::atomic::Ordering::Relaxed);
        eng.log_line(format!("Engine '{}' stopped by user.", name));
    }
    // Remove the entry — caller can re-start by POSTing /api/start.
    state.engines.remove(&name);
    Json(json!({ "status": "stopped", "name": name }))
}

async fn snapshot(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.live_engine(&name) {
        Some(eng) => Json(json!(eng.snapshot().await)).into_response(),
        None => Json(json!({ "running": false, "message": format!("agent '{}' is not running", name) })).into_response(),
    }
}

async fn kill(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    if let Some(eng) = state.live_engine(&name) {
        eng.kill_switch
            .trip(format!("manual kill from UI for '{}'", name))
            .await;
    }
    Json(json!({ "status": "kill_switch_tripped", "name": name }))
}

async fn reset(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    if let Some(eng) = state.live_engine(&name) {
        eng.kill_switch.manual_reset();
    }
    Json(json!({ "status": "reset", "name": name }))
}

/// List every saved agent config (the sidebar reads this). Also tells
/// the UI which ones are currently RUNNING by name so it can group them
/// under "Active" vs "Inactive" sections.
async fn list_agents(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let agents = state.saved_agents.read().clone();
    // Garbage-collect any engine whose `running` is now false, then
    // gather the names of the survivors.
    let stale: Vec<String> = state
        .engines
        .iter()
        .filter_map(|e| {
            if !e
                .value()
                .running
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                Some(e.key().clone())
            } else {
                None
            }
        })
        .collect();
    for name in stale {
        state.engines.remove(&name);
    }
    let active: Vec<String> = state.engines.iter().map(|e| e.key().clone()).collect();
    (
        StatusCode::OK,
        Json(json!({ "agents": agents, "active": active })),
    )
}

/// Upsert a saved agent (used by the UI's "Save" button on the form,
/// also called implicitly on Start).
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

/// Remove a saved agent from the sidebar list. Refuses to delete a
/// currently-running one.
async fn delete_agent(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    if state.live_engine(&name).is_some() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "cannot delete a running agent — stop it first" })),
        );
    }
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

/// Operator-triggered emergency flatten for the named running agent.
async fn force_flatten(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.live_engine(&name) {
        Some(eng) => {
            let (ok, msg) = eng.force_flatten().await;
            (
                StatusCode::OK,
                Json(json!({ "ok": ok, "message": msg, "name": name })),
            )
        }
        None => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "message": format!("agent '{}' is not running", name) })),
        ),
    }
}
