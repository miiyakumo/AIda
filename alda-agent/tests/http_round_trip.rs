use alda_agent::app_service::AppService;
use alda_agent::app_service::QueueCapacity;
use alda_agent::http::HttpAuth;
use alda_agent::protocol::ApprovalDecision;
use alda_agent::protocol::ApprovalStatus;
use alda_agent::protocol::ChoiceId;
use alda_agent::protocol::ClientCommand;
use alda_agent::protocol::ClientCommandId;
use alda_agent::protocol::ClientId;
use alda_agent::protocol::CommandEnvelope;
use alda_agent::protocol::CommandOutcome;
use alda_agent::protocol::CommandReply;
use alda_agent::protocol::PROTOCOL_VERSION;
use alda_agent::protocol::ProjectId;
use alda_agent::protocol::ProtocolErrorCode;
use alda_agent::protocol::QuestionId;
use alda_agent::protocol::StreamCursor;
use alda_agent::protocol::StreamKind;
use alda_agent::protocol::TurnStatus;
use reqwest::Client;

async fn assert_request_guards(
    client: &Client,
    endpoint: &str,
    origin: &str,
    envelope: &CommandEnvelope,
) {
    let malformed_unauthorized = client
        .post(endpoint)
        .header(reqwest::header::ORIGIN, origin)
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
        .header(reqwest::header::ORIGIN, origin)
        .json(envelope)
        .send()
        .await
        .expect("unauthorized response");
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

    let missing_origin = client
        .post(endpoint)
        .bearer_auth("test-token")
        .json(envelope)
        .send()
        .await
        .expect("missing origin response");
    assert_eq!(missing_origin.status(), reqwest::StatusCode::FORBIDDEN);

    let foreign_origin = client
        .post(endpoint)
        .bearer_auth("test-token")
        .header(reqwest::header::ORIGIN, "https://attacker.invalid")
        .json(envelope)
        .send()
        .await
        .expect("foreign origin response");
    assert_eq!(foreign_origin.status(), reqwest::StatusCode::FORBIDDEN);

    let wrong_host = client
        .post(endpoint)
        .bearer_auth("test-token")
        .header(reqwest::header::ORIGIN, origin)
        .header(reqwest::header::HOST, "127.0.0.1:1")
        .json(envelope)
        .send()
        .await
        .expect("wrong host response");
    assert_eq!(wrong_host.status(), reqwest::StatusCode::FORBIDDEN);

    let wrong_token = client
        .post(endpoint)
        .bearer_auth("wrong-token")
        .header(reqwest::header::ORIGIN, origin)
        .json(envelope)
        .send()
        .await
        .expect("wrong token response");
    assert_eq!(wrong_token.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn authenticated_http_command_round_trip_and_origin_rejection() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let address = listener.local_addr().expect("test listener address");
    let origin = format!("http://{address}");
    let service = AppService::spawn(QueueCapacity::new(8).expect("valid capacity"));
    let auth = HttpAuth::new("test-token", origin.clone(), address.to_string());
    let bootstrap_code = auth.bootstrap_code_for_terminal();
    let app = alda_agent::http::router(service, auth);
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

    assert_request_guards(&client, &endpoint, &origin, &envelope).await;

    let index = client.get(&origin).send().await.expect("PWA index");
    assert_eq!(index.status(), reqwest::StatusCode::OK);
    let csp = index.headers()[reqwest::header::CONTENT_SECURITY_POLICY]
        .to_str()
        .expect("CSP");
    assert!(csp.contains(&format!("connect-src 'self' ws://{address}")));
    assert!(!csp.contains('*'));
    assert!(!csp.contains("unsafe-inline"));
    assert!(!csp.contains("unsafe-eval"));
    let client_state = client
        .get(format!("{origin}/client-state.js"))
        .send()
        .await
        .expect("client state module");
    assert_eq!(client_state.status(), reqwest::StatusCode::OK);
    assert_eq!(
        client_state.headers()[reqwest::header::CONTENT_TYPE],
        "text/javascript; charset=utf-8"
    );
    let service_worker = client
        .get(format!("{origin}/sw.js"))
        .send()
        .await
        .expect("service worker")
        .text()
        .await
        .expect("service worker text");
    assert!(service_worker.contains(
        "const ALLOWLIST = [\"/\", \"/app.js\", \"/client-state.js\", \"/app.css\", \"/manifest.webmanifest\"]"
    ));
    assert!(!service_worker.contains("\"/v1/"));

    let oversized_bootstrap = client
        .post(format!("{origin}/v1/bootstrap"))
        .header(reqwest::header::ORIGIN, &origin)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(format!(r#"{{"code":"{}"}}"#, "x".repeat(1025)))
        .send()
        .await
        .expect("oversized bootstrap");
    assert_eq!(
        oversized_bootstrap.status(),
        reqwest::StatusCode::PAYLOAD_TOO_LARGE
    );
    let oversized_command = client
        .post(&endpoint)
        .bearer_auth("test-token")
        .header(reqwest::header::ORIGIN, &origin)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body("x".repeat(64 * 1024 + 1))
        .send()
        .await
        .expect("oversized command");
    assert_eq!(
        oversized_command.status(),
        reqwest::StatusCode::PAYLOAD_TOO_LARGE
    );

    let bootstrap_without_origin = client
        .post(format!("{origin}/v1/bootstrap"))
        .json(&serde_json::json!({"code": bootstrap_code.clone()}))
        .send()
        .await
        .expect("bootstrap without origin");
    assert_eq!(
        bootstrap_without_origin.status(),
        reqwest::StatusCode::FORBIDDEN
    );
    let bootstrap_wrong_host = client
        .post(format!("{origin}/v1/bootstrap"))
        .header(reqwest::header::ORIGIN, &origin)
        .header(reqwest::header::HOST, "127.0.0.1:1")
        .json(&serde_json::json!({"code": bootstrap_code.clone()}))
        .send()
        .await
        .expect("bootstrap wrong host");
    assert_eq!(
        bootstrap_wrong_host.status(),
        reqwest::StatusCode::FORBIDDEN
    );
    let wrong_bootstrap = client
        .post(format!("{origin}/v1/bootstrap"))
        .header(reqwest::header::ORIGIN, &origin)
        .json(&serde_json::json!({"code": "wrong"}))
        .send()
        .await
        .expect("wrong bootstrap");
    assert_eq!(wrong_bootstrap.status(), reqwest::StatusCode::UNAUTHORIZED);

    let bootstrap = client
        .post(format!("{origin}/v1/bootstrap"))
        .header(reqwest::header::ORIGIN, &origin)
        .json(&serde_json::json!({"code": bootstrap_code.clone()}))
        .send()
        .await
        .expect("bootstrap response");
    assert_eq!(bootstrap.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(
        bootstrap.headers()[reqwest::header::CACHE_CONTROL],
        "no-store"
    );
    let set_cookie = bootstrap.headers()[reqwest::header::SET_COOKIE]
        .to_str()
        .expect("set-cookie")
        .to_owned();
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("SameSite=Strict"));
    assert!(set_cookie.contains("Path=/"));
    assert!(!set_cookie.contains("Secure"));
    let browser_cookie = set_cookie
        .split(';')
        .next()
        .expect("cookie pair")
        .to_owned();
    let browser_token = browser_cookie
        .split_once('=')
        .expect("cookie value")
        .1
        .to_owned();
    let replay = client
        .post(format!("{origin}/v1/bootstrap"))
        .header(reqwest::header::ORIGIN, &origin)
        .json(&serde_json::json!({"code": bootstrap_code}))
        .send()
        .await
        .expect("bootstrap replay");
    assert_eq!(replay.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(replay.headers()[reqwest::header::CACHE_CONTROL], "no-store");
    for _ in 0..3 {
        let rejected = client
            .post(format!("{origin}/v1/bootstrap"))
            .header(reqwest::header::ORIGIN, &origin)
            .json(&serde_json::json!({"code": "wrong"}))
            .send()
            .await
            .expect("bootstrap failure");
        assert_eq!(rejected.status(), reqwest::StatusCode::UNAUTHORIZED);
    }
    let limited = client
        .post(format!("{origin}/v1/bootstrap"))
        .header(reqwest::header::ORIGIN, &origin)
        .json(&serde_json::json!({"code": "wrong"}))
        .send()
        .await
        .expect("bootstrap rate limit");
    assert_eq!(limited.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);

    let browser_command = client
        .post(&endpoint)
        .header(reqwest::header::ORIGIN, &origin)
        .header(reqwest::header::COOKIE, &browser_cookie)
        .json(&CommandEnvelope {
            client_command_id: ClientCommandId("browser-initialize".to_owned()),
            command: ClientCommand::Initialize,
            ..envelope.clone()
        })
        .send()
        .await
        .expect("browser cookie command");
    assert_eq!(browser_command.status(), reqwest::StatusCode::OK);
    let cookie_with_cli_token = client
        .post(&endpoint)
        .header(reqwest::header::ORIGIN, &origin)
        .header(reqwest::header::COOKIE, "alda_agent_session=test-token")
        .json(&envelope)
        .send()
        .await
        .expect("CLI token in cookie");
    assert_eq!(
        cookie_with_cli_token.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let browser_token_as_bearer = client
        .post(&endpoint)
        .header(reqwest::header::ORIGIN, &origin)
        .bearer_auth(browser_token)
        .json(&envelope)
        .send()
        .await
        .expect("browser token as bearer");
    assert_eq!(
        browser_token_as_bearer.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );

    let first: CommandReply = client
        .post(&endpoint)
        .bearer_auth("test-token")
        .header(reqwest::header::ORIGIN, &origin)
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
        .header(reqwest::header::ORIGIN, &origin)
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

    let send = |client_command_id: &str, command: ClientCommand| CommandEnvelope {
        protocol_version: PROTOCOL_VERSION,
        client_id: ClientId("http-test".to_owned()),
        client_command_id: ClientCommandId(client_command_id.to_owned()),
        command,
    };
    let session_reply: CommandReply = client
        .post(&endpoint)
        .bearer_auth("test-token")
        .header(reqwest::header::ORIGIN, &origin)
        .json(&send(
            "session-1",
            ClientCommand::SessionStart {
                project_id: ProjectId("project-1".to_owned()),
            },
        ))
        .send()
        .await
        .expect("session response")
        .json()
        .await
        .expect("session JSON");
    let CommandOutcome::Success {
        result: alda_agent::protocol::CommandResult::SessionStarted(session),
    } = session_reply.outcome
    else {
        panic!("expected session");
    };
    let snapshot_reply: CommandReply = client
        .post(&endpoint)
        .bearer_auth("test-token")
        .header(reqwest::header::ORIGIN, &origin)
        .json(&send(
            "session-snapshot-1",
            ClientCommand::SessionSnapshot {
                session_id: session.session_id.clone(),
            },
        ))
        .send()
        .await
        .expect("snapshot response")
        .json()
        .await
        .expect("snapshot JSON");
    assert!(matches!(
        snapshot_reply.outcome,
        CommandOutcome::Success {
            result: alda_agent::protocol::CommandResult::SessionSnapshot(_)
        }
    ));
    let turn_reply: CommandReply = client
        .post(&endpoint)
        .bearer_auth("test-token")
        .header(reqwest::header::ORIGIN, &origin)
        .json(&send(
            "turn-1",
            ClientCommand::TurnStart {
                session_id: session.session_id.clone(),
                prompt: "Write an etude".to_owned(),
            },
        ))
        .send()
        .await
        .expect("turn response")
        .json()
        .await
        .expect("turn JSON");
    let CommandOutcome::Success {
        result: alda_agent::protocol::CommandResult::TurnStarted(turn),
    } = turn_reply.outcome
    else {
        panic!("expected turn");
    };
    let cancel_reply: CommandReply = client
        .post(&endpoint)
        .bearer_auth("test-token")
        .header(reqwest::header::ORIGIN, &origin)
        .json(&send(
            "cancel-1",
            ClientCommand::TurnCancel {
                session_id: session.session_id.clone(),
                turn_id: turn.turn_id,
            },
        ))
        .send()
        .await
        .expect("cancel response")
        .json()
        .await
        .expect("cancel JSON");
    assert!(matches!(
        cancel_reply.outcome,
        CommandOutcome::Success {
            result: alda_agent::protocol::CommandResult::TurnCancelled(_)
        }
    ));
    let resume_reply: CommandReply = client
        .post(&endpoint)
        .bearer_auth("test-token")
        .header(reqwest::header::ORIGIN, &origin)
        .json(&send(
            "resume-1",
            ClientCommand::EventResume {
                cursor: StreamCursor {
                    stream_kind: StreamKind::SessionRollout,
                    stream_id: session.session_id.0.clone(),
                    epoch: 1,
                    after_sequence: 0,
                },
            },
        ))
        .send()
        .await
        .expect("resume response")
        .json()
        .await
        .expect("resume JSON");
    let CommandOutcome::Success {
        result: alda_agent::protocol::CommandResult::EventsResumed(page),
    } = resume_reply.outcome
    else {
        panic!("expected events");
    };
    assert_eq!(page.events.len(), 6);

    let a2_turn: CommandReply = client
        .post(&endpoint)
        .bearer_auth("test-token")
        .header(reqwest::header::ORIGIN, &origin)
        .json(&send(
            "a2-turn",
            ClientCommand::TurnStart {
                session_id: session.session_id.clone(),
                prompt: "A chamber miniature".to_owned(),
            },
        ))
        .send()
        .await
        .expect("A2 turn response")
        .json()
        .await
        .expect("A2 turn JSON");
    assert!(matches!(
        a2_turn.outcome,
        CommandOutcome::Success {
            result: alda_agent::protocol::CommandResult::TurnStarted(_)
        }
    ));
    let question_response: CommandReply = client
        .post(&endpoint)
        .bearer_auth("test-token")
        .header(reqwest::header::ORIGIN, &origin)
        .json(&send(
            "question-response",
            ClientCommand::QuestionRespond {
                session_id: session.session_id.clone(),
                question_id: QuestionId("question-2".to_owned()),
                choice_id: ChoiceId("bars_16".to_owned()),
            },
        ))
        .send()
        .await
        .expect("question response")
        .json()
        .await
        .expect("question response JSON");
    assert!(matches!(
        question_response.outcome,
        CommandOutcome::Success {
            result: alda_agent::protocol::CommandResult::QuestionAnswered(_)
        }
    ));
    let approval_snapshot: CommandReply = client
        .post(&endpoint)
        .bearer_auth("test-token")
        .header(reqwest::header::ORIGIN, &origin)
        .json(&send(
            "a2-snapshot",
            ClientCommand::SessionSnapshot {
                session_id: session.session_id.clone(),
            },
        ))
        .send()
        .await
        .expect("A2 snapshot response")
        .json()
        .await
        .expect("A2 snapshot JSON");
    let CommandOutcome::Success {
        result: alda_agent::protocol::CommandResult::SessionSnapshot(a2_snapshot),
    } = approval_snapshot.outcome
    else {
        panic!("expected A2 snapshot");
    };
    let approval = a2_snapshot.approvals.last().expect("pending approval");
    let approval_response: CommandReply = client
        .post(&endpoint)
        .bearer_auth("test-token")
        .header(reqwest::header::ORIGIN, &origin)
        .json(&send(
            "approval-response",
            ClientCommand::ApprovalRespond {
                session_id: session.session_id.clone(),
                approval_id: approval.approval_id.clone(),
                approval_subject_digest: approval.approval_subject_digest.clone(),
                decision: ApprovalDecision::Approve,
            },
        ))
        .send()
        .await
        .expect("approval response")
        .json()
        .await
        .expect("approval response JSON");
    let CommandOutcome::Success {
        result:
            alda_agent::protocol::CommandResult::ApprovalDecided {
                approval: decided_approval,
                artifact_manifest: Some(manifest),
            },
    } = approval_response.outcome
    else {
        panic!("expected approval decision with artifact");
    };
    assert_eq!(decided_approval.status, ApprovalStatus::Approved);

    let final_snapshot_reply: CommandReply = client
        .post(&endpoint)
        .bearer_auth("test-token")
        .header(reqwest::header::ORIGIN, &origin)
        .json(&send(
            "a2-final-snapshot",
            ClientCommand::SessionSnapshot {
                session_id: session.session_id.clone(),
            },
        ))
        .send()
        .await
        .expect("final snapshot response")
        .json()
        .await
        .expect("final snapshot JSON");
    let CommandOutcome::Success {
        result: alda_agent::protocol::CommandResult::SessionSnapshot(final_snapshot),
    } = final_snapshot_reply.outcome
    else {
        panic!("expected final snapshot");
    };
    let final_question = final_snapshot.questions.last().expect("answered question");
    let final_approval = final_snapshot.approvals.last().expect("decided approval");
    assert_eq!(
        final_question.answer.as_ref().expect("answer").choice_id.0,
        "bars_16"
    );
    assert_eq!(
        final_question
            .responder_client_id
            .as_ref()
            .expect("question responder")
            .0,
        "http-test"
    );
    assert_eq!(final_approval.decision, Some(ApprovalDecision::Approve));
    assert_eq!(
        final_approval.approval_subject_digest,
        decided_approval.approval_subject_digest
    );
    assert_eq!(
        final_snapshot.turns.last().expect("successful turn").status,
        TurnStatus::Succeeded
    );
    let final_events_reply: CommandReply = client
        .post(&endpoint)
        .bearer_auth("test-token")
        .header(reqwest::header::ORIGIN, &origin)
        .json(&send(
            "a2-final-events",
            ClientCommand::EventResume {
                cursor: StreamCursor {
                    stream_kind: StreamKind::SessionRollout,
                    stream_id: session.session_id.0.clone(),
                    epoch: 1,
                    after_sequence: final_snapshot.covered_through_sequence - 2,
                },
            },
        ))
        .send()
        .await
        .expect("final events response")
        .json()
        .await
        .expect("final events JSON");
    let CommandOutcome::Success {
        result: alda_agent::protocol::CommandResult::EventsResumed(final_events),
    } = final_events_reply.outcome
    else {
        panic!("expected final events");
    };
    assert!(matches!(
        final_events.events.as_slice(),
        [
            alda_agent::protocol::SessionEvent {
                event:
                    alda_agent::protocol::SessionEventKind::ApprovalResolved {
                        approval_subject_digest,
                        decision: ApprovalDecision::Approve,
                        responder_client_id,
                        ..
                    },
                ..
            },
            alda_agent::protocol::SessionEvent {
                event:
                    alda_agent::protocol::SessionEventKind::TurnCompleted {
                        status: TurnStatus::Succeeded,
                        ..
                    },
                ..
            }
        ] if approval_subject_digest == &decided_approval.approval_subject_digest
            && responder_client_id.0 == "http-test"
    ));

    let artifact_url = format!("{origin}/v1/artifacts/{}", manifest.artifact_hash.hex());
    let download = client
        .get(&artifact_url)
        .header(reqwest::header::ORIGIN, &origin)
        .header(reqwest::header::COOKIE, &browser_cookie)
        .header("x-alda-project-id", &manifest.project_id.0)
        .send()
        .await
        .expect("artifact download");
    assert_eq!(download.status(), reqwest::StatusCode::OK);
    assert_eq!(
        download.headers()[reqwest::header::CONTENT_TYPE],
        "text/x-alda; charset=utf-8"
    );
    assert_eq!(download.headers()[reqwest::header::CONTENT_LENGTH], "29");
    assert_eq!(
        download.headers()[reqwest::header::ETAG],
        format!("\"{}\"", manifest.artifact_hash.as_str())
    );
    assert_eq!(
        download.headers()[reqwest::header::CACHE_CONTROL],
        "private, no-store"
    );
    assert_eq!(
        download.headers()[reqwest::header::VARY],
        "Origin, Authorization, X-Alda-Project-Id"
    );
    assert_eq!(
        download.headers()[reqwest::header::X_CONTENT_TYPE_OPTIONS],
        "nosniff"
    );
    assert_eq!(
        download.headers()[reqwest::header::CONTENT_DISPOSITION],
        format!(
            "attachment; filename=\"score-{}.alda\"",
            &manifest.artifact_hash.hex()[..12]
        )
    );
    assert_eq!(
        download.bytes().await.expect("download bytes").as_ref(),
        b"piano: o4 c8 d e f g a b > c\n"
    );

    let etag = format!("\"{}\"", manifest.artifact_hash.as_str());
    let not_modified = client
        .get(&artifact_url)
        .bearer_auth("test-token")
        .header(reqwest::header::ORIGIN, &origin)
        .header("x-alda-project-id", &manifest.project_id.0)
        .header(reqwest::header::IF_NONE_MATCH, etag)
        .send()
        .await
        .expect("conditional download");
    assert_eq!(not_modified.status(), reqwest::StatusCode::NOT_MODIFIED);
    assert_eq!(
        not_modified.headers()[reqwest::header::CACHE_CONTROL],
        "private, no-store"
    );
    assert!(not_modified.bytes().await.expect("304 body").is_empty());

    let second_project = client
        .post(&endpoint)
        .bearer_auth("test-token")
        .header(reqwest::header::ORIGIN, &origin)
        .json(&send(
            "unreachable-project",
            ClientCommand::ProjectCreate {
                name: "Unreachable".to_owned(),
            },
        ))
        .send()
        .await
        .expect("second project");
    assert_eq!(second_project.status(), reqwest::StatusCode::OK);

    for (label, request, expected) in [
        (
            "missing project",
            client
                .get(&artifact_url)
                .bearer_auth("test-token")
                .header(reqwest::header::ORIGIN, &origin),
            reqwest::StatusCode::BAD_REQUEST,
        ),
        (
            "wrong project",
            client
                .get(&artifact_url)
                .bearer_auth("test-token")
                .header(reqwest::header::ORIGIN, &origin)
                .header("x-alda-project-id", "project-2")
                .header(
                    reqwest::header::IF_NONE_MATCH,
                    format!("\"{}\"", manifest.artifact_hash.as_str()),
                ),
            reqwest::StatusCode::NOT_FOUND,
        ),
        (
            "missing project identity",
            client
                .get(&artifact_url)
                .bearer_auth("test-token")
                .header(reqwest::header::ORIGIN, &origin)
                .header("x-alda-project-id", "project-999")
                .header(
                    reqwest::header::IF_NONE_MATCH,
                    format!("\"{}\"", manifest.artifact_hash.as_str()),
                ),
            reqwest::StatusCode::NOT_FOUND,
        ),
        (
            "missing token",
            client
                .get(&artifact_url)
                .header(reqwest::header::ORIGIN, &origin)
                .header("x-alda-project-id", &manifest.project_id.0)
                .header(
                    reqwest::header::IF_NONE_MATCH,
                    format!("\"{}\"", manifest.artifact_hash.as_str()),
                ),
            reqwest::StatusCode::UNAUTHORIZED,
        ),
        (
            "wrong token",
            client
                .get(&artifact_url)
                .bearer_auth("wrong-token")
                .header(reqwest::header::ORIGIN, &origin)
                .header("x-alda-project-id", &manifest.project_id.0),
            reqwest::StatusCode::UNAUTHORIZED,
        ),
        (
            "missing origin",
            client
                .get(&artifact_url)
                .bearer_auth("test-token")
                .header("x-alda-project-id", &manifest.project_id.0),
            reqwest::StatusCode::FORBIDDEN,
        ),
        (
            "wrong origin",
            client
                .get(&artifact_url)
                .bearer_auth("test-token")
                .header(reqwest::header::ORIGIN, "https://attacker.invalid")
                .header("x-alda-project-id", &manifest.project_id.0),
            reqwest::StatusCode::FORBIDDEN,
        ),
        (
            "wrong host",
            client
                .get(&artifact_url)
                .bearer_auth("test-token")
                .header(reqwest::header::ORIGIN, &origin)
                .header(reqwest::header::HOST, "127.0.0.1:1")
                .header("x-alda-project-id", &manifest.project_id.0),
            reqwest::StatusCode::FORBIDDEN,
        ),
    ] {
        let response = request
            .send()
            .await
            .unwrap_or_else(|error| panic!("{label}: {error}"));
        assert_eq!(response.status(), expected, "{label}");
        assert_eq!(
            response.headers()[reqwest::header::CACHE_CONTROL],
            "private, no-store",
            "{label}"
        );
    }
    let invalid_hash = client
        .get(format!("{origin}/v1/artifacts/not-a-hash"))
        .bearer_auth("test-token")
        .header(reqwest::header::ORIGIN, &origin)
        .header("x-alda-project-id", &manifest.project_id.0)
        .send()
        .await
        .expect("invalid hash");
    assert_eq!(invalid_hash.status(), reqwest::StatusCode::BAD_REQUEST);
    let client_filename = client
        .get(format!("{artifact_url}/client-name.alda"))
        .bearer_auth("test-token")
        .header(reqwest::header::ORIGIN, &origin)
        .header("x-alda-project-id", &manifest.project_id.0)
        .send()
        .await
        .expect("client filename path");
    assert_eq!(client_filename.status(), reqwest::StatusCode::NOT_FOUND);

    let conflicting = CommandEnvelope {
        command: ClientCommand::ProjectCreate {
            name: "Nocturne".to_owned(),
        },
        ..envelope
    };
    let conflict: CommandReply = client
        .post(&endpoint)
        .bearer_auth("test-token")
        .header(reqwest::header::ORIGIN, &origin)
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
