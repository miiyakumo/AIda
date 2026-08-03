use alda_agent::app_service::AppService;
use alda_agent::app_service::QueryQueueCapacity;
use alda_agent::app_service::QueueCapacity;
use alda_agent::http::HttpAuth;
use alda_agent::http::ProductionHttpHost;
use alda_agent::protocol::ApprovalDecision;
use alda_agent::protocol::ChoiceId;
use alda_agent::protocol::ClientCommand;
use alda_agent::protocol::ClientCommandId;
use alda_agent::protocol::ClientId;
use alda_agent::protocol::CommandEnvelope;
use alda_agent::protocol::CommandOutcome;
use alda_agent::protocol::CommandReply;
use alda_agent::protocol::CommandResult;
use alda_agent::protocol::PROTOCOL_VERSION;
use alda_agent::protocol::ProjectId;
use alda_agent::protocol::QuestionId;
use alda_agent::protocol::SessionEventKind;
use alda_agent::protocol::SessionId;
use alda_agent::protocol::TurnStatus;
use alda_agent::protocol::WsClientMessage;
use alda_agent::protocol::WsServerMessage;
use futures_util::SinkExt;
use futures_util::StreamExt;
use reqwest::Client;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

fn envelope(id: &str, command: ClientCommand) -> CommandEnvelope {
    CommandEnvelope {
        protocol_version: PROTOCOL_VERSION,
        client_id: ClientId("ws-external".to_owned()),
        client_command_id: ClientCommandId(id.to_owned()),
        command,
    }
}

async fn post(
    client: &Client,
    endpoint: &str,
    origin: &str,
    envelope: &CommandEnvelope,
) -> CommandReply {
    client
        .post(endpoint)
        .bearer_auth("test-token")
        .header(reqwest::header::ORIGIN, origin)
        .json(envelope)
        .send()
        .await
        .expect("HTTP command")
        .json()
        .await
        .expect("command reply")
}

async fn next_server_message<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
) -> WsServerMessage
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let message = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
        .await
        .expect("WS message timeout")
        .expect("WS remains open")
        .expect("valid WS frame");
    let Message::Text(text) = message else {
        panic!("expected text frame");
    };
    serde_json::from_str(&text).expect("typed server message")
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn websocket_observes_external_events_and_resumes_without_gaps() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let address = listener.local_addr().expect("listener address");
    let origin = format!("http://{address}");
    let ws_url = format!("ws://{address}/v2/ws");
    let service = AppService::spawn(QueueCapacity::new(64).expect("valid capacity"));
    let auth = HttpAuth::new("test-token", origin.clone(), address.to_string());
    let bootstrap_code = auth.bootstrap_code_for_terminal();
    let app = alda_agent::http::router(service, auth);
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("test server");
    });
    let client = Client::new();
    let endpoint = format!("{origin}/v2/commands");
    let bootstrap = client
        .post(format!("{origin}/v2/bootstrap"))
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
        .expect("cookie pair")
        .to_owned();

    post(
        &client,
        &endpoint,
        &origin,
        &envelope(
            "project",
            ClientCommand::ProjectCreate {
                name: "Etude".to_owned(),
            },
        ),
    )
    .await;
    post(
        &client,
        &endpoint,
        &origin,
        &envelope(
            "session",
            ClientCommand::SessionStart {
                project_id: ProjectId("project-1".to_owned()),
            },
        ),
    )
    .await;

    let mut request = ws_url.clone().into_client_request().expect("WS request");
    request
        .headers_mut()
        .insert("origin", HeaderValue::from_str(&origin).expect("origin"));
    request
        .headers_mut()
        .insert("cookie", HeaderValue::from_str(&cookie).expect("cookie"));
    request.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("alda-agent.v2"),
    );
    let (mut socket, response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("authenticated WS");
    assert_eq!(
        response.headers()["sec-websocket-protocol"],
        "alda-agent.v2"
    );
    socket
        .send(Message::Text(
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
    post(
        &client,
        &endpoint,
        &origin,
        &envelope(
            "turn",
            ClientCommand::TurnStart {
                session_id: SessionId("session-1".to_owned()),
                prompt: "A tiny etude".to_owned(),
            },
        ),
    )
    .await;
    let first = next_server_message(&mut socket).await;
    let WsServerMessage::SessionEvents { page, .. } = first else {
        panic!("expected initial events");
    };
    assert_eq!(
        page.events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    socket.close(None).await.expect("disconnect");

    post(
        &client,
        &endpoint,
        &origin,
        &envelope(
            "answer",
            ClientCommand::QuestionRespond {
                session_id: SessionId("session-1".to_owned()),
                question_id: QuestionId("question-1".to_owned()),
                choice_id: ChoiceId("bars_8".to_owned()),
            },
        ),
    )
    .await;

    let mut request = ws_url.into_client_request().expect("resume request");
    request
        .headers_mut()
        .insert("origin", HeaderValue::from_str(&origin).expect("origin"));
    request
        .headers_mut()
        .insert("cookie", HeaderValue::from_str(&cookie).expect("cookie"));
    request.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("alda-agent.v2"),
    );
    let (mut resumed, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("resume WS");
    resumed
        .send(Message::Text(
            serde_json::to_string(&WsClientMessage::Subscribe {
                session_id: SessionId("session-1".to_owned()),
                epoch: 1,
                after_sequence: 3,
            })
            .expect("resume JSON")
            .into(),
        ))
        .await
        .expect("resume subscribe");
    let resumed_events = next_server_message(&mut resumed).await;
    let WsServerMessage::SessionEvents { page, .. } = resumed_events else {
        panic!("expected resumed events");
    };
    assert_eq!(
        page.events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![4, 5]
    );

    let snapshot = post(
        &client,
        &endpoint,
        &origin,
        &envelope(
            "snapshot",
            ClientCommand::SessionSnapshot {
                session_id: SessionId("session-1".to_owned()),
            },
        ),
    )
    .await;
    let CommandOutcome::Success {
        result: CommandResult::SessionSnapshot(snapshot),
    } = snapshot.outcome
    else {
        panic!("snapshot");
    };
    let approval = snapshot.approvals.last().expect("approval");
    post(
        &client,
        &endpoint,
        &origin,
        &envelope(
            "approve",
            ClientCommand::ApprovalRespond {
                session_id: SessionId("session-1".to_owned()),
                approval_id: approval.approval_id.clone(),
                approval_subject_digest: approval.approval_subject_digest.clone(),
                decision: ApprovalDecision::Approve,
            },
        ),
    )
    .await;
    let terminal = next_server_message(&mut resumed).await;
    let WsServerMessage::SessionEvents { page, .. } = terminal else {
        panic!("terminal events");
    };
    assert_eq!(
        page.events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![6, 7]
    );
    assert!(matches!(
        page.events.last().expect("terminal").event,
        SessionEventKind::TurnCompleted {
            status: TurnStatus::Succeeded,
            ..
        }
    ));
    resumed
        .send(Message::Text(
            serde_json::to_string(&WsClientMessage::Subscribe {
                session_id: SessionId("session-1".to_owned()),
                epoch: 1,
                after_sequence: 999,
            })
            .expect("future cursor JSON")
            .into(),
        ))
        .await
        .expect("future cursor subscribe");
    assert!(matches!(
        next_server_message(&mut resumed).await,
        WsServerMessage::ProtocolError {
            code: alda_agent::protocol::ProtocolErrorCode::InvalidCursor,
            recovery:
                Some(alda_agent::protocol::RecoveryAction::FetchSessionSnapshot(
                    SessionId(ref session_id),
                )),
            ..
        } if session_id == "session-1"
    ));
    resumed
        .send(Message::Text(
            serde_json::to_string(&WsClientMessage::Subscribe {
                session_id: SessionId("session-1".to_owned()),
                epoch: 2,
                after_sequence: 0,
            })
            .expect("epoch cursor JSON")
            .into(),
        ))
        .await
        .expect("epoch cursor subscribe");
    assert!(matches!(
        next_server_message(&mut resumed).await,
        WsServerMessage::ProtocolError {
            code: alda_agent::protocol::ProtocolErrorCode::CursorEpochMismatch,
            recovery:
                Some(alda_agent::protocol::RecoveryAction::FetchSessionSnapshot(
                    SessionId(ref session_id),
                )),
            ..
        } if session_id == "session-1"
    ));
    server.abort();
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn websocket_handshake_auth_protocol_and_connection_limit_are_bounded() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let address = listener.local_addr().expect("listener address");
    let origin = format!("http://{address}");
    let ws_url = format!("ws://{address}/v2/ws");
    let auth = HttpAuth::new("test-token", origin.clone(), address.to_string());
    let code = auth.bootstrap_code_for_terminal();
    let app = alda_agent::http::router(
        AppService::spawn(QueueCapacity::new(8).expect("capacity")),
        auth,
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("server");
    });
    let bootstrap = Client::new()
        .post(format!("{origin}/v2/bootstrap"))
        .header(reqwest::header::ORIGIN, &origin)
        .json(&serde_json::json!({"code": code}))
        .send()
        .await
        .expect("bootstrap");
    let cookie = bootstrap.headers()[reqwest::header::SET_COOKIE]
        .to_str()
        .expect("cookie")
        .split(';')
        .next()
        .expect("cookie pair")
        .to_owned();

    let mut missing_cookie = ws_url.clone().into_client_request().expect("request");
    missing_cookie
        .headers_mut()
        .insert("origin", HeaderValue::from_str(&origin).expect("origin"));
    missing_cookie.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("alda-agent.v2"),
    );
    missing_cookie.headers_mut().insert(
        "authorization",
        HeaderValue::from_static("Bearer test-token"),
    );
    let error = tokio_tungstenite::connect_async(missing_cookie)
        .await
        .expect_err("cookie is required");
    assert!(error.to_string().contains("401"));

    let mut wrong_protocol = ws_url.clone().into_client_request().expect("request");
    wrong_protocol
        .headers_mut()
        .insert("origin", HeaderValue::from_str(&origin).expect("origin"));
    wrong_protocol
        .headers_mut()
        .insert("cookie", HeaderValue::from_str(&cookie).expect("cookie"));
    wrong_protocol.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("wrong.v1"),
    );
    let error = tokio_tungstenite::connect_async(wrong_protocol)
        .await
        .expect_err("subprotocol is required");
    assert!(error.to_string().contains("400"));

    let mut wrong_host = ws_url.clone().into_client_request().expect("request");
    wrong_host
        .headers_mut()
        .insert("origin", HeaderValue::from_str(&origin).expect("origin"));
    wrong_host
        .headers_mut()
        .insert("host", HeaderValue::from_static("127.0.0.1:1"));
    wrong_host
        .headers_mut()
        .insert("cookie", HeaderValue::from_str(&cookie).expect("cookie"));
    wrong_host.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("alda-agent.v2"),
    );
    let error = tokio_tungstenite::connect_async(wrong_host)
        .await
        .expect_err("exact Host is required");
    assert!(error.to_string().contains("403"));

    let mut sockets = Vec::new();
    for _ in 0..16 {
        let mut request = ws_url.clone().into_client_request().expect("request");
        request
            .headers_mut()
            .insert("origin", HeaderValue::from_str(&origin).expect("origin"));
        request
            .headers_mut()
            .insert("cookie", HeaderValue::from_str(&cookie).expect("cookie"));
        request.headers_mut().insert(
            "sec-websocket-protocol",
            HeaderValue::from_static("alda-agent.v2"),
        );
        sockets.push(
            tokio_tungstenite::connect_async(request)
                .await
                .expect("connection within limit")
                .0,
        );
    }
    let mut seventeenth = ws_url.into_client_request().expect("request");
    seventeenth
        .headers_mut()
        .insert("origin", HeaderValue::from_str(&origin).expect("origin"));
    seventeenth
        .headers_mut()
        .insert("cookie", HeaderValue::from_str(&cookie).expect("cookie"));
    seventeenth.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("alda-agent.v2"),
    );
    let error = tokio_tungstenite::connect_async(seventeenth)
        .await
        .expect_err("17th connection rejected");
    assert!(error.to_string().contains("503"));
    drop(sockets);
    server.abort();
}

#[tokio::test]
async fn c3_production_surface_v2_websocket_closes_on_actor_stopping() {
    let root = tempfile::tempdir().expect("创建 production data root");
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))
        .expect("设置 production data root 权限");
    let mut host = ProductionHttpHost::open(
        root.path(),
        QueueCapacity::new(8).expect("command 容量"),
        QueryQueueCapacity::new(8).expect("query 容量"),
    )
    .expect("打开 production actor");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let address = listener.local_addr().expect("listener 地址");
    let origin = format!("http://{address}");
    let auth = HttpAuth::new("test-token", origin.clone(), address.to_string());
    let bootstrap_code = auth.bootstrap_code_for_terminal();
    let app = host.router(auth);
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("production WS server");
    });
    let bootstrap = Client::new()
        .post(format!("{origin}/v2/bootstrap"))
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
        .expect("cookie pair")
        .to_owned();

    let mut old_path = format!("ws://{address}/v1/ws")
        .into_client_request()
        .expect("v1 path request");
    old_path
        .headers_mut()
        .insert("origin", HeaderValue::from_str(&origin).expect("origin"));
    old_path
        .headers_mut()
        .insert("cookie", HeaderValue::from_str(&cookie).expect("cookie"));
    old_path.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("alda-agent.v1"),
    );
    let old_path_error = tokio_tungstenite::connect_async(old_path)
        .await
        .expect_err("v1 path 不 upgrade");
    assert!(old_path_error.to_string().contains("404"));

    let ws_url = format!("ws://{address}/v2/ws");
    let mut old_protocol = ws_url
        .clone()
        .into_client_request()
        .expect("v1 protocol request");
    old_protocol
        .headers_mut()
        .insert("origin", HeaderValue::from_str(&origin).expect("origin"));
    old_protocol
        .headers_mut()
        .insert("cookie", HeaderValue::from_str(&cookie).expect("cookie"));
    old_protocol.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("alda-agent.v1"),
    );
    let old_protocol_error = tokio_tungstenite::connect_async(old_protocol)
        .await
        .expect_err("v1 subprotocol 不 upgrade");
    assert!(old_protocol_error.to_string().contains("400"));

    let mut request = ws_url.into_client_request().expect("v2 request");
    request
        .headers_mut()
        .insert("origin", HeaderValue::from_str(&origin).expect("origin"));
    request
        .headers_mut()
        .insert("cookie", HeaderValue::from_str(&cookie).expect("cookie"));
    request.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("alda-agent.v2"),
    );
    let (mut socket, response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("v2 websocket");
    assert_eq!(
        response.headers()["sec-websocket-protocol"],
        "alda-agent.v2"
    );

    host.shutdown();
    let closed = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
        .await
        .expect("Stopping 后 WebSocket 及时结束");
    assert!(closed.is_none() || matches!(closed, Some(Ok(Message::Close(_)) | Err(_))));
    host.shutdown_and_join().expect("join actor");
    server.abort();
}
