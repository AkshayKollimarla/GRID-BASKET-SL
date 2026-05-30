// Deribit connector — REST API, OAuth2 client_credentials grant.
//
// Endpoints:
//   Mainnet: https://www.deribit.com/api/v2
//   Testnet: https://test.deribit.com/api/v2
//
// Auth: public/auth with grant_type=client_credentials returns an access_token.
// Trading: private/buy, private/sell with post_only=true (maker-only) or
//          time_in_force=immediate_or_cancel + reduce_only=true (market exits).

use super::Exchange;
use crate::models::{
    Fill, Order as MyOrder, OrderBook, OrderBookLevel, OrderPurpose, OrderStatus, OrderType,
    Side as MySide,
};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use dashmap::DashMap;
use parking_lot::RwLock;
use reqwest::Client as HttpClient;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::{debug, warn};
use uuid::Uuid;

/// Conservative fallback contract size if the live fetch from Deribit
/// fails. The bot will warn loudly and use this number so an order can
/// still get placed. The REAL value is fetched at startup via
/// `public/get_instrument` and stored on the client.
fn fallback_contract_size(instrument: &str) -> f64 {
    let inst = instrument.to_ascii_uppercase();
    if inst.starts_with("BTC-") {
        10.0
    } else if inst.starts_with("ETH-") {
        1.0
    } else {
        1.0
    }
}

pub struct DeribitClient {
    http: HttpClient,
    base_url: String,
    client_id: String,
    client_secret: String,
    instrument: String, // e.g. "BTC-PERPETUAL"
    /// Live contract size (= USD increment for the `amount` field) fetched
    /// from Deribit's public/get_instrument on startup. Same env (mainnet
    /// vs testnet) and same instrument as the order placement endpoint, so
    /// rounding always matches whatever the venue actually requires.
    contract_size: f64,
    /// Live minimum trade amount. Often equal to `contract_size` but not
    /// always — kept separate so we floor an order at the real minimum.
    min_trade_amount: f64,
    token: RwLock<Option<(String, Instant)>>, // (access_token, expires_at)
    // Map deribit order_id (string) -> our internal Order
    open_orders: Arc<DashMap<String, MyOrder>>,
    /// Set of trade_ids we've already emitted a Fill for. Prevents double-
    /// counting when get_user_trades returns the same trade across ticks.
    /// Critical for handling partial fills: a single order can have several
    /// trade_ids, all of which must be processed individually.
    processed_trade_ids: Arc<DashMap<String, ()>>,
    /// Orders we recently cancelled (or tried to cancel) — kept around for
    /// ~30s so any in-flight fills that arrive AFTER cancellation can still
    /// be matched to their basket/purpose. Fixes the cancel-race orphan
    /// trade bug.
    recently_cancelled: Arc<DashMap<String, (MyOrder, Instant)>>,
    /// True once the first tick has run. Used to mark all pre-existing
    /// trades as processed at startup so old history doesn't pollute the
    /// orphan-trade log or risk being mis-attributed.
    first_tick_done: Arc<parking_lot::Mutex<bool>>,
    fills_tx: broadcast::Sender<Fill>,
    // Map our basket_id -> set of deribit order ids
    basket_orders: Arc<DashMap<Uuid, Vec<String>>>,
}

// AuthResp / AuthResult were removed in favor of tolerant Value-based parsing
// in ensure_token(). The previous strict types caused a confusing
// "missing field `result`" error whenever Deribit returned an error response
// (e.g. invalid credentials, IP block, key on the wrong env).

impl DeribitClient {
    pub async fn new(
        client_id: String,
        client_secret: String,
        instrument: String,
        mainnet: bool,
    ) -> (Arc<Self>, broadcast::Receiver<Fill>) {
        let base_url = if mainnet {
            "https://www.deribit.com/api/v2".to_string()
        } else {
            "https://test.deribit.com/api/v2".to_string()
        };
        let http = HttpClient::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();

        // Fetch live contract_size and min_trade_amount from Deribit so the
        // bot rounds to the SAME increment the venue requires. Falls back
        // to hardcoded defaults if the call fails (we never want startup
        // to hard-block on an exchange info fetch).
        let (contract_size, min_trade_amount) =
            match Self::fetch_instrument_spec(&http, &base_url, &instrument).await {
                Ok((cs, mta)) => {
                    tracing::info!(
                        instrument = %instrument,
                        env = if mainnet { "mainnet" } else { "testnet" },
                        contract_size = cs,
                        min_trade_amount = mta,
                        "Deribit instrument spec loaded"
                    );
                    (cs, mta)
                }
                Err(e) => {
                    let fb = fallback_contract_size(&instrument);
                    tracing::warn!(
                        ?e,
                        fallback = fb,
                        "Deribit get_instrument failed — using fallback contract size"
                    );
                    (fb, fb)
                }
            };

        let (tx, rx) = broadcast::channel(1024);
        let me = Arc::new(Self {
            http,
            base_url,
            client_id,
            client_secret,
            instrument,
            contract_size,
            min_trade_amount,
            token: RwLock::new(None),
            open_orders: Arc::new(DashMap::new()),
            processed_trade_ids: Arc::new(DashMap::new()),
            recently_cancelled: Arc::new(DashMap::new()),
            first_tick_done: Arc::new(parking_lot::Mutex::new(false)),
            fills_tx: tx,
            basket_orders: Arc::new(DashMap::new()),
        });
        (me, rx)
    }

    /// Query Deribit for the live contract_size + min_trade_amount of the
    /// configured instrument. Used at startup; doesn't require auth.
    async fn fetch_instrument_spec(
        http: &HttpClient,
        base_url: &str,
        instrument: &str,
    ) -> Result<(f64, f64)> {
        let url = format!(
            "{}/public/get_instrument?instrument_name={}",
            base_url, instrument
        );
        let resp: Value = http.get(&url).send().await?.json().await?;
        if let Some(err) = resp.get("error") {
            return Err(anyhow!("get_instrument error: {}", err));
        }
        let r = &resp["result"];
        let cs = r["contract_size"]
            .as_f64()
            .ok_or_else(|| anyhow!("missing contract_size in get_instrument response"))?;
        // min_trade_amount may equal contract_size; if absent, fall back to it.
        let mta = r["min_trade_amount"].as_f64().unwrap_or(cs);
        Ok((cs, mta))
    }

    /// Round a requested qty to the nearest contract multiple AND clamp to
    /// the venue's min_trade_amount. Both values came from Deribit at
    /// startup, so this is always correct for the live environment.
    fn snap_qty(&self, qty: f64) -> f64 {
        let cs = if self.contract_size > 0.0 {
            self.contract_size
        } else {
            1.0
        };
        let n = (qty / cs).round();
        let rounded = n * cs;
        rounded.max(self.min_trade_amount).max(cs)
    }

    async fn ensure_token(&self) -> Result<String> {
        if let Some((tok, exp)) = self.token.read().as_ref() {
            if Instant::now() < *exp {
                return Ok(tok.clone());
            }
        }
        let url = format!("{}/public/auth", self.base_url);
        // Parse as untyped Value so we can surface the real Deribit error
        // message (e.g. "invalid_credentials") instead of getting a confusing
        // "missing field `result`" deserialization error.
        let resp: Value = self
            .http
            .get(&url)
            .query(&[
                ("grant_type", "client_credentials"),
                ("client_id", &self.client_id),
                ("client_secret", &self.client_secret),
            ])
            .send()
            .await?
            .json()
            .await?;
        if let Some(err) = resp.get("error") {
            return Err(anyhow!(
                "Deribit auth failed ({}): {}",
                self.base_url,
                err
            ));
        }
        let access_token = resp["result"]["access_token"]
            .as_str()
            .ok_or_else(|| anyhow!("Deribit auth: missing access_token in response: {}", resp))?
            .to_string();
        let expires_in = resp["result"]["expires_in"].as_u64().unwrap_or(900);
        let exp = Instant::now() + Duration::from_secs(expires_in.saturating_sub(30));
        *self.token.write() = Some((access_token.clone(), exp));
        Ok(access_token)
    }

    async fn private_call(&self, endpoint: &str, params: Value) -> Result<Value> {
        let token = self.ensure_token().await?;
        let url = format!("{}/{}", self.base_url, endpoint);
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": endpoint,
            "params": params,
        });
        let resp: Value = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;
        if let Some(err) = resp.get("error") {
            return Err(anyhow!("Deribit error on {}: {}", endpoint, err));
        }
        Ok(resp)
    }

    async fn public_call(&self, endpoint: &str, params: Value) -> Result<Value> {
        let url = format!("{}/{}", self.base_url, endpoint);
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": endpoint,
            "params": params,
        });
        let resp: Value = self.http.post(&url).json(&body).send().await?.json().await?;
        if let Some(err) = resp.get("error") {
            return Err(anyhow!("Deribit error on {}: {}", endpoint, err));
        }
        Ok(resp)
    }
}

#[async_trait]
impl Exchange for DeribitClient {
    async fn name(&self) -> &'static str {
        "Deribit"
    }

    async fn orderbook(&self) -> OrderBook {
        match self
            .public_call(
                "public/get_order_book",
                json!({ "instrument_name": self.instrument, "depth": 10 }),
            )
            .await
        {
            Ok(resp) => {
                let r = &resp["result"];
                let parse = |arr: &Value| -> Vec<OrderBookLevel> {
                    arr.as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|lvl| {
                                    let arr = lvl.as_array()?;
                                    Some(OrderBookLevel {
                                        price: arr.get(0)?.as_f64()?,
                                        size: arr.get(1)?.as_f64()?,
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                };
                let bids = parse(&r["bids"]);
                let asks = parse(&r["asks"]);
                let mid = match (bids.first(), asks.first()) {
                    (Some(b), Some(a)) => (b.price + a.price) / 2.0,
                    _ => r["index_price"].as_f64().unwrap_or(0.0),
                };
                OrderBook {
                    bids,
                    asks,
                    mid,
                    timestamp: chrono::Utc::now().timestamp_millis(),
                }
            }
            Err(e) => {
                warn!(?e, "deribit orderbook fetch failed");
                OrderBook {
                    bids: vec![],
                    asks: vec![],
                    mid: 0.0,
                    timestamp: chrono::Utc::now().timestamp_millis(),
                }
            }
        }
    }

    async fn place_maker_only(
        &self,
        side: MySide,
        price: f64,
        qty: f64,
        basket_id: Uuid,
        purpose: OrderPurpose,
    ) -> Result<MyOrder> {
        let endpoint = match side {
            MySide::Buy => "private/buy",
            MySide::Sell => "private/sell",
        };
        // Snap qty to the LIVE contract size + min_trade_amount we fetched
        // from Deribit on startup. Same rule applies on mainnet and testnet
        // because the value comes from the actual venue.
        let qty_rounded = self.snap_qty(qty);
        let resp = self
            .private_call(
                endpoint,
                json!({
                    "instrument_name": self.instrument,
                    "amount": qty_rounded,
                    "type": "limit",
                    "price": price,
                    "post_only": true,
                    "reject_post_only": true,  // reject instead of converting to taker
                }),
            )
            .await?;
        let order_id = resp["result"]["order"]["order_id"]
            .as_str()
            .ok_or_else(|| anyhow!("no order_id in deribit response"))?
            .to_string();
        let state = resp["result"]["order"]["order_state"]
            .as_str()
            .unwrap_or("open");
        let trades_count = resp["result"]["trades"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        // Always log the placement outcome so we have an audit trail.
        tracing::info!(
            order_id = %order_id,
            side = ?side,
            price,
            qty = qty_rounded,
            purpose = ?purpose,
            state,
            trades_in_response = trades_count,
            "PLACEMENT response"
        );
        let status = match state {
            "open" => OrderStatus::Open,
            "filled" => OrderStatus::Filled,
            "rejected" => OrderStatus::Rejected,
            _ => OrderStatus::Pending,
        };
        let order = MyOrder {
            order_id: Uuid::new_v4(),
            basket_id,
            side,
            order_type: OrderType::LimitMaker,
            purpose,
            price,
            qty,
            filled_qty: 0.0,
            status,
            created_at: chrono::Utc::now().timestamp_millis(),
        };

        // ── Emit fills for ANY trades in the placement response. ───────────
        // Deribit returns `trades: [...]` in the response when the order
        // (partially or fully) filled at the moment of placement. We MUST
        // emit Fill events for these so basket bookkeeping stays in sync
        // with the exchange position. Bug fixed: previously the trades
        // array was ignored when state == "filled", so the bot's net qty
        // diverged from Deribit's actual position.
        if let Some(trades_arr) = resp["result"]["trades"].as_array() {
            for t in trades_arr {
                let trade_id = t["trade_id"].as_str().map(String::from);
                if let Some(ref tid) = trade_id {
                    // Pre-mark so the subsequent tick() poll doesn't
                    // double-process this same trade.
                    self.processed_trade_ids.insert(tid.clone(), ());
                }
                let trade_price = t["price"].as_f64().unwrap_or(price);
                let trade_qty = t["amount"].as_f64().unwrap_or(0.0);
                let trade_fee = t["fee"].as_f64().unwrap_or(0.0).abs();
                if trade_qty <= 0.0 {
                    continue;
                }
                let fill = Fill {
                    fill_id: Uuid::new_v4(),
                    order_id: order.order_id,
                    basket_id,
                    purpose,
                    side,
                    price: trade_price,
                    qty: trade_qty,
                    fee: trade_fee,
                    timestamp: chrono::Utc::now().timestamp_millis(),
                };
                let _ = self.fills_tx.send(fill);
                tracing::info!(
                    side = ?side,
                    purpose = ?purpose,
                    price = trade_price,
                    qty = trade_qty,
                    "Deribit immediate-fill at placement → emitted Fill event"
                );
            }
        }

        // ── Track the order if it's still alive on the book. ──────────────
        // For "filled" orders (fully matched at placement) we DON'T track —
        // their fills were already emitted above. For "open" (and the rare
        // post-only-rejected "rejected" state where Deribit chose not to
        // place) we either track or skip accordingly.
        if status == OrderStatus::Open {
            self.open_orders.insert(order_id.clone(), order.clone());
            self.basket_orders
                .entry(basket_id)
                .or_default()
                .push(order_id);
        }
        debug!(?order, "deribit maker order placed");
        Ok(order)
    }

    async fn place_market_reduce_only(
        &self,
        side: MySide,
        qty: f64,
        basket_id: Uuid,
        purpose: OrderPurpose,
    ) -> Result<MyOrder> {
        let endpoint = match side {
            MySide::Buy => "private/buy",
            MySide::Sell => "private/sell",
        };
        // Same snapping rule as maker orders — uses the live contract size
        // we fetched from Deribit at startup.
        let qty_rounded = self.snap_qty(qty);
        let resp = self
            .private_call(
                endpoint,
                json!({
                    "instrument_name": self.instrument,
                    "amount": qty_rounded,
                    "type": "market",
                    "reduce_only": true,
                }),
            )
            .await?;
        let avg_price = resp["result"]["order"]["average_price"]
            .as_f64()
            .unwrap_or(0.0);
        let filled = resp["result"]["order"]["filled_amount"]
            .as_f64()
            .unwrap_or(qty);
        let order = MyOrder {
            order_id: Uuid::new_v4(),
            basket_id,
            side,
            order_type: OrderType::MarketReduceOnly,
            purpose,
            price: avg_price,
            qty,
            filled_qty: filled,
            status: OrderStatus::Filled,
            created_at: chrono::Utc::now().timestamp_millis(),
        };
        // Emit a fill event so the engine accounts for it.
        let fill = Fill {
            fill_id: Uuid::new_v4(),
            order_id: order.order_id,
            basket_id,
            purpose,
            side,
            price: avg_price,
            qty: filled,
            // Fee not returned on this synchronous response; tick() will pick
            // up the authoritative fee from the user-trades feed shortly.
            fee: 0.0,
            timestamp: chrono::Utc::now().timestamp_millis(),
        };
        let _ = self.fills_tx.send(fill);
        Ok(order)
    }

    async fn cancel(&self, _order_id: Uuid) -> Result<()> {
        // Map internal Uuid -> deribit string id.
        let entry = self
            .open_orders
            .iter()
            .find(|e| e.value().order_id == _order_id)
            .map(|e| (e.key().clone(), e.value().clone()));
        let Some((id, our_order)) = entry else {
            return Ok(());
        };

        // Send the cancel. Three possible outcomes:
        //   (a) Deribit Ok response — cancel accepted. Remove from open_orders
        //       AND stash in recently_cancelled so any in-flight trades that
        //       arrive over the next ~30s can still be matched.
        //   (b) Deribit Err response (e.g., order already filled, network
        //       blip). DON'T remove from open_orders — the order MAY still
        //       be alive on the exchange; if we drop it from tracking, any
        //       future fill becomes an orphan trade.
        let resp = self
            .private_call("private/cancel", json!({ "order_id": id }))
            .await;
        match resp {
            Ok(_) => {
                // Deribit accepted the cancel. Move the order from
                // open_orders into recently_cancelled so any in-flight fill
                // (already happening on the exchange when we cancelled) can
                // still resolve to the right basket/purpose.
                self.open_orders.remove(&id);
                self.recently_cancelled
                    .insert(id.clone(), (our_order, Instant::now()));
            }
            Err(e) => {
                tracing::warn!(
                    ?e,
                    deribit_oid = %id,
                    "cancel call errored — KEEPING order in tracking; \
                     it may still be on the exchange and fill later"
                );
            }
        }
        Ok(())
    }

    async fn cancel_all(&self) -> Result<()> {
        let _ = self
            .private_call(
                "private/cancel_all_by_instrument",
                json!({ "instrument_name": self.instrument, "type": "all" }),
            )
            .await;
        self.open_orders.clear();
        self.basket_orders.clear();
        Ok(())
    }

    async fn open_orders(&self) -> Vec<MyOrder> {
        self.open_orders.iter().map(|e| e.value().clone()).collect()
    }

    /// Fetch the live exchange position for the configured instrument.
    /// Returns signed size: positive=long, negative=short. Units = same as
    /// the order `amount` field (USD for Deribit BTC-PERP). 0.0 if no
    /// position OR if the fetch fails (we don't want to noisily fail tick).
    async fn position(&self) -> f64 {
        let token = match self.ensure_token().await {
            Ok(t) => t,
            Err(_) => return 0.0,
        };
        let url = format!("{}/private/get_position", self.base_url);
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "private/get_position",
            "params": { "instrument_name": self.instrument }
        });
        let resp: Value = match self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
        {
            Ok(r) => match r.json().await {
                Ok(v) => v,
                Err(_) => return 0.0,
            },
            Err(_) => return 0.0,
        };
        // Deribit returns `size` (signed) for the position. Positive = long,
        // negative = short. Units = USD for inverse perps like BTC-PERPETUAL.
        resp["result"]["size"].as_f64().unwrap_or(0.0)
    }

    async fn tick(&self) {
        // ── 1. Poll Deribit for recent trades and emit fills ───────────────
        let token = match self.ensure_token().await {
            Ok(t) => t,
            Err(_) => return,
        };
        let url = format!("{}/private/get_user_trades_by_instrument", self.base_url);
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "private/get_user_trades_by_instrument",
            "params": { "instrument_name": self.instrument, "count": 50 }
        });
        let resp: Value = match self
            .http
            .post(&url)
            .bearer_auth(token.clone())
            .json(&body)
            .send()
            .await
        {
            Ok(r) => match r.json().await {
                Ok(v) => v,
                Err(_) => return,
            },
            Err(_) => return,
        };
        if let Some(trades) = resp["result"]["trades"].as_array() {
            // ── FIRST-TICK GATE ───────────────────────────────────────
            // The very first poll after startup just MARKS all pre-existing
            // trades as processed (they're from before our session) and
            // emits NO fills. Without this, every restart would see 50+
            // historical trades and incorrectly flood the orphan log.
            let mut first_tick = self.first_tick_done.lock();
            if !*first_tick {
                let mut seeded = 0usize;
                for t in trades {
                    if let Some(tid) = t["trade_id"].as_str() {
                        self.processed_trade_ids.insert(tid.to_string(), ());
                        seeded += 1;
                    }
                }
                *first_tick = true;
                drop(first_tick);
                tracing::info!(
                    seeded,
                    "First-tick gate: marked {} pre-session trades as processed (no fills emitted)",
                    seeded
                );
                // Fall through to the open-orders sync below, but don't
                // emit any fills.
            } else {
                drop(first_tick);
            for t in trades {
                let trade_id = match t["trade_id"].as_str() {
                    Some(s) => s.to_string(),
                    None => continue,
                };
                let dex_order_id = t["order_id"].as_str().unwrap_or("");
                let trade_price = t["price"].as_f64().unwrap_or(0.0);
                let trade_qty = t["amount"].as_f64().unwrap_or(0.0);
                let trade_side = t["direction"].as_str().unwrap_or("?");

                // Dedupe.
                if self
                    .processed_trade_ids
                    .insert(trade_id.clone(), ())
                    .is_some()
                {
                    continue; // already processed in a previous tick
                }

                // Order lookup: check open_orders first, then the recently
                // cancelled buffer (catches trades that arrive AFTER we've
                // cancelled the order — a common race on volatile markets).
                let our_order = match self.open_orders.get(dex_order_id) {
                    Some(o) => o.value().clone(),
                    None => match self.recently_cancelled.get(dex_order_id) {
                        Some(entry) => {
                            let (order, _ts) = entry.value().clone();
                            tracing::info!(
                                trade_id = %trade_id,
                                order_id = %dex_order_id,
                                "TICK FILL recovered from recently_cancelled (cancel-race save)"
                            );
                            order
                        }
                        None => {
                            tracing::error!(
                                trade_id = %trade_id,
                                order_id = %dex_order_id,
                                side = %trade_side,
                                price = trade_price,
                                qty = trade_qty,
                                tracked_orders = self.open_orders.len(),
                                recently_cancelled = self.recently_cancelled.len(),
                                "⚠ ORPHAN TRADE — trade detected for order_id NOT in our tracking. \
                                 Possible cause: (a) place_maker_only Err'd after Deribit accepted, or \
                                 (b) leftover from a previous session that survived restart."
                            );
                            continue;
                        }
                    },
                };
                tracing::info!(
                    trade_id = %trade_id,
                    order_id = %dex_order_id,
                    side = ?our_order.side,
                    purpose = ?our_order.purpose,
                    price = trade_price,
                    qty = trade_qty,
                    "TICK FILL detected → emitting"
                );
                let price = t["price"].as_f64().unwrap_or(our_order.price);
                let qty = t["amount"].as_f64().unwrap_or(our_order.qty);
                let fee = t["fee"].as_f64().unwrap_or(0.0).abs();
                let fill = Fill {
                    fill_id: Uuid::new_v4(),
                    order_id: our_order.order_id,
                    basket_id: our_order.basket_id,
                    purpose: our_order.purpose,
                    side: our_order.side,
                    price,
                    qty,
                    fee,
                    timestamp: chrono::Utc::now().timestamp_millis(),
                };
                let _ = self.fills_tx.send(fill);
                // NOTE: do NOT remove dex_order_id from open_orders here.
                // Partial fills emit multiple trades for the same order_id.
                // The order is cleaned up by the open-orders sync below.
            }
            } // end else (first_tick_done branch)
        }

        // Clean up the recently_cancelled buffer — entries older than 30s.
        // Any in-flight fill that didn't arrive within 30s is presumed lost.
        let cutoff = Instant::now()
            .checked_sub(Duration::from_secs(30))
            .unwrap_or_else(Instant::now);
        let stale: Vec<String> = self
            .recently_cancelled
            .iter()
            .filter(|e| e.value().1 < cutoff)
            .map(|e| e.key().clone())
            .collect();
        for id in stale {
            self.recently_cancelled.remove(&id);
        }

        // ── 2. Sync open_orders against exchange (clean up fully-filled
        // or cancelled orders so they don't linger in our tracking map) ───
        let url = format!("{}/private/get_open_orders_by_instrument", self.base_url);
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "private/get_open_orders_by_instrument",
            "params": { "instrument_name": self.instrument }
        });
        if let Ok(r) = self.http.post(&url).bearer_auth(token).json(&body).send().await {
            if let Ok(v) = r.json::<Value>().await {
                if let Some(arr) = v["result"].as_array() {
                    let still_open: std::collections::HashSet<String> = arr
                        .iter()
                        .filter_map(|o| o["order_id"].as_str().map(String::from))
                        .collect();
                    let our_ids: std::collections::HashSet<String> =
                        self.open_orders.iter().map(|e| e.key().clone()).collect();

                    // Move tracked orders no longer open on the exchange
                    // into recently_cancelled (NOT direct delete). They
                    // may have just filled — the trade feed will catch up
                    // within a few seconds, and the lookup fallback can
                    // still match the fill to the right basket/purpose.
                    for id in &our_ids {
                        if !still_open.contains(id) {
                            if let Some((_, order)) = self.open_orders.remove(id) {
                                self.recently_cancelled
                                    .insert(id.clone(), (order, Instant::now()));
                            }
                        }
                    }

                    // ORPHAN OPEN ORDER detection: orders on the exchange
                    // that we don't track AND aren't in our recently_cancelled
                    // buffer (those are race conditions, not real orphans).
                    for o in arr {
                        let id = match o["order_id"].as_str() {
                            Some(s) => s,
                            None => continue,
                        };
                        if our_ids.contains(id) {
                            continue;
                        }
                        if self.recently_cancelled.contains_key(id) {
                            // Cancel-in-progress; Deribit's open list still
                            // shows it because the cancel hasn't propagated.
                            continue;
                        }
                        tracing::error!(
                            order_id = %id,
                            side = %o["direction"].as_str().unwrap_or("?"),
                            price = o["price"].as_f64().unwrap_or(0.0),
                            amount = o["amount"].as_f64().unwrap_or(0.0),
                            filled_amount = o["filled_amount"].as_f64().unwrap_or(0.0),
                            "⚠ ORPHAN OPEN ORDER on exchange — not tracked by bot. \
                             Cancel it manually on Deribit, \
                             or Stop and Restart (cancel_all on startup will clear it)."
                        );
                    }
                    tracing::debug!(
                        bot_tracked = our_ids.len(),
                        exchange_open = still_open.len(),
                        recently_cancelled = self.recently_cancelled.len(),
                        "open-orders reconciliation"
                    );
                }
            }
        }

        // ── 3. Cap the processed_trade_ids set so it doesn't grow forever.
        // 5000 is plenty for any realistic session.
        if self.processed_trade_ids.len() > 5000 {
            // Drop arbitrary entries down to 1000 (we won't re-process them
            // because they're far older than the 50-trade lookback window).
            let to_drop: Vec<String> = self
                .processed_trade_ids
                .iter()
                .take(self.processed_trade_ids.len().saturating_sub(1000))
                .map(|e| e.key().clone())
                .collect();
            for id in to_drop {
                self.processed_trade_ids.remove(&id);
            }
        }
    }
}
