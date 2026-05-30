# Handoff — basket-grid trading bot

## 1. Project goal

Symmetric two-sided grid trading bot on Deribit (testnet/mainnet) and Hyperliquid.

- **Backend**: Rust (axum HTTP API on `:8080`)
- **Frontend**: Next.js 14 (`:3000`), polls `/api/snapshot` every 250 ms
- **Repo path** (Windows): `C:\Users\Dell\OneDrive\BTC,ETH,BTC  HIDEEN - STATARB\basket-grid\basket-grid`
- **Git remote**: `origin/main` → https://github.com/AkshayKollimarla/GRID-BASKET-SL
- **Memory shortcut**: "commit" means commit **AND** push to `origin/main`

**Trading model:**
- `grid_depth = N` → N buy levels below mid + N sell levels above mid (max 2N open orders at a time, plus TPs)
- `grid_distance` → cycle SL distance from `cycle_anchor` (= mid at cycle start, fixed per cycle)
- `grid_step` (UI: "Average") → spacing between grid levels
- When mid escapes `anchor ± grid_distance` → soft cycle reset: flatten all baskets via slicing, soft-reset to status `HIT`, new anchor = current mid, `basket_hits += 1`
- After `max_basket_hits` cycles → kill switch trips permanently (status → `KILLED`)
- Each entry fill places **one TP** at `fill_price ± tp_spread` (exact price, not snapped to grid)
- TP fill PnL = `tp_spread × qty` (fixed, divided by avg_price for inverse contracts)

## 2. Current bug

**Position drift** between bot's tracked `net_qty` and Deribit's actual position.

Symptom: red banner at top of page shows `Bot: -X | Exchange: -Y | Drift: Z`. The bot misses fills that happen on Deribit.

The latest fixes (sync-race + `recently_cancelled` buffer + `cancel_all` at startup + immediate-fill handling in placement response + `first_tick_done` gate + cancel error handling) should have closed all known paths. **Verify by restarting and watching the drift banner stays hidden across 10+ minutes of trading.**

If drift returns, the log will tell you the exact cause:
- `⚠ ORPHAN TRADE` → a trade arrived for an order never in our tracking → look at the placement that PRECEDED that order_id
- `⚠ ORPHAN OPEN ORDER` → an order rests on Deribit but the bot doesn't know
- `⚠ FILL RECEIVER LAGGED` → broadcast channel dropped fills (would need mpsc switch)
- `TICK FILL recovered from recently_cancelled` → NOT a bug; this is the save in action

## 3. Files modified (the touch-heavy ones this session)

| File | Purpose / what's in it |
|---|---|
| `backend/src/engine.rs` | `EngineHandle`, main loop, `process_fill` (entry → TP placement reactive at exact `fill.price ± tp_spread`), `check_cycle_boundary` (soft reset), position drift detector (every ~3s) |
| `backend/src/exchanges/deribit.rs` | `place_maker_only` (handles immediate-fill trades in response), `tick` (first-tick gate, processed_trade_ids dedup, recently_cancelled fallback), `cancel` (only removes on Ok; on Err keeps tracking), `position` (queries `/private/get_position`) |
| `backend/src/exchanges/mod.rs` | Added `position()` to `Exchange` trait with default 0.0 |
| `backend/src/engines/grid.rs` | **Entries-only** — places exactly `depth` buys + `depth` sells around mid. Cancels entries on wrong side and TPs > 3·step from mid. Does NOT plan or place TPs. |
| `backend/src/engines/trade_tracker.rs` | FIFO lot matching for PnL; separate `rtp_pnl` vs `sl_pnl`; separate `buy_notional` for correct VWAP |
| `backend/src/engines/kill_switch.rs` | Direction-correct flatten (long→Sell, short→Buy); critical-log on failure |
| `backend/src/engines/basket_manager.rs` | Half-long / half-short split |
| `backend/src/engines/risk.rs` | Breach message includes cap value |
| `backend/src/models/basket.rs` | Statuses: `Idle / Active / TpRecycling / Hit / Killed`. `Hit` = visually KILLED but tradeable next cycle. `soft_reset()` sets `Hit`. `apply_tp_fill` uses fixed `tp_spread × qty`. |
| `backend/src/models/config.rs` | `grid_distance`, `grid_depth`, `max_basket_hits` fields; `default_demo` updated |
| `backend/src/models/trade_stats.rs` | `RoundTrip.entry_side`, `TradeStats` with `rtp_pnl`/`sl_pnl`/`net_pnl` |
| `backend/.env` | DERIBIT_CLIENT_ID/SECRET, DERIBIT_TESTNET=true, HYPERLIQUID_*  (gitignored) |
| `frontend/src/app/page.tsx` | Sidebar + accordions, Trade Summary KPIs, Trade History (all fills), Round Trips panel, Bot Status block in Agent Inputs, **`PositionDriftBanner`**, **`KillSwitchSanityWarning`**, `NumInput` with proper backspace |
| `frontend/src/lib/api.ts` | Snapshot type with `exchange_position`, `bot_net_qty`, `position_drift`, `start_price`, `cycle_anchor`, etc. |

## 4. Important logic rules — DO NOT BREAK

1. **TP placement lives in `process_fill`** (engine.rs `OrderPurpose::Entry` branch), NOT in the grid tick. Each entry fill → one TP at exact `fill.price ± tp_spread`.
2. **Grid tick places entries only.** It cancels stale entries (wrong side of mid) and stale TPs (>3·step from mid). It never plans TPs.
3. **Cancel race fix:** `Deribit::cancel` only removes from `open_orders` on `Ok` response. On `Err` (e.g., `not_open_order`), it KEEPS the order in tracking. Order ALSO goes into `recently_cancelled` (30s TTL).
4. **Sync race fix:** when `tick()` open-orders sync detects an order is no longer in Deribit's open list, it MOVES it to `recently_cancelled` (does NOT delete). Late trades can still match via the fallback.
5. **First-tick gate:** the very first poll after startup just marks all returned `trade_id`s as processed without emitting fills (so pre-session history doesn't pollute the log or get mis-attributed).
6. **`processed_trade_ids` dedup** (capped at 5000 with eviction down to 1000) is what prevents double-counting when `place_maker_only`'s response trades array AND `get_user_trades` both report the same trade.
7. **TP dedup**: before placing a TP, `process_fill` checks if THIS basket already has a TP within 0.5 tick of the target price. If yes, skip.
8. **Cycle anchor is FIXED within a cycle.** Grid placement uses `mid` (trails price). Cycle SL boundaries use `cycle_anchor ± grid_distance` (anchor doesn't move until reset).
9. **Inverse vs linear**: Deribit is inverse → `is_inverse = true` → PnL divides by avg/entry price. Hyperliquid + mock are linear. Set once in `EngineHandle::new` and propagated to `Basket` and `TradeTracker`.
10. **Basket status flow**: `Idle → Active` (entry fill) → `Active ↔ TpRecycling` (partial TPs) → `Hit` (soft reset, tradeable) → back to `Active` on next entry. `Killed` is PERMANENT (kill switch only).
11. **SL Count in UI = `basket_hits`** (cycle resets), NOT `trade_stats.sl_count` (individual SL fill count). The user explicitly wants 1 basket hit = 1 SL.
12. **Backend tick 300 ms, frontend snapshot poll 250 ms.** Don't speed up further without considering Deribit rate limits.

## 5. Commands

```powershell
# Terminal 1 — backend (Rust)
cd "C:\Users\Dell\OneDrive\BTC,ETH,BTC  HIDEEN - STATARB\basket-grid\basket-grid\backend"
cargo run --release

# Terminal 2 — frontend (Next.js)
cd "C:\Users\Dell\OneDrive\BTC,ETH,BTC  HIDEEN - STATARB\basket-grid\basket-grid\frontend"
npm run dev
# then open http://localhost:3000

# Quick compile check (no run)
cargo check --manifest-path "...\backend\Cargo.toml"

# Commit (per user rule, "commit" means commit + push)
git -C "...\basket-grid" add <specific files>
git -C "...\basket-grid" commit -m "..."
git -C "...\basket-grid" push origin main
```

To enable debug logs:
```powershell
$env:RUST_LOG = "basket_grid_engine=debug"; cargo run --release
```

## 6. What NOT to change

- **The `cancel()` Ok/Err split.** Don't go back to unconditional `open_orders.remove`. That's the bug we just fixed.
- **TP placement formula.** Use exact `fill_price ± tp_spread`. Don't snap to grid. Don't use basket avg.
- **TP placement location.** Stays in `process_fill`. Don't move back to grid tick.
- **`recently_cancelled` 30s TTL.** Lower → late fills get dropped again. Higher → memory grows + small risk of double-counting if `processed_trade_ids` eviction collides.
- **First-tick gate.** Without it, every restart floods the log with pre-session orphan trades.
- **`Basket::Hit` vs `Basket::Killed`.** `Hit` = tradeable next cycle, `Killed` = permanent (kill switch only). UI shows both as "KILLED" but they behave differently in `find_basket_with_capacity` and `all_killed()`.
- **`cancel_all` at engine start.** Without it, orphan orders from previous sessions silently fill into our positions.
- **Direction-correct flatten in kill_switch.rs.** Long basket → SELL to close, Short basket → BUY to close. Reverting to hardcoded `Side::Sell` breaks short closures.
- **`Side` enum derives `Hash`.** Required for the grid's dedup `HashSet<(Side, i64)>`.
- **`max_position_cap` form warning.** Without it, users set it to 0.5 and the kill switch trips on the first fill. The `KillSwitchSanityWarning` component exists for this.

## 7. Next debugging step

**Test the latest fix stack:**

1. Stop backend (Ctrl+C). Manually close any open Deribit testnet position via the web UI.
2. `cargo run --release`. Confirm these log lines appear in order on startup:
   - `Deribit network: TESTNET`
   - `Cleared orphan orders at startup (clean slate)`
   - `First-tick gate: marked N pre-session trades as processed`
3. Refresh frontend. Verify `KillSwitchSanityWarning` is hidden (means `max_position_cap >= num_baskets × basket_size_qty`).
4. Click Reset → Start in UI.
5. Watch the bot for 10–15 minutes of trading.

**Verify success signals:**
- Drift banner stays hidden (`exchange_position == bot_net_qty` always)
- Trade Summary `SL Count (basket hits)` matches the `hits X/Y` in the top bar
- Open Orders panel shows at most 2N + (open basket TP count) — no duplicate TPs at same (basket, price)
- Log periodically shows `TICK FILL recovered from recently_cancelled` (proves the fallback is catching cancel races)
- Log NEVER shows `⚠ ORPHAN TRADE` for an order that THIS session placed

**If drift returns**, in this order:
1. Search the log for `⚠ ORPHAN TRADE`. Get the `order_id`.
2. Search upward in the log for that same `order_id` in a `PLACEMENT response` line. If found → bug is in the cancellation/sync removal path (the order WAS tracked, then lost).
3. If not found → bug is in placement → never tracked. Check for a placement that Err'd around the orphan trade's timestamp.
4. If neither → likely a Deribit-side oddity (manual order, different account leakage).

**If drift persists across all these checks**, the next architectural step is to **replace HTTP polling with Deribit's WebSocket `user.trades.future.btc.raw` subscription**. That gives push-based fills with no polling gap. Code lives in `wss://test.deribit.com/ws/api/v2`. This is a significant rewrite of `deribit.rs` and probably warrants its own session.

**Other open items**:
- The `Live Order Book` accordion in the frontend is a placeholder (shows ±0.50 around mid). Wire up `OrderBook` into the snapshot if user requests it.
- Round Trip CSV columns currently: `RTP_COUNT, SIDE, BUY, SELL, QTY, PNL, VOLUME, FEES, CLOSE, TIME`. User has been satisfied with these so far.
- `place_market_reduce_only` in deribit.rs uses `unwrap_or(qty)` for `filled_amount` — should reconsider if partial slicing fills surface.
