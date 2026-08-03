//! 私有 B4 control transaction log。
//!
//! 该日志同时承担 redo WAL、全局命令索引与 Project/Session catalog；
//! 其中有意只保存 primitive plan，而不保存 live capability。

#![allow(
    dead_code,
    reason = "B4b1 freezes the control/runtime API before B4c AppService wiring"
)]
#![allow(
    clippy::missing_errors_doc,
    clippy::large_enum_variant,
    clippy::result_large_err,
    clippy::too_many_lines,
    reason = "保留 writer 所有权的中毒恢复状态需要统一的类型化错误边界"
)]

#[cfg(test)]
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path};
use std::sync::Weak;

use rand::RngCore as _;
use rustix::fs::{CWD, Mode, OFlags, fstat, fsync, mkdirat, openat, renameat};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::artifact_store::ArtifactAuditPlanV1;
use crate::domain::DomainProjectId;
use crate::durable_runtime::{DurableRuntimeError, LockHealth};
use crate::protocol::{ProjectId, SessionId};
use crate::state_store::session::{INTERNAL_CLIENT_PREFIX, INTERNAL_RESTART_CLIENT_ID};
use crate::state_store::{
    StateStoreError, StoredCommandRecordV1, StoredProjectPlanV1, StoredSessionPlanV1,
    TransactionCommit, validate_sha256,
};

const STATE_LAYOUT: &str = "state-v1";
const CONTROL_DIRECTORY: &str = "control";
const CONTROL_LOG: &str = "control-v1.jsonl";
const CONTROL_CHECKPOINT: &str = "control-checkpoint-v1.json";
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_CHECKPOINT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CONTROL_FACTS: usize = 4;
const MAX_CATALOG_ENTRIES: usize = 100_000;
pub(crate) const MAX_EXTERNAL_PREPARED: usize = 10_000;
pub(crate) const MAX_INTERNAL_RESTART_PREPARED: usize = 10_000;
pub(crate) const MAX_TOTAL_PREPARED: usize = 20_000;
const DIRECTORY_MODE: Mode = Mode::from_raw_mode(0o700);
const FILE_MODE: Mode = Mode::from_raw_mode(0o600);
const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

#[cfg(test)]
thread_local! {
    static CHECKPOINT_LOAD_OBSERVED: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn reset_checkpoint_load_observed() {
    CHECKPOINT_LOAD_OBSERVED.set(false);
}

#[cfg(test)]
pub(crate) fn checkpoint_load_observed() -> bool {
    CHECKPOINT_LOAD_OBSERVED.get()
}

#[derive(Debug, Error)]
pub(crate) enum ControlStoreError {
    #[error("control Store root or entry is unsafe")]
    UnsafeRoot,
    #[error("control schema is incompatible")]
    IncompatibleSchema,
    #[error("control checksum mismatch")]
    ChecksumMismatch,
    #[error("control checksum chain mismatch")]
    ChecksumChainMismatch,
    #[error("control committed area is corrupt")]
    MiddleCorruption,
    #[error("control command or transaction conflicts with durable facts")]
    IdempotencyConflict,
    #[error("control catalog does not match the redo plan")]
    CatalogMismatch,
    #[error("control log exceeds a resource limit")]
    ResourceLimit,
    #[error("control writer is poisoned")]
    WriterPoisoned,
    #[error("control final tail requires repair")]
    RecoverableIncompleteTail {
        valid_bytes: u64,
        damaged_bytes: u64,
    },
    #[error("durable instance lock is not live")]
    LockUnavailable,
    #[error("stored aggregate plan is invalid")]
    InvalidAggregatePlan,
    #[error("control filesystem operation failed: {operation}")]
    Io {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
}

impl ControlStoreError {
    pub(crate) const fn is_capability_loss(&self) -> bool {
        matches!(self, Self::LockUnavailable | Self::WriterPoisoned)
    }
}

impl From<StateStoreError> for ControlStoreError {
    fn from(_: StateStoreError) -> Self {
        Self::InvalidAggregatePlan
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AggregateCommitV1 {
    pub resulting_last_sequence: u64,
    pub resulting_batch_checksum: String,
}

impl From<TransactionCommit> for AggregateCommitV1 {
    fn from(value: TransactionCommit) -> Self {
        Self {
            resulting_last_sequence: value.resulting_last_sequence,
            resulting_batch_checksum: value.resulting_batch_checksum,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparedTransactionV1 {
    schema_version: u32,
    pub global_tx_id: String,
    pub command_record: StoredCommandRecordV1,
    pub project_plan: Option<StoredProjectPlanV1>,
    pub session_plan: Option<StoredSessionPlanV1>,
    pub artifact_audit_plans: Vec<ArtifactAuditPlanV1>,
    prepared_digest: String,
}

impl PreparedTransactionV1 {
    pub(crate) fn new(
        global_tx_id: String,
        command_record: StoredCommandRecordV1,
        project_plan: Option<StoredProjectPlanV1>,
        session_plan: Option<StoredSessionPlanV1>,
        artifact_audit_plans: Vec<ArtifactAuditPlanV1>,
    ) -> Result<Self, ControlStoreError> {
        let mut prepared = Self {
            schema_version: 1,
            global_tx_id,
            command_record,
            project_plan,
            session_plan,
            artifact_audit_plans,
            prepared_digest: String::new(),
        };
        prepared.prepared_digest = prepared_digest(&prepared)?;
        prepared.validate()?;
        Ok(prepared)
    }

    pub(crate) fn validate(&self) -> Result<(), ControlStoreError> {
        if self.schema_version != 1 || !is_global_tx_id(&self.global_tx_id) {
            return Err(ControlStoreError::IncompatibleSchema);
        }
        self.command_record.decode_reply()?;
        if self.project_plan.is_none() && self.session_plan.is_none() {
            return Err(ControlStoreError::InvalidAggregatePlan);
        }
        let project_transaction = project_transaction_id(&self.global_tx_id);
        let session_transaction = session_transaction_id(&self.global_tx_id);
        let mut registered_artifacts = 0_usize;
        if let Some(plan) = &self.project_plan {
            plan.validate()?;
            if plan.transaction_id() != project_transaction
                || plan.command_record() != Some(&self.command_record)
            {
                return Err(ControlStoreError::InvalidAggregatePlan);
            }
            registered_artifacts = plan.registered_artifact_events().len();
        }
        if let Some(plan) = &self.session_plan {
            plan.validate()?;
            if plan.transaction_id() != session_transaction
                || plan.command_record() != Some(&self.command_record)
            {
                return Err(ControlStoreError::InvalidAggregatePlan);
            }
        }
        if registered_artifacts != self.artifact_audit_plans.len() {
            return Err(ControlStoreError::InvalidAggregatePlan);
        }
        for audit in &self.artifact_audit_plans {
            audit
                .validate_for_control(&self.global_tx_id)
                .map_err(|_| ControlStoreError::InvalidAggregatePlan)?;
        }
        self.kind()?;
        if self.prepared_digest != prepared_digest(self)? {
            return Err(ControlStoreError::ChecksumMismatch);
        }
        Ok(())
    }

    pub(crate) fn stable_reply(&self) -> Result<Vec<u8>, ControlStoreError> {
        self.command_record.decode_reply().map_err(Into::into)
    }

    fn kind(&self) -> Result<PreparedKind, ControlStoreError> {
        let client_id = self.command_record.client_id.as_str();
        if !client_id.starts_with(INTERNAL_CLIENT_PREFIX) {
            return Ok(PreparedKind::External);
        }
        if client_id != INTERNAL_RESTART_CLIENT_ID
            || self.project_plan.is_some()
            || !self.artifact_audit_plans.is_empty()
        {
            return Err(ControlStoreError::InvalidAggregatePlan);
        }
        let session_plan = self
            .session_plan
            .as_ref()
            .ok_or(ControlStoreError::InvalidAggregatePlan)?;
        if !session_plan.validate_coordinated_restart_identity()? {
            return Err(ControlStoreError::InvalidAggregatePlan);
        }
        Ok(PreparedKind::InternalRestart)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum StoredControlFactV1 {
    ProjectAllocated {
        project_id: String,
    },
    SessionAllocated {
        session_id: String,
        project_id: String,
    },
    CommandPreparedV1(Box<PreparedTransactionV1>),
    CommandCommittedV1 {
        global_tx_id: String,
        project_last: Option<AggregateCommitV1>,
        session_last: Option<AggregateCommitV1>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredControlBatchV1 {
    schema_version: u32,
    batch_sequence: u64,
    facts: Vec<StoredControlFactV1>,
    previous_batch_checksum: Option<String>,
    batch_checksum: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionAllocation {
    pub session_id: SessionId,
    pub project_id: ProjectId,
}

/// 只读 Session allocation catalog，只能从已验证的 control writer 投影导出。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionAllocationCatalogContext {
    allocations: BTreeMap<String, String>,
}

impl SessionAllocationCatalogContext {
    pub(crate) fn project_id(&self, session_id: &SessionId) -> Option<&str> {
        self.allocations.get(&session_id.0).map(String::as_str)
    }

    #[cfg(test)]
    pub(crate) fn for_test(allocations: impl IntoIterator<Item = (SessionId, ProjectId)>) -> Self {
        Self {
            allocations: allocations
                .into_iter()
                .map(|(session_id, project_id)| (session_id.0, project_id.0))
                .collect(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PrepareControlRequest {
    pub project_allocation: Option<DomainProjectId>,
    pub session_allocation: Option<SessionAllocation>,
    pub prepared: PreparedTransactionV1,
}

#[derive(Clone, Debug)]
pub(crate) struct CommitControlRequest {
    pub global_tx_id: String,
    pub project_last: Option<AggregateCommitV1>,
    pub session_last: Option<AggregateCommitV1>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ControlAppendOutcome {
    pub appended: bool,
    pub stable_reply: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ControlProjection {
    pub projects: BTreeSet<String>,
    pub sessions: BTreeMap<String, String>,
    #[serde(with = "stored_global_command_map")]
    pub commands: BTreeMap<(String, String), GlobalCommandRecord>,
    pub prepared: BTreeMap<String, PreparedTransactionV1>,
    #[serde(default)]
    pub prepared_order: Vec<String>,
    pub committed: BTreeMap<String, CommittedTransactionV1>,
    #[serde(default)]
    external_prepared_count: usize,
    #[serde(default)]
    internal_restart_prepared_count: usize,
    pub last_batch_sequence: u64,
    pub last_batch_checksum: Option<String>,
    pub valid_bytes: u64,
    #[cfg(test)]
    #[serde(skip)]
    startup_test_capacity: Option<ControlCapacity>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ControlCapacity {
    pub external: usize,
    pub internal_restart: usize,
    pub total: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreparedKind {
    External,
    InternalRestart,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GlobalCommandRecord {
    pub global_tx_id: String,
    pub command_record: StoredCommandRecordV1,
}

mod stored_global_command_map {
    use std::collections::BTreeMap;

    use serde::de::Error as _;
    use serde::ser::SerializeSeq as _;
    use serde::{Deserialize, Deserializer, Serializer};

    use super::GlobalCommandRecord;

    pub(super) fn serialize<S>(
        commands: &BTreeMap<(String, String), GlobalCommandRecord>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(commands.len()))?;
        for command in commands.values() {
            sequence.serialize_element(command)?;
        }
        sequence.end()
    }

    pub(super) fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<BTreeMap<(String, String), GlobalCommandRecord>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let entries = Vec::<GlobalCommandRecord>::deserialize(deserializer)?;
        let mut commands = BTreeMap::new();
        for command in entries {
            let key = (
                command.command_record.client_id.clone(),
                command.command_record.client_command_id.clone(),
            );
            if commands.insert(key, command).is_some() {
                return Err(D::Error::custom("duplicate global command key"));
            }
        }
        Ok(commands)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommittedTransactionV1 {
    pub project_last: Option<AggregateCommitV1>,
    pub session_last: Option<AggregateCommitV1>,
}

impl ControlProjection {
    pub(crate) fn pending(&self) -> Vec<PreparedTransactionV1> {
        self.prepared_order
            .iter()
            .filter(|tx| !self.committed.contains_key(*tx))
            .filter_map(|tx| self.prepared.get(tx).cloned())
            .collect()
    }

    pub(crate) fn capacity(&self) -> ControlCapacity {
        #[cfg(test)]
        if let Some(capacity) = self.startup_test_capacity {
            return capacity;
        }
        ControlCapacity {
            external: self.external_prepared_count,
            internal_restart: self.internal_restart_prepared_count,
            total: self.prepared.len(),
        }
    }

    pub(crate) fn require_internal_restart_capacity(
        &self,
        additional: usize,
    ) -> Result<(), ControlStoreError> {
        let capacity = self.capacity();
        if capacity
            .internal_restart
            .checked_add(additional)
            .is_none_or(|count| count > MAX_INTERNAL_RESTART_PREPARED)
            || capacity
                .total
                .checked_add(additional)
                .is_none_or(|count| count > MAX_TOTAL_PREPARED)
        {
            return Err(ControlStoreError::ResourceLimit);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredControlCheckpointV1 {
    schema_version: u32,
    covered_valid_bytes: u64,
    covered_batch_sequence: u64,
    covered_batch_checksum: Option<String>,
    projection: ControlProjection,
    checksum: String,
}

pub(crate) enum OpenControlWriter {
    Ready(ReadyControlWriter),
    RepairRequired(RepairRequiredControlWriter),
}

pub(crate) struct ReadyControlWriter {
    control_dir: OwnedFd,
    file: File,
    state: ControlProjection,
    lock_health: Weak<LockHealth>,
    #[cfg(test)]
    failpoint: Option<ControlAppendFailpoint>,
    #[cfg(test)]
    checkpoint_failpoint: Option<ControlCheckpointFailpoint>,
}

pub(crate) enum ControlAppendFailure {
    Rejected {
        writer: ReadyControlWriter,
        error: ControlStoreError,
    },
    Poisoned {
        writer: PoisonedControlWriter,
        error: ControlStoreError,
    },
}

pub(crate) struct PoisonedControlWriter {
    control_dir: OwnedFd,
    lock_health: Weak<LockHealth>,
    #[cfg(test)]
    recovery_failpoint: Option<ControlRecoveryFailpoint>,
}

pub(crate) enum ControlRecoveryOutcome {
    Ready(ReadyControlWriter),
    RepairRequired(RepairRequiredControlWriter),
    Corrupt(CorruptControlWriter),
}

pub(crate) struct RepairRequiredControlWriter {
    control_dir: OwnedFd,
    lock_health: Weak<LockHealth>,
    valid_bytes: u64,
    damaged_bytes: u64,
    tail_digest: String,
    #[cfg(test)]
    failpoint: Option<ControlRepairFailpoint>,
}

pub(crate) struct CorruptControlWriter {
    _control_dir: OwnedFd,
    _lock_health: Weak<LockHealth>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ControlAppendFailpoint {
    BeforeWrite,
    PartialWrite(usize),
    AfterNewlineBeforeSync,
    FileSync,
    AfterSync,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlCheckpointFailpoint {
    TempCreate,
    TempWrite,
    FileSync,
    BeforeInstall,
    AfterInstall,
    DirectorySync,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlRepairFailpoint {
    RescanRace,
    Truncate,
    FileSync,
    DirectorySync,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ControlRecoveryFailpoint {
    FileSync,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ControlOpenFailpoint {
    FileSync,
}

pub(crate) fn open_control_writer(
    root_path: &Path,
    lock_health: Weak<LockHealth>,
) -> Result<OpenControlWriter, ControlStoreError> {
    open_control_writer_inner(
        root_path,
        lock_health,
        #[cfg(test)]
        None,
    )
}

#[cfg(test)]
pub(crate) fn open_control_writer_with_failpoint(
    root_path: &Path,
    lock_health: Weak<LockHealth>,
    failpoint: ControlOpenFailpoint,
) -> Result<OpenControlWriter, ControlStoreError> {
    open_control_writer_inner(root_path, lock_health, Some(failpoint))
}

fn open_control_writer_inner(
    root_path: &Path,
    lock_health: Weak<LockHealth>,
    #[cfg(test)] open_failpoint: Option<ControlOpenFailpoint>,
) -> Result<OpenControlWriter, ControlStoreError> {
    require_lock(&lock_health)?;
    let root = open_absolute_directory(root_path)?;
    validate_directory(&root, true)?;
    let layout = open_directory(&root, STATE_LAYOUT)?;
    let control_dir = ensure_directory(&layout, CONTROL_DIRECTORY)?;
    let mut file = open_or_create_control_log(
        &control_dir,
        #[cfg(test)]
        open_failpoint,
    )?;
    let scan = match load_control_checkpoint(&control_dir, &mut file) {
        Ok(Some(state)) => {
            #[cfg(test)]
            CHECKPOINT_LOAD_OBSERVED.set(true);
            scan_control_log_from(&mut file, state)?
        }
        Ok(None) | Err(_) => scan_control_log(&mut file)?,
    };
    match scan {
        ControlScanOutcome::Clean(state) => {
            file.seek(SeekFrom::End(0))
                .map_err(|source| control_io("seek control log end", source))?;
            Ok(OpenControlWriter::Ready(ReadyControlWriter {
                control_dir,
                file,
                state,
                lock_health,
                #[cfg(test)]
                failpoint: None,
                #[cfg(test)]
                checkpoint_failpoint: None,
            }))
        }
        ControlScanOutcome::Incomplete {
            state,
            damaged_bytes,
            tail_digest,
        } => Ok(OpenControlWriter::RepairRequired(
            RepairRequiredControlWriter {
                control_dir,
                lock_health,
                valid_bytes: state.valid_bytes,
                damaged_bytes,
                tail_digest,
                #[cfg(test)]
                failpoint: None,
            },
        )),
    }
}

impl ReadyControlWriter {
    pub(crate) fn projection(&self) -> &ControlProjection {
        &self.state
    }

    pub(crate) fn session_allocation_catalog_context(&self) -> SessionAllocationCatalogContext {
        SessionAllocationCatalogContext {
            allocations: self.state.sessions.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_startup_test_capacity(&mut self, capacity: ControlCapacity) {
        self.state.startup_test_capacity = Some(capacity);
    }

    pub(crate) fn prepare(
        self,
        request: PrepareControlRequest,
    ) -> Result<(Self, ControlAppendOutcome), ControlAppendFailure> {
        if let Err(error) = request.prepared.validate() {
            return Err(ControlAppendFailure::Rejected {
                writer: self,
                error,
            });
        }
        let key = (
            request.prepared.command_record.client_id.clone(),
            request.prepared.command_record.client_command_id.clone(),
        );
        if let Some(existing) = self.state.commands.get(&key) {
            if existing.command_record.payload_digest
                != request.prepared.command_record.payload_digest
            {
                return Err(ControlAppendFailure::Rejected {
                    writer: self,
                    error: ControlStoreError::IdempotencyConflict,
                });
            }
            let reply = match existing.command_record.decode_reply() {
                Ok(reply) => reply,
                Err(error) => {
                    return Err(ControlAppendFailure::Rejected {
                        writer: self,
                        error: error.into(),
                    });
                }
            };
            return Ok((
                self,
                ControlAppendOutcome {
                    appended: false,
                    stable_reply: Some(reply),
                },
            ));
        }

        let mut facts = Vec::with_capacity(3);
        if let Some(project_id) = &request.project_allocation {
            facts.push(StoredControlFactV1::ProjectAllocated {
                project_id: project_id.as_str().to_owned(),
            });
        }
        if let Some(allocation) = &request.session_allocation {
            facts.push(StoredControlFactV1::SessionAllocated {
                session_id: allocation.session_id.0.clone(),
                project_id: allocation.project_id.0.clone(),
            });
        }
        facts.push(StoredControlFactV1::CommandPreparedV1(Box::new(
            request.prepared,
        )));
        self.append_facts(facts, true)
    }

    pub(crate) fn commit(
        self,
        request: CommitControlRequest,
    ) -> Result<(Self, ControlAppendOutcome), ControlAppendFailure> {
        if let Some(existing) = self.state.committed.get(&request.global_tx_id) {
            if existing.project_last.as_ref() == request.project_last.as_ref()
                && existing.session_last.as_ref() == request.session_last.as_ref()
            {
                return Ok((
                    self,
                    ControlAppendOutcome {
                        appended: false,
                        stable_reply: None,
                    },
                ));
            }
            return Err(ControlAppendFailure::Rejected {
                writer: self,
                error: ControlStoreError::IdempotencyConflict,
            });
        }
        self.append_facts(
            vec![StoredControlFactV1::CommandCommittedV1 {
                global_tx_id: request.global_tx_id,
                project_last: request.project_last,
                session_last: request.session_last,
            }],
            false,
        )
    }

    fn append_facts(
        mut self,
        facts: Vec<StoredControlFactV1>,
        include_reply: bool,
    ) -> Result<(Self, ControlAppendOutcome), ControlAppendFailure> {
        if let Err(error) = require_lock(&self.lock_health) {
            return Err(ControlAppendFailure::Rejected {
                writer: self,
                error,
            });
        }
        let mut next = self.state.clone();
        let mut batch = StoredControlBatchV1 {
            schema_version: 1,
            batch_sequence: match next.last_batch_sequence.checked_add(1) {
                Some(sequence) => sequence,
                None => {
                    return Err(ControlAppendFailure::Rejected {
                        writer: self,
                        error: ControlStoreError::ResourceLimit,
                    });
                }
            },
            facts,
            previous_batch_checksum: next.last_batch_checksum.clone(),
            batch_checksum: String::new(),
        };
        batch.batch_checksum = match control_batch_checksum(&batch) {
            Ok(checksum) => checksum,
            Err(error) => {
                return Err(ControlAppendFailure::Rejected {
                    writer: self,
                    error,
                });
            }
        };
        if let Err(error) = apply_control_batch(&mut next, &batch) {
            return Err(ControlAppendFailure::Rejected {
                writer: self,
                error,
            });
        }
        let stable_reply = if include_reply {
            batch.facts.iter().find_map(|fact| match fact {
                StoredControlFactV1::CommandPreparedV1(prepared) => Some(prepared.stable_reply()),
                _ => None,
            })
        } else {
            None
        };
        let stable_reply = match stable_reply.transpose() {
            Ok(reply) => reply,
            Err(error) => {
                return Err(ControlAppendFailure::Rejected {
                    writer: self,
                    error,
                });
            }
        };
        let Ok(mut bytes) = serde_json::to_vec(&batch) else {
            return Err(ControlAppendFailure::Rejected {
                writer: self,
                error: ControlStoreError::IncompatibleSchema,
            });
        };
        bytes.push(b'\n');
        if bytes.len() > MAX_LINE_BYTES {
            return Err(ControlAppendFailure::Rejected {
                writer: self,
                error: ControlStoreError::ResourceLimit,
            });
        }
        let Ok(line_len) = u64::try_from(bytes.len()) else {
            return Err(ControlAppendFailure::Rejected {
                writer: self,
                error: ControlStoreError::ResourceLimit,
            });
        };
        next.valid_bytes = match next.valid_bytes.checked_add(line_len) {
            Some(length) => length,
            None => {
                return Err(ControlAppendFailure::Rejected {
                    writer: self,
                    error: ControlStoreError::ResourceLimit,
                });
            }
        };

        #[cfg(test)]
        if self.failpoint == Some(ControlAppendFailpoint::BeforeWrite) {
            return Err(ControlAppendFailure::Rejected {
                writer: self,
                error: control_io(
                    "test before control write",
                    std::io::Error::other("injected"),
                ),
            });
        }
        #[cfg(test)]
        if let Some(ControlAppendFailpoint::PartialWrite(count)) = self.failpoint {
            let limit = count.min(bytes.len());
            let _ignored = self.file.write_all(&bytes[..limit]);
            return Err(ControlAppendFailure::Poisoned {
                writer: self.into_poisoned(),
                error: control_io(
                    "test partial control write",
                    std::io::Error::other("injected"),
                ),
            });
        }
        if let Err(source) = self.file.write_all(&bytes) {
            return Err(ControlAppendFailure::Poisoned {
                writer: self.into_poisoned(),
                error: control_io("append control batch", source),
            });
        }
        #[cfg(test)]
        if self.failpoint == Some(ControlAppendFailpoint::AfterNewlineBeforeSync) {
            return Err(ControlAppendFailure::Poisoned {
                writer: self.into_poisoned(),
                error: control_io(
                    "test after control newline",
                    std::io::Error::other("injected"),
                ),
            });
        }
        if let Err(source) = self.file.flush() {
            return Err(ControlAppendFailure::Poisoned {
                writer: self.into_poisoned(),
                error: control_io("flush control batch", source),
            });
        }
        #[cfg(test)]
        if self.failpoint == Some(ControlAppendFailpoint::FileSync) {
            return Err(ControlAppendFailure::Poisoned {
                writer: self.into_poisoned(),
                error: control_io("test control file sync", std::io::Error::other("injected")),
            });
        }
        if let Err(source) = self.file.sync_all() {
            return Err(ControlAppendFailure::Poisoned {
                writer: self.into_poisoned(),
                error: control_io("sync control batch", source),
            });
        }
        #[cfg(test)]
        if self.failpoint == Some(ControlAppendFailpoint::AfterSync) {
            return Err(ControlAppendFailure::Poisoned {
                writer: self.into_poisoned(),
                error: control_io("test after control sync", std::io::Error::other("injected")),
            });
        }
        self.state = next;
        Ok((
            self,
            ControlAppendOutcome {
                appended: true,
                stable_reply,
            },
        ))
    }

    fn into_poisoned(self) -> PoisonedControlWriter {
        PoisonedControlWriter {
            control_dir: self.control_dir,
            lock_health: self.lock_health,
            #[cfg(test)]
            recovery_failpoint: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn set_failpoint(&mut self, failpoint: ControlAppendFailpoint) {
        self.failpoint = Some(failpoint);
    }

    #[cfg(test)]
    fn set_checkpoint_failpoint(&mut self, failpoint: ControlCheckpointFailpoint) {
        self.checkpoint_failpoint = Some(failpoint);
    }

    pub(crate) fn write_checkpoint(&self) -> Result<(), ControlStoreError> {
        require_lock(&self.lock_health)?;
        let mut checkpoint = StoredControlCheckpointV1 {
            schema_version: 1,
            covered_valid_bytes: self.state.valid_bytes,
            covered_batch_sequence: self.state.last_batch_sequence,
            covered_batch_checksum: self.state.last_batch_checksum.clone(),
            projection: self.state.clone(),
            checksum: String::new(),
        };
        checkpoint.checksum = control_checkpoint_checksum(&checkpoint)?;
        let bytes =
            serde_json::to_vec(&checkpoint).map_err(|_| ControlStoreError::IncompatibleSchema)?;
        if u64::try_from(bytes.len()).map_or(true, |length| length > MAX_CHECKPOINT_BYTES) {
            return Err(ControlStoreError::ResourceLimit);
        }
        let temp_name = format!("control-checkpoint-{}.tmp", random_hex_128());
        #[cfg(test)]
        inject_checkpoint(
            self.checkpoint_failpoint,
            ControlCheckpointFailpoint::TempCreate,
            "test control checkpoint create",
        )?;
        let fd = openat(
            &self.control_dir,
            temp_name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            FILE_MODE,
        )
        .map_err(|source| control_io("create control checkpoint", source))?;
        let mut file = File::from(fd);
        #[cfg(test)]
        inject_checkpoint(
            self.checkpoint_failpoint,
            ControlCheckpointFailpoint::TempWrite,
            "test control checkpoint write",
        )?;
        file.write_all(&bytes)
            .map_err(|source| control_io("write control checkpoint", source))?;
        #[cfg(test)]
        inject_checkpoint(
            self.checkpoint_failpoint,
            ControlCheckpointFailpoint::FileSync,
            "test control checkpoint file sync",
        )?;
        file.sync_all()
            .map_err(|source| control_io("sync control checkpoint", source))?;
        #[cfg(test)]
        inject_checkpoint(
            self.checkpoint_failpoint,
            ControlCheckpointFailpoint::BeforeInstall,
            "test control checkpoint install",
        )?;
        renameat(
            &self.control_dir,
            temp_name.as_str(),
            &self.control_dir,
            CONTROL_CHECKPOINT,
        )
        .map_err(|source| control_io("install control checkpoint", source))?;
        #[cfg(test)]
        inject_checkpoint(
            self.checkpoint_failpoint,
            ControlCheckpointFailpoint::AfterInstall,
            "test after control checkpoint install",
        )?;
        #[cfg(test)]
        inject_checkpoint(
            self.checkpoint_failpoint,
            ControlCheckpointFailpoint::DirectorySync,
            "test control checkpoint directory sync",
        )?;
        fsync(&self.control_dir).map_err(|source| control_io("sync control directory", source))
    }
}

#[cfg(test)]
fn fixture_prepare_facts(request: PrepareControlRequest) -> Vec<StoredControlFactV1> {
    let mut facts = Vec::with_capacity(3);
    if let Some(project_id) = request.project_allocation {
        facts.push(StoredControlFactV1::ProjectAllocated {
            project_id: project_id.as_str().to_owned(),
        });
    }
    if let Some(allocation) = request.session_allocation {
        facts.push(StoredControlFactV1::SessionAllocated {
            session_id: allocation.session_id.0,
            project_id: allocation.project_id.0,
        });
    }
    facts.push(StoredControlFactV1::CommandPreparedV1(Box::new(
        request.prepared,
    )));
    facts
}

#[cfg(test)]
fn fixture_batch(
    state: &ControlProjection,
    facts: Vec<StoredControlFactV1>,
) -> Result<StoredControlBatchV1, ControlStoreError> {
    let mut batch = StoredControlBatchV1 {
        schema_version: 1,
        batch_sequence: state
            .last_batch_sequence
            .checked_add(1)
            .ok_or(ControlStoreError::ResourceLimit)?,
        facts,
        previous_batch_checksum: state.last_batch_checksum.clone(),
        batch_checksum: String::new(),
    };
    batch.batch_checksum = control_batch_checksum(&batch)?;
    Ok(batch)
}

#[cfg(test)]
fn write_fixture_batch(
    writer: &mut ReadyControlWriter,
    batch: &StoredControlBatchV1,
) -> Result<(), ControlStoreError> {
    let mut bytes = serde_json::to_vec(batch).map_err(|_| ControlStoreError::IncompatibleSchema)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_LINE_BYTES {
        return Err(ControlStoreError::ResourceLimit);
    }
    writer
        .file
        .write_all(&bytes)
        .map_err(|source| control_io("write validated control fixture", source))?;
    writer.state.valid_bytes = writer
        .state
        .valid_bytes
        .checked_add(u64::try_from(bytes.len()).map_err(|_| ControlStoreError::ResourceLimit)?)
        .ok_or(ControlStoreError::ResourceLimit)?;
    Ok(())
}

/// 批量生成真实 control JSONL fixture；每条记录仍经过 production reducer 全量校验。
#[cfg(test)]
pub(crate) fn append_validated_control_fixture(
    mut writer: ReadyControlWriter,
    requests: impl IntoIterator<Item = PrepareControlRequest>,
    committed: bool,
) -> Result<ReadyControlWriter, ControlStoreError> {
    require_lock(&writer.lock_health)?;
    for request in requests {
        let prepared = request.prepared.clone();
        let batch = fixture_batch(&writer.state, fixture_prepare_facts(request))?;
        apply_control_batch(&mut writer.state, &batch)?;
        write_fixture_batch(&mut writer, &batch)?;

        if committed {
            let anchor = || AggregateCommitV1 {
                resulting_last_sequence: 1,
                resulting_batch_checksum: format!("sha256:{}", "a".repeat(64)),
            };
            let facts = vec![StoredControlFactV1::CommandCommittedV1 {
                global_tx_id: prepared.global_tx_id,
                project_last: prepared.project_plan.is_some().then(anchor),
                session_last: prepared.session_plan.is_some().then(anchor),
            }];
            let batch = fixture_batch(&writer.state, facts)?;
            apply_control_batch(&mut writer.state, &batch)?;
            write_fixture_batch(&mut writer, &batch)?;
        }
    }
    writer
        .file
        .flush()
        .map_err(|source| control_io("flush validated control fixture", source))?;
    writer
        .file
        .sync_all()
        .map_err(|source| control_io("sync validated control fixture", source))?;
    Ok(writer)
}

/// 在合法边界后追加 checksum-chain 正确但容量超限的一条 replay 负例。
#[cfg(test)]
pub(crate) fn append_canonical_over_limit_fixture_tail(
    mut writer: ReadyControlWriter,
    request: PrepareControlRequest,
) -> Result<(), ControlStoreError> {
    require_lock(&writer.lock_health)?;
    request.prepared.validate()?;
    let batch = fixture_batch(&writer.state, fixture_prepare_facts(request))?;
    write_fixture_batch(&mut writer, &batch)?;
    writer
        .file
        .flush()
        .map_err(|source| control_io("flush over-limit control fixture", source))?;
    writer
        .file
        .sync_all()
        .map_err(|source| control_io("sync over-limit control fixture", source))
}

impl PoisonedControlWriter {
    #[cfg(test)]
    pub(crate) fn set_recovery_failpoint(&mut self, failpoint: ControlRecoveryFailpoint) {
        self.recovery_failpoint = Some(failpoint);
    }

    pub(crate) fn recover(self) -> ControlRecoveryOutcome {
        if require_lock(&self.lock_health).is_err() {
            return ControlRecoveryOutcome::Corrupt(CorruptControlWriter {
                _control_dir: self.control_dir,
                _lock_health: self.lock_health,
            });
        }
        recover_control(
            self.control_dir,
            self.lock_health,
            #[cfg(test)]
            self.recovery_failpoint,
        )
    }
}

impl RepairRequiredControlWriter {
    #[cfg(test)]
    fn set_failpoint(&mut self, failpoint: ControlRepairFailpoint) {
        self.failpoint = Some(failpoint);
    }

    pub(crate) fn repair(self) -> Result<ReadyControlWriter, CorruptControlWriter> {
        if require_lock(&self.lock_health).is_err() {
            return Err(self.into_corrupt());
        }
        let Ok(mut file) = open_control_log(&self.control_dir) else {
            return Err(self.into_corrupt());
        };
        #[cfg(test)]
        if self.failpoint == Some(ControlRepairFailpoint::RescanRace)
            && file
                .seek(SeekFrom::End(0))
                .and_then(|_| file.write_all(b"race"))
                .is_err()
        {
            return Err(self.into_corrupt());
        }
        let state = match scan_control_log(&mut file) {
            Ok(ControlScanOutcome::Incomplete {
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
        if self.failpoint == Some(ControlRepairFailpoint::Truncate) {
            return Err(self.into_corrupt());
        }
        if file.set_len(self.valid_bytes).is_err() {
            return Err(self.into_corrupt());
        }
        #[cfg(test)]
        if self.failpoint == Some(ControlRepairFailpoint::FileSync) {
            return Err(self.into_corrupt());
        }
        if file.sync_all().is_err() {
            return Err(self.into_corrupt());
        }
        #[cfg(test)]
        if self.failpoint == Some(ControlRepairFailpoint::DirectorySync) {
            return Err(self.into_corrupt());
        }
        if fsync(&self.control_dir).is_err() || file.seek(SeekFrom::End(0)).is_err() {
            return Err(self.into_corrupt());
        }
        Ok(ReadyControlWriter {
            control_dir: self.control_dir,
            file,
            state,
            lock_health: self.lock_health,
            #[cfg(test)]
            failpoint: None,
            #[cfg(test)]
            checkpoint_failpoint: None,
        })
    }

    fn into_corrupt(self) -> CorruptControlWriter {
        CorruptControlWriter {
            _control_dir: self.control_dir,
            _lock_health: self.lock_health,
        }
    }
}

fn recover_control(
    control_dir: OwnedFd,
    lock_health: Weak<LockHealth>,
    #[cfg(test)] recovery_failpoint: Option<ControlRecoveryFailpoint>,
) -> ControlRecoveryOutcome {
    let Ok(mut file) = open_control_log(&control_dir) else {
        return ControlRecoveryOutcome::Corrupt(CorruptControlWriter {
            _control_dir: control_dir,
            _lock_health: lock_health,
        });
    };
    match scan_control_log(&mut file) {
        Ok(ControlScanOutcome::Clean(state))
            if {
                #[cfg(test)]
                let sync_allowed = recovery_failpoint != Some(ControlRecoveryFailpoint::FileSync);
                #[cfg(not(test))]
                let sync_allowed = true;
                sync_allowed && file.sync_all().is_ok() && file.seek(SeekFrom::End(0)).is_ok()
            } =>
        {
            ControlRecoveryOutcome::Ready(ReadyControlWriter {
                control_dir,
                file,
                state,
                lock_health,
                #[cfg(test)]
                failpoint: None,
                #[cfg(test)]
                checkpoint_failpoint: None,
            })
        }
        Ok(ControlScanOutcome::Incomplete {
            state,
            damaged_bytes,
            tail_digest,
        }) => ControlRecoveryOutcome::RepairRequired(RepairRequiredControlWriter {
            control_dir,
            lock_health,
            valid_bytes: state.valid_bytes,
            damaged_bytes,
            tail_digest,
            #[cfg(test)]
            failpoint: None,
        }),
        _ => ControlRecoveryOutcome::Corrupt(CorruptControlWriter {
            _control_dir: control_dir,
            _lock_health: lock_health,
        }),
    }
}

fn apply_control_batch(
    state: &mut ControlProjection,
    batch: &StoredControlBatchV1,
) -> Result<(), ControlStoreError> {
    if batch.schema_version != 1
        || batch.facts.is_empty()
        || batch.facts.len() > MAX_CONTROL_FACTS
        || batch.batch_sequence != state.last_batch_sequence + 1
        || batch.previous_batch_checksum != state.last_batch_checksum
        || batch.batch_checksum != control_batch_checksum(batch)?
    {
        return Err(ControlStoreError::ChecksumChainMismatch);
    }

    let prepare_count = batch
        .facts
        .iter()
        .filter(|fact| matches!(fact, StoredControlFactV1::CommandPreparedV1(_)))
        .count();
    let commit_count = batch
        .facts
        .iter()
        .filter(|fact| matches!(fact, StoredControlFactV1::CommandCommittedV1 { .. }))
        .count();
    let allocation_count = batch
        .facts
        .iter()
        .filter(|fact| {
            matches!(
                fact,
                StoredControlFactV1::ProjectAllocated { .. }
                    | StoredControlFactV1::SessionAllocated { .. }
            )
        })
        .count();
    if !((prepare_count == 1 && commit_count == 0)
        || (prepare_count == 0 && commit_count == 1 && allocation_count == 0))
    {
        return Err(ControlStoreError::IncompatibleSchema);
    }

    let before_projects = state.projects.clone();
    let before_sessions = state.sessions.clone();
    let mut allocated_project = None;
    let mut allocated_session = None;
    for fact in &batch.facts {
        match fact {
            StoredControlFactV1::ProjectAllocated { project_id } => {
                let id = DomainProjectId::parse(project_id.clone())
                    .map_err(|_| ControlStoreError::CatalogMismatch)?;
                if state.projects.len() >= MAX_CATALOG_ENTRIES
                    || !state.projects.insert(id.as_str().to_owned())
                    || allocated_project.replace(id.as_str().to_owned()).is_some()
                {
                    return Err(ControlStoreError::CatalogMismatch);
                }
            }
            StoredControlFactV1::SessionAllocated {
                session_id,
                project_id,
            } => {
                validate_wire_id(session_id)?;
                validate_wire_id(project_id)?;
                if state.sessions.len() >= MAX_CATALOG_ENTRIES
                    || !state.projects.contains(project_id)
                    || state
                        .sessions
                        .insert(session_id.clone(), project_id.clone())
                        .is_some()
                    || allocated_session
                        .replace((session_id.clone(), project_id.clone()))
                        .is_some()
                {
                    return Err(ControlStoreError::CatalogMismatch);
                }
            }
            StoredControlFactV1::CommandPreparedV1(prepared) => {
                prepared.validate()?;
                if state.prepared.contains_key(&prepared.global_tx_id) {
                    return Err(ControlStoreError::IdempotencyConflict);
                }
                let kind = prepared.kind()?;
                require_capacity_for(state.capacity(), kind)?;
                let key = (
                    prepared.command_record.client_id.clone(),
                    prepared.command_record.client_command_id.clone(),
                );
                if state.commands.contains_key(&key) {
                    return Err(ControlStoreError::IdempotencyConflict);
                }
                if let Some(plan) = &prepared.project_plan {
                    let id = plan
                        .project_id()
                        .map_err(|_| ControlStoreError::CatalogMismatch)?;
                    let id = id.as_str().to_owned();
                    if !state.projects.contains(&id)
                        || (!before_projects.contains(&id)
                            && allocated_project.as_deref() != Some(&id))
                    {
                        return Err(ControlStoreError::CatalogMismatch);
                    }
                } else if allocated_project.is_some() {
                    return Err(ControlStoreError::CatalogMismatch);
                }
                if let Some(plan) = &prepared.session_plan {
                    let session = plan
                        .session_id()
                        .map_err(|_| ControlStoreError::CatalogMismatch)?;
                    let project = plan
                        .expected_project_id()
                        .map_err(|_| ControlStoreError::CatalogMismatch)?;
                    if state.sessions.get(&session.0) != Some(&project.0)
                        || (!before_sessions.contains_key(&session.0)
                            && allocated_session.as_ref()
                                != Some(&(session.0.clone(), project.0.clone())))
                    {
                        return Err(ControlStoreError::CatalogMismatch);
                    }
                } else if allocated_session.is_some() {
                    return Err(ControlStoreError::CatalogMismatch);
                }
                state.commands.insert(
                    key,
                    GlobalCommandRecord {
                        global_tx_id: prepared.global_tx_id.clone(),
                        command_record: prepared.command_record.clone(),
                    },
                );
                state
                    .prepared
                    .insert(prepared.global_tx_id.clone(), prepared.as_ref().clone());
                state.prepared_order.push(prepared.global_tx_id.clone());
                match kind {
                    PreparedKind::External => state.external_prepared_count += 1,
                    PreparedKind::InternalRestart => state.internal_restart_prepared_count += 1,
                }
            }
            StoredControlFactV1::CommandCommittedV1 {
                global_tx_id,
                project_last,
                session_last,
            } => {
                let prepared = state
                    .prepared
                    .get(global_tx_id)
                    .ok_or(ControlStoreError::CatalogMismatch)?;
                if state.committed.contains_key(global_tx_id)
                    || project_last.is_some() != prepared.project_plan.is_some()
                    || session_last.is_some() != prepared.session_plan.is_some()
                {
                    return Err(ControlStoreError::IdempotencyConflict);
                }
                for anchor in [project_last.as_ref(), session_last.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    validate_sha256(&anchor.resulting_batch_checksum)
                        .map_err(|_| ControlStoreError::InvalidAggregatePlan)?;
                }
                state.committed.insert(
                    global_tx_id.clone(),
                    CommittedTransactionV1 {
                        project_last: project_last.clone(),
                        session_last: session_last.clone(),
                    },
                );
            }
        }
    }
    state.last_batch_sequence = batch.batch_sequence;
    state.last_batch_checksum = Some(batch.batch_checksum.clone());
    Ok(())
}

enum ControlScanOutcome {
    Clean(ControlProjection),
    Incomplete {
        state: ControlProjection,
        damaged_bytes: u64,
        tail_digest: String,
    },
}

fn scan_control_log(file: &mut File) -> Result<ControlScanOutcome, ControlStoreError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|source| control_io("seek control log", source))?;
    scan_control_log_from(file, ControlProjection::default())
}

fn scan_control_log_from(
    file: &mut File,
    mut state: ControlProjection,
) -> Result<ControlScanOutcome, ControlStoreError> {
    let log_length = file
        .metadata()
        .map_err(|source| control_io("inspect control log length", source))?
        .len();
    if state.valid_bytes > log_length {
        return Err(ControlStoreError::MiddleCorruption);
    }
    if state.valid_bytes != 0 {
        file.seek(SeekFrom::Start(state.valid_bytes - 1))
            .map_err(|source| control_io("seek control checkpoint boundary", source))?;
        let mut boundary = [0_u8; 1];
        file.read_exact(&mut boundary)
            .map_err(|source| control_io("read control checkpoint boundary", source))?;
        if boundary[0] != b'\n' {
            return Err(ControlStoreError::MiddleCorruption);
        }
    }
    file.seek(SeekFrom::Start(state.valid_bytes))
        .map_err(|source| control_io("seek control log checkpoint", source))?;
    let mut reader = BufReader::new(file);
    loop {
        let mut line = Vec::new();
        let count = reader
            .by_ref()
            .take(u64::try_from(MAX_LINE_BYTES).expect("line limit fits u64") + 1)
            .read_until(b'\n', &mut line)
            .map_err(|source| control_io("read control log", source))?;
        if count == 0 {
            return Ok(ControlScanOutcome::Clean(state));
        }
        if count > MAX_LINE_BYTES {
            return Err(ControlStoreError::ResourceLimit);
        }
        if !line.ends_with(b"\n") {
            let damaged_bytes =
                u64::try_from(count).map_err(|_| ControlStoreError::ResourceLimit)?;
            return Ok(ControlScanOutcome::Incomplete {
                state,
                damaged_bytes,
                tail_digest: format!("sha256:{:x}", Sha256::digest(&line)),
            });
        }
        line.pop();
        let batch: StoredControlBatchV1 =
            serde_json::from_slice(&line).map_err(|_| ControlStoreError::MiddleCorruption)?;
        apply_control_batch(&mut state, &batch)?;
        state.valid_bytes = state
            .valid_bytes
            .checked_add(u64::try_from(count).map_err(|_| ControlStoreError::ResourceLimit)?)
            .ok_or(ControlStoreError::ResourceLimit)?;
    }
}

fn load_control_checkpoint(
    control_dir: &OwnedFd,
    control_file: &mut File,
) -> Result<Option<ControlProjection>, ControlStoreError> {
    let fd = match openat(
        control_dir,
        CONTROL_CHECKPOINT,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(source) => return Err(control_io("open control checkpoint", source)),
    };
    let file = File::from(fd);
    validate_regular_file(&file, Some(MAX_CHECKPOINT_BYTES))?;
    let length = file
        .metadata()
        .map_err(|source| control_io("inspect control checkpoint length", source))?
        .len();
    let capacity = usize::try_from(length).map_err(|_| ControlStoreError::ResourceLimit)?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(MAX_CHECKPOINT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| control_io("read control checkpoint", source))?;
    if u64::try_from(bytes.len()).map_or(true, |read| read > MAX_CHECKPOINT_BYTES) {
        return Err(ControlStoreError::ResourceLimit);
    }
    let checkpoint: StoredControlCheckpointV1 =
        serde_json::from_slice(&bytes).map_err(|_| ControlStoreError::MiddleCorruption)?;
    if checkpoint.schema_version != 1
        || checkpoint.checksum != control_checkpoint_checksum(&checkpoint)?
        || checkpoint.covered_valid_bytes != checkpoint.projection.valid_bytes
        || checkpoint.covered_batch_sequence != checkpoint.projection.last_batch_sequence
        || checkpoint.covered_batch_checksum != checkpoint.projection.last_batch_checksum
    {
        return Err(ControlStoreError::ChecksumMismatch);
    }
    validate_control_projection(&checkpoint.projection)?;
    let log_length = control_file
        .metadata()
        .map_err(|source| control_io("inspect control checkpoint anchor length", source))?
        .len();
    if checkpoint.covered_valid_bytes > log_length {
        return Ok(Some(checkpoint.projection));
    }

    let mut anchored = ControlProjection::default();
    loop {
        if anchored.valid_bytes == checkpoint.covered_valid_bytes {
            break;
        }
        if anchored.valid_bytes > checkpoint.covered_valid_bytes {
            return Ok(None);
        }
        control_file
            .seek(SeekFrom::Start(anchored.valid_bytes))
            .map_err(|source| control_io("seek control checkpoint anchor", source))?;
        let mut reader = BufReader::new(&mut *control_file);
        let mut line = Vec::new();
        let count = reader
            .by_ref()
            .take(u64::try_from(MAX_LINE_BYTES).expect("line limit fits u64") + 1)
            .read_until(b'\n', &mut line)
            .map_err(|source| control_io("read control checkpoint anchor", source))?;
        if count == 0 || count > MAX_LINE_BYTES || !line.ends_with(b"\n") {
            return Ok(None);
        }
        line.pop();
        let Ok(batch) = serde_json::from_slice::<StoredControlBatchV1>(&line) else {
            return Ok(None);
        };
        if apply_control_batch(&mut anchored, &batch).is_err() {
            return Ok(None);
        }
        anchored.valid_bytes = anchored
            .valid_bytes
            .checked_add(u64::try_from(count).map_err(|_| ControlStoreError::ResourceLimit)?)
            .ok_or(ControlStoreError::ResourceLimit)?;
    }
    if anchored != checkpoint.projection {
        return Ok(None);
    }

    Ok(Some(checkpoint.projection))
}

fn validate_control_projection(state: &ControlProjection) -> Result<(), ControlStoreError> {
    let counted = count_prepared_kinds(state.prepared.values())?;
    let ordered_prepared = state
        .prepared_order
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if state.projects.len() > MAX_CATALOG_ENTRIES
        || state.sessions.len() > MAX_CATALOG_ENTRIES
        || counted.external > MAX_EXTERNAL_PREPARED
        || counted.internal_restart > MAX_INTERNAL_RESTART_PREPARED
        || counted.total > MAX_TOTAL_PREPARED
        || counted.external != state.external_prepared_count
        || counted.internal_restart != state.internal_restart_prepared_count
        || state.commands.len() != state.prepared.len()
        || state.prepared_order.len() != state.prepared.len()
        || ordered_prepared.len() != state.prepared.len()
        || ordered_prepared != state.prepared.keys().cloned().collect()
        || state.committed.len() > state.prepared.len()
        || (state.last_batch_sequence == 0) != state.last_batch_checksum.is_none()
    {
        return Err(ControlStoreError::ResourceLimit);
    }
    if let Some(checksum) = &state.last_batch_checksum {
        validate_sha256(checksum).map_err(|_| ControlStoreError::ChecksumMismatch)?;
    }
    for project_id in &state.projects {
        DomainProjectId::parse(project_id.clone())
            .map_err(|_| ControlStoreError::CatalogMismatch)?;
    }
    for (session_id, project_id) in &state.sessions {
        validate_wire_id(session_id)?;
        validate_wire_id(project_id)?;
        if !state.projects.contains(project_id) {
            return Err(ControlStoreError::CatalogMismatch);
        }
    }
    for (global_tx_id, prepared) in &state.prepared {
        prepared.validate()?;
        if global_tx_id != &prepared.global_tx_id {
            return Err(ControlStoreError::IdempotencyConflict);
        }
        let key = (
            prepared.command_record.client_id.clone(),
            prepared.command_record.client_command_id.clone(),
        );
        let Some(command) = state.commands.get(&key) else {
            return Err(ControlStoreError::IdempotencyConflict);
        };
        if command.global_tx_id != *global_tx_id
            || command.command_record != prepared.command_record
        {
            return Err(ControlStoreError::IdempotencyConflict);
        }
        if let Some(project_plan) = &prepared.project_plan {
            let project_id = project_plan
                .project_id()
                .map_err(|_| ControlStoreError::CatalogMismatch)?;
            if !state.projects.contains(project_id.as_str()) {
                return Err(ControlStoreError::CatalogMismatch);
            }
        }
        if let Some(session_plan) = &prepared.session_plan {
            let session_id = session_plan
                .session_id()
                .map_err(|_| ControlStoreError::CatalogMismatch)?;
            let project_id = session_plan
                .expected_project_id()
                .map_err(|_| ControlStoreError::CatalogMismatch)?;
            if state.sessions.get(&session_id.0) != Some(&project_id.0) {
                return Err(ControlStoreError::CatalogMismatch);
            }
        }
    }
    for (global_tx_id, committed) in &state.committed {
        let prepared = state
            .prepared
            .get(global_tx_id)
            .ok_or(ControlStoreError::CatalogMismatch)?;
        if committed.project_last.is_some() != prepared.project_plan.is_some()
            || committed.session_last.is_some() != prepared.session_plan.is_some()
        {
            return Err(ControlStoreError::CatalogMismatch);
        }
        for anchor in [
            committed.project_last.as_ref(),
            committed.session_last.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            validate_sha256(&anchor.resulting_batch_checksum)
                .map_err(|_| ControlStoreError::ChecksumMismatch)?;
        }
    }
    Ok(())
}

fn count_prepared_kinds<'a>(
    prepared: impl IntoIterator<Item = &'a PreparedTransactionV1>,
) -> Result<ControlCapacity, ControlStoreError> {
    let mut capacity = ControlCapacity::default();
    for prepared in prepared {
        let kind = prepared.kind()?;
        require_capacity_for(capacity, kind)?;
        match kind {
            PreparedKind::External => capacity.external += 1,
            PreparedKind::InternalRestart => capacity.internal_restart += 1,
        }
        capacity.total += 1;
    }
    Ok(capacity)
}

fn require_capacity_for(
    capacity: ControlCapacity,
    kind: PreparedKind,
) -> Result<(), ControlStoreError> {
    if capacity.total >= MAX_TOTAL_PREPARED
        || match kind {
            PreparedKind::External => capacity.external >= MAX_EXTERNAL_PREPARED,
            PreparedKind::InternalRestart => {
                capacity.internal_restart >= MAX_INTERNAL_RESTART_PREPARED
            }
        }
    {
        return Err(ControlStoreError::ResourceLimit);
    }
    Ok(())
}

fn control_batch_checksum(batch: &StoredControlBatchV1) -> Result<String, ControlStoreError> {
    let bytes = serde_json::to_vec(&(
        "alda-control-batch-v1",
        batch.schema_version,
        batch.batch_sequence,
        &batch.facts,
        &batch.previous_batch_checksum,
    ))
    .map_err(|_| ControlStoreError::IncompatibleSchema)?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn prepared_digest(prepared: &PreparedTransactionV1) -> Result<String, ControlStoreError> {
    let bytes = serde_json::to_vec(&(
        "alda-control-prepared-v1",
        prepared.schema_version,
        &prepared.global_tx_id,
        &prepared.command_record,
        &prepared.project_plan,
        &prepared.session_plan,
        &prepared.artifact_audit_plans,
    ))
    .map_err(|_| ControlStoreError::IncompatibleSchema)?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn control_checkpoint_checksum(
    checkpoint: &StoredControlCheckpointV1,
) -> Result<String, ControlStoreError> {
    let bytes = serde_json::to_vec(&(
        "alda-control-checkpoint-v1",
        checkpoint.schema_version,
        checkpoint.covered_valid_bytes,
        checkpoint.covered_batch_sequence,
        &checkpoint.covered_batch_checksum,
        &checkpoint.projection,
    ))
    .map_err(|_| ControlStoreError::IncompatibleSchema)?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

pub(crate) fn project_transaction_id(global_tx_id: &str) -> String {
    format!("{global_tx_id}:project")
}

pub(crate) fn session_transaction_id(global_tx_id: &str) -> String {
    format!("{global_tx_id}:session")
}

fn is_global_tx_id(value: &str) -> bool {
    value.strip_prefix("global-").is_some_and(|hex| {
        hex.len() == 32
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn validate_wire_id(value: &str) -> Result<(), ControlStoreError> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.'))
    {
        return Err(ControlStoreError::IncompatibleSchema);
    }
    Ok(())
}

fn require_lock(lock_health: &Weak<LockHealth>) -> Result<(), ControlStoreError> {
    lock_health
        .upgrade()
        .ok_or(ControlStoreError::LockUnavailable)?
        .require_live()
        .map_err(|_| ControlStoreError::LockUnavailable)
}

fn open_absolute_directory(path: &Path) -> Result<OwnedFd, ControlStoreError> {
    if !path.is_absolute() {
        return Err(ControlStoreError::UnsafeRoot);
    }
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() <= 1
        || bytes[1..]
            .split(|byte| *byte == b'/')
            .any(|component| component.is_empty() || component == b"." || component == b"..")
    {
        return Err(ControlStoreError::UnsafeRoot);
    }
    let mut current = openat(CWD, "/", DIRECTORY_FLAGS, Mode::empty())
        .map_err(|source| control_io("open filesystem root", source))?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                current = openat(&current, name, DIRECTORY_FLAGS, Mode::empty())
                    .map_err(|source| control_io("open control root component", source))?;
            }
            _ => return Err(ControlStoreError::UnsafeRoot),
        }
    }
    Ok(current)
}

fn ensure_directory(parent: &OwnedFd, name: &str) -> Result<OwnedFd, ControlStoreError> {
    match mkdirat(parent, name, DIRECTORY_MODE) {
        Ok(()) => {
            let child = open_directory(parent, name)?;
            fsync(&child).map_err(|source| control_io("sync control directory", source))?;
            fsync(parent).map_err(|source| control_io("sync control parent", source))?;
            Ok(child)
        }
        Err(rustix::io::Errno::EXIST) => open_directory(parent, name),
        Err(source) => Err(control_io("create control directory", source)),
    }
}

fn open_directory(parent: &OwnedFd, name: &str) -> Result<OwnedFd, ControlStoreError> {
    let fd = openat(parent, name, DIRECTORY_FLAGS, Mode::empty())
        .map_err(|source| control_io("open control directory", source))?;
    validate_directory(&fd, false)?;
    Ok(fd)
}

fn validate_directory(fd: &OwnedFd, root: bool) -> Result<(), ControlStoreError> {
    let stat = fstat(fd).map_err(|source| control_io("inspect control directory", source))?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_dir()
        || stat.st_uid != rustix::process::getuid().as_raw()
        || stat.st_mode & 0o077 != 0
        || (root && stat.st_mode & 0o777 != 0o700)
    {
        return Err(ControlStoreError::UnsafeRoot);
    }
    Ok(())
}

fn open_or_create_control_log(
    control_dir: &OwnedFd,
    #[cfg(test)] failpoint: Option<ControlOpenFailpoint>,
) -> Result<File, ControlStoreError> {
    let fd = openat(
        control_dir,
        CONTROL_LOG,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        FILE_MODE,
    )
    .map_err(|source| control_io("open control log", source))?;
    let file = File::from(fd);
    validate_regular_file(&file, None)?;
    #[cfg(test)]
    if failpoint == Some(ControlOpenFailpoint::FileSync) {
        return Err(control_io(
            "test control log open file sync",
            std::io::Error::other("injected"),
        ));
    }
    file.sync_all()
        .map_err(|source| control_io("sync control log create", source))?;
    fsync(control_dir).map_err(|source| control_io("sync control log directory", source))?;
    Ok(file)
}

fn open_control_log(control_dir: &OwnedFd) -> Result<File, ControlStoreError> {
    let fd = openat(
        control_dir,
        CONTROL_LOG,
        OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|source| control_io("reopen control log", source))?;
    let file = File::from(fd);
    validate_regular_file(&file, None)?;
    Ok(file)
}

fn validate_regular_file(file: &File, max_bytes: Option<u64>) -> Result<(), ControlStoreError> {
    let stat = fstat(file).map_err(|source| control_io("inspect control file", source))?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file()
        || stat.st_uid != rustix::process::getuid().as_raw()
        || stat.st_mode & 0o077 != 0
        || max_bytes
            .is_some_and(|limit| u64::try_from(stat.st_size).map_or(true, |size| size > limit))
    {
        return Err(ControlStoreError::UnsafeRoot);
    }
    Ok(())
}

fn control_io(operation: &'static str, source: impl Into<std::io::Error>) -> ControlStoreError {
    ControlStoreError::Io {
        operation,
        source: source.into(),
    }
}

fn random_hex_128() -> String {
    let mut bytes = [0_u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().fold(String::new(), |mut output, byte| {
        use std::fmt::Write as _;
        let _ignored = write!(output, "{byte:02x}");
        output
    })
}

#[cfg(test)]
fn inject_checkpoint(
    actual: Option<ControlCheckpointFailpoint>,
    expected: ControlCheckpointFailpoint,
    operation: &'static str,
) -> Result<(), ControlStoreError> {
    if actual == Some(expected) {
        return Err(control_io(operation, std::io::Error::other("injected")));
    }
    Ok(())
}

impl From<DurableRuntimeError> for ControlStoreError {
    fn from(_: DurableRuntimeError) -> Self {
        Self::LockUnavailable
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    use std::sync::Arc;

    use crate::domain::{BranchId, ProjectEvent, ScoreId, TakeId};
    use crate::protocol::{ClientCommandId, ProtocolErrorCode};
    use crate::state_store::session::{
        OpenSessionWriter, SessionAppendRequest, SessionRolloutEvent, StoredSessionPlanV1,
        plan_coordinated_restart_reconciliation,
    };
    use crate::state_store::{
        AppendRequest, StateStore, StateStoreInstanceLease, StoredProjectPlanV1,
    };

    use super::*;

    const GLOBAL_TX: &str = "global-11111111111111111111111111111111";

    struct Fixture {
        root: tempfile::TempDir,
        health: Arc<LockHealth>,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("control root");
            fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))
                .expect("private root");
            let state = StateStore::open(root.path(), StateStoreInstanceLease::for_tests())
                .expect("initialize state layout");
            drop(state);
            let fixture = Self {
                root,
                health: Arc::new(LockHealth::new()),
            };
            drop(fixture.ready());
            fixture
        }

        fn ready(&self) -> ReadyControlWriter {
            match open_control_writer(self.root.path(), Arc::downgrade(&self.health))
                .expect("open control writer")
            {
                OpenControlWriter::Ready(writer) => writer,
                OpenControlWriter::RepairRequired(_) => panic!("unexpected incomplete tail"),
            }
        }

        fn log_path(&self) -> std::path::PathBuf {
            self.root
                .path()
                .join(STATE_LAYOUT)
                .join(CONTROL_DIRECTORY)
                .join(CONTROL_LOG)
        }

        fn checkpoint_path(&self) -> std::path::PathBuf {
            self.root
                .path()
                .join(STATE_LAYOUT)
                .join(CONTROL_DIRECTORY)
                .join(CONTROL_CHECKPOINT)
        }
    }

    fn project(value: &str) -> DomainProjectId {
        DomainProjectId::parse(value).expect("Project ID")
    }

    fn stable_reply(command_id: &str) -> Vec<u8> {
        serde_json::to_vec(&crate::protocol::CommandReply::error(
            ClientCommandId(command_id.to_owned()),
            ProtocolErrorCode::InvalidRequest,
            "stable control reply",
        ))
        .expect("canonical reply")
    }

    fn command(command_id: &str, digest_byte: char) -> StoredCommandRecordV1 {
        StoredCommandRecordV1::new(
            "client-control",
            command_id,
            format!("sha256:{}", digest_byte.to_string().repeat(64)),
            &stable_reply(command_id),
        )
        .expect("command record")
    }

    fn prepared_with(
        global_tx_id: &str,
        command: StoredCommandRecordV1,
        with_project: bool,
        with_session: bool,
    ) -> PreparedTransactionV1 {
        let project_id = project("project-control");
        let project_plan = with_project.then(|| {
            let request = AppendRequest {
                transaction_id: project_transaction_id(global_tx_id),
                command_record: Some(command.clone()),
                events: vec![ProjectEvent::ProjectInitialized {
                    project_id: project_id.clone(),
                    score_id: ScoreId::parse("score-control").expect("score"),
                    default_take_id: TakeId::parse("take-control").expect("take"),
                    default_branch_id: BranchId::parse("branch-control").expect("branch"),
                }],
            };
            StoredProjectPlanV1::from_append_request(&project_id, 0, None, &request)
                .expect("Project plan")
        });
        let session_id = SessionId("session-control".to_owned());
        let session_plan = with_session.then(|| {
            let request = SessionAppendRequest::new(
                session_transaction_id(global_tx_id),
                Some(command.clone()),
                vec![SessionRolloutEvent::SessionStarted {
                    session_id: session_id.clone(),
                    project_id: ProjectId(project_id.as_str().to_owned()),
                }],
            );
            StoredSessionPlanV1::from_append_request(
                &session_id,
                &ProjectId(project_id.as_str().to_owned()),
                0,
                None,
                &request,
            )
            .expect("Session plan")
        });
        PreparedTransactionV1::new(
            global_tx_id.to_owned(),
            command,
            project_plan,
            session_plan,
            Vec::new(),
        )
        .expect("Prepared")
    }

    fn combined_prepare(command_id: &str, digest_byte: char) -> PrepareControlRequest {
        let command = command(command_id, digest_byte);
        PrepareControlRequest {
            project_allocation: Some(project("project-control")),
            session_allocation: Some(SessionAllocation {
                session_id: SessionId("session-control".to_owned()),
                project_id: ProjectId("project-control".to_owned()),
            }),
            prepared: prepared_with(GLOBAL_TX, command, true, true),
        }
    }

    fn coordinated_restart_prepared() -> PreparedTransactionV1 {
        let root = tempfile::tempdir().expect("restart planning root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))
            .expect("private restart root");
        let store = StateStore::open(root.path(), StateStoreInstanceLease::for_tests())
            .expect("restart planning store");
        let session_id = SessionId("session-control".to_owned());
        let project_id = ProjectId("project-control".to_owned());
        let writer = match store
            .open_session_writer(session_id.clone())
            .expect("restart Session writer")
        {
            OpenSessionWriter::Ready(writer) => writer,
            OpenSessionWriter::RepairRequired(_) => panic!("fresh Session cannot need repair"),
        };
        let Ok((writer, _)) = writer.append(SessionAppendRequest::new(
            "fixture-prefix".to_owned(),
            None,
            vec![
                SessionRolloutEvent::SessionStarted {
                    session_id: session_id.clone(),
                    project_id: project_id.clone(),
                },
                SessionRolloutEvent::TurnStarted {
                    turn_id: crate::protocol::TurnId("turn-control".to_owned()),
                    canonical_prompt: "prompt".to_owned(),
                },
            ],
        )) else {
            panic!("restart prefix");
        };
        let coordinated =
            plan_coordinated_restart_reconciliation(store.instance_id(), writer.projection())
                .expect("restart planner")
                .expect("restart obligation");
        let (pre_sequence, pre_checksum) = writer.head();
        let session_plan = StoredSessionPlanV1::from_append_request(
            &session_id,
            &project_id,
            pre_sequence,
            pre_checksum.map(ToOwned::to_owned),
            &coordinated.append_request(),
        )
        .expect("coordinated Session plan");
        PreparedTransactionV1::new(
            coordinated.global_tx_id,
            coordinated.command_record,
            None,
            Some(session_plan),
            Vec::new(),
        )
        .expect("coordinated control Prepared")
    }

    struct RestartCapacityFixture {
        _root: tempfile::TempDir,
        _store: StateStore,
        writer: crate::state_store::session::ReadySessionWriter,
    }

    impl RestartCapacityFixture {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("容量 Session root");
            fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))
                .expect("私有容量 Session root");
            let store = StateStore::open(root.path(), StateStoreInstanceLease::for_tests())
                .expect("容量 Session store");
            let session_id = SessionId("session-control".to_owned());
            let writer = match store
                .open_session_writer(session_id.clone())
                .expect("容量 Session writer")
            {
                OpenSessionWriter::Ready(writer) => writer,
                OpenSessionWriter::RepairRequired(_) => panic!("新 Session 不应需要修复"),
            };
            let Ok((writer, _)) = writer.append(SessionAppendRequest::new(
                "capacity-prefix".to_owned(),
                None,
                vec![
                    SessionRolloutEvent::SessionStarted {
                        session_id,
                        project_id: ProjectId("project-control".to_owned()),
                    },
                    SessionRolloutEvent::TurnStarted {
                        turn_id: crate::protocol::TurnId("turn-control".to_owned()),
                        canonical_prompt: "容量边界重启义务".to_owned(),
                    },
                ],
            )) else {
                panic!("写入容量 Session 前缀");
            };
            Self {
                _root: root,
                _store: store,
                writer,
            }
        }

        fn request(&self, index: usize) -> PrepareControlRequest {
            let instance_id = format!("{index:032x}");
            let coordinated =
                plan_coordinated_restart_reconciliation(&instance_id, self.writer.projection())
                    .expect("容量 restart planner")
                    .expect("容量 restart obligation");
            let (pre_sequence, pre_checksum) = self.writer.head();
            let session_plan = StoredSessionPlanV1::from_append_request(
                &SessionId("session-control".to_owned()),
                &ProjectId("project-control".to_owned()),
                pre_sequence,
                pre_checksum.map(ToOwned::to_owned),
                &coordinated.append_request(),
            )
            .expect("容量 coordinated Session plan");
            let prepared = PreparedTransactionV1::new(
                coordinated.global_tx_id,
                coordinated.command_record,
                None,
                Some(session_plan),
                Vec::new(),
            )
            .expect("容量 internal Prepared");
            PrepareControlRequest {
                project_allocation: None,
                session_allocation: None,
                prepared,
            }
        }
    }

    fn external_capacity_request(index: usize) -> PrepareControlRequest {
        let global_tx_id = format!("global-e{index:031x}");
        let command_id = format!("command-capacity-external-{index}");
        let command = command(&command_id, 'c');
        let allocate_catalog = index == 0;
        PrepareControlRequest {
            project_allocation: allocate_catalog.then(|| project("project-control")),
            session_allocation: allocate_catalog.then(|| SessionAllocation {
                session_id: SessionId("session-control".to_owned()),
                project_id: ProjectId("project-control".to_owned()),
            }),
            prepared: prepared_with(&global_tx_id, command, allocate_catalog, true),
        }
    }

    fn assert_prepare_rejected_without_append(
        writer: ReadyControlWriter,
        request: PrepareControlRequest,
        log_path: &Path,
    ) -> ReadyControlWriter {
        let before = fs::read(log_path).expect("读取拒绝前 control log");
        let writer = match writer.prepare(request) {
            Err(ControlAppendFailure::Rejected {
                writer,
                error: ControlStoreError::ResourceLimit,
            }) => writer,
            Err(ControlAppendFailure::Rejected { error, .. }) => {
                panic!("容量拒绝返回了错误类型：{error}")
            }
            Err(ControlAppendFailure::Poisoned { error, .. }) => {
                panic!("容量拒绝不得使 writer 中毒：{error}")
            }
            Ok(_) => panic!("超限 Prepared 不得追加"),
        };
        assert_eq!(
            fs::read(log_path).expect("读取拒绝后 control log"),
            before,
            "容量拒绝必须发生在 durable append 前"
        );
        writer
    }

    fn open_capacity(
        fixture: &Fixture,
        expected: ControlCapacity,
        checkpoint: bool,
        label: &str,
    ) -> ReadyControlWriter {
        reset_checkpoint_load_observed();
        let writer = fixture.ready();
        assert_eq!(writer.projection().capacity(), expected, "{label}");
        assert_eq!(checkpoint_load_observed(), checkpoint, "{label}");
        println!(
            "{label}: external={}, internal={}, total={}, replay={}",
            expected.external,
            expected.internal_restart,
            expected.total,
            if checkpoint {
                "checkpoint+tail"
            } else {
                "full"
            }
        );
        writer
    }

    fn prepare_ok(
        writer: ReadyControlWriter,
        request: PrepareControlRequest,
    ) -> (ReadyControlWriter, ControlAppendOutcome) {
        let Ok(value) = writer.prepare(request) else {
            panic!("control prepare must succeed");
        };
        value
    }

    fn commit_ok(
        writer: ReadyControlWriter,
        global_tx_id: &str,
    ) -> (ReadyControlWriter, ControlAppendOutcome) {
        let Ok(value) = writer.commit(CommitControlRequest {
            global_tx_id: global_tx_id.to_owned(),
            project_last: Some(AggregateCommitV1 {
                resulting_last_sequence: 1,
                resulting_batch_checksum: format!("sha256:{}", "a".repeat(64)),
            }),
            session_last: Some(AggregateCommitV1 {
                resulting_last_sequence: 1,
                resulting_batch_checksum: format!("sha256:{}", "b".repeat(64)),
            }),
        }) else {
            panic!("control commit must succeed");
        };
        value
    }

    #[test]
    fn prepared_codec_is_canonical_validated_and_preserves_exact_reply() {
        let prepared = prepared_with(GLOBAL_TX, command("command-codec", 'a'), true, true);
        let bytes = serde_json::to_vec(&prepared).expect("serialize Prepared");
        let decoded: PreparedTransactionV1 =
            serde_json::from_slice(&bytes).expect("decode Prepared");
        decoded.validate().expect("validate decoded Prepared");
        assert_eq!(
            decoded.stable_reply().expect("stable reply"),
            stable_reply("command-codec")
        );
        assert_eq!(
            serde_json::to_vec(&decoded).expect("reencode Prepared"),
            bytes
        );

        let mut tampered: serde_json::Value =
            serde_json::from_slice(&bytes).expect("Prepared value");
        tampered["prepared_digest"] =
            serde_json::Value::String(format!("sha256:{}", "f".repeat(64)));
        let tampered: PreparedTransactionV1 =
            serde_json::from_value(tampered).expect("shape still decodes");
        assert!(matches!(
            tampered.validate(),
            Err(ControlStoreError::ChecksumMismatch)
        ));

        let mut unknown: serde_json::Value =
            serde_json::from_slice(&bytes).expect("Prepared value");
        unknown["unknown"] = serde_json::Value::Bool(true);
        assert!(serde_json::from_value::<PreparedTransactionV1>(unknown).is_err());
    }

    #[test]
    fn coordinated_restart_is_the_only_valid_internal_prepared_identity() {
        let prepared = coordinated_restart_prepared();
        assert!(matches!(prepared.kind(), Ok(PreparedKind::InternalRestart)));
        assert!(prepared.project_plan.is_none());
        assert!(prepared.artifact_audit_plans.is_empty());
        assert_eq!(
            prepared
                .session_plan
                .as_ref()
                .map(StoredSessionPlanV1::transaction_id),
            Some(session_transaction_id(&prepared.global_tx_id).as_str())
        );

        let mut forged_namespace = prepared.clone();
        forged_namespace.command_record.client_id = "__alda_internal_forged".to_owned();
        assert!(matches!(
            forged_namespace.kind(),
            Err(ControlStoreError::InvalidAggregatePlan)
        ));

        let mut external = prepared_with(GLOBAL_TX, command("command-forged", 'a'), true, false);
        external.command_record.client_id = INTERNAL_RESTART_CLIENT_ID.to_owned();
        assert!(matches!(
            external.validate(),
            Err(ControlStoreError::InvalidAggregatePlan)
        ));

        let fixture = Fixture::new();
        let (writer, _) = prepare_ok(
            fixture.ready(),
            combined_prepare("command-internal-catalog", 'c'),
        );
        let valid = coordinated_restart_prepared();
        let mut forged_retry = valid.clone();
        forged_retry.global_tx_id = "global-00000000000000000000000000000000".to_owned();
        let (writer, _) = prepare_ok(
            writer,
            PrepareControlRequest {
                project_allocation: None,
                session_allocation: None,
                prepared: valid,
            },
        );
        assert!(matches!(
            writer.prepare(PrepareControlRequest {
                project_allocation: None,
                session_allocation: None,
                prepared: forged_retry,
            }),
            Err(ControlAppendFailure::Rejected {
                error: ControlStoreError::InvalidAggregatePlan,
                ..
            })
        ));
    }

    #[test]
    fn durable_backend_capacity_external_internal_and_total_boundaries_are_independent() {
        assert!(
            require_capacity_for(
                ControlCapacity {
                    external: 9_999,
                    internal_restart: 10_000,
                    total: 19_999,
                },
                PreparedKind::External,
            )
            .is_ok()
        );
        assert!(matches!(
            require_capacity_for(
                ControlCapacity {
                    external: 10_000,
                    internal_restart: 0,
                    total: 10_000,
                },
                PreparedKind::External,
            ),
            Err(ControlStoreError::ResourceLimit)
        ));
        assert!(
            require_capacity_for(
                ControlCapacity {
                    external: 10_000,
                    internal_restart: 9_999,
                    total: 19_999,
                },
                PreparedKind::InternalRestart,
            )
            .is_ok()
        );
        assert!(matches!(
            require_capacity_for(
                ControlCapacity {
                    external: 0,
                    internal_restart: 10_000,
                    total: 10_000,
                },
                PreparedKind::InternalRestart,
            ),
            Err(ControlStoreError::ResourceLimit)
        ));
        for kind in [PreparedKind::External, PreparedKind::InternalRestart] {
            assert!(matches!(
                require_capacity_for(
                    ControlCapacity {
                        external: 10_000,
                        internal_restart: 10_000,
                        total: 20_000,
                    },
                    kind,
                ),
                Err(ControlStoreError::ResourceLimit)
            ));
        }
    }

    #[test]
    fn durable_backend_capacity_classification_survives_checkpoint_and_full_replay() {
        let fixture = Fixture::new();
        let (writer, _) = prepare_ok(fixture.ready(), combined_prepare("command-capacity", 'a'));
        let internal = coordinated_restart_prepared();
        let (writer, _) = prepare_ok(
            writer,
            PrepareControlRequest {
                project_allocation: None,
                session_allocation: None,
                prepared: internal,
            },
        );
        assert_eq!(
            writer.projection().capacity(),
            ControlCapacity {
                external: 1,
                internal_restart: 1,
                total: 2,
            }
        );
        writer.write_checkpoint().expect("capacity checkpoint");
        drop(writer);
        let reopened = fixture.ready();
        assert_eq!(
            reopened.projection().capacity(),
            ControlCapacity {
                external: 1,
                internal_restart: 1,
                total: 2,
            }
        );
        drop(reopened);
        fs::write(fixture.checkpoint_path(), b"invalid checkpoint").expect("force full replay");
        assert_eq!(
            fixture.ready().projection().capacity(),
            ControlCapacity {
                external: 1,
                internal_restart: 1,
                total: 2,
            }
        );
    }

    #[test]
    fn durable_backend_capacity_real_control_prepare_boundaries() {
        let external = Fixture::new();
        let writer = append_validated_control_fixture(
            external.ready(),
            (0..9_999).map(external_capacity_request),
            false,
        )
        .expect("生成 9,999 external durable Prepared");
        assert_eq!(
            writer.projection().capacity(),
            ControlCapacity {
                external: 9_999,
                internal_restart: 0,
                total: 9_999,
            }
        );
        let tenthousandth = external_capacity_request(9_999);
        let (writer, appended) = prepare_ok(writer, tenthousandth.clone());
        assert!(appended.appended);
        let before_duplicate = fs::read(external.log_path()).expect("读取幂等前 control log");
        let (writer, duplicate) = prepare_ok(writer, tenthousandth);
        assert!(!duplicate.appended);
        assert_eq!(
            fs::read(external.log_path()).expect("读取幂等后 control log"),
            before_duplicate
        );
        let writer = assert_prepare_rejected_without_append(
            writer,
            external_capacity_request(10_000),
            &external.log_path(),
        );
        assert_eq!(writer.projection().capacity().external, 10_000);
        println!("prepare external: 9,999 -> 10,000 -> 10,001 rejected");

        let internal = Fixture::new();
        let restart = RestartCapacityFixture::new();
        let writer = append_validated_control_fixture(
            internal.ready(),
            std::iter::once(external_capacity_request(0)),
            false,
        )
        .expect("生成 internal catalog");
        let writer = append_validated_control_fixture(
            writer,
            (0..9_999).map(|index| restart.request(index)),
            false,
        )
        .expect("生成 9,999 internal durable Prepared");
        assert_eq!(writer.projection().capacity().internal_restart, 9_999);
        let tenthousandth = restart.request(9_999);
        let (writer, appended) = prepare_ok(writer, tenthousandth.clone());
        assert!(appended.appended);
        let before_duplicate = fs::read(internal.log_path()).expect("读取 internal 幂等前日志");
        let (writer, duplicate) = prepare_ok(writer, tenthousandth);
        assert!(!duplicate.appended);
        assert_eq!(
            fs::read(internal.log_path()).expect("读取 internal 幂等后日志"),
            before_duplicate
        );
        let writer = assert_prepare_rejected_without_append(
            writer,
            restart.request(10_000),
            &internal.log_path(),
        );
        assert_eq!(writer.projection().capacity().internal_restart, 10_000);
        println!("prepare internal: 9,999 -> 10,000 -> 10,001 rejected");

        let physical = Fixture::new();
        let writer = append_validated_control_fixture(
            physical.ready(),
            (0..10_000).map(external_capacity_request),
            false,
        )
        .expect("生成 10,000 external physical fixture");
        let writer = append_validated_control_fixture(
            writer,
            (0..9_999).map(|index| restart.request(index)),
            false,
        )
        .expect("生成 19,999 physical durable Prepared");
        assert_eq!(writer.projection().capacity().total, 19_999);
        let (writer, appended) = prepare_ok(writer, restart.request(9_999));
        assert!(appended.appended);
        let writer = assert_prepare_rejected_without_append(
            writer,
            restart.request(10_000),
            &physical.log_path(),
        );
        assert_eq!(writer.projection().capacity().total, 20_000);
        println!("prepare physical: 19,999 -> 20,000 -> 20,001 rejected");

        let reverse_physical = Fixture::new();
        let writer = append_validated_control_fixture(
            reverse_physical.ready(),
            (0..9_999).map(external_capacity_request),
            false,
        )
        .expect("生成 9,999 external 反向 physical fixture");
        let writer = append_validated_control_fixture(
            writer,
            (0..10_000).map(|index| restart.request(index)),
            false,
        )
        .expect("生成 10,000 internal 反向 physical fixture");
        assert_eq!(
            writer.projection().capacity(),
            ControlCapacity {
                external: 9_999,
                internal_restart: 10_000,
                total: 19_999,
            }
        );
        let last_external = external_capacity_request(9_999);
        let (writer, appended) = prepare_ok(writer, last_external.clone());
        assert!(appended.appended);
        let stable_reply = appended.stable_reply;
        assert_eq!(
            writer.projection().capacity(),
            ControlCapacity {
                external: 10_000,
                internal_restart: 10_000,
                total: 20_000,
            }
        );
        let before_duplicate_capacity = writer.projection().capacity();
        let before_duplicate =
            fs::read(reverse_physical.log_path()).expect("读取反向满载幂等前日志");
        let (writer, duplicate) = prepare_ok(writer, last_external);
        assert!(!duplicate.appended);
        assert_eq!(duplicate.stable_reply, stable_reply);
        assert_eq!(writer.projection().capacity(), before_duplicate_capacity);
        assert_eq!(
            fs::read(reverse_physical.log_path()).expect("读取反向满载幂等后日志"),
            before_duplicate
        );
        let writer = assert_prepare_rejected_without_append(
            writer,
            external_capacity_request(10_000),
            &reverse_physical.log_path(),
        );
        assert_eq!(
            writer.projection().capacity(),
            ControlCapacity {
                external: 10_000,
                internal_restart: 10_000,
                total: 20_000,
            }
        );
        println!(
            "prepare reverse physical: 9,999/10,000/19,999 -> 10,000/10,000/20,000 -> 20,001 rejected; full-load duplicate unchanged"
        );
    }

    fn seed_replay_fixture(
        fixture: &Fixture,
        restart: &RestartCapacityFixture,
        kind: &str,
        checkpoint: bool,
    ) -> ReadyControlWriter {
        let writer = fixture.ready();
        let writer = match kind {
            "external" | "internal" | "physical" | "reverse-physical" => {
                append_validated_control_fixture(
                    writer,
                    std::iter::once(external_capacity_request(0)),
                    false,
                )
            }
            _ => panic!("未知容量 fixture：{kind}"),
        }
        .expect("写入容量 fixture checkpoint 前缀");
        if checkpoint {
            writer.write_checkpoint().expect("写入有效容量 checkpoint");
        }
        match kind {
            "external" => append_validated_control_fixture(
                writer,
                (1..9_999).map(external_capacity_request),
                false,
            ),
            "internal" => append_validated_control_fixture(
                writer,
                (0..9_999).map(|index| restart.request(index)),
                false,
            ),
            "physical" => {
                let writer = append_validated_control_fixture(
                    writer,
                    (1..10_000).map(external_capacity_request),
                    false,
                )
                .expect("写入 physical external fixture");
                append_validated_control_fixture(
                    writer,
                    (0..9_999).map(|index| restart.request(index)),
                    false,
                )
            }
            "reverse-physical" => {
                let writer = append_validated_control_fixture(
                    writer,
                    (1..9_999).map(external_capacity_request),
                    false,
                )
                .expect("写入 reverse physical external fixture");
                append_validated_control_fixture(
                    writer,
                    (0..10_000).map(|index| restart.request(index)),
                    false,
                )
            }
            _ => unreachable!(),
        }
        .expect("写入容量 fixture tail")
    }

    #[test]
    fn durable_backend_capacity_real_control_replay_boundaries() {
        let restart = RestartCapacityFixture::new();
        for checkpoint in [false, true] {
            for kind in ["external", "internal", "physical", "reverse-physical"] {
                let fixture = Fixture::new();
                let writer = seed_replay_fixture(&fixture, &restart, kind, checkpoint);
                drop(writer);
                let (before, at, overflow) = match kind {
                    "external" => (
                        ControlCapacity {
                            external: 9_999,
                            internal_restart: 0,
                            total: 9_999,
                        },
                        ControlCapacity {
                            external: 10_000,
                            internal_restart: 0,
                            total: 10_000,
                        },
                        external_capacity_request(10_000),
                    ),
                    "internal" => (
                        ControlCapacity {
                            external: 1,
                            internal_restart: 9_999,
                            total: 10_000,
                        },
                        ControlCapacity {
                            external: 1,
                            internal_restart: 10_000,
                            total: 10_001,
                        },
                        restart.request(10_000),
                    ),
                    "physical" => (
                        ControlCapacity {
                            external: 10_000,
                            internal_restart: 9_999,
                            total: 19_999,
                        },
                        ControlCapacity {
                            external: 10_000,
                            internal_restart: 10_000,
                            total: 20_000,
                        },
                        restart.request(10_000),
                    ),
                    "reverse-physical" => (
                        ControlCapacity {
                            external: 9_999,
                            internal_restart: 10_000,
                            total: 19_999,
                        },
                        ControlCapacity {
                            external: 10_000,
                            internal_restart: 10_000,
                            total: 20_000,
                        },
                        external_capacity_request(10_000),
                    ),
                    _ => unreachable!(),
                };
                let writer = open_capacity(
                    &fixture,
                    before,
                    checkpoint,
                    &format!("replay {kind} before"),
                );
                let boundary = match kind {
                    "external" | "reverse-physical" => external_capacity_request(9_999),
                    "internal" | "physical" => restart.request(9_999),
                    _ => unreachable!(),
                };
                let (writer, outcome) = prepare_ok(writer, boundary);
                assert!(outcome.appended);
                drop(writer);
                let writer = open_capacity(&fixture, at, checkpoint, &format!("replay {kind} at"));
                append_canonical_over_limit_fixture_tail(writer, overflow)
                    .expect("追加 canonical 超限 tail");
                reset_checkpoint_load_observed();
                assert!(matches!(
                    open_control_writer(fixture.root.path(), Arc::downgrade(&fixture.health)),
                    Err(ControlStoreError::ResourceLimit)
                ));
                assert_eq!(checkpoint_load_observed(), checkpoint);
                println!(
                    "replay {kind} overflow rejected before writer publish: checkpoint={checkpoint}"
                );
            }
        }
    }

    #[test]
    fn durable_backend_capacity_duplicate_prepare_does_not_consume_capacity() {
        let fixture = Fixture::new();
        let (writer, first) = prepare_ok(
            fixture.ready(),
            combined_prepare("command-capacity-duplicate", 'b'),
        );
        assert!(first.appended);
        let before = writer.projection().capacity();
        let (writer, duplicate) =
            prepare_ok(writer, combined_prepare("command-capacity-duplicate", 'b'));
        assert!(!duplicate.appended);
        assert_eq!(writer.projection().capacity(), before);
    }

    #[test]
    fn pending_transactions_preserve_control_prepare_order() {
        let fixture = Fixture::new();
        let first_tx = format!("global-{}", "f".repeat(32));
        let second_tx = format!("global-{}", "0".repeat(32));
        let (writer, _) = prepare_ok(
            fixture.ready(),
            PrepareControlRequest {
                project_allocation: Some(project("project-control")),
                session_allocation: Some(SessionAllocation {
                    session_id: SessionId("session-control".to_owned()),
                    project_id: ProjectId("project-control".to_owned()),
                }),
                prepared: prepared_with(&first_tx, command("command-order-first", 'a'), true, true),
            },
        );
        let (writer, _) = prepare_ok(
            writer,
            PrepareControlRequest {
                project_allocation: None,
                session_allocation: None,
                prepared: prepared_with(
                    &second_tx,
                    command("command-order-second", 'b'),
                    true,
                    true,
                ),
            },
        );
        assert_eq!(
            writer
                .projection()
                .pending()
                .into_iter()
                .map(|prepared| prepared.global_tx_id)
                .collect::<Vec<_>>(),
            vec![first_tx, second_tx]
        );
    }

    #[test]
    fn allocations_prepared_command_index_and_exact_reply_are_one_atomic_batch() {
        let fixture = Fixture::new();
        let (writer, outcome) =
            prepare_ok(fixture.ready(), combined_prepare("command-atomic", 'a'));
        assert!(outcome.appended);
        assert_eq!(outcome.stable_reply, Some(stable_reply("command-atomic")));
        assert_eq!(writer.projection().projects.len(), 1);
        assert_eq!(
            writer.projection().sessions.get("session-control"),
            Some(&"project-control".to_owned())
        );
        assert_eq!(writer.projection().prepared.len(), 1);
        assert_eq!(writer.projection().commands.len(), 1);
        let lines = fs::read_to_string(fixture.log_path())
            .expect("control log")
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 1);
        let batch: StoredControlBatchV1 =
            serde_json::from_str(&lines[0]).expect("stored control batch");
        assert_eq!(batch.facts.len(), 3);
        assert!(matches!(
            batch.facts.as_slice(),
            [
                StoredControlFactV1::ProjectAllocated { .. },
                StoredControlFactV1::SessionAllocated { .. },
                StoredControlFactV1::CommandPreparedV1(_)
            ]
        ));
        drop(writer);

        let reopened = fixture.ready();
        assert_eq!(reopened.projection().projects.len(), 1);
        assert_eq!(reopened.projection().pending().len(), 1);
    }

    #[test]
    fn command_and_commit_idempotency_return_stored_facts_without_appending() {
        let fixture = Fixture::new();
        let request = combined_prepare("command-idempotent", 'a');
        let prepared = request.prepared.clone();
        let (writer, _) = prepare_ok(fixture.ready(), request);
        let first_length = writer.projection().valid_bytes;
        let (writer, duplicate) = prepare_ok(
            writer,
            PrepareControlRequest {
                project_allocation: Some(project("project-control")),
                session_allocation: Some(SessionAllocation {
                    session_id: SessionId("session-control".to_owned()),
                    project_id: ProjectId("project-control".to_owned()),
                }),
                prepared,
            },
        );
        assert!(!duplicate.appended);
        assert_eq!(
            duplicate.stable_reply,
            Some(stable_reply("command-idempotent"))
        );
        assert_eq!(writer.projection().valid_bytes, first_length);

        let conflicting = combined_prepare("command-idempotent", 'b');
        let writer = match writer.prepare(conflicting) {
            Err(ControlAppendFailure::Rejected { writer, error }) => {
                assert!(matches!(error, ControlStoreError::IdempotencyConflict));
                writer
            }
            _ => panic!("different digest must be rejected"),
        };
        let (writer, first_commit) = commit_ok(writer, GLOBAL_TX);
        assert!(first_commit.appended);
        let committed_length = writer.projection().valid_bytes;
        let (writer, duplicate_commit) = commit_ok(writer, GLOBAL_TX);
        assert!(!duplicate_commit.appended);
        assert_eq!(writer.projection().valid_bytes, committed_length);
    }

    #[test]
    fn append_failpoints_distinguish_untouched_rejection_from_poisoned_recovery() {
        let fixture = Fixture::new();
        let mut writer = fixture.ready();
        writer.set_failpoint(ControlAppendFailpoint::BeforeWrite);
        writer = match writer.prepare(combined_prepare("command-before-write", 'a')) {
            Err(ControlAppendFailure::Rejected { writer, error }) => {
                assert!(matches!(error, ControlStoreError::Io { .. }));
                writer
            }
            _ => panic!("before-write failure must retain a Ready writer"),
        };
        assert_eq!(writer.projection().last_batch_sequence, 0);
        drop(writer);
        assert_eq!(fs::metadata(fixture.log_path()).expect("log").len(), 0);

        let mut writer = fixture.ready();
        writer.set_failpoint(ControlAppendFailpoint::PartialWrite(17));
        let poisoned = match writer.prepare(combined_prepare("command-partial", 'b')) {
            Err(ControlAppendFailure::Poisoned { writer, error }) => {
                assert!(matches!(error, ControlStoreError::Io { .. }));
                writer
            }
            _ => panic!("partial write must poison"),
        };
        let ControlRecoveryOutcome::RepairRequired(mut repair) = poisoned.recover() else {
            panic!("partial tail must require repair");
        };
        assert_eq!(repair.valid_bytes, 0);
        assert_eq!(repair.damaged_bytes, 17);
        repair.set_failpoint(ControlRepairFailpoint::RescanRace);
        assert!(repair.repair().is_err());

        let reopened =
            match open_control_writer(fixture.root.path(), Arc::downgrade(&fixture.health))
                .expect("reopen incomplete control")
            {
                OpenControlWriter::RepairRequired(repair) => repair,
                OpenControlWriter::Ready(_) => panic!("tail remains incomplete"),
            };
        let Ok(writer) = reopened.repair() else {
            panic!("repair exact tail");
        };
        assert_eq!(writer.projection().last_batch_sequence, 0);
    }

    #[test]
    fn completed_line_failpoints_recover_as_committed_and_retry_exactly() {
        for failpoint in [
            ControlAppendFailpoint::AfterNewlineBeforeSync,
            ControlAppendFailpoint::FileSync,
            ControlAppendFailpoint::AfterSync,
        ] {
            let fixture = Fixture::new();
            let mut writer = fixture.ready();
            writer.set_failpoint(failpoint);
            let Err(ControlAppendFailure::Poisoned {
                writer: poisoned, ..
            }) = writer.prepare(combined_prepare("command-complete-line", 'c'))
            else {
                panic!("post-write failure must poison");
            };
            let ControlRecoveryOutcome::Ready(writer) = poisoned.recover() else {
                panic!("complete checksummed line must recover Ready");
            };
            let (writer, duplicate) =
                prepare_ok(writer, combined_prepare("command-complete-line", 'c'));
            assert!(!duplicate.appended);
            assert_eq!(
                duplicate.stable_reply,
                Some(stable_reply("command-complete-line"))
            );
            assert_eq!(writer.projection().last_batch_sequence, 1);
        }
    }

    #[test]
    fn completed_control_line_requires_a_successful_recovery_sync() {
        let fixture = Fixture::new();
        let mut writer = fixture.ready();
        writer.set_failpoint(ControlAppendFailpoint::FileSync);
        let Err(ControlAppendFailure::Poisoned {
            writer: mut poisoned,
            ..
        }) = writer.prepare(combined_prepare("command-recovery-sync", 'c'))
        else {
            panic!("file sync failure must poison a complete control line");
        };
        poisoned.set_recovery_failpoint(ControlRecoveryFailpoint::FileSync);
        assert!(matches!(
            poisoned.recover(),
            ControlRecoveryOutcome::Corrupt(_)
        ));
    }

    #[test]
    fn repair_failpoints_fail_closed_and_never_forge_a_ready_writer() {
        for failpoint in [
            ControlRepairFailpoint::Truncate,
            ControlRepairFailpoint::FileSync,
            ControlRepairFailpoint::DirectorySync,
        ] {
            let fixture = Fixture::new();
            fs::write(fixture.log_path(), b"incomplete").expect("incomplete tail");
            let mut repair =
                match open_control_writer(fixture.root.path(), Arc::downgrade(&fixture.health))
                    .expect("open repair state")
                {
                    OpenControlWriter::RepairRequired(repair) => repair,
                    OpenControlWriter::Ready(_) => panic!("must require repair"),
                };
            repair.set_failpoint(failpoint);
            assert!(repair.repair().is_err());
        }
    }

    #[test]
    fn checkpoint_round_trip_fallback_and_failpoints_preserve_authoritative_log() {
        let fixture = Fixture::new();
        let (writer, _) = prepare_ok(fixture.ready(), combined_prepare("command-checkpoint", 'd'));
        writer.write_checkpoint().expect("write checkpoint");
        let metadata = fs::metadata(fixture.checkpoint_path()).expect("checkpoint metadata");
        assert!(metadata.is_file());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        drop(writer);

        let reopened = fixture.ready();
        assert_eq!(reopened.projection().prepared.len(), 1);
        drop(reopened);

        let mut forged: StoredControlCheckpointV1 = serde_json::from_slice(
            &fs::read(fixture.checkpoint_path()).expect("read control checkpoint"),
        )
        .expect("decode control checkpoint");
        forged.projection.commands.clear();
        forged.projection.prepared.clear();
        forged.projection.prepared_order.clear();
        forged.projection.committed.clear();
        forged.projection.external_prepared_count = 0;
        forged.projection.internal_restart_prepared_count = 0;
        forged.checksum = control_checkpoint_checksum(&forged).expect("rechecksum forged cache");
        fs::write(
            fixture.checkpoint_path(),
            serde_json::to_vec(&forged).expect("encode forged cache"),
        )
        .expect("write forged cache");
        let anchored = fixture.ready();
        assert_eq!(anchored.projection().prepared.len(), 1);
        assert_eq!(anchored.projection().capacity().external, 1);
        drop(anchored);

        fs::write(fixture.checkpoint_path(), b"{broken").expect("corrupt checkpoint");
        let fallback = fixture.ready();
        assert_eq!(fallback.projection().prepared.len(), 1);
        drop(fallback);

        for failpoint in [
            ControlCheckpointFailpoint::TempCreate,
            ControlCheckpointFailpoint::TempWrite,
            ControlCheckpointFailpoint::FileSync,
            ControlCheckpointFailpoint::BeforeInstall,
            ControlCheckpointFailpoint::AfterInstall,
            ControlCheckpointFailpoint::DirectorySync,
        ] {
            let mut writer = fixture.ready();
            writer.set_checkpoint_failpoint(failpoint);
            assert!(matches!(
                writer.write_checkpoint(),
                Err(ControlStoreError::Io { .. })
            ));
            drop(writer);
            assert_eq!(fixture.ready().projection().prepared.len(), 1);
        }
    }

    #[test]
    fn valid_checkpoint_detects_authoritative_log_truncation() {
        let fixture = Fixture::new();
        let (writer, _) = prepare_ok(fixture.ready(), combined_prepare("command-truncate", 'e'));
        writer.write_checkpoint().expect("checkpoint");
        drop(writer);
        let file = fs::OpenOptions::new()
            .write(true)
            .open(fixture.log_path())
            .expect("open log");
        file.set_len(0).expect("truncate log");
        assert!(matches!(
            open_control_writer(fixture.root.path(), Arc::downgrade(&fixture.health)),
            Err(ControlStoreError::MiddleCorruption)
        ));
    }

    #[test]
    fn corrupt_complete_lines_incomplete_tails_and_oversized_lines_are_bounded() {
        let fixture = Fixture::new();
        let (writer, _) = prepare_ok(fixture.ready(), combined_prepare("command-corrupt", 'f'));
        drop(writer);
        let mut bytes = fs::read(fixture.log_path()).expect("control bytes");
        let index = bytes
            .iter()
            .position(|byte| *byte == b'1')
            .expect("digit to corrupt");
        bytes[index] = b'2';
        fs::write(fixture.log_path(), bytes).expect("corrupt complete line");
        assert!(open_control_writer(fixture.root.path(), Arc::downgrade(&fixture.health)).is_err());

        let incomplete = Fixture::new();
        fs::write(incomplete.log_path(), b"{\"partial\":true}").expect("partial tail");
        assert!(matches!(
            open_control_writer(incomplete.root.path(), Arc::downgrade(&incomplete.health)),
            Ok(OpenControlWriter::RepairRequired(_))
        ));

        let oversized = Fixture::new();
        fs::write(oversized.log_path(), vec![b'x'; MAX_LINE_BYTES + 1]).expect("oversized tail");
        assert!(matches!(
            open_control_writer(oversized.root.path(), Arc::downgrade(&oversized.health)),
            Err(ControlStoreError::ResourceLimit)
        ));
    }

    #[test]
    fn unsafe_roots_entries_and_lock_loss_fail_closed() {
        let fixture = Fixture::new();
        assert!(matches!(
            open_control_writer(Path::new("relative"), Arc::downgrade(&fixture.health)),
            Err(ControlStoreError::UnsafeRoot)
        ));

        let weak = tempfile::tempdir().expect("weak root");
        fs::set_permissions(weak.path(), fs::Permissions::from_mode(0o755))
            .expect("weak permissions");
        assert!(matches!(
            open_control_writer(weak.path(), Arc::downgrade(&fixture.health)),
            Err(ControlStoreError::UnsafeRoot)
        ));

        let special = Fixture::new();
        fs::remove_file(special.log_path()).expect("remove temporary test log");
        symlink("/dev/null", special.log_path()).expect("special log symlink");
        assert!(open_control_writer(special.root.path(), Arc::downgrade(&special.health)).is_err());

        let lost = Fixture::new();
        let writer = lost.ready();
        lost.health.invalidate();
        assert!(matches!(
            writer.prepare(combined_prepare("command-lock-lost", 'a')),
            Err(ControlAppendFailure::Rejected {
                error: ControlStoreError::LockUnavailable,
                ..
            })
        ));
        let dropped = Fixture::new();
        let weak_health = Arc::downgrade(&dropped.health);
        drop(dropped.health);
        assert!(matches!(
            open_control_writer(dropped.root.path(), weak_health),
            Err(ControlStoreError::LockUnavailable)
        ));
    }

    #[test]
    fn catalog_and_fact_shape_conflicts_are_rejected_before_write() {
        let fixture = Fixture::new();
        let writer = fixture.ready();
        let prepared = prepared_with(GLOBAL_TX, command("command-catalog", 'a'), true, true);
        let failure = writer.prepare(PrepareControlRequest {
            project_allocation: None,
            session_allocation: None,
            prepared,
        });
        let writer = match failure {
            Err(ControlAppendFailure::Rejected { writer, error }) => {
                assert!(matches!(error, ControlStoreError::CatalogMismatch));
                writer
            }
            _ => panic!("missing allocations must reject"),
        };
        let too_many = (0..=MAX_CONTROL_FACTS)
            .map(|index| StoredControlFactV1::ProjectAllocated {
                project_id: format!("project-{index}"),
            })
            .collect();
        assert!(matches!(
            writer.append_facts(too_many, false),
            Err(ControlAppendFailure::Rejected {
                error: ControlStoreError::ChecksumChainMismatch,
                ..
            })
        ));
        assert_eq!(
            fs::metadata(fixture.log_path()).expect("control log").len(),
            0
        );
    }
}
