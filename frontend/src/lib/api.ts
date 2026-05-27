const BASE = process.env.NEXT_PUBLIC_API || "http://localhost:8080";

export type AgentConfig = {
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
    basket_sl_distance: number;
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
    sl_distance: number;
    sl_price: number | null;
    status: "IDLE" | "ACTIVE" | "TPRECYCLING" | "KILLED";
    realized_pnl: number;
    fills_count: number;
    tp_count: number;
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
  cycle_anchor: number;
  cycle_lower: number;
  cycle_upper: number;
  basket_hits: number;
  max_basket_hits: number;
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
