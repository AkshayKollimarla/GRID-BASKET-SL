"use client";
import { useEffect, useMemo, useState } from "react";
import {
  AgentConfig,
  RoundTrip,
  Snapshot,
  TradeStats,
  getDefaultConfig,
  getInstruments,
  startEngine,
  stopEngine,
  killSwitch,
  resetKillSwitch,
  getSnapshot,
  forceFlatten,
  listAgents,
  saveAgent,
  deleteAgent,
  AgentList,
} from "@/lib/api";

/* ===================================================================
   ROOT
   =================================================================== */
export default function Home() {
  const [cfg, setCfg] = useState<AgentConfig | null>(null);
  const [snap, setSnap] = useState<Snapshot | null>(null);
  const [agents, setAgents] = useState<AgentList>({ agents: [], active: null });
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
    // 250ms polling — sees fills within at most 1 cycle.
    const t = setInterval(async () => {
      const s = await getSnapshot();
      if (s) setSnap(s);
    }, 250);
    return () => clearInterval(t);
  }, []);

  // Poll the saved-agents list separately and less often — it only
  // changes on Save/Delete/Start, so 2s is plenty and saves traffic.
  useEffect(() => {
    let cancelled = false;
    const refresh = async () => {
      const a = await listAgents();
      if (!cancelled) setAgents(a);
    };
    refresh();
    const t = setInterval(refresh, 2000);
    return () => {
      cancelled = true;
      clearInterval(t);
    };
  }, []);

  if (err) return <ErrorBox msg={err} />;
  if (!cfg) return <Loading />;

  const refreshAgents = async () => setAgents(await listAgents());

  return (
    <main className="min-h-screen flex">
      <AgentsSidebar
        agents={agents}
        activeName={agents.active}
        currentName={cfg.name}
        running={snap?.running ?? false}
        onEdit={(loaded) => setCfg(loaded)}
        onDeleted={refreshAgents}
      />
      <div className="flex-1 min-w-0">
        <TopBar
          cfg={cfg}
          snap={snap}
          onSaved={refreshAgents}
        />
        <PositionDriftBanner snap={snap} />
        <div className="px-6 pb-8 max-w-7xl mx-auto">
          <AccordionStack cfg={cfg} setCfg={setCfg} snap={snap} />
        </div>
      </div>
    </main>
  );
}

/* ===================================================================
   SIDEBAR — saved-agent list. Click Edit to load a config back into
   the form (rename, tweak, then Start). Click ✕ to delete. The
   currently-running agent is marked ACTIVE; everything else INACTIVE.
   =================================================================== */
function AgentsSidebar({
  agents,
  activeName,
  currentName,
  running,
  onEdit,
  onDeleted,
}: {
  agents: AgentList;
  activeName: string | null;
  currentName: string;
  running: boolean;
  onEdit: (cfg: AgentConfig) => void;
  onDeleted: () => void | Promise<void>;
}) {
  const list = agents.agents;
  return (
    <aside className="w-64 shrink-0 border-r border-edge bg-slate-50 min-h-screen sticky top-0 self-start max-h-screen overflow-y-auto">
      <div className="px-4 py-4 border-b border-edge">
        <h2 className="font-bold text-sm uppercase tracking-wider text-muted">
          Agents
        </h2>
        <p className="text-[11px] text-muted mt-1">
          {list.length} saved · {activeName ? "1 running" : "none running"}
        </p>
      </div>
      {list.length === 0 ? (
        <div className="px-4 py-6 text-xs text-muted">
          No saved agents yet. Configure one in the form and click Save (or
          Start — saves automatically).
        </div>
      ) : (
        <ul className="py-2">
          {list.map((a) => {
            const isActive = activeName === a.name;
            const isEditing = currentName === a.name;
            return (
              <li
                key={a.name}
                className={`px-3 py-2 mx-2 mb-1 rounded border ${
                  isActive
                    ? "border-good bg-emerald-50"
                    : isEditing
                    ? "border-blue-300 bg-blue-50"
                    : "border-edge bg-white"
                }`}
              >
                <div className="flex items-center justify-between gap-2">
                  <span className="font-bold text-sm truncate" title={a.name}>
                    {a.name}
                  </span>
                  <span
                    className={`text-[9px] font-bold px-1.5 py-0.5 rounded ${
                      isActive
                        ? "bg-good text-white"
                        : "bg-slate-200 text-slate-600"
                    }`}
                  >
                    {isActive ? "ACTIVE" : "INACTIVE"}
                  </span>
                </div>
                <div className="text-[11px] text-muted font-mono mt-0.5">
                  {a.trading.exchange} · {a.trading.token}
                </div>
                <div className="text-[10px] text-muted font-mono leading-snug mt-1">
                  step {a.trading.grid_step} · spread {a.trading.tp_spread} ·
                  depth {a.trading.grid_depth}
                  <br />
                  qty {a.trading.per_step_qty} · {a.basket.num_baskets}{" "}
                  basket{a.basket.num_baskets === 1 ? "" : "s"}
                </div>
                <div className="flex justify-end gap-1 mt-2">
                  <button
                    className="text-[10px] px-2 py-0.5 rounded border border-edge bg-white hover:bg-slate-100 disabled:opacity-50"
                    disabled={isEditing && !running}
                    title={
                      running
                        ? "Stop the running agent first to edit its config in the form"
                        : "Load this config into the form so you can edit it"
                    }
                    onClick={() => onEdit({ ...a })}
                  >
                    ✎ Edit
                  </button>
                  <button
                    className="text-[10px] px-2 py-0.5 rounded border border-edge bg-white hover:bg-rose-50 text-danger disabled:opacity-50"
                    disabled={isActive}
                    title={
                      isActive
                        ? "Cannot delete the currently running agent"
                        : "Remove this saved agent"
                    }
                    onClick={async () => {
                      if (
                        window.confirm(`Delete saved agent "${a.name}"?`)
                      ) {
                        await deleteAgent(a.name);
                        await onDeleted();
                      }
                    }}
                  >
                    ✕
                  </button>
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </aside>
  );
}

/* ===================================================================
   TOP BAR — status, mid price, action buttons
   =================================================================== */
function TopBar({
  cfg,
  snap,
  onSaved,
}: {
  cfg: AgentConfig;
  snap: Snapshot | null;
  onSaved: () => void | Promise<void>;
}) {
  const running = snap?.running ?? false;
  const tripped = snap?.kill_switch_tripped ?? false;
  return (
    <div className="px-6 py-4 border-b border-edge bg-white">
      <div className="flex items-center justify-between gap-4 flex-wrap">
        <div>
          <h1>{cfg.name || "Active Agent"}</h1>
          <p className="text-muted text-xs mt-1">
            {snap?.exchange_name ?? "—"} · {cfg.trading.token}
          </p>
        </div>
        <div className="flex items-center gap-3">
          <StatusPill
            label={tripped ? "KILLED" : running ? "RUNNING" : "IDLE"}
            color={tripped ? "danger" : running ? "good" : "muted"}
          />
          {snap && snap.cycle_anchor > 0 && (
            <div className="hidden lg:flex flex-col items-end text-[11px] font-mono leading-tight">
              <span className="text-muted">
                anchor <span className="text-ink font-bold">{snap.cycle_anchor.toFixed(2)}</span>
              </span>
              <span className="text-muted">
                <span className="text-danger">{snap.cycle_lower.toFixed(2)}</span>
                {" ↔ "}
                <span className="text-danger">{snap.cycle_upper.toFixed(2)}</span>
              </span>
              <span className="text-muted">
                hits{" "}
                <span className="text-ink font-bold">
                  {snap.basket_hits}/{snap.max_basket_hits}
                </span>
              </span>
            </div>
          )}
          {snap && (
            <div className="font-mono text-xl font-bold tabular-nums">
              {snap.mid_price.toFixed(2)}
              <span className="text-muted text-xs font-normal ml-2">mid</span>
            </div>
          )}
          <button
            className="btn btn-ghost"
            disabled={!cfg.name?.trim()}
            title="Save this config to the sidebar (works while running too)"
            onClick={async () => {
              const r = await saveAgent(cfg);
              if (r?.error) {
                window.alert(`Save failed: ${r.error}`);
              }
              await onSaved();
            }}
          >
            ⤓ Save
          </button>
          <button
            className="btn btn-primary"
            disabled={running}
            onClick={async () => {
              await startEngine(cfg);
              await onSaved();
            }}
          >
            ▶ Start
          </button>
          <button
            className="btn btn-ghost"
            disabled={!running}
            onClick={stopEngine}
          >
            ■ Stop
          </button>
          <button
            className="btn btn-danger"
            disabled={!running || tripped}
            onClick={killSwitch}
          >
            ⚠ Kill
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
    </div>
  );
}

/// Persistent red banner shown across the whole page whenever the bot's
/// bookkeeping doesn't match the exchange's actual position. Tolerance of
/// 0.5 unit covers floating-point noise. Big drifts indicate a missed fill.
function PositionDriftBanner({ snap }: { snap: Snapshot | null }) {
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<string | null>(null);
  if (!snap) return null;
  const drift = snap.position_drift ?? 0;
  if (drift <= 0.5) return null;

  const onForceFlatten = async () => {
    if (busy) return;
    if (
      !window.confirm(
        "Emergency flatten?\n\nThis will:\n  1. Cancel every resting order\n  2. Slice every basket flat at market\n  3. Mop up any residual exchange position\n  4. Verify the exchange-side position is zero\n\nProceed?"
      )
    )
      return;
    setBusy(true);
    setResult(null);
    const r = await forceFlatten();
    setBusy(false);
    setResult(r.message);
  };

  return (
    <div className="bg-rose-50 border-y border-danger px-6 py-3 text-sm">
      <div className="flex items-center gap-4 flex-wrap">
        <span className="font-bold text-danger">
          ⚠ Position desync detected
        </span>
        <span className="font-mono">
          Bot:{" "}
          <span className="font-bold">
            {snap.bot_net_qty >= 0 ? "+" : ""}
            {snap.bot_net_qty.toFixed(2)}
          </span>
        </span>
        <span className="font-mono">
          Exchange:{" "}
          <span className="font-bold">
            {snap.exchange_position >= 0 ? "+" : ""}
            {snap.exchange_position.toFixed(2)}
          </span>
        </span>
        <span className="font-mono text-danger font-bold">
          Drift: {drift.toFixed(2)}
        </span>
        <button
          onClick={onForceFlatten}
          disabled={busy}
          className="ml-auto px-3 py-1 rounded bg-danger text-white text-xs font-bold tracking-wide disabled:opacity-50 hover:bg-rose-700"
        >
          {busy ? "FLATTENING…" : "FORCE FLATTEN"}
        </button>
      </div>
      {result && (
        <div className="mt-2 font-mono text-xs text-danger">
          → {result}
        </div>
      )}
      <div className="text-xs text-muted mt-1">
        A fill was likely missed. Click Force Flatten to cancel all orders +
        market-close the residual position, or close manually on the
        exchange.
      </div>
    </div>
  );
}

function StatusPill({ label, color }: { label: string; color: string }) {
  const colorMap: Record<string, string> = {
    good: "bg-good text-white",
    danger: "bg-danger text-white",
    warn: "bg-warn text-white",
    muted: "bg-slate-200 text-slate-600",
  };
  return (
    <span
      className={`px-3 py-1 rounded-full text-xs font-bold tracking-wider ${colorMap[color]}`}
    >
      ● {label}
    </span>
  );
}

/* ===================================================================
   ACCORDION STACK
   =================================================================== */
type SectionKey =
  | "inputs"
  | "book"
  | "orders"
  | "summary"
  | "history"
  | "rtps"
  | "baskets"
  | "risk";

function AccordionStack({
  cfg,
  setCfg,
  snap,
}: {
  cfg: AgentConfig;
  setCfg: (c: AgentConfig) => void;
  snap: Snapshot | null;
}) {
  const [openMap, setOpenMap] = useState<Record<SectionKey, boolean>>({
    inputs: false,
    book: false,
    orders: false,
    summary: true,
    history: false,
    rtps: false,
    baskets: false,
    risk: false,
  });
  const toggle = (k: SectionKey) =>
    setOpenMap((m) => ({ ...m, [k]: !m[k] }));
  const allOpen = Object.values(openMap).every(Boolean);
  const expandAll = () =>
    setOpenMap((m) => {
      const flip = !allOpen;
      return Object.fromEntries(
        Object.keys(m).map((k) => [k, flip])
      ) as Record<SectionKey, boolean>;
    });

  return (
    <div className="pt-4 space-y-3">
      <div className="flex justify-end">
        <button className="btn btn-ghost text-xs" onClick={expandAll}>
          {allOpen ? "▴ Collapse All" : "▾ Expand All"}
        </button>
      </div>

      <Accordion
        title="Agent Inputs"
        icon="≡"
        iconBg="bg-violet-100"
        iconColor="text-violet-600"
        isOpen={openMap.inputs}
        onToggle={() => toggle("inputs")}
        rightExtra={
          snap && snap.start_price > 0
            ? `start $${snap.start_price.toFixed(2)}`
            : undefined
        }
      >
        <ConfigPanel cfg={cfg} setCfg={setCfg} snap={snap} />
      </Accordion>

      <Accordion
        title="Baskets"
        icon="◫"
        iconBg="bg-blue-100"
        iconColor="text-blue-600"
        isOpen={openMap.baskets}
        onToggle={() => toggle("baskets")}
        rightExtra={
          snap
            ? `${snap.baskets.filter((b) => b.status !== "IDLE").length}/${
                snap.baskets.length
              } active`
            : undefined
        }
      >
        <BasketGrid snap={snap} />
      </Accordion>

      <Accordion
        title="Live Order Book"
        icon="≋"
        iconBg="bg-emerald-100"
        iconColor="text-emerald-600"
        isOpen={openMap.book}
        onToggle={() => toggle("book")}
        rightExtra={
          <span className="text-emerald-600 text-xs font-semibold">
            ● Live
          </span>
        }
      >
        <LiveOrderBook snap={snap} />
      </Accordion>

      <Accordion
        title="Open Orders"
        icon="≡"
        iconBg="bg-amber-100"
        iconColor="text-amber-600"
        isOpen={openMap.orders}
        onToggle={() => toggle("orders")}
        rightExtra={
          snap
            ? snap.parked_tp_count > 0
              ? `${snap.open_orders.length} · ${snap.parked_tp_count} parked`
              : `${snap.open_orders.length}`
            : undefined
        }
      >
        <OpenOrders snap={snap} />
      </Accordion>

      <Accordion
        title="Trade Summary"
        icon="▤"
        iconBg="bg-emerald-100"
        iconColor="text-emerald-600"
        isOpen={openMap.summary}
        onToggle={() => toggle("summary")}
      >
        <TradeSummary snap={snap} />
      </Accordion>

      <Accordion
        title="Trade History"
        icon="◷"
        iconBg="bg-violet-100"
        iconColor="text-violet-600"
        isOpen={openMap.history}
        onToggle={() => toggle("history")}
        rightExtra={
          snap ? `${snap.recent_fills.length} fills` : undefined
        }
      >
        <TradeHistory snap={snap} />
      </Accordion>

      <Accordion
        title="Round Trips"
        icon="↻"
        iconBg="bg-emerald-100"
        iconColor="text-emerald-600"
        isOpen={openMap.rtps}
        onToggle={() => toggle("rtps")}
        rightExtra={
          snap
            ? `${snap.trade_stats.round_trips} TP · ${snap.trade_stats.sl_count} SL`
            : undefined
        }
      >
        <RoundTripsPanel snap={snap} />
      </Accordion>

      <Accordion
        title="Risk & Log"
        icon="⚐"
        iconBg="bg-rose-100"
        iconColor="text-rose-600"
        isOpen={openMap.risk}
        onToggle={() => toggle("risk")}
      >
        <RiskAndLog snap={snap} />
      </Accordion>
    </div>
  );
}

function Accordion({
  title,
  icon,
  iconBg,
  iconColor,
  isOpen,
  onToggle,
  rightExtra,
  children,
}: {
  title: string;
  icon: string;
  iconBg: string;
  iconColor: string;
  isOpen: boolean;
  onToggle: () => void;
  rightExtra?: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <div className="panel overflow-hidden">
      <button
        onClick={onToggle}
        className="w-full flex items-center gap-3 px-5 py-4 text-left hover:bg-slate-50 transition"
      >
        <div
          className={`w-8 h-8 rounded-md grid place-items-center ${iconBg} ${iconColor} text-base font-bold`}
        >
          {icon}
        </div>
        <h2 className="flex-1">{title}</h2>
        {rightExtra && (
          <span className="text-xs text-muted font-semibold mr-2">
            {rightExtra}
          </span>
        )}
        <span className="text-muted text-sm">{isOpen ? "▴" : "▾"}</span>
      </button>
      {isOpen && <div className="px-5 pb-5 pt-1 border-t border-edge">{children}</div>}
    </div>
  );
}

/* ===================================================================
   AGENT INPUTS
   =================================================================== */
function ConfigPanel({
  cfg,
  setCfg,
  snap,
}: {
  cfg: AgentConfig;
  setCfg: (c: AgentConfig) => void;
  snap: Snapshot | null;
}) {
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

  // Live-fetched symbols for the selected exchange.
  const [symbols, setSymbols] = useState<string[]>([]);
  const [loadingSymbols, setLoadingSymbols] = useState(false);
  useEffect(() => {
    let cancelled = false;
    setLoadingSymbols(true);
    setSymbols([]);
    getInstruments(cfg.trading.exchange).then((s) => {
      if (cancelled) return;
      setSymbols(s);
      setLoadingSymbols(false);
      // If the current token isn't in the new list, default to the first one.
      if (s.length > 0 && !s.includes(cfg.trading.token)) {
        update("trading", "token", s[0]);
      }
    });
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [cfg.trading.exchange]);

  const running = snap?.running ?? false;
  const hasStarted = !!snap && snap.start_price > 0;

  return (
    <div className="space-y-5 pt-4">
      {hasStarted && <BotStatusBlock snap={snap!} />}
      <Section title="Identity">
        <Field label="Agent name (used as the saved-agent key)">
          <input
            className="input"
            value={cfg.name}
            disabled={running}
            placeholder="e.g. ETH Trend Long"
            onChange={(e) => setCfg({ ...cfg, name: e.target.value })}
          />
        </Field>
      </Section>
      <Section title="Trading">
        <div className="grid grid-cols-2 gap-3">
          <Field label="Exchange">
            <select
              className="input"
              value={cfg.trading.exchange}
              onChange={(e) =>
                update("trading", "exchange", e.target.value as any)
              }
            >
              <option value="mock">Mock (paper)</option>
              <option value="deribit">Deribit</option>
              <option value="hyperliquid">Hyperliquid</option>
            </select>
          </Field>
          <Field
            label={
              loadingSymbols
                ? "Symbol (loading…)"
                : `Symbol (${symbols.length} available)`
            }
          >
            <select
              className="input"
              value={cfg.trading.token}
              disabled={loadingSymbols || symbols.length === 0}
              onChange={(e) => update("trading", "token", e.target.value)}
            >
              {symbols.length === 0 && (
                <option value={cfg.trading.token}>
                  {loadingSymbols ? "Loading…" : "No symbols found"}
                </option>
              )}
              {symbols.map((s) => (
                <option key={s} value={s}>
                  {s}
                </option>
              ))}
            </select>
          </Field>
          <Field label="Grid distance (± from initial mid)">
            <NumInput
              v={cfg.trading.grid_distance}
              on={(v) => update("trading", "grid_distance", v)}
            />
          </Field>
          <Field label="Average (price spacing per step)">
            <NumInput
              v={cfg.trading.grid_step}
              on={(v) => update("trading", "grid_step", v)}
            />
          </Field>
          <Field label="Grid depth (levels each side)">
            <NumInput
              v={cfg.trading.grid_depth}
              on={(v) =>
                update("trading", "grid_depth", Math.max(1, Math.round(v)))
              }
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
          <Field label="Max exposure">
            <div className="input bg-slate-50 font-mono">
              {(cfg.basket.num_baskets * cfg.basket.basket_size_qty).toFixed(4)}
            </div>
          </Field>
        </div>
      </Section>

      <Section title="Kill Switch">
        <KillSwitchSanityWarning cfg={cfg} setCfg={setCfg} />
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
          <Field label="Max basket hits (cycle resets)">
            <NumInput
              v={cfg.kill_switch.max_basket_hits}
              on={(v) =>
                update(
                  "kill_switch",
                  "max_basket_hits",
                  Math.max(1, Math.round(v))
                )
              }
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
          {/* Book depth levels intentionally hidden — it's a slicing internal
              (book liquidity probe), distinct from the Trading→Grid depth above.
              Kept in config with a sane default of 5. */}
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
    </div>
  );
}

/// Warns when Kill Switch settings are clearly misconfigured so a fresh
/// session won't blow up on the first fill. Two trigger conditions:
///   1. max_position_cap < per_step_qty  (single fill instantly trips)
///   2. max_position_cap < num_baskets × basket_size_qty (cap < expected exposure)
/// Offers a one-click "Use suggested" button to fix it.
function KillSwitchSanityWarning({
  cfg,
  setCfg,
}: {
  cfg: AgentConfig;
  setCfg: (c: AgentConfig) => void;
}) {
  const maxExposure = cfg.basket.num_baskets * cfg.basket.basket_size_qty;
  const cap = cfg.kill_switch.max_position_cap;
  const per = cfg.trading.per_step_qty;

  const tooSmallForOneFill = cap < per;
  const tooSmallForFullExposure = cap < maxExposure;

  if (!tooSmallForOneFill && !tooSmallForFullExposure) return null;

  // Suggested cap = 20% headroom above max exposure.
  const suggested = Math.max(
    Math.ceil(maxExposure * 1.2),
    Math.ceil(per * 2)
  );

  const applySuggested = () => {
    setCfg({
      ...cfg,
      kill_switch: { ...cfg.kill_switch, max_position_cap: suggested },
    });
  };

  return (
    <div className="rounded-md border border-danger bg-rose-50 p-3 mb-3 text-sm">
      <div className="font-bold text-danger mb-1">
        ⚠ Max position cap is too low
      </div>
      <ul className="text-xs text-ink/80 space-y-1 list-disc ml-5 mb-2">
        {tooSmallForOneFill && (
          <li>
            <span className="font-mono">cap ({cap})</span> &lt;{" "}
            <span className="font-mono">per_step_qty ({per})</span> → a single
            fill will instantly trip the kill switch.
          </li>
        )}
        {tooSmallForFullExposure && (
          <li>
            <span className="font-mono">cap ({cap})</span> &lt;{" "}
            <span className="font-mono">
              max_exposure ({maxExposure.toFixed(4)})
            </span>{" "}
            (= # baskets × basket size) → the kill switch will trip before
            all baskets fully fill.
          </li>
        )}
      </ul>
      <div className="flex items-center gap-2 text-xs">
        <span className="text-muted">Suggested:</span>
        <span className="font-mono font-bold">{suggested}</span>
        <button
          type="button"
          className="btn btn-ghost text-xs py-1 px-3 ml-auto"
          onClick={applySuggested}
        >
          Use suggested
        </button>
      </div>
    </div>
  );
}

function BotStatusBlock({ snap }: { snap: Snapshot }) {
  const drift = snap.cycle_anchor - snap.start_price;
  const driftStr =
    drift === 0
      ? "0.00"
      : `${drift > 0 ? "+" : ""}${drift.toFixed(2)}`;
  const driftColor =
    drift > 0 ? "text-good" : drift < 0 ? "text-danger" : "text-muted";
  return (
    <div className="rounded-md border border-edge bg-slate-50 p-4">
      <div className="flex items-center justify-between mb-3">
        <h3>Bot Status</h3>
        <span className="text-xs text-muted">
          hits{" "}
          <span className="text-ink font-bold">
            {snap.basket_hits}/{snap.max_basket_hits}
          </span>
        </span>
      </div>
      <div className="grid grid-cols-2 md:grid-cols-3 gap-3">
        <StatusBox label="Bot started" v={`$${snap.start_price.toFixed(2)}`} />
        <StatusBox label="Cycle anchor" v={`$${snap.cycle_anchor.toFixed(2)}`} />
        <StatusBox
          label="Cycle drift"
          v={`${driftStr}`}
          color={driftColor}
        />
        <StatusBox
          label="Upper limit"
          v={`$${snap.cycle_upper.toFixed(2)}`}
          color="text-danger"
        />
        <StatusBox
          label="Lower limit"
          v={`$${snap.cycle_lower.toFixed(2)}`}
          color="text-danger"
        />
        <StatusBox
          label="Distance ±"
          v={`$${snap.grid_distance.toFixed(2)}`}
          color="text-accent"
        />
        <StatusBox
          label="Current mid"
          v={`$${snap.mid_price.toFixed(2)}`}
          color="text-ink"
        />
      </div>
    </div>
  );
}

function StatusBox({
  label,
  v,
  color = "text-ink",
}: {
  label: string;
  v: string;
  color?: string;
}) {
  return (
    <div className="rounded bg-white border border-edge px-3 py-2">
      <div className="text-[10px] font-bold tracking-wider uppercase text-muted">
        {label}
      </div>
      <div className={`text-base font-bold mt-0.5 tabular-nums ${color}`}>
        {v}
      </div>
    </div>
  );
}

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div>
      <h3 className="mb-3">{title}</h3>
      <div className="space-y-3">{children}</div>
    </div>
  );
}
function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div>
      <div className="label mb-1">{label}</div>
      {children}
    </div>
  );
}
function NumInput({ v, on }: { v: number; on: (n: number) => void }) {
  // Keep a local string while the user types so backspace can clear "0",
  // negative signs / decimals don't snap back, etc. Only push numeric updates
  // upstream when the string parses cleanly.
  const [raw, setRaw] = useState<string>(String(v));
  // Sync external value changes (e.g., resetting form) into our raw state.
  useEffect(() => {
    // If the parsed local matches the upstream value, leave the raw alone
    // (preserves intermediate state like "0." while typing).
    if (parseFloat(raw) !== v) {
      setRaw(String(v));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [v]);
  return (
    <input
      className="input"
      type="text"
      inputMode="decimal"
      value={raw}
      onChange={(e) => {
        const next = e.target.value;
        // Allow empty / partial numeric strings while editing.
        if (next === "" || next === "-" || next === "." || next === "-.") {
          setRaw(next);
          on(0);
          return;
        }
        // Only accept valid numeric input.
        if (/^-?\d*\.?\d*$/.test(next)) {
          setRaw(next);
          const parsed = parseFloat(next);
          if (!isNaN(parsed)) on(parsed);
        }
      }}
      onBlur={() => {
        // On blur, normalize empty back to "0".
        if (raw === "" || raw === "-" || raw === "." || raw === "-.") {
          setRaw("0");
          on(0);
        }
      }}
    />
  );
}
function Toggle({ v, on }: { v: boolean; on: (b: boolean) => void }) {
  return (
    <button
      onClick={() => on(!v)}
      className={`input text-left font-semibold ${
        v ? "text-accent" : "text-muted"
      }`}
    >
      {v ? "● ENABLED" : "○ DISABLED"}
    </button>
  );
}

/* ===================================================================
   BASKETS
   =================================================================== */
function BasketGrid({ snap }: { snap: Snapshot | null }) {
  if (!snap) return <EmptyState text="No data yet" />;
  return (
    <div className="grid grid-cols-2 md:grid-cols-3 gap-3 pt-4">
      {snap.baskets.map((b) => (
        <BasketCard key={b.basket_id} b={b} />
      ))}
    </div>
  );
}

function BasketCard({ b }: { b: Snapshot["baskets"][0] }) {
  const colorMap: Record<string, string> = {
    IDLE: "border-edge text-muted",
    ACTIVE: "border-blue-300 text-blue-700",
    TPRECYCLING: "border-emerald-300 text-emerald-700",
    // HIT = just cycle-SL'd; displayed as KILLED but basket can still trade.
    HIT: "border-rose-300 text-rose-700 opacity-80",
    KILLED: "border-rose-400 text-rose-800 opacity-60",
  };
  const sideBadge =
    b.side === "LONG"
      ? "bg-emerald-100 text-emerald-700"
      : "bg-rose-100 text-rose-700";
  // Render HIT as "KILLED" to match the user-facing terminology, but reserve
  // a separate look for the permanent KILLED (more faded, darker red).
  const displayStatus = b.status === "HIT" ? "KILLED" : b.status;
  return (
    <div className={`border rounded-md p-3 bg-white ${colorMap[b.status]}`}>
      <div className="flex justify-between items-center mb-2 gap-2">
        <span className="font-bold">#{b.index}</span>
        <span
          className={`text-[10px] font-bold px-2 py-0.5 rounded ${sideBadge}`}
        >
          {b.side}
        </span>
        <span className="text-[10px] tracking-widest ml-auto">
          {displayStatus}
        </span>
      </div>
      <div className="font-mono space-y-1.5 text-ink/85">
        <Row k="open" v={b.open_qty.toFixed(4)} />
        <Row k="max" v={b.max_qty.toFixed(4)} />
        <Row k="avg" v={b.avg_price > 0 ? b.avg_price.toFixed(2) : "—"} />
        <Row
          k="PnL"
          v={b.realized_pnl.toFixed(4)}
          highlight={b.realized_pnl !== 0}
        />
        <Row k="fills/tp" v={`${b.fills_count}/${b.tp_count}`} />
        {/* Per-basket SL — set on first entry fill, fixed for basket life. */}
        <div className="pt-2 mt-1 border-t border-edge/60 space-y-1.5">
          <Row
            k="anchor"
            v={b.anchor_price > 0 ? b.anchor_price.toFixed(2) : "—"}
          />
          <Row
            k="upper SL"
            v={b.upper_sl > 0 ? b.upper_sl.toFixed(2) : "—"}
          />
          <Row
            k="lower SL"
            v={b.lower_sl > 0 ? b.lower_sl.toFixed(2) : "—"}
          />
        </div>
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
    <div className="flex justify-between items-baseline">
      <span className="text-muted text-xs">{k}</span>
      <span
        className={`text-sm font-bold tabular-nums ${
          highlight ? "text-ink" : "text-ink/80"
        }`}
      >
        {v}
      </span>
    </div>
  );
}

/* ===================================================================
   LIVE ORDER BOOK — derived from mid for now (best-effort visual)
   =================================================================== */
function LiveOrderBook({ snap }: { snap: Snapshot | null }) {
  if (!snap || snap.mid_price <= 0) {
    return <EmptyState text="Waiting for live mid-price…" />;
  }
  return (
    <div className="pt-4 grid grid-cols-2 gap-4">
      <div>
        <div className="label mb-2">Bids</div>
        <div className="font-mono text-xs space-y-1">
          {[1, 2, 3, 4, 5].map((i) => (
            <div
              key={i}
              className="flex justify-between py-1 px-2 bg-emerald-50/60 rounded"
            >
              <span className="text-emerald-700">
                {(snap.mid_price - i * 0.5).toFixed(2)}
              </span>
              <span className="text-muted">—</span>
            </div>
          ))}
        </div>
      </div>
      <div>
        <div className="label mb-2">Asks</div>
        <div className="font-mono text-xs space-y-1">
          {[1, 2, 3, 4, 5].map((i) => (
            <div
              key={i}
              className="flex justify-between py-1 px-2 bg-rose-50/60 rounded"
            >
              <span className="text-rose-700">
                {(snap.mid_price + i * 0.5).toFixed(2)}
              </span>
              <span className="text-muted">—</span>
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}

/* ===================================================================
   OPEN ORDERS
   =================================================================== */
function OpenOrders({ snap }: { snap: Snapshot | null }) {
  if (!snap) return <EmptyState text="No data yet" />;
  if (snap.open_orders.length === 0)
    return <EmptyState text="No open orders" />;
  return (
    <div className="pt-4 overflow-x-auto">
      <table className="w-full text-xs font-mono">
        <thead className="text-muted text-[11px] uppercase">
          <tr>
            <Th>Side</Th>
            <Th>Purpose</Th>
            <Th right>Price</Th>
            <Th right>Qty</Th>
          </tr>
        </thead>
        <tbody>
          {snap.open_orders.map((o) => (
            <tr key={o.order_id} className="border-t border-edge">
              <Td>
                <span
                  className={
                    o.side === "BUY"
                      ? "text-good font-semibold"
                      : "text-danger font-semibold"
                  }
                >
                  {o.side}
                </span>
              </Td>
              <Td>
                <span className="text-muted">{o.purpose}</span>
              </Td>
              <Td right>{o.price.toFixed(2)}</Td>
              <Td right>{o.qty.toFixed(4)}</Td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

/* ===================================================================
   TRADE SUMMARY — the 16-KPI grid (the heart of the screenshot)
   =================================================================== */
function TradeSummary({ snap }: { snap: Snapshot | null }) {
  const ts: TradeStats = snap?.trade_stats ?? {
    start_time: 0,
    duration_seconds: 0,
    total_pnl: 0,
    net_pnl: 0,
    rtp_pnl: 0,
    sl_pnl: 0,
    total_fees: 0,
    round_trips: 0,
    sl_count: 0,
    rtp_per_hour: 0,
    pnl_per_hour: 0,
    buy_vwap: 0,
    sell_vwap: 0,
    total_volume: 0,
    buy_volume: 0,
    sell_volume: 0,
    buy_qty: 0,
    sell_qty: 0,
    net_qty: 0,
    total_fills: 0,
    total_buys: 0,
    total_sells: 0,
  };
  const durationStr = useMemo(
    () => formatDuration(ts.duration_seconds),
    [ts.duration_seconds]
  );
  const netColor = colorForSigned(ts.net_pnl);
  const pnlPerHourColor = colorForSigned(ts.pnl_per_hour);
  const netQtyColor = colorForSigned(ts.net_qty);

  return (
    <div className="pt-4 grid grid-cols-2 md:grid-cols-4 gap-3">
      {/* Row 1 — headline PnLs (separated per user spec). */}
      <KPI label="Net PnL" v={fmt(ts.net_pnl, 4)} color={netColor} />
      <KPI label="RTP PnL (TPs)" v={fmt(ts.rtp_pnl, 4)} color="text-good" />
      <KPI label="SL PnL" v={fmt(ts.sl_pnl, 4)} color="text-danger" />
      <KPI label="PnL / Hour" v={fmt(ts.pnl_per_hour, 4)} color={pnlPerHourColor} />

      {/* Row 2 — trip counts and timing. */}
      <KPI label="Round Trips" v={ts.round_trips.toString()} color="text-good" />
      {/* SL Count = number of basket hits (cycle resets), NOT individual
          SL exit fills. Each cycle SL hit increments by exactly 1. */}
      <KPI
        label="SL Count (basket hits)"
        v={(snap?.basket_hits ?? 0).toString()}
        color="text-danger"
      />
      <KPI label="RTP / Hour" v={Math.round(ts.rtp_per_hour).toString()} />
      <KPI label="Duration" v={durationStr} />

      {/* Row 3 — VWAPs and volumes (USD). */}
      <KPI label="Buy VWAP" v={fmt(ts.buy_vwap, 4)} color="text-good" />
      <KPI label="Sell VWAP" v={fmt(ts.sell_vwap, 4)} color="text-danger" />
      <KPI label="Total Volume" v={fmt(ts.total_volume, 2)} color="text-muted" />
      <KPI label="Total Fees" v={fmt(ts.total_fees, 4)} color="text-muted" />

      {/* Row 4 — buys/sells split. */}
      <KPI label="Buy Volume" v={fmt(ts.buy_volume, 2)} color="text-good" />
      <KPI label="Sell Volume" v={fmt(ts.sell_volume, 2)} color="text-danger" />
      <KPI label="Total Buys" v={ts.total_buys.toString()} color="text-good" />
      <KPI label="Total Sells" v={ts.total_sells.toString()} color="text-danger" />

      {/* Row 5 — qty + fills. */}
      <KPI label="Buy Qty" v={fmt(ts.buy_qty, 4)} color="text-good" />
      <KPI label="Sell Qty" v={fmt(ts.sell_qty, 4)} color="text-danger" />
      <KPI label="Net Qty" v={fmt(ts.net_qty, 4)} color={netQtyColor} />
      <KPI label="Total Fills" v={ts.total_fills.toString()} />
    </div>
  );
}

function KPI({
  label,
  v,
  color = "text-ink",
}: {
  label: string;
  v: string;
  color?: string;
}) {
  return (
    <div className="rounded-md border border-edge bg-white p-3">
      <div className="text-[10px] font-bold text-muted tracking-wider uppercase">
        {label}
      </div>
      <div className={`font-mono text-lg font-bold mt-1 tabular-nums ${color}`}>
        {v}
      </div>
    </div>
  );
}

/* ===================================================================
   TRADE HISTORY — one row per logical ORDER (partials aggregated)
   =================================================================== */
type AggregatedFill = {
  /** Synthetic key for React + CSV; first fill_id of the group. */
  group_id: string;
  order_id: string;
  basket_id: string;
  side: "BUY" | "SELL";
  purpose: string;
  /** Volume-weighted average price across all partials. */
  price: number;
  /** Sum of all partial qtys. */
  qty: number;
  /** Sum of all partial fees. */
  fee: number;
  /** Earliest partial timestamp — when the order first started filling. */
  timestamp: number;
  /** How many partials rolled up into this row (≥ 1). */
  partials: number;
};

/// Collapse consecutive partial-fills for the same `order_id` into one
/// aggregated row. Deribit reports every partial chunk as a separate trade
/// (e.g. one $2000 limit fills as 1166 + 396 + 438), so the raw list shows
/// the same order N times. The user wants ONE row per logical order.
function aggregateFills(
  fills: Snapshot["recent_fills"]
): AggregatedFill[] {
  const byOrder = new Map<string, AggregatedFill>();
  for (const f of fills) {
    const existing = byOrder.get(f.order_id);
    if (existing) {
      // Running volume-weighted price: (Σ p·q + p·q) / (Σ q + q)
      const newQty = existing.qty + f.qty;
      existing.price =
        (existing.price * existing.qty + f.price * f.qty) / (newQty || 1);
      existing.qty = newQty;
      existing.fee += f.fee;
      existing.timestamp = Math.min(existing.timestamp, f.timestamp);
      existing.partials += 1;
    } else {
      byOrder.set(f.order_id, {
        group_id: f.fill_id,
        order_id: f.order_id,
        basket_id: f.basket_id,
        side: f.side,
        purpose: f.purpose,
        price: f.price,
        qty: f.qty,
        fee: f.fee,
        timestamp: f.timestamp,
        partials: 1,
      });
    }
  }
  // Sort by earliest timestamp ascending (matches original Fill ordering).
  return Array.from(byOrder.values()).sort((a, b) => a.timestamp - b.timestamp);
}

function TradeHistory({ snap }: { snap: Snapshot | null }) {
  const fills = snap?.recent_fills ?? [];
  if (fills.length === 0) {
    return <EmptyState text="No fills yet" />;
  }
  const aggregated = aggregateFills(fills);
  // Newest first in the UI.
  const reversed = [...aggregated].reverse();
  return (
    <div className="pt-4">
      <div className="flex justify-between items-center mb-3">
        <div className="text-xs text-muted">
          {aggregated.length} order{aggregated.length === 1 ? "" : "s"}
          {fills.length !== aggregated.length && (
            <span className="ml-1">
              (from {fills.length} partial{fills.length === 1 ? "" : "s"})
            </span>
          )}
          {fills.length >= 1000 && (
            <span className="ml-1 text-warn">(buffer cap — older fills evicted)</span>
          )}
        </div>
        <button
          className="btn btn-ghost text-xs"
          onClick={() => downloadAggregatedCsv(aggregated)}
        >
          ⬇ Download CSV
        </button>
      </div>
      <div className="overflow-x-auto max-h-96 overflow-y-auto">
        <table className="w-full text-sm">
          <thead className="text-muted text-[11px] uppercase sticky top-0 bg-white">
            <tr>
              <Th>#</Th>
              <Th>Time</Th>
              <Th>Basket</Th>
              <Th>Side</Th>
              <Th>Purpose</Th>
              <Th right>Price</Th>
              <Th right>Qty</Th>
              <Th right>Fee</Th>
            </tr>
          </thead>
          <tbody>
            {reversed.map((f, idx) => {
              const n = aggregated.length - idx;
              const sideCls =
                f.side === "BUY" ? "text-good font-bold" : "text-danger font-bold";
              const purposeLabel = purposeText(f.purpose);
              const purposeCls = purposeColor(f.purpose);
              const partialBadge =
                f.partials > 1 ? (
                  <span
                    className="ml-1 text-[10px] text-muted"
                    title={`Filled in ${f.partials} partials`}
                  >
                    ×{f.partials}
                  </span>
                ) : null;
              return (
                <tr key={f.group_id} className="border-t border-edge">
                  <Td>
                    <span className="text-muted font-bold">{n}</span>
                  </Td>
                  <Td>
                    <span className="text-muted">
                      {new Date(f.timestamp).toLocaleTimeString()}
                    </span>
                  </Td>
                  <Td>
                    <span className="text-muted">
                      {f.basket_id.slice(0, 4)}…
                    </span>
                  </Td>
                  <Td>
                    <span className={sideCls}>{f.side}</span>
                  </Td>
                  <Td>
                    <span className={purposeCls + " font-semibold"}>{purposeLabel}</span>
                  </Td>
                  <Td right>
                    <span className="font-bold">${f.price.toFixed(2)}</span>
                  </Td>
                  <Td right>
                    <span className="font-bold">{f.qty.toFixed(4)}</span>
                    {partialBadge}
                  </Td>
                  <Td right>
                    <span className="text-muted">{f.fee.toFixed(4)}</span>
                  </Td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </div>
  );
}

function purposeText(p: string): string {
  switch (p) {
    case "entry":
      return "ENTRY";
    case "take_profit":
      return "TP";
    case "stop_loss_exit":
      return "SL";
    case "kill_switch_exit":
      return "KILL";
    default:
      return p.toUpperCase();
  }
}
function purposeColor(p: string): string {
  switch (p) {
    case "entry":
      return "text-accent";
    case "take_profit":
      return "text-good";
    case "stop_loss_exit":
    case "kill_switch_exit":
      return "text-danger";
    default:
      return "text-muted";
  }
}

/// CSV export for aggregated fills — one row per logical order (matches the
/// Trade History view, partials rolled up by order_id).
function downloadAggregatedCsv(rows: AggregatedFill[]) {
  if (rows.length === 0) return;
  const header =
    "ORDER_COUNT,TIME,SIDE,PURPOSE,AVG_PRICE,QTY,FEE,PARTIALS,ORDER_ID,BASKET_ID";
  const csvRows = rows.map((f, i) => {
    return [
      i + 1,
      new Date(f.timestamp).toISOString(),
      f.side,
      purposeText(f.purpose),
      f.price.toFixed(6),
      f.qty.toFixed(6),
      f.fee.toFixed(6),
      f.partials,
      f.order_id,
      f.basket_id,
    ].join(",");
  });
  const csv = [header, ...csvRows].join("\n");
  const blob = new Blob([csv], { type: "text/csv;charset=utf-8" });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  const ts = new Date().toISOString().replace(/[:.]/g, "-");
  a.download = `fills_${ts}.csv`;
  document.body.appendChild(a);
  a.click();
  document.body.removeChild(a);
  URL.revokeObjectURL(url);
}

/* ===================================================================
   ROUND TRIPS — paired entry + closing fill, with PnL
   =================================================================== */
function RoundTripsPanel({ snap }: { snap: Snapshot | null }) {
  const hasData = !!snap && snap.round_trips.length > 0;
  // snap.round_trips comes newest-first; reverse for chronological CSV.
  const chronological: RoundTrip[] = hasData
    ? [...snap!.round_trips].reverse()
    : [];

  return (
    <div className="pt-4">
      <div className="flex justify-between items-center mb-3">
        <div className="text-xs text-muted">
          {hasData
            ? `${chronological.length} completed round-trips`
            : "No completed round-trips yet"}
        </div>
        <button
          className="btn btn-ghost text-xs"
          disabled={!hasData}
          onClick={() => downloadRoundTripsCsv(chronological)}
        >
          ⬇ Download CSV
        </button>
      </div>
      {hasData && (
        <div className="overflow-x-auto max-h-96 overflow-y-auto">
          <table className="w-full text-sm">
            <thead className="text-muted text-[11px] uppercase sticky top-0 bg-white">
              <tr>
                <Th>#</Th>
                <Th>Side</Th>
                <Th right>Buy</Th>
                <Th right>Sell</Th>
                <Th right>Qty</Th>
                <Th right>PnL</Th>
                <Th right>Volume</Th>
                <Th right>When</Th>
              </tr>
            </thead>
            <tbody>
              {/* Display newest first in the UI table; CSV is chronological. */}
              {snap!.round_trips.map((r, idx) => {
                const rtpCount = snap!.round_trips.length - idx;
                const isLong = r.entry_side === "BUY";
                // Long: buy first → sell.   Short: sell first → buy.
                const buyPx = isLong ? r.entry_price : r.exit_price;
                const sellPx = isLong ? r.exit_price : r.entry_price;
                const sideText = isLong ? "BUY → SELL" : "SELL → BUY";
                const sideClass = isLong ? "text-good" : "text-danger";
                return (
                  <tr key={r.rtp_id} className="border-t border-edge">
                    <Td>
                      <span className="text-muted font-bold">{rtpCount}</span>
                    </Td>
                    <Td>
                      <span
                        className={`${sideClass} font-bold`}
                        title={
                          r.is_take_profit
                            ? "Closed by take-profit"
                            : "Closed by stop-loss"
                        }
                      >
                        {sideText}
                        {!r.is_take_profit && (
                          <span className="ml-1 text-[10px] text-danger">
                            (SL)
                          </span>
                        )}
                      </span>
                    </Td>
                    <Td right>
                      <span className="text-good font-bold">
                        ${buyPx.toFixed(4)}
                      </span>
                    </Td>
                    <Td right>
                      <span className="text-danger font-bold">
                        ${sellPx.toFixed(4)}
                      </span>
                    </Td>
                    <Td right>
                      <span className="font-bold">{r.qty.toFixed(4)}</span>
                    </Td>
                    <Td right>
                      <span
                        className={`font-bold ${colorForSigned(r.pnl)}`}
                      >
                        {r.pnl >= 0 ? "+" : ""}${r.pnl.toFixed(4)}
                      </span>
                    </Td>
                    <Td right>
                      <span className="text-muted font-bold">
                        ${r.volume.toFixed(2)}
                      </span>
                    </Td>
                    <Td right>
                      <span className="text-muted">
                        {new Date(r.exit_time).toLocaleTimeString()}
                      </span>
                    </Td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

/// Build a CSV blob and trigger a download. Columns mirror the UI layout:
/// RTP_COUNT, SIDE, BUY, SELL, QTY, PNL, VOLUME, FEES, CLOSE, TIME
function downloadRoundTripsCsv(rtps: RoundTrip[]) {
  if (rtps.length === 0) return;
  const header =
    "RTP_COUNT,SIDE,BUY,SELL,QTY,PNL,VOLUME,FEES,CLOSE,TIME";
  const rows = rtps.map((r, i) => {
    const isLong = r.entry_side === "BUY";
    const side = isLong ? "BUY->SELL" : "SELL->BUY";
    const buyPx = isLong ? r.entry_price : r.exit_price;
    const sellPx = isLong ? r.exit_price : r.entry_price;
    const close = r.is_take_profit ? "TP" : "SL";
    const time = new Date(r.exit_time).toISOString();
    return [
      i + 1,
      side,
      buyPx,
      sellPx,
      r.qty,
      r.pnl.toFixed(6),
      r.volume.toFixed(2),
      r.fees.toFixed(6),
      close,
      time,
    ].join(",");
  });
  const csv = [header, ...rows].join("\n");
  const blob = new Blob([csv], { type: "text/csv;charset=utf-8" });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  const ts = new Date().toISOString().replace(/[:.]/g, "-");
  a.download = `trade_history_${ts}.csv`;
  document.body.appendChild(a);
  a.click();
  document.body.removeChild(a);
  URL.revokeObjectURL(url);
}

/* ===================================================================
   RISK + LOG
   =================================================================== */
function RiskAndLog({ snap }: { snap: Snapshot | null }) {
  if (!snap) return <EmptyState text="No data yet" />;
  return (
    <div className="pt-4 grid grid-cols-1 xl:grid-cols-[1fr_2fr] gap-6">
      <div>
        <h3 className="mb-3">Risk Engine</h3>
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
          <div className="mt-3 text-danger text-xs font-semibold">
            BREACH: {snap.risk.breach_reason}
          </div>
        )}
        {snap.kill_switch_reason && (
          <div className="mt-3 text-danger text-xs font-semibold">
            KILL: {snap.kill_switch_reason}
          </div>
        )}
      </div>
      <div>
        <h3 className="mb-3">Engine Log</h3>
        <div className="overflow-y-auto max-h-72 text-xs font-mono space-y-1">
          {[...snap.log].reverse().map((l, i) => (
            <div key={i} className="text-ink/75">
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
      <span
        className={
          ok ? "text-good font-semibold" : "text-danger font-semibold"
        }
      >
        {ok ? "● OK" : "○ BREACH"}
      </span>
    </div>
  );
}

/* ===================================================================
   Small utilities
   =================================================================== */
function Th({
  children,
  right,
}: {
  children: React.ReactNode;
  right?: boolean;
}) {
  return (
    <th className={`px-2 py-1 ${right ? "text-right" : "text-left"} font-bold`}>
      {children}
    </th>
  );
}
function Td({
  children,
  right,
}: {
  children: React.ReactNode;
  right?: boolean;
}) {
  return (
    <td className={`px-2 py-1.5 ${right ? "text-right" : "text-left"}`}>
      {children}
    </td>
  );
}

function EmptyState({ text }: { text: string }) {
  return <div className="text-muted text-sm py-6 text-center">{text}</div>;
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

function fmt(n: number, digits: number): string {
  if (!isFinite(n)) return "—";
  return n.toLocaleString("en-US", {
    minimumFractionDigits: digits,
    maximumFractionDigits: digits,
  });
}
function colorForSigned(n: number): string {
  if (n > 0) return "text-good";
  if (n < 0) return "text-danger";
  return "text-ink";
}
function formatDuration(sec: number): string {
  if (sec <= 0) return "0s";
  const h = Math.floor(sec / 3600);
  const m = Math.floor((sec % 3600) / 60);
  const s = sec % 60;
  if (h > 0) return `${(h + m / 60).toFixed(2)}h`;
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}
