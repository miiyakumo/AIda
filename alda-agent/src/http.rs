use std::fmt::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::Path;
use axum::extract::Request;
use axum::extract::State;
use axum::extract::WebSocketUpgrade;
use axum::extract::ws::Message;
use axum::extract::ws::WebSocket;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::middleware;
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use futures_util::SinkExt;
use futures_util::StreamExt;
use rand::RngCore;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::time::Instant as TokioInstant;

use crate::app_service::AppService;
use crate::app_service::DownloadResolution;
use crate::app_service::SubmitError;
use crate::protocol::ArtifactHash;
use crate::protocol::CommandEnvelope;
use crate::protocol::CommandOutcome;
use crate::protocol::CommandReply;
use crate::protocol::CommandResult;
use crate::protocol::ProjectId;
use crate::protocol::ProtocolErrorCode;
use crate::protocol::RecoveryAction;
use crate::protocol::SessionId;
use crate::protocol::StreamCursor;
use crate::protocol::StreamKind;
use crate::protocol::WsClientMessage;
use crate::protocol::WsServerMessage;

const WS_SUBPROTOCOL: &str = "alda-agent.v1";
const BROWSER_COOKIE: &str = "alda_agent_session";
const MAX_WS_CONNECTIONS: usize = 16;
const MAX_WS_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_OUTBOUND_MESSAGES: usize = 16;
const MAX_POLL_IN_FLIGHT: usize = 8;

#[derive(Clone)]
pub struct HttpAuth {
    cli_bearer_token: Arc<str>,
    browser_session_token: Arc<str>,
    browser_session_expires_at: Instant,
    bootstrap: Arc<Mutex<BootstrapState>>,
    expected_origin: Arc<str>,
    expected_host: Arc<str>,
}

struct BootstrapState {
    code: String,
    expires_at: Instant,
    consumed: bool,
    failures: u8,
}

impl HttpAuth {
    #[must_use]
    pub fn new(
        bearer_token: impl Into<Arc<str>>,
        expected_origin: impl Into<Arc<str>>,
        expected_host: impl Into<Arc<str>>,
    ) -> Self {
        let bootstrap_code = random_secret();
        Self {
            cli_bearer_token: bearer_token.into(),
            browser_session_token: random_secret().into(),
            browser_session_expires_at: Instant::now() + Duration::from_secs(30 * 60),
            bootstrap: Arc::new(Mutex::new(BootstrapState {
                code: bootstrap_code,
                expires_at: Instant::now() + Duration::from_secs(5 * 60),
                consumed: false,
                failures: 0,
            })),
            expected_origin: expected_origin.into(),
            expected_host: expected_host.into(),
        }
    }

    #[must_use]
    pub fn bootstrap_code_for_terminal(&self) -> String {
        self.bootstrap
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .code
            .clone()
    }

    fn authorize_origin_host(&self, headers: &HeaderMap) -> Result<(), StatusCode> {
        let host = headers
            .get(axum::http::header::HOST)
            .and_then(|value| value.to_str().ok())
            .ok_or(StatusCode::BAD_REQUEST)?;
        if host != self.expected_host.as_ref() {
            return Err(StatusCode::FORBIDDEN);
        }

        let origin = headers
            .get(axum::http::header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            .ok_or(StatusCode::FORBIDDEN)?;
        if origin != self.expected_origin.as_ref() {
            return Err(StatusCode::FORBIDDEN);
        }

        Ok(())
    }

    fn authorize(&self, headers: &HeaderMap) -> Result<(), StatusCode> {
        self.authorize_origin_host(headers)?;
        if self.valid_cli_bearer(headers) || self.valid_browser_cookie(headers) {
            Ok(())
        } else {
            Err(StatusCode::UNAUTHORIZED)
        }
    }

    fn authorize_browser(&self, headers: &HeaderMap) -> Result<(), StatusCode> {
        self.authorize_origin_host(headers)?;
        if self.valid_browser_cookie(headers) {
            Ok(())
        } else {
            Err(StatusCode::UNAUTHORIZED)
        }
    }

    fn valid_cli_bearer(&self, headers: &HeaderMap) -> bool {
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            == Some(self.cli_bearer_token.as_ref())
    }

    fn valid_browser_cookie(&self, headers: &HeaderMap) -> bool {
        if Instant::now() >= self.browser_session_expires_at {
            return false;
        }
        headers
            .get(axum::http::header::COOKIE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|cookies| {
                cookies.split(';').any(|cookie| {
                    cookie.trim() == format!("{BROWSER_COOKIE}={}", self.browser_session_token)
                })
            })
    }
}

fn random_secret() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to String cannot fail");
            output
        })
}

#[derive(Clone)]
struct HttpState {
    service: AppService,
    auth: HttpAuth,
    bootstrap_limit: Arc<Semaphore>,
    command_limit: Arc<Semaphore>,
    artifact_limit: Arc<Semaphore>,
    ws_connections: Arc<Semaphore>,
    poll_limit: Arc<Semaphore>,
    csp: Arc<str>,
}

pub fn router(service: AppService, auth: HttpAuth) -> Router {
    let command_auth = auth.clone();
    let csp = format!(
        "default-src 'self'; script-src 'self'; style-src 'self'; \
         connect-src 'self' ws://{}; img-src 'self'; object-src 'none'; \
         base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
        auth.expected_host
    );
    let state = HttpState {
        service,
        auth,
        bootstrap_limit: Arc::new(Semaphore::new(32)),
        command_limit: Arc::new(Semaphore::new(32)),
        artifact_limit: Arc::new(Semaphore::new(32)),
        ws_connections: Arc::new(Semaphore::new(MAX_WS_CONNECTIONS)),
        poll_limit: Arc::new(Semaphore::new(MAX_POLL_IN_FLIGHT)),
        csp: csp.into(),
    };
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/client-state.js", get(client_state_js))
        .route("/app.css", get(app_css))
        .route("/manifest.webmanifest", get(web_manifest))
        .route("/sw.js", get(service_worker))
        .route("/health", get(health))
        .route(
            "/v1/bootstrap",
            post(bootstrap)
                .layer(axum::extract::DefaultBodyLimit::max(1024))
                .route_layer(middleware::from_fn(ensure_no_store)),
        )
        .route(
            "/v1/commands",
            post(command)
                .route_layer(middleware::from_fn_with_state(
                    command_auth,
                    authorize_request,
                ))
                .layer(axum::extract::DefaultBodyLimit::max(64 * 1024)),
        )
        .route("/v1/artifacts/{sha256_hex}", get(artifact_download))
        .route("/v1/ws", get(websocket))
        .with_state(state)
}

async fn ensure_no_store(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    insert_header(
        response.headers_mut(),
        axum::http::header::CACHE_CONTROL,
        "no-store",
    );
    response
}

async fn index(State(state): State<HttpState>) -> Response {
    static_response(
        "text/html; charset=utf-8",
        include_str!("../web/index.html"),
        Some(&state.csp),
        "no-cache",
    )
}

async fn app_js() -> Response {
    static_response(
        "text/javascript; charset=utf-8",
        include_str!("../web/app.js"),
        None,
        "public, max-age=3600",
    )
}

async fn client_state_js() -> Response {
    static_response(
        "text/javascript; charset=utf-8",
        include_str!("../web/client-state.js"),
        None,
        "public, max-age=3600",
    )
}

async fn app_css() -> Response {
    static_response(
        "text/css; charset=utf-8",
        include_str!("../web/app.css"),
        None,
        "public, max-age=3600",
    )
}

async fn web_manifest() -> Response {
    static_response(
        "application/manifest+json",
        include_str!("../web/manifest.webmanifest"),
        None,
        "public, max-age=3600",
    )
}

async fn service_worker() -> Response {
    static_response(
        "text/javascript; charset=utf-8",
        include_str!("../web/sw.js"),
        None,
        "no-cache",
    )
}

fn static_response(
    content_type: &str,
    body: &'static str,
    csp: Option<&str>,
    cache: &str,
) -> Response {
    let mut response = Response::new(Body::from(body));
    insert_header(
        response.headers_mut(),
        axum::http::header::CONTENT_TYPE,
        content_type,
    );
    insert_header(
        response.headers_mut(),
        axum::http::header::CACHE_CONTROL,
        cache,
    );
    insert_header(
        response.headers_mut(),
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        "nosniff",
    );
    if let Some(csp) = csp {
        insert_header(
            response.headers_mut(),
            axum::http::header::CONTENT_SECURITY_POLICY,
            csp,
        );
    }
    response
}

async fn websocket(
    State(state): State<HttpState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(status) = state.auth.authorize_browser(&headers) {
        return no_store_response(status, Body::empty());
    }
    let offered = headers
        .get(axum::http::header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|item| item.trim() == WS_SUBPROTOCOL));
    if !offered {
        return no_store_response(StatusCode::BAD_REQUEST, Body::empty());
    }
    let Ok(connection_permit) = Arc::clone(&state.ws_connections).try_acquire_owned() else {
        return no_store_response(StatusCode::SERVICE_UNAVAILABLE, Body::empty());
    };
    ws.protocols([WS_SUBPROTOCOL])
        .max_message_size(MAX_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| run_websocket(socket, state, connection_permit))
}

#[derive(Clone, Debug)]
struct Subscription {
    generation: u64,
    session_id: SessionId,
    epoch: u64,
    queued_through: u64,
    written_through: u64,
}

struct OutboundFrame {
    generation: Option<u64>,
    session_id: Option<SessionId>,
    through_sequence: Option<u64>,
    json: String,
}

struct WriterAck {
    generation: u64,
    session_id: SessionId,
    through_sequence: u64,
}

#[allow(clippy::too_many_lines)]
async fn run_websocket(
    socket: WebSocket,
    state: HttpState,
    _connection_permit: tokio::sync::OwnedSemaphorePermit,
) {
    let (mut sink, mut source) = socket.split();
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<OutboundFrame>(MAX_OUTBOUND_MESSAGES);
    let (ack_tx, mut ack_rx) = mpsc::channel::<WriterAck>(MAX_OUTBOUND_MESSAGES);
    let writer = tokio::spawn(async move {
        while let Some(frame) = outbound_rx.recv().await {
            if sink.send(Message::Text(frame.json.into())).await.is_err() {
                break;
            }
            if let (Some(generation), Some(session_id), Some(through_sequence)) =
                (frame.generation, frame.session_id, frame.through_sequence)
            {
                if ack_tx
                    .send(WriterAck {
                        generation,
                        session_id,
                        through_sequence,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    });

    let mut subscription: Option<Subscription> = None;
    let mut next_generation = 0_u64;
    let mut poll_delay = Duration::from_millis(250);
    let mut poll_deadline = TokioInstant::now();
    loop {
        tokio::select! {
            inbound = source.next() => {
                let Some(Ok(message)) = inbound else { break };
                match message {
                    Message::Text(text) => {
                        if text.len() > MAX_WS_MESSAGE_BYTES {
                            break;
                        }
                        let Ok(message) = serde_json::from_str::<WsClientMessage>(&text) else {
                            if !try_send_server(
                                &outbound_tx,
                                WsServerMessage::ProtocolError {
                                    code: ProtocolErrorCode::InvalidRequest,
                                    message: "invalid WebSocket protocol message".to_owned(),
                                    recovery: None,
                                },
                                None,
                            ) { break; }
                            continue;
                        };
                        match message {
                            WsClientMessage::Command(envelope) => {
                                let reply = match state.service.execute(envelope).await {
                                    Ok(reply) => WsServerMessage::CommandReply(reply),
                                    Err(error) => WsServerMessage::ProtocolError {
                                        code: match error {
                                            SubmitError::Overloaded => ProtocolErrorCode::Overloaded,
                                            SubmitError::Closed | SubmitError::ReplyDropped => {
                                                ProtocolErrorCode::ServiceUnavailable
                                            }
                                        },
                                        message: error.to_string(),
                                        recovery: None,
                                    },
                                };
                                if !try_send_server(&outbound_tx, reply, None) { break; }
                            }
                            WsClientMessage::Subscribe {
                                session_id,
                                epoch,
                                after_sequence,
                            } => {
                                next_generation = next_generation.saturating_add(1);
                                subscription = Some(Subscription {
                                    generation: next_generation,
                                    session_id,
                                    epoch,
                                    queued_through: after_sequence,
                                    written_through: after_sequence,
                                });
                                poll_delay = Duration::from_millis(250);
                                poll_deadline = TokioInstant::now();
                            }
                            WsClientMessage::Unsubscribe { session_id } => {
                                if subscription.as_ref().is_some_and(|current| current.session_id == session_id) {
                                    subscription = None;
                                }
                            }
                            WsClientMessage::Ping => {
                                if !try_send_server(&outbound_tx, WsServerMessage::Pong, None) { break; }
                            }
                        }
                    }
                    Message::Close(_) => break,
                    Message::Binary(bytes) if bytes.len() > MAX_WS_MESSAGE_BYTES => break,
                    Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => {}
                }
            }
            Some(ack) = ack_rx.recv() => {
                apply_writer_ack(&mut subscription, &ack);
            }
            () = tokio::time::sleep_until(poll_deadline), if subscription.is_some() => {
                let Some(current) = subscription.clone() else { continue };
                let Ok(_poll_permit) = Arc::clone(&state.poll_limit).try_acquire_owned() else {
                    poll_delay = next_poll_delay(poll_delay);
                    poll_deadline = TokioInstant::now() + poll_delay;
                    continue;
                };
                let cursor = StreamCursor {
                    stream_kind: StreamKind::SessionRollout,
                    stream_id: current.session_id.0.clone(),
                    epoch: current.epoch,
                    after_sequence: current.queued_through,
                };
                match state.service.resolve_session_events(cursor).await {
                    Ok(reply) => {
                        poll_delay = Duration::from_millis(250);
                        poll_deadline = TokioInstant::now() + poll_delay;
                        let message = match reply.outcome {
                            CommandOutcome::Success {
                                result: CommandResult::EventsResumed(page),
                            } if !page.events.is_empty() => {
                                let through = page.next_after_sequence;
                                let server = WsServerMessage::SessionEvents {
                                    subscription_generation: current.generation,
                                    page,
                                };
                                Some((server, through))
                            }
                            CommandOutcome::Success { .. } => None,
                            CommandOutcome::Error { error } => {
                                let server = WsServerMessage::ProtocolError {
                                    code: error.code,
                                    message: error.message,
                                    recovery: error.details.map(|details| details.recovery_action),
                                };
                                if !try_send_server(&outbound_tx, server, None) { break; }
                                subscription = None;
                                None
                            }
                        };
                        if let Some((server, through)) = message {
                            if serde_json::to_vec(&server)
                                .map_or(true, |encoded| encoded.len() > MAX_WS_MESSAGE_BYTES)
                            {
                                let _ = try_send_server(
                                    &outbound_tx,
                                    WsServerMessage::ProtocolError {
                                        code: ProtocolErrorCode::EventTooLarge,
                                        message: "Session event frame exceeds 64 KiB".to_owned(),
                                        recovery: None,
                                    },
                                    None,
                                );
                                break;
                            }
                            let identity = Some((current.generation, current.session_id.clone(), through));
                            if try_send_server(&outbound_tx, server, identity) {
                                if let Some(active) = subscription.as_mut().filter(|active| {
                                    active.generation == current.generation
                                        && active.session_id == current.session_id
                                }) {
                                    active.queued_through = through;
                                }
                            } else {
                                let lagged = WsServerMessage::Lagged {
                                    subscription_generation: current.generation,
                                    session_id: current.session_id.clone(),
                                    last_delivered_sequence: current.written_through,
                                    recovery: RecoveryAction::FetchSessionSnapshot(current.session_id),
                                };
                                let _ = try_send_server(&outbound_tx, lagged, None);
                                break;
                            }
                        }
                    }
                    Err(SubmitError::Overloaded) => {
                        poll_delay = next_poll_delay(poll_delay);
                        poll_deadline = TokioInstant::now() + poll_delay;
                    }
                    Err(SubmitError::Closed | SubmitError::ReplyDropped) => break,
                }
            }
        }
    }
    drop(outbound_tx);
    let _ = writer.await;
}

fn next_poll_delay(current: Duration) -> Duration {
    (current * 2).min(Duration::from_secs(2))
}

#[allow(clippy::needless_pass_by_value)]
fn try_send_server(
    outbound: &mpsc::Sender<OutboundFrame>,
    message: WsServerMessage,
    identity: Option<(u64, SessionId, u64)>,
) -> bool {
    let Ok(json) = serde_json::to_string(&message) else {
        return false;
    };
    if json.len() > MAX_WS_MESSAGE_BYTES {
        return false;
    }
    let (generation, session_id, through_sequence) = match identity {
        Some((generation, session_id, through_sequence)) => {
            (Some(generation), Some(session_id), Some(through_sequence))
        }
        None => (None, None, None),
    };
    outbound
        .try_send(OutboundFrame {
            generation,
            session_id,
            through_sequence,
            json,
        })
        .is_ok()
}

fn apply_writer_ack(subscription: &mut Option<Subscription>, ack: &WriterAck) {
    if let Some(active) = subscription
        .as_mut()
        .filter(|active| active.generation == ack.generation && active.session_id == ack.session_id)
    {
        active.written_through = active.written_through.max(ack.through_sequence);
    }
}

async fn artifact_download(
    State(state): State<HttpState>,
    Path(sha256_hex): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(status) = state.auth.authorize(&headers) {
        return artifact_response(status, Body::empty());
    }
    let Ok(_permit) = Arc::clone(&state.artifact_limit).try_acquire_owned() else {
        return artifact_response(StatusCode::SERVICE_UNAVAILABLE, Body::empty());
    };
    let Some(project_id) = headers
        .get("x-alda-project-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(|value| ProjectId(value.to_owned()))
    else {
        return artifact_response(StatusCode::BAD_REQUEST, Body::empty());
    };
    if headers.contains_key(axum::http::header::RANGE) {
        return artifact_response(StatusCode::BAD_REQUEST, Body::empty());
    }
    let Ok(hash) = ArtifactHash::parse(&format!("sha256:{sha256_hex}")) else {
        return artifact_response(StatusCode::BAD_REQUEST, Body::empty());
    };
    let if_none_match = match headers.get(axum::http::header::IF_NONE_MATCH) {
        None => None,
        Some(value) => match value.to_str() {
            Ok(value) => Some(value.to_owned()),
            Err(_) => return artifact_response(StatusCode::BAD_REQUEST, Body::empty()),
        },
    };
    let resolution = match state
        .service
        .resolve_artifact_download(project_id, hash, if_none_match)
        .await
    {
        Ok(resolution) => resolution,
        Err(SubmitError::Overloaded | SubmitError::Closed | SubmitError::ReplyDropped) => {
            return artifact_response(StatusCode::SERVICE_UNAVAILABLE, Body::empty());
        }
    };
    match resolution {
        DownloadResolution::Verified(download) => {
            let short_hash = &download.artifact_hash.hex()[..12];
            let body = Body::from(bytes::Bytes::from_owner(download.bytes));
            let mut response = artifact_response(StatusCode::OK, body);
            let response_headers = response.headers_mut();
            insert_header(
                response_headers,
                axum::http::header::CONTENT_TYPE,
                &download.mime_type,
            );
            insert_header(
                response_headers,
                axum::http::header::CONTENT_LENGTH,
                &download.size_bytes.to_string(),
            );
            insert_header(
                response_headers,
                axum::http::header::ETAG,
                &format!("\"{}\"", download.artifact_hash.as_str()),
            );
            insert_header(
                response_headers,
                axum::http::header::CONTENT_DISPOSITION,
                &format!("attachment; filename=\"score-{short_hash}.alda\""),
            );
            insert_header(
                response_headers,
                axum::http::header::X_CONTENT_TYPE_OPTIONS,
                "nosniff",
            );
            response
        }
        DownloadResolution::NotModified(hash) => {
            let mut response = artifact_response(StatusCode::NOT_MODIFIED, Body::empty());
            insert_header(
                response.headers_mut(),
                axum::http::header::ETAG,
                &format!("\"{}\"", hash.as_str()),
            );
            response
        }
        DownloadResolution::NotFound => artifact_response(StatusCode::NOT_FOUND, Body::empty()),
        DownloadResolution::Corrupt => {
            artifact_response(StatusCode::INTERNAL_SERVER_ERROR, Body::empty())
        }
    }
}

fn artifact_response(status: StatusCode, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    insert_header(
        response.headers_mut(),
        axum::http::header::CACHE_CONTROL,
        "private, no-store",
    );
    insert_header(
        response.headers_mut(),
        axum::http::header::VARY,
        "Origin, Authorization, X-Alda-Project-Id",
    );
    response
}

fn insert_header(headers: &mut HeaderMap, name: axum::http::HeaderName, value: &str) {
    headers.insert(
        name,
        value
            .parse()
            .expect("server-generated artifact header value must be valid"),
    );
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

#[derive(Deserialize)]
struct BootstrapRequest {
    code: String,
}

async fn bootstrap(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<BootstrapRequest>,
) -> Response {
    if let Err(status) = state.auth.authorize_origin_host(&headers) {
        return no_store_response(status, Body::empty());
    }
    let Ok(_permit) = Arc::clone(&state.bootstrap_limit).try_acquire_owned() else {
        return no_store_response(StatusCode::SERVICE_UNAVAILABLE, Body::empty());
    };
    let mut bootstrap = state
        .auth
        .bootstrap
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if bootstrap.failures >= 5 {
        return no_store_response(StatusCode::TOO_MANY_REQUESTS, Body::empty());
    }
    if bootstrap.consumed
        || Instant::now() >= bootstrap.expires_at
        || request.code != bootstrap.code
    {
        bootstrap.failures = bootstrap.failures.saturating_add(1);
        return no_store_response(StatusCode::UNAUTHORIZED, Body::empty());
    }
    bootstrap.consumed = true;
    bootstrap.code.clear();
    let mut response = no_store_response(StatusCode::NO_CONTENT, Body::empty());
    insert_header(
        response.headers_mut(),
        axum::http::header::SET_COOKIE,
        &format!(
            "{BROWSER_COOKIE}={}; HttpOnly; SameSite=Strict; Path=/",
            state.auth.browser_session_token
        ),
    );
    response
}

fn no_store_response(status: StatusCode, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    insert_header(
        response.headers_mut(),
        axum::http::header::CACHE_CONTROL,
        "no-store",
    );
    response
}

async fn command(
    State(state): State<HttpState>,
    Json(envelope): Json<CommandEnvelope>,
) -> Result<Json<CommandReply>, (StatusCode, Json<CommandReply>)> {
    let Ok(_permit) = Arc::clone(&state.command_limit).try_acquire_owned() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(CommandReply::error(
                envelope.client_command_id,
                ProtocolErrorCode::Overloaded,
                "HTTP command concurrency limit reached",
            )),
        ));
    };
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

async fn authorize_request(State(auth): State<HttpAuth>, request: Request, next: Next) -> Response {
    if let Err(status) = auth.authorize(request.headers()) {
        return no_store_response(status, Body::empty());
    }
    let mut response = next.run(request).await;
    insert_header(
        response.headers_mut(),
        axum::http::header::CACHE_CONTROL,
        "no-store",
    );
    response
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_service::QueryQueueCapacity;
    use crate::app_service::QueueCapacity;
    use crate::protocol::ClientCommand;
    use crate::protocol::ClientCommandId;
    use crate::protocol::ClientId;
    use crate::protocol::CommandEnvelope;
    use crate::protocol::PROTOCOL_VERSION;
    use crate::protocol::ProjectId;
    use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    #[tokio::test]
    async fn conditional_download_rejects_corruption_before_returning_304() {
        let (service, project_id, hash) = AppService::spawn_with_corrupt_download_fixture_for_test(
            QueueCapacity::new(8).expect("valid capacity"),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let address = listener.local_addr().expect("listener address");
        let origin = format!("http://{address}");
        let auth = HttpAuth::new("test-token", origin.clone(), address.to_string());
        let app = router(service, auth);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("test server");
        });

        let response = reqwest::Client::new()
            .get(format!("{origin}/v1/artifacts/{}", hash.hex()))
            .bearer_auth("test-token")
            .header(reqwest::header::ORIGIN, &origin)
            .header("x-alda-project-id", project_id.0)
            .header(
                reqwest::header::IF_NONE_MATCH,
                format!("\"{}\"", hash.as_str()),
            )
            .send()
            .await
            .expect("conditional corrupt request");

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            response.headers()[reqwest::header::CACHE_CONTROL],
            "private, no-store"
        );
        assert!(response.bytes().await.expect("error body").is_empty());
        server.abort();
    }

    #[test]
    fn late_writer_ack_never_advances_a_replaced_or_removed_subscription() {
        let first_session = SessionId("session-1".to_owned());
        let mut subscription = Some(Subscription {
            generation: 1,
            session_id: first_session.clone(),
            epoch: 1,
            queued_through: 10,
            written_through: 4,
        });
        apply_writer_ack(
            &mut subscription,
            &WriterAck {
                generation: 1,
                session_id: first_session.clone(),
                through_sequence: 6,
            },
        );
        assert_eq!(
            subscription.as_ref().expect("subscription").written_through,
            6
        );

        subscription = Some(Subscription {
            generation: 2,
            session_id: first_session.clone(),
            epoch: 1,
            queued_through: 2,
            written_through: 2,
        });
        apply_writer_ack(
            &mut subscription,
            &WriterAck {
                generation: 1,
                session_id: first_session,
                through_sequence: 10,
            },
        );
        assert_eq!(
            subscription.as_ref().expect("replacement").written_through,
            2
        );

        subscription = Some(Subscription {
            generation: 3,
            session_id: SessionId("session-2".to_owned()),
            epoch: 1,
            queued_through: 1,
            written_through: 1,
        });
        apply_writer_ack(
            &mut subscription,
            &WriterAck {
                generation: 2,
                session_id: SessionId("session-1".to_owned()),
                through_sequence: 20,
            },
        );
        assert_eq!(
            subscription
                .as_ref()
                .expect("different session")
                .written_through,
            1
        );
        subscription = None;
        apply_writer_ack(
            &mut subscription,
            &WriterAck {
                generation: 3,
                session_id: SessionId("session-2".to_owned()),
                through_sequence: 20,
            },
        );
        assert!(subscription.is_none());
    }

    #[test]
    fn queued_cursor_does_not_impersonate_written_cursor_after_writer_failure() {
        let subscription = Subscription {
            generation: 1,
            session_id: SessionId("session-1".to_owned()),
            epoch: 1,
            queued_through: 12,
            written_through: 7,
        };
        assert_eq!(subscription.written_through, 7);
        assert_eq!(subscription.queued_through, 12);
        let recovery = WsServerMessage::Lagged {
            subscription_generation: subscription.generation,
            session_id: subscription.session_id.clone(),
            last_delivered_sequence: subscription.written_through,
            recovery: RecoveryAction::FetchSessionSnapshot(subscription.session_id),
        };
        assert!(matches!(
            recovery,
            WsServerMessage::Lagged {
                last_delivered_sequence: 7,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn outbound_message_and_frame_limits_fail_closed() {
        let (sender, _receiver) = mpsc::channel(1);
        assert!(try_send_server(&sender, WsServerMessage::Pong, None));
        assert!(!try_send_server(&sender, WsServerMessage::Pong, None));

        let (sender, _receiver) = mpsc::channel(1);
        assert!(!try_send_server(
            &sender,
            WsServerMessage::ProtocolError {
                code: ProtocolErrorCode::InvalidRequest,
                message: "x".repeat(MAX_WS_MESSAGE_BYTES),
                recovery: None,
            },
            None,
        ));
    }

    #[test]
    fn poll_overload_backoff_is_bounded_and_not_a_tight_loop() {
        let mut delay = Duration::from_millis(250);
        let expected = [500, 1000, 2000, 2000];
        for expected_millis in expected {
            delay = next_poll_delay(delay);
            assert_eq!(delay, Duration::from_millis(expected_millis));
        }
    }

    #[tokio::test]
    async fn poll_and_http_concurrency_semaphores_enforce_global_limits() {
        let poll = Arc::new(Semaphore::new(MAX_POLL_IN_FLIGHT));
        let mut poll_permits = Vec::new();
        for _ in 0..MAX_POLL_IN_FLIGHT {
            poll_permits.push(
                Arc::clone(&poll)
                    .try_acquire_owned()
                    .expect("poll permit within limit"),
            );
        }
        assert!(Arc::clone(&poll).try_acquire_owned().is_err());
        drop(poll_permits);

        let http = Arc::new(Semaphore::new(32));
        let mut http_permits = Vec::new();
        for _ in 0..32 {
            http_permits.push(
                Arc::clone(&http)
                    .try_acquire_owned()
                    .expect("HTTP permit within limit"),
            );
        }
        assert!(Arc::clone(&http).try_acquire_owned().is_err());
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn real_websocket_poller_backs_off_and_resumes_from_its_original_cursor() {
        fn envelope(id: &str, command: ClientCommand) -> CommandEnvelope {
            CommandEnvelope {
                protocol_version: PROTOCOL_VERSION,
                client_id: ClientId("poll-test".to_owned()),
                client_command_id: ClientCommandId(id.to_owned()),
                command,
            }
        }

        let (service, runner) = AppService::build_with_capacities(
            QueueCapacity::new(8).expect("command capacity"),
            QueryQueueCapacity::new(1).expect("query capacity"),
        );
        let runner_task = tokio::spawn(runner.run());
        service
            .execute(envelope(
                "create",
                ClientCommand::ProjectCreate {
                    name: "Etude".to_owned(),
                },
            ))
            .await
            .expect("create project");
        service
            .execute(envelope(
                "session",
                ClientCommand::SessionStart {
                    project_id: ProjectId("project-1".to_owned()),
                },
            ))
            .await
            .expect("start session");

        service.pause_queries_for_test(true);
        service
            .execute(envelope(
                "pause-barrier",
                ClientCommand::ProjectSnapshot {
                    project_id: ProjectId("project-1".to_owned()),
                },
            ))
            .await
            .expect("command barrier observes query pause");
        let original_cursor = StreamCursor {
            stream_kind: StreamKind::SessionRollout,
            stream_id: "session-1".to_owned(),
            epoch: 1,
            after_sequence: 1,
        };
        service.fill_query_queue_with_cursor_for_test(original_cursor.clone());
        let mut attempts = service.install_query_attempt_probe_for_test();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let address = listener.local_addr().expect("listener address");
        let origin = format!("http://{address}");
        let auth = HttpAuth::new("test-token", origin.clone(), address.to_string());
        let bootstrap_code = auth.bootstrap_code_for_terminal();
        let server_service = service.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(server_service, auth))
                .await
                .expect("test server");
        });
        let client = reqwest::Client::new();
        let bootstrap = client
            .post(format!("{origin}/v1/bootstrap"))
            .header(reqwest::header::ORIGIN, &origin)
            .json(&serde_json::json!({"code": bootstrap_code}))
            .send()
            .await
            .expect("bootstrap");
        let cookie = bootstrap.headers()[axum::http::header::SET_COOKIE]
            .to_str()
            .expect("cookie")
            .split(';')
            .next()
            .expect("cookie pair")
            .to_owned();
        let mut request = format!("ws://{address}/v1/ws")
            .into_client_request()
            .expect("WS request");
        request
            .headers_mut()
            .insert("origin", HeaderValue::from_str(&origin).expect("origin"));
        request
            .headers_mut()
            .insert("cookie", HeaderValue::from_str(&cookie).expect("cookie"));
        request.headers_mut().insert(
            "sec-websocket-protocol",
            HeaderValue::from_static(WS_SUBPROTOCOL),
        );
        let (mut socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("authenticated WS");
        socket
            .send(TungsteniteMessage::Text(
                serde_json::to_string(&WsClientMessage::Subscribe {
                    session_id: SessionId("session-1".to_owned()),
                    epoch: 1,
                    after_sequence: 1,
                })
                .expect("subscribe JSON")
                .into(),
            ))
            .await
            .expect("subscribe");

        let first = tokio::time::timeout(Duration::from_millis(200), attempts.recv())
            .await
            .expect("first production poll attempt")
            .expect("probe open");
        assert_eq!(first.0, original_cursor);

        let command_started = TokioInstant::now();
        service
            .execute(envelope(
                "turn",
                ClientCommand::TurnStart {
                    session_id: SessionId("session-1".to_owned()),
                    prompt: "A tiny etude".to_owned(),
                },
            ))
            .await
            .expect("command progresses while query queue is full");
        assert!(command_started.elapsed() < Duration::from_millis(200));

        let second = tokio::time::timeout(Duration::from_millis(700), attempts.recv())
            .await
            .expect("second production poll attempt")
            .expect("probe open");
        let third = tokio::time::timeout(Duration::from_millis(1200), attempts.recv())
            .await
            .expect("third production poll attempt")
            .expect("probe open");
        assert_eq!(second.0, original_cursor);
        assert_eq!(third.0, original_cursor);
        assert!(second.1.duration_since(first.1) >= Duration::from_millis(450));
        assert!(third.1.duration_since(second.1) >= Duration::from_millis(900));

        service.pause_queries_for_test(false);
        service
            .execute(envelope(
                "release-barrier",
                ClientCommand::ProjectSnapshot {
                    project_id: ProjectId("project-1".to_owned()),
                },
            ))
            .await
            .expect("wake runner after releasing query pause");
        let frame = tokio::time::timeout(Duration::from_millis(2500), socket.next())
            .await
            .expect("events resume after query release")
            .expect("socket open")
            .expect("valid frame");
        let TungsteniteMessage::Text(text) = frame else {
            panic!("expected text frame");
        };
        let WsServerMessage::SessionEvents { page, .. } =
            serde_json::from_str(&text).expect("typed server message")
        else {
            panic!("expected SessionEvents");
        };
        assert_eq!(
            page.events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );

        server.abort();
        runner_task.abort();
    }

    #[test]
    fn pwa_sources_use_safe_dom_and_exact_cache_allowlist() {
        let app = include_str!("../web/app.js");
        let client_state = include_str!("../web/client-state.js");
        let worker = include_str!("../web/sw.js");
        let html = include_str!("../web/index.html");
        assert!(!app.contains("innerHTML"));
        assert!(!app.contains("localStorage"));
        assert!(!client_state.contains("localStorage"));
        assert!(!html.contains("unsafe-inline"));
        assert!(app.contains("textContent"));
        assert!(worker.contains(
            "const ALLOWLIST = [\"/\", \"/app.js\", \"/client-state.js\", \"/app.css\", \"/manifest.webmanifest\"]"
        ));
        assert!(worker.contains("url.search === \"\""));
        assert!(!worker.contains("\"/v1/"));
        assert!(!worker.contains("startsWith"));
    }

    #[tokio::test]
    async fn expired_bootstrap_code_is_rejected_without_cache() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let address = listener.local_addr().expect("listener address");
        let origin = format!("http://{address}");
        let auth = HttpAuth::new("cli-token", origin.clone(), address.to_string());
        let code = auth.bootstrap_code_for_terminal();
        auth.bootstrap
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .expires_at = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("test Instant supports one-second subtraction");
        let app = router(
            AppService::spawn(QueueCapacity::new(8).expect("capacity")),
            auth,
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server");
        });
        let response = reqwest::Client::new()
            .post(format!("{origin}/v1/bootstrap"))
            .header(reqwest::header::ORIGIN, origin)
            .json(&serde_json::json!({"code": code}))
            .send()
            .await
            .expect("expired bootstrap request");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers()[reqwest::header::CACHE_CONTROL],
            "no-store"
        );
        server.abort();
    }

    #[test]
    fn expired_browser_cookie_and_cross_domain_credentials_are_rejected() {
        let mut auth = HttpAuth::new("cli-token", "http://127.0.0.1:37891", "127.0.0.1:37891");
        let browser_token = auth.browser_session_token.clone();
        auth.browser_session_expires_at = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("test Instant supports one-second subtraction");
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            "127.0.0.1:37891".parse().expect("host"),
        );
        headers.insert(
            axum::http::header::ORIGIN,
            "http://127.0.0.1:37891".parse().expect("origin"),
        );
        headers.insert(
            axum::http::header::COOKIE,
            format!("{BROWSER_COOKIE}={browser_token}")
                .parse()
                .expect("cookie"),
        );
        assert_eq!(
            auth.authorize_browser(&headers),
            Err(StatusCode::UNAUTHORIZED)
        );
        headers.insert(
            axum::http::header::COOKIE,
            format!("{BROWSER_COOKIE}=cli-token")
                .parse()
                .expect("cookie"),
        );
        assert_eq!(auth.authorize(&headers), Err(StatusCode::UNAUTHORIZED));
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn full_command_and_artifact_query_queues_map_to_immediate_503() {
        let (service, _runner) = AppService::build_with_capacities(
            QueueCapacity::new(1).expect("command capacity"),
            QueryQueueCapacity::new(1).expect("query capacity"),
        );
        service
            .enqueue(CommandEnvelope {
                protocol_version: PROTOCOL_VERSION,
                client_id: ClientId("held".to_owned()),
                client_command_id: ClientCommandId("held".to_owned()),
                command: ClientCommand::Initialize,
            })
            .expect("fill command queue");
        service.fill_query_queue_for_test();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let address = listener.local_addr().expect("listener address");
        let origin = format!("http://{address}");
        let auth = HttpAuth::new("test-token", origin.clone(), address.to_string());
        let bootstrap_code = auth.bootstrap_code_for_terminal();
        let app = router(service, auth);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server");
        });
        let client = reqwest::Client::new();
        let bootstrap = client
            .post(format!("{origin}/v1/bootstrap"))
            .header(reqwest::header::ORIGIN, &origin)
            .json(&serde_json::json!({"code": bootstrap_code}))
            .send()
            .await
            .expect("bootstrap");
        let cookie = bootstrap.headers()[reqwest::header::SET_COOKIE]
            .to_str()
            .expect("cookie")
            .split(';')
            .next()
            .expect("cookie pair");
        let mut request =
            tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
                format!("ws://{address}/v1/ws"),
            )
            .expect("WS request");
        request.headers_mut().insert(
            axum::http::header::ORIGIN,
            origin.parse().expect("origin header"),
        );
        request.headers_mut().insert(
            axum::http::header::COOKIE,
            cookie.parse().expect("cookie header"),
        );
        request.headers_mut().insert(
            axum::http::header::SEC_WEBSOCKET_PROTOCOL,
            WS_SUBPROTOCOL.parse().expect("protocol header"),
        );
        let (mut socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("WS connection");
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::to_string(&WsClientMessage::Command(CommandEnvelope {
                    protocol_version: PROTOCOL_VERSION,
                    client_id: ClientId("ws-overload".to_owned()),
                    client_command_id: ClientCommandId("ws-overload".to_owned()),
                    command: ClientCommand::Initialize,
                }))
                .expect("WS command JSON")
                .into(),
            ))
            .await
            .expect("send WS command");
        let ws_reply = socket.next().await.expect("WS reply").expect("WS text");
        let tokio_tungstenite::tungstenite::Message::Text(ws_reply) = ws_reply else {
            panic!("expected WS text");
        };
        assert!(matches!(
            serde_json::from_str::<WsServerMessage>(&ws_reply).expect("typed WS reply"),
            WsServerMessage::ProtocolError {
                code: ProtocolErrorCode::Overloaded,
                ..
            }
        ));
        let command = client
            .post(format!("{origin}/v1/commands"))
            .bearer_auth("test-token")
            .header(reqwest::header::ORIGIN, &origin)
            .json(&CommandEnvelope {
                protocol_version: PROTOCOL_VERSION,
                client_id: ClientId("overload".to_owned()),
                client_command_id: ClientCommandId("overload".to_owned()),
                command: ClientCommand::Initialize,
            })
            .send()
            .await
            .expect("overloaded command");
        assert_eq!(command.status(), StatusCode::SERVICE_UNAVAILABLE);
        let reply: CommandReply = command.json().await.expect("typed overload");
        assert!(matches!(
            reply.outcome,
            CommandOutcome::Error {
                error: crate::protocol::ProtocolError {
                    code: ProtocolErrorCode::Overloaded,
                    ..
                }
            }
        ));

        let artifact = client
            .get(format!("{origin}/v1/artifacts/{}", "0".repeat(64)))
            .bearer_auth("test-token")
            .header(reqwest::header::ORIGIN, &origin)
            .header("x-alda-project-id", "project-1")
            .send()
            .await
            .expect("overloaded artifact query");
        assert_eq!(artifact.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            artifact.headers()[reqwest::header::CACHE_CONTROL],
            "private, no-store"
        );
        server.abort();
    }
}
