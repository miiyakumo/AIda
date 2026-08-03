//! B3b Session Rollout 持久化基础。
//!
//! 本模块有意不连接 production `AppService`。stored JSON 先解码成只含 primitive 的 DTO，
//! 再通过下方经过验证的 reducer 重建。

#![allow(
    dead_code,
    reason = "B3b freezes the rollout API before B4 production integration"
)]
#![allow(
    clippy::large_enum_variant,
    clippy::missing_errors_doc,
    clippy::result_large_err,
    clippy::too_many_lines,
    reason = "the stored whitelist and ownership-preserving typestates are intentionally explicit"
)]

#[cfg(test)]
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::CStr;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::str;
use std::sync::Arc;

use rustix::fs::{Dir, Mode, OFlags, fsync, openat, renameat};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::control_store::SessionAllocationCatalogContext;
use crate::protocol::{
    ApprovalDecision, ApprovalId, ApprovalPayload, ApprovalStatus, ApprovalSubjectDigest, ChoiceId,
    ClientCommand, ClientCommandId, ClientId, CommandOutcome, CommandReply, CommandResult,
    EventPage, PROTOCOL_VERSION, PendingApproval, PendingQuestion, ProjectId, ProtocolErrorCode,
    QuestionAnswer, QuestionChoice, QuestionId, QuestionStatus, SESSION_STREAM_EPOCH, SessionEvent,
    SessionEventKind, SessionId, SessionSnapshot, StreamKind, TurnId, TurnSnapshot, TurnStatus,
    external_command_payload_digest,
};

#[cfg(test)]
use super::{AppendFailpoint, CheckpointFailpoint, RepairFailpoint};
use super::{
    AppendOutcome, DirectoryKind, FILE_MODE, InitFailpoint, MAX_CHECKPOINT_BYTES, MAX_EVENTS,
    MAX_LINE_BYTES, StateStore, StateStoreError, StoredCommandRecordV1, StoredTransactionCommitV1,
    TransactionCommit, TransactionProbe, ensure_directory, io_error, probe_transaction_index,
    random_hex_128, stored_transaction_index, validate_directory, validate_regular_file,
    validate_sha256,
};

const ROLLOUT_FILE: &str = "rollout-v1.jsonl";
const SESSION_CHECKPOINT_FILE: &str = "session-checkpoint-v1.json";
const MAX_ID_BYTES: usize = 256;
const MAX_PROMPT_BYTES: usize = 8_000;
const MAX_TEXT_BYTES: usize = 8_000;
const MAX_CHOICES: usize = 64;
const MAX_EGRESS_FIELDS: usize = 64;
const MAX_SESSIONS: usize = 100_000;
const ID_ALLOCATION_ATTEMPTS: usize = 32;

#[cfg(test)]
thread_local! {
    static SESSION_CHECKPOINT_LOAD_OBSERVED: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn reset_checkpoint_load_observed() {
    SESSION_CHECKPOINT_LOAD_OBSERVED.set(false);
}

#[cfg(test)]
pub(crate) fn checkpoint_load_observed() -> bool {
    SESSION_CHECKPOINT_LOAD_OBSERVED.get()
}
pub(crate) const INTERNAL_CLIENT_PREFIX: &str = "__alda_internal_";
pub(crate) const INTERNAL_RESTART_CLIENT_ID: &str = "__alda_internal_restart_v1";

pub(crate) fn allocate_typed_id(
    prefix: &str,
    occupied: &BTreeSet<String>,
) -> Result<String, StateStoreError> {
    allocate_typed_id_with(prefix, occupied, random_hex_128)
}

fn allocate_typed_id_with(
    prefix: &str,
    occupied: &BTreeSet<String>,
    mut candidate: impl FnMut() -> String,
) -> Result<String, StateStoreError> {
    if !matches!(prefix, "session" | "turn" | "question" | "approval") {
        return Err(StateStoreError::IncompatibleSchema);
    }
    for _ in 0..ID_ALLOCATION_ATTEMPTS {
        let entropy = candidate();
        if entropy.len() != 32
            || !entropy
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(StateStoreError::IncompatibleSchema);
        }
        let id = format!("{prefix}-{entropy}");
        if !occupied.contains(&id) {
            return Ok(id);
        }
    }
    Err(StateStoreError::IdempotencyConflict)
}

/// 已验证的进程内事件 vocabulary；绝不直接反序列化。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SessionRolloutEvent {
    SessionStarted {
        session_id: SessionId,
        project_id: ProjectId,
    },
    TurnStarted {
        turn_id: TurnId,
        canonical_prompt: String,
    },
    TurnCancelRequested {
        turn_id: TurnId,
    },
    TurnCompleted {
        turn_id: TurnId,
        status: TurnStatus,
    },
    /// 权威 budget-exhaustion 事实；它不同于接受任意 stored
    /// `TurnCompleted(BudgetExceeded)` 断言。
    TurnBudgetExceeded {
        turn_id: TurnId,
    },
    QuestionRequested {
        question_id: QuestionId,
        session_id: SessionId,
        owner_turn_id: TurnId,
        prompt: String,
        choices: Vec<QuestionChoice>,
    },
    QuestionResolved {
        question_id: QuestionId,
        choice_id: ChoiceId,
        responder_client_id: ClientId,
    },
    ApprovalRequested {
        approval_id: ApprovalId,
        session_id: SessionId,
        owner_turn_id: TurnId,
        payload: ApprovalPayload,
        subject_inputs: ApprovalSubjectInputsV1,
        approval_subject_digest: ApprovalSubjectDigest,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ApprovalSubjectInputsV1 {
    pub provider_origin: String,
    pub egress_field_names: Vec<String>,
}

impl ApprovalSubjectInputsV1 {
    pub(crate) fn canonical(
        provider_origin: impl Into<String>,
        egress_field_names: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, StateStoreError> {
        let provider_origin = validate_text(provider_origin.into(), MAX_TEXT_BYTES, false)?;
        let mut egress_field_names = egress_field_names
            .into_iter()
            .map(Into::into)
            .map(|field| validate_text(field, MAX_ID_BYTES, false))
            .collect::<Result<Vec<_>, _>>()?;
        egress_field_names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        egress_field_names.dedup();
        if egress_field_names.is_empty() || egress_field_names.len() > MAX_EGRESS_FIELDS {
            return Err(StateStoreError::ProjectionRejected);
        }
        Ok(Self {
            provider_origin,
            egress_field_names,
        })
    }

    fn from_stored(
        provider_origin: &str,
        egress_field_names: &[String],
    ) -> Result<Self, StateStoreError> {
        let canonical = Self::canonical(provider_origin, egress_field_names.iter().cloned())?;
        if canonical.provider_origin != provider_origin
            || canonical.egress_field_names != egress_field_names
        {
            return Err(StateStoreError::ProjectionRejected);
        }
        Ok(canonical)
    }

    fn digest(&self, owner_turn_id: &TurnId, prompt: &str) -> ApprovalSubjectDigest {
        let fields = self
            .egress_field_names
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        crate::protocol::approval_subject_digest_v1(
            &self.provider_origin,
            &fields,
            owner_turn_id,
            prompt,
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum StoredSessionEventV1 {
    SessionStarted {
        session_id: String,
        project_id: String,
    },
    TurnStarted {
        turn_id: String,
        canonical_prompt: String,
    },
    TurnCancelRequested {
        turn_id: String,
    },
    TurnCompleted {
        turn_id: String,
        status: String,
    },
    TurnBudgetExceeded {
        turn_id: String,
    },
    QuestionRequested {
        question_id: String,
        session_id: String,
        owner_turn_id: String,
        prompt: String,
        choices: Vec<StoredQuestionChoiceV1>,
    },
    QuestionResolved {
        question_id: String,
        choice_id: String,
        responder_client_id: String,
    },
    ApprovalRequested {
        approval_id: String,
        session_id: String,
        owner_turn_id: String,
        payload: StoredApprovalPayloadV1,
        subject_inputs: StoredApprovalSubjectInputsV1,
        approval_subject_digest: StoredApprovalSubjectDigestV1,
    },
    ApprovalResolved {
        approval_id: String,
        approval_subject_digest: StoredApprovalSubjectDigestV1,
        decision: String,
        responder_client_id: String,
    },
    QuestionOwnerTurnAborted {
        question_id: String,
        owner_turn_id: String,
        owner_terminal_status: String,
    },
    ApprovalOwnerTurnAborted {
        approval_id: String,
        owner_turn_id: String,
        owner_terminal_status: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredQuestionChoiceV1 {
    choice_id: String,
    label: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredApprovalPayloadV1 {
    action: String,
    effect: String,
    target: String,
    scope: String,
    estimated_impact: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredApprovalSubjectInputsV1 {
    provider_origin: String,
    egress_field_names: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredApprovalSubjectDigestV1 {
    algorithm: String,
    schema_version: u32,
    value: String,
}

impl StoredSessionEventV1 {
    fn from_live(event: &SessionRolloutEvent) -> Self {
        match event {
            SessionRolloutEvent::SessionStarted {
                session_id,
                project_id,
            } => Self::SessionStarted {
                session_id: session_id.0.clone(),
                project_id: project_id.0.clone(),
            },
            SessionRolloutEvent::TurnStarted {
                turn_id,
                canonical_prompt,
            } => Self::TurnStarted {
                turn_id: turn_id.0.clone(),
                canonical_prompt: canonical_prompt.clone(),
            },
            SessionRolloutEvent::TurnCancelRequested { turn_id } => Self::TurnCancelRequested {
                turn_id: turn_id.0.clone(),
            },
            SessionRolloutEvent::TurnCompleted { turn_id, status } => Self::TurnCompleted {
                turn_id: turn_id.0.clone(),
                status: turn_status_name(*status).to_owned(),
            },
            SessionRolloutEvent::TurnBudgetExceeded { turn_id } => Self::TurnBudgetExceeded {
                turn_id: turn_id.0.clone(),
            },
            SessionRolloutEvent::QuestionRequested {
                question_id,
                session_id,
                owner_turn_id,
                prompt,
                choices,
            } => Self::QuestionRequested {
                question_id: question_id.0.clone(),
                session_id: session_id.0.clone(),
                owner_turn_id: owner_turn_id.0.clone(),
                prompt: prompt.clone(),
                choices: choices
                    .iter()
                    .map(|choice| StoredQuestionChoiceV1 {
                        choice_id: choice.choice_id.0.clone(),
                        label: choice.label.clone(),
                    })
                    .collect(),
            },
            SessionRolloutEvent::QuestionResolved {
                question_id,
                choice_id,
                responder_client_id,
            } => Self::QuestionResolved {
                question_id: question_id.0.clone(),
                choice_id: choice_id.0.clone(),
                responder_client_id: responder_client_id.0.clone(),
            },
            SessionRolloutEvent::ApprovalRequested {
                approval_id,
                session_id,
                owner_turn_id,
                payload,
                subject_inputs,
                approval_subject_digest,
            } => Self::ApprovalRequested {
                approval_id: approval_id.0.clone(),
                session_id: session_id.0.clone(),
                owner_turn_id: owner_turn_id.0.clone(),
                payload: StoredApprovalPayloadV1::from_live(payload),
                subject_inputs: StoredApprovalSubjectInputsV1 {
                    provider_origin: subject_inputs.provider_origin.clone(),
                    egress_field_names: subject_inputs.egress_field_names.clone(),
                },
                approval_subject_digest: StoredApprovalSubjectDigestV1::from_live(
                    approval_subject_digest,
                ),
            },
            SessionRolloutEvent::ApprovalResolved {
                approval_id,
                approval_subject_digest,
                decision,
                responder_client_id,
            } => Self::ApprovalResolved {
                approval_id: approval_id.0.clone(),
                approval_subject_digest: StoredApprovalSubjectDigestV1::from_live(
                    approval_subject_digest,
                ),
                decision: approval_decision_name(*decision).to_owned(),
                responder_client_id: responder_client_id.0.clone(),
            },
            SessionRolloutEvent::QuestionOwnerTurnAborted {
                question_id,
                owner_turn_id,
                owner_terminal_status,
            } => Self::QuestionOwnerTurnAborted {
                question_id: question_id.0.clone(),
                owner_turn_id: owner_turn_id.0.clone(),
                owner_terminal_status: turn_status_name(*owner_terminal_status).to_owned(),
            },
            SessionRolloutEvent::ApprovalOwnerTurnAborted {
                approval_id,
                owner_turn_id,
                owner_terminal_status,
            } => Self::ApprovalOwnerTurnAborted {
                approval_id: approval_id.0.clone(),
                owner_turn_id: owner_turn_id.0.clone(),
                owner_terminal_status: turn_status_name(*owner_terminal_status).to_owned(),
            },
        }
    }

    fn into_live(self) -> Result<SessionRolloutEvent, StateStoreError> {
        Ok(match self {
            Self::SessionStarted {
                session_id,
                project_id,
            } => SessionRolloutEvent::SessionStarted {
                session_id: SessionId(validate_id(session_id)?),
                project_id: ProjectId(validate_id(project_id)?),
            },
            Self::TurnStarted {
                turn_id,
                canonical_prompt,
            } => SessionRolloutEvent::TurnStarted {
                turn_id: TurnId(validate_id(turn_id)?),
                canonical_prompt: validate_text(canonical_prompt, MAX_PROMPT_BYTES, false)?,
            },
            Self::TurnCancelRequested { turn_id } => SessionRolloutEvent::TurnCancelRequested {
                turn_id: TurnId(validate_id(turn_id)?),
            },
            Self::TurnCompleted { turn_id, status } => SessionRolloutEvent::TurnCompleted {
                turn_id: TurnId(validate_id(turn_id)?),
                status: parse_turn_status(&status)?,
            },
            Self::TurnBudgetExceeded { turn_id } => SessionRolloutEvent::TurnBudgetExceeded {
                turn_id: TurnId(validate_id(turn_id)?),
            },
            Self::QuestionRequested {
                question_id,
                session_id,
                owner_turn_id,
                prompt,
                choices,
            } => {
                if choices.is_empty() || choices.len() > MAX_CHOICES {
                    return Err(StateStoreError::ProjectionRejected);
                }
                let mut choice_ids = HashSet::new();
                let choices = choices
                    .into_iter()
                    .map(|choice| {
                        let choice_id = ChoiceId(validate_id(choice.choice_id)?);
                        if !choice_ids.insert(choice_id.clone()) {
                            return Err(StateStoreError::ProjectionRejected);
                        }
                        Ok(QuestionChoice {
                            choice_id,
                            label: validate_text(choice.label, MAX_TEXT_BYTES, false)?,
                        })
                    })
                    .collect::<Result<Vec<_>, StateStoreError>>()?;
                SessionRolloutEvent::QuestionRequested {
                    question_id: QuestionId(validate_id(question_id)?),
                    session_id: SessionId(validate_id(session_id)?),
                    owner_turn_id: TurnId(validate_id(owner_turn_id)?),
                    prompt: validate_text(prompt, MAX_TEXT_BYTES, false)?,
                    choices,
                }
            }
            Self::QuestionResolved {
                question_id,
                choice_id,
                responder_client_id,
            } => SessionRolloutEvent::QuestionResolved {
                question_id: QuestionId(validate_id(question_id)?),
                choice_id: ChoiceId(validate_id(choice_id)?),
                responder_client_id: ClientId(validate_id(responder_client_id)?),
            },
            Self::ApprovalRequested {
                approval_id,
                session_id,
                owner_turn_id,
                payload,
                subject_inputs,
                approval_subject_digest,
            } => SessionRolloutEvent::ApprovalRequested {
                approval_id: ApprovalId(validate_id(approval_id)?),
                session_id: SessionId(validate_id(session_id)?),
                owner_turn_id: TurnId(validate_id(owner_turn_id)?),
                payload: payload.into_live()?,
                subject_inputs: ApprovalSubjectInputsV1::from_stored(
                    &subject_inputs.provider_origin,
                    &subject_inputs.egress_field_names,
                )?,
                approval_subject_digest: approval_subject_digest.into_live()?,
            },
            Self::ApprovalResolved {
                approval_id,
                approval_subject_digest,
                decision,
                responder_client_id,
            } => SessionRolloutEvent::ApprovalResolved {
                approval_id: ApprovalId(validate_id(approval_id)?),
                approval_subject_digest: approval_subject_digest.into_live()?,
                decision: parse_approval_decision(&decision)?,
                responder_client_id: ClientId(validate_id(responder_client_id)?),
            },
            Self::QuestionOwnerTurnAborted {
                question_id,
                owner_turn_id,
                owner_terminal_status,
            } => SessionRolloutEvent::QuestionOwnerTurnAborted {
                question_id: QuestionId(validate_id(question_id)?),
                owner_turn_id: TurnId(validate_id(owner_turn_id)?),
                owner_terminal_status: parse_turn_status(&owner_terminal_status)?,
            },
            Self::ApprovalOwnerTurnAborted {
                approval_id,
                owner_turn_id,
                owner_terminal_status,
            } => SessionRolloutEvent::ApprovalOwnerTurnAborted {
                approval_id: ApprovalId(validate_id(approval_id)?),
                owner_turn_id: TurnId(validate_id(owner_turn_id)?),
                owner_terminal_status: parse_turn_status(&owner_terminal_status)?,
            },
        })
    }
}

impl StoredApprovalPayloadV1 {
    fn from_live(payload: &ApprovalPayload) -> Self {
        Self {
            action: payload.action.clone(),
            effect: "model_egress".to_owned(),
            target: payload.target.clone(),
            scope: payload.scope.clone(),
            estimated_impact: payload.estimated_impact.clone(),
        }
    }

    fn into_live(self) -> Result<ApprovalPayload, StateStoreError> {
        if self.effect != "model_egress" {
            return Err(StateStoreError::ProjectionRejected);
        }
        Ok(ApprovalPayload {
            action: validate_text(self.action, MAX_TEXT_BYTES, false)?,
            effect: crate::protocol::EffectClass::ModelEgress,
            target: validate_text(self.target, MAX_TEXT_BYTES, false)?,
            scope: validate_text(self.scope, MAX_TEXT_BYTES, false)?,
            estimated_impact: validate_text(self.estimated_impact, MAX_TEXT_BYTES, false)?,
        })
    }
}

impl StoredApprovalSubjectDigestV1 {
    fn from_live(digest: &ApprovalSubjectDigest) -> Self {
        Self {
            algorithm: digest.algorithm.clone(),
            schema_version: digest.schema_version,
            value: digest.value.clone(),
        }
    }

    fn into_live(self) -> Result<ApprovalSubjectDigest, StateStoreError> {
        if self.algorithm != "sha256"
            || self.schema_version != 1
            || self.value.len() != 64
            || !self
                .value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(StateStoreError::ProjectionRejected);
        }
        Ok(ApprovalSubjectDigest {
            algorithm: self.algorithm,
            schema_version: self.schema_version,
            value: self.value,
        })
    }
}

fn validate_id(value: String) -> Result<String, StateStoreError> {
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(StateStoreError::ProjectionRejected);
    }
    Ok(value)
}

fn validate_text(
    value: String,
    max_bytes: usize,
    allow_empty: bool,
) -> Result<String, StateStoreError> {
    if (!allow_empty && value.is_empty())
        || value.len() > max_bytes
        || value.contains('\0')
        || value.contains('\r')
    {
        return Err(StateStoreError::ProjectionRejected);
    }
    Ok(value)
}

fn turn_status_name(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Running => "running",
        TurnStatus::CancelRequested => "cancel_requested",
        TurnStatus::WaitingForInput => "waiting_for_input",
        TurnStatus::Succeeded => "succeeded",
        TurnStatus::Failed => "failed",
        TurnStatus::BudgetExceeded => "budget_exceeded",
        TurnStatus::Cancelled => "cancelled",
        TurnStatus::AbortedByRestart => "aborted_by_restart",
    }
}

fn parse_turn_status(value: &str) -> Result<TurnStatus, StateStoreError> {
    match value {
        "running" => Ok(TurnStatus::Running),
        "cancel_requested" => Ok(TurnStatus::CancelRequested),
        "waiting_for_input" => Ok(TurnStatus::WaitingForInput),
        "succeeded" => Ok(TurnStatus::Succeeded),
        "failed" => Ok(TurnStatus::Failed),
        "budget_exceeded" => Ok(TurnStatus::BudgetExceeded),
        "cancelled" => Ok(TurnStatus::Cancelled),
        "aborted_by_restart" => Ok(TurnStatus::AbortedByRestart),
        _ => Err(StateStoreError::ProjectionRejected),
    }
}

fn approval_decision_name(decision: ApprovalDecision) -> &'static str {
    match decision {
        ApprovalDecision::Approve => "approved",
        ApprovalDecision::Deny => "denied",
    }
}

fn parse_approval_decision(value: &str) -> Result<ApprovalDecision, StateStoreError> {
    match value {
        "approved" => Ok(ApprovalDecision::Approve),
        "denied" => Ok(ApprovalDecision::Deny),
        _ => Err(StateStoreError::ProjectionRejected),
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct TurnProjection {
    snapshot: TurnSnapshot,
    canonical_prompt: String,
    terminal_eligibility: Option<TerminalEligibility>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TerminalEligibility {
    ApprovalApproved,
    ApprovalDenied,
    RestartAuthorized,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub(crate) struct SessionRolloutProjection {
    session_id: Option<SessionId>,
    project_id: Option<ProjectId>,
    turns: BTreeMap<String, TurnProjection>,
    turn_order: Vec<String>,
    questions: BTreeMap<String, PendingQuestion>,
    question_order: Vec<String>,
    approvals: BTreeMap<String, PendingApproval>,
    approval_order: Vec<String>,
    head_sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PublishedSessionBatchHead {
    last_sequence: u64,
    checksum: String,
}

/// 只能由 live Ready writer 导出的不可变 Session 查询状态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PublishedSessionReadState {
    expected_session_id: SessionId,
    projection: SessionRolloutProjection,
    events: Vec<SessionRolloutEvent>,
    restart_authorizations: BTreeMap<u64, RestartAuthorizationV1>,
    batch_heads: Vec<PublishedSessionBatchHead>,
    last_sequence: u64,
    last_checksum: String,
    snapshot: SessionSnapshot,
}

impl PublishedSessionReadState {
    fn from_recovered(state: &RecoveredSessionState) -> Result<Self, StateStoreError> {
        let last_checksum = state
            .last_checksum
            .clone()
            .ok_or(StateStoreError::ChecksumChainMismatch)?;
        let published = Self {
            expected_session_id: state.expected_session_id.clone(),
            projection: state.projection.clone(),
            events: state.events.clone(),
            restart_authorizations: state.restart_authorizations.clone(),
            batch_heads: state.batch_heads.clone(),
            last_sequence: state.last_sequence,
            last_checksum,
            snapshot: state.projection.snapshot()?,
        };
        published.validate()?;
        Ok(published)
    }

    pub(crate) fn validate(&self) -> Result<(), StateStoreError> {
        validate_sha256(&self.last_checksum)?;
        let event_count =
            u64::try_from(self.events.len()).map_err(|_| StateStoreError::BatchTooLarge)?;
        if event_count != self.last_sequence
            || self.batch_heads.is_empty()
            || self.batch_heads.last().is_none_or(|head| {
                head.last_sequence != self.last_sequence || head.checksum != self.last_checksum
            })
        {
            return Err(StateStoreError::ChecksumChainMismatch);
        }
        let mut previous_sequence = 0;
        for head in &self.batch_heads {
            validate_sha256(&head.checksum)?;
            if head.last_sequence < previous_sequence || head.last_sequence > self.last_sequence {
                return Err(StateStoreError::SequenceMismatch);
            }
            previous_sequence = head.last_sequence;
        }

        let mut replayed = SessionRolloutProjection::default();
        let mut used_authorizations = BTreeSet::new();
        for (index, event) in self.events.iter().enumerate() {
            if let Some(authorization) = self.restart_authorizations.get(&replayed.head_sequence) {
                if authorization.pre_head_sequence != replayed.head_sequence
                    || !used_authorizations.insert(replayed.head_sequence)
                {
                    return Err(StateStoreError::ProjectionRejected);
                }
                replayed.authorize_restart(authorization)?;
            }
            let sequence = u64::try_from(index)
                .map_err(|_| StateStoreError::BatchTooLarge)?
                .checked_add(1)
                .ok_or(StateStoreError::SequenceMismatch)?;
            replayed.apply(sequence, event)?;
        }
        if used_authorizations.len() != self.restart_authorizations.len()
            || replayed != self.projection
            || replayed.head_sequence != self.last_sequence
        {
            return Err(StateStoreError::ProjectionRejected);
        }
        let snapshot = replayed.snapshot()?;
        if snapshot != self.snapshot
            || snapshot.session_id != self.expected_session_id
            || snapshot.covered_through_sequence != self.last_sequence
        {
            return Err(StateStoreError::StreamMismatch);
        }
        Ok(())
    }

    pub(crate) fn head(&self) -> (u64, &str) {
        (self.last_sequence, self.last_checksum.as_str())
    }

    pub(crate) fn snapshot(&self) -> &SessionSnapshot {
        &self.snapshot
    }

    pub(crate) fn turn_ids(&self) -> impl Iterator<Item = &str> {
        self.snapshot
            .turns
            .iter()
            .map(|turn| turn.turn_id.0.as_str())
    }

    pub(crate) fn question_ids(&self) -> impl Iterator<Item = &str> {
        self.snapshot
            .questions
            .iter()
            .map(|question| question.question_id.0.as_str())
    }

    pub(crate) fn approval_ids(&self) -> impl Iterator<Item = &str> {
        self.snapshot
            .approvals
            .iter()
            .map(|approval| approval.approval_id.0.as_str())
    }

    pub(crate) fn canonical_prompt(&self, turn_id: &TurnId) -> Option<&str> {
        self.projection.canonical_prompt(turn_id)
    }

    fn page(&self, after_sequence: u64) -> Result<EventPage, StateStoreError> {
        if after_sequence > self.last_sequence {
            return Err(StateStoreError::SequenceMismatch);
        }
        let events = self
            .events
            .iter()
            .enumerate()
            .skip(usize::try_from(after_sequence).map_err(|_| StateStoreError::SequenceMismatch)?)
            .take(crate::protocol::EVENT_PAGE_LIMIT)
            .map(|(index, event)| {
                Ok(event_to_wire(
                    u64::try_from(index).map_err(|_| StateStoreError::SequenceMismatch)? + 1,
                    event,
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let next_after_sequence = events.last().map_or(after_sequence, |event| event.sequence);
        Ok(EventPage {
            stream_kind: StreamKind::SessionRollout,
            stream_id: self.expected_session_id.0.clone(),
            epoch: SESSION_STREAM_EPOCH,
            head_sequence: self.last_sequence,
            events,
            next_after_sequence,
        })
    }

    pub(crate) fn resume(
        &self,
        cursor: &crate::protocol::StreamCursor,
    ) -> Result<EventPage, SessionCursorError> {
        if cursor.stream_kind != StreamKind::SessionRollout {
            return Err(SessionCursorError::UnsupportedStreamKind);
        }
        if cursor.stream_id != self.expected_session_id.0 {
            return Err(SessionCursorError::SessionMismatch);
        }
        if cursor.epoch != SESSION_STREAM_EPOCH {
            return Err(SessionCursorError::EpochMismatch {
                expected_epoch: SESSION_STREAM_EPOCH,
                actual_epoch: cursor.epoch,
                head_sequence: self.last_sequence,
            });
        }
        self.page(cursor.after_sequence)
            .map_err(|_| SessionCursorError::Future {
                head_sequence: self.last_sequence,
            })
    }
}

impl std::ops::Deref for PublishedSessionReadState {
    type Target = SessionSnapshot;

    fn deref(&self) -> &Self::Target {
        self.snapshot()
    }
}

impl SessionRolloutProjection {
    pub(crate) fn snapshot(&self) -> Result<SessionSnapshot, StateStoreError> {
        Ok(SessionSnapshot {
            session_id: self
                .session_id
                .clone()
                .ok_or(StateStoreError::ProjectionRejected)?,
            project_id: self
                .project_id
                .clone()
                .ok_or(StateStoreError::ProjectionRejected)?,
            stream_epoch: SESSION_STREAM_EPOCH,
            covered_through_sequence: self.head_sequence,
            turns: self
                .turn_order
                .iter()
                .map(|id| {
                    self.turns
                        .get(id)
                        .map(|turn| turn.snapshot.clone())
                        .ok_or(StateStoreError::ProjectionRejected)
                })
                .collect::<Result<_, _>>()?,
            questions: self
                .question_order
                .iter()
                .map(|id| {
                    self.questions
                        .get(id)
                        .cloned()
                        .ok_or(StateStoreError::ProjectionRejected)
                })
                .collect::<Result<_, _>>()?,
            approvals: self
                .approval_order
                .iter()
                .map(|id| {
                    self.approvals
                        .get(id)
                        .cloned()
                        .ok_or(StateStoreError::ProjectionRejected)
                })
                .collect::<Result<_, _>>()?,
        })
    }

    pub(crate) fn canonical_prompt(&self, turn_id: &TurnId) -> Option<&str> {
        self.turns
            .get(&turn_id.0)
            .map(|turn| turn.canonical_prompt.as_str())
    }

    fn canonical_digest(&self) -> Result<String, StateStoreError> {
        let canonical = serde_json::to_vec(&("alda-session-projection-v1", self))
            .map_err(|_| StateStoreError::IncompatibleSchema)?;
        Ok(format!("sha256:{:x}", Sha256::digest(canonical)))
    }

    fn pending_for_turn(&self, turn_id: &TurnId) -> Vec<PendingObject> {
        let mut pending = self
            .questions
            .values()
            .filter(|question| {
                question.owner_turn_id == *turn_id && question.status == QuestionStatus::Pending
            })
            .map(|question| {
                PendingObject::Question(question.created_sequence, question.question_id.clone())
            })
            .chain(
                self.approvals
                    .values()
                    .filter(|approval| {
                        approval.owner_turn_id == *turn_id
                            && approval.status == ApprovalStatus::Pending
                    })
                    .map(|approval| {
                        PendingObject::Approval(
                            approval.created_sequence,
                            approval.approval_id.clone(),
                        )
                    }),
            )
            .collect::<Vec<_>>();
        pending.sort_by_key(PendingObject::sequence);
        pending
    }

    fn authorize_restart(
        &mut self,
        authorization: &RestartAuthorizationV1,
    ) -> Result<(), StateStoreError> {
        if authorization.pre_head_sequence != self.head_sequence
            || authorization.turn_ids.is_empty()
        {
            return Err(StateStoreError::ProjectionRejected);
        }
        let mut unique = HashSet::new();
        for turn_id in &authorization.turn_ids {
            if !unique.insert(turn_id.clone())
                || self.turn(turn_id)?.snapshot.status != TurnStatus::Running
                || !self.pending_for_turn(turn_id).is_empty()
            {
                return Err(StateStoreError::ProjectionRejected);
            }
            self.turn_mut(turn_id)?.terminal_eligibility =
                Some(TerminalEligibility::RestartAuthorized);
        }
        Ok(())
    }

    fn apply(&mut self, sequence: u64, event: &SessionRolloutEvent) -> Result<(), StateStoreError> {
        if sequence != self.head_sequence + 1 {
            return Err(StateStoreError::SequenceMismatch);
        }
        match event {
            SessionRolloutEvent::SessionStarted {
                session_id,
                project_id,
            } => {
                validate_id(session_id.0.clone())?;
                validate_id(project_id.0.clone())?;
                if sequence != 1 || self.session_id.is_some() {
                    return Err(StateStoreError::ProjectionRejected);
                }
                self.session_id = Some(session_id.clone());
                self.project_id = Some(project_id.clone());
            }
            SessionRolloutEvent::TurnStarted {
                turn_id,
                canonical_prompt,
            } => {
                self.require_started()?;
                validate_id(turn_id.0.clone())?;
                validate_text(canonical_prompt.clone(), MAX_PROMPT_BYTES, false)?;
                if self.turns.contains_key(&turn_id.0) {
                    return Err(StateStoreError::ProjectionRejected);
                }
                self.turn_order.push(turn_id.0.clone());
                self.turns.insert(
                    turn_id.0.clone(),
                    TurnProjection {
                        snapshot: TurnSnapshot {
                            turn_id: turn_id.clone(),
                            status: TurnStatus::Running,
                            terminal_sequence: None,
                        },
                        canonical_prompt: canonical_prompt.clone(),
                        terminal_eligibility: None,
                    },
                );
            }
            SessionRolloutEvent::TurnCancelRequested { turn_id } => {
                let turn = self.turn_mut(turn_id)?;
                if turn.snapshot.status.is_terminal()
                    || turn.snapshot.status == TurnStatus::CancelRequested
                {
                    return Err(StateStoreError::ProjectionRejected);
                }
                turn.terminal_eligibility = None;
                turn.snapshot.status = TurnStatus::CancelRequested;
            }
            SessionRolloutEvent::TurnCompleted { turn_id, status } => {
                if !status.is_terminal() {
                    return Err(StateStoreError::ProjectionRejected);
                }
                let current = self.turn(turn_id)?.snapshot.status;
                let eligibility = self.turn(turn_id)?.terminal_eligibility;
                let eligible = match status {
                    TurnStatus::Succeeded => {
                        current == TurnStatus::Running
                            && eligibility == Some(TerminalEligibility::ApprovalApproved)
                    }
                    TurnStatus::Failed => {
                        current == TurnStatus::Running
                            && eligibility == Some(TerminalEligibility::ApprovalDenied)
                    }
                    TurnStatus::Cancelled => current == TurnStatus::CancelRequested,
                    TurnStatus::AbortedByRestart => {
                        current == TurnStatus::Running
                            && eligibility == Some(TerminalEligibility::RestartAuthorized)
                    }
                    // 当前 B3b whitelist 不含权威 budget-exhaustion 事实。
                    TurnStatus::BudgetExceeded => false,
                    _ => false,
                };
                if !self.pending_for_turn(turn_id).is_empty() || !eligible {
                    return Err(StateStoreError::ProjectionRejected);
                }
                let turn = self.turn_mut(turn_id)?;
                turn.snapshot.status = *status;
                turn.snapshot.terminal_sequence = Some(sequence);
                turn.terminal_eligibility = None;
            }
            SessionRolloutEvent::TurnBudgetExceeded { turn_id } => {
                let turn = self.turn(turn_id)?;
                if turn.snapshot.status != TurnStatus::Running
                    || turn.terminal_eligibility.is_some()
                    || !self.pending_for_turn(turn_id).is_empty()
                {
                    return Err(StateStoreError::ProjectionRejected);
                }
                let turn = self.turn_mut(turn_id)?;
                turn.snapshot.status = TurnStatus::BudgetExceeded;
                turn.snapshot.terminal_sequence = Some(sequence);
            }
            SessionRolloutEvent::QuestionRequested {
                question_id,
                session_id,
                owner_turn_id,
                prompt,
                choices,
            } => {
                self.require_session(session_id)?;
                validate_id(question_id.0.clone())?;
                validate_text(prompt.clone(), MAX_TEXT_BYTES, false)?;
                if choices.is_empty()
                    || choices.len() > MAX_CHOICES
                    || self.questions.contains_key(&question_id.0)
                    || self.turn(owner_turn_id)?.snapshot.status != TurnStatus::Running
                    || self.turn(owner_turn_id)?.terminal_eligibility.is_some()
                {
                    return Err(StateStoreError::ProjectionRejected);
                }
                let mut unique = HashSet::new();
                for choice in choices {
                    validate_id(choice.choice_id.0.clone())?;
                    validate_text(choice.label.clone(), MAX_TEXT_BYTES, false)?;
                    if !unique.insert(choice.choice_id.clone()) {
                        return Err(StateStoreError::ProjectionRejected);
                    }
                }
                self.question_order.push(question_id.0.clone());
                self.questions.insert(
                    question_id.0.clone(),
                    PendingQuestion {
                        question_id: question_id.clone(),
                        session_id: session_id.clone(),
                        owner_turn_id: owner_turn_id.clone(),
                        prompt: prompt.clone(),
                        choices: choices.clone(),
                        status: QuestionStatus::Pending,
                        created_sequence: sequence,
                        terminal_sequence: None,
                        answer: None,
                        responder_client_id: None,
                    },
                );
                self.turn_mut(owner_turn_id)?.snapshot.status = TurnStatus::WaitingForInput;
            }
            SessionRolloutEvent::QuestionResolved {
                question_id,
                choice_id,
                responder_client_id,
            } => {
                validate_id(responder_client_id.0.clone())?;
                let owner = {
                    let question = self
                        .questions
                        .get_mut(&question_id.0)
                        .ok_or(StateStoreError::ProjectionRejected)?;
                    if question.status != QuestionStatus::Pending
                        || !question
                            .choices
                            .iter()
                            .any(|choice| choice.choice_id == *choice_id)
                    {
                        return Err(StateStoreError::ProjectionRejected);
                    }
                    question.status = QuestionStatus::Answered;
                    question.terminal_sequence = Some(sequence);
                    question.answer = Some(QuestionAnswer {
                        choice_id: choice_id.clone(),
                    });
                    question.responder_client_id = Some(responder_client_id.clone());
                    question.owner_turn_id.clone()
                };
                if self.turn(&owner)?.snapshot.status != TurnStatus::WaitingForInput {
                    return Err(StateStoreError::ProjectionRejected);
                }
                self.turn_mut(&owner)?.snapshot.status = TurnStatus::Running;
                self.turn_mut(&owner)?.terminal_eligibility = None;
            }
            SessionRolloutEvent::ApprovalRequested {
                approval_id,
                session_id,
                owner_turn_id,
                payload,
                subject_inputs,
                approval_subject_digest,
            } => {
                self.require_session(session_id)?;
                validate_id(approval_id.0.clone())?;
                StoredApprovalPayloadV1::from_live(payload).into_live()?;
                StoredApprovalSubjectDigestV1::from_live(approval_subject_digest).into_live()?;
                if ApprovalSubjectInputsV1::from_stored(
                    &subject_inputs.provider_origin,
                    &subject_inputs.egress_field_names,
                )? != *subject_inputs
                {
                    return Err(StateStoreError::ProjectionRejected);
                }
                let turn = self.turn(owner_turn_id)?;
                if subject_inputs.digest(owner_turn_id, &turn.canonical_prompt)
                    != *approval_subject_digest
                {
                    return Err(StateStoreError::ProjectionRejected);
                }
                if self.approvals.contains_key(&approval_id.0)
                    || turn.snapshot.status != TurnStatus::Running
                    || turn.terminal_eligibility.is_some()
                {
                    return Err(StateStoreError::ProjectionRejected);
                }
                self.approval_order.push(approval_id.0.clone());
                self.approvals.insert(
                    approval_id.0.clone(),
                    PendingApproval {
                        approval_id: approval_id.clone(),
                        session_id: session_id.clone(),
                        owner_turn_id: owner_turn_id.clone(),
                        payload: payload.clone(),
                        approval_subject_digest: approval_subject_digest.clone(),
                        status: ApprovalStatus::Pending,
                        created_sequence: sequence,
                        terminal_sequence: None,
                        decision: None,
                        responder_client_id: None,
                    },
                );
                self.turn_mut(owner_turn_id)?.snapshot.status = TurnStatus::WaitingForInput;
            }
            SessionRolloutEvent::ApprovalResolved {
                approval_id,
                approval_subject_digest,
                decision,
                responder_client_id,
            } => {
                validate_id(responder_client_id.0.clone())?;
                let owner = {
                    let approval = self
                        .approvals
                        .get_mut(&approval_id.0)
                        .ok_or(StateStoreError::ProjectionRejected)?;
                    if approval.status != ApprovalStatus::Pending
                        || approval.approval_subject_digest != *approval_subject_digest
                    {
                        return Err(StateStoreError::ProjectionRejected);
                    }
                    approval.status = match decision {
                        ApprovalDecision::Approve => ApprovalStatus::Approved,
                        ApprovalDecision::Deny => ApprovalStatus::Denied,
                    };
                    approval.terminal_sequence = Some(sequence);
                    approval.decision = Some(*decision);
                    approval.responder_client_id = Some(responder_client_id.clone());
                    approval.owner_turn_id.clone()
                };
                if self.turn(&owner)?.snapshot.status != TurnStatus::WaitingForInput {
                    return Err(StateStoreError::ProjectionRejected);
                }
                self.turn_mut(&owner)?.snapshot.status = TurnStatus::Running;
                self.turn_mut(&owner)?.terminal_eligibility = Some(match decision {
                    ApprovalDecision::Approve => TerminalEligibility::ApprovalApproved,
                    ApprovalDecision::Deny => TerminalEligibility::ApprovalDenied,
                });
            }
            SessionRolloutEvent::QuestionOwnerTurnAborted {
                question_id,
                owner_turn_id,
                owner_terminal_status,
            } => {
                if *owner_terminal_status != TurnStatus::Cancelled
                    || self.turn(owner_turn_id)?.snapshot.status != TurnStatus::CancelRequested
                {
                    return Err(StateStoreError::ProjectionRejected);
                }
                if !matches!(
                    self.pending_for_turn(owner_turn_id).first(),
                    Some(PendingObject::Question(_, expected)) if expected == question_id
                ) {
                    return Err(StateStoreError::ProjectionRejected);
                }
                let question = self
                    .questions
                    .get_mut(&question_id.0)
                    .ok_or(StateStoreError::ProjectionRejected)?;
                if question.status != QuestionStatus::Pending
                    || question.owner_turn_id != *owner_turn_id
                {
                    return Err(StateStoreError::ProjectionRejected);
                }
                question.status = QuestionStatus::OwnerTurnAborted;
                question.terminal_sequence = Some(sequence);
            }
            SessionRolloutEvent::ApprovalOwnerTurnAborted {
                approval_id,
                owner_turn_id,
                owner_terminal_status,
            } => {
                if *owner_terminal_status != TurnStatus::Cancelled
                    || self.turn(owner_turn_id)?.snapshot.status != TurnStatus::CancelRequested
                {
                    return Err(StateStoreError::ProjectionRejected);
                }
                if !matches!(
                    self.pending_for_turn(owner_turn_id).first(),
                    Some(PendingObject::Approval(_, expected)) if expected == approval_id
                ) {
                    return Err(StateStoreError::ProjectionRejected);
                }
                let approval = self
                    .approvals
                    .get_mut(&approval_id.0)
                    .ok_or(StateStoreError::ProjectionRejected)?;
                if approval.status != ApprovalStatus::Pending
                    || approval.owner_turn_id != *owner_turn_id
                {
                    return Err(StateStoreError::ProjectionRejected);
                }
                approval.status = ApprovalStatus::OwnerTurnAborted;
                approval.terminal_sequence = Some(sequence);
            }
        }
        self.head_sequence = sequence;
        Ok(())
    }

    fn require_started(&self) -> Result<(), StateStoreError> {
        if self.session_id.is_none() {
            Err(StateStoreError::ProjectionRejected)
        } else {
            Ok(())
        }
    }

    fn require_session(&self, id: &SessionId) -> Result<(), StateStoreError> {
        if self.session_id.as_ref() == Some(id) {
            Ok(())
        } else {
            Err(StateStoreError::StreamMismatch)
        }
    }

    fn turn(&self, id: &TurnId) -> Result<&TurnProjection, StateStoreError> {
        self.turns
            .get(&id.0)
            .ok_or(StateStoreError::ProjectionRejected)
    }

    fn turn_mut(&mut self, id: &TurnId) -> Result<&mut TurnProjection, StateStoreError> {
        self.turns
            .get_mut(&id.0)
            .ok_or(StateStoreError::ProjectionRejected)
    }
}

fn validate_restart_authorization(
    state: &RecoveredSessionState,
    batch: &StoredSessionBatchV1,
) -> Result<Option<RestartAuthorizationV1>, StateStoreError> {
    let Some(stored) = &batch.restart_authorization else {
        return Ok(None);
    };
    if stored.pre_head_sequence != state.last_sequence || stored.turn_ids.is_empty() {
        return Err(StateStoreError::ProjectionRejected);
    }
    let turn_ids = stored
        .turn_ids
        .iter()
        .cloned()
        .map(validate_id)
        .map(|value| value.map(TurnId))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(RestartAuthorizationV1 {
        pre_head_sequence: stored.pre_head_sequence,
        turn_ids,
    }))
}

fn validate_restart_plan_batch(
    state: &RecoveredSessionState,
    batch: &StoredSessionBatchV1,
    events: &[SessionRolloutEvent],
    authorization: Option<&RestartAuthorizationV1>,
) -> Result<(), StateStoreError> {
    let internal_client = batch
        .command_record
        .as_ref()
        .map(|command| command.client_id.as_str())
        .filter(|client_id| client_id.starts_with(INTERNAL_CLIENT_PREFIX));
    let looks_legacy = authorization.is_some() || batch.transaction_id.starts_with("restart-v1:");
    if !looks_legacy && internal_client.is_none() {
        return Ok(());
    }

    if internal_client.is_none() && looks_legacy {
        let expected = plan_restart_reconciliation(&state.state_instance_id, &state.projection)?
            .ok_or(StateStoreError::ProjectionRejected)?;
        if batch.transaction_id != expected.transaction_id
            || events != expected.events
            || authorization != expected.authorization.as_ref()
            || batch.command_record.is_some()
        {
            return Err(StateStoreError::ProjectionRejected);
        }
        return Ok(());
    }

    if internal_client != Some(INTERNAL_RESTART_CLIENT_ID) {
        return Err(StateStoreError::ProjectionRejected);
    }
    let expected =
        plan_coordinated_restart_reconciliation(&state.state_instance_id, &state.projection)?
            .ok_or(StateStoreError::ProjectionRejected)?;
    if batch.transaction_id != expected.session_transaction_id
        || events != expected.events
        || authorization != expected.authorization.as_ref()
        || batch.command_record.as_ref() != Some(&expected.command_record)
    {
        return Err(StateStoreError::ProjectionRejected);
    }
    Ok(())
}

#[derive(Clone, Debug)]
enum PendingObject {
    Question(u64, QuestionId),
    Approval(u64, ApprovalId),
}

impl PendingObject {
    const fn sequence(&self) -> u64 {
        match self {
            Self::Question(sequence, _) | Self::Approval(sequence, _) => *sequence,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RestartPlan {
    pub transaction_id: String,
    pub events: Vec<SessionRolloutEvent>,
    authorization: Option<RestartAuthorizationV1>,
}

#[cfg(test)]
impl RestartPlan {
    /// 仅供跨模块持久化测试把 trusted planner 结果写入真实 Session log。
    pub(crate) fn into_append_request(self) -> SessionAppendRequest {
        SessionAppendRequest {
            transaction_id: self.transaction_id,
            command_record: None,
            events: self.events,
            restart_authorization: self.authorization,
            command_only_authorization: None,
        }
    }
}

/// identity 在 control 与 Session log 上闭合的 restart obligation；每个字段都源自同一份
/// reconciliation 前 projection。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CoordinatedRestartPlan {
    pub intent: String,
    pub payload_digest: String,
    pub global_tx_id: String,
    pub session_transaction_id: String,
    pub command_record: StoredCommandRecordV1,
    pub events: Vec<SessionRolloutEvent>,
    authorization: Option<RestartAuthorizationV1>,
}

impl CoordinatedRestartPlan {
    pub(crate) fn append_request(&self) -> SessionAppendRequest {
        SessionAppendRequest {
            transaction_id: self.session_transaction_id.clone(),
            command_record: Some(self.command_record.clone()),
            events: self.events.clone(),
            restart_authorization: self.authorization.clone(),
            command_only_authorization: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RestartAuthorizationV1 {
    pre_head_sequence: u64,
    turn_ids: Vec<TurnId>,
}

pub(crate) fn plan_restart_reconciliation(
    instance_id: &str,
    projection: &SessionRolloutProjection,
) -> Result<Option<RestartPlan>, StateStoreError> {
    let session_id = projection
        .session_id
        .as_ref()
        .ok_or(StateStoreError::ProjectionRejected)?;
    let mut events = Vec::new();
    let mut restart_turn_ids = Vec::new();
    for id in &projection.turn_order {
        let turn = projection
            .turns
            .get(id)
            .ok_or(StateStoreError::ProjectionRejected)?;
        match turn.snapshot.status {
            TurnStatus::Running => {
                restart_turn_ids.push(turn.snapshot.turn_id.clone());
                events.push(SessionRolloutEvent::TurnCompleted {
                    turn_id: turn.snapshot.turn_id.clone(),
                    status: TurnStatus::AbortedByRestart,
                });
            }
            TurnStatus::CancelRequested => {
                for pending in projection.pending_for_turn(&turn.snapshot.turn_id) {
                    match pending {
                        PendingObject::Question(_, question_id) => {
                            events.push(SessionRolloutEvent::QuestionOwnerTurnAborted {
                                question_id,
                                owner_turn_id: turn.snapshot.turn_id.clone(),
                                owner_terminal_status: TurnStatus::Cancelled,
                            });
                        }
                        PendingObject::Approval(_, approval_id) => {
                            events.push(SessionRolloutEvent::ApprovalOwnerTurnAborted {
                                approval_id,
                                owner_turn_id: turn.snapshot.turn_id.clone(),
                                owner_terminal_status: TurnStatus::Cancelled,
                            });
                        }
                    }
                }
                events.push(SessionRolloutEvent::TurnCompleted {
                    turn_id: turn.snapshot.turn_id.clone(),
                    status: TurnStatus::Cancelled,
                });
            }
            TurnStatus::WaitingForInput => {
                if projection.pending_for_turn(&turn.snapshot.turn_id).len() != 1 {
                    return Err(StateStoreError::ProjectionRejected);
                }
            }
            status if status.is_terminal() => {}
            _ => return Err(StateStoreError::ProjectionRejected),
        }
    }
    if events.is_empty() {
        return Ok(None);
    }
    Ok(Some(RestartPlan {
        transaction_id: format!(
            "restart-v1:{instance_id}:{}:{}",
            session_id.0, projection.head_sequence
        ),
        events,
        authorization: (!restart_turn_ids.is_empty()).then_some(RestartAuthorizationV1 {
            pre_head_sequence: projection.head_sequence,
            turn_ids: restart_turn_ids,
        }),
    }))
}

/// 规划由 B4 control 协调的 restart reconciliation。不同于上方 legacy planner，
/// 该形式始终携带预留 command identity，以及包含最终 Session snapshot 的 canonical reply。
pub(crate) fn plan_coordinated_restart_reconciliation(
    instance_id: &str,
    projection: &SessionRolloutProjection,
) -> Result<Option<CoordinatedRestartPlan>, StateStoreError> {
    let Some(legacy) = plan_restart_reconciliation(instance_id, projection)? else {
        return Ok(None);
    };
    let intent = legacy.transaction_id;
    let payload_digest = restart_payload_digest(&intent, &legacy.events)?;
    let global_tx_id = restart_global_tx_id(&payload_digest)?;
    let session_transaction_id = format!("{global_tx_id}:session");

    let mut reconciled = projection.clone();
    if let Some(authorization) = &legacy.authorization {
        reconciled.authorize_restart(authorization)?;
    }
    for (offset, event) in legacy.events.iter().enumerate() {
        let sequence = projection
            .head_sequence
            .checked_add(
                u64::try_from(offset)
                    .map_err(|_| StateStoreError::BatchTooLarge)?
                    .checked_add(1)
                    .ok_or(StateStoreError::SequenceMismatch)?,
            )
            .ok_or(StateStoreError::SequenceMismatch)?;
        reconciled.apply(sequence, event)?;
    }
    let stable_reply = serde_json::to_vec(&CommandReply::success(
        ClientCommandId(intent.clone()),
        CommandResult::SessionSnapshot(reconciled.snapshot()?),
    ))
    .map_err(|_| StateStoreError::IncompatibleSchema)?;
    let command_record = StoredCommandRecordV1::new(
        INTERNAL_RESTART_CLIENT_ID,
        intent.clone(),
        payload_digest.clone(),
        &stable_reply,
    )?;
    Ok(Some(CoordinatedRestartPlan {
        intent,
        payload_digest,
        global_tx_id,
        session_transaction_id,
        command_record,
        events: legacy.events,
        authorization: legacy.authorization,
    }))
}

fn restart_payload_digest(
    intent: &str,
    events: &[SessionRolloutEvent],
) -> Result<String, StateStoreError> {
    let events = events
        .iter()
        .map(StoredSessionEventV1::from_live)
        .collect::<Vec<_>>();
    restart_payload_digest_from_stored(intent, &events)
}

fn restart_payload_digest_from_stored(
    intent: &str,
    events: &[StoredSessionEventV1],
) -> Result<String, StateStoreError> {
    let canonical = serde_json::to_vec(&("alda-restart-control-v1", intent, events))
        .map_err(|_| StateStoreError::IncompatibleSchema)?;
    Ok(format!("sha256:{:x}", Sha256::digest(canonical)))
}

fn restart_global_tx_id(payload_digest: &str) -> Result<String, StateStoreError> {
    validate_sha256(payload_digest)?;
    let hex = payload_digest
        .strip_prefix("sha256:")
        .ok_or(StateStoreError::IncompatibleSchema)?;
    let prefix = hex.get(..32).ok_or(StateStoreError::IncompatibleSchema)?;
    Ok(format!("global-{prefix}"))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredCommandOnlyAuthorizationV1 {
    schema_version: u32,
    protocol_version: u32,
    command: ClientCommand,
    reason: CommandOnlyReasonV1,
}

impl StoredCommandOnlyAuthorizationV1 {
    pub(crate) const fn new(command: ClientCommand, reason: CommandOnlyReasonV1) -> Self {
        Self {
            schema_version: 1,
            protocol_version: PROTOCOL_VERSION,
            command,
            reason,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CommandOnlyReasonV1 {
    TurnAlreadyTerminal,
    QuestionAlreadyResolved,
    ApprovalAlreadyResolved,
    TurnOwnershipMismatch,
    QuestionOwnershipMismatch,
    ApprovalOwnershipMismatch,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredSessionBatchV1 {
    schema_version: u32,
    session_id: String,
    stream_id: String,
    epoch: u64,
    transaction_id: String,
    event_count: u64,
    first_sequence: u64,
    last_sequence: u64,
    command_record: Option<StoredCommandRecordV1>,
    restart_authorization: Option<StoredRestartAuthorizationV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command_only_authorization: Option<StoredCommandOnlyAuthorizationV1>,
    events: Vec<StoredSessionEventV1>,
    previous_batch_checksum: Option<String>,
    batch_checksum: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredRestartAuthorizationV1 {
    pre_head_sequence: u64,
    turn_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredSessionCheckpointV1 {
    schema_version: u32,
    projection_schema_version: u32,
    session_id: String,
    stream_id: String,
    epoch: u64,
    covered_sequence: u64,
    covered_batch_checksum: Option<String>,
    covered_valid_bytes: u64,
    projection_digest: String,
    projection: serde_json::Value,
    events: Vec<StoredSessionEventV1>,
    command_index: Vec<StoredCommandRecordV1>,
    transaction_index: Vec<StoredTransactionCommitV1>,
    checksum: String,
}

#[derive(Clone, Debug)]
struct RecoveredSessionState {
    state_instance_id: String,
    expected_session_id: SessionId,
    last_sequence: u64,
    last_checksum: Option<String>,
    projection: SessionRolloutProjection,
    events: Vec<SessionRolloutEvent>,
    restart_authorizations: BTreeMap<u64, RestartAuthorizationV1>,
    batch_heads: Vec<PublishedSessionBatchHead>,
    commands: BTreeMap<(String, String), StoredCommandRecordV1>,
    transactions: BTreeMap<String, TransactionCommit>,
    valid_bytes: u64,
}

pub(crate) struct SessionAppendRequest {
    pub transaction_id: String,
    pub command_record: Option<StoredCommandRecordV1>,
    pub events: Vec<SessionRolloutEvent>,
    restart_authorization: Option<RestartAuthorizationV1>,
    command_only_authorization: Option<StoredCommandOnlyAuthorizationV1>,
}

impl SessionAppendRequest {
    pub(crate) fn new(
        transaction_id: String,
        command_record: Option<StoredCommandRecordV1>,
        events: Vec<SessionRolloutEvent>,
    ) -> Self {
        Self {
            transaction_id,
            command_record,
            events,
            restart_authorization: None,
            command_only_authorization: None,
        }
    }

    pub(crate) fn new_command_only(
        transaction_id: String,
        command_record: StoredCommandRecordV1,
        authorization: StoredCommandOnlyAuthorizationV1,
    ) -> Self {
        Self {
            transaction_id,
            command_record: Some(command_record),
            events: Vec::new(),
            restart_authorization: None,
            command_only_authorization: Some(authorization),
        }
    }

    pub(crate) fn canonical_plan_digest(
        &self,
        session_id: &SessionId,
    ) -> Result<String, StateStoreError> {
        let events = self
            .events
            .iter()
            .map(StoredSessionEventV1::from_live)
            .collect::<Vec<_>>();
        let authorization =
            self.restart_authorization
                .as_ref()
                .map(|authorization| StoredRestartAuthorizationV1 {
                    pre_head_sequence: authorization.pre_head_sequence,
                    turn_ids: authorization
                        .turn_ids
                        .iter()
                        .map(|id| id.0.clone())
                        .collect(),
                });
        session_plan_digest(
            session_id,
            &self.transaction_id,
            self.command_record.as_ref(),
            authorization.as_ref(),
            self.command_only_authorization.as_ref(),
            &events,
        )
    }
}

/// 冻结在 control WAL 中、只含 primitive 的 Session redo plan。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredSessionPlanV1 {
    schema_version: u32,
    session_id: String,
    expected_project_id: String,
    expected_pre_sequence: u64,
    expected_pre_batch_checksum: Option<String>,
    transaction_id: String,
    command_record: Option<StoredCommandRecordV1>,
    restart_authorization: Option<StoredRestartAuthorizationV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command_only_authorization: Option<StoredCommandOnlyAuthorizationV1>,
    events: Vec<StoredSessionEventV1>,
    canonical_plan_digest: String,
}

impl StoredSessionPlanV1 {
    pub(crate) fn from_append_request(
        session_id: &SessionId,
        expected_project_id: &ProjectId,
        expected_pre_sequence: u64,
        expected_pre_batch_checksum: Option<String>,
        request: &SessionAppendRequest,
    ) -> Result<Self, StateStoreError> {
        if let Some(checksum) = &expected_pre_batch_checksum {
            validate_sha256(checksum)?;
        }
        let restart_authorization = request.restart_authorization.as_ref().map(|authorization| {
            StoredRestartAuthorizationV1 {
                pre_head_sequence: authorization.pre_head_sequence,
                turn_ids: authorization
                    .turn_ids
                    .iter()
                    .map(|id| id.0.clone())
                    .collect(),
            }
        });
        let plan = Self {
            schema_version: 1,
            session_id: session_id.0.clone(),
            expected_project_id: expected_project_id.0.clone(),
            expected_pre_sequence,
            expected_pre_batch_checksum,
            transaction_id: request.transaction_id.clone(),
            command_record: request.command_record.clone(),
            restart_authorization,
            command_only_authorization: request.command_only_authorization.clone(),
            events: request
                .events
                .iter()
                .map(StoredSessionEventV1::from_live)
                .collect(),
            canonical_plan_digest: request.canonical_plan_digest(session_id)?,
        };
        plan.validate_shape()?;
        Ok(plan)
    }

    pub(crate) fn session_id(&self) -> Result<SessionId, StateStoreError> {
        Ok(SessionId(validate_id(self.session_id.clone())?))
    }

    pub(crate) fn expected_project_id(&self) -> Result<ProjectId, StateStoreError> {
        Ok(ProjectId(validate_id(self.expected_project_id.clone())?))
    }

    pub(crate) const fn expected_pre_sequence(&self) -> u64 {
        self.expected_pre_sequence
    }

    pub(crate) fn expected_pre_batch_checksum(&self) -> Option<&str> {
        self.expected_pre_batch_checksum.as_deref()
    }

    pub(crate) fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    pub(crate) fn canonical_plan_digest(&self) -> &str {
        &self.canonical_plan_digest
    }

    pub(crate) fn command_record(&self) -> Option<&StoredCommandRecordV1> {
        self.command_record.as_ref()
    }

    /// 判断该值是否为结构有效的 control-coordinated restart plan。凡使用预留内部前缀但不
    /// 精确匹配 restart identity 的输入都会被拒绝，不会被归类为 external command。
    pub(crate) fn validate_coordinated_restart_identity(&self) -> Result<bool, StateStoreError> {
        let Some(command) = &self.command_record else {
            return Ok(false);
        };
        if !command.client_id.starts_with(INTERNAL_CLIENT_PREFIX) {
            return Ok(false);
        }
        if command.client_id != INTERNAL_RESTART_CLIENT_ID {
            return Err(StateStoreError::ProjectionRejected);
        }

        let intent = &command.client_command_id;
        let suffix = format!(":{}:{}", self.session_id, self.expected_pre_sequence);
        let instance_id = intent
            .strip_prefix("restart-v1:")
            .and_then(|rest| rest.strip_suffix(&suffix))
            .filter(|instance_id| !instance_id.is_empty())
            .ok_or(StateStoreError::ProjectionRejected)?;
        validate_id(instance_id.to_owned())?;
        if self
            .restart_authorization
            .as_ref()
            .is_some_and(|authorization| {
                authorization.pre_head_sequence != self.expected_pre_sequence
            })
        {
            return Err(StateStoreError::ProjectionRejected);
        }
        let payload_digest = restart_payload_digest_from_stored(intent, &self.events)?;
        if command.payload_digest != payload_digest
            || self.transaction_id != format!("{}:session", restart_global_tx_id(&payload_digest)?)
        {
            return Err(StateStoreError::ProjectionRejected);
        }

        let reply_bytes = command.decode_reply()?;
        let reply: CommandReply = serde_json::from_slice(&reply_bytes)
            .map_err(|_| StateStoreError::IncompatibleSchema)?;
        let CommandOutcome::Success {
            result: CommandResult::SessionSnapshot(snapshot),
        } = reply.outcome
        else {
            return Err(StateStoreError::ProjectionRejected);
        };
        let event_count =
            u64::try_from(self.events.len()).map_err(|_| StateStoreError::BatchTooLarge)?;
        let expected_post_sequence = self
            .expected_pre_sequence
            .checked_add(event_count)
            .ok_or(StateStoreError::SequenceMismatch)?;
        if snapshot.session_id.0 != self.session_id
            || snapshot.project_id.0 != self.expected_project_id
            || snapshot.covered_through_sequence != expected_post_sequence
        {
            return Err(StateStoreError::ProjectionRejected);
        }
        Ok(true)
    }

    pub(crate) fn validate(&self) -> Result<(), StateStoreError> {
        self.validate_shape()
    }

    pub(crate) fn into_append_request(self) -> Result<SessionAppendRequest, StateStoreError> {
        self.validate_shape()?;
        let session_id = SessionId(validate_id(self.session_id)?);
        let restart_authorization = self
            .restart_authorization
            .map(|stored| {
                let turn_ids = stored
                    .turn_ids
                    .into_iter()
                    .map(validate_id)
                    .map(|result| result.map(TurnId))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, StateStoreError>(RestartAuthorizationV1 {
                    pre_head_sequence: stored.pre_head_sequence,
                    turn_ids,
                })
            })
            .transpose()?;
        let request = SessionAppendRequest {
            transaction_id: self.transaction_id,
            command_record: self.command_record,
            events: self
                .events
                .into_iter()
                .map(StoredSessionEventV1::into_live)
                .collect::<Result<Vec<_>, _>>()?,
            restart_authorization,
            command_only_authorization: self.command_only_authorization,
        };
        if request.canonical_plan_digest(&session_id)? != self.canonical_plan_digest {
            return Err(StateStoreError::ChecksumMismatch);
        }
        Ok(request)
    }

    fn validate_shape(&self) -> Result<(), StateStoreError> {
        if self.schema_version != 1
            || self.transaction_id.is_empty()
            || self.events.is_empty() != self.command_only_authorization.is_some()
            || (self.command_only_authorization.is_some() && self.restart_authorization.is_some())
        {
            return Err(StateStoreError::IncompatibleSchema);
        }
        validate_id(self.session_id.clone())?;
        validate_id(self.expected_project_id.clone())?;
        if let Some(checksum) = &self.expected_pre_batch_checksum {
            validate_sha256(checksum)?;
        } else if self.expected_pre_sequence != 0 {
            return Err(StateStoreError::IncompatibleSchema);
        }
        validate_sha256(&self.canonical_plan_digest)?;
        let request = SessionAppendRequest {
            transaction_id: self.transaction_id.clone(),
            command_record: self.command_record.clone(),
            events: self
                .events
                .iter()
                .cloned()
                .map(StoredSessionEventV1::into_live)
                .collect::<Result<Vec<_>, _>>()?,
            restart_authorization: self
                .restart_authorization
                .as_ref()
                .map(|stored| {
                    Ok::<_, StateStoreError>(RestartAuthorizationV1 {
                        pre_head_sequence: stored.pre_head_sequence,
                        turn_ids: stored
                            .turn_ids
                            .iter()
                            .cloned()
                            .map(validate_id)
                            .map(|result| result.map(TurnId))
                            .collect::<Result<Vec<_>, _>>()?,
                    })
                })
                .transpose()?,
            command_only_authorization: self.command_only_authorization.clone(),
        };
        if request.canonical_plan_digest(&SessionId(self.session_id.clone()))?
            != self.canonical_plan_digest
        {
            return Err(StateStoreError::ChecksumMismatch);
        }
        validate_command_only_shape(
            &SessionId(self.session_id.clone()),
            self.expected_pre_sequence,
            self.expected_pre_batch_checksum.as_deref(),
            self.command_record.as_ref(),
            self.restart_authorization.as_ref(),
            &self.events,
            self.command_only_authorization.as_ref(),
        )?;
        self.validate_coordinated_restart_identity()?;
        Ok(())
    }
}

fn validate_command_only_shape(
    session_id: &SessionId,
    pre_sequence: u64,
    pre_checksum: Option<&str>,
    command_record: Option<&StoredCommandRecordV1>,
    restart_authorization: Option<&StoredRestartAuthorizationV1>,
    events: &[StoredSessionEventV1],
    authorization: Option<&StoredCommandOnlyAuthorizationV1>,
) -> Result<(), StateStoreError> {
    let Some(authorization) = authorization else {
        return if events.is_empty() {
            Err(StateStoreError::IncompatibleSchema)
        } else {
            Ok(())
        };
    };
    let command_record = command_record.ok_or(StateStoreError::IncompatibleSchema)?;
    if !events.is_empty()
        || restart_authorization.is_some()
        || pre_sequence == 0
        || pre_checksum.is_none()
        || authorization.schema_version != 1
        || authorization.protocol_version != PROTOCOL_VERSION
        || command_record.client_id.starts_with(INTERNAL_CLIENT_PREFIX)
        || external_command_payload_digest(authorization.protocol_version, &authorization.command)
            .map_err(|_| StateStoreError::IncompatibleSchema)?
            != command_record.payload_digest
    {
        return Err(StateStoreError::ProjectionRejected);
    }
    let (_raw, reply) = command_record.decode_reply_for_protocol(PROTOCOL_VERSION)?;
    let shape_matches = match (authorization.reason, &authorization.command, &reply.outcome) {
        (
            CommandOnlyReasonV1::TurnAlreadyTerminal,
            ClientCommand::TurnCancel {
                session_id: requested,
                turn_id,
            },
            CommandOutcome::Success {
                result:
                    CommandResult::TurnAlreadyTerminal {
                        turn_id: reply_turn_id,
                        ..
                    },
            },
        ) => requested == session_id && reply_turn_id == turn_id,
        (
            CommandOnlyReasonV1::QuestionAlreadyResolved,
            ClientCommand::QuestionRespond {
                session_id: requested,
                question_id,
                ..
            },
            CommandOutcome::Success {
                result: CommandResult::QuestionAlreadyResolved(question),
            },
        ) => {
            requested == session_id
                && question.question_id == *question_id
                && question.session_id == *requested
        }
        (
            CommandOnlyReasonV1::ApprovalAlreadyResolved,
            ClientCommand::ApprovalRespond {
                session_id: requested,
                approval_id,
                ..
            },
            CommandOutcome::Success {
                result: CommandResult::ApprovalAlreadyResolved(approval),
            },
        ) => {
            requested == session_id
                && approval.approval_id == *approval_id
                && approval.session_id == *requested
        }
        (
            CommandOnlyReasonV1::TurnOwnershipMismatch,
            ClientCommand::TurnCancel {
                session_id: requested,
                turn_id,
            },
            _,
        ) => {
            reply
                == command_only_ownership_reply(
                    command_record,
                    ProtocolErrorCode::TurnOwnershipMismatch,
                    format!(
                        "turn `{}` does not belong to session `{}`",
                        turn_id.0, requested.0
                    ),
                )?
        }
        (
            CommandOnlyReasonV1::QuestionOwnershipMismatch,
            ClientCommand::QuestionRespond {
                session_id: requested,
                ..
            },
            _,
        ) => {
            reply
                == command_only_ownership_reply(
                    command_record,
                    ProtocolErrorCode::QuestionOwnershipMismatch,
                    format!("question does not belong to session `{}`", requested.0),
                )?
        }
        (
            CommandOnlyReasonV1::ApprovalOwnershipMismatch,
            ClientCommand::ApprovalRespond {
                session_id: requested,
                ..
            },
            _,
        ) => {
            reply
                == command_only_ownership_reply(
                    command_record,
                    ProtocolErrorCode::ApprovalOwnershipMismatch,
                    format!("approval does not belong to session `{}`", requested.0),
                )?
        }
        _ => false,
    };
    if !shape_matches {
        return Err(StateStoreError::ProjectionRejected);
    }
    Ok(())
}

fn command_only_ownership_reply(
    command_record: &StoredCommandRecordV1,
    code: ProtocolErrorCode,
    message: String,
) -> Result<CommandReply, StateStoreError> {
    Ok(CommandReply::error(
        ClientCommandId(validate_id(command_record.client_command_id.clone())?),
        code,
        message,
    ))
}

struct SessionWriterLease {
    inner: Arc<super::StoreInner>,
    key: String,
}

impl SessionWriterLease {
    fn matches(&self, session_id: &SessionId) -> bool {
        self.key == session_key(session_id)
    }
}

impl Drop for SessionWriterLease {
    fn drop(&mut self) {
        self.inner
            .session_registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.key);
    }
}

pub(crate) enum OpenSessionWriter {
    Ready(ReadySessionWriter),
    RepairRequired(RepairRequiredSessionWriter),
}

pub(crate) struct ReadySessionWriter {
    lease: SessionWriterLease,
    session_dir: OwnedFd,
    file: File,
    state: RecoveredSessionState,
    catalog_context: SessionAllocationCatalogContext,
    #[cfg(test)]
    failpoint: Option<AppendFailpoint>,
    #[cfg(test)]
    checkpoint_failpoint: Option<CheckpointFailpoint>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SessionCursorError {
    UnsupportedStreamKind,
    SessionMismatch,
    EpochMismatch {
        expected_epoch: u64,
        actual_epoch: u64,
        head_sequence: u64,
    },
    Future {
        head_sequence: u64,
    },
}

pub(crate) enum SessionAppendFailure {
    Rejected {
        writer: ReadySessionWriter,
        error: StateStoreError,
    },
    Poisoned {
        writer: PoisonedSessionWriter,
        error: StateStoreError,
    },
}

pub(crate) struct PoisonedSessionWriter {
    lease: SessionWriterLease,
    session_dir: OwnedFd,
    session_id: SessionId,
    catalog_context: SessionAllocationCatalogContext,
    #[cfg(test)]
    recovery_failpoint: Option<RecoveryFailpoint>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecoveryFailpoint {
    FileSync,
}

pub(crate) enum SessionRecoveryOutcome {
    Ready(ReadySessionWriter),
    RepairRequired(RepairRequiredSessionWriter),
    Corrupt(CorruptSessionWriter),
}

pub(crate) struct RepairRequiredSessionWriter {
    lease: SessionWriterLease,
    session_dir: OwnedFd,
    session_id: SessionId,
    valid_bytes: u64,
    damaged_bytes: u64,
    tail_digest: String,
    catalog_context: SessionAllocationCatalogContext,
    #[cfg(test)]
    repair_failpoint: Option<RepairFailpoint>,
}

pub(crate) struct CorruptSessionWriter {
    _lease: SessionWriterLease,
    _session_dir: OwnedFd,
    _session_id: SessionId,
}

impl StateStore {
    #[cfg(test)]
    pub(crate) fn open_session_writer(
        &self,
        session_id: SessionId,
    ) -> Result<OpenSessionWriter, StateStoreError> {
        self.open_session_writer_with_catalog(
            session_id,
            SessionAllocationCatalogContext::for_test([]),
        )
    }

    pub(crate) fn open_session_writer_with_catalog(
        &self,
        session_id: SessionId,
        catalog_context: SessionAllocationCatalogContext,
    ) -> Result<OpenSessionWriter, StateStoreError> {
        validate_id(session_id.0.clone())?;
        let key = session_key(&session_id);
        #[cfg(test)]
        self.record_writer_open(format!("session:{key}"));
        {
            let mut registry = self
                .inner
                .session_registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !registry.insert(key.clone()) {
                return Err(StateStoreError::WriterLockRequired);
            }
        }
        let lease = SessionWriterLease {
            inner: Arc::clone(&self.inner),
            key: key.clone(),
        };
        match open_session_writer_with_lease(
            lease,
            session_id,
            &key,
            self.init_failpoint,
            catalog_context,
        ) {
            Ok(writer) => Ok(writer),
            Err((lease, error)) => {
                drop(lease);
                Err(error)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn list_sessions(&self) -> Result<SessionCatalog, StateStoreError> {
        self.list_sessions_with_catalog(&SessionAllocationCatalogContext::for_test([]))
    }

    pub(crate) fn list_sessions_with_catalog(
        &self,
        catalog_context: &SessionAllocationCatalogContext,
    ) -> Result<SessionCatalog, StateStoreError> {
        let mut directory = Dir::read_from(&self.inner.sessions)
            .map_err(|source| io_error("list sessions", source))?;
        let mut catalog = SessionCatalog::default();
        for entry in &mut directory {
            let entry = entry.map_err(|source| io_error("read sessions entry", source))?;
            let name = entry.file_name();
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            let key = canonical_session_directory_name(name)?;
            if catalog.sessions.len() >= MAX_SESSIONS {
                return Err(StateStoreError::BatchTooLarge);
            }
            let session_dir = open_session_directory(&self.inner.sessions, &key)?;
            let mut file = open_rollout_read(&session_dir)?;
            let expected = discover_session_id(&mut file)?;
            if session_key(&expected) != key {
                return Err(StateStoreError::StreamMismatch);
            }
            let state = match load_session_checkpoint(
                &session_dir,
                &mut file,
                &expected,
                &self.instance_id,
                catalog_context,
            ) {
                Ok(Some(state)) => {
                    match scan_session_log_from(&mut file, state, catalog_context)? {
                        SessionScanOutcome::Clean(state) => state,
                        SessionScanOutcome::Incomplete {
                            state,
                            damaged_bytes,
                            ..
                        } => {
                            return Err(StateStoreError::RecoverableIncompleteTail {
                                valid_bytes: state.valid_bytes,
                                damaged_bytes,
                            });
                        }
                    }
                }
                Ok(None) | Err(_) => {
                    match scan_session_log(
                        &mut file,
                        &expected,
                        &self.instance_id,
                        catalog_context,
                    )? {
                        SessionScanOutcome::Clean(state) => state,
                        SessionScanOutcome::Incomplete {
                            state,
                            damaged_bytes,
                            ..
                        } => {
                            return Err(StateStoreError::RecoverableIncompleteTail {
                                valid_bytes: state.valid_bytes,
                                damaged_bytes,
                            });
                        }
                    }
                }
            };
            catalog.insert(&state)?;
        }
        Ok(catalog)
    }

    /// 仅比较受管目录名，不打开或重放 aggregate 日志。
    pub(crate) fn validate_session_directory_catalog(
        &self,
        expected_session_ids: &BTreeSet<String>,
        strict: bool,
    ) -> Result<(), StateStoreError> {
        let expected = expected_session_ids
            .iter()
            .map(|session_id| validate_id(session_id.clone()).map(|id| session_key(&SessionId(id))))
            .collect::<Result<BTreeSet<_>, _>>()?;
        let mut actual = BTreeSet::new();
        let mut directory = Dir::read_from(&self.inner.sessions)
            .map_err(|source| io_error("list Session directory keys", source))?;
        for entry in &mut directory {
            let entry = entry.map_err(|source| io_error("read Session directory key", source))?;
            let name = entry.file_name();
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            if actual.len() >= MAX_SESSIONS
                || !actual.insert(canonical_session_directory_name(name)?)
            {
                return Err(StateStoreError::ProjectionRejected);
            }
        }
        if (strict && actual != expected) || (!strict && !actual.is_subset(&expected)) {
            return Err(StateStoreError::StreamMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SessionCatalog {
    pub sessions: BTreeMap<String, SessionSnapshot>,
    pub turn_owners: BTreeMap<String, String>,
    pub question_owners: BTreeMap<String, String>,
    pub approval_owners: BTreeMap<String, String>,
}

impl SessionCatalog {
    fn insert(&mut self, state: &RecoveredSessionState) -> Result<(), StateStoreError> {
        let snapshot = state.projection.snapshot()?;
        let session_id = snapshot.session_id.0.clone();
        if self
            .sessions
            .insert(session_id.clone(), snapshot.clone())
            .is_some()
        {
            return Err(StateStoreError::ProjectionRejected);
        }
        for turn in snapshot.turns {
            insert_unique_owner(&mut self.turn_owners, turn.turn_id.0, &session_id)?;
        }
        for question in snapshot.questions {
            insert_unique_owner(
                &mut self.question_owners,
                question.question_id.0,
                &session_id,
            )?;
        }
        for approval in snapshot.approvals {
            insert_unique_owner(
                &mut self.approval_owners,
                approval.approval_id.0,
                &session_id,
            )?;
        }
        Ok(())
    }
}

fn insert_unique_owner(
    owners: &mut BTreeMap<String, String>,
    id: String,
    session_id: &str,
) -> Result<(), StateStoreError> {
    if owners.insert(id, session_id.to_owned()).is_some() {
        return Err(StateStoreError::ProjectionRejected);
    }
    Ok(())
}

impl ReadySessionWriter {
    #[cfg(test)]
    pub(crate) fn set_failpoint(&mut self, failpoint: AppendFailpoint) {
        self.failpoint = Some(failpoint);
    }

    #[cfg(test)]
    pub(crate) fn set_checkpoint_failpoint(&mut self, failpoint: CheckpointFailpoint) {
        self.checkpoint_failpoint = Some(failpoint);
    }

    pub(crate) fn projection(&self) -> &SessionRolloutProjection {
        &self.state.projection
    }

    pub(crate) fn head(&self) -> (u64, Option<&str>) {
        (
            self.state.last_sequence,
            self.state.last_checksum.as_deref(),
        )
    }

    pub(crate) fn snapshot(&self) -> Result<SessionSnapshot, StateStoreError> {
        self.state.projection.snapshot()
    }

    pub(crate) fn published_read_state(
        &self,
    ) -> Result<PublishedSessionReadState, StateStoreError> {
        PublishedSessionReadState::from_recovered(&self.state)
    }

    pub(crate) fn probe_transaction(
        &self,
        transaction_id: &str,
        canonical_plan_digest: &str,
    ) -> TransactionProbe {
        probe_transaction_index(
            &self.state.transactions,
            transaction_id,
            canonical_plan_digest,
        )
    }

    pub(crate) fn transaction_index(&self) -> &BTreeMap<String, TransactionCommit> {
        &self.state.transactions
    }

    pub(crate) fn page(&self, after_sequence: u64) -> Result<EventPage, StateStoreError> {
        if after_sequence > self.state.last_sequence {
            return Err(StateStoreError::SequenceMismatch);
        }
        let events = self
            .state
            .events
            .iter()
            .enumerate()
            .skip(usize::try_from(after_sequence).map_err(|_| StateStoreError::SequenceMismatch)?)
            .take(crate::protocol::EVENT_PAGE_LIMIT)
            .map(|(index, event)| {
                Ok(event_to_wire(
                    u64::try_from(index).map_err(|_| StateStoreError::SequenceMismatch)? + 1,
                    event,
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let next_after_sequence = events.last().map_or(after_sequence, |event| event.sequence);
        Ok(EventPage {
            stream_kind: StreamKind::SessionRollout,
            stream_id: self.state.expected_session_id.0.clone(),
            epoch: SESSION_STREAM_EPOCH,
            head_sequence: self.state.last_sequence,
            events,
            next_after_sequence,
        })
    }

    pub(crate) fn resume(
        &self,
        cursor: &crate::protocol::StreamCursor,
    ) -> Result<EventPage, SessionCursorError> {
        if cursor.stream_kind != StreamKind::SessionRollout {
            return Err(SessionCursorError::UnsupportedStreamKind);
        }
        if cursor.stream_id != self.state.expected_session_id.0 {
            return Err(SessionCursorError::SessionMismatch);
        }
        if cursor.epoch != SESSION_STREAM_EPOCH {
            return Err(SessionCursorError::EpochMismatch {
                expected_epoch: SESSION_STREAM_EPOCH,
                actual_epoch: cursor.epoch,
                head_sequence: self.state.last_sequence,
            });
        }
        self.page(cursor.after_sequence)
            .map_err(|_| SessionCursorError::Future {
                head_sequence: self.state.last_sequence,
            })
    }

    pub(crate) fn append(
        mut self,
        request: SessionAppendRequest,
    ) -> Result<(Self, AppendOutcome), SessionAppendFailure> {
        let prepared = match prepare_session_batch(&self.state, request, &self.catalog_context) {
            Ok(PreparedSessionAppend::Idempotent(outcome)) => return Ok((self, outcome)),
            Ok(PreparedSessionAppend::Batch(batch, next)) => (batch, next),
            Err(error) => {
                return Err(SessionAppendFailure::Rejected {
                    writer: self,
                    error,
                });
            }
        };
        let (batch, mut next_state) = prepared;
        let mut bytes = match serde_json::to_vec(&batch) {
            Ok(bytes) => bytes,
            Err(_) => {
                return Err(SessionAppendFailure::Rejected {
                    writer: self,
                    error: StateStoreError::IncompatibleSchema,
                });
            }
        };
        bytes.push(b'\n');
        if bytes.len() > MAX_LINE_BYTES {
            return Err(SessionAppendFailure::Rejected {
                writer: self,
                error: StateStoreError::BatchTooLarge,
            });
        }
        let line_len = match u64::try_from(bytes.len()) {
            Ok(length) => length,
            Err(_) => {
                return Err(SessionAppendFailure::Rejected {
                    writer: self,
                    error: StateStoreError::BatchTooLarge,
                });
            }
        };
        next_state.valid_bytes = match next_state.valid_bytes.checked_add(line_len) {
            Some(value) => value,
            None => {
                return Err(SessionAppendFailure::Rejected {
                    writer: self,
                    error: StateStoreError::BatchTooLarge,
                });
            }
        };
        #[cfg(test)]
        if self.failpoint == Some(AppendFailpoint::BeforeWrite) {
            return Err(SessionAppendFailure::Rejected {
                writer: self,
                error: io_error(
                    "test before Session write",
                    std::io::Error::other("injected"),
                ),
            });
        }
        #[cfg(test)]
        if let Some(AppendFailpoint::PartialWrite(count)) = self.failpoint {
            let limit = count.min(bytes.len());
            let _ignored = self.file.write_all(&bytes[..limit]);
            return Err(SessionAppendFailure::Poisoned {
                writer: self.into_poisoned(),
                error: io_error(
                    "test partial Session write",
                    std::io::Error::other("injected"),
                ),
            });
        }
        if let Err(source) = self.file.write_all(&bytes) {
            return Err(SessionAppendFailure::Poisoned {
                writer: self.into_poisoned(),
                error: io_error("append Session batch", source),
            });
        }
        #[cfg(test)]
        if self.failpoint == Some(AppendFailpoint::AfterNewlineBeforeSync) {
            return Err(SessionAppendFailure::Poisoned {
                writer: self.into_poisoned(),
                error: io_error(
                    "test Session after newline",
                    std::io::Error::other("injected"),
                ),
            });
        }
        if let Err(source) = self.file.flush() {
            return Err(SessionAppendFailure::Poisoned {
                writer: self.into_poisoned(),
                error: io_error("flush Session batch", source),
            });
        }
        #[cfg(test)]
        if self.failpoint == Some(AppendFailpoint::FileSyncError) {
            return Err(SessionAppendFailure::Poisoned {
                writer: self.into_poisoned(),
                error: io_error("test Session sync", std::io::Error::other("injected")),
            });
        }
        if let Err(source) = self.file.sync_all() {
            return Err(SessionAppendFailure::Poisoned {
                writer: self.into_poisoned(),
                error: io_error("sync Session batch", source),
            });
        }
        #[cfg(test)]
        if self.failpoint == Some(AppendFailpoint::AfterSyncBeforeUpdate) {
            return Err(SessionAppendFailure::Poisoned {
                writer: self.into_poisoned(),
                error: io_error("test Session after sync", std::io::Error::other("injected")),
            });
        }
        let stable_reply = match batch
            .command_record
            .as_ref()
            .map(StoredCommandRecordV1::decode_reply)
            .transpose()
        {
            Ok(reply) => reply,
            Err(error) => {
                return Err(SessionAppendFailure::Poisoned {
                    writer: self.into_poisoned(),
                    error,
                });
            }
        };
        let outcome = AppendOutcome {
            last_sequence: next_state.last_sequence,
            stable_reply,
            appended: true,
        };
        self.state = next_state;
        Ok((self, outcome))
    }

    pub(crate) fn write_checkpoint(&self) -> Result<(), StateStoreError> {
        let projection_digest = self.state.projection.canonical_digest()?;
        let projection = serde_json::to_value(&self.state.projection)
            .map_err(|_| StateStoreError::IncompatibleSchema)?;
        let mut command_index = self.state.commands.values().cloned().collect::<Vec<_>>();
        command_index.sort();
        let events = self
            .state
            .events
            .iter()
            .map(StoredSessionEventV1::from_live)
            .collect();
        let transaction_index = stored_transaction_index(&self.state.transactions);
        for command in &command_index {
            command.decode_reply()?;
        }
        let mut checkpoint = StoredSessionCheckpointV1 {
            schema_version: 1,
            projection_schema_version: 1,
            session_id: self.state.expected_session_id.0.clone(),
            stream_id: self.state.expected_session_id.0.clone(),
            epoch: SESSION_STREAM_EPOCH,
            covered_sequence: self.state.last_sequence,
            covered_batch_checksum: self.state.last_checksum.clone(),
            covered_valid_bytes: self.state.valid_bytes,
            projection_digest,
            projection,
            events,
            command_index,
            transaction_index,
            checksum: String::new(),
        };
        checkpoint.checksum = session_checkpoint_checksum(&checkpoint)?;
        let bytes =
            serde_json::to_vec(&checkpoint).map_err(|_| StateStoreError::IncompatibleSchema)?;
        if u64::try_from(bytes.len()).map_or(true, |len| len > MAX_CHECKPOINT_BYTES) {
            return Err(StateStoreError::BatchTooLarge);
        }
        let temp_name = format!("session-checkpoint-{}.tmp", random_hex_128());
        #[cfg(test)]
        if self.checkpoint_failpoint == Some(CheckpointFailpoint::TempCreate) {
            return Err(io_error(
                "test Session checkpoint create",
                std::io::Error::other("injected"),
            ));
        }
        let fd = openat(
            &self.session_dir,
            temp_name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            FILE_MODE,
        )
        .map_err(|source| io_error("create Session checkpoint temp", source))?;
        let mut file = File::from(fd);
        #[cfg(test)]
        if self.checkpoint_failpoint == Some(CheckpointFailpoint::TempWrite) {
            return Err(io_error(
                "test Session checkpoint write",
                std::io::Error::other("injected"),
            ));
        }
        file.write_all(&bytes)
            .map_err(|source| io_error("write Session checkpoint", source))?;
        #[cfg(test)]
        if self.checkpoint_failpoint == Some(CheckpointFailpoint::FileSync) {
            return Err(io_error(
                "test Session checkpoint sync",
                std::io::Error::other("injected"),
            ));
        }
        file.sync_all()
            .map_err(|source| io_error("sync Session checkpoint", source))?;
        #[cfg(test)]
        if self.checkpoint_failpoint == Some(CheckpointFailpoint::BeforeInstall) {
            return Err(io_error(
                "test Session checkpoint install",
                std::io::Error::other("injected"),
            ));
        }
        renameat(
            &self.session_dir,
            temp_name.as_str(),
            &self.session_dir,
            SESSION_CHECKPOINT_FILE,
        )
        .map_err(|source| io_error("install Session checkpoint", source))?;
        #[cfg(test)]
        if self.checkpoint_failpoint == Some(CheckpointFailpoint::AfterInstall) {
            return Err(io_error(
                "test Session checkpoint after install",
                std::io::Error::other("injected"),
            ));
        }
        #[cfg(test)]
        if self.checkpoint_failpoint == Some(CheckpointFailpoint::DirectorySyncError) {
            return Err(io_error(
                "test Session checkpoint directory sync",
                std::io::Error::other("injected"),
            ));
        }
        fsync(&self.session_dir)
            .map_err(|source| io_error("sync Session checkpoint directory", source))
    }

    fn into_poisoned(self) -> PoisonedSessionWriter {
        PoisonedSessionWriter {
            lease: self.lease,
            session_dir: self.session_dir,
            session_id: self.state.expected_session_id,
            catalog_context: self.catalog_context,
            #[cfg(test)]
            recovery_failpoint: None,
        }
    }
}

impl PoisonedSessionWriter {
    #[cfg(test)]
    pub(crate) fn set_recovery_failpoint(&mut self, failpoint: RecoveryFailpoint) {
        self.recovery_failpoint = Some(failpoint);
    }

    pub(crate) fn recover(self) -> SessionRecoveryOutcome {
        if !self.lease.matches(&self.session_id) {
            return SessionRecoveryOutcome::Corrupt(CorruptSessionWriter {
                _lease: self.lease,
                _session_dir: self.session_dir,
                _session_id: self.session_id,
            });
        }
        recover_session_with_lease(
            self.lease,
            self.session_dir,
            self.session_id,
            self.catalog_context,
            #[cfg(test)]
            self.recovery_failpoint,
        )
    }
}

impl RepairRequiredSessionWriter {
    #[cfg(test)]
    pub(crate) fn set_failpoint(&mut self, failpoint: RepairFailpoint) {
        self.repair_failpoint = Some(failpoint);
    }

    pub(crate) fn repair(self) -> Result<ReadySessionWriter, CorruptSessionWriter> {
        if !self.lease.matches(&self.session_id) {
            return Err(self.into_corrupt());
        }
        let mut file = match open_rollout(&self.session_dir) {
            Ok(file) => file,
            Err(_) => return Err(self.into_corrupt()),
        };
        #[cfg(test)]
        if self.repair_failpoint == Some(RepairFailpoint::RescanRace)
            && file
                .seek(SeekFrom::End(0))
                .and_then(|_| file.write_all(b"race"))
                .is_err()
        {
            return Err(self.into_corrupt());
        }
        let scan = match scan_session_log(
            &mut file,
            &self.session_id,
            &self.lease.inner.instance_id,
            &self.catalog_context,
        ) {
            Ok(SessionScanOutcome::Incomplete {
                state,
                damaged_bytes,
                tail_digest,
            }) if state.valid_bytes == self.valid_bytes
                && damaged_bytes == self.damaged_bytes
                && tail_digest == self.tail_digest =>
            {
                state
            }
            _ => return Err(self.into_corrupt()),
        };
        #[cfg(test)]
        if self.repair_failpoint == Some(RepairFailpoint::TruncateError) {
            return Err(self.into_corrupt());
        }
        if file.set_len(self.valid_bytes).is_err() {
            return Err(self.into_corrupt());
        }
        #[cfg(test)]
        if self.repair_failpoint == Some(RepairFailpoint::FileSyncError) {
            return Err(self.into_corrupt());
        }
        if file.sync_all().is_err() {
            return Err(self.into_corrupt());
        }
        #[cfg(test)]
        if self.repair_failpoint == Some(RepairFailpoint::DirectorySyncError) {
            return Err(self.into_corrupt());
        }
        if fsync(&self.session_dir).is_err() || file.seek(SeekFrom::End(0)).is_err() {
            return Err(self.into_corrupt());
        }
        Ok(ReadySessionWriter {
            lease: self.lease,
            session_dir: self.session_dir,
            file,
            state: scan,
            catalog_context: self.catalog_context,
            #[cfg(test)]
            failpoint: None,
            #[cfg(test)]
            checkpoint_failpoint: None,
        })
    }

    fn into_corrupt(self) -> CorruptSessionWriter {
        CorruptSessionWriter {
            _lease: self.lease,
            _session_dir: self.session_dir,
            _session_id: self.session_id,
        }
    }
}

enum PreparedSessionAppend {
    Idempotent(AppendOutcome),
    Batch(StoredSessionBatchV1, RecoveredSessionState),
}

fn prepare_session_batch(
    state: &RecoveredSessionState,
    request: SessionAppendRequest,
    catalog_context: &SessionAllocationCatalogContext,
) -> Result<PreparedSessionAppend, StateStoreError> {
    if request.events.len() > MAX_EVENTS {
        return Err(StateStoreError::BatchTooLarge);
    }
    if request.events.is_empty() && request.command_record.is_none() {
        return Err(StateStoreError::BatchTooLarge);
    }
    if request.events.is_empty() && state.projection.session_id.is_none() {
        return Err(StateStoreError::StreamMismatch);
    }
    let stored_events = request
        .events
        .iter()
        .map(StoredSessionEventV1::from_live)
        .collect::<Vec<_>>();
    let stored_authorization =
        request
            .restart_authorization
            .as_ref()
            .map(|authorization| StoredRestartAuthorizationV1 {
                pre_head_sequence: authorization.pre_head_sequence,
                turn_ids: authorization
                    .turn_ids
                    .iter()
                    .map(|id| id.0.clone())
                    .collect(),
            });
    let canonical_plan_digest = session_plan_digest(
        &state.expected_session_id,
        &request.transaction_id,
        request.command_record.as_ref(),
        stored_authorization.as_ref(),
        request.command_only_authorization.as_ref(),
        &stored_events,
    )?;
    match probe_transaction_index(
        &state.transactions,
        &request.transaction_id,
        &canonical_plan_digest,
    ) {
        TransactionProbe::Absent => {}
        TransactionProbe::SamePlanCommitted(committed) => {
            return Ok(PreparedSessionAppend::Idempotent(AppendOutcome {
                last_sequence: committed.resulting_last_sequence,
                stable_reply: request
                    .command_record
                    .as_ref()
                    .map(StoredCommandRecordV1::decode_reply)
                    .transpose()?,
                appended: false,
            }));
        }
        TransactionProbe::ConflictingPlan => return Err(StateStoreError::IdempotencyConflict),
    }
    if let Some(command) = &request.command_record {
        let key = (command.client_id.clone(), command.client_command_id.clone());
        if let Some(existing) = state.commands.get(&key) {
            if existing.payload_digest != command.payload_digest {
                return Err(StateStoreError::IdempotencyConflict);
            }
            return Ok(PreparedSessionAppend::Idempotent(AppendOutcome {
                last_sequence: state.last_sequence,
                stable_reply: Some(existing.decode_reply()?),
                appended: false,
            }));
        }
    }
    let first_sequence = state
        .last_sequence
        .checked_add(1)
        .ok_or(StateStoreError::SequenceMismatch)?;
    let event_count =
        u64::try_from(request.events.len()).map_err(|_| StateStoreError::BatchTooLarge)?;
    let last_sequence = if event_count == 0 {
        state.last_sequence
    } else {
        first_sequence
            .checked_add(event_count - 1)
            .ok_or(StateStoreError::SequenceMismatch)?
    };
    let mut batch = StoredSessionBatchV1 {
        schema_version: 1,
        session_id: state.expected_session_id.0.clone(),
        stream_id: state.expected_session_id.0.clone(),
        epoch: SESSION_STREAM_EPOCH,
        transaction_id: request.transaction_id,
        event_count,
        first_sequence,
        last_sequence,
        command_record: request.command_record,
        restart_authorization: stored_authorization,
        command_only_authorization: request.command_only_authorization,
        events: stored_events,
        previous_batch_checksum: state.last_checksum.clone(),
        batch_checksum: String::new(),
    };
    batch.batch_checksum = session_batch_checksum(&batch)?;
    let mut next = state.clone();
    apply_session_batch(&mut next, &batch, catalog_context)?;
    Ok(PreparedSessionAppend::Batch(batch, next))
}

enum SessionScanOutcome {
    Clean(RecoveredSessionState),
    Incomplete {
        state: RecoveredSessionState,
        damaged_bytes: u64,
        tail_digest: String,
    },
}

fn scan_session_log(
    file: &mut File,
    expected_session: &SessionId,
    state_instance_id: &str,
    catalog_context: &SessionAllocationCatalogContext,
) -> Result<SessionScanOutcome, StateStoreError> {
    scan_session_log_from(
        file,
        empty_session_state(expected_session.clone(), state_instance_id.to_owned()),
        catalog_context,
    )
}

fn scan_session_log_from(
    file: &mut File,
    mut state: RecoveredSessionState,
    catalog_context: &SessionAllocationCatalogContext,
) -> Result<SessionScanOutcome, StateStoreError> {
    let mut offset = state.valid_bytes;
    file.seek(SeekFrom::Start(offset))
        .map_err(|source| io_error("seek Session rollout", source))?;
    let mut reader = BufReader::new(file);
    loop {
        let mut line = Vec::new();
        let count = reader
            .by_ref()
            .take(u64::try_from(MAX_LINE_BYTES).expect("line limit fits u64") + 1)
            .read_until(b'\n', &mut line)
            .map_err(|source| io_error("read Session rollout", source))?;
        if count == 0 {
            state.valid_bytes = offset;
            return Ok(SessionScanOutcome::Clean(state));
        }
        if count > MAX_LINE_BYTES {
            return Err(StateStoreError::BatchTooLarge);
        }
        if !line.ends_with(b"\n") {
            let damaged_bytes =
                u64::try_from(line.len()).map_err(|_| StateStoreError::BatchTooLarge)?;
            let tail_digest = format!("sha256:{:x}", Sha256::digest(&line));
            state.valid_bytes = offset;
            return Ok(SessionScanOutcome::Incomplete {
                state,
                damaged_bytes,
                tail_digest,
            });
        }
        line.pop();
        let batch: StoredSessionBatchV1 =
            serde_json::from_slice(&line).map_err(|_| StateStoreError::MiddleCorruption)?;
        apply_session_batch(&mut state, &batch, catalog_context)?;
        offset = offset
            .checked_add(u64::try_from(count).map_err(|_| StateStoreError::BatchTooLarge)?)
            .ok_or(StateStoreError::BatchTooLarge)?;
        state.valid_bytes = offset;
    }
}

fn apply_session_batch(
    state: &mut RecoveredSessionState,
    batch: &StoredSessionBatchV1,
    catalog_context: &SessionAllocationCatalogContext,
) -> Result<(), StateStoreError> {
    if batch.schema_version != 1 || batch.epoch != SESSION_STREAM_EPOCH {
        return Err(StateStoreError::IncompatibleSchema);
    }
    validate_id(batch.session_id.clone())?;
    if batch.session_id != state.expected_session_id.0 || batch.stream_id != batch.session_id {
        return Err(StateStoreError::StreamMismatch);
    }
    let event_count =
        u64::try_from(batch.events.len()).map_err(|_| StateStoreError::BatchTooLarge)?;
    if event_count != batch.event_count || batch.events.len() > MAX_EVENTS {
        return Err(StateStoreError::SequenceMismatch);
    }
    let expected_first = state
        .last_sequence
        .checked_add(1)
        .ok_or(StateStoreError::SequenceMismatch)?;
    let expected_last = if event_count == 0 {
        state.last_sequence
    } else {
        expected_first
            .checked_add(event_count - 1)
            .ok_or(StateStoreError::SequenceMismatch)?
    };
    if batch.first_sequence != expected_first || batch.last_sequence != expected_last {
        return Err(StateStoreError::SequenceMismatch);
    }
    if event_count == 0 && batch.command_record.is_none() {
        return Err(StateStoreError::SequenceMismatch);
    }
    if event_count == 0 && state.projection.session_id.is_none() {
        return Err(StateStoreError::StreamMismatch);
    }
    if batch.transaction_id.is_empty() || state.transactions.contains_key(&batch.transaction_id) {
        return Err(StateStoreError::SequenceMismatch);
    }
    if batch.previous_batch_checksum != state.last_checksum {
        return Err(StateStoreError::ChecksumChainMismatch);
    }
    if batch.batch_checksum != session_batch_checksum(batch)? {
        return Err(StateStoreError::ChecksumMismatch);
    }

    validate_command_only_shape(
        &state.expected_session_id,
        state.last_sequence,
        state.last_checksum.as_deref(),
        batch.command_record.as_ref(),
        batch.restart_authorization.as_ref(),
        &batch.events,
        batch.command_only_authorization.as_ref(),
    )?;
    validate_command_only_authorization(state, batch, catalog_context)?;

    let live_events = batch
        .events
        .iter()
        .cloned()
        .map(StoredSessionEventV1::into_live)
        .collect::<Result<Vec<_>, _>>()?;
    let authorization = validate_restart_authorization(state, batch)?;
    validate_restart_plan_batch(state, batch, &live_events, authorization.as_ref())?;
    let mut projection = state.projection.clone();
    if let Some(authorization) = &authorization {
        projection.authorize_restart(authorization)?;
    }
    for (index, event) in live_events.iter().enumerate() {
        let sequence = batch
            .first_sequence
            .checked_add(u64::try_from(index).map_err(|_| StateStoreError::BatchTooLarge)?)
            .ok_or(StateStoreError::SequenceMismatch)?;
        projection.apply(sequence, event)?;
    }
    let aborted_turns = live_events
        .iter()
        .filter_map(|event| match event {
            SessionRolloutEvent::TurnCompleted {
                turn_id,
                status: TurnStatus::AbortedByRestart,
            } => Some(turn_id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let authorized_turns = authorization
        .as_ref()
        .map_or_else(Vec::new, |value| value.turn_ids.clone());
    if aborted_turns != authorized_turns {
        return Err(StateStoreError::ProjectionRejected);
    }
    if projection
        .session_id
        .as_ref()
        .is_some_and(|id| id != &state.expected_session_id)
    {
        return Err(StateStoreError::StreamMismatch);
    }
    if let Some(authorization) = authorization.as_ref()
        && state
            .restart_authorizations
            .contains_key(&authorization.pre_head_sequence)
    {
        return Err(StateStoreError::ProjectionRejected);
    }
    if let Some(command) = &batch.command_record {
        command.decode_reply()?;
        let key = (command.client_id.clone(), command.client_command_id.clone());
        if state.commands.contains_key(&key) {
            return Err(StateStoreError::IdempotencyConflict);
        }
        state.commands.insert(key, command.clone());
    }
    state.events.extend(live_events);
    if let Some(authorization) = authorization {
        state
            .restart_authorizations
            .insert(authorization.pre_head_sequence, authorization);
    }
    state.batch_heads.push(PublishedSessionBatchHead {
        last_sequence: expected_last,
        checksum: batch.batch_checksum.clone(),
    });
    state.projection = projection;
    state.last_sequence = expected_last;
    state.last_checksum = Some(batch.batch_checksum.clone());
    let plan_digest = session_plan_digest(
        &state.expected_session_id,
        &batch.transaction_id,
        batch.command_record.as_ref(),
        batch.restart_authorization.as_ref(),
        batch.command_only_authorization.as_ref(),
        &batch.events,
    )?;
    state.transactions.insert(
        batch.transaction_id.clone(),
        TransactionCommit {
            canonical_plan_digest: plan_digest,
            resulting_last_sequence: batch.last_sequence,
            resulting_batch_checksum: batch.batch_checksum.clone(),
        },
    );
    Ok(())
}

fn validate_command_only_authorization(
    state: &RecoveredSessionState,
    batch: &StoredSessionBatchV1,
    catalog_context: &SessionAllocationCatalogContext,
) -> Result<(), StateStoreError> {
    let Some(authorization) = &batch.command_only_authorization else {
        return Ok(());
    };
    let command_record = batch
        .command_record
        .as_ref()
        .ok_or(StateStoreError::ProjectionRejected)?;
    let actual_session = state
        .projection
        .session_id
        .as_ref()
        .ok_or(StateStoreError::ProjectionRejected)?;
    let actual_project = state
        .projection
        .project_id
        .as_ref()
        .ok_or(StateStoreError::ProjectionRejected)?;
    if actual_session != &state.expected_session_id
        || catalog_context.project_id(actual_session) != Some(actual_project.0.as_str())
    {
        return Err(StateStoreError::ProjectionRejected);
    }

    let expected = match (authorization.reason, &authorization.command) {
        (
            CommandOnlyReasonV1::TurnAlreadyTerminal,
            ClientCommand::TurnCancel {
                session_id,
                turn_id,
            },
        ) if session_id == actual_session => {
            let turn = state
                .projection
                .turns
                .get(&turn_id.0)
                .ok_or(StateStoreError::ProjectionRejected)?;
            if !turn.snapshot.status.is_terminal() {
                return Err(StateStoreError::ProjectionRejected);
            }
            CommandReply::success(
                ClientCommandId(command_record.client_command_id.clone()),
                CommandResult::TurnAlreadyTerminal {
                    turn_id: turn_id.clone(),
                    terminal_status: turn.snapshot.status,
                    terminal_sequence: turn
                        .snapshot
                        .terminal_sequence
                        .ok_or(StateStoreError::ProjectionRejected)?,
                },
            )
        }
        (
            CommandOnlyReasonV1::QuestionAlreadyResolved,
            ClientCommand::QuestionRespond {
                session_id,
                question_id,
                choice_id,
            },
        ) if session_id == actual_session => {
            let question = state
                .projection
                .questions
                .get(&question_id.0)
                .ok_or(StateStoreError::ProjectionRejected)?;
            if question.status != QuestionStatus::Answered
                || question.session_id != *actual_session
                || !question
                    .choices
                    .iter()
                    .any(|choice| choice.choice_id == *choice_id)
            {
                return Err(StateStoreError::ProjectionRejected);
            }
            CommandReply::success(
                ClientCommandId(command_record.client_command_id.clone()),
                CommandResult::QuestionAlreadyResolved(question.clone()),
            )
        }
        (
            CommandOnlyReasonV1::ApprovalAlreadyResolved,
            ClientCommand::ApprovalRespond {
                session_id,
                approval_id,
                approval_subject_digest,
                ..
            },
        ) if session_id == actual_session => {
            let approval = state
                .projection
                .approvals
                .get(&approval_id.0)
                .ok_or(StateStoreError::ProjectionRejected)?;
            if matches!(
                approval.status,
                ApprovalStatus::Pending | ApprovalStatus::OwnerTurnAborted
            ) || approval.session_id != *actual_session
                || approval.approval_subject_digest != *approval_subject_digest
            {
                return Err(StateStoreError::ProjectionRejected);
            }
            CommandReply::success(
                ClientCommandId(command_record.client_command_id.clone()),
                CommandResult::ApprovalAlreadyResolved(approval.clone()),
            )
        }
        (
            CommandOnlyReasonV1::TurnOwnershipMismatch,
            ClientCommand::TurnCancel {
                session_id,
                turn_id,
            },
        ) => {
            validate_mismatched_requested_session(catalog_context, actual_session, session_id)?;
            if !state.projection.turns.contains_key(&turn_id.0) {
                return Err(StateStoreError::ProjectionRejected);
            }
            command_only_ownership_reply(
                command_record,
                ProtocolErrorCode::TurnOwnershipMismatch,
                format!(
                    "turn `{}` does not belong to session `{}`",
                    turn_id.0, session_id.0
                ),
            )?
        }
        (
            CommandOnlyReasonV1::QuestionOwnershipMismatch,
            ClientCommand::QuestionRespond {
                session_id,
                question_id,
                choice_id,
            },
        ) => {
            validate_mismatched_requested_session(catalog_context, actual_session, session_id)?;
            let question = state
                .projection
                .questions
                .get(&question_id.0)
                .ok_or(StateStoreError::ProjectionRejected)?;
            if question.session_id != *actual_session
                || !question
                    .choices
                    .iter()
                    .any(|choice| choice.choice_id == *choice_id)
            {
                return Err(StateStoreError::ProjectionRejected);
            }
            command_only_ownership_reply(
                command_record,
                ProtocolErrorCode::QuestionOwnershipMismatch,
                format!("question does not belong to session `{}`", session_id.0),
            )?
        }
        (
            CommandOnlyReasonV1::ApprovalOwnershipMismatch,
            ClientCommand::ApprovalRespond {
                session_id,
                approval_id,
                approval_subject_digest,
                ..
            },
        ) => {
            validate_mismatched_requested_session(catalog_context, actual_session, session_id)?;
            let approval = state
                .projection
                .approvals
                .get(&approval_id.0)
                .ok_or(StateStoreError::ProjectionRejected)?;
            if approval.session_id != *actual_session
                || approval.approval_subject_digest != *approval_subject_digest
            {
                return Err(StateStoreError::ProjectionRejected);
            }
            command_only_ownership_reply(
                command_record,
                ProtocolErrorCode::ApprovalOwnershipMismatch,
                format!("approval does not belong to session `{}`", session_id.0),
            )?
        }
        _ => return Err(StateStoreError::ProjectionRejected),
    };
    let expected_raw =
        serde_json::to_vec(&expected).map_err(|_| StateStoreError::IncompatibleSchema)?;
    if command_record.decode_reply()? != expected_raw {
        return Err(StateStoreError::ProjectionRejected);
    }
    Ok(())
}

fn validate_mismatched_requested_session(
    catalog_context: &SessionAllocationCatalogContext,
    actual_session: &SessionId,
    requested_session: &SessionId,
) -> Result<(), StateStoreError> {
    if requested_session == actual_session
        || catalog_context.project_id(requested_session).is_none()
    {
        return Err(StateStoreError::ProjectionRejected);
    }
    Ok(())
}

fn session_plan_digest(
    session_id: &SessionId,
    transaction_id: &str,
    command_record: Option<&StoredCommandRecordV1>,
    restart_authorization: Option<&StoredRestartAuthorizationV1>,
    command_only_authorization: Option<&StoredCommandOnlyAuthorizationV1>,
    events: &[StoredSessionEventV1],
) -> Result<String, StateStoreError> {
    if transaction_id.is_empty() {
        return Err(StateStoreError::SequenceMismatch);
    }
    let canonical = if let Some(authorization) = command_only_authorization {
        serde_json::to_vec(&(
            "alda-session-plan-v1",
            &session_id.0,
            transaction_id,
            command_record,
            restart_authorization,
            authorization,
            events,
        ))
    } else {
        serde_json::to_vec(&(
            "alda-session-plan-v1",
            &session_id.0,
            transaction_id,
            command_record,
            restart_authorization,
            events,
        ))
    }
    .map_err(|_| StateStoreError::IncompatibleSchema)?;
    Ok(format!("sha256:{:x}", Sha256::digest(canonical)))
}

fn session_batch_checksum(batch: &StoredSessionBatchV1) -> Result<String, StateStoreError> {
    let canonical = if let Some(authorization) = &batch.command_only_authorization {
        serde_json::to_vec(&(
            "alda-session-batch-v1",
            batch.schema_version,
            &batch.session_id,
            &batch.stream_id,
            batch.epoch,
            &batch.transaction_id,
            batch.event_count,
            batch.first_sequence,
            batch.last_sequence,
            &batch.command_record,
            &batch.restart_authorization,
            authorization,
            &batch.events,
            &batch.previous_batch_checksum,
        ))
    } else {
        serde_json::to_vec(&(
            "alda-session-batch-v1",
            batch.schema_version,
            &batch.session_id,
            &batch.stream_id,
            batch.epoch,
            &batch.transaction_id,
            batch.event_count,
            batch.first_sequence,
            batch.last_sequence,
            &batch.command_record,
            &batch.restart_authorization,
            &batch.events,
            &batch.previous_batch_checksum,
        ))
    }
    .map_err(|_| StateStoreError::IncompatibleSchema)?;
    Ok(format!("sha256:{:x}", Sha256::digest(canonical)))
}

fn session_checkpoint_checksum(
    checkpoint: &StoredSessionCheckpointV1,
) -> Result<String, StateStoreError> {
    let canonical = serde_json::to_vec(&(
        "alda-session-checkpoint-v1",
        checkpoint.schema_version,
        checkpoint.projection_schema_version,
        &checkpoint.session_id,
        &checkpoint.stream_id,
        checkpoint.epoch,
        checkpoint.covered_sequence,
        &checkpoint.covered_batch_checksum,
        checkpoint.covered_valid_bytes,
        &checkpoint.projection_digest,
        &checkpoint.projection,
        &checkpoint.events,
        &checkpoint.command_index,
        &checkpoint.transaction_index,
    ))
    .map_err(|_| StateStoreError::IncompatibleSchema)?;
    Ok(format!("sha256:{:x}", Sha256::digest(canonical)))
}

fn load_session_checkpoint(
    session_dir: &OwnedFd,
    rollout_file: &mut File,
    expected_session: &SessionId,
    state_instance_id: &str,
    catalog_context: &SessionAllocationCatalogContext,
) -> Result<Option<RecoveredSessionState>, StateStoreError> {
    let fd = match openat(
        session_dir,
        SESSION_CHECKPOINT_FILE,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(_) => return Ok(None),
    };
    let file = File::from(fd);
    if validate_regular_file(&file, Some(MAX_CHECKPOINT_BYTES)).is_err() {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.take(MAX_CHECKPOINT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| io_error("read Session checkpoint", source))?;
    if u64::try_from(bytes.len()).map_or(true, |length| length > MAX_CHECKPOINT_BYTES) {
        return Ok(None);
    }
    let Ok(checkpoint) = serde_json::from_slice::<StoredSessionCheckpointV1>(&bytes) else {
        return Ok(None);
    };
    if checkpoint.schema_version != 1
        || checkpoint.projection_schema_version != 1
        || checkpoint.session_id != expected_session.0
        || checkpoint.stream_id != expected_session.0
        || checkpoint.epoch != SESSION_STREAM_EPOCH
        || checkpoint.checksum != session_checkpoint_checksum(&checkpoint)?
    {
        return Ok(None);
    }
    let checkpoint_events = checkpoint
        .events
        .iter()
        .cloned()
        .map(StoredSessionEventV1::into_live)
        .collect::<Result<Vec<_>, _>>()?;
    if checkpoint.covered_sequence
        != u64::try_from(checkpoint_events.len()).map_err(|_| StateStoreError::BatchTooLarge)?
    {
        return Ok(None);
    }
    let mut checkpoint_commands = BTreeMap::new();
    for command in &checkpoint.command_index {
        command.decode_reply()?;
        let key = (command.client_id.clone(), command.client_command_id.clone());
        if checkpoint_commands.insert(key, command.clone()).is_some() {
            return Ok(None);
        }
    }
    let mut checkpoint_transactions = BTreeMap::new();
    for stored in &checkpoint.transaction_index {
        if stored.transaction_id.is_empty()
            || validate_sha256(&stored.canonical_plan_digest).is_err()
            || validate_sha256(&stored.resulting_batch_checksum).is_err()
            || stored.resulting_last_sequence > checkpoint.covered_sequence
            || checkpoint_transactions
                .insert(
                    stored.transaction_id.clone(),
                    TransactionCommit {
                        canonical_plan_digest: stored.canonical_plan_digest.clone(),
                        resulting_last_sequence: stored.resulting_last_sequence,
                        resulting_batch_checksum: stored.resulting_batch_checksum.clone(),
                    },
                )
                .is_some()
        {
            return Ok(None);
        }
    }

    let mut anchored = empty_session_state(expected_session.clone(), state_instance_id.to_owned());
    loop {
        if anchored.valid_bytes == checkpoint.covered_valid_bytes {
            break;
        }
        if anchored.valid_bytes > checkpoint.covered_valid_bytes {
            return Ok(None);
        }
        rollout_file
            .seek(SeekFrom::Start(anchored.valid_bytes))
            .map_err(|source| io_error("seek Session checkpoint anchor", source))?;
        let mut reader = BufReader::new(&mut *rollout_file);
        let mut line = Vec::new();
        let count = reader
            .by_ref()
            .take(u64::try_from(MAX_LINE_BYTES).expect("line limit fits u64") + 1)
            .read_until(b'\n', &mut line)
            .map_err(|source| io_error("read Session checkpoint anchor", source))?;
        if count == 0 || count > MAX_LINE_BYTES || !line.ends_with(b"\n") {
            return Ok(None);
        }
        line.pop();
        let Ok(batch) = serde_json::from_slice::<StoredSessionBatchV1>(&line) else {
            return Ok(None);
        };
        if apply_session_batch(&mut anchored, &batch, catalog_context).is_err() {
            return Ok(None);
        }
        anchored.valid_bytes = anchored
            .valid_bytes
            .checked_add(u64::try_from(count).map_err(|_| StateStoreError::BatchTooLarge)?)
            .ok_or(StateStoreError::BatchTooLarge)?;
    }
    if anchored.last_sequence != checkpoint.covered_sequence
        || anchored.last_checksum != checkpoint.covered_batch_checksum
        || anchored.commands != checkpoint_commands
        || anchored.transactions != checkpoint_transactions
        || anchored.events != checkpoint_events
        || checkpoint.projection_digest != anchored.projection.canonical_digest()?
        || checkpoint.projection
            != serde_json::to_value(&anchored.projection)
                .map_err(|_| StateStoreError::IncompatibleSchema)?
    {
        return Ok(None);
    }
    Ok(Some(anchored))
}

fn open_session_writer_with_lease(
    lease: SessionWriterLease,
    session_id: SessionId,
    key: &str,
    init_failpoint: Option<InitFailpoint>,
    catalog_context: SessionAllocationCatalogContext,
) -> Result<OpenSessionWriter, (SessionWriterLease, StateStoreError)> {
    let session_dir = match ensure_directory(
        &lease.inner.sessions,
        key,
        DirectoryKind::Session,
        init_failpoint,
    ) {
        Ok(directory) => directory,
        Err(error) => return Err((lease, error)),
    };
    let mut file = match open_or_create_rollout(&session_dir, init_failpoint) {
        Ok(file) => file,
        Err(error) => return Err((lease, error)),
    };
    let scan = match load_session_checkpoint(
        &session_dir,
        &mut file,
        &session_id,
        &lease.inner.instance_id,
        &catalog_context,
    ) {
        Ok(Some(state)) => {
            #[cfg(test)]
            SESSION_CHECKPOINT_LOAD_OBSERVED.set(true);
            scan_session_log_from(&mut file, state, &catalog_context)
        }
        Ok(None) | Err(_) => scan_session_log(
            &mut file,
            &session_id,
            &lease.inner.instance_id,
            &catalog_context,
        ),
    };
    match scan {
        Ok(SessionScanOutcome::Clean(state)) => {
            if let Err(source) = file.seek(SeekFrom::End(0)) {
                return Err((lease, io_error("seek Session rollout end", source)));
            }
            Ok(OpenSessionWriter::Ready(ReadySessionWriter {
                lease,
                session_dir,
                file,
                state,
                catalog_context,
                #[cfg(test)]
                failpoint: None,
                #[cfg(test)]
                checkpoint_failpoint: None,
            }))
        }
        Ok(SessionScanOutcome::Incomplete {
            state,
            damaged_bytes,
            tail_digest,
        }) => Ok(OpenSessionWriter::RepairRequired(
            RepairRequiredSessionWriter {
                lease,
                session_dir,
                session_id,
                valid_bytes: state.valid_bytes,
                damaged_bytes,
                tail_digest,
                catalog_context,
                #[cfg(test)]
                repair_failpoint: None,
            },
        )),
        Err(error) => Err((lease, error)),
    }
}

fn recover_session_with_lease(
    lease: SessionWriterLease,
    session_dir: OwnedFd,
    session_id: SessionId,
    catalog_context: SessionAllocationCatalogContext,
    #[cfg(test)] recovery_failpoint: Option<RecoveryFailpoint>,
) -> SessionRecoveryOutcome {
    let mut file = match open_rollout(&session_dir) {
        Ok(file) => file,
        Err(_) => {
            return SessionRecoveryOutcome::Corrupt(CorruptSessionWriter {
                _lease: lease,
                _session_dir: session_dir,
                _session_id: session_id,
            });
        }
    };
    let scan = match load_session_checkpoint(
        &session_dir,
        &mut file,
        &session_id,
        &lease.inner.instance_id,
        &catalog_context,
    ) {
        Ok(Some(state)) => scan_session_log_from(&mut file, state, &catalog_context),
        Ok(None) | Err(_) => scan_session_log(
            &mut file,
            &session_id,
            &lease.inner.instance_id,
            &catalog_context,
        ),
    };
    match scan {
        Ok(SessionScanOutcome::Clean(state))
            if {
                #[cfg(test)]
                let sync_allowed = recovery_failpoint != Some(RecoveryFailpoint::FileSync);
                #[cfg(not(test))]
                let sync_allowed = true;
                sync_allowed && file.sync_all().is_ok() && file.seek(SeekFrom::End(0)).is_ok()
            } =>
        {
            SessionRecoveryOutcome::Ready(ReadySessionWriter {
                lease,
                session_dir,
                file,
                state,
                catalog_context,
                #[cfg(test)]
                failpoint: None,
                #[cfg(test)]
                checkpoint_failpoint: None,
            })
        }
        Ok(SessionScanOutcome::Incomplete {
            state,
            damaged_bytes,
            tail_digest,
        }) => SessionRecoveryOutcome::RepairRequired(RepairRequiredSessionWriter {
            lease,
            session_dir,
            session_id,
            valid_bytes: state.valid_bytes,
            damaged_bytes,
            tail_digest,
            catalog_context,
            #[cfg(test)]
            repair_failpoint: None,
        }),
        _ => SessionRecoveryOutcome::Corrupt(CorruptSessionWriter {
            _lease: lease,
            _session_dir: session_dir,
            _session_id: session_id,
        }),
    }
}

fn empty_session_state(session_id: SessionId, state_instance_id: String) -> RecoveredSessionState {
    RecoveredSessionState {
        state_instance_id,
        expected_session_id: session_id,
        last_sequence: 0,
        last_checksum: None,
        projection: SessionRolloutProjection::default(),
        events: Vec::new(),
        restart_authorizations: BTreeMap::new(),
        batch_heads: Vec::new(),
        commands: BTreeMap::new(),
        transactions: BTreeMap::new(),
        valid_bytes: 0,
    }
}

fn session_key(session_id: &SessionId) -> String {
    format!("{:x}", Sha256::digest(session_id.0.as_bytes()))
}

fn canonical_session_directory_name(name: &CStr) -> Result<String, StateStoreError> {
    let bytes = name.to_bytes();
    if bytes.len() != 64
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        return Err(StateStoreError::UnsafeRoot);
    }
    str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| StateStoreError::UnsafeRoot)
}

fn open_session_directory(parent: &OwnedFd, name: &str) -> Result<OwnedFd, StateStoreError> {
    let fd = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|source| io_error("open Session directory", source))?;
    validate_directory(&fd, false)?;
    Ok(fd)
}

fn open_or_create_rollout(
    session_dir: &OwnedFd,
    failpoint: Option<InitFailpoint>,
) -> Result<File, StateStoreError> {
    // B3b 与 B3a 共享 durable file-create stage；Session 特有的目录 stage 已在上方单独表示。
    super::inject_init(
        failpoint,
        InitFailpoint::RolloutCreate,
        "test Session rollout create",
    )?;
    let fd = openat(
        session_dir,
        ROLLOUT_FILE,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        FILE_MODE,
    )
    .map_err(|source| io_error("open Session rollout", source))?;
    let file = File::from(fd);
    validate_regular_file(&file, None)?;
    super::inject_init(
        failpoint,
        InitFailpoint::RolloutFileSync,
        "test Session rollout file sync",
    )?;
    file.sync_all()
        .map_err(|source| io_error("sync Session rollout", source))?;
    super::inject_init(
        failpoint,
        InitFailpoint::RolloutDirectorySync,
        "test Session rollout directory sync",
    )?;
    fsync(session_dir).map_err(|source| io_error("sync Session directory", source))?;
    Ok(file)
}

fn open_rollout(session_dir: &OwnedFd) -> Result<File, StateStoreError> {
    let fd = openat(
        session_dir,
        ROLLOUT_FILE,
        OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|source| io_error("reopen Session rollout", source))?;
    let file = File::from(fd);
    validate_regular_file(&file, None)?;
    Ok(file)
}

fn open_rollout_read(session_dir: &OwnedFd) -> Result<File, StateStoreError> {
    let fd = openat(
        session_dir,
        ROLLOUT_FILE,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|source| io_error("read Session rollout", source))?;
    let file = File::from(fd);
    validate_regular_file(&file, None)?;
    Ok(file)
}

fn discover_session_id(file: &mut File) -> Result<SessionId, StateStoreError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|source| io_error("seek Session first batch", source))?;
    let mut line = Vec::new();
    let count = BufReader::new(&mut *file)
        .take(u64::try_from(MAX_LINE_BYTES).expect("line limit fits u64") + 1)
        .read_until(b'\n', &mut line)
        .map_err(|source| io_error("read Session first batch", source))?;
    if count == 0 || count > MAX_LINE_BYTES || !line.ends_with(b"\n") {
        return Err(StateStoreError::MiddleCorruption);
    }
    line.pop();
    let batch: StoredSessionBatchV1 =
        serde_json::from_slice(&line).map_err(|_| StateStoreError::MiddleCorruption)?;
    let id = SessionId(validate_id(batch.session_id)?);
    file.seek(SeekFrom::Start(0))
        .map_err(|source| io_error("rewind Session rollout", source))?;
    Ok(id)
}

fn event_to_wire(sequence: u64, event: &SessionRolloutEvent) -> SessionEvent {
    let event = match event {
        SessionRolloutEvent::SessionStarted {
            session_id,
            project_id,
        } => SessionEventKind::SessionStarted {
            session_id: session_id.clone(),
            project_id: project_id.clone(),
        },
        SessionRolloutEvent::TurnStarted { turn_id, .. } => SessionEventKind::TurnStarted {
            turn_id: turn_id.clone(),
        },
        SessionRolloutEvent::TurnCancelRequested { turn_id } => {
            SessionEventKind::TurnCancelRequested {
                turn_id: turn_id.clone(),
            }
        }
        SessionRolloutEvent::TurnCompleted { turn_id, status } => SessionEventKind::TurnCompleted {
            turn_id: turn_id.clone(),
            status: *status,
        },
        SessionRolloutEvent::TurnBudgetExceeded { turn_id } => SessionEventKind::TurnCompleted {
            turn_id: turn_id.clone(),
            status: TurnStatus::BudgetExceeded,
        },
        SessionRolloutEvent::QuestionRequested {
            question_id,
            session_id,
            owner_turn_id,
            prompt,
            choices,
        } => SessionEventKind::QuestionRequested {
            question: PendingQuestion {
                question_id: question_id.clone(),
                session_id: session_id.clone(),
                owner_turn_id: owner_turn_id.clone(),
                prompt: prompt.clone(),
                choices: choices.clone(),
                status: QuestionStatus::Pending,
                created_sequence: sequence,
                terminal_sequence: None,
                answer: None,
                responder_client_id: None,
            },
        },
        SessionRolloutEvent::QuestionResolved {
            question_id,
            choice_id,
            responder_client_id,
        } => SessionEventKind::QuestionResolved {
            question_id: question_id.clone(),
            choice_id: choice_id.clone(),
            responder_client_id: responder_client_id.clone(),
        },
        SessionRolloutEvent::ApprovalRequested {
            approval_id,
            session_id,
            owner_turn_id,
            payload,
            subject_inputs: _,
            approval_subject_digest,
        } => SessionEventKind::ApprovalRequested {
            approval: PendingApproval {
                approval_id: approval_id.clone(),
                session_id: session_id.clone(),
                owner_turn_id: owner_turn_id.clone(),
                payload: payload.clone(),
                approval_subject_digest: approval_subject_digest.clone(),
                status: ApprovalStatus::Pending,
                created_sequence: sequence,
                terminal_sequence: None,
                decision: None,
                responder_client_id: None,
            },
        },
        SessionRolloutEvent::ApprovalResolved {
            approval_id,
            approval_subject_digest,
            decision,
            responder_client_id,
        } => SessionEventKind::ApprovalResolved {
            approval_id: approval_id.clone(),
            approval_subject_digest: approval_subject_digest.clone(),
            decision: *decision,
            responder_client_id: responder_client_id.clone(),
        },
        SessionRolloutEvent::QuestionOwnerTurnAborted {
            question_id,
            owner_turn_id,
            owner_terminal_status,
        } => SessionEventKind::QuestionOwnerTurnAborted {
            question_id: question_id.clone(),
            owner_turn_id: owner_turn_id.clone(),
            owner_terminal_status: *owner_terminal_status,
        },
        SessionRolloutEvent::ApprovalOwnerTurnAborted {
            approval_id,
            owner_turn_id,
            owner_terminal_status,
        } => SessionEventKind::ApprovalOwnerTurnAborted {
            approval_id: approval_id.clone(),
            owner_turn_id: owner_turn_id.clone(),
            owner_terminal_status: *owner_terminal_status,
        },
    };
    SessionEvent { sequence, event }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::PathBuf;

    use crate::protocol::{
        ClientCommandId, CommandReply, ProtocolErrorCode, QuestionStatus, StreamCursor, TurnStatus,
    };

    use super::*;
    use crate::state_store::StateStoreInstanceLease;

    fn make_private(root: &tempfile::TempDir) {
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))
            .expect("private test root");
    }

    fn open_store(root: &tempfile::TempDir) -> StateStore {
        StateStore::open(root.path(), StateStoreInstanceLease::for_tests()).expect("open state")
    }

    fn empty_catalog() -> SessionAllocationCatalogContext {
        SessionAllocationCatalogContext::for_test([])
    }

    fn command_only_catalog(
        actual_session: &SessionId,
        requested_session: &SessionId,
    ) -> SessionAllocationCatalogContext {
        SessionAllocationCatalogContext::for_test([
            (actual_session.clone(), ProjectId("project-1".to_owned())),
            (
                requested_session.clone(),
                ProjectId("project-other".to_owned()),
            ),
        ])
    }

    fn session(value: &str) -> SessionId {
        SessionId(value.to_owned())
    }

    fn ready(store: &StateStore, session_id: &SessionId) -> ReadySessionWriter {
        match store
            .open_session_writer(session_id.clone())
            .expect("open Session writer")
        {
            OpenSessionWriter::Ready(writer) => writer,
            OpenSessionWriter::RepairRequired(_) => panic!("unexpected incomplete tail"),
        }
    }

    fn ready_with_catalog(
        store: &StateStore,
        session_id: &SessionId,
        catalog: SessionAllocationCatalogContext,
    ) -> ReadySessionWriter {
        match store
            .open_session_writer_with_catalog(session_id.clone(), catalog)
            .expect("open Session writer with catalog")
        {
            OpenSessionWriter::Ready(writer) => writer,
            OpenSessionWriter::RepairRequired(_) => panic!("unexpected incomplete tail"),
        }
    }

    fn reply(command_id: &str) -> Vec<u8> {
        serde_json::to_vec(&CommandReply::error(
            ClientCommandId(command_id.to_owned()),
            ProtocolErrorCode::InvalidRequest,
            "stable",
        ))
        .expect("canonical reply")
    }

    fn command(command_id: &str, digest_byte: char) -> StoredCommandRecordV1 {
        StoredCommandRecordV1::new(
            "client",
            command_id,
            format!("sha256:{}", digest_byte.to_string().repeat(64)),
            &reply(command_id),
        )
        .expect("command record")
    }

    fn command_only_request(
        command_id: &str,
        command: ClientCommand,
        reason: CommandOnlyReasonV1,
        reply: CommandReply,
    ) -> SessionAppendRequest {
        let digest =
            external_command_payload_digest(PROTOCOL_VERSION, &command).expect("command digest");
        let raw = serde_json::to_vec(&reply).expect("canonical reply");
        drop(reply);
        let record =
            StoredCommandRecordV1::new("client", command_id, digest, &raw).expect("command record");
        SessionAppendRequest::new_command_only(
            format!("tx-{command_id}"),
            record,
            StoredCommandOnlyAuthorizationV1::new(command, reason),
        )
    }

    fn subject_inputs() -> ApprovalSubjectInputsV1 {
        ApprovalSubjectInputsV1::canonical("fake-provider-fixture-v1", ["prompt"])
            .expect("canonical subject inputs")
    }

    fn digest(turn: &str) -> ApprovalSubjectDigest {
        subject_inputs().digest(&TurnId(turn.to_owned()), "write a calm motif")
    }

    fn payload() -> ApprovalPayload {
        ApprovalPayload {
            action: "generate".to_owned(),
            effect: crate::protocol::EffectClass::ModelEgress,
            target: "provider".to_owned(),
            scope: "prompt".to_owned(),
            estimated_impact: "one request".to_owned(),
        }
    }

    fn choice() -> QuestionChoice {
        QuestionChoice {
            choice_id: ChoiceId("bars_8".to_owned()),
            label: "8 bars".to_owned(),
        }
    }

    fn prefix(session_id: &SessionId, turn: &str) -> Vec<SessionRolloutEvent> {
        vec![
            SessionRolloutEvent::SessionStarted {
                session_id: session_id.clone(),
                project_id: ProjectId("project-1".to_owned()),
            },
            SessionRolloutEvent::TurnStarted {
                turn_id: TurnId(turn.to_owned()),
                canonical_prompt: "write a calm motif".to_owned(),
            },
        ]
    }

    fn through_question(session_id: &SessionId, turn: &str) -> Vec<SessionRolloutEvent> {
        let mut events = prefix(session_id, turn);
        events.push(SessionRolloutEvent::QuestionRequested {
            question_id: QuestionId(format!("question-{turn}")),
            session_id: session_id.clone(),
            owner_turn_id: TurnId(turn.to_owned()),
            prompt: "How long?".to_owned(),
            choices: vec![choice()],
        });
        events
    }

    fn through_approval(session_id: &SessionId, turn: &str) -> Vec<SessionRolloutEvent> {
        let mut events = through_question(session_id, turn);
        events.extend([
            SessionRolloutEvent::QuestionResolved {
                question_id: QuestionId(format!("question-{turn}")),
                choice_id: ChoiceId("bars_8".to_owned()),
                responder_client_id: ClientId("client".to_owned()),
            },
            SessionRolloutEvent::ApprovalRequested {
                approval_id: ApprovalId(format!("approval-{turn}")),
                session_id: session_id.clone(),
                owner_turn_id: TurnId(turn.to_owned()),
                payload: payload(),
                subject_inputs: subject_inputs(),
                approval_subject_digest: digest(turn),
            },
        ]);
        events
    }

    fn finished_vector(
        session_id: &SessionId,
        turn: &str,
        decision: ApprovalDecision,
    ) -> Vec<SessionRolloutEvent> {
        let mut events = through_approval(session_id, turn);
        events.extend([
            SessionRolloutEvent::ApprovalResolved {
                approval_id: ApprovalId(format!("approval-{turn}")),
                approval_subject_digest: digest(turn),
                decision,
                responder_client_id: ClientId("client".to_owned()),
            },
            SessionRolloutEvent::TurnCompleted {
                turn_id: TurnId(turn.to_owned()),
                status: if decision == ApprovalDecision::Approve {
                    TurnStatus::Succeeded
                } else {
                    TurnStatus::Failed
                },
            },
        ]);
        events
    }

    fn replay(
        expected: &SessionId,
        events: &[SessionRolloutEvent],
    ) -> Result<SessionRolloutProjection, StateStoreError> {
        let mut projection = SessionRolloutProjection::default();
        for (index, event) in events.iter().enumerate() {
            projection.apply(u64::try_from(index).expect("index") + 1, event)?;
        }
        if projection.session_id.as_ref() != Some(expected) {
            return Err(StateStoreError::StreamMismatch);
        }
        Ok(projection)
    }

    fn assert_matches_online_reducer(
        events: &[SessionRolloutEvent],
        projection: &SessionRolloutProjection,
    ) {
        let wire = events
            .iter()
            .enumerate()
            .map(|(index, event)| event_to_wire(u64::try_from(index).expect("index") + 1, event))
            .collect::<Vec<_>>();
        let (turns, questions, approvals) =
            crate::app_service::replay_session_events_for_test(&wire);
        let snapshot = projection.snapshot().expect("snapshot");
        assert_eq!(snapshot.turns, turns);
        assert_eq!(snapshot.questions, questions);
        assert_eq!(snapshot.approvals, approvals);
    }

    fn rollout_path(root: &tempfile::TempDir, session_id: &SessionId) -> PathBuf {
        root.path()
            .join(super::super::STATE_LAYOUT)
            .join("sessions")
            .join(session_key(session_id))
            .join(ROLLOUT_FILE)
    }

    fn checkpoint_path(root: &tempfile::TempDir, session_id: &SessionId) -> PathBuf {
        root.path()
            .join(super::super::STATE_LAYOUT)
            .join("sessions")
            .join(session_key(session_id))
            .join(SESSION_CHECKPOINT_FILE)
    }

    fn append(
        writer: ReadySessionWriter,
        transaction_id: &str,
        command_record: Option<StoredCommandRecordV1>,
        events: Vec<SessionRolloutEvent>,
    ) -> ReadySessionWriter {
        match writer.append(SessionAppendRequest {
            transaction_id: transaction_id.to_owned(),
            command_record,
            events,
            restart_authorization: None,
            command_only_authorization: None,
        }) {
            Ok((writer, _)) => writer,
            Err(_) => panic!("append should succeed"),
        }
    }

    #[test]
    fn fixed_happy_deny_and_cancel_vectors_reduce_to_a2_snapshots() {
        let session_id = session("session-vector");
        for (decision, expected) in [
            (ApprovalDecision::Approve, TurnStatus::Succeeded),
            (ApprovalDecision::Deny, TurnStatus::Failed),
        ] {
            let events = finished_vector(&session_id, "turn-main", decision);
            let projection = replay(&session_id, &events).expect("valid fixed vector");
            let snapshot = projection.snapshot().expect("snapshot");
            assert_matches_online_reducer(&events, &projection);
            assert_eq!(snapshot.covered_through_sequence, 7);
            assert_eq!(snapshot.turns[0].status, expected);
            assert_eq!(snapshot.questions[0].created_sequence, 3);
            assert_eq!(snapshot.questions[0].terminal_sequence, Some(4));
            assert_eq!(snapshot.approvals[0].created_sequence, 5);
            assert_eq!(snapshot.approvals[0].terminal_sequence, Some(6));
            assert_eq!(
                snapshot.approvals[0].status,
                if decision == ApprovalDecision::Approve {
                    ApprovalStatus::Approved
                } else {
                    ApprovalStatus::Denied
                }
            );
        }

        let mut question_cancel = through_question(&session_id, "turn-q");
        question_cancel.extend([
            SessionRolloutEvent::TurnCancelRequested {
                turn_id: TurnId("turn-q".to_owned()),
            },
            SessionRolloutEvent::QuestionOwnerTurnAborted {
                question_id: QuestionId("question-turn-q".to_owned()),
                owner_turn_id: TurnId("turn-q".to_owned()),
                owner_terminal_status: TurnStatus::Cancelled,
            },
            SessionRolloutEvent::TurnCompleted {
                turn_id: TurnId("turn-q".to_owned()),
                status: TurnStatus::Cancelled,
            },
        ]);
        let projection = replay(&session_id, &question_cancel).expect("question cancel");
        assert_matches_online_reducer(&question_cancel, &projection);
        let snapshot = projection.snapshot().expect("snapshot");
        assert_eq!(snapshot.covered_through_sequence, 6);
        assert_eq!(snapshot.turns[0].status, TurnStatus::Cancelled);
        assert_eq!(
            snapshot.questions[0].status,
            QuestionStatus::OwnerTurnAborted
        );

        let mut approval_cancel = through_approval(&session_id, "turn-a");
        approval_cancel.extend([
            SessionRolloutEvent::TurnCancelRequested {
                turn_id: TurnId("turn-a".to_owned()),
            },
            SessionRolloutEvent::ApprovalOwnerTurnAborted {
                approval_id: ApprovalId("approval-turn-a".to_owned()),
                owner_turn_id: TurnId("turn-a".to_owned()),
                owner_terminal_status: TurnStatus::Cancelled,
            },
            SessionRolloutEvent::TurnCompleted {
                turn_id: TurnId("turn-a".to_owned()),
                status: TurnStatus::Cancelled,
            },
        ]);
        let projection = replay(&session_id, &approval_cancel).expect("approval cancel");
        assert_matches_online_reducer(&approval_cancel, &projection);
        let snapshot = projection.snapshot().expect("snapshot");
        assert_eq!(snapshot.covered_through_sequence, 8);
        assert_eq!(
            snapshot.approvals[0].status,
            ApprovalStatus::OwnerTurnAborted
        );
        assert_eq!(snapshot.approvals[0].terminal_sequence, Some(7));
    }

    #[test]
    fn stored_prompt_is_authoritative_but_never_leaks_to_wire() {
        let root = tempfile::tempdir().expect("tempdir");
        make_private(&root);
        let id = session("session-prompt");
        let store = open_store(&root);
        let writer = append(
            ready(&store, &id),
            "start",
            Some(command("start", '1')),
            through_question(&id, "turn-prompt"),
        );
        let before = writer
            .projection()
            .canonical_prompt(&TurnId("turn-prompt".to_owned()))
            .expect("stored prompt")
            .to_owned();
        let uninterrupted_digest = crate::app_service::approval_subject_digest_for_test(
            "fake-provider-fixture-v1",
            &["prompt"],
            &TurnId("turn-prompt".to_owned()),
            &before,
        );
        let page = writer.page(0).expect("page");
        assert_eq!(before, "write a calm motif");
        let wire = serde_json::to_string(&page).expect("wire");
        assert!(!wire.contains("write a calm motif"));
        drop(writer);
        drop(store);

        let reopened_store = open_store(&root);
        let reopened = ready(&reopened_store, &id);
        let after = reopened
            .projection()
            .canonical_prompt(&TurnId("turn-prompt".to_owned()))
            .expect("replayed prompt");
        assert_eq!(after, before);
        let restarted_digest = crate::app_service::approval_subject_digest_for_test(
            "fake-provider-fixture-v1",
            &["prompt"],
            &TurnId("turn-prompt".to_owned()),
            after,
        );
        assert_eq!(restarted_digest, uninterrupted_digest);
        let reopened = append(
            reopened,
            "answer-after-restart",
            None,
            vec![
                SessionRolloutEvent::QuestionResolved {
                    question_id: QuestionId("question-turn-prompt".to_owned()),
                    choice_id: ChoiceId("bars_8".to_owned()),
                    responder_client_id: ClientId("client".to_owned()),
                },
                SessionRolloutEvent::ApprovalRequested {
                    approval_id: ApprovalId("approval-turn-prompt".to_owned()),
                    session_id: id,
                    owner_turn_id: TurnId("turn-prompt".to_owned()),
                    payload: payload(),
                    subject_inputs: subject_inputs(),
                    approval_subject_digest: restarted_digest.clone(),
                },
            ],
        );
        assert_eq!(
            reopened.snapshot().expect("snapshot").approvals[0].approval_subject_digest,
            uninterrupted_digest
        );
    }

    #[test]
    fn command_only_authorization_accepts_all_six_reasons_at_exact_pre_head() {
        let actual = session("session-command-only-owner");
        let requested = session("session-command-only-requested");
        let catalog = command_only_catalog(&actual, &requested);
        let mut state = state_after(
            &actual,
            &finished_vector(&actual, "turn-command-only", ApprovalDecision::Approve),
        );
        let initial_sequence = state.last_sequence;
        let question = state
            .projection
            .questions
            .get("question-turn-command-only")
            .expect("question")
            .clone();
        let approval = state
            .projection
            .approvals
            .get("approval-turn-command-only")
            .expect("approval")
            .clone();
        let turn_id = TurnId("turn-command-only".to_owned());
        let question_id = QuestionId("question-turn-command-only".to_owned());
        let approval_id = ApprovalId("approval-turn-command-only".to_owned());
        let vectors = vec![
            command_only_request(
                "terminal",
                ClientCommand::TurnCancel {
                    session_id: actual.clone(),
                    turn_id: turn_id.clone(),
                },
                CommandOnlyReasonV1::TurnAlreadyTerminal,
                CommandReply::success(
                    ClientCommandId("terminal".to_owned()),
                    CommandResult::TurnAlreadyTerminal {
                        turn_id: turn_id.clone(),
                        terminal_status: TurnStatus::Succeeded,
                        terminal_sequence: 7,
                    },
                ),
            ),
            command_only_request(
                "question-resolved",
                ClientCommand::QuestionRespond {
                    session_id: actual.clone(),
                    question_id: question_id.clone(),
                    choice_id: ChoiceId("bars_8".to_owned()),
                },
                CommandOnlyReasonV1::QuestionAlreadyResolved,
                CommandReply::success(
                    ClientCommandId("question-resolved".to_owned()),
                    CommandResult::QuestionAlreadyResolved(question.clone()),
                ),
            ),
            command_only_request(
                "approval-resolved",
                ClientCommand::ApprovalRespond {
                    session_id: actual.clone(),
                    approval_id: approval_id.clone(),
                    approval_subject_digest: digest("turn-command-only"),
                    decision: ApprovalDecision::Deny,
                },
                CommandOnlyReasonV1::ApprovalAlreadyResolved,
                CommandReply::success(
                    ClientCommandId("approval-resolved".to_owned()),
                    CommandResult::ApprovalAlreadyResolved(approval.clone()),
                ),
            ),
            command_only_request(
                "turn-owner",
                ClientCommand::TurnCancel {
                    session_id: requested.clone(),
                    turn_id: turn_id.clone(),
                },
                CommandOnlyReasonV1::TurnOwnershipMismatch,
                CommandReply::error(
                    ClientCommandId("turn-owner".to_owned()),
                    ProtocolErrorCode::TurnOwnershipMismatch,
                    format!(
                        "turn `{}` does not belong to session `{}`",
                        turn_id.0, requested.0
                    ),
                ),
            ),
            command_only_request(
                "question-owner",
                ClientCommand::QuestionRespond {
                    session_id: requested.clone(),
                    question_id: question_id.clone(),
                    choice_id: ChoiceId("bars_8".to_owned()),
                },
                CommandOnlyReasonV1::QuestionOwnershipMismatch,
                CommandReply::error(
                    ClientCommandId("question-owner".to_owned()),
                    ProtocolErrorCode::QuestionOwnershipMismatch,
                    format!("question does not belong to session `{}`", requested.0),
                ),
            ),
            command_only_request(
                "approval-owner",
                ClientCommand::ApprovalRespond {
                    session_id: requested.clone(),
                    approval_id,
                    approval_subject_digest: digest("turn-command-only"),
                    decision: ApprovalDecision::Approve,
                },
                CommandOnlyReasonV1::ApprovalOwnershipMismatch,
                CommandReply::error(
                    ClientCommandId("approval-owner".to_owned()),
                    ProtocolErrorCode::ApprovalOwnershipMismatch,
                    format!("approval does not belong to session `{}`", requested.0),
                ),
            ),
        ];

        for request in vectors {
            let previous_checksum = state.last_checksum.clone();
            let PreparedSessionAppend::Batch(batch, next) =
                prepare_session_batch(&state, request, &catalog).expect("authorized batch")
            else {
                panic!("new command must append");
            };
            let mut replayed = state.clone();
            apply_session_batch(&mut replayed, &batch, &catalog).expect("authorized replay");
            assert_eq!(replayed.last_sequence, initial_sequence);
            assert_ne!(replayed.last_checksum, previous_checksum);
            assert_eq!(replayed.last_checksum, next.last_checksum);
            state = next;
        }
        assert_eq!(state.commands.len(), 6);
    }

    #[test]
    fn command_only_authorization_rejects_tampered_shape_state_and_catalog() {
        let actual = session("session-command-only-negative-owner");
        let requested = session("session-command-only-negative-requested");
        let catalog = command_only_catalog(&actual, &requested);
        let state = state_after(
            &actual,
            &finished_vector(&actual, "turn-negative", ApprovalDecision::Approve),
        );
        let turn_id = TurnId("turn-negative".to_owned());
        let terminal_command = ClientCommand::TurnCancel {
            session_id: actual.clone(),
            turn_id: turn_id.clone(),
        };
        let terminal_reply = |command_id: &str| {
            CommandReply::success(
                ClientCommandId(command_id.to_owned()),
                CommandResult::TurnAlreadyTerminal {
                    turn_id: turn_id.clone(),
                    terminal_status: TurnStatus::Succeeded,
                    terminal_sequence: 7,
                },
            )
        };

        let mut wrong_reason = command_only_request(
            "wrong-reason",
            terminal_command.clone(),
            CommandOnlyReasonV1::TurnAlreadyTerminal,
            terminal_reply("wrong-reason"),
        );
        wrong_reason
            .command_only_authorization
            .as_mut()
            .expect("authorization")
            .reason = CommandOnlyReasonV1::QuestionAlreadyResolved;

        let wrong_target_command = ClientCommand::TurnCancel {
            session_id: actual.clone(),
            turn_id: TurnId("missing-turn".to_owned()),
        };
        let wrong_target = command_only_request(
            "wrong-target",
            wrong_target_command,
            CommandOnlyReasonV1::TurnAlreadyTerminal,
            CommandReply::success(
                ClientCommandId("wrong-target".to_owned()),
                CommandResult::TurnAlreadyTerminal {
                    turn_id: TurnId("missing-turn".to_owned()),
                    terminal_status: TurnStatus::Succeeded,
                    terminal_sequence: 7,
                },
            ),
        );

        let wrong_reply = command_only_request(
            "wrong-reply",
            terminal_command.clone(),
            CommandOnlyReasonV1::TurnAlreadyTerminal,
            CommandReply::success(
                ClientCommandId("wrong-reply".to_owned()),
                CommandResult::TurnAlreadyTerminal {
                    turn_id: turn_id.clone(),
                    terminal_status: TurnStatus::Failed,
                    terminal_sequence: 6,
                },
            ),
        );

        let mut wrong_digest = command_only_request(
            "wrong-digest",
            terminal_command.clone(),
            CommandOnlyReasonV1::TurnAlreadyTerminal,
            terminal_reply("wrong-digest"),
        );
        wrong_digest
            .command_record
            .as_mut()
            .expect("command")
            .payload_digest = format!("sha256:{}", "0".repeat(64));

        let wrong_variant = command_only_request(
            "wrong-variant",
            ClientCommand::Initialize,
            CommandOnlyReasonV1::TurnAlreadyTerminal,
            terminal_reply("wrong-variant"),
        );

        let mismatch_command = ClientCommand::TurnCancel {
            session_id: requested.clone(),
            turn_id: turn_id.clone(),
        };
        let wrong_message = command_only_request(
            "wrong-message",
            mismatch_command.clone(),
            CommandOnlyReasonV1::TurnOwnershipMismatch,
            CommandReply::error(
                ClientCommandId("wrong-message".to_owned()),
                ProtocolErrorCode::TurnOwnershipMismatch,
                "wrong literal",
            ),
        );
        let wrong_details = command_only_request(
            "wrong-details",
            mismatch_command,
            CommandOnlyReasonV1::TurnOwnershipMismatch,
            CommandReply::error_with_details(
                ClientCommandId("wrong-details".to_owned()),
                ProtocolErrorCode::TurnOwnershipMismatch,
                format!(
                    "turn `{}` does not belong to session `{}`",
                    turn_id.0, requested.0
                ),
                Some(crate::protocol::ProtocolErrorDetails {
                    expected_epoch: None,
                    actual_epoch: None,
                    head_sequence: Some(7),
                    recovery_action: crate::protocol::RecoveryAction::None,
                }),
            ),
        );
        let same_requested_session = command_only_request(
            "same-requested-session",
            ClientCommand::TurnCancel {
                session_id: actual.clone(),
                turn_id: turn_id.clone(),
            },
            CommandOnlyReasonV1::TurnOwnershipMismatch,
            CommandReply::error(
                ClientCommandId("same-requested-session".to_owned()),
                ProtocolErrorCode::TurnOwnershipMismatch,
                format!(
                    "turn `{}` does not belong to session `{}`",
                    turn_id.0, actual.0
                ),
            ),
        );
        let question = state
            .projection
            .questions
            .get("question-turn-negative")
            .expect("question")
            .clone();
        let invalid_choice = command_only_request(
            "invalid-choice",
            ClientCommand::QuestionRespond {
                session_id: actual.clone(),
                question_id: QuestionId("question-turn-negative".to_owned()),
                choice_id: ChoiceId("not-offered".to_owned()),
            },
            CommandOnlyReasonV1::QuestionAlreadyResolved,
            CommandReply::success(
                ClientCommandId("invalid-choice".to_owned()),
                CommandResult::QuestionAlreadyResolved(question),
            ),
        );
        let approval = state
            .projection
            .approvals
            .get("approval-turn-negative")
            .expect("approval")
            .clone();
        let subject_mismatch = command_only_request(
            "subject-mismatch",
            ClientCommand::ApprovalRespond {
                session_id: actual.clone(),
                approval_id: ApprovalId("approval-turn-negative".to_owned()),
                approval_subject_digest: digest("different-turn"),
                decision: ApprovalDecision::Approve,
            },
            CommandOnlyReasonV1::ApprovalAlreadyResolved,
            CommandReply::success(
                ClientCommandId("subject-mismatch".to_owned()),
                CommandResult::ApprovalAlreadyResolved(approval),
            ),
        );

        for request in [
            wrong_reason,
            wrong_target,
            wrong_reply,
            wrong_digest,
            wrong_variant,
            wrong_message,
            wrong_details,
            same_requested_session,
            invalid_choice,
            subject_mismatch,
        ] {
            assert!(matches!(
                prepare_session_batch(&state, request, &catalog),
                Err(StateStoreError::ProjectionRejected | StateStoreError::IncompatibleSchema)
            ));
        }

        let missing_requested = session("session-not-allocated");
        let mismatch_command = ClientCommand::TurnCancel {
            session_id: missing_requested.clone(),
            turn_id: turn_id.clone(),
        };
        let missing_catalog_request = command_only_request(
            "missing-requested",
            mismatch_command,
            CommandOnlyReasonV1::TurnOwnershipMismatch,
            CommandReply::error(
                ClientCommandId("missing-requested".to_owned()),
                ProtocolErrorCode::TurnOwnershipMismatch,
                format!(
                    "turn `{}` does not belong to session `{}`",
                    turn_id.0, missing_requested.0
                ),
            ),
        );
        assert!(matches!(
            prepare_session_batch(&state, missing_catalog_request, &catalog),
            Err(StateStoreError::ProjectionRejected)
        ));

        let wrong_actual_catalog = SessionAllocationCatalogContext::for_test([
            (actual.clone(), ProjectId("wrong-project".to_owned())),
            (requested, ProjectId("project-other".to_owned())),
        ]);
        let wrong_actual_request = command_only_request(
            "wrong-actual-owner",
            terminal_command.clone(),
            CommandOnlyReasonV1::TurnAlreadyTerminal,
            terminal_reply("wrong-actual-owner"),
        );
        assert!(matches!(
            prepare_session_batch(&state, wrong_actual_request, &wrong_actual_catalog),
            Err(StateStoreError::ProjectionRejected)
        ));

        let pre_head_zero_request = command_only_request(
            "pre-head-zero",
            terminal_command,
            CommandOnlyReasonV1::TurnAlreadyTerminal,
            terminal_reply("pre-head-zero"),
        );
        assert!(matches!(
            StoredSessionPlanV1::from_append_request(
                &actual,
                &ProjectId("project-1".to_owned()),
                0,
                None,
                &pre_head_zero_request,
            ),
            Err(StateStoreError::ProjectionRejected)
        ));
    }

    #[test]
    fn command_only_authorization_survives_checkpoint_full_replay_and_poisoned_rescan() {
        let root = tempfile::tempdir().expect("tempdir");
        make_private(&root);
        let actual = session("session-command-only-replay-owner");
        let requested = session("session-command-only-replay-requested");
        let catalog = command_only_catalog(&actual, &requested);
        let store = open_store(&root);
        let writer = append(
            ready_with_catalog(&store, &actual, catalog.clone()),
            "base-events",
            None,
            finished_vector(&actual, "turn-replay", ApprovalDecision::Approve),
        );
        writer.write_checkpoint().expect("base checkpoint");
        let terminal = |command_id: &str| {
            command_only_request(
                command_id,
                ClientCommand::TurnCancel {
                    session_id: actual.clone(),
                    turn_id: TurnId("turn-replay".to_owned()),
                },
                CommandOnlyReasonV1::TurnAlreadyTerminal,
                CommandReply::success(
                    ClientCommandId(command_id.to_owned()),
                    CommandResult::TurnAlreadyTerminal {
                        turn_id: TurnId("turn-replay".to_owned()),
                        terminal_status: TurnStatus::Succeeded,
                        terminal_sequence: 7,
                    },
                ),
            )
        };
        let ownership = |command_id: &str| {
            command_only_request(
                command_id,
                ClientCommand::TurnCancel {
                    session_id: requested.clone(),
                    turn_id: TurnId("turn-replay".to_owned()),
                },
                CommandOnlyReasonV1::TurnOwnershipMismatch,
                CommandReply::error(
                    ClientCommandId(command_id.to_owned()),
                    ProtocolErrorCode::TurnOwnershipMismatch,
                    format!(
                        "turn `turn-replay` does not belong to session `{}`",
                        requested.0
                    ),
                ),
            )
        };
        let (writer, _) = match writer.append(terminal("checkpoint-tail")) {
            Ok(value) => value,
            Err(_) => panic!("tail append must succeed"),
        };
        let expected_checksum = writer.state.last_checksum.clone();
        drop(writer);
        drop(store);

        reset_checkpoint_load_observed();
        let store = open_store(&root);
        let mut writer = ready_with_catalog(&store, &actual, catalog.clone());
        assert!(checkpoint_load_observed());
        assert_eq!(writer.state.last_sequence, 7);
        assert_eq!(writer.state.last_checksum, expected_checksum);
        writer.set_failpoint(AppendFailpoint::AfterSyncBeforeUpdate);
        let poisoned = match writer.append(ownership("poisoned-rescan")) {
            Err(SessionAppendFailure::Poisoned { writer, .. }) => writer,
            _ => panic!("injected post-sync failure must poison writer"),
        };
        let writer = match poisoned.recover() {
            SessionRecoveryOutcome::Ready(writer) => writer,
            SessionRecoveryOutcome::RepairRequired(_) | SessionRecoveryOutcome::Corrupt(_) => {
                panic!("durable command-only line must rescan cleanly")
            }
        };
        assert_eq!(writer.state.last_sequence, 7);
        assert_eq!(writer.state.commands.len(), 2);
        drop(writer);
        drop(store);

        fs::remove_file(checkpoint_path(&root, &actual)).expect("remove checkpoint fixture");
        let store = open_store(&root);
        let writer = ready_with_catalog(&store, &actual, catalog.clone());
        assert_eq!(writer.state.last_sequence, 7);
        assert_eq!(writer.state.commands.len(), 2);
        drop(writer);
        drop(store);

        let missing_requested = SessionAllocationCatalogContext::for_test([(
            actual.clone(),
            ProjectId("project-1".to_owned()),
        )]);
        let store = open_store(&root);
        assert!(matches!(
            store.open_session_writer_with_catalog(actual, missing_requested),
            Err(StateStoreError::ProjectionRejected)
        ));
    }

    #[test]
    fn command_only_none_preserves_legacy_nonempty_serialization_and_digests() {
        #[derive(Serialize)]
        struct LegacyPlan<'a> {
            schema_version: u32,
            session_id: &'a str,
            expected_project_id: &'a str,
            expected_pre_sequence: u64,
            expected_pre_batch_checksum: &'a Option<String>,
            transaction_id: &'a str,
            command_record: &'a Option<StoredCommandRecordV1>,
            restart_authorization: &'a Option<StoredRestartAuthorizationV1>,
            events: &'a [StoredSessionEventV1],
            canonical_plan_digest: &'a str,
        }

        #[derive(Serialize)]
        struct LegacyBatch<'a> {
            schema_version: u32,
            session_id: &'a str,
            stream_id: &'a str,
            epoch: u64,
            transaction_id: &'a str,
            event_count: u64,
            first_sequence: u64,
            last_sequence: u64,
            command_record: &'a Option<StoredCommandRecordV1>,
            restart_authorization: &'a Option<StoredRestartAuthorizationV1>,
            events: &'a [StoredSessionEventV1],
            previous_batch_checksum: &'a Option<String>,
            batch_checksum: &'a str,
        }

        let session_id = session("session-command-only-legacy");
        let request = SessionAppendRequest::new(
            "legacy-transaction".to_owned(),
            None,
            vec![SessionRolloutEvent::SessionStarted {
                session_id: session_id.clone(),
                project_id: ProjectId("project-1".to_owned()),
            }],
        );
        let plan = StoredSessionPlanV1::from_append_request(
            &session_id,
            &ProjectId("project-1".to_owned()),
            0,
            None,
            &request,
        )
        .expect("legacy plan");
        let legacy_plan = LegacyPlan {
            schema_version: plan.schema_version,
            session_id: &plan.session_id,
            expected_project_id: &plan.expected_project_id,
            expected_pre_sequence: plan.expected_pre_sequence,
            expected_pre_batch_checksum: &plan.expected_pre_batch_checksum,
            transaction_id: &plan.transaction_id,
            command_record: &plan.command_record,
            restart_authorization: &plan.restart_authorization,
            events: &plan.events,
            canonical_plan_digest: &plan.canonical_plan_digest,
        };
        assert_eq!(
            serde_json::to_vec(&plan).expect("new plan bytes"),
            serde_json::to_vec(&legacy_plan).expect("legacy plan bytes")
        );

        let state = empty_session_state(session_id.clone(), "test-instance".to_owned());
        let PreparedSessionAppend::Batch(batch, _) =
            prepare_session_batch(&state, request, &empty_catalog()).expect("legacy batch")
        else {
            panic!("new legacy transaction must append");
        };
        let legacy_batch = LegacyBatch {
            schema_version: batch.schema_version,
            session_id: &batch.session_id,
            stream_id: &batch.stream_id,
            epoch: batch.epoch,
            transaction_id: &batch.transaction_id,
            event_count: batch.event_count,
            first_sequence: batch.first_sequence,
            last_sequence: batch.last_sequence,
            command_record: &batch.command_record,
            restart_authorization: &batch.restart_authorization,
            events: &batch.events,
            previous_batch_checksum: &batch.previous_batch_checksum,
            batch_checksum: &batch.batch_checksum,
        };
        assert_eq!(
            serde_json::to_vec(&batch).expect("new batch bytes"),
            serde_json::to_vec(&legacy_batch).expect("legacy batch bytes")
        );
    }

    #[test]
    fn cursor_truth_table_and_stream_identity_survive_restart() {
        let root = tempfile::tempdir().expect("tempdir");
        make_private(&root);
        let id = session("session-cursor");
        let store = open_store(&root);
        let writer = append(
            ready(&store, &id),
            "vector",
            None,
            finished_vector(&id, "turn-cursor", ApprovalDecision::Approve),
        );
        let snapshot = writer.snapshot().expect("snapshot");
        assert_eq!(snapshot.stream_epoch, SESSION_STREAM_EPOCH);
        assert_eq!(snapshot.covered_through_sequence, 7);
        let all = writer.page(0).expect("from zero");
        assert_eq!(all.stream_id, id.0);
        assert_eq!(all.events.len(), 7);
        assert_eq!(writer.page(7).expect("head").events, Vec::new());
        assert!(matches!(
            writer.page(8),
            Err(StateStoreError::SequenceMismatch)
        ));
        let cursor = |stream_kind, stream_id: &str, epoch, after_sequence| StreamCursor {
            stream_kind,
            stream_id: stream_id.to_owned(),
            epoch,
            after_sequence,
        };
        assert_eq!(
            writer.resume(&cursor(
                StreamKind::ProjectEvent,
                &id.0,
                SESSION_STREAM_EPOCH,
                0
            )),
            Err(SessionCursorError::UnsupportedStreamKind)
        );
        assert_eq!(
            writer.resume(&cursor(
                StreamKind::SessionRollout,
                "another-session",
                SESSION_STREAM_EPOCH,
                0
            )),
            Err(SessionCursorError::SessionMismatch)
        );
        assert_eq!(
            writer.resume(&cursor(StreamKind::SessionRollout, &id.0, 2, 0)),
            Err(SessionCursorError::EpochMismatch {
                expected_epoch: SESSION_STREAM_EPOCH,
                actual_epoch: 2,
                head_sequence: 7,
            })
        );
        assert_eq!(
            writer.resume(&cursor(
                StreamKind::SessionRollout,
                &id.0,
                SESSION_STREAM_EPOCH,
                8
            )),
            Err(SessionCursorError::Future { head_sequence: 7 })
        );
        drop(writer);
        drop(store);

        let store = open_store(&root);
        let writer = ready(&store, &id);
        let resumed = writer.page(3).expect("resume");
        assert_eq!(resumed.stream_id, snapshot.session_id.0);
        assert_eq!(resumed.epoch, SESSION_STREAM_EPOCH);
        assert_eq!(resumed.head_sequence, 7);
        assert_eq!(
            resumed
                .events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![4, 5, 6, 7]
        );
    }

    #[test]
    fn restart_planner_converges_running_cancelled_waiting_and_terminal_turns() {
        let running_id = session("session-running");
        let running = replay(&running_id, &prefix(&running_id, "turn-running")).expect("running");
        let plan = plan_restart_reconciliation("instance", &running)
            .expect("plan")
            .expect("events");
        assert_eq!(plan.transaction_id, "restart-v1:instance:session-running:2");
        assert_eq!(
            plan.events,
            vec![SessionRolloutEvent::TurnCompleted {
                turn_id: TurnId("turn-running".to_owned()),
                status: TurnStatus::AbortedByRestart,
            }]
        );
        let mut after = running.clone();
        after
            .authorize_restart(plan.authorization.as_ref().expect("authorization"))
            .expect("authorize planner facts");
        for (offset, event) in plan.events.iter().enumerate() {
            after
                .apply(
                    running.head_sequence + u64::try_from(offset).expect("offset") + 1,
                    event,
                )
                .expect("apply reconciliation");
        }
        assert_eq!(
            after.snapshot().expect("snapshot").turns[0].status,
            TurnStatus::AbortedByRestart
        );
        assert!(
            plan_restart_reconciliation("instance", &after)
                .expect("terminal plan")
                .is_none()
        );

        let waiting_id = session("session-waiting");
        let waiting =
            replay(&waiting_id, &through_question(&waiting_id, "turn-wait")).expect("waiting");
        assert!(
            plan_restart_reconciliation("instance", &waiting)
                .expect("waiting valid")
                .is_none()
        );

        let mut cancel_events = through_question(&waiting_id, "turn-cancel");
        cancel_events.push(SessionRolloutEvent::TurnCancelRequested {
            turn_id: TurnId("turn-cancel".to_owned()),
        });
        let cancelling = replay(&waiting_id, &cancel_events).expect("cancel requested");
        let plan = plan_restart_reconciliation("instance", &cancelling)
            .expect("plan")
            .expect("cancel events");
        assert!(matches!(
            plan.events.as_slice(),
            [
                SessionRolloutEvent::QuestionOwnerTurnAborted { .. },
                SessionRolloutEvent::TurnCompleted {
                    status: TurnStatus::Cancelled,
                    ..
                }
            ]
        ));

        let budget_id = session("session-budget");
        let mut budget_events = prefix(&budget_id, "turn-budget");
        budget_events.push(SessionRolloutEvent::TurnCompleted {
            turn_id: TurnId("turn-budget".to_owned()),
            status: TurnStatus::BudgetExceeded,
        });
        assert!(matches!(
            replay(&budget_id, &budget_events),
            Err(StateStoreError::ProjectionRejected)
        ));

        let mut invalid_waiting = running;
        invalid_waiting
            .turns
            .get_mut("turn-running")
            .expect("turn")
            .snapshot
            .status = TurnStatus::WaitingForInput;
        assert!(matches!(
            plan_restart_reconciliation("instance", &invalid_waiting),
            Err(StateStoreError::ProjectionRejected)
        ));
    }

    #[test]
    fn coordinated_restart_identity_is_stable_and_replays_with_its_exact_snapshot_reply() {
        let id = session("session-coordinated-restart");
        let projection = replay(&id, &prefix(&id, "turn-coordinated")).expect("running");
        let first = plan_coordinated_restart_reconciliation("test-instance", &projection)
            .expect("coordinated planner")
            .expect("restart obligation");
        let second = plan_coordinated_restart_reconciliation("test-instance", &projection)
            .expect("coordinated planner")
            .expect("same restart obligation");
        assert_eq!(first, second);
        assert_eq!(
            first.intent,
            "restart-v1:test-instance:session-coordinated-restart:2"
        );
        assert_eq!(
            first.payload_digest,
            "sha256:849a68f87bb2528f48809c492496c3a06ab1b3ce2cf243a2b79a10be21bd0361"
        );
        assert_eq!(
            first.global_tx_id,
            "global-849a68f87bb2528f48809c492496c3a0"
        );
        assert_eq!(
            first.global_tx_id,
            restart_global_tx_id(&first.payload_digest).expect("global mapping")
        );
        assert_eq!(
            first.session_transaction_id,
            format!("{}:session", first.global_tx_id)
        );
        assert_eq!(first.command_record.client_id, INTERNAL_RESTART_CLIENT_ID);
        assert_eq!(first.command_record.client_command_id, first.intent);
        assert_eq!(first.command_record.payload_digest, first.payload_digest);

        let reply: CommandReply = serde_json::from_slice(
            &first
                .command_record
                .decode_reply()
                .expect("canonical reply"),
        )
        .expect("typed reply");
        let CommandOutcome::Success {
            result: CommandResult::SessionSnapshot(snapshot),
        } = reply.outcome
        else {
            panic!("restart reply must be a SessionSnapshot");
        };
        assert_eq!(snapshot.session_id, id);
        assert_eq!(snapshot.covered_through_sequence, 3);
        assert_eq!(snapshot.turns[0].status, TurnStatus::AbortedByRestart);

        let state = state_after(&id, &prefix(&id, "turn-coordinated"));
        let PreparedSessionAppend::Batch(batch, committed) =
            prepare_session_batch(&state, first.append_request(), &empty_catalog())
                .expect("coordinated append")
        else {
            panic!("first coordinated append cannot be idempotent");
        };
        assert_eq!(batch.transaction_id, first.session_transaction_id);
        assert_eq!(
            committed.projection.snapshot().expect("committed snapshot"),
            snapshot
        );
    }

    #[test]
    fn forged_internal_restart_namespace_and_mapping_are_rejected() {
        let id = session("session-forged-restart");
        let projection = replay(&id, &prefix(&id, "turn-forged")).expect("running");
        let valid = plan_coordinated_restart_reconciliation("test-instance", &projection)
            .expect("planner")
            .expect("restart obligation");
        let state = state_after(&id, &prefix(&id, "turn-forged"));

        let mut wrong_namespace = valid.append_request();
        wrong_namespace
            .command_record
            .as_mut()
            .expect("command")
            .client_id = "__alda_internal_forged".to_owned();
        assert!(matches!(
            prepare_session_batch(&state, wrong_namespace, &empty_catalog()),
            Err(StateStoreError::ProjectionRejected)
        ));

        let mut wrong_transaction = valid.append_request();
        wrong_transaction.transaction_id =
            "global-00000000000000000000000000000000:session".to_owned();
        assert!(matches!(
            prepare_session_batch(&state, wrong_transaction, &empty_catalog()),
            Err(StateStoreError::ProjectionRejected)
        ));

        let mut wrong_digest = valid.append_request();
        wrong_digest
            .command_record
            .as_mut()
            .expect("command")
            .payload_digest = format!("sha256:{}", "0".repeat(64));
        assert!(matches!(
            prepare_session_batch(&state, wrong_digest, &empty_catalog()),
            Err(StateStoreError::ProjectionRejected)
        ));
    }

    #[test]
    fn partial_tail_poison_retains_lease_until_compare_and_truncate_repair() {
        let root = tempfile::tempdir().expect("tempdir");
        make_private(&root);
        let id = session("session-tail");
        let store = open_store(&root);
        let writer = append(
            ready(&store, &id),
            "start",
            None,
            vec![SessionRolloutEvent::SessionStarted {
                session_id: id.clone(),
                project_id: ProjectId("project-1".to_owned()),
            }],
        );
        let mut writer = writer;
        writer.set_failpoint(AppendFailpoint::PartialWrite(11));
        let poisoned = match writer.append(SessionAppendRequest {
            transaction_id: "turn".to_owned(),
            command_record: Some(command("turn", '1')),
            events: vec![SessionRolloutEvent::TurnStarted {
                turn_id: TurnId("turn-1".to_owned()),
                canonical_prompt: "prompt".to_owned(),
            }],
            restart_authorization: None,
            command_only_authorization: None,
        }) {
            Err(SessionAppendFailure::Poisoned { writer, .. }) => writer,
            _ => panic!("must poison"),
        };
        assert!(matches!(
            store.open_session_writer(id.clone()),
            Err(StateStoreError::WriterLockRequired)
        ));
        let repair = match poisoned.recover() {
            SessionRecoveryOutcome::RepairRequired(writer) => writer,
            _ => panic!("partial tail requires repair"),
        };
        assert!(matches!(
            store.open_session_writer(id.clone()),
            Err(StateStoreError::WriterLockRequired)
        ));
        let writer = match repair.repair() {
            Ok(writer) => writer,
            Err(_) => panic!("repair exact tail"),
        };
        assert_eq!(writer.state.last_sequence, 1);
        drop(writer);
        assert!(matches!(
            store.open_session_writer(id),
            Ok(OpenSessionWriter::Ready(_))
        ));
    }

    #[test]
    fn synced_batch_before_response_recovers_exact_reply_without_duplicate_events() {
        let root = tempfile::tempdir().expect("tempdir");
        make_private(&root);
        let id = session("session-after-sync");
        let store = open_store(&root);
        let writer = append(
            ready(&store, &id),
            "start",
            None,
            vec![SessionRolloutEvent::SessionStarted {
                session_id: id.clone(),
                project_id: ProjectId("project-1".to_owned()),
            }],
        );
        let mut writer = writer;
        writer.set_failpoint(AppendFailpoint::AfterSyncBeforeUpdate);
        let poisoned = match writer.append(SessionAppendRequest {
            transaction_id: "turn".to_owned(),
            command_record: Some(command("turn", '1')),
            events: vec![SessionRolloutEvent::TurnStarted {
                turn_id: TurnId("turn-1".to_owned()),
                canonical_prompt: "prompt".to_owned(),
            }],
            restart_authorization: None,
            command_only_authorization: None,
        }) {
            Err(SessionAppendFailure::Poisoned { writer, .. }) => writer,
            _ => panic!("must poison"),
        };
        let writer = match poisoned.recover() {
            SessionRecoveryOutcome::Ready(writer) => writer,
            _ => panic!("complete synced line is committed"),
        };
        assert_eq!(writer.state.last_sequence, 2);
        let retry = writer.append(SessionAppendRequest {
            transaction_id: "retry".to_owned(),
            command_record: Some(command("turn", '1')),
            events: vec![SessionRolloutEvent::TurnStarted {
                turn_id: TurnId("would-duplicate".to_owned()),
                canonical_prompt: "bad".to_owned(),
            }],
            restart_authorization: None,
            command_only_authorization: None,
        });
        let (writer, outcome) = match retry {
            Ok(value) => value,
            Err(_) => panic!("exact retry"),
        };
        assert!(!outcome.appended);
        assert_eq!(outcome.stable_reply, Some(reply("turn")));
        assert_eq!(writer.state.events.len(), 2);
    }

    #[test]
    fn complete_session_line_requires_a_successful_recovery_sync() {
        let root = tempfile::tempdir().expect("tempdir");
        make_private(&root);
        let id = session("session-recovery-sync");
        let store = open_store(&root);
        let writer = append(
            ready(&store, &id),
            "start",
            None,
            vec![SessionRolloutEvent::SessionStarted {
                session_id: id.clone(),
                project_id: ProjectId("project-1".to_owned()),
            }],
        );
        let mut writer = writer;
        writer.set_failpoint(AppendFailpoint::FileSyncError);
        let Err(SessionAppendFailure::Poisoned {
            writer: mut poisoned,
            ..
        }) = writer.append(SessionAppendRequest {
            transaction_id: "turn-recovery-sync".to_owned(),
            command_record: Some(command("turn-recovery-sync", '1')),
            events: vec![SessionRolloutEvent::TurnStarted {
                turn_id: TurnId("turn-recovery-sync".to_owned()),
                canonical_prompt: "prompt".to_owned(),
            }],
            restart_authorization: None,
            command_only_authorization: None,
        })
        else {
            panic!("file sync failure must poison a complete Session line");
        };
        poisoned.set_recovery_failpoint(RecoveryFailpoint::FileSync);
        assert!(matches!(
            poisoned.recover(),
            SessionRecoveryOutcome::Corrupt(_)
        ));
    }

    #[test]
    fn full_replay_and_checkpoint_tail_across_same_head_batches_are_identical() {
        let root = tempfile::tempdir().expect("tempdir");
        make_private(&root);
        let id = session("session-checkpoint");
        let catalog = SessionAllocationCatalogContext::for_test([(
            id.clone(),
            ProjectId("project-1".to_owned()),
        )]);
        let store = open_store(&root);
        let writer = append(
            ready_with_catalog(&store, &id, catalog.clone()),
            "start",
            Some(command("start", '1')),
            finished_vector(&id, "turn-terminal", ApprovalDecision::Approve),
        );
        let terminal_request = |command_id: &str| {
            command_only_request(
                command_id,
                ClientCommand::TurnCancel {
                    session_id: id.clone(),
                    turn_id: TurnId("turn-terminal".to_owned()),
                },
                CommandOnlyReasonV1::TurnAlreadyTerminal,
                CommandReply::success(
                    ClientCommandId(command_id.to_owned()),
                    CommandResult::TurnAlreadyTerminal {
                        turn_id: TurnId("turn-terminal".to_owned()),
                        terminal_status: TurnStatus::Succeeded,
                        terminal_sequence: 7,
                    },
                ),
            )
        };
        let writer = match writer.append(terminal_request("empty-1")) {
            Ok((writer, _)) => writer,
            Err(_) => panic!("first same-head command-only append"),
        };
        let writer = match writer.append(terminal_request("empty-2")) {
            Ok((writer, _)) => writer,
            Err(_) => panic!("second same-head command-only append"),
        };
        writer
            .write_checkpoint()
            .expect("checkpoint over empty batches");
        let writer = append(
            writer,
            "turn",
            Some(command("turn", '4')),
            vec![SessionRolloutEvent::TurnStarted {
                turn_id: TurnId("turn-after-checkpoint".to_owned()),
                canonical_prompt: "prompt".to_owned(),
            }],
        );
        let writer = match writer.append(terminal_request("empty-3")) {
            Ok((writer, _)) => writer,
            Err(_) => panic!("tail same-head command-only append"),
        };
        let expected_snapshot = writer.snapshot().expect("snapshot");
        let expected_checksum = writer.state.last_checksum.clone();
        let expected_commands = writer.state.commands.clone();
        drop(writer);
        drop(store);

        let store = open_store(&root);
        let checkpoint_tail = ready_with_catalog(&store, &id, catalog.clone());
        assert_eq!(
            checkpoint_tail.snapshot().expect("snapshot"),
            expected_snapshot
        );
        assert_eq!(checkpoint_tail.state.last_checksum, expected_checksum);
        assert_eq!(checkpoint_tail.state.commands, expected_commands);
        drop(checkpoint_tail);
        drop(store);

        fs::write(checkpoint_path(&root, &id), b"corrupt cache").expect("corrupt cache");
        let store = open_store(&root);
        let full = ready_with_catalog(&store, &id, catalog);
        assert_eq!(full.snapshot().expect("snapshot"), expected_snapshot);
        assert_eq!(full.state.last_checksum, expected_checksum);
        assert_eq!(full.state.commands, expected_commands);
    }

    #[test]
    fn transaction_probe_same_plan_conflict_and_checkpoint_replay_are_exact() {
        let root = tempfile::tempdir().expect("tempdir");
        make_private(&root);
        let id = session("session-transaction-vector");
        let store = open_store(&root);
        let request = SessionAppendRequest {
            transaction_id: "control-session-transaction-1".to_owned(),
            command_record: None,
            events: vec![SessionRolloutEvent::SessionStarted {
                session_id: id.clone(),
                project_id: ProjectId("project-1".to_owned()),
            }],
            restart_authorization: None,
            command_only_authorization: None,
        };
        let digest = request
            .canonical_plan_digest(&id)
            .expect("canonical Session plan digest");
        let writer = ready(&store, &id);
        assert_eq!(
            writer.probe_transaction("control-session-transaction-1", &digest),
            TransactionProbe::Absent
        );
        let (writer, first) = match writer.append(request) {
            Ok(value) => value,
            Err(_) => panic!("first Session transaction"),
        };
        assert!(first.appended);
        let committed = match writer.probe_transaction("control-session-transaction-1", &digest) {
            TransactionProbe::SamePlanCommitted(committed) => committed,
            other => panic!("expected same committed Session plan, got {other:?}"),
        };
        assert_eq!(
            digest,
            "sha256:07f40debbda76f03ad3f1347fc08d5f0bc940ea19455952d82045c23c506352d"
        );
        assert_eq!(
            committed.resulting_batch_checksum,
            "sha256:f20017af921e4b0f20347cc46476059097f8957ea9cd1be4b84cd33be49a1666"
        );
        assert_eq!(committed.resulting_last_sequence, 1);

        let retry = SessionAppendRequest {
            transaction_id: "control-session-transaction-1".to_owned(),
            command_record: None,
            events: vec![SessionRolloutEvent::SessionStarted {
                session_id: id.clone(),
                project_id: ProjectId("project-1".to_owned()),
            }],
            restart_authorization: None,
            command_only_authorization: None,
        };
        let (writer, retry) = match writer.append(retry) {
            Ok(value) => value,
            Err(_) => panic!("same Session plan retry"),
        };
        assert!(!retry.appended);
        assert_eq!(retry.last_sequence, 1);
        assert_eq!(
            writer.probe_transaction(
                "control-session-transaction-1",
                &format!("sha256:{}", "f".repeat(64))
            ),
            TransactionProbe::ConflictingPlan
        );

        let conflict = SessionAppendRequest {
            transaction_id: "control-session-transaction-1".to_owned(),
            command_record: None,
            events: vec![SessionRolloutEvent::TurnStarted {
                turn_id: TurnId("turn-conflict".to_owned()),
                canonical_prompt: "different".to_owned(),
            }],
            restart_authorization: None,
            command_only_authorization: None,
        };
        let writer = match writer.append(conflict) {
            Err(SessionAppendFailure::Rejected { writer, error }) => {
                assert!(matches!(error, StateStoreError::IdempotencyConflict));
                writer
            }
            _ => panic!("same Session transaction with different plan must conflict"),
        };
        writer.write_checkpoint().expect("Session checkpoint");
        let expected_transactions = writer.state.transactions.clone();
        drop(writer);
        drop(store);

        let store = open_store(&root);
        let checkpoint = ready(&store, &id);
        assert_eq!(checkpoint.state.transactions, expected_transactions);
        assert!(matches!(
            checkpoint.probe_transaction("control-session-transaction-1", &digest),
            TransactionProbe::SamePlanCommitted(_)
        ));
        drop(checkpoint);
        drop(store);

        let checkpoint_path = checkpoint_path(&root, &id);
        let mut tampered: StoredSessionCheckpointV1 =
            serde_json::from_slice(&fs::read(&checkpoint_path).expect("read Session checkpoint"))
                .expect("decode Session checkpoint");
        tampered.transaction_index[0].canonical_plan_digest = format!("sha256:{}", "e".repeat(64));
        tampered.checksum =
            session_checkpoint_checksum(&tampered).expect("rechecksum tampered Session cache");
        fs::write(
            checkpoint_path,
            serde_json::to_vec(&tampered).expect("encode tampered Session checkpoint"),
        )
        .expect("tamper Session checkpoint");
        let store = open_store(&root);
        let full_replay = ready(&store, &id);
        assert_eq!(full_replay.state.transactions, expected_transactions);
    }

    #[test]
    fn list_sessions_rebuilds_global_owner_index_and_rejects_unknown_or_duplicate_entries() {
        let root = tempfile::tempdir().expect("tempdir");
        make_private(&root);
        let first = session("session-catalog-1");
        let second = session("session-catalog-2");
        let store = open_store(&root);
        drop(append(
            ready(&store, &first),
            "first",
            None,
            prefix(&first, "shared-turn"),
        ));
        drop(append(
            ready(&store, &second),
            "second",
            None,
            prefix(&second, "other-turn"),
        ));
        let catalog = store.list_sessions().expect("valid catalog");
        assert_eq!(catalog.sessions.len(), 2);
        assert_eq!(catalog.turn_owners.get("shared-turn"), Some(&first.0));

        let sessions_path = root
            .path()
            .join(super::super::STATE_LAYOUT)
            .join("sessions");
        fs::create_dir(sessions_path.join("unknown")).expect("unknown entry");
        fs::set_permissions(
            sessions_path.join("unknown"),
            fs::Permissions::from_mode(0o700),
        )
        .expect("private");
        assert!(matches!(
            store.list_sessions(),
            Err(StateStoreError::UnsafeRoot)
        ));
        fs::remove_dir(sessions_path.join("unknown")).expect("remove fixture");
        drop(store);

        let duplicate_root = tempfile::tempdir().expect("tempdir");
        make_private(&duplicate_root);
        let store = open_store(&duplicate_root);
        drop(append(
            ready(&store, &first),
            "first",
            None,
            prefix(&first, "duplicate-turn"),
        ));
        drop(append(
            ready(&store, &second),
            "second",
            None,
            prefix(&second, "duplicate-turn"),
        ));
        assert!(matches!(
            store.list_sessions(),
            Err(StateStoreError::ProjectionRejected)
        ));
    }

    #[test]
    fn directory_hash_mismatch_and_special_rollout_fail_closed() {
        let root = tempfile::tempdir().expect("tempdir");
        make_private(&root);
        let first = session("session-hash-source");
        let second = session("session-hash-target");
        let store = open_store(&root);
        drop(append(
            ready(&store, &first),
            "first",
            None,
            vec![SessionRolloutEvent::SessionStarted {
                session_id: first.clone(),
                project_id: ProjectId("project-1".to_owned()),
            }],
        ));
        drop(append(
            ready(&store, &second),
            "second",
            None,
            vec![SessionRolloutEvent::SessionStarted {
                session_id: second.clone(),
                project_id: ProjectId("project-1".to_owned()),
            }],
        ));
        fs::copy(rollout_path(&root, &first), rollout_path(&root, &second))
            .expect("copy wrong identity");
        assert!(matches!(
            store.list_sessions(),
            Err(StateStoreError::StreamMismatch)
        ));
        drop(store);

        let special_root = tempfile::tempdir().expect("tempdir");
        make_private(&special_root);
        let id = session("session-special");
        let store = open_store(&special_root);
        drop(append(
            ready(&store, &id),
            "start",
            None,
            vec![SessionRolloutEvent::SessionStarted {
                session_id: id.clone(),
                project_id: ProjectId("project-1".to_owned()),
            }],
        ));
        fs::remove_file(rollout_path(&special_root, &id)).expect("remove rollout");
        std::os::unix::fs::symlink("/dev/null", rollout_path(&special_root, &id))
            .expect("special symlink");
        assert!(matches!(
            store.list_sessions(),
            Err(StateStoreError::Io { .. })
        ));
    }

    #[test]
    fn typed_random_id_allocation_is_bounded_and_collision_checked() {
        let occupied = BTreeSet::from(["turn-00000000000000000000000000000000".to_owned()]);
        let mut candidates = vec![
            "00000000000000000000000000000001".to_owned(),
            "00000000000000000000000000000000".to_owned(),
        ];
        let allocated =
            allocate_typed_id_with("turn", &occupied, || candidates.pop().expect("candidate"))
                .expect("collision retry");
        assert_eq!(allocated, "turn-00000000000000000000000000000001");

        let occupied = BTreeSet::from(["session-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned()]);
        assert!(matches!(
            allocate_typed_id_with("session", &occupied, || "a".repeat(32)),
            Err(StateStoreError::IdempotencyConflict)
        ));
        let generated = allocate_typed_id("approval", &BTreeSet::new()).expect("random ID");
        assert!(generated.starts_with("approval-"));
        assert_eq!(generated.len(), "approval-".len() + 32);
    }

    #[test]
    fn session_initialization_durability_failpoints_are_explicit_and_reopenable() {
        for failpoint in [
            InitFailpoint::SessionsCreate,
            InitFailpoint::SessionsChildSync,
            InitFailpoint::SessionsParentSync,
        ] {
            let root = tempfile::tempdir().expect("tempdir");
            make_private(&root);
            assert!(
                StateStore::open_with_failpoint(
                    root.path(),
                    StateStoreInstanceLease::for_tests(),
                    failpoint,
                )
                .is_err()
            );
            let reopened = StateStore::open(root.path(), StateStoreInstanceLease::for_tests());
            assert!(
                reopened.is_ok()
                    || matches!(
                        reopened,
                        Err(StateStoreError::Io { .. } | StateStoreError::IncompatibleSchema)
                    )
            );
        }

        for failpoint in [
            InitFailpoint::SessionDirectoryCreate,
            InitFailpoint::SessionDirectoryChildSync,
            InitFailpoint::SessionDirectoryParentSync,
            InitFailpoint::RolloutCreate,
            InitFailpoint::RolloutFileSync,
            InitFailpoint::RolloutDirectorySync,
        ] {
            let root = tempfile::tempdir().expect("tempdir");
            make_private(&root);
            let store = StateStore::open_with_failpoint(
                root.path(),
                StateStoreInstanceLease::for_tests(),
                failpoint,
            )
            .expect("global layout ignores per-Session failpoint");
            assert!(
                store
                    .open_session_writer(session("session-init-fail"))
                    .is_err()
            );
            drop(store);
            let reopened = open_store(&root);
            assert!(matches!(
                reopened.open_session_writer(session("session-init-fail")),
                Ok(OpenSessionWriter::Ready(_))
            ));
        }
    }

    #[test]
    fn append_checkpoint_and_repair_failpoint_matrix_preserves_prefix() {
        for failpoint in [
            AppendFailpoint::BeforeWrite,
            AppendFailpoint::PartialWrite(7),
            AppendFailpoint::AfterNewlineBeforeSync,
            AppendFailpoint::FileSyncError,
            AppendFailpoint::AfterSyncBeforeUpdate,
        ] {
            let root = tempfile::tempdir().expect("tempdir");
            make_private(&root);
            let id = session(&format!("session-append-{failpoint:?}"));
            let store = open_store(&root);
            let writer = append(
                ready(&store, &id),
                "start",
                None,
                vec![SessionRolloutEvent::SessionStarted {
                    session_id: id.clone(),
                    project_id: ProjectId("project-1".to_owned()),
                }],
            );
            let mut writer = writer;
            writer.set_failpoint(failpoint);
            match writer.append(SessionAppendRequest {
                transaction_id: "turn".to_owned(),
                command_record: None,
                events: vec![SessionRolloutEvent::TurnStarted {
                    turn_id: TurnId("turn-1".to_owned()),
                    canonical_prompt: "prompt".to_owned(),
                }],
                restart_authorization: None,
                command_only_authorization: None,
            }) {
                Err(SessionAppendFailure::Rejected { writer, .. }) => {
                    assert_eq!(writer.state.last_sequence, 1);
                }
                Err(SessionAppendFailure::Poisoned { writer, .. }) => match writer.recover() {
                    SessionRecoveryOutcome::Ready(writer) => {
                        assert!(matches!(writer.state.last_sequence, 1 | 2));
                    }
                    SessionRecoveryOutcome::RepairRequired(writer) => {
                        let writer = match writer.repair() {
                            Ok(writer) => writer,
                            Err(_) => panic!("repair injected append tail"),
                        };
                        assert_eq!(writer.state.last_sequence, 1);
                    }
                    SessionRecoveryOutcome::Corrupt(_) => panic!("injected append is recoverable"),
                },
                Ok(_) => panic!("failpoint must not return success"),
            }
        }

        for failpoint in [
            CheckpointFailpoint::TempCreate,
            CheckpointFailpoint::TempWrite,
            CheckpointFailpoint::FileSync,
            CheckpointFailpoint::BeforeInstall,
            CheckpointFailpoint::AfterInstall,
            CheckpointFailpoint::DirectorySyncError,
        ] {
            let root = tempfile::tempdir().expect("tempdir");
            make_private(&root);
            let id = session(&format!("session-checkpoint-{failpoint:?}"));
            let store = open_store(&root);
            let mut writer = append(
                ready(&store, &id),
                "start",
                Some(command("start", '1')),
                vec![SessionRolloutEvent::SessionStarted {
                    session_id: id.clone(),
                    project_id: ProjectId("project-1".to_owned()),
                }],
            );
            writer.set_checkpoint_failpoint(failpoint);
            assert!(writer.write_checkpoint().is_err());
            drop(writer);
            drop(store);
            let store = open_store(&root);
            let reopened = ready(&store, &id);
            assert_eq!(reopened.state.last_sequence, 1);
            assert_eq!(reopened.state.commands.len(), 1);
        }

        for failpoint in [
            RepairFailpoint::RescanRace,
            RepairFailpoint::TruncateError,
            RepairFailpoint::FileSyncError,
            RepairFailpoint::DirectorySyncError,
        ] {
            let root = tempfile::tempdir().expect("tempdir");
            make_private(&root);
            let id = session(&format!("session-repair-{failpoint:?}"));
            let store = open_store(&root);
            let writer = append(
                ready(&store, &id),
                "start",
                None,
                vec![SessionRolloutEvent::SessionStarted {
                    session_id: id.clone(),
                    project_id: ProjectId("project-1".to_owned()),
                }],
            );
            let mut writer = writer;
            writer.set_failpoint(AppendFailpoint::PartialWrite(5));
            let poisoned = match writer.append(SessionAppendRequest {
                transaction_id: "turn".to_owned(),
                command_record: None,
                events: vec![SessionRolloutEvent::TurnStarted {
                    turn_id: TurnId("turn-1".to_owned()),
                    canonical_prompt: "prompt".to_owned(),
                }],
                restart_authorization: None,
                command_only_authorization: None,
            }) {
                Err(SessionAppendFailure::Poisoned { writer, .. }) => writer,
                _ => panic!("partial write"),
            };
            let mut repair = match poisoned.recover() {
                SessionRecoveryOutcome::RepairRequired(repair) => repair,
                _ => panic!("repair required"),
            };
            repair.set_failpoint(failpoint);
            let corrupt = repair.repair();
            assert!(corrupt.is_err());
            drop(corrupt);
            match store.open_session_writer(id) {
                Ok(OpenSessionWriter::RepairRequired(_)) => {}
                Ok(OpenSessionWriter::Ready(writer))
                    if matches!(
                        failpoint,
                        RepairFailpoint::FileSyncError | RepairFailpoint::DirectorySyncError
                    ) =>
                {
                    assert_eq!(writer.state.last_sequence, 1);
                }
                _ => panic!("failed repair must leave explicit repair or a clean committed prefix"),
            }
        }
    }

    fn unchecked_batch(
        state: &RecoveredSessionState,
        events: &[SessionRolloutEvent],
    ) -> StoredSessionBatchV1 {
        let event_count = u64::try_from(events.len()).expect("event count");
        let first_sequence = state.last_sequence + 1;
        let last_sequence = if event_count == 0 {
            state.last_sequence
        } else {
            first_sequence + event_count - 1
        };
        let mut batch = StoredSessionBatchV1 {
            schema_version: 1,
            session_id: state.expected_session_id.0.clone(),
            stream_id: state.expected_session_id.0.clone(),
            epoch: SESSION_STREAM_EPOCH,
            transaction_id: format!("malicious-{first_sequence}"),
            event_count,
            first_sequence,
            last_sequence,
            command_record: None,
            restart_authorization: None,
            command_only_authorization: None,
            events: events.iter().map(StoredSessionEventV1::from_live).collect(),
            previous_batch_checksum: state.last_checksum.clone(),
            batch_checksum: String::new(),
        };
        batch.batch_checksum = session_batch_checksum(&batch).expect("checksum");
        batch
    }

    fn state_after(id: &SessionId, events: &[SessionRolloutEvent]) -> RecoveredSessionState {
        let mut state = empty_session_state(id.clone(), "test-instance".to_owned());
        let batch = unchecked_batch(&state, events);
        apply_session_batch(&mut state, &batch, &empty_catalog()).expect("valid prefix");
        state
    }

    #[test]
    fn recomputed_outer_checksum_cannot_bypass_session_domain_validation() {
        let id = session("session-tamper");

        let state = state_after(&id, &through_question(&id, "turn-choice"));
        let invalid_choice = unchecked_batch(
            &state,
            &[SessionRolloutEvent::QuestionResolved {
                question_id: QuestionId("question-turn-choice".to_owned()),
                choice_id: ChoiceId("not-offered".to_owned()),
                responder_client_id: ClientId("client".to_owned()),
            }],
        );
        assert!(matches!(
            apply_session_batch(&mut state.clone(), &invalid_choice, &empty_catalog()),
            Err(StateStoreError::ProjectionRejected)
        ));

        let state = state_after(&id, &through_approval(&id, "turn-digest"));
        let mut wrong_digest = digest("turn-digest");
        wrong_digest.value = "b".repeat(64);
        let invalid_digest = unchecked_batch(
            &state,
            &[SessionRolloutEvent::ApprovalResolved {
                approval_id: ApprovalId("approval-turn-digest".to_owned()),
                approval_subject_digest: wrong_digest,
                decision: ApprovalDecision::Approve,
                responder_client_id: ClientId("client".to_owned()),
            }],
        );
        assert!(matches!(
            apply_session_batch(&mut state.clone(), &invalid_digest, &empty_catalog()),
            Err(StateStoreError::ProjectionRejected)
        ));

        let state = state_after(&id, &prefix(&id, "turn-owner"));
        let wrong_owner = unchecked_batch(
            &state,
            &[SessionRolloutEvent::QuestionRequested {
                question_id: QuestionId("question-owner".to_owned()),
                session_id: session("another-session"),
                owner_turn_id: TurnId("turn-owner".to_owned()),
                prompt: "question".to_owned(),
                choices: vec![choice()],
            }],
        );
        assert!(matches!(
            apply_session_batch(&mut state.clone(), &wrong_owner, &empty_catalog()),
            Err(StateStoreError::StreamMismatch)
        ));

        let state = state_after(&id, &through_question(&id, "turn-terminal"));
        let terminal_with_pending = unchecked_batch(
            &state,
            &[SessionRolloutEvent::TurnCompleted {
                turn_id: TurnId("turn-terminal".to_owned()),
                status: TurnStatus::Succeeded,
            }],
        );
        assert!(matches!(
            apply_session_batch(&mut state.clone(), &terminal_with_pending, &empty_catalog(),),
            Err(StateStoreError::ProjectionRejected)
        ));

        let mut bad_sequence = unchecked_batch(
            &state,
            &[SessionRolloutEvent::TurnCancelRequested {
                turn_id: TurnId("turn-terminal".to_owned()),
            }],
        );
        bad_sequence.first_sequence += 1;
        bad_sequence.last_sequence += 1;
        bad_sequence.batch_checksum =
            session_batch_checksum(&bad_sequence).expect("recomputed checksum");
        assert!(matches!(
            apply_session_batch(&mut state.clone(), &bad_sequence, &empty_catalog()),
            Err(StateStoreError::SequenceMismatch)
        ));
    }

    #[test]
    fn terminal_facts_require_exact_approval_or_planner_evidence() {
        let id = session("session-terminal-evidence");
        let running = state_after(&id, &prefix(&id, "turn-direct"));
        for status in [
            TurnStatus::Succeeded,
            TurnStatus::Failed,
            TurnStatus::BudgetExceeded,
            TurnStatus::AbortedByRestart,
        ] {
            let batch = unchecked_batch(
                &running,
                &[SessionRolloutEvent::TurnCompleted {
                    turn_id: TurnId("turn-direct".to_owned()),
                    status,
                }],
            );
            assert!(matches!(
                apply_session_batch(&mut running.clone(), &batch, &empty_catalog()),
                Err(StateStoreError::ProjectionRejected)
            ));
        }

        let budget_event = SessionRolloutEvent::TurnBudgetExceeded {
            turn_id: TurnId("turn-direct".to_owned()),
        };
        let stored_budget = StoredSessionEventV1::from_live(&budget_event);
        assert_eq!(
            stored_budget
                .clone()
                .into_live()
                .expect("budget fact codec"),
            budget_event
        );
        let budget_batch = unchecked_batch(&running, std::slice::from_ref(&budget_event));
        let mut budget_state = running.clone();
        apply_session_batch(&mut budget_state, &budget_batch, &empty_catalog())
            .expect("authoritative budget fact");
        assert_eq!(
            budget_state
                .projection
                .snapshot()
                .expect("budget snapshot")
                .turns[0]
                .status,
            TurnStatus::BudgetExceeded
        );
        assert!(matches!(
            event_to_wire(running.last_sequence + 1, &budget_event).event,
            SessionEventKind::TurnCompleted {
                status: TurnStatus::BudgetExceeded,
                ..
            }
        ));

        for (decision, wrong_terminal) in [
            (ApprovalDecision::Approve, TurnStatus::Failed),
            (ApprovalDecision::Deny, TurnStatus::Succeeded),
        ] {
            let turn = format!("turn-wrong-{decision:?}");
            let state = state_after(&id, &through_approval(&id, &turn));
            let batch = unchecked_batch(
                &state,
                &[
                    SessionRolloutEvent::ApprovalResolved {
                        approval_id: ApprovalId(format!("approval-{turn}")),
                        approval_subject_digest: digest(&turn),
                        decision,
                        responder_client_id: ClientId("client".to_owned()),
                    },
                    SessionRolloutEvent::TurnCompleted {
                        turn_id: TurnId(turn),
                        status: wrong_terminal,
                    },
                ],
            );
            assert!(matches!(
                apply_session_batch(&mut state.clone(), &batch, &empty_catalog()),
                Err(StateStoreError::ProjectionRejected)
            ));
        }

        let mut forged_restart = unchecked_batch(
            &running,
            &[SessionRolloutEvent::TurnCompleted {
                turn_id: TurnId("turn-direct".to_owned()),
                status: TurnStatus::AbortedByRestart,
            }],
        );
        forged_restart.restart_authorization = Some(StoredRestartAuthorizationV1 {
            pre_head_sequence: running.last_sequence,
            turn_ids: vec!["turn-direct".to_owned()],
        });
        // 即使重新计算外层 checksum，该 transaction 也不是本 state-store instance 的
        // restart planner 所铸造。
        forged_restart.batch_checksum = session_batch_checksum(&forged_restart).expect("checksum");
        assert!(matches!(
            apply_session_batch(&mut running.clone(), &forged_restart, &empty_catalog()),
            Err(StateStoreError::ProjectionRejected)
        ));
    }

    #[test]
    fn approval_request_recomputes_subject_from_authoritative_context() {
        let id = session("session-subject-binding");
        let turn = "turn-subject";
        let mut answered = through_question(&id, turn);
        answered.push(SessionRolloutEvent::QuestionResolved {
            question_id: QuestionId(format!("question-{turn}")),
            choice_id: ChoiceId("bars_8".to_owned()),
            responder_client_id: ClientId("client".to_owned()),
        });
        let state = state_after(&id, &answered);
        let original_digest = digest(turn);

        let request = |inputs: ApprovalSubjectInputsV1, approval_subject_digest| {
            SessionRolloutEvent::ApprovalRequested {
                approval_id: ApprovalId(format!("approval-{turn}")),
                session_id: id.clone(),
                owner_turn_id: TurnId(turn.to_owned()),
                payload: payload(),
                subject_inputs: inputs,
                approval_subject_digest,
            }
        };

        let mut wrong_digest = original_digest.clone();
        wrong_digest.value = "b".repeat(64);
        for forged in [
            request(subject_inputs(), wrong_digest.clone()),
            request(
                ApprovalSubjectInputsV1::canonical("other-provider", ["prompt"]).expect("inputs"),
                original_digest.clone(),
            ),
            request(
                ApprovalSubjectInputsV1::canonical(
                    "fake-provider-fixture-v1",
                    ["constraints", "prompt"],
                )
                .expect("inputs"),
                original_digest.clone(),
            ),
            request(
                subject_inputs(),
                subject_inputs().digest(&TurnId(turn.to_owned()), "a different prompt"),
            ),
        ] {
            let batch = unchecked_batch(&state, &[forged]);
            assert!(matches!(
                apply_session_batch(&mut state.clone(), &batch, &empty_catalog()),
                Err(StateStoreError::ProjectionRejected)
            ));
        }

        let forged_pair = unchecked_batch(
            &state,
            &[
                request(subject_inputs(), wrong_digest.clone()),
                SessionRolloutEvent::ApprovalResolved {
                    approval_id: ApprovalId(format!("approval-{turn}")),
                    approval_subject_digest: wrong_digest,
                    decision: ApprovalDecision::Approve,
                    responder_client_id: ClientId("client".to_owned()),
                },
            ],
        );
        assert!(matches!(
            apply_session_batch(&mut state.clone(), &forged_pair, &empty_catalog()),
            Err(StateStoreError::ProjectionRejected)
        ));
    }

    #[test]
    fn newline_terminated_corruption_and_resource_bounds_fail_closed() {
        let root = tempfile::tempdir().expect("tempdir");
        make_private(&root);
        let id = session("session-corrupt-line");
        let store = open_store(&root);
        drop(append(
            ready(&store, &id),
            "start",
            None,
            vec![SessionRolloutEvent::SessionStarted {
                session_id: id.clone(),
                project_id: ProjectId("project-1".to_owned()),
            }],
        ));
        drop(store);
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(rollout_path(&root, &id))
            .expect("open rollout");
        file.write_all(b"{not-json}\n").expect("corrupt line");
        file.sync_all().expect("sync fixture");
        let store = open_store(&root);
        assert!(matches!(
            store.open_session_writer(id),
            Err(StateStoreError::MiddleCorruption)
        ));

        let id = session("session-bounds");
        let projection = state_after(
            &id,
            &[SessionRolloutEvent::SessionStarted {
                session_id: id.clone(),
                project_id: ProjectId("project-1".to_owned()),
            }],
        );
        let oversized = vec![
            SessionRolloutEvent::TurnStarted {
                turn_id: TurnId("turn".to_owned()),
                canonical_prompt: "p".to_owned(),
            };
            MAX_EVENTS + 1
        ];
        assert!(matches!(
            prepare_session_batch(
                &projection,
                SessionAppendRequest {
                    transaction_id: "too-many".to_owned(),
                    command_record: None,
                    events: oversized,
                    restart_authorization: None,
                    command_only_authorization: None,
                },
                &empty_catalog(),
            ),
            Err(StateStoreError::BatchTooLarge)
        ));
        assert!(matches!(
            StoredSessionEventV1::TurnStarted {
                turn_id: "turn".to_owned(),
                canonical_prompt: "x".repeat(MAX_PROMPT_BYTES + 1),
            }
            .into_live(),
            Err(StateStoreError::ProjectionRejected)
        ));
    }

    #[test]
    fn restart_reconciliation_batch_is_atomic_across_partial_and_synced_crashes() {
        for (failpoint, committed) in [
            (AppendFailpoint::PartialWrite(9), false),
            (AppendFailpoint::AfterSyncBeforeUpdate, true),
        ] {
            let root = tempfile::tempdir().expect("tempdir");
            make_private(&root);
            let id = session(&format!("session-restart-{committed}"));
            let store = open_store(&root);
            let writer = append(
                ready(&store, &id),
                "prefix",
                None,
                prefix(&id, "turn-runtime"),
            );
            let plan = plan_restart_reconciliation(store.instance_id(), writer.projection())
                .expect("planner")
                .expect("restart work");
            let stable_transaction = plan.transaction_id.clone();
            let mut writer = writer;
            writer.set_failpoint(failpoint);
            let poisoned = match writer.append(SessionAppendRequest {
                transaction_id: plan.transaction_id,
                command_record: None,
                events: plan.events,
                restart_authorization: plan.authorization,
                command_only_authorization: None,
            }) {
                Err(SessionAppendFailure::Poisoned { writer, .. }) => writer,
                _ => panic!("restart injected crash"),
            };
            match poisoned.recover() {
                SessionRecoveryOutcome::Ready(writer) => {
                    assert!(committed);
                    assert_eq!(
                        writer.snapshot().expect("snapshot").turns[0].status,
                        TurnStatus::AbortedByRestart
                    );
                    assert!(
                        plan_restart_reconciliation(store.instance_id(), writer.projection())
                            .expect("planner")
                            .is_none()
                    );
                    assert!(writer.state.transactions.contains_key(&stable_transaction));
                    writer.write_checkpoint().expect("restart checkpoint");
                    drop(writer);
                    let reopened = ready(&store, &id);
                    assert_eq!(
                        reopened.snapshot().expect("snapshot").turns[0].status,
                        TurnStatus::AbortedByRestart
                    );
                }
                SessionRecoveryOutcome::RepairRequired(repair) => {
                    assert!(!committed);
                    let writer = match repair.repair() {
                        Ok(writer) => writer,
                        Err(_) => panic!("repair"),
                    };
                    let retry =
                        plan_restart_reconciliation(store.instance_id(), writer.projection())
                            .expect("planner")
                            .expect("same restart work");
                    assert_eq!(retry.transaction_id, stable_transaction);
                    let result = writer.append(SessionAppendRequest {
                        transaction_id: retry.transaction_id,
                        command_record: None,
                        events: retry.events,
                        restart_authorization: retry.authorization,
                        command_only_authorization: None,
                    });
                    let writer = match result {
                        Ok((writer, _)) => writer,
                        Err(_) => panic!("retry restart append"),
                    };
                    assert_eq!(
                        writer.snapshot().expect("snapshot").turns[0].status,
                        TurnStatus::AbortedByRestart
                    );
                    writer.write_checkpoint().expect("restart checkpoint");
                    drop(writer);
                    let reopened = ready(&store, &id);
                    assert_eq!(
                        reopened.snapshot().expect("snapshot").turns[0].status,
                        TurnStatus::AbortedByRestart
                    );
                }
                SessionRecoveryOutcome::Corrupt(_) => panic!("restart batch is recoverable"),
            }
        }
    }

    #[test]
    fn published_session_read_state_replays_live_events_and_restart_authorization() {
        let root = tempfile::tempdir().expect("tempdir");
        make_private(&root);
        let id = session("session-published-restart");
        let store = open_store(&root);
        let writer = append(ready(&store, &id), "prefix", None, prefix(&id, "turn-1"));
        let plan = plan_restart_reconciliation(store.instance_id(), writer.projection())
            .expect("restart planner")
            .expect("restart work");
        let writer = match writer.append(plan.into_append_request()) {
            Ok((writer, _)) => writer,
            Err(_) => panic!("append restart"),
        };

        let published = writer.published_read_state().expect("published state");
        published.validate().expect("replay published state");
        assert_eq!(published.snapshot().session_id, id);
        assert_eq!(
            published.snapshot().turns[0].status,
            TurnStatus::AbortedByRestart
        );
        assert_eq!(published.head().0, 3);
    }

    #[test]
    fn published_session_read_state_rejects_event_projection_identity_head_checksum_and_owner_tampering()
     {
        let root = tempfile::tempdir().expect("tempdir");
        make_private(&root);
        let id = session("session-published-tamper");
        let store = open_store(&root);
        let writer = append(ready(&store, &id), "prefix", None, prefix(&id, "turn-1"));
        let published = writer.published_read_state().expect("published state");

        let mut event = published.clone();
        let SessionRolloutEvent::TurnStarted {
            canonical_prompt, ..
        } = &mut event.events[1]
        else {
            panic!("TurnStarted event");
        };
        canonical_prompt.push_str("-tampered");
        assert!(event.validate().is_err());

        let mut projection = published.clone();
        projection.projection.head_sequence -= 1;
        assert!(projection.validate().is_err());

        let mut identity = published.clone();
        identity.expected_session_id = session("session-other");
        assert!(identity.validate().is_err());

        let mut head = published.clone();
        head.last_sequence -= 1;
        assert!(head.validate().is_err());

        let mut checksum = published.clone();
        checksum.last_checksum = format!("sha256:{}", "f".repeat(64));
        assert!(checksum.validate().is_err());

        let mut owner = published;
        owner.snapshot.turns[0].turn_id = TurnId("turn-other".to_owned());
        assert!(owner.validate().is_err());
    }
}
