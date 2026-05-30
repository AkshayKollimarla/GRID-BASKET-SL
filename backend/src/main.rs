use basket_grid_engine::api;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env (silently — only required if using Deribit or Hyperliquid).
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Resolve the actual network from .env so the startup banner reflects
    // reality. We ONLY mention an exchange if its credentials are
    // configured (otherwise it's unreachable and irrelevant) — the
    // previous version warned about Hyperliquid MAINNET for users who
    // weren't using Hyperliquid at all, scaring them about real money
    // when only the test exchange was actually in play.
    let env_flag = |key: &str| {
        std::env::var(key)
            .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
            .unwrap_or(false)
    };
    let env_present = |key: &str| {
        std::env::var(key).map(|v| !v.is_empty()).unwrap_or(false)
    };
    let deribit_configured = env_present("DERIBIT_CLIENT_ID")
        && env_present("DERIBIT_CLIENT_SECRET");
    let hyperliquid_configured = env_present("HYPERLIQUID_PRIVATE_KEY");
    let deribit_net = if env_flag("DERIBIT_TESTNET") { "TESTNET" } else { "MAINNET" };
    let hyperliquid_net = if env_flag("HYPERLIQUID_TESTNET") { "TESTNET" } else { "MAINNET" };
    let any_mainnet = (deribit_configured && deribit_net == "MAINNET")
        || (hyperliquid_configured && hyperliquid_net == "MAINNET");

    tracing::warn!("==============================================");
    tracing::warn!("  BASKET GRID ENGINE");
    if deribit_configured {
        tracing::warn!("    Deribit:     {}", deribit_net);
    }
    if hyperliquid_configured {
        tracing::warn!("    Hyperliquid: {}", hyperliquid_net);
    }
    if !deribit_configured && !hyperliquid_configured {
        tracing::warn!("    No live-exchange credentials in .env — mock only.");
    }
    if any_mainnet {
        tracing::warn!("  ⚠ MAINNET exchange configured — real money.");
        tracing::warn!("    Start with tiny sizes.");
    } else if deribit_configured || hyperliquid_configured {
        tracing::warn!("  Testnet only — no real money.");
    }
    tracing::warn!("==============================================");

    let app = api::router();
    let addr = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("API listening on http://{}", addr);
    tracing::info!("Open the UI at  http://localhost:3000");
    axum::serve(listener, app).await?;
    Ok(())
}
