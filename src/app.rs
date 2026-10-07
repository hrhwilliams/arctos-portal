#[cfg(test)]
use axum::{body::Body, extract::Request};
use axum::{Json, http::Method, routing::get};
use tokio::net::TcpListener;
#[cfg(test)]
use tower::{ServiceExt, util::Oneshot};
use tower_http::{
    compression::CompressionLayer,
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};
use utoipa::OpenApi;
use utoipa_axum::{router::OpenApiRouter, routes};
use utoipa_scalar::{Scalar, Servable};

use crate::{
    routes,
    search::{AttrOp, Format},
    state::AppState,
};

/// The document's own header. The paths and schemas come from the handlers
/// and the types they return, so they cannot drift from the code. The two
/// enums are named here because only query parameters reach them, and a
/// parameter's `$ref` does not register its target on its own.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Arctos portal search API",
        description = "Search and export over a snapshot of Arctos specimen records. \
                       Every search endpoint reads the same form; `/api/schema` supplies \
                       the vocabularies and limits the form needs."
    ),
    components(schemas(AttrOp, Format))
)]
struct ApiDoc;

pub struct App {
    router: axum::Router,
}

impl App {
    /// The routes, and the document that describes them, from one list.
    fn api() -> OpenApiRouter<AppState> {
        OpenApiRouter::with_openapi(ApiDoc::openapi())
            .routes(routes!(routes::search))
            .routes(routes!(routes::relations))
            .routes(routes!(routes::summary))
            .routes(routes!(routes::schema))
            .routes(routes!(routes::berkeleymapper))
            .routes(routes!(routes::taxa))
            .routes(routes!(routes::download))
            .routes(routes!(routes::es_download))
    }

    /// The document `/api/openapi.json` serves.
    #[must_use]
    pub fn openapi() -> utoipa::openapi::OpenApi {
        Self::api().split_for_parts().1
    }

    #[must_use]
    pub fn new(app_state: AppState) -> Self {
        let (router, api) = Self::api().split_for_parts();
        let spec = api.clone();
        let router = router
            .route(
                "/api/openapi.json",
                get(move || {
                    let spec = spec.clone();
                    async move { Json(spec) }
                }),
            )
            .merge(Scalar::with_url("/api/docs", api))
            .layer(CompressionLayer::new())
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
    #[tracing::instrument(skip(self, listener))]
    pub async fn serve(self, listener: TcpListener) -> Result<(), std::io::Error> {
        // tracing::info!("starting on {}:{}", listener.local_addr());
        axum::serve(listener, self.router.into_make_service()).await
    }

    #[cfg(test)]
    pub fn oneshot(self, request: Request) -> Oneshot<axum::Router, Request<Body>> {
        self.router.oneshot(request)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_document_describes_every_route_from_the_form_it_reads() {
        let api = App::openapi();
        let json = serde_json::to_value(&api).unwrap();
        let paths = json["paths"].as_object().unwrap();
        for route in [
            "/api/search",
            "/api/relations",
            "/api/summary",
            "/api/schema",
            "/api/berkeleymapper.xml",
            "/api/taxa",
            "/api/download",
            "/api/es-download",
        ] {
            assert!(paths.contains_key(route), "{route} is not documented");
        }
        // The form's list parameters are arrays, the enum is an enum, and the
        // page is not required: the document follows the type, not a hand copy.
        let params = json["paths"]["/api/search"]["get"]["parameters"]
            .as_array()
            .unwrap();
        let param = |name: &str| params.iter().find(|p| p["name"] == name).unwrap();
        assert_eq!(param("taxon")["schema"]["type"], "array");
        assert_eq!(param("page")["required"], false);
        // A parameter that refers to a component must find it there.
        assert_eq!(
            param("attr_op")["schema"]["$ref"],
            "#/components/schemas/AttrOp"
        );
        assert_eq!(
            json["components"]["schemas"]["AttrOp"]["enum"],
            serde_json::json!(["and", "or"])
        );
        assert_eq!(
            json["components"]["schemas"]["Format"]["enum"],
            serde_json::json!(["json", "csv"])
        );
        assert!(json["components"]["schemas"]["Schema"]["properties"]["limits"].is_object());
    }
}
