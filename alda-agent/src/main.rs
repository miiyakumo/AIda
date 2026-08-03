use std::env;
use std::future::IntoFuture;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::path::PathBuf;

use alda_agent::app_service::QueryQueueCapacity;
use alda_agent::app_service::QueueCapacity;
use alda_agent::http::HttpAuth;
use alda_agent::http::ProductionHttpHost;
use alda_agent::production_runtime::ActorLifecycle;
use alda_agent::protocol::ApprovalDecision;
use alda_agent::protocol::ApprovalId;
use alda_agent::protocol::ApprovalSubjectDigest;
use alda_agent::protocol::ArtifactOccurrenceId;
use alda_agent::protocol::ChoiceId;
use alda_agent::protocol::ClientCommand;
use alda_agent::protocol::ClientCommandId;
use alda_agent::protocol::ClientId;
use alda_agent::protocol::CommandEnvelope;
use alda_agent::protocol::CommandOutcome;
use alda_agent::protocol::CommandReply;
use alda_agent::protocol::PROTOCOL_VERSION;
use alda_agent::protocol::ProjectId;
use alda_agent::protocol::QuestionId;
use alda_agent::protocol::SessionId;
use alda_agent::protocol::StreamCursor;
use alda_agent::protocol::StreamKind;
use alda_agent::protocol::TurnId;
use anyhow::Context;
use anyhow::bail;
use clap::Parser;
use clap::Subcommand;

const SESSION_TOKEN_ENV: &str = "ALDA_AGENT_SESSION_TOKEN";
const DEFAULT_SERVER: &str = "http://127.0.0.1:37891";
const PRODUCTION_STATUS: &str =
    "durable v2 Local Service is ready; clients using v1 must upgrade and reconnect";

#[derive(Debug, Parser)]
#[command(
    name = "alda-agent",
    about = "Alda Music Agent local service and thin CLI"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 在回环地址运行 durable v2 Local Service。
    Serve {
        #[arg(long)]
        data_root: PathBuf,
        #[arg(long, default_value = "127.0.0.1:37891")]
        listen: SocketAddr,
        #[arg(long, default_value_t = 64)]
        queue_capacity: usize,
        #[arg(long, default_value_t = 32)]
        query_queue_capacity: usize,
    },
    /// 通过 durable v2 Local Service 协议操作 Project。
    Project {
        #[arg(long, default_value = DEFAULT_SERVER)]
        server: String,
        #[arg(long, default_value = "cli")]
        client_id: String,
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// 通过 durable v2 Local Service 协议读取 Revision 投影。
    Revision {
        #[arg(long, default_value = DEFAULT_SERVER)]
        server: String,
        #[arg(long, default_value = "cli")]
        client_id: String,
        #[command(subcommand)]
        command: RevisionCommand,
    },
    /// 通过 durable v2 Local Service 协议操作 Session。
    Session {
        #[arg(long, default_value = DEFAULT_SERVER)]
        server: String,
        #[arg(long, default_value = "cli")]
        client_id: String,
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// 通过 durable v2 Local Service 协议操作 Turn。
    Turn {
        #[arg(long, default_value = DEFAULT_SERVER)]
        server: String,
        #[arg(long, default_value = "cli")]
        client_id: String,
        #[command(subcommand)]
        command: TurnCommand,
    },
    /// 通过 durable v2 Local Service 协议恢复结构化事件流。
    Event {
        #[arg(long, default_value = DEFAULT_SERVER)]
        server: String,
        #[arg(long, default_value = "cli")]
        client_id: String,
        #[command(subcommand)]
        command: EventCommand,
    },
    /// 通过 durable v2 Local Service 协议回答结构化 Question。
    Question {
        #[arg(long, default_value = DEFAULT_SERVER)]
        server: String,
        #[arg(long, default_value = "cli")]
        client_id: String,
        #[command(subcommand)]
        command: QuestionCommand,
    },
    /// 通过 durable v2 Local Service 协议响应 effect Approval。
    Approval {
        #[arg(long, default_value = DEFAULT_SERVER)]
        server: String,
        #[arg(long, default_value = "cli")]
        client_id: String,
        #[command(subcommand)]
        command: ApprovalCommand,
    },
    /// 通过 durable v2 Local Service 协议查询 Artifact 元数据，不写入本地文件。
    Artifact {
        #[arg(long, default_value = DEFAULT_SERVER)]
        server: String,
        #[arg(long, default_value = "cli")]
        client_id: String,
        #[command(subcommand)]
        command: ArtifactCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    /// 通过 durable v2 Local Service 创建 Project。
    Create {
        #[arg(long)]
        command_id: String,
        #[arg(long)]
        name: String,
    },
    /// 读取当前持久化 Project snapshot。
    Snapshot {
        #[arg(long)]
        command_id: String,
        #[arg(long)]
        project_id: String,
    },
    /// 读取带版本的 Project domain projection。
    DomainSnapshot {
        #[arg(long)]
        command_id: String,
        #[arg(long)]
        project_id: String,
    },
}

#[derive(Debug, Subcommand)]
enum RevisionCommand {
    /// 列出 Project 的不可变 Revision。
    List {
        #[arg(long)]
        command_id: String,
        #[arg(long)]
        project_id: String,
    },
    /// 读取一项不可变 Revision。
    Read {
        #[arg(long)]
        command_id: String,
        #[arg(long)]
        project_id: String,
        #[arg(long)]
        revision_id: String,
    },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// 为已有 Project 启动持久化 Session。
    Start {
        #[arg(long)]
        command_id: String,
        #[arg(long)]
        project_id: String,
    },
    /// 读取 Session snapshot 及其覆盖的 stream sequence。
    Snapshot {
        #[arg(long)]
        command_id: String,
        #[arg(long)]
        session_id: String,
    },
}

#[derive(Debug, Subcommand)]
enum TurnCommand {
    /// 启动 Fake Turn，并保持运行直到显式取消。
    Start {
        #[arg(long)]
        command_id: String,
        #[arg(long)]
        session_id: String,
        #[arg(long)]
        prompt: String,
    },
    /// 请求取消，并以 cancelled 状态完成 Fake Turn。
    Cancel {
        #[arg(long)]
        command_id: String,
        #[arg(long)]
        session_id: String,
        #[arg(long)]
        turn_id: String,
    },
}

#[derive(Debug, Subcommand)]
enum EventCommand {
    /// 从指定 sequence number 后恢复 Session Rollout。
    Resume {
        #[arg(long)]
        command_id: String,
        #[arg(long)]
        session_id: String,
        #[arg(long, default_value_t = 1)]
        epoch: u64,
        #[arg(long, default_value_t = 0)]
        after_sequence: u64,
    },
}

#[derive(Debug, Subcommand)]
enum QuestionCommand {
    /// 从 Question 公布的 choice ID 中选择一项。
    Respond {
        #[arg(long)]
        command_id: String,
        #[arg(long)]
        session_id: String,
        #[arg(long)]
        question_id: String,
        #[arg(long)]
        choice_id: String,
    },
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum CliApprovalDecision {
    Approve,
    Deny,
}

#[derive(Debug, Subcommand)]
enum ApprovalCommand {
    /// 回显完整 subject digest 后决定 Approval。
    Respond {
        #[arg(long)]
        command_id: String,
        #[arg(long)]
        session_id: String,
        #[arg(long)]
        approval_id: String,
        #[arg(long)]
        digest_algorithm: String,
        #[arg(long)]
        digest_schema_version: u32,
        #[arg(long)]
        digest_value: String,
        #[arg(long, value_enum)]
        decision: CliApprovalDecision,
    },
}

#[derive(Debug, Subcommand)]
enum ArtifactCommand {
    /// 通过命令协议读取 occurrence manifest。
    Manifest {
        #[arg(long)]
        command_id: String,
        #[arg(long)]
        project_id: String,
        #[arg(long)]
        artifact_occurrence_id: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve {
            data_root,
            listen,
            queue_capacity,
            query_queue_capacity,
        } => serve(data_root, listen, queue_capacity, query_queue_capacity).await,
        Command::Project {
            server,
            client_id,
            command,
        } => {
            let (id, command) = project_command(command);
            submit(&server, client_id, id, command).await
        }
        Command::Revision {
            server,
            client_id,
            command,
        } => {
            let (id, command) = revision_command(command);
            submit(&server, client_id, id, command).await
        }
        Command::Session {
            server,
            client_id,
            command,
        } => {
            let (id, command) = session_command(command);
            submit(&server, client_id, id, command).await
        }
        Command::Turn {
            server,
            client_id,
            command,
        } => {
            let (id, command) = turn_command(command);
            submit(&server, client_id, id, command).await
        }
        Command::Event {
            server,
            client_id,
            command,
        } => {
            let (id, command) = event_command(command);
            submit(&server, client_id, id, command).await
        }
        Command::Question {
            server,
            client_id,
            command,
        } => {
            let (id, command) = question_command(command);
            submit(&server, client_id, id, command).await
        }
        Command::Approval {
            server,
            client_id,
            command,
        } => {
            let (id, command) = approval_command(command);
            submit(&server, client_id, id, command).await
        }
        Command::Artifact {
            server,
            client_id,
            command,
        } => {
            let (id, command) = artifact_command(command);
            submit(&server, client_id, id, command).await
        }
    }
}

async fn serve(
    data_root: PathBuf,
    listen: SocketAddr,
    queue_capacity: usize,
    query_queue_capacity: usize,
) -> anyhow::Result<()> {
    if !listen.ip().is_loopback() {
        bail!("Local Service refuses non-loopback listen address `{listen}`");
    }
    if listen.port() == 0 {
        bail!("Local Service refuses port 0 outside the Rust test harness");
    }
    let capacity = QueueCapacity::new(queue_capacity)?;
    let query_capacity = QueryQueueCapacity::new(query_queue_capacity)?;
    let token = session_token()?;
    let mut production = ProductionHttpHost::open(&data_root, capacity, query_capacity)?;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("failed to bind Local Service at {listen}"))?;
    let local_addr = listener
        .local_addr()
        .context("failed to read listen address")?;
    let origin = format!("http://{local_addr}");
    let auth = HttpAuth::new(token, origin, local_addr.to_string());
    let bootstrap_code = auth.bootstrap_code_for_terminal();
    let app = production.router(auth);
    let mut lifecycle = production.subscribe_lifecycle();

    eprintln!("Alda Agent durable Local Service listening at http://{local_addr}");
    eprintln!("One-time browser bootstrap code (expires in 5 minutes): {bootstrap_code}");
    eprintln!("{PRODUCTION_STATUS}");

    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            wait_for_actor_shutdown(&mut lifecycle).await;
        })
        .into_future();
    tokio::pin!(server);
    let serve_result = tokio::select! {
        result = &mut server => result,
        () = shutdown_signal() => {
            production.shutdown();
            server.await
        }
    };
    production.shutdown();
    let join_result = production.shutdown_and_join();
    serve_result.context("Local Service failed")?;
    join_result.context("durable actor failed while serving")
}

async fn wait_for_actor_shutdown(lifecycle: &mut tokio::sync::watch::Receiver<ActorLifecycle>) {
    loop {
        if *lifecycle.borrow() != ActorLifecycle::Running {
            return;
        }
        if lifecycle.changed().await.is_err() {
            return;
        }
    }
}

fn project_command(command: ProjectCommand) -> (ClientCommandId, ClientCommand) {
    match command {
        ProjectCommand::Create { command_id, name } => (
            ClientCommandId(command_id),
            ClientCommand::ProjectCreate { name },
        ),
        ProjectCommand::Snapshot {
            command_id,
            project_id,
        } => (
            ClientCommandId(command_id),
            ClientCommand::ProjectSnapshot {
                project_id: ProjectId(project_id),
            },
        ),
        ProjectCommand::DomainSnapshot {
            command_id,
            project_id,
        } => (
            ClientCommandId(command_id),
            ClientCommand::ProjectDomainSnapshot {
                project_id: ProjectId(project_id),
            },
        ),
    }
}

fn revision_command(command: RevisionCommand) -> (ClientCommandId, ClientCommand) {
    match command {
        RevisionCommand::List {
            command_id,
            project_id,
        } => (
            ClientCommandId(command_id),
            ClientCommand::RevisionList {
                project_id: ProjectId(project_id),
            },
        ),
        RevisionCommand::Read {
            command_id,
            project_id,
            revision_id,
        } => (
            ClientCommandId(command_id),
            ClientCommand::RevisionRead {
                project_id: ProjectId(project_id),
                revision_id: alda_agent::protocol::ScoreRevisionId(revision_id),
            },
        ),
    }
}

fn session_command(command: SessionCommand) -> (ClientCommandId, ClientCommand) {
    match command {
        SessionCommand::Start {
            command_id,
            project_id,
        } => (
            ClientCommandId(command_id),
            ClientCommand::SessionStart {
                project_id: ProjectId(project_id),
            },
        ),
        SessionCommand::Snapshot {
            command_id,
            session_id,
        } => (
            ClientCommandId(command_id),
            ClientCommand::SessionSnapshot {
                session_id: SessionId(session_id),
            },
        ),
    }
}

fn turn_command(command: TurnCommand) -> (ClientCommandId, ClientCommand) {
    match command {
        TurnCommand::Start {
            command_id,
            session_id,
            prompt,
        } => (
            ClientCommandId(command_id),
            ClientCommand::TurnStart {
                session_id: SessionId(session_id),
                prompt,
            },
        ),
        TurnCommand::Cancel {
            command_id,
            session_id,
            turn_id,
        } => (
            ClientCommandId(command_id),
            ClientCommand::TurnCancel {
                session_id: SessionId(session_id),
                turn_id: TurnId(turn_id),
            },
        ),
    }
}

fn event_command(command: EventCommand) -> (ClientCommandId, ClientCommand) {
    match command {
        EventCommand::Resume {
            command_id,
            session_id,
            epoch,
            after_sequence,
        } => (
            ClientCommandId(command_id),
            ClientCommand::EventResume {
                cursor: StreamCursor {
                    stream_kind: StreamKind::SessionRollout,
                    stream_id: session_id,
                    epoch,
                    after_sequence,
                },
            },
        ),
    }
}

fn question_command(command: QuestionCommand) -> (ClientCommandId, ClientCommand) {
    match command {
        QuestionCommand::Respond {
            command_id,
            session_id,
            question_id,
            choice_id,
        } => (
            ClientCommandId(command_id),
            ClientCommand::QuestionRespond {
                session_id: SessionId(session_id),
                question_id: QuestionId(question_id),
                choice_id: ChoiceId(choice_id),
            },
        ),
    }
}

fn approval_command(command: ApprovalCommand) -> (ClientCommandId, ClientCommand) {
    match command {
        ApprovalCommand::Respond {
            command_id,
            session_id,
            approval_id,
            digest_algorithm,
            digest_schema_version,
            digest_value,
            decision,
        } => (
            ClientCommandId(command_id),
            ClientCommand::ApprovalRespond {
                session_id: SessionId(session_id),
                approval_id: ApprovalId(approval_id),
                approval_subject_digest: ApprovalSubjectDigest {
                    algorithm: digest_algorithm,
                    schema_version: digest_schema_version,
                    value: digest_value,
                },
                decision: match decision {
                    CliApprovalDecision::Approve => ApprovalDecision::Approve,
                    CliApprovalDecision::Deny => ApprovalDecision::Deny,
                },
            },
        ),
    }
}

fn artifact_command(command: ArtifactCommand) -> (ClientCommandId, ClientCommand) {
    match command {
        ArtifactCommand::Manifest {
            command_id,
            project_id,
            artifact_occurrence_id,
        } => (
            ClientCommandId(command_id),
            ClientCommand::ArtifactManifest {
                project_id: ProjectId(project_id),
                artifact_occurrence_id: ArtifactOccurrenceId(artifact_occurrence_id),
            },
        ),
    }
}

async fn submit(
    server: &str,
    client_id: String,
    client_command_id: ClientCommandId,
    command: ClientCommand,
) -> anyhow::Result<()> {
    let token = session_token()?;
    let envelope = CommandEnvelope {
        protocol_version: PROTOCOL_VERSION,
        client_id: ClientId(client_id),
        client_command_id,
        command,
    };
    let endpoint = command_endpoint(server)?;
    let origin = endpoint.origin().ascii_serialization();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("failed to build Local Service HTTP client")?;
    let response = client
        .post(endpoint)
        .bearer_auth(token)
        .header(reqwest::header::ORIGIN, origin)
        .json(&envelope)
        .send()
        .await
        .context("failed to contact Local Service")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read Local Service reply")?;
    if !status.is_success() {
        bail!("Local Service returned HTTP {status}: {body}");
    }

    println!("{}", successful_reply_json(&body)?);
    Ok(())
}

fn command_endpoint(server: &str) -> anyhow::Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(server).context("--server must be an absolute URL")?;
    if url.scheme() != "http" {
        bail!("--server must use http on an explicit loopback IP address");
    }
    let host = url
        .host_str()
        .context("--server must include a loopback IP address")?;
    let ip: IpAddr = host
        .parse()
        .context("--server host must be an explicit loopback IP address")?;
    if !ip.is_loopback() {
        bail!("--server refuses non-loopback address `{host}`");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("--server must not contain user information");
    }
    url.set_path("/v2/commands");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn successful_reply_json(body: &str) -> anyhow::Result<String> {
    let reply: CommandReply =
        serde_json::from_str(body).context("Local Service returned an invalid protocol reply")?;
    if let CommandOutcome::Error { error } = &reply.outcome {
        bail!(
            "Local Service protocol error {:?}: {}",
            error.code,
            error.message
        );
    }
    serde_json::to_string(&reply).context("failed to encode Local Service reply")
}

fn session_token() -> anyhow::Result<String> {
    let token = env::var(SESSION_TOKEN_ENV).with_context(|| {
        format!("{SESSION_TOKEN_ENV} must contain the development session token")
    })?;
    if token.trim().is_empty() {
        bail!("{SESSION_TOKEN_ENV} must not be empty");
    }
    Ok(token)
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("failed to install Ctrl+C handler: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alda_agent::protocol::CommandReply;
    use alda_agent::protocol::ProtocolErrorCode;

    #[test]
    fn protocol_errors_are_cli_errors() {
        let reply = CommandReply::error(
            ClientCommandId("conflict-1".to_owned()),
            ProtocolErrorCode::IdempotencyConflict,
            "command ID conflict",
        );
        let body = serde_json::to_string(&reply).expect("serialize test reply");

        let error = successful_reply_json(&body).expect_err("protocol error must fail the CLI");

        assert!(error.to_string().contains("IdempotencyConflict"));
    }

    #[test]
    fn b1_cli_queries_map_to_independent_wire_commands() {
        assert_eq!(
            revision_command(RevisionCommand::List {
                command_id: "list-1".to_owned(),
                project_id: "project-1".to_owned(),
            }),
            (
                ClientCommandId("list-1".to_owned()),
                ClientCommand::RevisionList {
                    project_id: ProjectId("project-1".to_owned()),
                },
            )
        );
        assert_eq!(
            revision_command(RevisionCommand::Read {
                command_id: "read-1".to_owned(),
                project_id: "project-1".to_owned(),
                revision_id: "revision-1".to_owned(),
            }),
            (
                ClientCommandId("read-1".to_owned()),
                ClientCommand::RevisionRead {
                    project_id: ProjectId("project-1".to_owned()),
                    revision_id: alda_agent::protocol::ScoreRevisionId("revision-1".to_owned(),),
                },
            )
        );
    }

    #[test]
    fn command_endpoint_rejects_remote_and_ambiguous_hosts() {
        assert!(command_endpoint("https://127.0.0.1:37891").is_err());
        assert!(command_endpoint("http://example.com:37891").is_err());
        assert!(command_endpoint("http://localhost:37891").is_err());
        assert!(command_endpoint("http://user@127.0.0.1:37891").is_err());
        assert_eq!(
            command_endpoint("http://127.0.0.1:37891/base?secret=value")
                .expect("explicit loopback URL")
                .as_str(),
            "http://127.0.0.1:37891/v2/commands"
        );
    }

    #[test]
    fn validated_endpoint_derives_the_exact_origin() {
        let endpoint = command_endpoint("http://127.0.0.1:37891/base?secret=value#fragment")
            .expect("explicit loopback URL");

        assert_eq!(endpoint.as_str(), "http://127.0.0.1:37891/v2/commands");
        assert_eq!(
            endpoint.origin().ascii_serialization(),
            "http://127.0.0.1:37891"
        );
    }

    #[test]
    fn c3_production_surface_status_declares_durable_v2() {
        assert!(PRODUCTION_STATUS.contains("durable v2"));
        assert!(PRODUCTION_STATUS.contains("upgrade and reconnect"));
        assert!(!PRODUCTION_STATUS.contains("process-local"));
    }

    #[tokio::test]
    async fn c3_production_surface_serve_rejects_port_zero_before_opening() {
        let error = serve(
            PathBuf::from("/unused-for-port-zero"),
            "127.0.0.1:0".parse().expect("socket address"),
            64,
            32,
        )
        .await
        .expect_err("port zero must be rejected");
        assert!(error.to_string().contains("port 0"));
    }

    #[test]
    fn c3_production_surface_data_root_is_required_by_cli() {
        let error =
            Cli::try_parse_from(["alda-agent", "serve"]).expect_err("serve 必须显式要求 data root");
        assert!(error.to_string().contains("--data-root"));
    }
}
