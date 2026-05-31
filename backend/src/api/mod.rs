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
        // 24h (or arbitrary-hours) summary — reads every per-agent
        // history file under `history/` and aggregates by (exchange,
        // token). Includes data from STOPPED bots that traded inside
        // the window, plus live data from currently running ones (since
        // both append to the same files).
        .route("/api/summary", get(summary))
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
    // Stamp `last_active_at` so the UI can sort Inactive by recency.
    // Clear any previous stop reason — the bot is being started fresh.
    let mut to_save = cfg.clone();
    to_save.name = name.clone();
    to_save.last_active_at = chrono::Utc::now().timestamp_millis();
    to_save.last_stop_reason = String::new();
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

/// Update only the `last_stop_reason` field of a saved agent and
/// persist. No-op if the name isn't found (deleted agent).
fn set_stop_reason(state: &AppState, name: &str, reason: impl Into<String>) {
    let reason = reason.into();
    {
        let mut g = state.saved_agents.write();
        if let Some(slot) = g.iter_mut().find(|c| c.name == name) {
            slot.last_stop_reason = reason;
        }
    }
    state.persist();
}

async fn stop(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    if let Some(eng) = state.live_engine(&name) {
        eng.log_line(format!("Engine '{}' stopped by user.", name));
        // Freeze the FINAL state to disk BEFORE flipping `running` so
        // the snapshot captures the bot mid-trade with every basket /
        // order / fill / stat intact.
        eng.save_final_snapshot().await;
        eng.running
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }
    state.engines.remove(&name);
    set_stop_reason(&state, &name, "Stopped by user");
    Json(json!({ "status": "stopped", "name": name }))
}

async fn snapshot(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    if let Some(eng) = state.live_engine(&name) {
        return Json(json!(eng.snapshot().await)).into_response();
    }
    // No live engine — try to serve the FROZEN snapshot saved when
    // the bot last stopped, so the operator can still review the
    // post-mortem state in the UI.
    let path = EngineHandle::snapshot_file_path(&name);
    match tokio::fs::read_to_string(&path).await {
        Ok(s) => match serde_json::from_str::<serde_json::Value>(&s) {
            Ok(mut v) => {
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("frozen".into(), serde_json::json!(true));
                    // `running` should already be false in the saved
                    // snapshot, but force it just in case the file was
                    // written while the engine was still alive.
                    obj.insert("running".into(), serde_json::json!(false));
                }
                Json(v).into_response()
            }
            Err(_) => Json(
                json!({ "running": false, "frozen": false, "message": format!("agent '{}' snapshot file is corrupt", name) }),
            )
            .into_response(),
        },
        Err(_) => Json(
            json!({ "running": false, "frozen": false, "message": format!("agent '{}' is not running and has no frozen snapshot", name) }),
        )
        .into_response(),
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
        // Freeze the post-kill state so the inactive view shows what
        // happened (basket statuses, last open orders, realized PnL).
        eng.save_final_snapshot().await;
    }
    set_stop_reason(&state, &name, "Killed by user");
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
    // Garbage-collect any engine whose `running` is now false. For each
    // self-stopped engine, capture its kill_switch reason (set by
    // all_killed self-trip or risk-engine trip) and write it to the
    // saved agent so the Inactive sidebar can display it.
    let stale: Vec<(String, Option<String>)> = state
        .engines
        .iter()
        .filter_map(|e| {
            if !e
                .value()
                .running
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                let reason = e.value().kill_switch.reason();
                Some((e.key().clone(), reason))
            } else {
                None
            }
        })
        .collect();
    // For each engine we're about to garbage-collect, freeze its
    // final snapshot to disk first — main loop self-stop didn't
    // necessarily do so itself.
    for (name, _) in &stale {
        if let Some(eng) = state.engines.get(name) {
            eng.value().save_final_snapshot().await;
        }
    }
    for (name, reason) in stale {
        state.engines.remove(&name);
        if let Some(r) = reason {
            // Only overwrite if the saved entry doesn't already have a
            // more specific reason set by an action endpoint.
            let need_set = {
                let g = state.saved_agents.read();
                g.iter()
                    .find(|c| c.name == name)
                    .map(|c| c.last_stop_reason.is_empty())
                    .unwrap_or(false)
            };
            if need_set {
                set_stop_reason(&state, &name, r);
            }
        }
    }
    let agents = state.saved_agents.read().clone();
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

#[derive(Debug, Deserialize)]
struct SummaryQuery {
    /// Legacy "last N hours" — used by the old sidebar mini-summary.
    hours: Option<u64>,
    /// Explicit start (Unix epoch milliseconds). Overrides `hours`.
    since_ms: Option<i64>,
    /// Explicit end (Unix epoch milliseconds). Defaults to now.
    until_ms: Option<i64>,
}

/// Summary endpoint — reads every per-agent history JSONL under
/// `history/`, filters by timestamp within [since_ms, until_ms], and
/// returns:
///   - `accounts: [ { name, agents:[…], cumulative:{…} } ]`
///   - `rows: [ … ]` — legacy aggregation by (exchange, token) used by
///     the small sidebar summary section.
/// One ROW per (exchange, token) for the legacy small summary; one
/// AGENT entry per (account, agent) for the Summary Report view.
async fn summary(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SummaryQuery>,
) -> impl IntoResponse {
    use std::collections::HashMap;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let since_ms = q.since_ms.unwrap_or_else(|| {
        let hours = q.hours.unwrap_or(24).max(1) as i64;
        now_ms - hours * 3_600_000
    });
    let until_ms = q.until_ms.unwrap_or(now_ms);
    let elapsed_hours = ((until_ms - since_ms).max(1) as f64) / 3_600_000.0;

    // Helper: read every history event in [since, until] into one vec.
    let mut events: Vec<crate::engine::HistoryEvent> = Vec::new();
    let history_dir = std::path::Path::new("history");
    if let Ok(entries) = std::fs::read_dir(history_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|s| s != "jsonl").unwrap_or(true) {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            for line in content.lines() {
                let ev: crate::engine::HistoryEvent = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if ev.ts < since_ms || ev.ts > until_ms {
                    continue;
                }
                events.push(ev);
            }
        }
    }

    // ── Per-agent aggregation for the Summary Report view ────────────
    #[derive(Default)]
    struct AgentAgg {
        token: String,
        exchange: String,
        rtp_count: u64,
        gross_pnl: f64,
        fees: f64,
        rebates: f64,
        volume: f64,
        qty: f64,
        basket_hits: u64,
        basket_hit_pnl: f64,
        // VWAP accumulators — sum(price × qty) and sum(qty) per side.
        buy_pq_sum: f64,
        buy_qty_sum: f64,
        sell_pq_sum: f64,
        sell_qty_sum: f64,
        buys: u64,
        sells: u64,
    }
    let mut per_agent: HashMap<(String, String), AgentAgg> = HashMap::new();
    let mut active_set = std::collections::HashSet::new();
    for k in state.engines.iter() {
        active_set.insert(k.key().clone());
    }

    for ev in &events {
        let key = (ev.exchange.clone(), ev.agent.clone());
        let a = per_agent.entry(key).or_default();
        a.exchange = ev.exchange.clone();
        a.token = ev.token.clone();
        match ev.kind.as_str() {
            "rtp" | "sl_rtp" => {
                a.rtp_count += 1;
                a.gross_pnl += ev.gross_pnl;
                if ev.fees >= 0.0 {
                    a.fees += ev.fees;
                } else {
                    a.rebates += -ev.fees;
                }
                a.volume += ev.volume;
                a.qty += ev.qty;
                // Each RT = 1 buy leg + 1 sell leg of equal qty. One
                // leg's price is `entry_price`, the other is
                // `exit_price`, depending on `entry_side`.
                let entry_is_buy = ev.entry_side.eq_ignore_ascii_case("BUY");
                let (buy_price, sell_price) = if entry_is_buy {
                    (ev.entry_price, ev.exit_price)
                } else {
                    (ev.exit_price, ev.entry_price)
                };
                if buy_price > 0.0 {
                    a.buy_pq_sum += buy_price * ev.qty;
                    a.buy_qty_sum += ev.qty;
                    a.buys += 1;
                }
                if sell_price > 0.0 {
                    a.sell_pq_sum += sell_price * ev.qty;
                    a.sell_qty_sum += ev.qty;
                    a.sells += 1;
                }
            }
            "basket_hit" => {
                a.basket_hits += 1;
                a.basket_hit_pnl += ev.gross_pnl;
            }
            _ => {}
        }
    }

    // Group per-agent rows by EXCHANGE (= "account" for now). Each
    // group also carries a CUMULATIVE row.
    let mut by_account: HashMap<String, Vec<(String, AgentAgg)>> = HashMap::new();
    for ((exchange, agent), a) in per_agent {
        by_account.entry(exchange).or_default().push((agent, a));
    }
    let mut accounts: Vec<serde_json::Value> = Vec::new();
    for (account, mut agents_in_acc) in by_account {
        agents_in_acc.sort_by(|(a, _), (b, _)| a.cmp(b));
        let agents_json: Vec<serde_json::Value> = agents_in_acc
            .iter()
            .map(|(name, a)| {
                let buy_vwap = if a.buy_qty_sum > 0.0 {
                    a.buy_pq_sum / a.buy_qty_sum
                } else {
                    0.0
                };
                let sell_vwap = if a.sell_qty_sum > 0.0 {
                    a.sell_pq_sum / a.sell_qty_sum
                } else {
                    0.0
                };
                let net_pnl = a.gross_pnl - a.fees + a.rebates + a.basket_hit_pnl;
                let pnl_per_rtp = if a.rtp_count > 0 {
                    net_pnl / a.rtp_count as f64
                } else {
                    0.0
                };
                let rtp_per_hr = a.rtp_count as f64 / elapsed_hours;
                let vol_per_hr = a.volume / elapsed_hours;
                json!({
                    "name": name,
                    "symbol": a.token,
                    "status": if active_set.contains(name) { "active" } else { "inactive" },
                    "buy_vwap": buy_vwap,
                    "sell_vwap": sell_vwap,
                    "buys": a.buys,
                    "sells": a.sells,
                    "rtps": a.rtp_count,
                    "rtp_per_hr": rtp_per_hr,
                    "gross_pnl": a.gross_pnl,
                    "fees": a.fees,
                    "rebates": a.rebates,
                    "net_pnl": net_pnl,
                    "pnl_per_rtp": pnl_per_rtp,
                    "vol_per_hr": vol_per_hr,
                    "volume": a.volume,
                    "basket_hits": a.basket_hits,
                    "basket_hit_pnl": a.basket_hit_pnl,
                })
            })
            .collect();
        // Per-account cumulative row.
        let cum = agents_in_acc.iter().fold(AgentAgg::default(), |mut c, (_, a)| {
            c.rtp_count += a.rtp_count;
            c.gross_pnl += a.gross_pnl;
            c.fees += a.fees;
            c.rebates += a.rebates;
            c.volume += a.volume;
            c.qty += a.qty;
            c.basket_hits += a.basket_hits;
            c.basket_hit_pnl += a.basket_hit_pnl;
            c.buy_pq_sum += a.buy_pq_sum;
            c.buy_qty_sum += a.buy_qty_sum;
            c.sell_pq_sum += a.sell_pq_sum;
            c.sell_qty_sum += a.sell_qty_sum;
            c.buys += a.buys;
            c.sells += a.sells;
            c
        });
        let cum_net = cum.gross_pnl - cum.fees + cum.rebates + cum.basket_hit_pnl;
        let cum_pnl_per_rtp = if cum.rtp_count > 0 {
            cum_net / cum.rtp_count as f64
        } else {
            0.0
        };
        let cum_buy_vwap = if cum.buy_qty_sum > 0.0 {
            cum.buy_pq_sum / cum.buy_qty_sum
        } else {
            0.0
        };
        let cum_sell_vwap = if cum.sell_qty_sum > 0.0 {
            cum.sell_pq_sum / cum.sell_qty_sum
        } else {
            0.0
        };
        accounts.push(json!({
            "name": account,
            "agent_count": agents_json.len(),
            "agents": agents_json,
            "cumulative": {
                "buys": cum.buys,
                "sells": cum.sells,
                "rtps": cum.rtp_count,
                "rtp_per_hr": cum.rtp_count as f64 / elapsed_hours,
                "buy_vwap": cum_buy_vwap,
                "sell_vwap": cum_sell_vwap,
                "gross_pnl": cum.gross_pnl,
                "fees": cum.fees,
                "rebates": cum.rebates,
                "net_pnl": cum_net,
                "pnl_per_rtp": cum_pnl_per_rtp,
                "vol_per_hr": cum.volume / elapsed_hours,
                "volume": cum.volume,
                "basket_hits": cum.basket_hits,
                "basket_hit_pnl": cum.basket_hit_pnl,
            }
        }));
    }
    accounts.sort_by(|a, b| {
        a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or(""))
    });

    // ── Legacy aggregation by (exchange, token) for the small sidebar ──
    #[derive(Default)]
    struct CoinAgg {
        rtp_count: u64,
        gross_pnl: f64,
        fees: f64,
        rebates: f64,
        volume: f64,
        qty: f64,
        basket_hits: u64,
        basket_hit_pnl: f64,
        agents: std::collections::BTreeSet<String>,
    }
    let mut coin_agg: HashMap<(String, String), CoinAgg> = HashMap::new();
    for ev in &events {
        let key = (ev.exchange.clone(), ev.token.clone());
        let s = coin_agg.entry(key).or_default();
        s.agents.insert(ev.agent.clone());
        match ev.kind.as_str() {
            "rtp" | "sl_rtp" => {
                s.rtp_count += 1;
                s.gross_pnl += ev.gross_pnl;
                if ev.fees >= 0.0 {
                    s.fees += ev.fees;
                } else {
                    s.rebates += -ev.fees;
                }
                s.volume += ev.volume;
                s.qty += ev.qty;
            }
            "basket_hit" => {
                s.basket_hits += 1;
                s.basket_hit_pnl += ev.gross_pnl;
            }
            _ => {}
        }
    }
    let mut rows: Vec<serde_json::Value> = coin_agg
        .into_iter()
        .map(|((exchange, token), s)| {
            let per_rtp_pnl = if s.rtp_count > 0 {
                s.gross_pnl / s.rtp_count as f64
            } else {
                0.0
            };
            let net_pnl = s.gross_pnl - s.fees + s.rebates + s.basket_hit_pnl;
            json!({
                "exchange": exchange,
                "token": token,
                "agents": s.agents.into_iter().collect::<Vec<_>>(),
                "rtp_count": s.rtp_count,
                "per_rtp_pnl": per_rtp_pnl,
                "gross_pnl": s.gross_pnl,
                "fees": s.fees,
                "rebates": s.rebates,
                "net_pnl": net_pnl,
                "volume": s.volume,
                "basket_hits": s.basket_hits,
                "basket_hit_pnl": s.basket_hit_pnl,
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        let ka = (
            a["exchange"].as_str().unwrap_or("").to_string(),
            a["token"].as_str().unwrap_or("").to_string(),
        );
        let kb = (
            b["exchange"].as_str().unwrap_or("").to_string(),
            b["token"].as_str().unwrap_or("").to_string(),
        );
        ka.cmp(&kb)
    });

    (
        StatusCode::OK,
        Json(json!({
            "hours": ((until_ms - since_ms) / 3_600_000).max(1),
            "since_ms": since_ms,
            "until_ms": until_ms,
            "now_ms": now_ms,
            "accounts": accounts,
            "rows": rows,
        })),
    )
}

/// Operator-triggered emergency flatten for the named running agent.
async fn force_flatten(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.live_engine(&name) {
        Some(eng) => {
            let (ok, msg) = eng.force_flatten().await;
            // Snapshot the post-flatten state so even if the operator
            // later stops the bot without further events, the frozen
            // view shows the flat exchange + cleared baskets.
            eng.save_final_snapshot().await;
            set_stop_reason(&state, &name, format!("Force flatten: {}", msg));
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
