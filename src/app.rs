use axum::routing::{get, post};
#[cfg(test)]
use axum::{body::Body, extract::Request};
use tokio::net::TcpListener;
#[cfg(test)]
use tower::{ServiceExt, util::Oneshot};

use crate::{routes, state::AppState};

pub struct App {
    router: axum::Router,
}

impl App {
    pub fn new(app_state: AppState) -> Self {
        let router = axum::Router::new()
            .route("/", get(routes::index))
            .route("/hx-attribute-search", post(routes::search))
            .with_state(app_state);

        Self { router }
    }

    pub async fn serve(self, listener: TcpListener) -> Result<(), std::io::Error> {
        axum::serve(listener, self.router.into_make_service()).await
    }

    #[cfg(test)]
    pub async fn oneshot(self, request: Request) -> Oneshot<axum::Router, Request<Body>> {
        self.router.oneshot(request)
    }
}
