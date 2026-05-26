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

    tracing::warn!("==============================================");
    tracing::warn!("  BASKET GRID ENGINE  —  MAINNET");
    tracing::warn!("  Real money mode. Start with tiny sizes.");
    tracing::warn!("==============================================");

    let app = api::router();
    let addr = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("API listening on http://{}", addr);
    tracing::info!("Open the UI at  http://localhost:3000");
    axum::serve(listener, app).await?;
    Ok(())
}
