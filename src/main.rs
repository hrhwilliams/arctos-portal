use arctos_portal::{app, state::AppState};
use elasticsearch::{Elasticsearch, http::transport::Transport};
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

    let transport =
        Transport::single_node("http://localhost:9200").expect("failed to get transport");
    let es = Elasticsearch::new(transport);

    let app_state = AppState::new(es).await.expect("failed to create app state");

    let app = app::App::new(app_state);
    app.serve(listener).await
}
