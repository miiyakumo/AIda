use serde::Deserialize;
use serde::Serialize;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ClientId(pub String);

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ClientCommandId(pub String);

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ProjectId(pub String);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommandEnvelope {
    pub protocol_version: u32,
    pub client_id: ClientId,
    pub client_command_id: ClientCommandId,
    pub command: ClientCommand,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "params", rename_all = "snake_case")]
pub enum ClientCommand {
    Initialize,
    ProjectCreate { name: String },
    ProjectSnapshot { project_id: ProjectId },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectSnapshot {
    pub project_id: ProjectId,
    pub name: String,
    pub version: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum CommandResult {
    Initialized {
        server_name: String,
        protocol_version: u32,
        capabilities: Vec<String>,
    },
    ProjectCreated(ProjectSnapshot),
    ProjectSnapshot(ProjectSnapshot),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommandReply {
    pub protocol_version: u32,
    pub client_command_id: ClientCommandId,
    #[serde(flatten)]
    pub outcome: CommandOutcome,
}

impl CommandReply {
    #[must_use]
    pub fn success(client_command_id: ClientCommandId, result: CommandResult) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            client_command_id,
            outcome: CommandOutcome::Success { result },
        }
    }

    #[must_use]
    pub fn error(
        client_command_id: ClientCommandId,
        code: ProtocolErrorCode,
        message: impl Into<String>,
    ) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            client_command_id,
            outcome: CommandOutcome::Error {
                error: ProtocolError {
                    code,
                    message: message.into(),
                },
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CommandOutcome {
    Success { result: CommandResult },
    Error { error: ProtocolError },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProtocolError {
    pub code: ProtocolErrorCode,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolErrorCode {
    InvalidProtocolVersion,
    InvalidRequest,
    IdempotencyConflict,
    ProjectNotFound,
    Overloaded,
    ServiceUnavailable,
}
