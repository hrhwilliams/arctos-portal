use arctos_portal::{app, state::AppState};
use elasticsearch::{Elasticsearch, http::transport::Transport};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    let ip = "localhost";
    let port = 2334;
    let listener = TcpListener::bind((ip, port)).await.expect("failed to bind");

    let transport =
        Transport::single_node("http://localhost:9200").expect("failed to get transport");
    let es = Elasticsearch::new(transport);

    let app_state = AppState::new(es);

    let app = app::App::new(app_state);
    app.serve(listener).await
}
