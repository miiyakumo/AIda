use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::Request;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::middleware;
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use serde::Serialize;

use crate::app_service::AppService;
use crate::app_service::SubmitError;
use crate::protocol::CommandEnvelope;
use crate::protocol::CommandReply;
use crate::protocol::ProtocolErrorCode;

#[derive(Clone)]
pub struct HttpAuth {
    bearer_token: Arc<str>,
    expected_origin: Arc<str>,
    expected_host: Arc<str>,
}

impl HttpAuth {
    #[must_use]
    pub fn new(
        bearer_token: impl Into<Arc<str>>,
        expected_origin: impl Into<Arc<str>>,
        expected_host: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            bearer_token: bearer_token.into(),
            expected_origin: expected_origin.into(),
            expected_host: expected_host.into(),
        }
    }

    fn authorize(&self, headers: &HeaderMap) -> Result<(), StatusCode> {
        let host = headers
            .get(axum::http::header::HOST)
            .and_then(|value| value.to_str().ok())
            .ok_or(StatusCode::BAD_REQUEST)?;
        if host != self.expected_host.as_ref() {
            return Err(StatusCode::FORBIDDEN);
        }

        if let Some(origin) = headers.get(axum::http::header::ORIGIN) {
            let origin = origin.to_str().map_err(|_| StatusCode::FORBIDDEN)?;
            if origin != self.expected_origin.as_ref() {
                return Err(StatusCode::FORBIDDEN);
            }
        }

        let authorization = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(StatusCode::UNAUTHORIZED)?;
        let expected = format!("Bearer {}", self.bearer_token);
        if authorization != expected {
            return Err(StatusCode::UNAUTHORIZED);
        }

        Ok(())
    }
}

#[derive(Clone)]
struct HttpState {
    service: AppService,
}

pub fn router(service: AppService, auth: HttpAuth) -> Router {
    Router::new()
        .route("/health", get(health))
        .route(
            "/v1/commands",
            post(command).route_layer(middleware::from_fn_with_state(auth, authorize_request)),
        )
        .with_state(HttpState { service })
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn command(
    State(state): State<HttpState>,
    Json(envelope): Json<CommandEnvelope>,
) -> Result<Json<CommandReply>, (StatusCode, Json<CommandReply>)> {
    let client_command_id = envelope.client_command_id.clone();
    state
        .service
        .execute(envelope)
        .await
        .map(Json)
        .map_err(|error| {
            let (status, code) = match error {
                SubmitError::Overloaded => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    ProtocolErrorCode::Overloaded,
                ),
                SubmitError::Closed | SubmitError::ReplyDropped => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    ProtocolErrorCode::ServiceUnavailable,
                ),
            };
            (
                status,
                Json(CommandReply::error(
                    client_command_id,
                    code,
                    error.to_string(),
                )),
            )
        })
}

async fn authorize_request(
    State(auth): State<HttpAuth>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    auth.authorize(request.headers())?;
    Ok(next.run(request).await)
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}
