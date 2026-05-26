const BASE = process.env.NEXT_PUBLIC_API || "http://localhost:8080";

export type AgentConfig = {
  trading: {
    token: string;
    exchange: "binance" | "deribit" | "hyperliquid" | "mock";
    grid_lower: number;
    grid_upper: number;
    grid_step: number;
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

export type Snapshot = {
  running: boolean;
  kill_switch_tripped: boolean;
  kill_switch_reason: string | null;
  mid_price: number;
  total_open_qty: number;
  total_realized_pnl: number;
  baskets: Array<{
    basket_id: string;
    index: number;
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
