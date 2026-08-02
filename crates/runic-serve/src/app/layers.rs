use std::time::Duration;

use axum::Router;
use axum::http::HeaderName;
use tower::ServiceBuilder;
use tower_http::cors::CorsLayer;
use tower_http::request_id::{
    MakeRequestUuid, PropagateRequestIdLayer, RequestId, SetRequestIdLayer,
};
use tower_http::trace::TraceLayer;

const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

pub(crate) fn with_layers(router: Router) -> Router {
    router.layer(CorsLayer::permissive()).layer(
        ServiceBuilder::new()
            .layer(SetRequestIdLayer::new(REQUEST_ID_HEADER, MakeRequestUuid))
            .layer(
                TraceLayer::new_for_http()
                    .make_span_with(|request: &axum::http::Request<axum::body::Body>| {
                        let request_id = request
                            .extensions()
                            .get::<RequestId>()
                            .and_then(|id| id.header_value().to_str().ok())
                            .unwrap_or("-")
                            .to_string();
                        tracing::info_span!(
                            "http_request",
                            method = %request.method(),
                            path = %request.uri().path(),
                            request_id = %request_id,
                        )
                    })
                    .on_response(
                        |response: &axum::response::Response,
                         latency: Duration,
                         _span: &tracing::Span| {
                            tracing::info!(
                                status = %response.status().as_u16(),
                                latency_ms = %latency.as_millis(),
                                "request completed"
                            );
                        },
                    ),
            )
            .layer(PropagateRequestIdLayer::new(REQUEST_ID_HEADER)),
    )
}
