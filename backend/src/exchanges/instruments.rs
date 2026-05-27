// Lightweight instrument-list fetchers used by the /api/instruments endpoint.
// These don't touch the running engine — they make their own short-lived
// public requests so the frontend can populate symbol dropdowns.

use anyhow::{anyhow, Result};
use hyperliquid_rust_sdk::{BaseUrl, InfoClient};
use serde_json::Value;

fn env_flag(key: &str) -> bool {
    std::env::var(key)
        .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
        .unwrap_or(false)
}

/// Fetch all perpetual instrument names from Deribit (testnet or mainnet
/// depending on DERIBIT_TESTNET env var). We iterate the most-common
/// currencies because Deribit's `get_instruments` requires a currency arg.
pub async fn fetch_deribit_perps() -> Result<Vec<String>> {
    let testnet = env_flag("DERIBIT_TESTNET");
    let base = if testnet {
        "https://test.deribit.com/api/v2"
    } else {
        "https://www.deribit.com/api/v2"
    };
    let currencies = ["BTC", "ETH", "SOL", "MATIC", "XRP", "USDC"];
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()?;
    let mut names: Vec<String> = Vec::new();
    for c in currencies {
        let url = format!(
            "{}/public/get_instruments?currency={}&kind=future&expired=false",
            base, c
        );
        let resp: Value = match http.get(&url).send().await {
            Ok(r) => r.json().await.unwrap_or(Value::Null),
            Err(_) => continue, // currency not listed on this env; skip
        };
        if let Some(arr) = resp.get("result").and_then(|r| r.as_array()) {
            for inst in arr {
                // Only the perpetuals (not dated futures).
                let is_perp = inst
                    .get("settlement_period")
                    .and_then(|s| s.as_str())
                    .map(|s| s == "perpetual")
                    .unwrap_or(false);
                if is_perp {
                    if let Some(name) = inst.get("instrument_name").and_then(|n| n.as_str())
                    {
                        names.push(name.to_string());
                    }
                }
            }
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

/// Fetch all perpetual coin names from Hyperliquid's info-meta endpoint.
pub async fn fetch_hyperliquid_perps() -> Result<Vec<String>> {
    let testnet = env_flag("HYPERLIQUID_TESTNET");
    let base = if testnet {
        BaseUrl::Testnet
    } else {
        BaseUrl::Mainnet
    };
    let info = InfoClient::new(None, Some(base))
        .await
        .map_err(|e| anyhow!("hyperliquid InfoClient init failed: {}", e))?;
    let meta = info
        .meta()
        .await
        .map_err(|e| anyhow!("hyperliquid meta() failed: {}", e))?;
    let mut names: Vec<String> = meta.universe.into_iter().map(|a| a.name).collect();
    names.sort();
    Ok(names)
}
