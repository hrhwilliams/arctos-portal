use std::path::Path;

use arctos_portal::{app, state::AppState};
use tokio::net::TcpListener;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("debug"))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let ip = "0.0.0.0";
    let port = 2334;
    let listener = TcpListener::bind((ip, port)).await.expect("failed to bind");

    // the dump's filename carries the extraction date, which is what the UI
    // shows as "data current as of" (D2 — stale by design, so say so). The
    // Parquet itself is written out of band by `docs/build_parquet.py`.
    let snapshot_date = "2026-03-09";

    let app_state = AppState::new(
        "http://localhost:9200",
        Path::new("docs/data/code-tables"),
        snapshot_date,
    )
    .expect("failed to build schema");

    let app = app::App::new(app_state);

    app.serve(listener).await
}
