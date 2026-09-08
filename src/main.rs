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
    let port: u16 = std::env::var("APP_PORT")
        .map_err(|_| std::io::Error::other("APP_PORT must be set"))?
        .parse()
        .map_err(|_| std::io::Error::other("APP_PORT must be 0-65535"))?;
    let listener = TcpListener::bind((ip, port)).await?;

    let snapshot_date = "2026-03-09";

    let app_state = AppState::new(
        "http://elasticsearch:9200",
        Path::new("docs/data/code-tables"),
        snapshot_date,
    )
    .map_err(std::io::Error::other)?;

    let app = app::App::new(app_state);

    app.serve(listener).await
}
