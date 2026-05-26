"use client";
import { useEffect, useState } from "react";
import {
  AgentConfig,
  Snapshot,
  getDefaultConfig,
  startEngine,
  stopEngine,
  killSwitch,
  resetKillSwitch,
  getSnapshot,
} from "@/lib/api";

export default function Home() {
  const [cfg, setCfg] = useState<AgentConfig | null>(null);
  const [snap, setSnap] = useState<Snapshot | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    getDefaultConfig()
      .then(setCfg)
      .catch(() =>
        setErr(
          "Cannot reach backend at http://localhost:8080 — is `cargo run` running?"
        )
      );
  }, []);

  useEffect(() => {
    const t = setInterval(async () => {
      const s = await getSnapshot();
      if (s) setSnap(s);
    }, 500);
    return () => clearInterval(t);
  }, []);

  if (err) return <ErrorBox msg={err} />;
  if (!cfg) return <Loading />;

  return (
    <main className="min-h-screen p-6">
      <Header snap={snap} />
      <div className="grid grid-cols-1 lg:grid-cols-[420px_1fr] gap-6 mt-6">
        <ConfigPanel cfg={cfg} setCfg={setCfg} snap={snap} />
        <Dashboard snap={snap} />
      </div>
    </main>
  );
}

function Header({ snap }: { snap: Snapshot | null }) {
  const running = snap?.running ?? false;
  const tripped = snap?.kill_switch_tripped ?? false;
  return (
    <header className="flex items-center justify-between">
      <div>
        <h1 className="text-3xl font-display font-bold tracking-tight">
          BASKET<span className="text-accent">.</span>GRID
        </h1>
        <p className="text-muted text-xs uppercase tracking-widest mt-1">
          Maker-Only Basket Grid Engine — Paper Mode
        </p>
      </div>
      <div className="flex items-center gap-3">
        <StatusPill
          label={tripped ? "KILLED" : running ? "RUNNING" : "IDLE"}
          color={tripped ? "danger" : running ? "good" : "muted"}
        />
        {snap && (
          <div className="font-mono text-xl">
            {snap.mid_price.toFixed(2)}
            <span className="text-muted text-xs ml-2">mid</span>
          </div>
        )}
      </div>
    </header>
  );
}

function StatusPill({ label, color }: { label: string; color: string }) {
  const colorMap: Record<string, string> = {
    good: "bg-good text-ink",
    danger: "bg-danger text-white",
    warn: "bg-warn text-ink",
    muted: "bg-edge text-muted",
  };
  return (
    <span
      className={`px-3 py-1 rounded-full text-xs font-bold tracking-wider ${colorMap[color]}`}
    >
      ● {label}
    </span>
  );
}

function Loading() {
  return (
    <div className="min-h-screen flex items-center justify-center text-muted">
      Loading…
    </div>
  );
}
function ErrorBox({ msg }: { msg: string }) {
  return (
    <div className="min-h-screen flex items-center justify-center p-6">
      <div className="panel p-6 max-w-lg">
        <div className="text-danger font-bold mb-2">Backend unreachable</div>
        <div className="text-sm text-muted">{msg}</div>
      </div>
    </div>
  );
}

/* ---------- Config Panel ---------- */
function ConfigPanel({
  cfg,
  setCfg,
  snap,
}: {
  cfg: AgentConfig;
  setCfg: (c: AgentConfig) => void;
  snap: Snapshot | null;
}) {
  const running = snap?.running ?? false;
  const tripped = snap?.kill_switch_tripped ?? false;

  const update = <K extends keyof AgentConfig>(
    section: K,
    field: keyof AgentConfig[K],
    value: any
  ) => {
    setCfg({
      ...cfg,
      [section]: { ...(cfg[section] as any), [field]: value },
    });
  };

  return (
    <div className="panel p-5 space-y-5 h-fit">
      <Section title="Trading">
        <Field label="Token">
          <input
            className="input"
            value={cfg.trading.token}
            onChange={(e) => update("trading", "token", e.target.value)}
          />
        </Field>
        <Field label="Exchange">
          <select
            className="input"
            value={cfg.trading.exchange}
            onChange={(e) =>
              update("trading", "exchange", e.target.value as any)
            }
          >
            <option value="mock">Mock (paper)</option>
            <option value="binance">Binance</option>
            <option value="deribit">Deribit</option>
            <option value="hyperliquid">Hyperliquid</option>
          </select>
        </Field>
        <div className="grid grid-cols-2 gap-3">
          <Field label="Grid lower">
            <NumInput
              v={cfg.trading.grid_lower}
              on={(v) => update("trading", "grid_lower", v)}
            />
          </Field>
          <Field label="Grid upper">
            <NumInput
              v={cfg.trading.grid_upper}
              on={(v) => update("trading", "grid_upper", v)}
            />
          </Field>
          <Field label="Grid step">
            <NumInput
              v={cfg.trading.grid_step}
              on={(v) => update("trading", "grid_step", v)}
            />
          </Field>
          <Field label="Per-step qty">
            <NumInput
              v={cfg.trading.per_step_qty}
              on={(v) => update("trading", "per_step_qty", v)}
            />
          </Field>
          <Field label="TP spread">
            <NumInput
              v={cfg.trading.tp_spread}
              on={(v) => update("trading", "tp_spread", v)}
            />
          </Field>
          <Field label="Maker-only">
            <Toggle
              v={cfg.trading.maker_only}
              on={(v) => update("trading", "maker_only", v)}
            />
          </Field>
        </div>
      </Section>

      <Section title="Baskets">
        <div className="grid grid-cols-2 gap-3">
          <Field label="# Baskets">
            <NumInput
              v={cfg.basket.num_baskets}
              on={(v) => update("basket", "num_baskets", Math.round(v))}
            />
          </Field>
          <Field label="Basket size">
            <NumInput
              v={cfg.basket.basket_size_qty}
              on={(v) => update("basket", "basket_size_qty", v)}
            />
          </Field>
          <Field label="SL distance">
            <NumInput
              v={cfg.basket.basket_sl_distance}
              on={(v) => update("basket", "basket_sl_distance", v)}
            />
          </Field>
          <Field label="Max exposure">
            <div className="input bg-edge font-mono">
              {(cfg.basket.num_baskets * cfg.basket.basket_size_qty).toFixed(4)}
            </div>
          </Field>
        </div>
      </Section>

      <Section title="Kill Switch">
        <div className="grid grid-cols-2 gap-3">
          <Field label="Max position cap">
            <NumInput
              v={cfg.kill_switch.max_position_cap}
              on={(v) => update("kill_switch", "max_position_cap", v)}
            />
          </Field>
          <Field label="Max daily loss">
            <NumInput
              v={cfg.kill_switch.max_daily_loss}
              on={(v) => update("kill_switch", "max_daily_loss", v)}
            />
          </Field>
          <Field label="API disconnect protection">
            <Toggle
              v={cfg.kill_switch.api_disconnect_protection}
              on={(v) =>
                update("kill_switch", "api_disconnect_protection", v)
              }
            />
          </Field>
        </div>
      </Section>

      <Section title="Emergency Slicing">
        <div className="grid grid-cols-2 gap-3">
          <Field label="Enabled">
            <Toggle
              v={cfg.slicing.enabled}
              on={(v) => update("slicing", "enabled", v)}
            />
          </Field>
          <Field label="Max slice qty">
            <NumInput
              v={cfg.slicing.max_slice_qty}
              on={(v) => update("slicing", "max_slice_qty", v)}
            />
          </Field>
          <Field label="Slice delay (ms)">
            <NumInput
              v={cfg.slicing.slice_delay_ms}
              on={(v) => update("slicing", "slice_delay_ms", Math.round(v))}
            />
          </Field>
          <Field label="Max slippage (bps)">
            <NumInput
              v={cfg.slicing.max_slippage_bps}
              on={(v) => update("slicing", "max_slippage_bps", v)}
            />
          </Field>
          <Field label="Book depth levels">
            <NumInput
              v={cfg.slicing.book_depth_levels}
              on={(v) => update("slicing", "book_depth_levels", Math.round(v))}
            />
          </Field>
          <Field label="Participation rate">
            <NumInput
              v={cfg.slicing.participation_rate}
              on={(v) => update("slicing", "participation_rate", v)}
            />
          </Field>
          <Field label="Max attempts">
            <NumInput
              v={cfg.slicing.max_slice_attempts}
              on={(v) =>
                update("slicing", "max_slice_attempts", Math.round(v))
              }
            />
          </Field>
        </div>
      </Section>

      <div className="flex gap-3 pt-2">
        <button
          className="btn btn-primary flex-1"
          disabled={running}
          onClick={() => startEngine(cfg)}
        >
          ▶ Submit & Start
        </button>
        <button
          className="btn btn-ghost"
          disabled={!running}
          onClick={stopEngine}
        >
          ■ Stop
        </button>
      </div>
      <div className="flex gap-3">
        <button
          className="btn btn-danger flex-1"
          disabled={!running || tripped}
          onClick={killSwitch}
        >
          ⚠ KILL SWITCH
        </button>
        <button
          className="btn btn-ghost"
          disabled={!tripped}
          onClick={resetKillSwitch}
        >
          Reset
        </button>
      </div>
    </div>
  );
}

function Section({
  title,
  children,
}: {
  title: string;
  children: React.ReactNode;
}) {
  return (
    <div>
      <h3 className="text-accent font-mono text-xs tracking-widest mb-3">
        {title.toUpperCase()}
      </h3>
      <div className="space-y-3">{children}</div>
    </div>
  );
}
function Field({
  label,
  children,
}: {
  label: string;
  children: React.ReactNode;
}) {
  return (
    <div>
      <div className="label mb-1">{label}</div>
      {children}
    </div>
  );
}
function NumInput({ v, on }: { v: number; on: (n: number) => void }) {
  return (
    <input
      className="input"
      type="number"
      value={v}
      step="any"
      onChange={(e) => on(parseFloat(e.target.value) || 0)}
    />
  );
}
function Toggle({ v, on }: { v: boolean; on: (b: boolean) => void }) {
  return (
    <button
      onClick={() => on(!v)}
      className={`input text-left ${v ? "text-accent" : "text-muted"}`}
    >
      {v ? "● ENABLED" : "○ DISABLED"}
    </button>
  );
}

/* ---------- Dashboard ---------- */
function Dashboard({ snap }: { snap: Snapshot | null }) {
  if (!snap) {
    return (
      <div className="panel p-6 text-muted">
        Engine not started. Configure the parameters and click{" "}
        <span className="text-accent">Submit & Start</span>.
      </div>
    );
  }
  return (
    <div className="space-y-6">
      <KPIRow snap={snap} />
      <BasketGrid snap={snap} />
      <div className="grid grid-cols-1 xl:grid-cols-2 gap-6">
        <OpenOrders snap={snap} />
        <RecentFills snap={snap} />
      </div>
      <RiskAndLog snap={snap} />
    </div>
  );
}

function KPIRow({ snap }: { snap: Snapshot }) {
  const killed = snap.baskets.filter((b) => b.status === "KILLED").length;
  return (
    <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
      <KPI label="Mid Price" v={snap.mid_price.toFixed(2)} />
      <KPI label="Open Qty" v={snap.total_open_qty.toFixed(4)} />
      <KPI
        label="Realized PnL"
        v={snap.total_realized_pnl.toFixed(2)}
        color={snap.total_realized_pnl >= 0 ? "good" : "danger"}
      />
      <KPI
        label="Baskets Killed"
        v={`${killed}/${snap.baskets.length}`}
        color={killed > 0 ? "warn" : "muted"}
      />
    </div>
  );
}
function KPI({
  label,
  v,
  color = "muted",
}: {
  label: string;
  v: string;
  color?: string;
}) {
  const colorMap: Record<string, string> = {
    good: "text-good",
    danger: "text-danger",
    warn: "text-warn",
    muted: "",
  };
  return (
    <div className="panel p-4">
      <div className="label">{label}</div>
      <div className={`font-mono text-2xl mt-1 ${colorMap[color]}`}>{v}</div>
    </div>
  );
}

function BasketGrid({ snap }: { snap: Snapshot }) {
  return (
    <div className="panel p-5">
      <h3 className="text-accent font-mono text-xs tracking-widest mb-4">
        BASKETS
      </h3>
      <div className="grid grid-cols-2 md:grid-cols-3 gap-3">
        {snap.baskets.map((b) => (
          <BasketCard key={b.basket_id} b={b} />
        ))}
      </div>
    </div>
  );
}
function BasketCard({ b }: { b: Snapshot["baskets"][0] }) {
  const colorMap: Record<string, string> = {
    IDLE: "border-edge text-muted",
    ACTIVE: "border-accent text-accent",
    TPRECYCLING: "border-good text-good",
    KILLED: "border-danger text-danger opacity-60",
  };
  return (
    <div className={`border rounded-md p-3 ${colorMap[b.status]}`}>
      <div className="flex justify-between items-center mb-2">
        <span className="font-bold">#{b.index}</span>
        <span className="text-[10px] tracking-widest">{b.status}</span>
      </div>
      <div className="text-xs font-mono space-y-1 text-white/70">
        <Row k="open" v={b.open_qty.toFixed(4)} />
        <Row k="max" v={b.max_qty.toFixed(4)} />
        <Row k="avg" v={b.avg_price > 0 ? b.avg_price.toFixed(2) : "—"} />
        <Row k="SL" v={b.sl_price ? b.sl_price.toFixed(2) : "—"} />
        <Row
          k="PnL"
          v={b.realized_pnl.toFixed(2)}
          highlight={b.realized_pnl !== 0}
        />
        <Row k="fills/tp" v={`${b.fills_count}/${b.tp_count}`} />
      </div>
    </div>
  );
}
function Row({
  k,
  v,
  highlight,
}: {
  k: string;
  v: string;
  highlight?: boolean;
}) {
  return (
    <div className="flex justify-between">
      <span className="text-muted">{k}</span>
      <span className={highlight ? "text-white" : ""}>{v}</span>
    </div>
  );
}

function OpenOrders({ snap }: { snap: Snapshot }) {
  return (
    <div className="panel p-5">
      <h3 className="text-accent font-mono text-xs tracking-widest mb-3">
        OPEN ORDERS ({snap.open_orders.length})
      </h3>
      <div className="overflow-y-auto max-h-72 text-xs font-mono">
        {snap.open_orders.length === 0 && (
          <div className="text-muted py-4">No open orders</div>
        )}
        {snap.open_orders.map((o) => (
          <div
            key={o.order_id}
            className="grid grid-cols-4 gap-2 py-1 border-b border-edge"
          >
            <span
              className={o.side === "BUY" ? "text-good" : "text-warn"}
            >
              {o.side}
            </span>
            <span className="text-muted">{o.purpose}</span>
            <span>{o.price.toFixed(2)}</span>
            <span className="text-right">{o.qty.toFixed(4)}</span>
          </div>
        ))}
      </div>
    </div>
  );
}

function RecentFills({ snap }: { snap: Snapshot }) {
  return (
    <div className="panel p-5">
      <h3 className="text-accent font-mono text-xs tracking-widest mb-3">
        RECENT FILLS
      </h3>
      <div className="overflow-y-auto max-h-72 text-xs font-mono">
        {snap.recent_fills.length === 0 && (
          <div className="text-muted py-4">No fills yet</div>
        )}
        {[...snap.recent_fills].reverse().map((f) => (
          <div
            key={f.fill_id}
            className="grid grid-cols-4 gap-2 py-1 border-b border-edge"
          >
            <span
              className={
                f.purpose === "entry"
                  ? "text-good"
                  : f.purpose === "take_profit"
                  ? "text-accent"
                  : "text-danger"
              }
            >
              {f.purpose}
            </span>
            <span>{f.side}</span>
            <span>{f.price.toFixed(2)}</span>
            <span className="text-right">{f.qty.toFixed(4)}</span>
          </div>
        ))}
      </div>
    </div>
  );
}

function RiskAndLog({ snap }: { snap: Snapshot }) {
  return (
    <div className="grid grid-cols-1 xl:grid-cols-[1fr_2fr] gap-6">
      <div className="panel p-5">
        <h3 className="text-accent font-mono text-xs tracking-widest mb-3">
          RISK ENGINE
        </h3>
        <div className="space-y-2 text-sm font-mono">
          <RiskLine k="Max exposure" ok={snap.risk.max_exposure_ok} />
          <RiskLine k="Daily loss" ok={snap.risk.daily_loss_ok} />
          <RiskLine k="API connected" ok={snap.risk.api_connected} />
          <RiskLine k="Missing SL" ok={snap.risk.missing_sl_ok} />
          <RiskLine k="Slippage" ok={snap.risk.slippage_ok} />
          <RiskLine k="Liquidity" ok={snap.risk.liquidity_ok} />
          <RiskLine k="Runaway exec" ok={snap.risk.runaway_ok} />
        </div>
        {snap.risk.breach_reason && (
          <div className="mt-3 text-danger text-xs">
            BREACH: {snap.risk.breach_reason}
          </div>
        )}
        {snap.kill_switch_reason && (
          <div className="mt-3 text-danger text-xs">
            KILL: {snap.kill_switch_reason}
          </div>
        )}
      </div>
      <div className="panel p-5">
        <h3 className="text-accent font-mono text-xs tracking-widest mb-3">
          ENGINE LOG
        </h3>
        <div className="overflow-y-auto max-h-72 text-xs font-mono space-y-1">
          {[...snap.log].reverse().map((l, i) => (
            <div key={i} className="text-white/70">
              {l}
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
function RiskLine({ k, ok }: { k: string; ok: boolean }) {
  return (
    <div className="flex justify-between">
      <span className="text-muted">{k}</span>
      <span className={ok ? "text-good" : "text-danger"}>
        {ok ? "● OK" : "○ BREACH"}
      </span>
    </div>
  );
}
