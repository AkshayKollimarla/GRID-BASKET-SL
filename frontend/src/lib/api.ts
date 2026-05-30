const BASE = process.env.NEXT_PUBLIC_API || "http://localhost:8080";

export type AgentConfig = {
  /** Human-friendly identifier so the user can save & reload configs. */
  name: string;
  trading: {
    token: string;
    exchange: "binance" | "deribit" | "hyperliquid" | "mock";
    /** Mock-only seed price + legacy fallback. Not shown in the form. */
    grid_lower: number;
    grid_upper: number;
    /** Distance from first mid that defines the absolute hard cap on both sides. */
    grid_distance: number;
    grid_step: number;
    grid_depth: number;
    per_step_qty: number;
    tp_spread: number;
    maker_only: boolean;
  };
  basket: {
    num_baskets: number;
    basket_size_qty: number;
    /** @deprecated Per-basket SL removed; only cycle SL controls flattening. */
    basket_sl_distance?: number;
  };
  kill_switch: {
    max_position_cap: number;
    max_daily_loss: number;
    api_disconnect_protection: boolean;
    max_basket_hits: number;
  };
  slicing: {
    enabled: boolean;
    max_slice_qty: number;
    slice_delay_ms: number;
    max_slippage_bps: number;
    book_depth_levels: number;
    participation_rate: number;
    max_slice_attempts: number;
  };
};

export type TradeStats = {
  start_time: number;
  duration_seconds: number;
  /** = net_pnl (legacy alias) */
  total_pnl: number;
  net_pnl: number;
  /** PnL from TP-closed round trips only (≥ 0). */
  rtp_pnl: number;
  /** PnL from SL / kill-switch exits (≤ 0). */
  sl_pnl: number;
  total_fees: number;
  /** Count of TP-closed round trips. */
  round_trips: number;
  /** Count of SL / kill-switch exits. */
  sl_count: number;
  rtp_per_hour: number;
  pnl_per_hour: number;
  buy_vwap: number;
  sell_vwap: number;
  total_volume: number;
  buy_volume: number;
  sell_volume: number;
  buy_qty: number;
  sell_qty: number;
  net_qty: number;
  total_fills: number;
  total_buys: number;
  total_sells: number;
};

export type RoundTrip = {
  rtp_id: string;
  basket_id: string;
  basket_index: number;
  entry_side: "BUY" | "SELL";
  entry_price: number;
  exit_price: number;
  qty: number;
  gross_pnl: number;
  fees: number;
  pnl: number;
  volume: number;
  entry_time: number;
  exit_time: number;
  is_take_profit: boolean;
};

export type Snapshot = {
  running: boolean;
  kill_switch_tripped: boolean;
  kill_switch_reason: string | null;
  mid_price: number;
  total_open_qty: number;
  total_realized_pnl: number;
  exchange_name: string;
  baskets: Array<{
    basket_id: string;
    index: number;
    side: "LONG" | "SHORT";
    max_qty: number;
    open_qty: number;
    avg_price: number;
    status: "IDLE" | "ACTIVE" | "TPRECYCLING" | "HIT" | "KILLED";
    realized_pnl: number;
    fills_count: number;
    tp_count: number;
    /** Signed net position: + = net long, − = net short, 0 = flat. */
    net_qty: number;
    /** Set on the basket's FIRST entry fill. 0 = not yet activated. */
    anchor_price: number;
    /** anchor_price + grid_distance. SL fires when mid ≥ this and open_qty > 0. */
    upper_sl: number;
    /** anchor_price − grid_distance. SL fires when mid ≤ this and open_qty > 0. */
    lower_sl: number;
  }>;
  open_orders: Array<{
    order_id: string;
    basket_id: string;
    side: "BUY" | "SELL";
    order_type: string;
    purpose: string;
    price: number;
    qty: number;
    status: string;
  }>;
  recent_fills: Array<{
    fill_id: string;
    /** Exchange order ID — partials for the same order share this. */
    order_id: string;
    basket_id: string;
    side: "BUY" | "SELL";
    purpose: string;
    price: number;
    qty: number;
    fee: number;
    timestamp: number;
  }>;
  risk: {
    max_exposure_ok: boolean;
    daily_loss_ok: boolean;
    api_connected: boolean;
    missing_sl_ok: boolean;
    slippage_ok: boolean;
    liquidity_ok: boolean;
    runaway_ok: boolean;
    breach_reason: string | null;
  };
  log: string[];
  trade_stats: TradeStats;
  round_trips: RoundTrip[];
  /** Mid at the moment the bot started — never changes after init. */
  start_price: number;
  /** Mid at the start of the current cycle. */
  cycle_anchor: number;
  /** Current-cycle lower SL = anchor − grid_distance */
  cycle_lower: number;
  /** Current-cycle upper SL = anchor + grid_distance */
  cycle_upper: number;
  /** Distance (= cycle SL distance). */
  grid_distance: number;
  basket_hits: number;
  max_basket_hits: number;
  /** Live exchange position (signed). */
  exchange_position: number;
  /** Bot's tracked net qty (= buy_qty - sell_qty). */
  bot_net_qty: number;
  /** |exchange_position - bot_net_qty| — large means desync. */
  position_drift: number;
  /** TPs currently parked off-exchange (depth budget was full when they
   *  were placed). They auto-return when mid drifts back into range. */
  parked_tp_count: number;
};

export async function getDefaultConfig(): Promise<AgentConfig> {
  const r = await fetch(`${BASE}/api/config/default`);
  return r.json();
}

export async function startEngine(cfg: AgentConfig) {
  const r = await fetch(`${BASE}/api/start`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(cfg),
  });
  return r.json();
}

export async function stopEngine() {
  const r = await fetch(`${BASE}/api/stop`, { method: "POST" });
  return r.json();
}

export async function killSwitch() {
  const r = await fetch(`${BASE}/api/kill`, { method: "POST" });
  return r.json();
}

export async function resetKillSwitch() {
  const r = await fetch(`${BASE}/api/reset`, { method: "POST" });
  return r.json();
}

/**
 * Emergency operator-triggered flatten. Cancels every order, slices every
 * basket flat, runs a residual mop-up against any leftover exchange
 * position, then verifies the exchange-side position is zero.
 * Returns `{ ok, message }`.
 */
export async function forceFlatten(): Promise<{ ok: boolean; message: string }> {
  try {
    const r = await fetch(`${BASE}/api/force_flatten`, { method: "POST" });
    return r.json();
  } catch (e: any) {
    return { ok: false, message: e?.message ?? "request failed" };
  }
}

/* ===================================================================
   SAVED AGENTS — sidebar list of inactive configs.
   =================================================================== */
export type AgentList = {
  /** All persisted saved configs (active + inactive). */
  agents: AgentConfig[];
  /** Name of the currently-running agent (null if engine isn't running). */
  active: string | null;
};

/** List every saved agent + the currently active one. Tolerates a
 *  backend that doesn't have the /api/agents route yet (empty body /
 *  404) and just shows an empty list rather than crashing.
 */
export async function listAgents(): Promise<AgentList> {
  try {
    const r = await fetch(`${BASE}/api/agents`, { cache: "no-store" });
    const j = await safeJson(r);
    return {
      agents: Array.isArray(j?.agents) ? j.agents : [],
      active: j?.active ?? null,
    };
  } catch {
    return { agents: [], active: null };
  }
}

/**
 * Parse a response body that MIGHT be empty / non-JSON. Older backend
 * builds without the /api/agents endpoint will 404 with no body, and
 * `r.json()` throws "Unexpected end of JSON input" on that. We catch
 * that here and return a structured error the caller can surface.
 */
async function safeJson(r: Response): Promise<any> {
  const text = await r.text();
  if (!text) {
    return {
      error: r.ok
        ? "empty response from backend"
        : `backend returned ${r.status} ${r.statusText} (is it running the latest code? restart cargo run)`,
    };
  }
  try {
    return JSON.parse(text);
  } catch {
    return { error: `non-JSON response: ${text.slice(0, 200)}` };
  }
}

/** Upsert a saved agent by name. */
export async function saveAgent(cfg: AgentConfig) {
  try {
    const r = await fetch(`${BASE}/api/agents`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(cfg),
    });
    return await safeJson(r);
  } catch (e: any) {
    return { error: e?.message ?? "save failed" };
  }
}

/** Remove a saved agent from the sidebar. */
export async function deleteAgent(name: string) {
  try {
    const r = await fetch(
      `${BASE}/api/agents/${encodeURIComponent(name)}`,
      { method: "DELETE" }
    );
    return await safeJson(r);
  } catch (e: any) {
    return { error: e?.message ?? "delete failed" };
  }
}

export async function getInstruments(
  exchange: AgentConfig["trading"]["exchange"]
): Promise<string[]> {
  try {
    const r = await fetch(`${BASE}/api/instruments?exchange=${exchange}`);
    const j = await r.json();
    return Array.isArray(j.symbols) ? j.symbols : [];
  } catch {
    return [];
  }
}

export async function getSnapshot(): Promise<Snapshot | null> {
  try {
    const r = await fetch(`${BASE}/api/snapshot`, { cache: "no-store" });
    const j = await r.json();
    if (j.running === false && !j.baskets) return null;
    return j;
  } catch {
    return null;
  }
}
