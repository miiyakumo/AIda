use std::env;
use std::net::IpAddr;
use std::net::SocketAddr;

use alda_agent::app_service::AppService;
use alda_agent::app_service::QueryQueueCapacity;
use alda_agent::app_service::QueueCapacity;
use alda_agent::http::HttpAuth;
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
const DEVELOPMENT_STATUS: &str = "PWA bootstrap and WebSocket streaming are available; state is process-local and in memory, and persistence is not implemented";

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
    /// Run the development Local Service on a loopback address.
    Serve {
        #[arg(long, default_value = "127.0.0.1:37891")]
        listen: SocketAddr,
        #[arg(long, default_value_t = 64)]
        queue_capacity: usize,
        #[arg(long, default_value_t = 32)]
        query_queue_capacity: usize,
    },
    /// Operate on projects through the Local Service protocol.
    Project {
        #[arg(long, default_value = DEFAULT_SERVER)]
        server: String,
        #[arg(long, default_value = "cli")]
        client_id: String,
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Read the in-memory B1 Revision projection through stable wire DTOs.
    Revision {
        #[arg(long, default_value = DEFAULT_SERVER)]
        server: String,
        #[arg(long, default_value = "cli")]
        client_id: String,
        #[command(subcommand)]
        command: RevisionCommand,
    },
    /// Operate on sessions through the Local Service protocol.
    Session {
        #[arg(long, default_value = DEFAULT_SERVER)]
        server: String,
        #[arg(long, default_value = "cli")]
        client_id: String,
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Operate on turns through the Local Service protocol.
    Turn {
        #[arg(long, default_value = DEFAULT_SERVER)]
        server: String,
        #[arg(long, default_value = "cli")]
        client_id: String,
        #[command(subcommand)]
        command: TurnCommand,
    },
    /// Resume structured events through the Local Service protocol.
    Event {
        #[arg(long, default_value = DEFAULT_SERVER)]
        server: String,
        #[arg(long, default_value = "cli")]
        client_id: String,
        #[command(subcommand)]
        command: EventCommand,
    },
    /// Respond to structured questions through the Local Service protocol.
    Question {
        #[arg(long, default_value = DEFAULT_SERVER)]
        server: String,
        #[arg(long, default_value = "cli")]
        client_id: String,
        #[command(subcommand)]
        command: QuestionCommand,
    },
    /// Respond to effect approvals through the Local Service protocol.
    Approval {
        #[arg(long, default_value = DEFAULT_SERVER)]
        server: String,
        #[arg(long, default_value = "cli")]
        client_id: String,
        #[command(subcommand)]
        command: ApprovalCommand,
    },
    /// Query Artifact metadata without writing local files.
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
    /// Create a project through the Local Service.
    Create {
        #[arg(long)]
        command_id: String,
        #[arg(long)]
        name: String,
    },
    /// Read the current in-memory project snapshot.
    Snapshot {
        #[arg(long)]
        command_id: String,
        #[arg(long)]
        project_id: String,
    },
    /// Read the versioned B1 project-domain projection.
    DomainSnapshot {
        #[arg(long)]
        command_id: String,
        #[arg(long)]
        project_id: String,
    },
}

#[derive(Debug, Subcommand)]
enum RevisionCommand {
    /// List immutable revisions for a project.
    List {
        #[arg(long)]
        command_id: String,
        #[arg(long)]
        project_id: String,
    },
    /// Read one immutable revision.
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
    /// Start an in-memory session for an existing project.
    Start {
        #[arg(long)]
        command_id: String,
        #[arg(long)]
        project_id: String,
    },
    /// Read a session snapshot and its covered stream sequence.
    Snapshot {
        #[arg(long)]
        command_id: String,
        #[arg(long)]
        session_id: String,
    },
}

#[derive(Debug, Subcommand)]
enum TurnCommand {
    /// Start a Fake Turn that remains running until explicitly cancelled.
    Start {
        #[arg(long)]
        command_id: String,
        #[arg(long)]
        session_id: String,
        #[arg(long)]
        prompt: String,
    },
    /// Request cancellation and complete the Fake Turn as cancelled.
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
    /// Resume a Session Rollout after a sequence number.
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
    /// Choose one of the question's advertised choice IDs.
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
    /// Decide an approval after echoing its complete subject digest.
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
    /// Read an occurrence manifest through the command protocol.
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
            listen,
            queue_capacity,
            query_queue_capacity,
        } => serve(listen, queue_capacity, query_queue_capacity).await,
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
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("failed to bind Local Service at {listen}"))?;
    let local_addr = listener
        .local_addr()
        .context("failed to read listen address")?;
    let origin = format!("http://{local_addr}");
    let (service, runner) = AppService::build_with_capacities(capacity, query_capacity);
    tokio::spawn(runner.run());
    let auth = HttpAuth::new(token, origin, local_addr.to_string());
    let bootstrap_code = auth.bootstrap_code_for_terminal();
    let app = alda_agent::http::router(service, auth);

    eprintln!("Alda Agent development Local Service listening at http://{local_addr}");
    eprintln!("One-time browser bootstrap code (expires in 5 minutes): {bootstrap_code}");
    eprintln!("{DEVELOPMENT_STATUS}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("Local Service failed")
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
    url.set_path("/v1/commands");
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
            "http://127.0.0.1:37891/v1/commands"
        );
    }

    #[test]
    fn validated_endpoint_derives_the_exact_origin() {
        let endpoint = command_endpoint("http://127.0.0.1:37891/base?secret=value#fragment")
            .expect("explicit loopback URL");

        assert_eq!(endpoint.as_str(), "http://127.0.0.1:37891/v1/commands");
        assert_eq!(
            endpoint.origin().ascii_serialization(),
            "http://127.0.0.1:37891"
        );
    }

    #[test]
    fn development_status_matches_the_a4_runtime_surface() {
        assert!(DEVELOPMENT_STATUS.contains("PWA bootstrap"));
        assert!(DEVELOPMENT_STATUS.contains("WebSocket streaming are available"));
        assert!(DEVELOPMENT_STATUS.contains("state is process-local and in memory"));
        assert!(DEVELOPMENT_STATUS.contains("persistence is not implemented"));
        assert!(!DEVELOPMENT_STATUS.contains("WebSocket streaming are not implemented"));
        assert!(!DEVELOPMENT_STATUS.contains("PWA bootstrap is not implemented"));
    }

    #[tokio::test]
    async fn serve_rejects_port_zero_before_binding() {
        let error = serve("127.0.0.1:0".parse().expect("socket address"), 64, 32)
            .await
            .expect_err("port zero must be rejected");
        assert!(error.to_string().contains("port 0"));
    }
}
