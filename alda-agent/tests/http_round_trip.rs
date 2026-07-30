use alda_agent::app_service::AppService;
use alda_agent::app_service::QueueCapacity;
use alda_agent::http::HttpAuth;
use alda_agent::protocol::ClientCommand;
use alda_agent::protocol::ClientCommandId;
use alda_agent::protocol::ClientId;
use alda_agent::protocol::CommandEnvelope;
use alda_agent::protocol::CommandOutcome;
use alda_agent::protocol::CommandReply;
use alda_agent::protocol::PROTOCOL_VERSION;
use alda_agent::protocol::ProtocolErrorCode;
use reqwest::Client;

async fn assert_request_guards(client: &Client, endpoint: &str, envelope: &CommandEnvelope) {
    let malformed_unauthorized = client
        .post(endpoint)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body("not-json")
        .send()
        .await
        .expect("malformed unauthorized response");
    assert_eq!(
        malformed_unauthorized.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );

    let unauthorized = client
        .post(endpoint)
        .json(envelope)
        .send()
        .await
        .expect("unauthorized response");
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

    let foreign_origin = client
        .post(endpoint)
        .bearer_auth("test-token")
        .header(reqwest::header::ORIGIN, "https://attacker.invalid")
        .json(envelope)
        .send()
        .await
        .expect("foreign origin response");
    assert_eq!(foreign_origin.status(), reqwest::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn authenticated_http_command_round_trip_and_origin_rejection() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let address = listener.local_addr().expect("test listener address");
    let origin = format!("http://{address}");
    let service = AppService::spawn(QueueCapacity::new(8).expect("valid capacity"));
    let app = alda_agent::http::router(
        service,
        HttpAuth::new("test-token", origin.clone(), address.to_string()),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("test server");
    });

    let endpoint = format!("{origin}/v1/commands");
    let envelope = CommandEnvelope {
        protocol_version: PROTOCOL_VERSION,
        client_id: ClientId("http-test".to_owned()),
        client_command_id: ClientCommandId("create-1".to_owned()),
        command: ClientCommand::ProjectCreate {
            name: "Etude".to_owned(),
        },
    };
    let client = reqwest::Client::new();

    assert_request_guards(&client, &endpoint, &envelope).await;

    let first: CommandReply = client
        .post(&endpoint)
        .bearer_auth("test-token")
        .json(&envelope)
        .send()
        .await
        .expect("create response")
        .error_for_status()
        .expect("successful create status")
        .json()
        .await
        .expect("create reply JSON");
    let repeated: CommandReply = client
        .post(&endpoint)
        .bearer_auth("test-token")
        .json(&envelope)
        .send()
        .await
        .expect("repeat response")
        .error_for_status()
        .expect("successful repeat status")
        .json()
        .await
        .expect("repeat reply JSON");
    assert_eq!(first, repeated);

    let conflicting = CommandEnvelope {
        command: ClientCommand::ProjectCreate {
            name: "Nocturne".to_owned(),
        },
        ..envelope
    };
    let conflict: CommandReply = client
        .post(&endpoint)
        .bearer_auth("test-token")
        .json(&conflicting)
        .send()
        .await
        .expect("conflict response")
        .error_for_status()
        .expect("protocol conflict is an HTTP success")
        .json()
        .await
        .expect("conflict reply JSON");
    assert!(matches!(
        conflict.outcome,
        CommandOutcome::Error {
            error: alda_agent::protocol::ProtocolError {
                code: ProtocolErrorCode::IdempotencyConflict,
                ..
            }
        }
    ));

    server.abort();
}
