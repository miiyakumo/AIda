use std::env;
use std::net::IpAddr;
use std::net::SocketAddr;

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
use alda_agent::protocol::ProjectId;
use anyhow::Context;
use anyhow::bail;
use clap::Parser;
use clap::Subcommand;

const SESSION_TOKEN_ENV: &str = "ALDA_AGENT_SESSION_TOKEN";
const DEFAULT_SERVER: &str = "http://127.0.0.1:37891";

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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve {
            listen,
            queue_capacity,
        } => serve(listen, queue_capacity).await,
        Command::Project {
            server,
            client_id,
            command,
        } => project(&server, client_id, command).await,
    }
}

async fn serve(listen: SocketAddr, queue_capacity: usize) -> anyhow::Result<()> {
    if !listen.ip().is_loopback() {
        bail!("Local Service refuses non-loopback listen address `{listen}`");
    }
    let capacity = QueueCapacity::new(queue_capacity)?;
    let token = session_token()?;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("failed to bind Local Service at {listen}"))?;
    let local_addr = listener
        .local_addr()
        .context("failed to read listen address")?;
    let origin = format!("http://{local_addr}");
    let service = AppService::spawn(capacity);
    let app = alda_agent::http::router(
        service,
        HttpAuth::new(token, origin, local_addr.to_string()),
    );

    eprintln!("Alda Agent development Local Service listening at http://{local_addr}");
    eprintln!("PWA bootstrap, persistence, and WebSocket streaming are not implemented yet");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("Local Service failed")
}

async fn project(server: &str, client_id: String, command: ProjectCommand) -> anyhow::Result<()> {
    let token = session_token()?;
    let (client_command_id, command) = match command {
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
    };
    let envelope = CommandEnvelope {
        protocol_version: PROTOCOL_VERSION,
        client_id: ClientId(client_id),
        client_command_id,
        command,
    };
    let endpoint = command_endpoint(server)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("failed to build Local Service HTTP client")?;
    let response = client
        .post(endpoint)
        .bearer_auth(token)
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
    fn command_endpoint_rejects_remote_and_ambiguous_hosts() {
        assert!(command_endpoint("https://127.0.0.1:37891").is_err());
        assert!(command_endpoint("http://example.com:37891").is_err());
        assert!(command_endpoint("http://localhost:37891").is_err());
        assert_eq!(
            command_endpoint("http://127.0.0.1:37891/base?secret=value")
                .expect("explicit loopback URL")
                .as_str(),
            "http://127.0.0.1:37891/v1/commands"
        );
    }
}
