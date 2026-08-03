#[cfg(test)]
use axum::{body::Body, extract::Request};
use axum::{http::Method, routing::get};
use tokio::net::TcpListener;
#[cfg(test)]
use tower::{ServiceExt, util::Oneshot};
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};

use crate::{routes, state::AppState};

pub struct App {
    router: axum::Router,
}

impl App {
    pub fn new(app_state: AppState) -> Self {
        let router = axum::Router::new()
            .route("/api/search", get(routes::search))
            .layer(TraceLayer::new_for_http())
            .layer(
                CorsLayer::new()
                    .allow_methods([Method::GET, Method::POST])
                    .allow_origin(Any),
            )
            .with_state(app_state);

        Self { router }
    }

    /// # Errors
    ///
    /// Returns an error if the server fails to accept connections on `listener`.
    pub async fn serve(self, listener: TcpListener) -> Result<(), std::io::Error> {
        axum::serve(listener, self.router.into_make_service()).await
    }

    #[cfg(test)]
    pub fn oneshot(self, request: Request) -> Oneshot<axum::Router, Request<Body>> {
        self.router.oneshot(request)
    }
}
