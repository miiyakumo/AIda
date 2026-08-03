#![allow(
    clippy::struct_field_names,
    dead_code,
    reason = "测试内嵌持久化模块保留 production schema 命名且只用于隔离 restart fixture"
)]

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use alda_agent::protocol::{
    ApprovalDecision, ApprovalStatus, ArtifactDurability, ClientCommand, ClientCommandId, ClientId,
    CommandEnvelope, CommandOutcome, CommandReply, CommandResult, PROTOCOL_VERSION,
    ProtocolErrorCode, QuestionStatus, SessionEventKind, SessionSnapshot, StreamCursor, StreamKind,
    TurnStatus, WsClientMessage, WsServerMessage,
};
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

#[path = "../src/app_service.rs"]
mod app_service;
#[path = "../src/artifact_store.rs"]
mod artifact_store;
#[path = "../src/control_store.rs"]
mod control_store;
#[path = "../src/domain/mod.rs"]
mod domain;
#[path = "../src/durable_runtime.rs"]
mod durable_runtime;
mod production_v2_fixture;
#[path = "../src/protocol.rs"]
mod protocol;
#[path = "../src/state/mod.rs"]
mod state;
#[path = "../src/state_store/mod.rs"]
mod state_store;

const TOKEN: &str = "c3-production-e2e-token";
const BINARY: &str = env!("CARGO_BIN_EXE_alda-agent");

struct ProductionProcess {
    child: Option<Child>,
    stderr: BufReader<ChildStderr>,
    address: SocketAddr,
    bootstrap_code: String,
}

impl ProductionProcess {
    fn start(root: &Path) -> Self {
        let address = unused_loopback_address();
        let mut child = Command::new(BINARY)
            .args([
                "serve",
                "--data-root",
                root.to_str().expect("data root UTF-8"),
                "--listen",
                &address.to_string(),
            ])
            .env("ALDA_AGENT_SESSION_TOKEN", TOKEN)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("启动 production 子进程");
        let stderr = child.stderr.take().expect("捕获 production stderr");
        let mut stderr = BufReader::new(stderr);
        let mut startup = String::new();
        let bootstrap_code = loop {
            let mut line = String::new();
            let read = stderr
                .read_line(&mut line)
                .expect("读取 production 启动输出");
            assert_ne!(read, 0, "production 未启动：{startup}");
            startup.push_str(&line);
            if let Some(code) = line
                .trim_end()
                .strip_prefix("One-time browser bootstrap code (expires in 5 minutes): ")
            {
                break code.to_owned();
            }
        };
        Self {
            child: Some(child),
            stderr,
            address,
            bootstrap_code,
        }
    }

    fn origin(&self) -> String {
        format!("http://{}", self.address)
    }

    fn signal_interrupt(&mut self) {
        let pid = self.child.as_ref().expect("production 仍在运行").id();
        let status = Command::new("kill")
            .args(["-INT", &pid.to_string()])
            .status()
            .expect("发送 SIGINT");
        assert!(status.success(), "SIGINT 发送失败");
    }

    fn wait(&mut self, expect_success: bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(2);
        let status = loop {
            let child = self.child.as_mut().expect("production 仍可等待");
            if let Some(status) = child.try_wait().expect("等待 production 子进程") {
                break status;
            }
            assert!(Instant::now() < deadline, "production 子进程未及时退出");
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(status.success(), expect_success, "production 退出状态异常");
        let mut remainder = String::new();
        self.stderr
            .read_to_string(&mut remainder)
            .expect("读取 production 退出诊断");
        self.child.take();
        remainder
    }

    fn stop(&mut self) {
        self.signal_interrupt();
        self.wait(true);
    }
}

impl Drop for ProductionProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn private_root() -> TempDir {
    let root = tempfile::tempdir().expect("创建 data root");
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))
        .expect("设置 data root 权限");
    root
}

fn unused_loopback_address() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("分配随机回环端口");
    listener.local_addr().expect("读取随机回环端口")
}

fn failed_start(arguments: &[&str], address: SocketAddr) -> Output {
    let output = Command::new(BINARY)
        .args(arguments)
        .env("ALDA_AGENT_SESSION_TOKEN", TOKEN)
        .output()
        .expect("运行预启动失败向量");
    assert!(!output.status.success(), "预启动失败向量意外成功");
    let rebound = TcpListener::bind(address).expect("失败路径不得占用 listener");
    drop(rebound);
    output
}

fn envelope(id: &str, command: ClientCommand) -> CommandEnvelope {
    CommandEnvelope {
        protocol_version: PROTOCOL_VERSION,
        client_id: ClientId("c3-e2e".to_owned()),
        client_command_id: ClientCommandId(id.to_owned()),
        command,
    }
}

async fn post_raw(client: &Client, origin: &str, value: &CommandEnvelope) -> Vec<u8> {
    let response = client
        .post(format!("{origin}/v2/commands"))
        .bearer_auth(TOKEN)
        .header(reqwest::header::ORIGIN, origin)
        .json(value)
        .send()
        .await
        .expect("提交 v2 HTTP 命令");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    response.bytes().await.expect("读取 v2 HTTP 回复").to_vec()
}

async fn post(client: &Client, origin: &str, value: &CommandEnvelope) -> CommandReply {
    serde_json::from_slice(&post_raw(client, origin, value).await).expect("解析 v2 HTTP 回复")
}

async fn bootstrap_cookie(client: &Client, process: &ProductionProcess) -> String {
    let origin = process.origin();
    let response = client
        .post(format!("{origin}/v2/bootstrap"))
        .header(reqwest::header::ORIGIN, &origin)
        .json(&serde_json::json!({"code": process.bootstrap_code}))
        .send()
        .await
        .expect("兑换 bootstrap code");
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    response.headers()[reqwest::header::SET_COOKIE]
        .to_str()
        .expect("bootstrap cookie UTF-8")
        .split(';')
        .next()
        .expect("bootstrap cookie pair")
        .to_owned()
}

async fn websocket(
    process: &ProductionProcess,
    cookie: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let origin = process.origin();
    let mut request = format!("ws://{}/v2/ws", process.address)
        .into_client_request()
        .expect("构造 v2 WebSocket 请求");
    request.headers_mut().insert(
        "origin",
        HeaderValue::from_str(&origin).expect("Origin header"),
    );
    request.headers_mut().insert(
        "cookie",
        HeaderValue::from_str(cookie).expect("Cookie header"),
    );
    request.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("alda-agent.v2"),
    );
    let (socket, response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("连接 v2 WebSocket");
    assert_eq!(
        response.headers()["sec-websocket-protocol"],
        "alda-agent.v2"
    );
    socket
}

async fn next_ws<S>(socket: &mut tokio_tungstenite::WebSocketStream<S>) -> WsServerMessage
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let frame = tokio::time::timeout(Duration::from_secs(1), socket.next())
        .await
        .expect("WebSocket 回复超时")
        .expect("WebSocket 保持连接")
        .expect("WebSocket frame 有效");
    let Message::Text(text) = frame else {
        panic!("期望 WebSocket text frame");
    };
    serde_json::from_str(&text).expect("解析 WebSocket server message")
}

fn cli(arguments: &[&str]) -> Vec<u8> {
    let output = Command::new(BINARY)
        .args(arguments)
        .env("ALDA_AGENT_SESSION_TOKEN", TOKEN)
        .output()
        .expect("运行真实 CLI");
    assert!(
        output.status.success(),
        "CLI 失败：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn reply(bytes: &[u8]) -> CommandReply {
    serde_json::from_slice(bytes).expect("解析 CLI/HTTP command reply")
}

fn assert_events(reply: &CommandReply, expected: &[u64]) {
    let CommandOutcome::Success {
        result: CommandResult::EventsResumed(page),
    } = &reply.outcome
    else {
        panic!("期望 EventsResumed");
    };
    assert_eq!(
        page.events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        page.next_after_sequence,
        expected.last().copied().unwrap_or(0)
    );
}

async fn assert_idle_socket_closes<S>(socket: &mut tokio_tungstenite::WebSocketStream<S>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let closed = tokio::time::timeout(Duration::from_secs(1), socket.next())
        .await
        .expect("终态后空闲 WebSocket 未及时结束");
    assert!(closed.is_none() || matches!(closed, Some(Ok(Message::Close(_)) | Err(_))));
}

fn assert_prebind_failures() {
    let missing_address = unused_loopback_address();
    let missing = failed_start(
        &["serve", "--listen", &missing_address.to_string()],
        missing_address,
    );
    assert!(String::from_utf8_lossy(&missing.stderr).contains("--data-root"));

    let relative_address = unused_loopback_address();
    let relative = failed_start(
        &[
            "serve",
            "--data-root",
            "relative-root",
            "--listen",
            &relative_address.to_string(),
        ],
        relative_address,
    );
    assert!(String::from_utf8_lossy(&relative.stderr).contains("preflight"));

    let weak = private_root();
    fs::set_permissions(weak.path(), fs::Permissions::from_mode(0o750))
        .expect("弱化 data root 权限");
    let weak_address = unused_loopback_address();
    let weak_output = failed_start(
        &[
            "serve",
            "--data-root",
            weak.path().to_str().expect("weak root UTF-8"),
            "--listen",
            &weak_address.to_string(),
        ],
        weak_address,
    );
    assert!(String::from_utf8_lossy(&weak_output.stderr).contains("preflight"));
}

async fn assert_v1_rejected(client: &Client, process: &ProductionProcess, cookie: &str) {
    let origin = process.origin();
    let old_path = client
        .post(format!("{origin}/v1/commands"))
        .bearer_auth(TOKEN)
        .header(reqwest::header::ORIGIN, &origin)
        .json(&envelope("old-path", ClientCommand::Initialize))
        .send()
        .await
        .expect("v1 path response");
    assert_eq!(old_path.status(), reqwest::StatusCode::NOT_FOUND);

    let old_payload = post(
        client,
        &origin,
        &CommandEnvelope {
            protocol_version: 1,
            ..envelope("old-payload", ClientCommand::Initialize)
        },
    )
    .await;
    assert!(matches!(
        old_payload.outcome,
        CommandOutcome::Error { error }
            if error.code == ProtocolErrorCode::InvalidProtocolVersion
                && error.message.contains("upgrade")
                && error.message.contains("reconnect")
    ));

    for (path, protocol, expected) in [
        ("/v1/ws", "alda-agent.v1", "404"),
        ("/v2/ws", "alda-agent.v1", "400"),
    ] {
        let mut request = format!("ws://{}{path}", process.address)
            .into_client_request()
            .expect("构造 v1 WebSocket 请求");
        request.headers_mut().insert(
            "origin",
            HeaderValue::from_str(&origin).expect("Origin header"),
        );
        request.headers_mut().insert(
            "cookie",
            HeaderValue::from_str(cookie).expect("Cookie header"),
        );
        request.headers_mut().insert(
            "sec-websocket-protocol",
            HeaderValue::from_str(protocol).expect("protocol header"),
        );
        let error = tokio_tungstenite::connect_async(request)
            .await
            .expect_err("v1 WebSocket 必须拒绝");
        assert!(error.to_string().contains(expected));
    }
}

fn session_snapshot(reply: &CommandReply) -> &SessionSnapshot {
    let CommandOutcome::Success {
        result: CommandResult::SessionSnapshot(snapshot),
    } = &reply.outcome
    else {
        panic!("期望 SessionSnapshot");
    };
    snapshot
}

fn copy_tree_contents(source: &Path, destination: &Path) {
    for entry in fs::read_dir(source).expect("枚举 fixture 根") {
        let entry = entry.expect("读取 fixture 目录项");
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = entry.metadata().expect("读取 fixture metadata");
        if metadata.is_dir() {
            fs::create_dir(&destination_path).expect("创建 fixture 子目录");
            fs::set_permissions(&destination_path, metadata.permissions())
                .expect("复制 fixture 目录权限");
            copy_tree_contents(&source_path, &destination_path);
        } else {
            assert!(metadata.is_file(), "fixture 只允许普通文件和目录");
            fs::copy(&source_path, &destination_path).expect("复制 fixture 文件");
            fs::set_permissions(&destination_path, metadata.permissions())
                .expect("复制 fixture 文件权限");
        }
    }
}

fn collect_authority_files(root: &Path, directory: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
    for entry in fs::read_dir(directory).expect("枚举权威目录") {
        let entry = entry.expect("读取权威目录项");
        let path = entry.path();
        let metadata = entry.metadata().expect("读取权威文件 metadata");
        if metadata.is_dir() {
            collect_authority_files(root, &path, files);
        } else {
            assert!(metadata.is_file(), "权威目录只允许普通文件");
            files.insert(
                path.strip_prefix(root)
                    .expect("权威文件位于 data root")
                    .to_owned(),
                fs::read(path).expect("读取权威文件"),
            );
        }
    }
}

fn authoritative_control_session_bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    for relative in ["state-v1/control", "state-v1/sessions"] {
        collect_authority_files(root, &root.join(relative), &mut files);
    }
    files
}

fn internal_restart_prepared_count(root: &Path) -> usize {
    let control = fs::read_to_string(root.join("state-v1/control/control-v1.jsonl"))
        .expect("读取 control log 统计 internal Prepared");
    control
        .lines()
        .filter(|line| line.contains("__alda_internal_restart_v1"))
        .count()
}

#[allow(
    clippy::too_many_lines,
    reason = "两个 Pending 基线必须经同一真实 production 进程连续准备并返回权威 ID"
)]
async fn seed_pending_restart_controls(
    client: &Client,
    process: &ProductionProcess,
) -> (
    alda_agent::protocol::ProjectSnapshot,
    SessionSnapshot,
    SessionSnapshot,
) {
    let origin = process.origin();
    let project_reply = post(
        client,
        &origin,
        &envelope(
            "restart-project",
            ClientCommand::ProjectCreate {
                name: "C3 Restart Matrix".to_owned(),
            },
        ),
    )
    .await;
    let CommandOutcome::Success {
        result: CommandResult::ProjectCreated(project),
    } = project_reply.outcome
    else {
        panic!("restart fixture 必须创建 Project");
    };

    let pending_question = post(
        client,
        &origin,
        &envelope(
            "restart-pending-question-session",
            ClientCommand::SessionStart {
                project_id: project.project_id.clone(),
            },
        ),
    )
    .await;
    let CommandOutcome::Success {
        result: CommandResult::SessionStarted(pending_question),
    } = pending_question.outcome
    else {
        panic!("restart fixture 必须创建 Pending Question Session");
    };
    post(
        client,
        &origin,
        &envelope(
            "restart-pending-question-turn",
            ClientCommand::TurnStart {
                session_id: pending_question.session_id.clone(),
                prompt: "保留 Pending Question".to_owned(),
            },
        ),
    )
    .await;

    let pending_approval = post(
        client,
        &origin,
        &envelope(
            "restart-pending-approval-session",
            ClientCommand::SessionStart {
                project_id: project.project_id.clone(),
            },
        ),
    )
    .await;
    let CommandOutcome::Success {
        result: CommandResult::SessionStarted(pending_approval),
    } = pending_approval.outcome
    else {
        panic!("restart fixture 必须创建 Pending Approval Session");
    };
    post(
        client,
        &origin,
        &envelope(
            "restart-pending-approval-turn",
            ClientCommand::TurnStart {
                session_id: pending_approval.session_id.clone(),
                prompt: "保留 Pending Approval".to_owned(),
            },
        ),
    )
    .await;
    let snapshot_reply = post(
        client,
        &origin,
        &envelope(
            "restart-pending-approval-before-answer",
            ClientCommand::SessionSnapshot {
                session_id: pending_approval.session_id.clone(),
            },
        ),
    )
    .await;
    let question = session_snapshot(&snapshot_reply).questions[0].clone();
    post(
        client,
        &origin,
        &envelope(
            "restart-pending-approval-answer",
            ClientCommand::QuestionRespond {
                session_id: pending_approval.session_id.clone(),
                question_id: question.question_id,
                choice_id: question.choices[0].choice_id.clone(),
            },
        ),
    )
    .await;

    (project, pending_question, pending_approval)
}

#[allow(
    clippy::too_many_lines,
    reason = "真实 restart 矩阵需连续保留准备、失败原子性、首次恢复与幂等重启证据"
)]
async fn assert_restart_obligation_matrix(client: &Client) {
    let root = private_root();
    let mut seed = ProductionProcess::start(root.path());
    let (project, pending_question, pending_approval) =
        seed_pending_restart_controls(client, &seed).await;
    seed.stop();

    let fixture = production_v2_fixture::prepare(root.path(), &project.project_id.0)
        .expect("准备 production-compatible restart fixture");
    assert_eq!(internal_restart_prepared_count(root.path()), 0);
    let before_reconciliation = authoritative_control_session_bytes(root.path());

    let failed_root = private_root();
    copy_tree_contents(root.path(), failed_root.path());
    let failed_before = authoritative_control_session_bytes(failed_root.path());
    assert_eq!(failed_before, before_reconciliation);
    fs::set_permissions(
        failed_root.path().join("state-v1/sessions"),
        fs::Permissions::from_mode(0o750),
    )
    .expect("构造 Session 目录权限预检失败");
    let failed_address = unused_loopback_address();
    let failed = failed_start(
        &[
            "serve",
            "--data-root",
            failed_root.path().to_str().expect("failed root UTF-8"),
            "--listen",
            &failed_address.to_string(),
        ],
        failed_address,
    );
    let failed_diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&failed.stdout),
        String::from_utf8_lossy(&failed.stderr)
    );
    assert!(failed_diagnostic.contains("preflight"));
    assert!(!failed_diagnostic.contains("One-time browser bootstrap code"));
    assert_eq!(
        authoritative_control_session_bytes(failed_root.path()),
        failed_before,
        "多 obligation 启动失败不得部分收敛"
    );

    let mut first = ProductionProcess::start(root.path());
    let first_origin = first.origin();
    let first_cookie = bootstrap_cookie(client, &first).await;
    let snapshot_envelopes = [
        envelope(
            "restart-query-pending-question",
            ClientCommand::SessionSnapshot {
                session_id: pending_question.session_id.clone(),
            },
        ),
        envelope(
            "restart-query-pending-approval",
            ClientCommand::SessionSnapshot {
                session_id: pending_approval.session_id.clone(),
            },
        ),
        envelope(
            "restart-query-running",
            ClientCommand::SessionSnapshot {
                session_id: fixture.running_session_id.clone(),
            },
        ),
        envelope(
            "restart-query-cancel-requested",
            ClientCommand::SessionSnapshot {
                session_id: fixture.cancel_requested_session_id.clone(),
            },
        ),
    ];
    let mut first_snapshot_bytes = Vec::new();
    for request in &snapshot_envelopes {
        first_snapshot_bytes.push(post_raw(client, &first_origin, request).await);
    }
    let snapshots = first_snapshot_bytes
        .iter()
        .map(|bytes| reply(bytes))
        .collect::<Vec<_>>();
    let question_snapshot = session_snapshot(&snapshots[0]);
    assert_eq!(
        question_snapshot.turns[0].status,
        TurnStatus::WaitingForInput
    );
    assert_eq!(
        question_snapshot.questions[0].status,
        QuestionStatus::Pending
    );
    let approval_snapshot = session_snapshot(&snapshots[1]);
    assert_eq!(
        approval_snapshot.turns[0].status,
        TurnStatus::WaitingForInput
    );
    assert_eq!(
        approval_snapshot.questions[0].status,
        QuestionStatus::Answered
    );
    assert_eq!(
        approval_snapshot.approvals[0].status,
        ApprovalStatus::Pending
    );
    let running_snapshot = session_snapshot(&snapshots[2]);
    assert_eq!(running_snapshot.covered_through_sequence, 3);
    assert_eq!(running_snapshot.turns[0].turn_id, fixture.running_turn_id);
    assert_eq!(
        running_snapshot.turns[0].status,
        TurnStatus::AbortedByRestart
    );
    assert_eq!(running_snapshot.turns[0].terminal_sequence, Some(3));
    let cancel_snapshot = session_snapshot(&snapshots[3]);
    assert_eq!(cancel_snapshot.covered_through_sequence, 6);
    assert_eq!(
        cancel_snapshot.turns[0].turn_id,
        fixture.cancel_requested_turn_id
    );
    assert_eq!(cancel_snapshot.turns[0].status, TurnStatus::Cancelled);
    assert_eq!(cancel_snapshot.turns[0].terminal_sequence, Some(6));
    assert_eq!(
        cancel_snapshot.questions[0].question_id,
        fixture.cancel_requested_question_id
    );
    assert_eq!(
        cancel_snapshot.questions[0].status,
        QuestionStatus::OwnerTurnAborted
    );
    assert_eq!(cancel_snapshot.questions[0].terminal_sequence, Some(5));

    let running_cursor = envelope(
        "restart-running-cursor",
        ClientCommand::EventResume {
            cursor: StreamCursor {
                stream_kind: StreamKind::SessionRollout,
                stream_id: fixture.running_session_id.0.clone(),
                epoch: 1,
                after_sequence: 0,
            },
        },
    );
    let cancel_cursor = envelope(
        "restart-cancel-cursor",
        ClientCommand::EventResume {
            cursor: StreamCursor {
                stream_kind: StreamKind::SessionRollout,
                stream_id: fixture.cancel_requested_session_id.0.clone(),
                epoch: 1,
                after_sequence: 0,
            },
        },
    );
    let first_running_cursor = post_raw(client, &first_origin, &running_cursor).await;
    let first_cancel_cursor = post_raw(client, &first_origin, &cancel_cursor).await;
    assert_events(&reply(&first_running_cursor), &[1, 2, 3]);
    assert_events(&reply(&first_cancel_cursor), &[1, 2, 3, 4, 5, 6]);

    let cli_snapshot = reply(&cli(&[
        "session",
        "--server",
        &first_origin,
        "--client-id",
        "c3-restart-cli",
        "snapshot",
        "--command-id",
        "restart-cli-running",
        "--session-id",
        &fixture.running_session_id.0,
    ]));
    assert_eq!(session_snapshot(&cli_snapshot), running_snapshot);

    let mut first_socket = websocket(&first, &first_cookie).await;
    first_socket
        .send(Message::Text(
            serde_json::to_string(&WsClientMessage::Subscribe {
                session_id: fixture.cancel_requested_session_id.clone(),
                epoch: 1,
                after_sequence: 0,
            })
            .expect("编码 restart WS subscribe")
            .into(),
        ))
        .await
        .expect("发送 restart WS subscribe");
    let WsServerMessage::SessionEvents {
        page: first_ws_page,
        ..
    } = next_ws(&mut first_socket).await
    else {
        panic!("restart WS 必须返回 Session events");
    };
    assert!(matches!(
        first_ws_page.events.as_slice(),
        [
            alda_agent::protocol::SessionEvent {
                event: SessionEventKind::SessionStarted { .. },
                ..
            },
            alda_agent::protocol::SessionEvent {
                event: SessionEventKind::TurnStarted { .. },
                ..
            },
            alda_agent::protocol::SessionEvent {
                event: SessionEventKind::QuestionRequested { .. },
                ..
            },
            alda_agent::protocol::SessionEvent {
                event: SessionEventKind::TurnCancelRequested { .. },
                ..
            },
            alda_agent::protocol::SessionEvent {
                event: SessionEventKind::QuestionOwnerTurnAborted { .. },
                ..
            },
            alda_agent::protocol::SessionEvent {
                event: SessionEventKind::TurnCompleted {
                    status: TurnStatus::Cancelled,
                    ..
                },
                ..
            }
        ]
    ));
    first.stop();

    assert_eq!(internal_restart_prepared_count(root.path()), 2);
    let after_first_restart = authoritative_control_session_bytes(root.path());
    assert_ne!(after_first_restart, before_reconciliation);

    let mut second = ProductionProcess::start(root.path());
    let second_origin = second.origin();
    let second_cookie = bootstrap_cookie(client, &second).await;
    for (request, expected) in snapshot_envelopes.iter().zip(&first_snapshot_bytes) {
        assert_eq!(post_raw(client, &second_origin, request).await, *expected);
    }
    assert_eq!(
        post_raw(client, &second_origin, &running_cursor).await,
        first_running_cursor
    );
    assert_eq!(
        post_raw(client, &second_origin, &cancel_cursor).await,
        first_cancel_cursor
    );
    let mut second_socket = websocket(&second, &second_cookie).await;
    second_socket
        .send(Message::Text(
            serde_json::to_string(&WsClientMessage::Subscribe {
                session_id: fixture.cancel_requested_session_id,
                epoch: 1,
                after_sequence: 0,
            })
            .expect("编码幂等 restart WS subscribe")
            .into(),
        ))
        .await
        .expect("发送幂等 restart WS subscribe");
    let WsServerMessage::SessionEvents {
        page: second_ws_page,
        ..
    } = next_ws(&mut second_socket).await
    else {
        panic!("幂等 restart WS 必须返回 Session events");
    };
    assert_eq!(second_ws_page, first_ws_page);
    second.stop();

    assert_eq!(internal_restart_prepared_count(root.path()), 2);
    assert_eq!(
        authoritative_control_session_bytes(root.path()),
        after_first_restart,
        "第二次启动不得增加 internal Prepared 或改写 Session"
    );
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::too_many_lines)]
async fn c3_production_e2e_watchdog() {
    assert_prebind_failures();
    let root = private_root();
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("构造 HTTP client");
    assert_restart_obligation_matrix(&client).await;

    let mut first = ProductionProcess::start(root.path());
    let origin = first.origin();
    let cookie = bootstrap_cookie(&client, &first).await;
    assert_v1_rejected(&client, &first, &cookie).await;

    let second_address = unused_loopback_address();
    let second = failed_start(
        &[
            "serve",
            "--data-root",
            root.path().to_str().expect("data root UTF-8"),
            "--listen",
            &second_address.to_string(),
        ],
        second_address,
    );
    assert!(String::from_utf8_lossy(&second.stderr).contains("preflight"));

    let project_stdout = cli(&[
        "project",
        "--server",
        &origin,
        "--client-id",
        "c3-cli",
        "create",
        "--command-id",
        "project-create",
        "--name",
        "C3 Etude",
    ]);
    let project_reply = reply(&project_stdout);
    let CommandOutcome::Success {
        result: CommandResult::ProjectCreated(project),
    } = project_reply.outcome
    else {
        panic!("CLI 必须创建 Project");
    };

    let session_reply = post(
        &client,
        &origin,
        &envelope(
            "session-start",
            ClientCommand::SessionStart {
                project_id: project.project_id.clone(),
            },
        ),
    )
    .await;
    let CommandOutcome::Success {
        result: CommandResult::SessionStarted(session),
    } = session_reply.outcome
    else {
        panic!("HTTP 必须创建 Session");
    };

    let turn_command = CommandEnvelope {
        client_id: ClientId("c3-ws".to_owned()),
        ..envelope(
            "turn-start",
            ClientCommand::TurnStart {
                session_id: session.session_id.clone(),
                prompt: "写一段八小节练习曲".to_owned(),
            },
        )
    };
    let mut first_socket = websocket(&first, &cookie).await;
    first_socket
        .send(Message::Text(
            serde_json::to_string(&WsClientMessage::Command(turn_command.clone()))
                .expect("编码 WS command")
                .into(),
        ))
        .await
        .expect("发送 WS command");
    let WsServerMessage::CommandReply(turn_reply) = next_ws(&mut first_socket).await else {
        panic!("WS 必须返回 Turn reply");
    };
    let exact_turn_reply = serde_json::to_vec(&turn_reply).expect("编码 exact Turn reply");
    let CommandOutcome::Success {
        result: CommandResult::TurnStarted(turn),
    } = &turn_reply.outcome
    else {
        panic!("WS 必须启动 Turn");
    };
    first_socket
        .send(Message::Text(
            serde_json::to_string(&WsClientMessage::Subscribe {
                session_id: session.session_id.clone(),
                epoch: 1,
                after_sequence: 0,
            })
            .expect("编码 WS subscribe")
            .into(),
        ))
        .await
        .expect("订阅 Session events");
    let WsServerMessage::SessionEvents { page, .. } = next_ws(&mut first_socket).await else {
        panic!("WS 必须返回 Session events");
    };
    assert_eq!(
        page.events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );

    first.signal_interrupt();
    assert_idle_socket_closes(&mut first_socket).await;
    first.wait(true);

    let mut second = ProductionProcess::start(root.path());
    let second_origin = second.origin();
    let pending_question_reply = post(
        &client,
        &second_origin,
        &envelope(
            "question-pending-snapshot",
            ClientCommand::SessionSnapshot {
                session_id: session.session_id.clone(),
            },
        ),
    )
    .await;
    let CommandOutcome::Success {
        result: CommandResult::SessionSnapshot(pending_question),
    } = &pending_question_reply.outcome
    else {
        panic!("重启后必须恢复 Session snapshot");
    };
    assert_eq!(pending_question.covered_through_sequence, 3);
    assert_eq!(
        pending_question.turns[0].status,
        TurnStatus::WaitingForInput
    );
    assert_eq!(
        pending_question.questions[0].status,
        QuestionStatus::Pending
    );
    assert_eq!(pending_question.questions[0].owner_turn_id, turn.turn_id);
    let retry_bytes = post_raw(&client, &second_origin, &turn_command).await;
    assert_eq!(
        retry_bytes, exact_turn_reply,
        "重启后 command reply 必须 byte-exact"
    );
    let cursor_reply = post(
        &client,
        &second_origin,
        &envelope(
            "cursor-after-question-restart",
            ClientCommand::EventResume {
                cursor: StreamCursor {
                    stream_kind: StreamKind::SessionRollout,
                    stream_id: session.session_id.0.clone(),
                    epoch: 1,
                    after_sequence: 0,
                },
            },
        ),
    )
    .await;
    assert_events(&cursor_reply, &[1, 2, 3]);

    let question = &pending_question.questions[0];
    let answer_command = envelope(
        "question-answer",
        ClientCommand::QuestionRespond {
            session_id: session.session_id.clone(),
            question_id: question.question_id.clone(),
            choice_id: question.choices[0].choice_id.clone(),
        },
    );
    let answer_bytes = post_raw(&client, &second_origin, &answer_command).await;
    let answer_reply = reply(&answer_bytes);
    assert!(matches!(
        answer_reply.outcome,
        CommandOutcome::Success {
            result: CommandResult::QuestionAnswered(_)
        }
    ));
    second.stop();

    let mut third = ProductionProcess::start(root.path());
    let third_origin = third.origin();
    let cookie = bootstrap_cookie(&client, &third).await;
    let approval_snapshot_reply = post(
        &client,
        &third_origin,
        &envelope(
            "approval-pending-snapshot",
            ClientCommand::SessionSnapshot {
                session_id: session.session_id.clone(),
            },
        ),
    )
    .await;
    let CommandOutcome::Success {
        result: CommandResult::SessionSnapshot(approval_snapshot),
    } = approval_snapshot_reply.outcome
    else {
        panic!("第二次重启后必须恢复 Approval");
    };
    assert_eq!(approval_snapshot.covered_through_sequence, 5);
    assert_eq!(
        approval_snapshot.questions[0].status,
        QuestionStatus::Answered
    );
    assert_eq!(
        approval_snapshot.approvals[0].status,
        ApprovalStatus::Pending
    );
    let answer_retry = post_raw(&client, &third_origin, &answer_command).await;
    assert_eq!(
        answer_retry, answer_bytes,
        "Question reply 重启后必须 byte-exact"
    );
    let second_cursor = post(
        &client,
        &third_origin,
        &envelope(
            "cursor-after-approval-restart",
            ClientCommand::EventResume {
                cursor: StreamCursor {
                    stream_kind: StreamKind::SessionRollout,
                    stream_id: session.session_id.0.clone(),
                    epoch: 1,
                    after_sequence: 3,
                },
            },
        ),
    )
    .await;
    assert_events(&second_cursor, &[4, 5]);
    let project_snapshot = post(
        &client,
        &third_origin,
        &envelope(
            "project-snapshot-after-restart",
            ClientCommand::ProjectSnapshot {
                project_id: project.project_id.clone(),
            },
        ),
    )
    .await;
    assert!(matches!(
        project_snapshot.outcome,
        CommandOutcome::Success {
            result: CommandResult::ProjectSnapshot(ref snapshot)
        } if snapshot == &project
    ));
    let approval = &approval_snapshot.approvals[0];

    let mut terminal_socket = websocket(&third, &cookie).await;
    terminal_socket
        .send(Message::Text(
            serde_json::to_string(&WsClientMessage::Subscribe {
                session_id: session.session_id.clone(),
                epoch: 1,
                after_sequence: 5,
            })
            .expect("编码 terminal subscribe")
            .into(),
        ))
        .await
        .expect("订阅 terminal events");
    let approval_stdout = cli(&[
        "approval",
        "--server",
        &third_origin,
        "--client-id",
        "c3-cli",
        "respond",
        "--command-id",
        "approval-approve",
        "--session-id",
        &session.session_id.0,
        "--approval-id",
        &approval.approval_id.0,
        "--digest-algorithm",
        &approval.approval_subject_digest.algorithm,
        "--digest-schema-version",
        &approval.approval_subject_digest.schema_version.to_string(),
        "--digest-value",
        &approval.approval_subject_digest.value,
        "--decision",
        "approve",
    ]);
    let approval_reply = reply(&approval_stdout);
    let CommandOutcome::Success {
        result:
            CommandResult::ApprovalDecided {
                approval: decided,
                artifact_manifest: Some(manifest),
            },
    } = approval_reply.outcome
    else {
        panic!("CLI Approval 必须产生 Artifact");
    };
    assert_eq!(decided.status, ApprovalStatus::Approved);
    assert_eq!(decided.decision, Some(ApprovalDecision::Approve));
    assert_eq!(manifest.durability, ArtifactDurability::DurableLocal);
    let WsServerMessage::SessionEvents { page, .. } = next_ws(&mut terminal_socket).await else {
        panic!("WS 必须返回 terminal events");
    };
    assert_eq!(
        page.events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![6, 7]
    );
    assert!(matches!(
        page.events.last().expect("terminal event").event,
        SessionEventKind::TurnCompleted {
            status: TurnStatus::Succeeded,
            ..
        }
    ));

    let manifest_command = envelope(
        "artifact-manifest",
        ClientCommand::ArtifactManifest {
            project_id: project.project_id.clone(),
            artifact_occurrence_id: manifest.artifact_occurrence_id.clone(),
        },
    );
    let manifest_bytes = post_raw(&client, &third_origin, &manifest_command).await;
    let manifest_reply = reply(&manifest_bytes);
    assert!(matches!(
        manifest_reply.outcome,
        CommandOutcome::Success {
            result: CommandResult::ArtifactManifest(ref queried)
        } if queried == &manifest
    ));
    let download = client
        .get(format!(
            "{third_origin}/v2/artifacts/{}",
            manifest.artifact_hash.hex()
        ))
        .bearer_auth(TOKEN)
        .header(reqwest::header::ORIGIN, &third_origin)
        .header("x-alda-project-id", &project.project_id.0)
        .send()
        .await
        .expect("下载 verified Artifact");
    assert_eq!(download.status(), reqwest::StatusCode::OK);
    assert_eq!(
        download
            .bytes()
            .await
            .expect("读取 Artifact bytes")
            .as_ref(),
        b"piano: o4 c8 d e f g a b > c\n"
    );
    let store_manifest =
        fs::read_to_string(root.path().join("artifacts-v1/store-manifest-v1.json"))
            .expect("读取 Artifact Store manifest");
    assert!(store_manifest.contains("linux_file_and_directory_synced"));
    third.stop();

    let mut fourth = ProductionProcess::start(root.path());
    let fourth_origin = fourth.origin();
    assert_eq!(
        post_raw(&client, &fourth_origin, &manifest_command).await,
        manifest_bytes,
        "Artifact manifest 重启后必须一致"
    );
    let reopened_download = client
        .get(format!(
            "{fourth_origin}/v2/artifacts/{}",
            manifest.artifact_hash.hex()
        ))
        .bearer_auth(TOKEN)
        .header(reqwest::header::ORIGIN, &fourth_origin)
        .header("x-alda-project-id", &project.project_id.0)
        .send()
        .await
        .expect("重启后下载 verified Artifact");
    assert_eq!(reopened_download.status(), reqwest::StatusCode::OK);
    assert_eq!(
        reopened_download
            .bytes()
            .await
            .expect("读取重启后 Artifact bytes")
            .as_ref(),
        b"piano: o4 c8 d e f g a b > c\n"
    );
    fourth.stop();

    let corrupt_root = private_root();
    let mut clean = ProductionProcess::start(corrupt_root.path());
    clean.stop();
    let control_log = corrupt_root
        .path()
        .join("state-v1/control/control-v1.jsonl");
    let mut log = fs::OpenOptions::new()
        .append(true)
        .open(control_log)
        .expect("打开 control log 构造损坏尾");
    log.write_all(b"not-json\n").expect("写入损坏 control line");
    log.sync_all().expect("同步损坏 control line");
    let corrupt_address = unused_loopback_address();
    let corrupt = failed_start(
        &[
            "serve",
            "--data-root",
            corrupt_root.path().to_str().expect("corrupt root UTF-8"),
            "--listen",
            &corrupt_address.to_string(),
        ],
        corrupt_address,
    );
    assert!(String::from_utf8_lossy(&corrupt.stderr).contains("preflight"));

    let fatal_root = private_root();
    let mut fatal = ProductionProcess::start(fatal_root.path());
    let fatal_origin = fatal.origin();
    let fatal_cookie = bootstrap_cookie(&client, &fatal).await;
    let mut fatal_socket = websocket(&fatal, &fatal_cookie).await;
    let projects = fatal_root.path().join("state-v1/projects");
    fs::set_permissions(&projects, fs::Permissions::from_mode(0o500))
        .expect("撤销新 Project 目录写权限");
    let fatal_response = client
        .post(format!("{fatal_origin}/v2/commands"))
        .bearer_auth(TOKEN)
        .header(reqwest::header::ORIGIN, &fatal_origin)
        .json(&envelope(
            "fatal-project",
            ClientCommand::ProjectCreate {
                name: "Fatal boundary".to_owned(),
            },
        ))
        .send()
        .await;
    if let Ok(response) = fatal_response {
        assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    }
    assert_idle_socket_closes(&mut fatal_socket).await;
    let fatal_diagnostic = fatal.wait(false);
    assert!(fatal_diagnostic.contains("durable actor failed"));
    fs::set_permissions(&projects, fs::Permissions::from_mode(0o700))
        .expect("恢复 Project 目录权限");
    let mut recovered = ProductionProcess::start(fatal_root.path());
    recovered.stop();

    assert_eq!(
        serde_json::to_string(&ArtifactDurability::ProcessLifetimeFixture)
            .expect("编码内存 durability"),
        "\"process_lifetime_fixture\""
    );
    assert_eq!(
        serde_json::to_string(&ArtifactDurability::DurableLocal).expect("编码 durable durability"),
        "\"durable_local\""
    );
    assert_eq!(project.name, "C3 Etude");
    assert_eq!(project.version, 1);
    assert_eq!(session.project_id, project.project_id);
    assert_eq!(session.stream_epoch, 1);
}
