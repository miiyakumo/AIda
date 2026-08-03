#![allow(
    clippy::missing_errors_doc,
    reason = "Rustdoc 依照仓库语言规范使用中文“错误”标题"
)]

use serde::Deserialize;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

pub const PROTOCOL_VERSION: u32 = 2;
pub const SESSION_STREAM_EPOCH: u64 = 1;
pub const EVENT_PAGE_LIMIT: usize = 256;

#[allow(
    dead_code,
    reason = "C1a1 先冻结由后续 durable planner 消费的命令摘要契约"
)]
#[derive(Debug, Error)]
#[error("canonical command serialization failed")]
pub(crate) struct CanonicalCommandError(#[source] serde_json::Error);

#[allow(
    dead_code,
    reason = "C1a1 先冻结由后续 durable planner 消费的命令摘要契约"
)]
pub(crate) fn external_command_payload_digest(
    protocol_version: u32,
    command: &ClientCommand,
) -> Result<String, CanonicalCommandError> {
    external_command_payload_digest_from_serializable(protocol_version, command)
}

fn external_command_payload_digest_from_serializable<T: Serialize + ?Sized>(
    protocol_version: u32,
    command: &T,
) -> Result<String, CanonicalCommandError> {
    let canonical =
        serde_json::to_vec(&(protocol_version, command)).map_err(CanonicalCommandError)?;
    Ok(format!("sha256:{:x}", Sha256::digest(canonical)))
}

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
        #[serde(transparent)]
        pub struct $name(pub String);
    };
}

string_id!(ClientId);
string_id!(ClientCommandId);
string_id!(ProjectId);
string_id!(SessionId);
string_id!(TurnId);
string_id!(QuestionId);
string_id!(ChoiceId);
string_id!(ApprovalId);
string_id!(ArtifactOccurrenceId);
string_id!(ScoreRevisionId);

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
    ProjectCreate {
        name: String,
    },
    ProjectSnapshot {
        project_id: ProjectId,
    },
    ProjectDomainSnapshot {
        project_id: ProjectId,
    },
    RevisionList {
        project_id: ProjectId,
    },
    RevisionRead {
        project_id: ProjectId,
        revision_id: ScoreRevisionId,
    },
    SessionStart {
        project_id: ProjectId,
    },
    SessionSnapshot {
        session_id: SessionId,
    },
    TurnStart {
        session_id: SessionId,
        prompt: String,
    },
    TurnCancel {
        session_id: SessionId,
        turn_id: TurnId,
    },
    QuestionRespond {
        session_id: SessionId,
        question_id: QuestionId,
        choice_id: ChoiceId,
    },
    ApprovalRespond {
        session_id: SessionId,
        approval_id: ApprovalId,
        approval_subject_digest: ApprovalSubjectDigest,
        decision: ApprovalDecision,
    },
    ArtifactManifest {
        project_id: ProjectId,
        artifact_occurrence_id: ArtifactOccurrenceId,
    },
    EventResume {
        cursor: StreamCursor,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectSnapshot {
    pub project_id: ProjectId,
    pub name: String,
    pub version: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DomainProjectSnapshotV1 {
    pub schema_version: u32,
    pub project_id: ProjectId,
    pub score_id: String,
    pub active_brief_id: Option<String>,
    pub accepted_revision_id: Option<ScoreRevisionId>,
    pub takes: Vec<TakeSummaryV1>,
    pub branches: Vec<BranchSummaryV1>,
    pub revisions: Vec<RevisionSummaryV1>,
    pub projection_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TakeSummaryV1 {
    pub take_id: String,
    pub common_base_revision_id: Option<ScoreRevisionId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BranchSummaryV1 {
    pub branch_id: String,
    pub take_id: String,
    pub head_revision_id: Option<ScoreRevisionId>,
    pub fork_base_revision_id: Option<ScoreRevisionId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RevisionSummaryV1 {
    pub revision_id: ScoreRevisionId,
    pub take_id: String,
    pub branch_id: String,
    pub parent_revision_ids: Vec<ScoreRevisionId>,
    pub lifecycle: String,
    pub source_artifact_hash: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RevisionDetailV1 {
    pub summary: RevisionSummaryV1,
    pub project_id: ProjectId,
    pub score_id: String,
    pub brief_revision_id: String,
    pub ir_artifact_hash: Option<String>,
    pub origin: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionSnapshot {
    pub session_id: SessionId,
    pub project_id: ProjectId,
    pub stream_epoch: u64,
    pub covered_through_sequence: u64,
    pub turns: Vec<TurnSnapshot>,
    pub questions: Vec<PendingQuestion>,
    pub approvals: Vec<PendingApproval>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TurnSnapshot {
    pub turn_id: TurnId,
    pub status: TurnStatus,
    pub terminal_sequence: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QuestionChoice {
    pub choice_id: ChoiceId,
    pub label: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QuestionAnswer {
    pub choice_id: ChoiceId,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionStatus {
    Pending,
    Answered,
    OwnerTurnAborted,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingQuestion {
    pub question_id: QuestionId,
    pub session_id: SessionId,
    pub owner_turn_id: TurnId,
    pub prompt: String,
    pub choices: Vec<QuestionChoice>,
    pub status: QuestionStatus,
    pub created_sequence: u64,
    pub terminal_sequence: Option<u64>,
    pub answer: Option<QuestionAnswer>,
    pub responder_client_id: Option<ClientId>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    ModelEgress,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovalPayload {
    pub action: String,
    pub effect: EffectClass,
    pub target: String,
    pub scope: String,
    pub estimated_impact: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovalSubjectDigest {
    pub algorithm: String,
    pub schema_version: u32,
    pub value: String,
}

pub(crate) fn approval_subject_digest_v1(
    provider_origin: &str,
    egress_field_names: &[&str],
    owner_turn_id: &TurnId,
    prompt: &str,
) -> ApprovalSubjectDigest {
    let mut fields = egress_field_names.to_vec();
    fields.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    fields.dedup();
    let prompt_digest = format!("{:x}", Sha256::digest(prompt.as_bytes()));
    let canonical = serde_json::to_vec(&(
        "alda-agent.approval-subject",
        1_u32,
        provider_origin,
        fields,
        &owner_turn_id.0,
        prompt_digest,
    ))
    .expect("canonical approval tuple is serializable");
    ApprovalSubjectDigest {
        algorithm: "sha256".to_owned(),
        schema_version: 1,
        value: format!("{:x}", Sha256::digest(canonical)),
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approve,
    Deny,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
    Expired,
    OwnerTurnAborted,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingApproval {
    pub approval_id: ApprovalId,
    pub session_id: SessionId,
    pub owner_turn_id: TurnId,
    pub payload: ApprovalPayload,
    pub approval_subject_digest: ApprovalSubjectDigest,
    pub status: ApprovalStatus,
    pub created_sequence: u64,
    pub terminal_sequence: Option<u64>,
    pub decision: Option<ApprovalDecision>,
    pub responder_client_id: Option<ClientId>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ArtifactHash(String);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactHashParseError;

impl std::fmt::Display for ArtifactHashParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("artifact hash must be sha256: plus 64 lowercase hex characters")
    }
}

impl std::error::Error for ArtifactHashParseError {}

impl ArtifactHash {
    /// 解析 canonical content hash。
    ///
    /// # 错误
    ///
    /// 除非输入由 `sha256:` 加恰好 64 个小写十六进制字符组成，否则返回
    /// [`ArtifactHashParseError`]。
    pub fn parse(value: &str) -> Result<Self, ArtifactHashParseError> {
        let Some(hex) = value.strip_prefix("sha256:") else {
            return Err(ArtifactHashParseError);
        };
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ArtifactHashParseError);
        }
        Ok(Self(value.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn hex(&self) -> &str {
        self.0.strip_prefix("sha256:").unwrap_or_default()
    }
}

impl<'de> Deserialize<'de> for ArtifactHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    AldaSource,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactProducer {
    FakeProviderFixtureV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactDurability {
    ProcessLifetimeFixture,
    DurableLocal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArtifactManifest {
    pub artifact_occurrence_id: ArtifactOccurrenceId,
    pub artifact_hash: ArtifactHash,
    pub kind: ArtifactKind,
    pub mime_type: String,
    pub size_bytes: u64,
    pub producer: ArtifactProducer,
    pub project_id: ProjectId,
    pub source_session_id: SessionId,
    pub source_turn_id: TurnId,
    pub fixture_version: u32,
    pub created_sequence: u64,
    pub provenance_label: String,
    pub durability: ArtifactDurability,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Running,
    CancelRequested,
    WaitingForInput,
    Succeeded,
    Failed,
    BudgetExceeded,
    Cancelled,
    AbortedByRestart,
}

impl TurnStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded
                | Self::Failed
                | Self::BudgetExceeded
                | Self::Cancelled
                | Self::AbortedByRestart
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    SessionRollout,
    ProjectEvent,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StreamCursor {
    pub stream_kind: StreamKind,
    pub stream_id: String,
    pub epoch: u64,
    pub after_sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionEvent {
    pub sequence: u64,
    #[serde(flatten)]
    pub event: SessionEventKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum SessionEventKind {
    SessionStarted {
        session_id: SessionId,
        project_id: ProjectId,
    },
    TurnStarted {
        turn_id: TurnId,
    },
    TurnCancelRequested {
        turn_id: TurnId,
    },
    TurnCompleted {
        turn_id: TurnId,
        status: TurnStatus,
    },
    QuestionRequested {
        question: PendingQuestion,
    },
    QuestionResolved {
        question_id: QuestionId,
        choice_id: ChoiceId,
        responder_client_id: ClientId,
    },
    ApprovalRequested {
        approval: PendingApproval,
    },
    ApprovalResolved {
        approval_id: ApprovalId,
        approval_subject_digest: ApprovalSubjectDigest,
        decision: ApprovalDecision,
        responder_client_id: ClientId,
    },
    QuestionOwnerTurnAborted {
        question_id: QuestionId,
        owner_turn_id: TurnId,
        owner_terminal_status: TurnStatus,
    },
    ApprovalOwnerTurnAborted {
        approval_id: ApprovalId,
        owner_turn_id: TurnId,
        owner_terminal_status: TurnStatus,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventPage {
    pub stream_kind: StreamKind,
    pub stream_id: String,
    pub epoch: u64,
    pub head_sequence: u64,
    pub events: Vec<SessionEvent>,
    pub next_after_sequence: u64,
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
    ProjectDomainSnapshot(DomainProjectSnapshotV1),
    RevisionList(Vec<RevisionSummaryV1>),
    RevisionRead(RevisionDetailV1),
    SessionStarted(SessionSnapshot),
    SessionSnapshot(SessionSnapshot),
    TurnStarted(TurnSnapshot),
    TurnCancelled(TurnSnapshot),
    TurnAlreadyTerminal {
        turn_id: TurnId,
        terminal_status: TurnStatus,
        terminal_sequence: u64,
    },
    QuestionAnswered(PendingQuestion),
    QuestionAlreadyResolved(PendingQuestion),
    ApprovalDecided {
        approval: PendingApproval,
        artifact_manifest: Option<ArtifactManifest>,
    },
    ApprovalAlreadyResolved(PendingApproval),
    ArtifactManifest(ArtifactManifest),
    EventsResumed(EventPage),
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
        Self::error_with_details(client_command_id, code, message, None)
    }

    #[must_use]
    pub fn error_with_details(
        client_command_id: ClientCommandId,
        code: ProtocolErrorCode,
        message: impl Into<String>,
        details: Option<ProtocolErrorDetails>,
    ) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            client_command_id,
            outcome: CommandOutcome::Error {
                error: ProtocolError {
                    code,
                    message: message.into(),
                    details,
                },
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
// stable wire DTO 有意保持为直接 enum payload；serde 不会在线路上暴露 Rust 表示大小。
#[allow(clippy::large_enum_variant)]
pub enum CommandOutcome {
    Success { result: CommandResult },
    Error { error: ProtocolError },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProtocolError {
    pub code: ProtocolErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<ProtocolErrorDetails>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProtocolErrorDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_epoch: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_epoch: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_sequence: Option<u64>,
    pub recovery_action: RecoveryAction,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "session_id", rename_all = "snake_case")]
pub enum RecoveryAction {
    None,
    FetchSessionSnapshot(SessionId),
    UseSupportedStreamKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum WsClientMessage {
    Command(CommandEnvelope),
    Subscribe {
        session_id: SessionId,
        epoch: u64,
        after_sequence: u64,
    },
    Unsubscribe {
        session_id: SessionId,
    },
    Ping,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum WsServerMessage {
    CommandReply(CommandReply),
    SessionEvents {
        subscription_generation: u64,
        page: EventPage,
    },
    Lagged {
        subscription_generation: u64,
        session_id: SessionId,
        last_delivered_sequence: u64,
        recovery: RecoveryAction,
    },
    ProtocolError {
        code: ProtocolErrorCode,
        message: String,
        recovery: Option<RecoveryAction>,
    },
    Pong,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolErrorCode {
    InvalidProtocolVersion,
    InvalidRequest,
    IdempotencyConflict,
    ProjectNotFound,
    RevisionNotFound,
    SessionNotFound,
    TurnNotFound,
    TurnOwnershipMismatch,
    QuestionNotFound,
    QuestionOwnershipMismatch,
    InvalidQuestionChoice,
    QuestionAlreadyResolved,
    ApprovalNotFound,
    ApprovalOwnershipMismatch,
    ApprovalAlreadyResolved,
    ApprovalSubjectMismatch,
    RequestOwnerTurnAborted,
    ArtifactNotFound,
    ArtifactPreparationFailed,
    EventTooLarge,
    InvalidCursor,
    CursorEpochMismatch,
    UnsupportedStreamKind,
    Overloaded,
    ServiceUnavailable,
}

#[cfg(test)]
mod tests {
    use serde::ser::Error as _;

    use super::*;

    #[test]
    fn command_digest_matches_the_frozen_initialize_vector() {
        let canonical =
            serde_json::to_vec(&(1_u32, &ClientCommand::Initialize)).expect("序列化固定命令向量");
        assert_eq!(canonical, br#"[1,{"type":"initialize"}]"#);
        assert_eq!(
            external_command_payload_digest(1, &ClientCommand::Initialize)
                .expect("计算固定命令摘要"),
            "sha256:bbc84e52448ca1368c8642238783b130d5bef4ad49c41ec5106fc7c3e42e28ca"
        );
    }

    #[test]
    fn command_digest_changes_only_with_protocol_or_command_payload() {
        let first = CommandEnvelope {
            protocol_version: 1,
            client_id: ClientId("client-first".to_owned()),
            client_command_id: ClientCommandId("command-first".to_owned()),
            command: ClientCommand::Initialize,
        };
        let second = CommandEnvelope {
            protocol_version: 1,
            client_id: ClientId("client-second".to_owned()),
            client_command_id: ClientCommandId("command-second".to_owned()),
            command: ClientCommand::Initialize,
        };
        let first_digest = external_command_payload_digest(first.protocol_version, &first.command)
            .expect("计算第一条命令摘要");
        let second_digest =
            external_command_payload_digest(second.protocol_version, &second.command)
                .expect("计算第二条命令摘要");
        assert_eq!(first_digest, second_digest);
        assert_ne!(
            first_digest,
            external_command_payload_digest(2, &ClientCommand::Initialize)
                .expect("计算不同协议版本摘要")
        );
        assert_ne!(
            first_digest,
            external_command_payload_digest(
                1,
                &ClientCommand::ProjectCreate {
                    name: "练习曲".to_owned(),
                },
            )
            .expect("计算不同命令摘要")
        );
    }

    #[test]
    fn command_digest_propagates_serialization_errors() {
        struct SerializationFailure;

        impl Serialize for SerializationFailure {
            fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                Err(S::Error::custom("注入序列化失败"))
            }
        }

        assert!(
            external_command_payload_digest_from_serializable(1, &SerializationFailure).is_err()
        );
    }
}
