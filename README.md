# Basket Grid Trading Engine — MAINNET

Maker-only basket grid trading engine — Rust backend + Next.js UI.

Supports:
- **Mock** (paper trading, default — no keys needed, for verifying code)
- **Deribit mainnet** (OAuth2)
- **Hyperliquid mainnet** (EIP-712 signing)

> ⚠️ **This bot trades real money on mainnet.** Start with tiny basket sizes.
> The kill-switch button cancels everything and flattens all positions — use it when in doubt.

## Architecture

- 6 baskets with lifecycle: `IDLE → ACTIVE → TP_RECYCLING → KILLED`
- Maker-only entries + maker-only TPs
- Per-basket stop-loss → cancel orders → emergency slicing → kill basket permanently
- Global kill switch → cancel all → flatten all → lock
- Risk engine: exposure, daily loss, API disconnect, missing SL, slippage, liquidity, runaway
- TP recycles the same basket qty. SL kills the basket. Bot stops when all are killed.

---

## Prerequisites

- **Rust** (stable): https://rustup.rs
- **Node.js 18+**: https://nodejs.org

---

## Step 1 — Set up your .env file

```bash
cd backend
cp .env.example .env
```

Open `backend/.env` in VS Code and follow the instructions. You only need to fill in the exchanges you'll actually use.

### Deribit setup
1. Sign in at https://www.deribit.com
2. Account → API → Create a new API key with `trade` + `read` scope
3. **Strongly recommended:** restrict the key by IP address (Account → API → Edit → Allowed IPs)
4. Copy Client ID and Client Secret into `.env`

### Hyperliquid setup
1. Go to https://app.hyperliquid.xyz/API
2. Connect your main wallet (the one holding USDC)
3. Click **Generate** under "API Wallets"
4. **Click Approve** to authorize the API wallet (this signs a transaction — critical step)
5. Copy the generated private key into `.env` as `HYPERLIQUID_PRIVATE_KEY`
6. Put your main wallet address in `HYPERLIQUID_MAIN_WALLET`

The API wallet can only place orders. It cannot withdraw funds. Still, treat the key as a secret.

---

## Step 2 — Run

Two terminals (Git Bash works fine on Windows):

```bash
# Terminal 1 — backend
cd backend
cargo run --release
```

First build takes 5–10 minutes (downloads Rust + crypto crates).

```bash
# Terminal 2 — frontend
cd frontend
npm install
npm run dev
```

Open **http://localhost:3000**

---

## Step 3 — Configure in the UI

The form is on the left side. Three things to set carefully:

### Exchange dropdown
- `mock` — paper trading, doesn't touch real exchanges (try this first!)
- `deribit` — real Deribit mainnet
- `hyperliquid` — real Hyperliquid mainnet

### Token field (must match the exchange's format)
- **Deribit**: `BTC-PERPETUAL`, `ETH-PERPETUAL`, `SOL-PERPETUAL`
- **Hyperliquid**: `BTC`, `ETH`, `SOL`
- **Mock**: anything

### Grid range
Look up the **current live price** of your token first. Set `Grid lower` and `Grid upper` to bracket it.

Example for BTC at ~$95,000:
- Grid lower: 92000
- Grid upper: 98000
- Grid step: 200  (= 31 grid levels)
- Per-step qty: 0.001  (= ~$95 per fill, small)

### Recommended first-run config on mainnet

| Field | Value | Why |
|---|---|---|
| Per-step qty | 0.001 BTC | ~$95 risk per fill |
| Basket size | 0.005 BTC | 5 fills per basket = ~$475 |
| Num baskets | 2 | total exposure ~$950 |
| SL distance | 500 | tight enough to fire |
| Max position cap | 0.011 | matches num × basket size + buffer |
| Max daily loss | 100 | small dollar stop |

---

## Step 4 — Click Submit & Start

Watch the dashboard:
- Mid price ticks from the live exchange
- Open Orders fill with maker buys below mid
- Fills appear in the Recent Fills panel
- Baskets light up green (`ACTIVE`) then bright green (`TP_RECYCLING`)
- If price drops to a basket's SL → emergency slicing kicks in → basket goes red (`KILLED`)

---

## Stop / Kill / Reset

- **Stop** (gray): stops placing new orders but leaves existing ones alone
- **KILL SWITCH** (red): cancels all open orders + market-closes every position immediately
- **Reset**: clears the kill-switch lock so you can restart

---

## Safety Checklist before clicking Submit & Start on mainnet

- [ ] You ran it in `mock` mode first and the dashboard worked
- [ ] You ran it on `deribit` or `hyperliquid` with tiny size first
- [ ] You restricted your Deribit API key by IP
- [ ] Your Hyperliquid API wallet is a separate sub-key, not your main wallet
- [ ] The grid range brackets the current price (otherwise nothing will happen)
- [ ] The token symbol matches the exchange's format
- [ ] Per-step qty and basket size are small enough that losing them won't hurt
- [ ] You've located the KILL SWITCH button and know where it is

---

## Golden Rules (enforced in code)

- Entries = maker-only (Deribit `post_only` / Hyperliquid `ALO`)
- TPs = maker-only
- SL exits & kill-switch exits = reduce-only via emergency slicing
- TP recycles same basket qty
- Killed basket never trades again
- Bot stops when **all** baskets are killed
