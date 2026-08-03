//! B4 持久化组合根与实例锁能力。
//!
//! 本模块持有进程级实例锁，以及 B2、B3 和 control writer 消费的私有健康能力。

#![allow(dead_code, reason = "部分恢复辅助类型只由冻结的持久化恢复向量直接覆盖")]
#![allow(
    clippy::missing_errors_doc,
    reason = "运行时通过统一的类型化启动与恢复错误边界报告失败"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Cursor, Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::{Condvar, Mutex, OnceLock};

use rand::{RngCore as _, TryRngCore as _};
use rustix::fs::{CWD, FlockOperation, Mode, OFlags, flock, fstat, fsync, openat};
use thiserror::Error;

use crate::artifact_store::{
    ArtifactAuditPlanV1, ArtifactRecoveryGuard, ArtifactStore, ExpectedArtifact, StoreError,
    VerifiedBlobFile,
};
use crate::control_store::{
    AggregateCommitV1, CommitControlRequest, ControlAppendFailure, ControlProjection,
    ControlRecoveryOutcome, MAX_EXTERNAL_PREPARED, MAX_TOTAL_PREPARED, OpenControlWriter,
    PrepareControlRequest, PreparedTransactionV1, ReadyControlWriter, SessionAllocation,
    SessionAllocationCatalogContext, open_control_writer,
};
#[cfg(test)]
use crate::control_store::{
    ControlAppendFailpoint, ControlCapacity, ControlOpenFailpoint, ControlRecoveryFailpoint,
    MAX_INTERNAL_RESTART_PREPARED, append_validated_control_fixture,
    open_control_writer_with_failpoint,
};
use crate::domain::{
    ArtifactAvailability, ArtifactHash, BranchId, DomainProjectId, ProjectEvent, ScoreId, TakeId,
};
use crate::protocol::{
    ApprovalDecision, ApprovalId, ApprovalPayload, ApprovalStatus, ArtifactDurability,
    ArtifactKind, ArtifactManifest, ArtifactOccurrenceId, ArtifactProducer, ChoiceId,
    ClientCommand, ClientCommandId, ClientId, CommandOutcome, CommandResult, EffectClass,
    EventPage, PROTOCOL_VERSION, PendingApproval, ProjectId as ProtocolProjectId,
    ProjectSnapshot as ProtocolProjectSnapshot, ProtocolErrorCode, QuestionAnswer, QuestionChoice,
    QuestionId, QuestionStatus, SessionId, SessionSnapshot, StreamCursor, TurnId, TurnSnapshot,
    TurnStatus,
};
use crate::state::ProjectSnapshot;
use crate::state_store::session::{
    ApprovalSubjectInputsV1, CommandOnlyReasonV1, OpenSessionWriter, PublishedSessionReadState,
    ReadySessionWriter, SessionAppendFailure, SessionAppendRequest, SessionCursorError,
    SessionRecoveryOutcome, SessionRolloutEvent, StoredCommandOnlyAuthorizationV1,
    plan_coordinated_restart_reconciliation,
};
use crate::state_store::{
    AppendFailure, AppendRequest, CommittedOccurrenceFact, OpenProjectWriter, ReadyProjectWriter,
    RecoveredArtifactProjectHandoff, RecoveryOutcome, StateStore, StateStoreInstanceLease,
    StoredCommandRecordV1, StoredProjectPlanV1, StoredSessionPlanV1, TransactionCommit,
    TransactionProbe, recover_artifact_for_project_plan,
};

const INSTANCE_LOCK_FILE: &str = "instance-lock-v1";
const ROOT_MODE: u32 = 0o700;
const LOCK_MODE: u32 = 0o600;
const DURABLE_FIXTURE_BYTES: &[u8] = b"piano: o4 c8 d e f g a b > c\n";
const DURABLE_FIXTURE_HASH: &str =
    "sha256:de66932c53e0e50127757614e9925d0b3675571c7298f944dc0c736f1b3a1be8";
const DURABLE_FIXTURE_SIZE_BYTES: u64 = 29;
const DURABLE_FIXTURE_KIND: ArtifactKind = ArtifactKind::AldaSource;
const DURABLE_FIXTURE_MIME_TYPE: &str = "text/x-alda; charset=utf-8";
const DURABLE_FIXTURE_PRODUCER: ArtifactProducer = ArtifactProducer::FakeProviderFixtureV1;
const DURABLE_FIXTURE_VERSION: u32 = 1;
const DURABLE_FIXTURE_PROVENANCE_LABEL: &str = "A3 deterministic Fake Provider Alda source fixture";
const DURABLE_FIXTURE_DURABILITY: ArtifactDurability = ArtifactDurability::DurableLocal;
const ID_ALLOCATION_ATTEMPTS: usize = 32;
const TURN_START_QUESTION_PROMPT: &str = "请选择作品长度";
const APPROVAL_PROVIDER_ORIGIN: &str = "https://api.openai.com";
const APPROVAL_ACTION: &str = "Send the Fake Action Plan fields to the configured model provider";
const APPROVAL_SCOPE: &str = "prompt, constraints";
const APPROVAL_ESTIMATED_IMPACT: &str = "The listed fields would leave the local process";
const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

#[derive(Debug, Error)]
pub enum DurableRuntimeError {
    #[error("data root must be an explicit absolute private 0700 directory")]
    InvalidDataRoot,
    #[error("another durable service instance already owns this data root")]
    InstanceAlreadyRunning,
    #[error("the durable instance lock is no longer healthy")]
    InstanceLockLost,
    #[error("durable filesystem operation failed: {operation}")]
    Io {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("durable {component} rejected persisted state: {detail}")]
    Component {
        component: &'static str,
        detail: String,
    },
    #[error("durable control catalog and aggregate directories disagree")]
    CatalogMismatch,
    #[error("durable transaction conflicts with an aggregate head or plan")]
    TransactionConflict,
    #[error("durable runtime failpoint injected after {stage}")]
    InjectedFailure { stage: &'static str },
}

fn component_error(component: &'static str, error: impl std::fmt::Display) -> DurableRuntimeError {
    DurableRuntimeError::Component {
        component,
        detail: error.to_string(),
    }
}

/// 共享存活见证；只有组合根持有强引用，store 仅接收 `Weak<LockHealth>`，
/// 因而无法自行铸造或重新激活该见证。
#[derive(Debug)]
pub(crate) struct LockHealth {
    live: AtomicBool,
}

impl LockHealth {
    pub(crate) fn new() -> Self {
        Self {
            live: AtomicBool::new(true),
        }
    }

    pub(crate) fn require_live(&self) -> Result<(), DurableRuntimeError> {
        if self.live.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(DurableRuntimeError::InstanceLockLost)
        }
    }

    pub(crate) fn invalidate(&self) {
        self.live.store(false, Ordering::Release);
    }
}

/// 不可克隆的内核 advisory lock；其文件描述符不会暴露、复制或移交给单个 store。
struct InstanceLock {
    root: OwnedFd,
    file: File,
    #[cfg(test)]
    _spawn_guard: TestInstanceLockGuard,
}

#[cfg(test)]
#[derive(Default)]
struct TestProcessSpawnState {
    live_instance_locks: usize,
    spawning: bool,
}

#[cfg(test)]
struct TestProcessSpawnGate {
    state: Mutex<TestProcessSpawnState>,
    changed: Condvar,
}

#[cfg(test)]
struct TestInstanceLockGuard;

#[cfg(test)]
impl Drop for TestInstanceLockGuard {
    fn drop(&mut self) {
        let gate = test_process_spawn_gate();
        let mut state = gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.live_instance_locks -= 1;
        gate.changed.notify_all();
    }
}

#[cfg(test)]
fn test_process_spawn_gate() -> &'static TestProcessSpawnGate {
    static GATE: OnceLock<TestProcessSpawnGate> = OnceLock::new();
    GATE.get_or_init(|| TestProcessSpawnGate {
        state: Mutex::new(TestProcessSpawnState::default()),
        changed: Condvar::new(),
    })
}

#[cfg(test)]
fn register_test_instance_lock() -> TestInstanceLockGuard {
    let gate = test_process_spawn_gate();
    let mut state = gate
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while state.spawning {
        state = gate
            .changed
            .wait(state)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
    state.live_instance_locks += 1;
    TestInstanceLockGuard
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LockFailpoint {
    Create,
    Write,
    FileSync,
    DirectorySync,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DomainIdKind {
    Project,
    Score,
    Take,
    Branch,
    Session,
    Turn,
    Question,
    Approval,
    Occurrence,
}

impl DomainIdKind {
    const fn prefix(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Score => "score",
            Self::Take => "take",
            Self::Branch => "branch",
            Self::Session => "session",
            Self::Turn => "turn",
            Self::Question => "question",
            Self::Approval => "approval",
            Self::Occurrence => "occurrence",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AllocatedDomainId {
    Project(DomainProjectId),
    Score(ScoreId),
    Take(TakeId),
    Branch(BranchId),
    Session(SessionId),
    Turn(TurnId),
    Question(QuestionId),
    Approval(ApprovalId),
    Occurrence(ArtifactOccurrenceId),
}

impl AllocatedDomainId {
    fn from_candidate(
        kind: DomainIdKind,
        candidate: String,
    ) -> Result<Self, DomainIdAllocationError> {
        match kind {
            DomainIdKind::Project => DomainProjectId::parse(candidate)
                .map(Self::Project)
                .map_err(|_| DomainIdAllocationError::EntropyUnavailable),
            DomainIdKind::Score => ScoreId::parse(candidate)
                .map(Self::Score)
                .map_err(|_| DomainIdAllocationError::EntropyUnavailable),
            DomainIdKind::Take => TakeId::parse(candidate)
                .map(Self::Take)
                .map_err(|_| DomainIdAllocationError::EntropyUnavailable),
            DomainIdKind::Branch => BranchId::parse(candidate)
                .map(Self::Branch)
                .map_err(|_| DomainIdAllocationError::EntropyUnavailable),
            DomainIdKind::Session => Ok(Self::Session(SessionId(candidate))),
            DomainIdKind::Turn => Ok(Self::Turn(TurnId(candidate))),
            DomainIdKind::Question => Ok(Self::Question(QuestionId(candidate))),
            DomainIdKind::Approval => Ok(Self::Approval(ApprovalId(candidate))),
            DomainIdKind::Occurrence => Ok(Self::Occurrence(ArtifactOccurrenceId(candidate))),
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Project(id) => id.as_str(),
            Self::Score(id) => id.as_str(),
            Self::Take(id) => id.as_str(),
            Self::Branch(id) => id.as_str(),
            Self::Session(id) => &id.0,
            Self::Turn(id) => &id.0,
            Self::Question(id) => &id.0,
            Self::Approval(id) => &id.0,
            Self::Occurrence(id) => &id.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum DomainIdAllocationError {
    #[error("安全随机源暂不可用")]
    EntropyUnavailable,
    #[error("ID 分配尝试次数已耗尽")]
    Exhausted,
}

impl DomainIdAllocationError {
    pub(crate) const fn protocol_code(self) -> ProtocolErrorCode {
        match self {
            Self::EntropyUnavailable | Self::Exhausted => ProtocolErrorCode::ServiceUnavailable,
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("外部命令容量已耗尽")]
pub(crate) struct ExternalCapacityError;

impl ExternalCapacityError {
    pub(crate) const fn protocol_code(self) -> ProtocolErrorCode {
        match self {
            Self => ProtocolErrorCode::ServiceUnavailable,
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct GlobalTransactionId(String);

impl GlobalTransactionId {
    fn new(value: String) -> Option<Self> {
        is_prefixed_hex_128(&value, "global").then_some(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn into_inner(self) -> String {
        self.0
    }

    pub(crate) fn project_transaction_id(&self) -> String {
        format!("{}:project", self.0)
    }

    pub(crate) fn session_transaction_id(&self) -> String {
        format!("{}:session", self.0)
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum GlobalTransactionIdError {
    #[error("安全随机源暂不可用")]
    EntropyUnavailable,
    #[error("global transaction ID 分配尝试次数已耗尽")]
    Exhausted,
}

/// 固定 Artifact 已耐久、但尚未交给权威 Prepared 的一次性能力。
///
/// 字段保持私有，且该类型不实现 `Clone`，从而只能选择进入 Prepared 或失败分类之一。
pub(crate) struct DurableFixturePrepared {
    record: crate::domain::ArtifactRecord,
    audit_plan: ArtifactAuditPlanV1,
    pending_reference: PendingArtifactReference,
}

/// Approval approve 已完成 Artifact put、但尚未写入 control Prepared 的完整计划。
pub(crate) struct DurableApprovalApprovePlan {
    request: PrepareControlRequest,
    pending_reference: Option<PendingArtifactReference>,
}

impl DurableApprovalApprovePlan {
    pub(crate) fn into_parts(self) -> (PrepareControlRequest, Option<PendingArtifactReference>) {
        (self.request, self.pending_reference)
    }
}

impl DurableFixturePrepared {
    pub(crate) fn into_prepared_facts(
        self,
    ) -> (crate::domain::ArtifactRecord, ArtifactAuditPlanV1) {
        let Self {
            record,
            audit_plan,
            pending_reference: _,
        } = self;
        (record, audit_plan)
    }

    fn into_prepared_facts_and_reference(
        self,
    ) -> (
        crate::domain::ArtifactRecord,
        ArtifactAuditPlanV1,
        PendingArtifactReference,
    ) {
        let Self {
            record,
            audit_plan,
            pending_reference,
        } = self;
        (record, audit_plan, pending_reference)
    }
}

/// 尚未由权威 Prepared 建立引用的一次性分类能力。
pub(crate) struct PendingArtifactReference {
    hash: ArtifactHash,
    published_generation: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct OrphanArtifact {
    hash: ArtifactHash,
}

impl OrphanArtifact {
    pub(crate) fn hash(&self) -> &ArtifactHash {
        &self.hash
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ArtifactReferenceDisposition {
    AlreadyReachable,
    OrphanCandidate(OrphanArtifact),
}

impl GlobalTransactionIdError {
    pub(crate) const fn protocol_code(self) -> ProtocolErrorCode {
        match self {
            Self::EntropyUnavailable | Self::Exhausted => ProtocolErrorCode::ServiceUnavailable,
        }
    }
}

impl InstanceLock {
    fn acquire(root_path: &Path) -> Result<Self, DurableRuntimeError> {
        Self::acquire_inner(root_path, None)
    }

    fn acquire_inner(
        root_path: &Path,
        #[cfg_attr(not(test), allow(unused_variables))] failpoint: Option<LockFailpoint>,
    ) -> Result<Self, DurableRuntimeError> {
        #[cfg(test)]
        let spawn_guard = register_test_instance_lock();
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (root_path, failpoint);
            return Err(DurableRuntimeError::InvalidDataRoot);
        }
        #[cfg(target_os = "linux")]
        {
            let root = open_absolute_private_root(root_path)?;
            #[cfg(test)]
            inject_lock(failpoint, LockFailpoint::Create, "test lock create")?;
            let fd = openat(
                &root,
                INSTANCE_LOCK_FILE,
                OFlags::RDWR
                    | OFlags::CREATE
                    | OFlags::NOFOLLOW
                    | OFlags::CLOEXEC
                    | OFlags::NONBLOCK,
                Mode::from_raw_mode(LOCK_MODE),
            )
            .map_err(|source| runtime_io("open instance lock", source))?;
            let mut file = File::from(fd);
            validate_lock_file(&file)?;
            match flock(&file, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => {}
                Err(rustix::io::Errno::WOULDBLOCK) => {
                    return Err(DurableRuntimeError::InstanceAlreadyRunning);
                }
                Err(source) => return Err(runtime_io("acquire instance lock", source)),
            }

            file.set_len(0)
                .map_err(|source| runtime_io("truncate instance lock", source))?;
            file.seek(SeekFrom::Start(0))
                .map_err(|source| runtime_io("seek instance lock", source))?;
            #[cfg(test)]
            inject_lock(failpoint, LockFailpoint::Write, "test lock write")?;
            let diagnostic = format!(
                "pid={}\nstart_nonce={}\n",
                std::process::id(),
                random_hex_128()
            );
            file.write_all(diagnostic.as_bytes())
                .map_err(|source| runtime_io("write instance lock", source))?;
            file.flush()
                .map_err(|source| runtime_io("flush instance lock", source))?;
            #[cfg(test)]
            inject_lock(failpoint, LockFailpoint::FileSync, "test lock file sync")?;
            file.sync_all()
                .map_err(|source| runtime_io("sync instance lock", source))?;
            #[cfg(test)]
            inject_lock(
                failpoint,
                LockFailpoint::DirectorySync,
                "test lock directory sync",
            )?;
            fsync(&root).map_err(|source| runtime_io("sync data root", source))?;
            Ok(Self {
                root,
                file,
                #[cfg(test)]
                _spawn_guard: spawn_guard,
            })
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectReadState {
    snapshot: ProjectSnapshot,
    last_sequence: u64,
    last_checksum: String,
}

impl ProjectReadState {
    fn from_writer(writer: &ReadyProjectWriter) -> Result<Self, DurableRuntimeError> {
        let (last_sequence, last_checksum) = writer.head();
        let state = Self {
            snapshot: writer.snapshot().clone(),
            last_sequence,
            last_checksum: last_checksum
                .map(ToOwned::to_owned)
                .ok_or(DurableRuntimeError::CatalogMismatch)?,
        };
        state.validate()?;
        Ok(state)
    }

    fn validate(&self) -> Result<(), DurableRuntimeError> {
        if self.snapshot.project_id.is_none()
            || self.snapshot.last_sequence != self.last_sequence
            || !is_sha256(&self.last_checksum)
        {
            return Err(DurableRuntimeError::CatalogMismatch);
        }
        for record in self.snapshot.artifacts.values() {
            record
                .validate_audit()
                .map_err(|error| component_error("Project read state", error))?;
        }
        Ok(())
    }
}

impl std::ops::Deref for ProjectReadState {
    type Target = ProjectSnapshot;

    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct OwnerIndex {
    turns: BTreeMap<String, String>,
    questions: BTreeMap<String, String>,
    approvals: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AggregateHead {
    pub last_sequence: u64,
    pub last_checksum: String,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum SessionObjectRef<'a> {
    Turn(&'a TurnId),
    Question(&'a QuestionId),
    Approval(&'a ApprovalId),
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub(crate) enum SessionReadError {
    #[error("Session not found")]
    SessionNotFound,
    #[error("Turn not found")]
    TurnNotFound,
    #[error("Turn belongs to another Session")]
    TurnOwnershipMismatch,
    #[error("published Session owner index and projection disagree")]
    CorruptPublishedView,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub(crate) enum DurableCursorError {
    #[error("unsupported stream kind")]
    UnsupportedStreamKind,
    #[error("Session not found")]
    SessionNotFound,
    #[error("cursor epoch does not match the published Session epoch")]
    EpochMismatch {
        expected_epoch: u64,
        actual_epoch: u64,
        head_sequence: u64,
    },
    #[error("cursor is ahead of the published Session head")]
    Future { head_sequence: u64 },
    #[error("published Session identity is inconsistent")]
    CorruptPublishedView,
}

/// 仅在完整候选校验后一次性替换的不可变查询 generation。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DurableReadView {
    pub projects: BTreeMap<String, ProjectReadState>,
    pub sessions: BTreeMap<String, PublishedSessionReadState>,
    owners: OwnerIndex,
    project_metadata: BTreeMap<String, ProtocolProjectSnapshot>,
    occurrence_metadata: BTreeMap<String, ArtifactManifest>,
    reachable_artifact_hashes: BTreeSet<ArtifactHash>,
    generation: u64,
}

impl DurableReadView {
    fn validate(&self, control: &ControlProjection) -> Result<(), DurableRuntimeError> {
        if self.generation == 0
            || self.projects.keys().ne(control.projects.iter())
            || self.sessions.keys().ne(control.sessions.keys())
        {
            return Err(DurableRuntimeError::CatalogMismatch);
        }
        for (project_id, state) in &self.projects {
            state.validate()?;
            if state
                .snapshot
                .project_id
                .as_ref()
                .map(DomainProjectId::as_str)
                != Some(project_id.as_str())
            {
                return Err(DurableRuntimeError::CatalogMismatch);
            }
        }
        for (session_id, state) in &self.sessions {
            state
                .validate()
                .map_err(|error| component_error("Session read state", error))?;
            if state.snapshot().session_id.0 != *session_id
                || control.sessions.get(session_id) != Some(&state.snapshot().project_id.0)
            {
                return Err(DurableRuntimeError::CatalogMismatch);
            }
        }
        let (owners, reachable_artifact_hashes) = self.derived_indexes()?;
        let project_metadata = rebuild_project_metadata(control, &self.projects)?;
        let occurrence_metadata =
            rebuild_occurrence_metadata(control, &self.projects, &self.sessions)?;
        if owners != self.owners
            || project_metadata != self.project_metadata
            || occurrence_metadata != self.occurrence_metadata
            || reachable_artifact_hashes != self.reachable_artifact_hashes
        {
            return Err(DurableRuntimeError::CatalogMismatch);
        }
        Ok(())
    }

    fn refresh_derived_indexes(
        &mut self,
        control: &ControlProjection,
    ) -> Result<(), DurableRuntimeError> {
        let (owners, reachable_artifact_hashes) = self.derived_indexes()?;
        self.owners = owners;
        self.project_metadata = rebuild_project_metadata(control, &self.projects)?;
        self.occurrence_metadata =
            rebuild_occurrence_metadata(control, &self.projects, &self.sessions)?;
        self.reachable_artifact_hashes = reachable_artifact_hashes;
        Ok(())
    }

    fn derived_indexes(&self) -> Result<(OwnerIndex, BTreeSet<ArtifactHash>), DurableRuntimeError> {
        let mut owners = OwnerIndex::default();
        for (session_id, state) in &self.sessions {
            for id in state.turn_ids() {
                insert_owner(&mut owners.turns, id, session_id)?;
            }
            for id in state.question_ids() {
                insert_owner(&mut owners.questions, id, session_id)?;
            }
            for id in state.approval_ids() {
                insert_owner(&mut owners.approvals, id, session_id)?;
            }
        }
        let reachable = self
            .projects
            .values()
            .flat_map(|state| state.snapshot.artifacts.values())
            .filter(|record| record.availability() == ArtifactAvailability::VerifiedDurable)
            .map(|record| record.hash().clone())
            .collect();
        Ok((owners, reachable))
    }
}

struct ApprovalDecisionProvenance<'a> {
    approval: &'a PendingApproval,
    project_id: String,
    session_id: String,
    terminal_sequence: u64,
}

#[allow(
    clippy::too_many_lines,
    reason = "单次扫描必须连续分类普通 reply、approve occurrence 与 deny 闭包"
)]
fn rebuild_occurrence_metadata(
    control: &ControlProjection,
    projects: &BTreeMap<String, ProjectReadState>,
    sessions: &BTreeMap<String, PublishedSessionReadState>,
) -> Result<BTreeMap<String, ArtifactManifest>, DurableRuntimeError> {
    let mut seen = BTreeSet::new();
    let mut metadata = BTreeMap::new();
    for global_tx_id in &control.prepared_order {
        if !seen.insert(global_tx_id.as_str()) {
            return Err(DurableRuntimeError::CatalogMismatch);
        }
        let prepared = control
            .prepared
            .get(global_tx_id)
            .ok_or(DurableRuntimeError::CatalogMismatch)?;
        let Some(committed) = control.committed.get(global_tx_id) else {
            continue;
        };
        prepared
            .validate()
            .map_err(|error| component_error("occurrence metadata", error))?;
        let (_raw_reply, reply) = prepared
            .command_record
            .decode_reply_for_protocol(PROTOCOL_VERSION)
            .map_err(|error| component_error("occurrence metadata", error))?;
        let project_artifact_count = prepared
            .project_plan
            .as_ref()
            .map_or(0, |plan| plan.registered_artifact_events().len());
        let session_request = prepared
            .session_plan
            .clone()
            .map(StoredSessionPlanV1::into_append_request)
            .transpose()
            .map_err(|error| component_error("occurrence metadata", error))?;
        let approval_event_count = session_request.as_ref().map_or(0, |request| {
            request
                .events
                .iter()
                .filter(|event| matches!(event, SessionRolloutEvent::ApprovalResolved { .. }))
                .count()
        });
        let CommandOutcome::Success {
            result:
                CommandResult::ApprovalDecided {
                    approval,
                    artifact_manifest,
                },
        } = &reply.outcome
        else {
            if project_artifact_count != 0
                || !prepared.artifact_audit_plans.is_empty()
                || approval_event_count != 0
            {
                return Err(DurableRuntimeError::CatalogMismatch);
            }
            continue;
        };

        let decision = validate_approval_decision_provenance(
            control,
            sessions,
            prepared,
            committed,
            session_request.as_ref(),
            approval,
        )?;
        match (approval.decision, artifact_manifest) {
            (Some(ApprovalDecision::Approve), Some(manifest)) => {
                let project_plan = prepared
                    .project_plan
                    .as_ref()
                    .ok_or(DurableRuntimeError::CatalogMismatch)?;
                let [audit_plan] = prepared.artifact_audit_plans.as_slice() else {
                    return Err(DurableRuntimeError::CatalogMismatch);
                };
                if project_artifact_count != 1 || committed.project_last.is_none() {
                    return Err(DurableRuntimeError::CatalogMismatch);
                }
                let fact = project_plan
                    .committed_occurrence_fact(&prepared.global_tx_id, audit_plan)
                    .map_err(|error| component_error("occurrence metadata", error))?;
                validate_occurrence_manifest(projects, project_plan, &decision, &fact, manifest)?;
                if metadata
                    .insert(manifest.artifact_occurrence_id.0.clone(), manifest.clone())
                    .is_some()
                {
                    return Err(DurableRuntimeError::CatalogMismatch);
                }
            }
            (Some(ApprovalDecision::Deny), None) => {
                if prepared.project_plan.is_some()
                    || !prepared.artifact_audit_plans.is_empty()
                    || committed.project_last.is_some()
                    || project_artifact_count != 0
                {
                    return Err(DurableRuntimeError::CatalogMismatch);
                }
            }
            _ => return Err(DurableRuntimeError::CatalogMismatch),
        }
    }
    if seen.len() != control.prepared.len()
        || control
            .committed
            .keys()
            .any(|global_tx_id| !seen.contains(global_tx_id.as_str()))
    {
        return Err(DurableRuntimeError::CatalogMismatch);
    }
    Ok(metadata)
}

fn validate_approval_decision_provenance<'a>(
    control: &ControlProjection,
    sessions: &BTreeMap<String, PublishedSessionReadState>,
    prepared: &PreparedTransactionV1,
    committed: &crate::control_store::CommittedTransactionV1,
    session_request: Option<&crate::state_store::session::SessionAppendRequest>,
    approval: &'a PendingApproval,
) -> Result<ApprovalDecisionProvenance<'a>, DurableRuntimeError> {
    let plan = prepared
        .session_plan
        .as_ref()
        .ok_or(DurableRuntimeError::CatalogMismatch)?;
    let request = session_request.ok_or(DurableRuntimeError::CatalogMismatch)?;
    let (resolved, terminal_event) = match request.events.as_slice() {
        [resolved @ SessionRolloutEvent::ApprovalResolved { .. }] => (resolved, None),
        [
            resolved @ SessionRolloutEvent::ApprovalResolved { .. },
            completed,
        ] => (resolved, Some(completed)),
        _ => return Err(DurableRuntimeError::CatalogMismatch),
    };
    let SessionRolloutEvent::ApprovalResolved {
        approval_id,
        approval_subject_digest,
        decision,
        responder_client_id,
    } = resolved
    else {
        return Err(DurableRuntimeError::CatalogMismatch);
    };
    let anchor = committed
        .session_last
        .as_ref()
        .ok_or(DurableRuntimeError::CatalogMismatch)?;
    let terminal_sequence = plan
        .expected_pre_sequence()
        .checked_add(1)
        .ok_or(DurableRuntimeError::CatalogMismatch)?;
    let resulting_last_sequence = plan
        .expected_pre_sequence()
        .checked_add(
            u64::try_from(request.events.len())
                .map_err(|_| DurableRuntimeError::CatalogMismatch)?,
        )
        .ok_or(DurableRuntimeError::CatalogMismatch)?;
    let session_id = plan
        .session_id()
        .map_err(|error| component_error("occurrence metadata", error))?;
    let project_id = plan
        .expected_project_id()
        .map_err(|error| component_error("occurrence metadata", error))?;
    let expected_status = match decision {
        ApprovalDecision::Approve => ApprovalStatus::Approved,
        ApprovalDecision::Deny => ApprovalStatus::Denied,
    };
    let published = sessions
        .get(&session_id.0)
        .ok_or(DurableRuntimeError::CatalogMismatch)?;
    let projected_approval = published
        .snapshot()
        .approvals
        .iter()
        .find(|candidate| candidate.approval_id == approval.approval_id)
        .ok_or(DurableRuntimeError::CatalogMismatch)?;
    let terminal_matches = terminal_event.is_none_or(|event| {
        let SessionRolloutEvent::TurnCompleted { turn_id, status } = event else {
            return false;
        };
        let expected_turn_status = match decision {
            ApprovalDecision::Approve => TurnStatus::Succeeded,
            ApprovalDecision::Deny => TurnStatus::Failed,
        };
        *turn_id == approval.owner_turn_id
            && *status == expected_turn_status
            && published.snapshot().turns.iter().any(|turn| {
                turn.turn_id == *turn_id
                    && turn.status == expected_turn_status
                    && turn.terminal_sequence == Some(resulting_last_sequence)
            })
    });
    if request.command_record.as_ref() != Some(&prepared.command_record)
        || approval_id != &approval.approval_id
        || approval_subject_digest != &approval.approval_subject_digest
        || approval.decision != Some(*decision)
        || approval.status != expected_status
        || approval.responder_client_id.as_ref() != Some(responder_client_id)
        || approval.session_id != session_id
        || approval.terminal_sequence != Some(terminal_sequence)
        || approval.created_sequence >= terminal_sequence
        || anchor.resulting_last_sequence != resulting_last_sequence
        || !is_sha256(&anchor.resulting_batch_checksum)
        || control.sessions.get(&session_id.0) != Some(&project_id.0)
        || published.snapshot().project_id != project_id
        || projected_approval != approval
        || !terminal_matches
    {
        return Err(DurableRuntimeError::CatalogMismatch);
    }
    Ok(ApprovalDecisionProvenance {
        approval,
        project_id: project_id.0,
        session_id: session_id.0,
        terminal_sequence,
    })
}

fn validate_occurrence_manifest(
    projects: &BTreeMap<String, ProjectReadState>,
    project_plan: &StoredProjectPlanV1,
    decision: &ApprovalDecisionProvenance<'_>,
    fact: &CommittedOccurrenceFact,
    manifest: &ArtifactManifest,
) -> Result<(), DurableRuntimeError> {
    let plan_project_id = project_plan
        .project_id()
        .map_err(|error| component_error("occurrence metadata", error))?;
    let state = projects
        .get(plan_project_id.as_str())
        .ok_or(DurableRuntimeError::CatalogMismatch)?;
    let record = state
        .snapshot
        .artifacts
        .get(fact.hash())
        .ok_or(DurableRuntimeError::CatalogMismatch)?;
    if manifest.artifact_occurrence_id.0.is_empty()
        || manifest
            .artifact_occurrence_id
            .0
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        || manifest.artifact_hash.as_str() != DURABLE_FIXTURE_HASH
        || manifest.artifact_hash.as_str() != fact.hash().as_str()
        || manifest.size_bytes != DURABLE_FIXTURE_SIZE_BYTES
        || manifest.size_bytes != fact.size()
        || manifest.kind != DURABLE_FIXTURE_KIND
        || manifest.mime_type != DURABLE_FIXTURE_MIME_TYPE
        || manifest.producer != DURABLE_FIXTURE_PRODUCER
        || manifest.fixture_version != DURABLE_FIXTURE_VERSION
        || manifest.provenance_label != DURABLE_FIXTURE_PROVENANCE_LABEL
        || manifest.durability != DURABLE_FIXTURE_DURABILITY
        || manifest.project_id.0 != decision.project_id
        || manifest.project_id.0 != plan_project_id.as_str()
        || manifest.source_session_id.0 != decision.session_id
        || manifest.source_session_id != decision.approval.session_id
        || manifest.source_turn_id != decision.approval.owner_turn_id
        || manifest.created_sequence != decision.terminal_sequence
        || fact.control_transaction_id().is_empty()
        || record.availability() != ArtifactAvailability::VerifiedDurable
        || record.hash() != fact.hash()
        || record.size() != fact.size()
        || record.layout_version() != Some(fact.layout_version())
        || record.store_instance_id() != Some(fact.store_instance_id())
        || record.durability() != Some(fact.durability())
        || record.store_commit_identity() != Some(fact.commit_identity())
    {
        return Err(DurableRuntimeError::CatalogMismatch);
    }
    Ok(())
}

fn rebuild_project_metadata(
    control: &ControlProjection,
    projects: &BTreeMap<String, ProjectReadState>,
) -> Result<BTreeMap<String, ProtocolProjectSnapshot>, DurableRuntimeError> {
    let mut seen = BTreeSet::new();
    let mut metadata = BTreeMap::new();
    for global_tx_id in &control.prepared_order {
        if !seen.insert(global_tx_id.as_str()) {
            return Err(DurableRuntimeError::CatalogMismatch);
        }
        let prepared = control
            .prepared
            .get(global_tx_id)
            .ok_or(DurableRuntimeError::CatalogMismatch)?;
        let Some(committed) = control.committed.get(global_tx_id) else {
            continue;
        };
        prepared
            .validate()
            .map_err(|error| component_error("Project metadata", error))?;
        let (_raw_reply, reply) = prepared
            .command_record
            .decode_reply_for_protocol(PROTOCOL_VERSION)
            .map_err(|error| component_error("Project metadata", error))?;
        let created = match reply.outcome {
            CommandOutcome::Success {
                result: CommandResult::ProjectCreated(snapshot),
            } => Some(snapshot),
            _ => None,
        };
        let creation_plan = prepared
            .project_plan
            .as_ref()
            .is_some_and(|plan| plan.expected_pre_sequence() == 0);
        match (created, creation_plan) {
            (Some(snapshot), true) => {
                validate_project_created(prepared, committed, projects, &snapshot)?;
                if metadata
                    .insert(snapshot.project_id.0.clone(), snapshot)
                    .is_some()
                {
                    return Err(DurableRuntimeError::CatalogMismatch);
                }
            }
            (None, false) => {}
            (Some(_), false) | (None, true) => {
                return Err(DurableRuntimeError::CatalogMismatch);
            }
        }
    }
    if seen.len() != control.prepared.len()
        || control
            .committed
            .keys()
            .any(|global_tx_id| !seen.contains(global_tx_id.as_str()))
        || metadata.keys().ne(control.projects.iter())
    {
        return Err(DurableRuntimeError::CatalogMismatch);
    }
    Ok(metadata)
}

fn validate_project_created(
    prepared: &PreparedTransactionV1,
    committed: &crate::control_store::CommittedTransactionV1,
    projects: &BTreeMap<String, ProjectReadState>,
    metadata: &ProtocolProjectSnapshot,
) -> Result<(), DurableRuntimeError> {
    let plan = prepared
        .project_plan
        .as_ref()
        .ok_or(DurableRuntimeError::CatalogMismatch)?;
    let anchor = committed
        .project_last
        .as_ref()
        .ok_or(DurableRuntimeError::CatalogMismatch)?;
    if prepared.session_plan.is_some()
        || !prepared.artifact_audit_plans.is_empty()
        || committed.session_last.is_some()
        || plan.expected_pre_sequence() != 0
        || plan.expected_pre_batch_checksum().is_some()
        || anchor.resulting_last_sequence != 1
        || !is_sha256(&anchor.resulting_batch_checksum)
        || metadata.version != 1
    {
        return Err(DurableRuntimeError::CatalogMismatch);
    }
    let plan_project_id = plan
        .project_id()
        .map_err(|error| component_error("Project metadata", error))?;
    let request = plan
        .clone()
        .into_append_request(Vec::new())
        .map_err(|error| component_error("Project metadata", error))?;
    if request.command_record.as_ref() != Some(&prepared.command_record) {
        return Err(DurableRuntimeError::CatalogMismatch);
    }
    let [
        ProjectEvent::ProjectInitialized {
            project_id,
            score_id,
            default_take_id,
            default_branch_id,
        },
    ] = request.events.as_slice()
    else {
        return Err(DurableRuntimeError::CatalogMismatch);
    };
    if project_id != &plan_project_id
        || metadata.project_id.0 != plan_project_id.as_str()
        || !projects.contains_key(plan_project_id.as_str())
    {
        return Err(DurableRuntimeError::CatalogMismatch);
    }
    let state = projects
        .get(plan_project_id.as_str())
        .ok_or(DurableRuntimeError::CatalogMismatch)?;
    let take = state
        .snapshot
        .takes
        .get(default_take_id)
        .ok_or(DurableRuntimeError::CatalogMismatch)?;
    let branch = state
        .snapshot
        .branches
        .get(default_branch_id)
        .ok_or(DurableRuntimeError::CatalogMismatch)?;
    if state.snapshot.project_id.as_ref() != Some(project_id)
        || state.snapshot.score_id.as_ref() != Some(score_id)
        || take.score_id != *score_id
        || !take.branches.contains(default_branch_id)
        || branch.score_id != *score_id
        || branch.take_id != *default_take_id
    {
        return Err(DurableRuntimeError::CatalogMismatch);
    }
    Ok(())
}

fn insert_owner(
    index: &mut BTreeMap<String, String>,
    id: &str,
    session_id: &str,
) -> Result<(), DurableRuntimeError> {
    if index.insert(id.to_owned(), session_id.to_owned()).is_some() {
        return Err(DurableRuntimeError::CatalogMismatch);
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

/// 不可克隆实例锁与全部 durable component 的唯一所有者。
///
/// component 保存在 `Option` 中，使 `Drop` 能强制执行所需关闭顺序：先 writer/store，
/// 再使健康见证失效，最后释放内核锁。
struct RuntimeCore {
    components: Option<RuntimeComponents>,
    health: Arc<LockHealth>,
    instance_lock: Option<InstanceLock>,
}

struct RuntimeComponents {
    artifact_store: ArtifactStore,
    artifact_recovery_guard: ArtifactRecoveryGuard,
    state_store: StateStore,
    control_writer: Option<ReadyControlWriter>,
    session_catalog_context: SessionAllocationCatalogContext,
}

struct StartupAggregates {
    projects: BTreeMap<String, ReadyProjectWriter>,
    sessions: BTreeMap<String, ReadySessionWriter>,
}

impl Drop for RuntimeCore {
    fn drop(&mut self) {
        drop(self.components.take());
        self.health.invalidate();
        drop(self.instance_lock.take());
    }
}

/// 启动恢复完全收敛后暴露的 runtime typestate。
pub(crate) struct ReadyDurableRuntime {
    core: RuntimeCore,
    published: DurableReadView,
    #[cfg(test)]
    failpoint: Option<RuntimeFailpoint>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum CommandLookup {
    Unseen,
    ExactReply(Vec<u8>),
}

#[derive(Debug, Error)]
pub(crate) enum CommandLookupError {
    #[error("stored command ID has a different payload")]
    IdempotencyConflict,
    #[error("committed command index is corrupt")]
    CorruptCommittedIndex(#[source] DurableRuntimeError),
}

/// durable Prepared 事实存在但尚未完成发布时的 runtime typestate；
/// 它有意不暴露查询或命令方法。
pub(crate) struct RecoveringDurableRuntime {
    core: RuntimeCore,
    prepared: PreparedTransactionV1,
    execution: PreparedExecution,
    base_published: DurableReadView,
    #[cfg(test)]
    failpoint: Option<RuntimeFailpoint>,
}

struct PreparedExecution {
    project_writer: Option<ReadyProjectWriter>,
    session_writer: Option<ReadySessionWriter>,
}

/// 终止 typestate；仅为保证有序关闭而暂时保留 composition root，不暴露任何状态操作。
pub(crate) struct FatalDurableRuntime {
    _core: RuntimeCore,
    error: DurableRuntimeError,
}

pub(crate) enum SubmitFailure {
    Rejected {
        runtime: Box<ReadyDurableRuntime>,
        error: DurableRuntimeError,
    },
    Recovering {
        runtime: Box<RecoveringDurableRuntime>,
        error: DurableRuntimeError,
    },
    Fatal(Box<FatalDurableRuntime>),
}

pub(crate) enum RecoveryFailure {
    Fatal(Box<FatalDurableRuntime>),
}

enum FinishFailure {
    Recovering {
        runtime: Box<RecoveringDurableRuntime>,
        error: DurableRuntimeError,
    },
    Fatal(Box<FatalDurableRuntime>),
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeFailpoint {
    Prepare,
    Project,
    Session,
    Commit,
    CommitRecoverySync,
    Publish,
    Response,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StartupSyncFailpoint {
    Control,
    Project,
    Session,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StartupStage {
    BeforePrepare,
    AfterPrepare,
    AfterSession,
    AfterCommit,
}

#[derive(Clone, Copy, Debug)]
struct StartupFailpoint {
    stage: StartupStage,
    occurrence: usize,
}

fn inject_startup(
    failpoint: &mut Option<StartupFailpoint>,
    stage: StartupStage,
    label: &'static str,
) -> Result<(), DurableRuntimeError> {
    let Some(actual) = failpoint.as_mut() else {
        return Ok(());
    };
    if actual.stage != stage {
        return Ok(());
    }
    if actual.occurrence > 0 {
        actual.occurrence -= 1;
        return Ok(());
    }
    Err(DurableRuntimeError::InjectedFailure { stage: label })
}

#[cfg(test)]
fn with_startup_test_capacity(
    mut writer: ReadyControlWriter,
    capacity: Option<ControlCapacity>,
) -> ReadyControlWriter {
    // 只供小规模算法单测注入计数，不构成 durable replay 或真实 startup 容量证据。
    if let Some(capacity) = capacity {
        writer.set_startup_test_capacity(capacity);
    }
    writer
}

impl ReadyDurableRuntime {
    /// 获取实例锁并打开所有 durable component，按 control 顺序完成 pending redo，
    /// 最后发布一份 committed read view。
    pub(crate) fn open(root_path: &Path) -> Result<Self, DurableRuntimeError> {
        Self::open_inner(
            root_path,
            None,
            #[cfg(test)]
            None,
            #[cfg(test)]
            None,
        )
    }

    fn open_inner(
        root_path: &Path,
        #[cfg_attr(not(test), allow(unused_variables))] mut startup_failpoint: Option<
            StartupFailpoint,
        >,
        #[cfg(test)] startup_sync_failpoint: Option<StartupSyncFailpoint>,
        #[cfg(test)] startup_test_capacity: Option<ControlCapacity>,
    ) -> Result<Self, DurableRuntimeError> {
        let instance_lock = InstanceLock::acquire(root_path)?;
        let health = Arc::new(LockHealth::new());
        let (artifact_store, artifact_recovery_guard) =
            ArtifactStore::open_for_durable_runtime(root_path)
                .map_err(|error| component_error("artifact store", error))?;
        #[cfg(test)]
        let state_store = match startup_sync_failpoint {
            Some(StartupSyncFailpoint::Project) => StateStore::open_with_failpoint(
                root_path,
                StateStoreInstanceLease::for_durable_runtime(),
                crate::state_store::InitFailpoint::EventsFileSync,
            ),
            Some(StartupSyncFailpoint::Session) => StateStore::open_with_failpoint(
                root_path,
                StateStoreInstanceLease::for_durable_runtime(),
                crate::state_store::InitFailpoint::RolloutFileSync,
            ),
            _ => StateStore::open(root_path, StateStoreInstanceLease::for_durable_runtime()),
        }
        .map_err(|error| component_error("state store", error))?;
        #[cfg(not(test))]
        let state_store =
            StateStore::open(root_path, StateStoreInstanceLease::for_durable_runtime())
                .map_err(|error| component_error("state store", error))?;
        #[cfg(test)]
        let opened_control = match startup_sync_failpoint {
            Some(StartupSyncFailpoint::Control) => open_control_writer_with_failpoint(
                root_path,
                Arc::downgrade(&health),
                ControlOpenFailpoint::FileSync,
            ),
            _ => open_control_writer(root_path, Arc::downgrade(&health)),
        };
        #[cfg(not(test))]
        let opened_control = open_control_writer(root_path, Arc::downgrade(&health));
        let control_writer =
            match opened_control.map_err(|error| component_error("control store", error))? {
                OpenControlWriter::Ready(writer) => writer,
                OpenControlWriter::RepairRequired(writer) => writer
                    .repair()
                    .map_err(|_| component_error("control store", "unrepairable final tail"))?,
            };
        #[cfg(test)]
        let control_writer = with_startup_test_capacity(control_writer, startup_test_capacity);
        let session_catalog_context = control_writer.session_allocation_catalog_context();
        let mut core = RuntimeCore {
            components: Some(RuntimeComponents {
                artifact_store,
                artifact_recovery_guard,
                state_store,
                control_writer: Some(control_writer),
                session_catalog_context,
            }),
            health,
            instance_lock: Some(instance_lock),
        };

        core.require_lock()?;
        let mut aggregates = core.open_startup_aggregates()?;
        let pending = core.control()?.projection().pending();
        for prepared in pending {
            let project_last = core.redo_startup_project(&mut aggregates, &prepared)?;
            let session_last = RuntimeCore::redo_startup_session(&mut aggregates, &prepared)?;
            core.commit_prepared_with_anchors(&prepared, project_last, session_last, false)?;
        }
        let restarts = core.plan_startup_restarts(&aggregates)?;
        for request in restarts {
            let prepared = request.prepared.clone();
            inject_startup(
                &mut startup_failpoint,
                StartupStage::BeforePrepare,
                "restart prepare",
            )?;
            core.prepare_startup_control(request)?;
            inject_startup(
                &mut startup_failpoint,
                StartupStage::AfterPrepare,
                "restart prepare",
            )?;
            let session_last = RuntimeCore::redo_startup_session(&mut aggregates, &prepared)?;
            inject_startup(
                &mut startup_failpoint,
                StartupStage::AfterSession,
                "restart Session append",
            )?;
            core.commit_prepared_with_anchors(&prepared, None, session_last, false)?;
            inject_startup(
                &mut startup_failpoint,
                StartupStage::AfterCommit,
                "restart control commit",
            )?;
        }
        core.audit_startup_transactions(&aggregates)?;
        let published = core.startup_read_view(&aggregates)?;
        Ok(Self {
            core,
            published,
            #[cfg(test)]
            failpoint: None,
        })
    }

    #[cfg(test)]
    fn open_with_startup_failpoint(
        root_path: &Path,
        failpoint: StartupFailpoint,
    ) -> Result<Self, DurableRuntimeError> {
        Self::open_inner(root_path, Some(failpoint), None, None)
    }

    #[cfg(test)]
    fn open_with_startup_sync_failpoint(
        root_path: &Path,
        failpoint: StartupSyncFailpoint,
    ) -> Result<Self, DurableRuntimeError> {
        Self::open_inner(root_path, None, Some(failpoint), None)
    }

    #[cfg(test)]
    fn open_with_startup_test_capacity(
        root_path: &Path,
        capacity: ControlCapacity,
    ) -> Result<Self, DurableRuntimeError> {
        // 精确边界门禁必须调用 `open` 并从 control JSONL replay 得到真实计数。
        Self::open_inner(root_path, None, None, Some(capacity))
    }

    pub(crate) fn read_view(&self) -> &DurableReadView {
        &self.published
    }

    pub(crate) fn project_projection(&self, id: &ProtocolProjectId) -> Option<&ProjectSnapshot> {
        self.published
            .projects
            .get(&id.0)
            .map(|state| &state.snapshot)
    }

    pub(crate) fn project_metadata(
        &self,
        id: &ProtocolProjectId,
    ) -> Option<&ProtocolProjectSnapshot> {
        self.published.project_metadata.get(&id.0)
    }

    pub(crate) fn session_snapshot(&self, id: &SessionId) -> Option<SessionSnapshot> {
        self.published
            .sessions
            .get(&id.0)
            .map(|state| state.snapshot().clone())
    }

    pub(crate) fn project_head(&self, id: &ProtocolProjectId) -> Option<AggregateHead> {
        self.published
            .projects
            .get(&id.0)
            .map(|state| AggregateHead {
                last_sequence: state.last_sequence,
                last_checksum: state.last_checksum.clone(),
            })
    }

    pub(crate) fn session_head(&self, id: &SessionId) -> Option<AggregateHead> {
        self.published.sessions.get(&id.0).map(|state| {
            let (last_sequence, last_checksum) = state.head();
            AggregateHead {
                last_sequence,
                last_checksum: last_checksum.to_owned(),
            }
        })
    }

    pub(crate) fn owner_of(&self, object: SessionObjectRef<'_>) -> Option<&SessionId> {
        let owner = match object {
            SessionObjectRef::Turn(id) => self.published.owners.turns.get(&id.0),
            SessionObjectRef::Question(id) => self.published.owners.questions.get(&id.0),
            SessionObjectRef::Approval(id) => self.published.owners.approvals.get(&id.0),
        }?;
        self.published
            .sessions
            .get(owner)
            .map(|state| &state.snapshot().session_id)
    }

    pub(crate) fn canonical_prompt(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> Result<&str, SessionReadError> {
        let requested = self
            .published
            .sessions
            .get(&session_id.0)
            .ok_or(SessionReadError::SessionNotFound)?;
        let Some(owner) = self.published.owners.turns.get(&turn_id.0) else {
            if requested.canonical_prompt(turn_id).is_some()
                || self
                    .published
                    .sessions
                    .values()
                    .any(|state| state.canonical_prompt(turn_id).is_some())
            {
                return Err(SessionReadError::CorruptPublishedView);
            }
            return Err(SessionReadError::TurnNotFound);
        };
        let owner_state = self
            .published
            .sessions
            .get(owner)
            .ok_or(SessionReadError::CorruptPublishedView)?;
        let prompt = owner_state
            .canonical_prompt(turn_id)
            .ok_or(SessionReadError::CorruptPublishedView)?;
        if owner != &session_id.0 {
            return Err(SessionReadError::TurnOwnershipMismatch);
        }
        Ok(prompt)
    }

    pub(crate) fn resume_events(
        &self,
        cursor: &StreamCursor,
    ) -> Result<EventPage, DurableCursorError> {
        if cursor.stream_kind != crate::protocol::StreamKind::SessionRollout {
            return Err(DurableCursorError::UnsupportedStreamKind);
        }
        let state = self
            .published
            .sessions
            .get(&cursor.stream_id)
            .ok_or(DurableCursorError::SessionNotFound)?;
        state.resume(cursor).map_err(|error| match error {
            SessionCursorError::UnsupportedStreamKind => DurableCursorError::UnsupportedStreamKind,
            SessionCursorError::SessionMismatch => DurableCursorError::CorruptPublishedView,
            SessionCursorError::EpochMismatch {
                expected_epoch,
                actual_epoch,
                head_sequence,
            } => DurableCursorError::EpochMismatch {
                expected_epoch,
                actual_epoch,
                head_sequence,
            },
            SessionCursorError::Future { head_sequence } => {
                DurableCursorError::Future { head_sequence }
            }
        })
    }

    pub(crate) fn occurrence(
        &self,
        project_id: &ProtocolProjectId,
        occurrence_id: &ArtifactOccurrenceId,
    ) -> Option<&ArtifactManifest> {
        self.published
            .occurrence_metadata
            .get(&occurrence_id.0)
            .filter(|manifest| manifest.project_id == *project_id)
    }

    pub(crate) fn read_artifact(
        &self,
        hash: &ArtifactHash,
    ) -> Result<VerifiedBlobFile, StoreError> {
        self.core
            .components()
            .expect("Ready runtime 必须持有 Artifact Store")
            .artifact_store
            .get(hash)
    }

    /// 先验证请求 Project 对 hash 的 committed reachability，再由同一 live handle 复验内容。
    pub(crate) fn read_project_artifact(
        &self,
        project_id: &ProtocolProjectId,
        hash: &crate::protocol::ArtifactHash,
    ) -> Result<Option<VerifiedBlobFile>, StoreError> {
        let Some(project) = self.published.projects.get(&project_id.0) else {
            return Ok(None);
        };
        let domain_hash =
            ArtifactHash::parse(hash.as_str().to_owned()).map_err(|_| StoreError::InvalidHash)?;
        let reachable = project
            .snapshot
            .artifacts
            .get(&domain_hash)
            .is_some_and(|record| {
                record.availability() == ArtifactAvailability::VerifiedDurable
                    && self
                        .published
                        .reachable_artifact_hashes
                        .contains(&domain_hash)
            });
        if !reachable {
            return Ok(None);
        }
        self.core
            .components()
            .expect("Ready runtime 必须持有 Artifact Store")
            .artifact_store
            .get(&domain_hash)
            .map(Some)
    }

    pub(crate) fn put_fixed_alda_fixture(
        &self,
        global_tx_id: &GlobalTransactionId,
    ) -> Result<DurableFixturePrepared, StoreError> {
        let expected_hash =
            ArtifactHash::parse(DURABLE_FIXTURE_HASH).map_err(|_| StoreError::InvalidHash)?;
        let receipt = self
            .core
            .components()
            .expect("Ready runtime 必须持有 Artifact Store")
            .artifact_store
            .put(
                Cursor::new(DURABLE_FIXTURE_BYTES),
                Some(&ExpectedArtifact {
                    hash: expected_hash,
                    size: DURABLE_FIXTURE_SIZE_BYTES,
                }),
            )?;
        let pending_reference = PendingArtifactReference {
            hash: receipt.hash().clone(),
            published_generation: self.published.generation,
        };
        let audit_plan = receipt.recovery_audit_plan(global_tx_id.as_str())?;
        let record = receipt
            .into_record()
            .map_err(|_| StoreError::RecoveryAuditMismatch)?;
        Ok(DurableFixturePrepared {
            record,
            audit_plan,
            pending_reference,
        })
    }

    /// 仅供权威证明 Prepared 前拒绝的分支消费；generation 改变时拒绝分类。
    pub(crate) fn classify_artifact_reference_after_prepared_rejection(
        &self,
        prepared: DurableFixturePrepared,
    ) -> Option<ArtifactReferenceDisposition> {
        let (_, _, pending_reference) = prepared.into_prepared_facts_and_reference();
        self.classify_pending_artifact_reference_after_prepared_rejection(pending_reference)
    }

    /// 只消费已从 put capability 分离的 Prepared 前引用分类令牌。
    pub(crate) fn classify_pending_artifact_reference_after_prepared_rejection(
        &self,
        pending_reference: PendingArtifactReference,
    ) -> Option<ArtifactReferenceDisposition> {
        if pending_reference.published_generation != self.published.generation {
            return None;
        }
        if self
            .published
            .reachable_artifact_hashes
            .contains(&pending_reference.hash)
        {
            Some(ArtifactReferenceDisposition::AlreadyReachable)
        } else {
            Some(ArtifactReferenceDisposition::OrphanCandidate(
                OrphanArtifact {
                    hash: pending_reference.hash,
                },
            ))
        }
    }

    pub(crate) fn require_external_capacity(
        &self,
        additional: usize,
    ) -> Result<(), ExternalCapacityError> {
        let control = self
            .core
            .control()
            .expect("Ready runtime 必须持有 control writer")
            .projection();
        let capacity = control.capacity();
        if capacity
            .external
            .checked_add(additional)
            .is_none_or(|count| count > MAX_EXTERNAL_PREPARED)
            || capacity
                .total
                .checked_add(additional)
                .is_none_or(|count| count > MAX_TOTAL_PREPARED)
        {
            return Err(ExternalCapacityError);
        }
        Ok(())
    }

    /// 在任何权威写入前构造完整的 `ProjectCreate` redo plan。
    pub(crate) fn plan_project_create(
        &self,
        client_id: &ClientId,
        client_command_id: &ClientCommandId,
        payload_digest: &str,
        name: &str,
    ) -> Result<PrepareControlRequest, DurableRuntimeError> {
        self.require_external_capacity(1).map_err(|_| {
            component_error(
                "ProjectCreate planning",
                "external durable command capacity is exhausted",
            )
        })?;

        let mut reserved_ids = BTreeSet::new();
        let AllocatedDomainId::Project(project_id) = self
            .allocate_id(DomainIdKind::Project, &reserved_ids)
            .map_err(|error| component_error("ProjectCreate project ID allocation", error))?
        else {
            return Err(DurableRuntimeError::CatalogMismatch);
        };
        reserved_ids.insert(project_id.as_str().to_owned());
        let AllocatedDomainId::Score(score_id) = self
            .allocate_id(DomainIdKind::Score, &reserved_ids)
            .map_err(|error| component_error("ProjectCreate score ID allocation", error))?
        else {
            return Err(DurableRuntimeError::CatalogMismatch);
        };
        reserved_ids.insert(score_id.as_str().to_owned());
        let AllocatedDomainId::Take(default_take_id) = self
            .allocate_id(DomainIdKind::Take, &reserved_ids)
            .map_err(|error| component_error("ProjectCreate take ID allocation", error))?
        else {
            return Err(DurableRuntimeError::CatalogMismatch);
        };
        reserved_ids.insert(default_take_id.as_str().to_owned());
        let AllocatedDomainId::Branch(default_branch_id) = self
            .allocate_id(DomainIdKind::Branch, &reserved_ids)
            .map_err(|error| component_error("ProjectCreate branch ID allocation", error))?
        else {
            return Err(DurableRuntimeError::CatalogMismatch);
        };
        let global_tx_id = self
            .allocate_external_global_tx_id(&BTreeSet::new())
            .map_err(|error| component_error("ProjectCreate transaction ID allocation", error))?;

        let protocol_project_id = ProtocolProjectId(project_id.as_str().to_owned());
        let reply = crate::protocol::CommandReply::success(
            client_command_id.clone(),
            CommandResult::ProjectCreated(ProtocolProjectSnapshot {
                project_id: protocol_project_id,
                name: name.to_owned(),
                version: 1,
            }),
        );
        let raw_reply = serde_json::to_vec(&reply)
            .map_err(|error| component_error("ProjectCreate stable reply", error))?;
        let command_record = StoredCommandRecordV1::new(
            client_id.0.clone(),
            client_command_id.0.clone(),
            payload_digest,
            &raw_reply,
        )
        .map_err(|error| component_error("ProjectCreate command record", error))?;
        let append_request = AppendRequest {
            transaction_id: global_tx_id.project_transaction_id(),
            command_record: Some(command_record.clone()),
            events: vec![ProjectEvent::ProjectInitialized {
                project_id: project_id.clone(),
                score_id,
                default_take_id,
                default_branch_id,
            }],
        };
        let project_plan =
            StoredProjectPlanV1::from_append_request(&project_id, 0, None, &append_request)
                .map_err(|error| component_error("ProjectCreate Project plan", error))?;
        let prepared = PreparedTransactionV1::new(
            global_tx_id.into_inner(),
            command_record,
            Some(project_plan),
            None,
            Vec::new(),
        )
        .map_err(|error| component_error("ProjectCreate control plan", error))?;
        Ok(PrepareControlRequest {
            project_allocation: Some(project_id),
            session_allocation: None,
            prepared,
        })
    }

    /// 在任何权威写入前构造完整的 `SessionStart` redo plan。
    pub(crate) fn plan_session_start(
        &self,
        client_id: &ClientId,
        client_command_id: &ClientCommandId,
        payload_digest: &str,
        project_id: &ProtocolProjectId,
    ) -> Result<PrepareControlRequest, DurableRuntimeError> {
        self.require_external_capacity(1).map_err(|_| {
            component_error(
                "SessionStart planning",
                "external durable command capacity is exhausted",
            )
        })?;
        if self.project_projection(project_id).is_none()
            || self.project_metadata(project_id).is_none()
        {
            return Err(component_error(
                "SessionStart planning",
                "requested Project is not in the committed catalog",
            ));
        }

        let AllocatedDomainId::Session(session_id) = self
            .allocate_id(DomainIdKind::Session, &BTreeSet::new())
            .map_err(|error| component_error("SessionStart Session ID allocation", error))?
        else {
            return Err(DurableRuntimeError::CatalogMismatch);
        };
        let global_tx_id = self
            .allocate_external_global_tx_id(&BTreeSet::new())
            .map_err(|error| component_error("SessionStart transaction ID allocation", error))?;
        let snapshot = SessionSnapshot {
            session_id: session_id.clone(),
            project_id: project_id.clone(),
            stream_epoch: crate::protocol::SESSION_STREAM_EPOCH,
            covered_through_sequence: 1,
            turns: Vec::new(),
            questions: Vec::new(),
            approvals: Vec::new(),
        };
        let reply = crate::protocol::CommandReply::success(
            client_command_id.clone(),
            CommandResult::SessionStarted(snapshot),
        );
        let raw_reply = serde_json::to_vec(&reply)
            .map_err(|error| component_error("SessionStart stable reply", error))?;
        let command_record = StoredCommandRecordV1::new(
            client_id.0.clone(),
            client_command_id.0.clone(),
            payload_digest,
            &raw_reply,
        )
        .map_err(|error| component_error("SessionStart command record", error))?;
        let append_request = SessionAppendRequest::new(
            global_tx_id.session_transaction_id(),
            Some(command_record.clone()),
            vec![SessionRolloutEvent::SessionStarted {
                session_id: session_id.clone(),
                project_id: project_id.clone(),
            }],
        );
        let session_plan = StoredSessionPlanV1::from_append_request(
            &session_id,
            project_id,
            0,
            None,
            &append_request,
        )
        .map_err(|error| component_error("SessionStart Session plan", error))?;
        let prepared = PreparedTransactionV1::new(
            global_tx_id.into_inner(),
            command_record,
            None,
            Some(session_plan),
            Vec::new(),
        )
        .map_err(|error| component_error("SessionStart control plan", error))?;
        Ok(PrepareControlRequest {
            project_allocation: None,
            session_allocation: Some(SessionAllocation {
                session_id,
                project_id: project_id.clone(),
            }),
            prepared,
        })
    }

    /// 在任何权威写入前构造包含首个 Question 的完整 `TurnStart` redo plan。
    pub(crate) fn plan_turn_start(
        &self,
        client_id: &ClientId,
        client_command_id: &ClientCommandId,
        payload_digest: &str,
        session_id: &SessionId,
        canonical_prompt: &str,
    ) -> Result<PrepareControlRequest, DurableRuntimeError> {
        self.require_external_capacity(1).map_err(|_| {
            component_error(
                "TurnStart planning",
                "external durable command capacity is exhausted",
            )
        })?;
        let session = self.session_snapshot(session_id).ok_or_else(|| {
            component_error(
                "TurnStart planning",
                "requested Session is not in the committed catalog",
            )
        })?;
        let head = self
            .session_head(session_id)
            .ok_or(DurableRuntimeError::CatalogMismatch)?;

        let mut reserved_ids = BTreeSet::new();
        let AllocatedDomainId::Turn(turn_id) = self
            .allocate_id(DomainIdKind::Turn, &reserved_ids)
            .map_err(|error| component_error("TurnStart Turn ID allocation", error))?
        else {
            return Err(DurableRuntimeError::CatalogMismatch);
        };
        reserved_ids.insert(turn_id.0.clone());
        let AllocatedDomainId::Question(question_id) = self
            .allocate_id(DomainIdKind::Question, &reserved_ids)
            .map_err(|error| component_error("TurnStart Question ID allocation", error))?
        else {
            return Err(DurableRuntimeError::CatalogMismatch);
        };
        let global_tx_id = self
            .allocate_external_global_tx_id(&BTreeSet::new())
            .map_err(|error| component_error("TurnStart transaction ID allocation", error))?;
        let choices = vec![
            QuestionChoice {
                choice_id: ChoiceId("bars_8".to_owned()),
                label: "8 bars".to_owned(),
            },
            QuestionChoice {
                choice_id: ChoiceId("bars_16".to_owned()),
                label: "16 bars".to_owned(),
            },
        ];
        let reply = crate::protocol::CommandReply::success(
            client_command_id.clone(),
            CommandResult::TurnStarted(TurnSnapshot {
                turn_id: turn_id.clone(),
                status: TurnStatus::WaitingForInput,
                terminal_sequence: None,
            }),
        );
        let raw_reply = serde_json::to_vec(&reply)
            .map_err(|error| component_error("TurnStart stable reply", error))?;
        let command_record = StoredCommandRecordV1::new(
            client_id.0.clone(),
            client_command_id.0.clone(),
            payload_digest,
            &raw_reply,
        )
        .map_err(|error| component_error("TurnStart command record", error))?;
        let append_request = SessionAppendRequest::new(
            global_tx_id.session_transaction_id(),
            Some(command_record.clone()),
            vec![
                SessionRolloutEvent::TurnStarted {
                    turn_id: turn_id.clone(),
                    canonical_prompt: canonical_prompt.to_owned(),
                },
                SessionRolloutEvent::QuestionRequested {
                    question_id,
                    session_id: session_id.clone(),
                    owner_turn_id: turn_id,
                    prompt: TURN_START_QUESTION_PROMPT.to_owned(),
                    choices,
                },
            ],
        );
        let session_plan = StoredSessionPlanV1::from_append_request(
            session_id,
            &session.project_id,
            head.last_sequence,
            Some(head.last_checksum),
            &append_request,
        )
        .map_err(|error| component_error("TurnStart Session plan", error))?;
        let prepared = PreparedTransactionV1::new(
            global_tx_id.into_inner(),
            command_record,
            None,
            Some(session_plan),
            Vec::new(),
        )
        .map_err(|error| component_error("TurnStart control plan", error))?;
        Ok(PrepareControlRequest {
            project_allocation: None,
            session_allocation: None,
            prepared,
        })
    }

    /// 构造 `TurnCancel` 的事件 batch，或带已验证理由的零事件 Session plan。
    #[allow(
        clippy::too_many_lines,
        reason = "取消规划需连续审计 owner、pending 顺序、terminal sequence 与 command-only 授权"
    )]
    pub(crate) fn plan_turn_cancel(
        &self,
        client_id: &ClientId,
        client_command_id: &ClientCommandId,
        payload_digest: &str,
        requested_session_id: &SessionId,
        turn_id: &TurnId,
    ) -> Result<PrepareControlRequest, DurableRuntimeError> {
        self.require_external_capacity(1).map_err(|_| {
            component_error(
                "TurnCancel planning",
                "external durable command capacity is exhausted",
            )
        })?;
        if self.session_snapshot(requested_session_id).is_none() {
            return Err(component_error(
                "TurnCancel planning",
                "requested Session is not in the committed catalog",
            ));
        }
        let owner_session_id = self
            .owner_of(SessionObjectRef::Turn(turn_id))
            .cloned()
            .ok_or_else(|| {
                component_error("TurnCancel planning", "requested Turn was not found")
            })?;
        let owner_snapshot = self
            .session_snapshot(&owner_session_id)
            .ok_or(DurableRuntimeError::CatalogMismatch)?;
        let turn = owner_snapshot
            .turns
            .iter()
            .find(|turn| turn.turn_id == *turn_id)
            .ok_or(DurableRuntimeError::CatalogMismatch)?;
        let head = self
            .session_head(&owner_session_id)
            .ok_or(DurableRuntimeError::CatalogMismatch)?;
        let command = ClientCommand::TurnCancel {
            session_id: requested_session_id.clone(),
            turn_id: turn_id.clone(),
        };

        let (reply, events, command_only_reason) = if owner_session_id != *requested_session_id {
            (
                crate::protocol::CommandReply::error(
                    client_command_id.clone(),
                    ProtocolErrorCode::TurnOwnershipMismatch,
                    format!(
                        "turn `{}` does not belong to session `{}`",
                        turn_id.0, requested_session_id.0
                    ),
                ),
                Vec::new(),
                Some(CommandOnlyReasonV1::TurnOwnershipMismatch),
            )
        } else if turn.status.is_terminal() {
            let terminal_sequence = turn
                .terminal_sequence
                .ok_or(DurableRuntimeError::CatalogMismatch)?;
            (
                crate::protocol::CommandReply::success(
                    client_command_id.clone(),
                    CommandResult::TurnAlreadyTerminal {
                        turn_id: turn_id.clone(),
                        terminal_status: turn.status,
                        terminal_sequence,
                    },
                ),
                Vec::new(),
                Some(CommandOnlyReasonV1::TurnAlreadyTerminal),
            )
        } else {
            if !matches!(
                turn.status,
                TurnStatus::Running | TurnStatus::WaitingForInput
            ) {
                return Err(component_error(
                    "TurnCancel planning",
                    "published Turn is not cancellable",
                ));
            }
            let mut pending = owner_snapshot
                .questions
                .iter()
                .filter(|question| {
                    question.owner_turn_id == *turn_id && question.status == QuestionStatus::Pending
                })
                .map(|question| {
                    (
                        question.created_sequence,
                        SessionRolloutEvent::QuestionOwnerTurnAborted {
                            question_id: question.question_id.clone(),
                            owner_turn_id: turn_id.clone(),
                            owner_terminal_status: TurnStatus::Cancelled,
                        },
                    )
                })
                .chain(
                    owner_snapshot
                        .approvals
                        .iter()
                        .filter(|approval| {
                            approval.owner_turn_id == *turn_id
                                && approval.status == ApprovalStatus::Pending
                        })
                        .map(|approval| {
                            (
                                approval.created_sequence,
                                SessionRolloutEvent::ApprovalOwnerTurnAborted {
                                    approval_id: approval.approval_id.clone(),
                                    owner_turn_id: turn_id.clone(),
                                    owner_terminal_status: TurnStatus::Cancelled,
                                },
                            )
                        }),
                )
                .collect::<Vec<_>>();
            pending.sort_by_key(|(sequence, _)| *sequence);
            let event_count = pending
                .len()
                .checked_add(2)
                .and_then(|count| u64::try_from(count).ok())
                .ok_or(DurableRuntimeError::CatalogMismatch)?;
            let terminal_sequence = head
                .last_sequence
                .checked_add(event_count)
                .ok_or(DurableRuntimeError::CatalogMismatch)?;
            let mut events = Vec::with_capacity(pending.len() + 2);
            events.push(SessionRolloutEvent::TurnCancelRequested {
                turn_id: turn_id.clone(),
            });
            events.extend(pending.into_iter().map(|(_, event)| event));
            events.push(SessionRolloutEvent::TurnCompleted {
                turn_id: turn_id.clone(),
                status: TurnStatus::Cancelled,
            });
            (
                crate::protocol::CommandReply::success(
                    client_command_id.clone(),
                    CommandResult::TurnCancelled(TurnSnapshot {
                        turn_id: turn_id.clone(),
                        status: TurnStatus::Cancelled,
                        terminal_sequence: Some(terminal_sequence),
                    }),
                ),
                events,
                None,
            )
        };

        let raw_reply = serde_json::to_vec(&reply)
            .map_err(|error| component_error("TurnCancel stable reply", error))?;
        let command_record = StoredCommandRecordV1::new(
            client_id.0.clone(),
            client_command_id.0.clone(),
            payload_digest,
            &raw_reply,
        )
        .map_err(|error| component_error("TurnCancel command record", error))?;
        let global_tx_id = self
            .allocate_external_global_tx_id(&BTreeSet::new())
            .map_err(|error| component_error("TurnCancel transaction ID allocation", error))?;
        let append_request = command_only_reason.map_or_else(
            || {
                SessionAppendRequest::new(
                    global_tx_id.session_transaction_id(),
                    Some(command_record.clone()),
                    events,
                )
            },
            |reason| {
                SessionAppendRequest::new_command_only(
                    global_tx_id.session_transaction_id(),
                    command_record.clone(),
                    StoredCommandOnlyAuthorizationV1::new(command, reason),
                )
            },
        );
        let session_plan = StoredSessionPlanV1::from_append_request(
            &owner_session_id,
            &owner_snapshot.project_id,
            head.last_sequence,
            Some(head.last_checksum),
            &append_request,
        )
        .map_err(|error| component_error("TurnCancel Session plan", error))?;
        let prepared = PreparedTransactionV1::new(
            global_tx_id.into_inner(),
            command_record,
            None,
            Some(session_plan),
            Vec::new(),
        )
        .map_err(|error| component_error("TurnCancel control plan", error))?;
        Ok(PrepareControlRequest {
            project_allocation: None,
            session_allocation: None,
            prepared,
        })
    }

    /// 构造 `QuestionRespond` 的两条权威事实，或带已验证理由的零事件 Session plan。
    #[allow(
        clippy::too_many_lines,
        reason = "Question 规划需连续审计权威 prompt、subject inputs、稳定回复与 Session batch"
    )]
    pub(crate) fn plan_question_respond(
        &self,
        client_id: &ClientId,
        client_command_id: &ClientCommandId,
        payload_digest: &str,
        requested_session_id: &SessionId,
        question_id: &QuestionId,
        choice_id: &ChoiceId,
    ) -> Result<PrepareControlRequest, DurableRuntimeError> {
        self.require_external_capacity(1).map_err(|_| {
            component_error(
                "QuestionRespond planning",
                "external durable command capacity is exhausted",
            )
        })?;
        if self.session_snapshot(requested_session_id).is_none() {
            return Err(component_error(
                "QuestionRespond planning",
                "requested Session is not in the committed catalog",
            ));
        }
        let owner_session_id = self
            .owner_of(SessionObjectRef::Question(question_id))
            .cloned()
            .ok_or_else(|| {
                component_error(
                    "QuestionRespond planning",
                    "requested Question was not found",
                )
            })?;
        let owner_snapshot = self
            .session_snapshot(&owner_session_id)
            .ok_or(DurableRuntimeError::CatalogMismatch)?;
        let question = owner_snapshot
            .questions
            .iter()
            .find(|question| question.question_id == *question_id)
            .ok_or(DurableRuntimeError::CatalogMismatch)?;
        if !question
            .choices
            .iter()
            .any(|choice| choice.choice_id == *choice_id)
        {
            return Err(component_error(
                "QuestionRespond planning",
                "choice is not in the authoritative Question",
            ));
        }
        if question.status == QuestionStatus::OwnerTurnAborted {
            return Err(component_error(
                "QuestionRespond planning",
                "Question owner Turn is terminal",
            ));
        }
        let head = self
            .session_head(&owner_session_id)
            .ok_or(DurableRuntimeError::CatalogMismatch)?;
        let command = ClientCommand::QuestionRespond {
            session_id: requested_session_id.clone(),
            question_id: question_id.clone(),
            choice_id: choice_id.clone(),
        };

        let (reply, events, command_only_reason) = if owner_session_id != *requested_session_id {
            (
                crate::protocol::CommandReply::error(
                    client_command_id.clone(),
                    ProtocolErrorCode::QuestionOwnershipMismatch,
                    format!(
                        "question does not belong to session `{}`",
                        requested_session_id.0
                    ),
                ),
                Vec::new(),
                Some(CommandOnlyReasonV1::QuestionOwnershipMismatch),
            )
        } else if question.status == QuestionStatus::Answered {
            (
                crate::protocol::CommandReply::success(
                    client_command_id.clone(),
                    CommandResult::QuestionAlreadyResolved(question.clone()),
                ),
                Vec::new(),
                Some(CommandOnlyReasonV1::QuestionAlreadyResolved),
            )
        } else {
            if question.status != QuestionStatus::Pending {
                return Err(DurableRuntimeError::CatalogMismatch);
            }
            let AllocatedDomainId::Approval(approval_id) = self
                .allocate_id(DomainIdKind::Approval, &BTreeSet::new())
                .map_err(|error| {
                    component_error("QuestionRespond Approval ID allocation", error)
                })?
            else {
                return Err(DurableRuntimeError::CatalogMismatch);
            };
            let canonical_prompt = self
                .canonical_prompt(&owner_session_id, &question.owner_turn_id)
                .map_err(|error| component_error("QuestionRespond canonical prompt", error))?;
            let subject_inputs = ApprovalSubjectInputsV1::canonical(
                APPROVAL_PROVIDER_ORIGIN,
                ["constraints", "prompt", "constraints"],
            )
            .map_err(|error| component_error("QuestionRespond subject inputs", error))?;
            let approval_subject_digest = crate::protocol::approval_subject_digest_v1(
                APPROVAL_PROVIDER_ORIGIN,
                &["constraints", "prompt"],
                &question.owner_turn_id,
                canonical_prompt,
            );
            let resolved_sequence = head
                .last_sequence
                .checked_add(1)
                .ok_or(DurableRuntimeError::CatalogMismatch)?;
            let mut answered = question.clone();
            answered.status = QuestionStatus::Answered;
            answered.terminal_sequence = Some(resolved_sequence);
            answered.answer = Some(QuestionAnswer {
                choice_id: choice_id.clone(),
            });
            answered.responder_client_id = Some(client_id.clone());
            (
                crate::protocol::CommandReply::success(
                    client_command_id.clone(),
                    CommandResult::QuestionAnswered(answered),
                ),
                vec![
                    SessionRolloutEvent::QuestionResolved {
                        question_id: question_id.clone(),
                        choice_id: choice_id.clone(),
                        responder_client_id: client_id.clone(),
                    },
                    SessionRolloutEvent::ApprovalRequested {
                        approval_id,
                        session_id: owner_session_id.clone(),
                        owner_turn_id: question.owner_turn_id.clone(),
                        payload: ApprovalPayload {
                            action: APPROVAL_ACTION.to_owned(),
                            effect: EffectClass::ModelEgress,
                            target: APPROVAL_PROVIDER_ORIGIN.to_owned(),
                            scope: APPROVAL_SCOPE.to_owned(),
                            estimated_impact: APPROVAL_ESTIMATED_IMPACT.to_owned(),
                        },
                        subject_inputs,
                        approval_subject_digest,
                    },
                ],
                None,
            )
        };

        self.session_only_prepare(
            client_id,
            client_command_id,
            payload_digest,
            command,
            &owner_session_id,
            &owner_snapshot.project_id,
            &head,
            &reply,
            events,
            command_only_reason,
            "QuestionRespond",
        )
    }

    /// 构造只影响 Session aggregate 的 Approval deny 计划。
    #[allow(
        clippy::too_many_lines,
        reason = "deny 规划需连续审计 subject、terminal fact、稳定回复与零 Project/Artifact 边界"
    )]
    pub(crate) fn plan_approval_deny(
        &self,
        client_id: &ClientId,
        client_command_id: &ClientCommandId,
        payload_digest: &str,
        requested_session_id: &SessionId,
        approval_id: &ApprovalId,
        approval_subject_digest: &crate::protocol::ApprovalSubjectDigest,
    ) -> Result<PrepareControlRequest, DurableRuntimeError> {
        self.require_external_capacity(1).map_err(|_| {
            component_error(
                "ApprovalRespond deny planning",
                "external durable command capacity is exhausted",
            )
        })?;
        if self.session_snapshot(requested_session_id).is_none() {
            return Err(component_error(
                "ApprovalRespond deny planning",
                "requested Session is not in the committed catalog",
            ));
        }
        let owner_session_id = self
            .owner_of(SessionObjectRef::Approval(approval_id))
            .cloned()
            .ok_or_else(|| {
                component_error(
                    "ApprovalRespond deny planning",
                    "requested Approval was not found",
                )
            })?;
        let owner_snapshot = self
            .session_snapshot(&owner_session_id)
            .ok_or(DurableRuntimeError::CatalogMismatch)?;
        let approval = owner_snapshot
            .approvals
            .iter()
            .find(|approval| approval.approval_id == *approval_id)
            .ok_or(DurableRuntimeError::CatalogMismatch)?;
        if approval.approval_subject_digest != *approval_subject_digest {
            return Err(component_error(
                "ApprovalRespond deny planning",
                "Approval subject digest does not match",
            ));
        }
        if approval.status == ApprovalStatus::OwnerTurnAborted {
            return Err(component_error(
                "ApprovalRespond deny planning",
                "Approval owner Turn is terminal",
            ));
        }
        let head = self
            .session_head(&owner_session_id)
            .ok_or(DurableRuntimeError::CatalogMismatch)?;
        let command = ClientCommand::ApprovalRespond {
            session_id: requested_session_id.clone(),
            approval_id: approval_id.clone(),
            approval_subject_digest: approval_subject_digest.clone(),
            decision: ApprovalDecision::Deny,
        };

        let (reply, events, command_only_reason) = if owner_session_id != *requested_session_id {
            (
                crate::protocol::CommandReply::error(
                    client_command_id.clone(),
                    ProtocolErrorCode::ApprovalOwnershipMismatch,
                    format!(
                        "approval does not belong to session `{}`",
                        requested_session_id.0
                    ),
                ),
                Vec::new(),
                Some(CommandOnlyReasonV1::ApprovalOwnershipMismatch),
            )
        } else if approval.status != ApprovalStatus::Pending {
            (
                crate::protocol::CommandReply::success(
                    client_command_id.clone(),
                    CommandResult::ApprovalAlreadyResolved(approval.clone()),
                ),
                Vec::new(),
                Some(CommandOnlyReasonV1::ApprovalAlreadyResolved),
            )
        } else {
            let resolved_sequence = head
                .last_sequence
                .checked_add(1)
                .ok_or(DurableRuntimeError::CatalogMismatch)?;
            let mut denied = approval.clone();
            denied.status = ApprovalStatus::Denied;
            denied.terminal_sequence = Some(resolved_sequence);
            denied.decision = Some(ApprovalDecision::Deny);
            denied.responder_client_id = Some(client_id.clone());
            (
                crate::protocol::CommandReply::success(
                    client_command_id.clone(),
                    CommandResult::ApprovalDecided {
                        approval: denied,
                        artifact_manifest: None,
                    },
                ),
                vec![
                    SessionRolloutEvent::ApprovalResolved {
                        approval_id: approval_id.clone(),
                        approval_subject_digest: approval_subject_digest.clone(),
                        decision: ApprovalDecision::Deny,
                        responder_client_id: client_id.clone(),
                    },
                    SessionRolloutEvent::TurnCompleted {
                        turn_id: approval.owner_turn_id.clone(),
                        status: TurnStatus::Failed,
                    },
                ],
                None,
            )
        };

        self.session_only_prepare(
            client_id,
            client_command_id,
            payload_digest,
            command,
            &owner_session_id,
            &owner_snapshot.project_id,
            &head,
            &reply,
            events,
            command_only_reason,
            "ApprovalRespond deny",
        )
    }

    /// 构造 Approval approve 的 Project+Session 双 aggregate 计划并保留 Prepared 前分类能力。
    #[allow(
        clippy::too_many_lines,
        reason = "approve 规划需连续绑定 subject、Artifact audit、双 aggregate head、occurrence 与稳定回复"
    )]
    pub(crate) fn plan_approval_approve(
        &self,
        client_id: &ClientId,
        client_command_id: &ClientCommandId,
        payload_digest: &str,
        requested_session_id: &SessionId,
        approval_id: &ApprovalId,
        approval_subject_digest: &crate::protocol::ApprovalSubjectDigest,
    ) -> Result<DurableApprovalApprovePlan, DurableRuntimeError> {
        self.require_external_capacity(1).map_err(|_| {
            component_error(
                "ApprovalRespond approve planning",
                "external durable command capacity is exhausted",
            )
        })?;
        if self.session_snapshot(requested_session_id).is_none() {
            return Err(component_error(
                "ApprovalRespond approve planning",
                "requested Session is not in the committed catalog",
            ));
        }
        let owner_session_id = self
            .owner_of(SessionObjectRef::Approval(approval_id))
            .cloned()
            .ok_or_else(|| {
                component_error(
                    "ApprovalRespond approve planning",
                    "requested Approval was not found",
                )
            })?;
        let owner_snapshot = self
            .session_snapshot(&owner_session_id)
            .ok_or(DurableRuntimeError::CatalogMismatch)?;
        let approval = owner_snapshot
            .approvals
            .iter()
            .find(|approval| approval.approval_id == *approval_id)
            .ok_or(DurableRuntimeError::CatalogMismatch)?;
        if approval.approval_subject_digest != *approval_subject_digest {
            return Err(component_error(
                "ApprovalRespond approve planning",
                "Approval subject digest does not match",
            ));
        }
        if approval.status == ApprovalStatus::OwnerTurnAborted {
            return Err(component_error(
                "ApprovalRespond approve planning",
                "Approval owner Turn is terminal",
            ));
        }
        let session_head = self
            .session_head(&owner_session_id)
            .ok_or(DurableRuntimeError::CatalogMismatch)?;
        let project_id = owner_snapshot.project_id.clone();
        let project_head = self
            .project_head(&project_id)
            .ok_or(DurableRuntimeError::CatalogMismatch)?;
        let command = ClientCommand::ApprovalRespond {
            session_id: requested_session_id.clone(),
            approval_id: approval_id.clone(),
            approval_subject_digest: approval_subject_digest.clone(),
            decision: ApprovalDecision::Approve,
        };

        if owner_session_id != *requested_session_id || approval.status != ApprovalStatus::Pending {
            let (reply, reason) = if owner_session_id == *requested_session_id {
                (
                    crate::protocol::CommandReply::success(
                        client_command_id.clone(),
                        CommandResult::ApprovalAlreadyResolved(approval.clone()),
                    ),
                    CommandOnlyReasonV1::ApprovalAlreadyResolved,
                )
            } else {
                (
                    crate::protocol::CommandReply::error(
                        client_command_id.clone(),
                        ProtocolErrorCode::ApprovalOwnershipMismatch,
                        format!(
                            "approval does not belong to session `{}`",
                            requested_session_id.0
                        ),
                    ),
                    CommandOnlyReasonV1::ApprovalOwnershipMismatch,
                )
            };
            let request = self.session_only_prepare(
                client_id,
                client_command_id,
                payload_digest,
                command,
                &owner_session_id,
                &project_id,
                &session_head,
                &reply,
                Vec::new(),
                Some(reason),
                "ApprovalRespond approve",
            )?;
            return Ok(DurableApprovalApprovePlan {
                request,
                pending_reference: None,
            });
        }

        let AllocatedDomainId::Occurrence(occurrence_id) = self
            .allocate_id(DomainIdKind::Occurrence, &BTreeSet::new())
            .map_err(|error| component_error("ApprovalRespond occurrence ID allocation", error))?
        else {
            return Err(DurableRuntimeError::CatalogMismatch);
        };
        let global_tx_id = self
            .allocate_external_global_tx_id(&BTreeSet::new())
            .map_err(|error| component_error("ApprovalRespond transaction ID allocation", error))?;
        let resolved_sequence = session_head
            .last_sequence
            .checked_add(1)
            .ok_or(DurableRuntimeError::CatalogMismatch)?;
        let mut approved = approval.clone();
        approved.status = ApprovalStatus::Approved;
        approved.terminal_sequence = Some(resolved_sequence);
        approved.decision = Some(ApprovalDecision::Approve);
        approved.responder_client_id = Some(client_id.clone());
        let manifest = ArtifactManifest {
            artifact_occurrence_id: occurrence_id,
            artifact_hash: crate::protocol::ArtifactHash::parse(DURABLE_FIXTURE_HASH)
                .map_err(|_| DurableRuntimeError::CatalogMismatch)?,
            kind: DURABLE_FIXTURE_KIND,
            mime_type: DURABLE_FIXTURE_MIME_TYPE.to_owned(),
            size_bytes: DURABLE_FIXTURE_SIZE_BYTES,
            producer: DURABLE_FIXTURE_PRODUCER,
            project_id: project_id.clone(),
            source_session_id: owner_session_id.clone(),
            source_turn_id: approval.owner_turn_id.clone(),
            fixture_version: DURABLE_FIXTURE_VERSION,
            created_sequence: resolved_sequence,
            provenance_label: DURABLE_FIXTURE_PROVENANCE_LABEL.to_owned(),
            durability: DURABLE_FIXTURE_DURABILITY,
        };
        let reply = crate::protocol::CommandReply::success(
            client_command_id.clone(),
            CommandResult::ApprovalDecided {
                approval: approved,
                artifact_manifest: Some(manifest),
            },
        );
        let raw_reply = serde_json::to_vec(&reply)
            .map_err(|error| component_error("ApprovalRespond approve stable reply", error))?;
        let command_record = StoredCommandRecordV1::new(
            client_id.0.clone(),
            client_command_id.0.clone(),
            payload_digest,
            &raw_reply,
        )
        .map_err(|error| component_error("ApprovalRespond approve command record", error))?;
        let session_request = SessionAppendRequest::new(
            global_tx_id.session_transaction_id(),
            Some(command_record.clone()),
            vec![
                SessionRolloutEvent::ApprovalResolved {
                    approval_id: approval_id.clone(),
                    approval_subject_digest: approval_subject_digest.clone(),
                    decision: ApprovalDecision::Approve,
                    responder_client_id: client_id.clone(),
                },
                SessionRolloutEvent::TurnCompleted {
                    turn_id: approval.owner_turn_id.clone(),
                    status: TurnStatus::Succeeded,
                },
            ],
        );
        let session_plan = StoredSessionPlanV1::from_append_request(
            &owner_session_id,
            &project_id,
            session_head.last_sequence,
            Some(session_head.last_checksum),
            &session_request,
        )
        .map_err(|error| component_error("ApprovalRespond approve Session plan", error))?;

        let artifact = self
            .put_fixed_alda_fixture(&global_tx_id)
            .map_err(|error| component_error("ApprovalRespond approve Artifact put", error))?;
        let (artifact_record, audit_plan, pending_reference) =
            artifact.into_prepared_facts_and_reference();
        let request: Result<PrepareControlRequest, DurableRuntimeError> = (|| {
            let project_id = DomainProjectId::parse(project_id.0.clone())
                .map_err(|error| component_error("ApprovalRespond approve Project ID", error))?;
            let project_request = AppendRequest {
                transaction_id: global_tx_id.project_transaction_id(),
                command_record: Some(command_record.clone()),
                events: vec![ProjectEvent::ArtifactRegistered(artifact_record)],
            };
            let project_plan = StoredProjectPlanV1::from_append_request(
                &project_id,
                project_head.last_sequence,
                Some(project_head.last_checksum),
                &project_request,
            )
            .map_err(|error| component_error("ApprovalRespond approve Project plan", error))?;
            let prepared = PreparedTransactionV1::new(
                global_tx_id.as_str().to_owned(),
                command_record,
                Some(project_plan),
                Some(session_plan),
                vec![audit_plan],
            )
            .map_err(|error| component_error("ApprovalRespond approve control plan", error))?;
            Ok(PrepareControlRequest {
                project_allocation: None,
                session_allocation: None,
                prepared,
            })
        })();
        match request {
            Ok(request) => Ok(DurableApprovalApprovePlan {
                request,
                pending_reference: Some(pending_reference),
            }),
            Err(error) => {
                let disposition = self
                    .classify_pending_artifact_reference_after_prepared_rejection(pending_reference)
                    .map_or("unclassified", |disposition| match disposition {
                        ArtifactReferenceDisposition::AlreadyReachable => "already reachable",
                        ArtifactReferenceDisposition::OrphanCandidate(_) => "orphan candidate",
                    });
                Err(component_error(
                    "ApprovalRespond approve planning",
                    format!("{error}; Artifact disposition: {disposition}"),
                ))
            }
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "共享 Session-only WAL 封装显式保留命令、aggregate head 与稳定回复边界"
    )]
    pub(crate) fn session_only_prepare(
        &self,
        client_id: &ClientId,
        client_command_id: &ClientCommandId,
        payload_digest: &str,
        command: ClientCommand,
        session_id: &SessionId,
        project_id: &ProtocolProjectId,
        head: &AggregateHead,
        reply: &crate::protocol::CommandReply,
        events: Vec<SessionRolloutEvent>,
        command_only_reason: Option<CommandOnlyReasonV1>,
        component: &'static str,
    ) -> Result<PrepareControlRequest, DurableRuntimeError> {
        let raw_reply =
            serde_json::to_vec(&reply).map_err(|error| component_error(component, error))?;
        let command_record = StoredCommandRecordV1::new(
            client_id.0.clone(),
            client_command_id.0.clone(),
            payload_digest,
            &raw_reply,
        )
        .map_err(|error| component_error(component, error))?;
        let global_tx_id = self
            .allocate_external_global_tx_id(&BTreeSet::new())
            .map_err(|error| component_error(component, error))?;
        let append_request = command_only_reason.map_or_else(
            || {
                SessionAppendRequest::new(
                    global_tx_id.session_transaction_id(),
                    Some(command_record.clone()),
                    events,
                )
            },
            |reason| {
                SessionAppendRequest::new_command_only(
                    global_tx_id.session_transaction_id(),
                    command_record.clone(),
                    StoredCommandOnlyAuthorizationV1::new(command, reason),
                )
            },
        );
        let session_plan = StoredSessionPlanV1::from_append_request(
            session_id,
            project_id,
            head.last_sequence,
            Some(head.last_checksum.clone()),
            &append_request,
        )
        .map_err(|error| component_error(component, error))?;
        let prepared = PreparedTransactionV1::new(
            global_tx_id.into_inner(),
            command_record,
            None,
            Some(session_plan),
            Vec::new(),
        )
        .map_err(|error| component_error(component, error))?;
        Ok(PrepareControlRequest {
            project_allocation: None,
            session_allocation: None,
            prepared,
        })
    }

    pub(crate) fn allocate_id(
        &self,
        kind: DomainIdKind,
        additionally_reserved: &BTreeSet<String>,
    ) -> Result<AllocatedDomainId, DomainIdAllocationError> {
        let control = self
            .core
            .control()
            .expect("Ready runtime 必须持有 control writer")
            .projection();
        allocate_domain_id_with(
            control,
            &self.published,
            kind,
            additionally_reserved,
            os_random_128,
        )
    }

    pub(crate) fn allocate_external_global_tx_id(
        &self,
        additionally_reserved: &BTreeSet<String>,
    ) -> Result<GlobalTransactionId, GlobalTransactionIdError> {
        let control = self
            .core
            .control()
            .expect("Ready runtime 必须持有 control writer")
            .projection();
        allocate_global_transaction_id_with(
            |candidate| {
                control.prepared.contains_key(candidate)
                    || control
                        .prepared
                        .values()
                        .any(|prepared| prepared.global_tx_id == candidate)
            },
            additionally_reserved,
            os_random_128,
        )
    }

    pub(crate) fn lookup_command(
        &self,
        client_id: &ClientId,
        client_command_id: &ClientCommandId,
        payload_digest: &str,
    ) -> Result<CommandLookup, CommandLookupError> {
        let projection = self
            .core
            .control()
            .map_err(CommandLookupError::CorruptCommittedIndex)?
            .projection();
        lookup_command_in_projection(projection, client_id, client_command_id, payload_digest)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "提交必须在一个所有权流程中完整表达 Prepared 后的状态转换"
    )]
    pub(crate) fn submit(
        mut self,
        request: PrepareControlRequest,
    ) -> Result<(Self, Vec<u8>), SubmitFailure> {
        if let Err(error) = self.core.require_lock() {
            return Err(SubmitFailure::Fatal(Box::new(FatalDurableRuntime::new(
                self.core, error,
            ))));
        }
        #[cfg(test)]
        if self.failpoint == Some(RuntimeFailpoint::Prepare) {
            return Err(SubmitFailure::Rejected {
                runtime: Box::new(self),
                error: injected("control prepare"),
            });
        }

        let requested_prepared = request.prepared.clone();
        let command_key = (
            requested_prepared.command_record.client_id.clone(),
            requested_prepared.command_record.client_command_id.clone(),
        );
        let writer = match self.core.take_control() {
            Ok(writer) => writer,
            Err(error) => {
                return Err(SubmitFailure::Fatal(Box::new(FatalDurableRuntime::new(
                    self.core, error,
                ))));
            }
        };
        let (writer, _outcome) = match writer.prepare(request) {
            Ok(success) => success,
            Err(ControlAppendFailure::Rejected { writer, error }) => {
                let capability_lost = error.is_capability_loss();
                let error = component_error("control prepare", error);
                if let Err(put_error) = self.core.put_control(writer) {
                    return Err(SubmitFailure::Fatal(Box::new(FatalDurableRuntime::new(
                        self.core, put_error,
                    ))));
                }
                if capability_lost {
                    return Err(SubmitFailure::Fatal(Box::new(FatalDurableRuntime::new(
                        self.core, error,
                    ))));
                }
                return Err(SubmitFailure::Rejected {
                    runtime: Box::new(self),
                    error,
                });
            }
            Err(ControlAppendFailure::Poisoned { writer, error }) => {
                let original = component_error("control prepare", error);
                match recover_control_ready(writer.recover()) {
                    Ok(writer) => {
                        if let Err(put_error) = self.core.put_control(writer) {
                            return Err(SubmitFailure::Fatal(Box::new(FatalDurableRuntime::new(
                                self.core, put_error,
                            ))));
                        }
                        let prepared = self.core.prepared_for_command(&command_key).cloned();
                        return match prepared {
                            Some(prepared) => {
                                match PreparedExecution::open(&self.core, &prepared) {
                                    Ok(execution) => Err(SubmitFailure::Recovering {
                                        runtime: Box::new(RecoveringDurableRuntime {
                                            core: self.core,
                                            prepared,
                                            execution,
                                            base_published: self.published,
                                            #[cfg(test)]
                                            failpoint: self.failpoint,
                                        }),
                                        error: original,
                                    }),
                                    Err(error) => Err(SubmitFailure::Fatal(Box::new(
                                        FatalDurableRuntime::new(self.core, error),
                                    ))),
                                }
                            }
                            None => Err(SubmitFailure::Rejected {
                                runtime: Box::new(self),
                                error: original,
                            }),
                        };
                    }
                    Err(recovery_error) => {
                        return Err(SubmitFailure::Fatal(Box::new(FatalDurableRuntime::new(
                            self.core,
                            recovery_error,
                        ))));
                    }
                }
            }
        };
        if let Err(error) = self.core.put_control(writer) {
            return Err(SubmitFailure::Fatal(Box::new(FatalDurableRuntime::new(
                self.core, error,
            ))));
        }

        let Some(authoritative) = self.core.prepared_for_command(&command_key).cloned() else {
            return Err(SubmitFailure::Fatal(Box::new(FatalDurableRuntime::new(
                self.core,
                component_error(
                    "control prepare",
                    "prepared command disappeared after fsync",
                ),
            ))));
        };
        if authoritative != requested_prepared {
            return Err(SubmitFailure::Rejected {
                runtime: Box::new(self),
                error: DurableRuntimeError::TransactionConflict,
            });
        }
        if self.core.is_committed(&authoritative.global_tx_id) {
            return self.return_committed_reply(&authoritative);
        }

        let execution = match PreparedExecution::open(&self.core, &authoritative) {
            Ok(execution) => execution,
            Err(error) => {
                return Err(SubmitFailure::Fatal(Box::new(FatalDurableRuntime::new(
                    self.core, error,
                ))));
            }
        };
        let recovering = Box::new(RecoveringDurableRuntime {
            core: self.core,
            prepared: authoritative,
            execution,
            base_published: self.published,
            #[cfg(test)]
            failpoint: self.failpoint,
        });
        match recovering.finish_once() {
            Ok((core, published, reply)) => {
                let ready = Self {
                    core,
                    published,
                    #[cfg(test)]
                    failpoint: self.failpoint,
                };
                Ok((ready, reply))
            }
            Err(FinishFailure::Recovering { runtime, error }) => {
                Err(SubmitFailure::Recovering { runtime, error })
            }
            Err(FinishFailure::Fatal(runtime)) => Err(SubmitFailure::Fatal(runtime)),
        }
    }

    fn return_committed_reply(
        self,
        authoritative: &PreparedTransactionV1,
    ) -> Result<(Self, Vec<u8>), SubmitFailure> {
        let reply = match authoritative.stable_reply() {
            Ok(reply) => reply,
            Err(error) => {
                return Err(SubmitFailure::Fatal(Box::new(FatalDurableRuntime::new(
                    self.core,
                    component_error("stable reply", error),
                ))));
            }
        };
        Ok((self, reply))
    }

    #[cfg(test)]
    pub(crate) fn set_failpoint(&mut self, failpoint: RuntimeFailpoint) {
        self.failpoint = Some(failpoint);
    }
}

fn allocate_domain_id_with(
    control: &ControlProjection,
    published: &DurableReadView,
    kind: DomainIdKind,
    additionally_reserved: &BTreeSet<String>,
    mut entropy: impl FnMut() -> Result<[u8; 16], ()>,
) -> Result<AllocatedDomainId, DomainIdAllocationError> {
    for _ in 0..ID_ALLOCATION_ATTEMPTS {
        let bytes = entropy().map_err(|()| DomainIdAllocationError::EntropyUnavailable)?;
        let candidate = format!("{}-{}", kind.prefix(), lowercase_hex_128(bytes));
        if !domain_id_is_occupied(control, published, additionally_reserved, &candidate) {
            return AllocatedDomainId::from_candidate(kind, candidate);
        }
    }
    Err(DomainIdAllocationError::Exhausted)
}

fn domain_id_is_occupied(
    control: &ControlProjection,
    published: &DurableReadView,
    additionally_reserved: &BTreeSet<String>,
    candidate: &str,
) -> bool {
    additionally_reserved.contains(candidate)
        || control.projects.contains(candidate)
        || control.sessions.contains_key(candidate)
        || control.sessions.values().any(|id| id == candidate)
        || published.projects.contains_key(candidate)
        || published.projects.values().any(|state| {
            state
                .snapshot
                .project_id
                .as_ref()
                .is_some_and(|id| id.as_str() == candidate)
                || state
                    .snapshot
                    .score_id
                    .as_ref()
                    .is_some_and(|id| id.as_str() == candidate)
                || state
                    .snapshot
                    .active_brief
                    .as_ref()
                    .is_some_and(|id| id.as_str() == candidate)
                || state
                    .snapshot
                    .accepted_revision
                    .as_ref()
                    .is_some_and(|id| id.as_str() == candidate)
                || state
                    .snapshot
                    .briefs
                    .keys()
                    .any(|id| id.as_str() == candidate)
                || state
                    .snapshot
                    .constraints
                    .keys()
                    .any(|id| id.as_str() == candidate)
                || state
                    .snapshot
                    .revisions
                    .keys()
                    .any(|id| id.as_str() == candidate)
                || state
                    .snapshot
                    .evidence
                    .keys()
                    .any(|id| id.as_str() == candidate)
                || state
                    .snapshot
                    .takes
                    .keys()
                    .any(|id| id.as_str() == candidate)
                || state
                    .snapshot
                    .branches
                    .keys()
                    .any(|id| id.as_str() == candidate)
                || state
                    .snapshot
                    .takes
                    .values()
                    .flat_map(|take| &take.branches)
                    .any(|id| id.as_str() == candidate)
                || state
                    .snapshot
                    .lifecycle
                    .keys()
                    .any(|id| id.as_str() == candidate)
                || state
                    .snapshot
                    .artifacts
                    .keys()
                    .any(|hash| hash.as_str() == candidate)
        })
        || published.sessions.contains_key(candidate)
        || published.sessions.values().any(|state| {
            state.snapshot().session_id.0 == candidate
                || state.turn_ids().any(|id| id == candidate)
                || state.question_ids().any(|id| id == candidate)
                || state.approval_ids().any(|id| id == candidate)
        })
        || published.owners.turns.contains_key(candidate)
        || published.owners.questions.contains_key(candidate)
        || published.owners.approvals.contains_key(candidate)
        || published.occurrence_metadata.contains_key(candidate)
}

fn allocate_global_transaction_id_with(
    mut prepared_contains: impl FnMut(&str) -> bool,
    additionally_reserved: &BTreeSet<String>,
    mut entropy: impl FnMut() -> Result<[u8; 16], ()>,
) -> Result<GlobalTransactionId, GlobalTransactionIdError> {
    for _ in 0..ID_ALLOCATION_ATTEMPTS {
        let bytes = entropy().map_err(|()| GlobalTransactionIdError::EntropyUnavailable)?;
        let candidate = format!("global-{}", lowercase_hex_128(bytes));
        if !prepared_contains(&candidate) && !additionally_reserved.contains(&candidate) {
            return GlobalTransactionId::new(candidate)
                .ok_or(GlobalTransactionIdError::EntropyUnavailable);
        }
    }
    Err(GlobalTransactionIdError::Exhausted)
}

fn os_random_128() -> Result<[u8; 16], ()> {
    let mut bytes = [0_u8; 16];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| ())?;
    Ok(bytes)
}

fn lowercase_hex_128(bytes: [u8; 16]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(32), |mut value, byte| {
            use std::fmt::Write as _;
            let _ignored = write!(value, "{byte:02x}");
            value
        })
}

fn is_prefixed_hex_128(value: &str, prefix: &str) -> bool {
    value
        .strip_prefix(prefix)
        .and_then(|suffix| suffix.strip_prefix('-'))
        .is_some_and(|hex| {
            hex.len() == 32
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

fn lookup_command_in_projection(
    projection: &ControlProjection,
    client_id: &ClientId,
    client_command_id: &ClientCommandId,
    payload_digest: &str,
) -> Result<CommandLookup, CommandLookupError> {
    let key = (client_id.0.clone(), client_command_id.0.clone());
    let Some(command) = projection.commands.get(&key) else {
        return Ok(CommandLookup::Unseen);
    };
    if command.command_record.payload_digest != payload_digest {
        return Err(CommandLookupError::IdempotencyConflict);
    }
    let corrupt = |detail: &'static str| {
        CommandLookupError::CorruptCommittedIndex(component_error("command lookup", detail))
    };
    if command.command_record.client_id != client_id.0
        || command.command_record.client_command_id != client_command_id.0
    {
        return Err(corrupt(
            "global command key does not match its stored record",
        ));
    }
    let prepared = projection
        .prepared
        .get(&command.global_tx_id)
        .ok_or_else(|| corrupt("global command does not name a Prepared transaction"))?;
    if prepared.global_tx_id != command.global_tx_id
        || prepared.command_record != command.command_record
    {
        return Err(corrupt(
            "Prepared transaction does not match the global command record",
        ));
    }
    if !projection.committed.contains_key(&command.global_tx_id) {
        return Err(corrupt("Prepared transaction has no Committed anchor"));
    }
    let (raw, _reply) = command
        .command_record
        .decode_reply_for_protocol(PROTOCOL_VERSION)
        .map_err(|error| {
            CommandLookupError::CorruptCommittedIndex(component_error("command lookup", error))
        })?;
    Ok(CommandLookup::ExactReply(raw))
}

impl PreparedExecution {
    fn open(
        core: &RuntimeCore,
        prepared: &PreparedTransactionV1,
    ) -> Result<Self, DurableRuntimeError> {
        let state_store = &core.components()?.state_store;
        let session_catalog_context = core.session_catalog_context()?.clone();
        let project_writer = prepared
            .project_plan
            .as_ref()
            .map(|plan| {
                let project_id = plan
                    .project_id()
                    .map_err(|error| component_error("Project plan", error))?;
                match state_store
                    .open_project_writer(project_id)
                    .map_err(|error| component_error("Project execution", error))?
                {
                    OpenProjectWriter::Ready(writer) => Ok(writer),
                    OpenProjectWriter::RepairRequired(writer) => writer.repair().map_err(|_| {
                        component_error("Project execution", "unrepairable final tail")
                    }),
                }
            })
            .transpose()?;
        let session_writer = prepared
            .session_plan
            .as_ref()
            .map(|plan| {
                let session_id = plan
                    .session_id()
                    .map_err(|error| component_error("Session plan", error))?;
                match state_store
                    .open_session_writer_with_catalog(session_id, session_catalog_context.clone())
                    .map_err(|error| component_error("Session execution", error))?
                {
                    OpenSessionWriter::Ready(writer) => Ok(writer),
                    OpenSessionWriter::RepairRequired(writer) => writer.repair().map_err(|_| {
                        component_error("Session execution", "unrepairable final tail")
                    }),
                }
            })
            .transpose()?;
        Ok(Self {
            project_writer,
            session_writer,
        })
    }
}

fn committed_read_view_candidate(
    base: &DurableReadView,
    control: Result<&ControlProjection, DurableRuntimeError>,
    prepared: &PreparedTransactionV1,
    execution: &PreparedExecution,
) -> Result<DurableReadView, DurableRuntimeError> {
    let control = control?;
    let mut candidate = base.clone();
    candidate.generation = candidate
        .generation
        .checked_add(1)
        .ok_or(DurableRuntimeError::CatalogMismatch)?;

    match (&prepared.project_plan, execution.project_writer.as_ref()) {
        (Some(plan), Some(writer)) => {
            let project_id = plan
                .project_id()
                .map_err(|error| component_error("Project read state", error))?;
            candidate.projects.insert(
                project_id.as_str().to_owned(),
                ProjectReadState::from_writer(writer)?,
            );
        }
        (None, None) => {}
        _ => return Err(DurableRuntimeError::TransactionConflict),
    }
    match (&prepared.session_plan, execution.session_writer.as_ref()) {
        (Some(plan), Some(writer)) => {
            let session_id = plan
                .session_id()
                .map_err(|error| component_error("Session read state", error))?;
            let state = writer
                .published_read_state()
                .map_err(|error| component_error("Session read state", error))?;
            candidate.sessions.insert(session_id.0, state);
        }
        (None, None) => {}
        _ => return Err(DurableRuntimeError::TransactionConflict),
    }
    candidate.refresh_derived_indexes(control)?;
    candidate.validate(control)?;
    Ok(candidate)
}

impl RecoveringDurableRuntime {
    /// 精确重试该状态保留的权威 transaction。
    pub(crate) fn recover(self: Box<Self>) -> Result<ReadyDurableRuntime, RecoveryFailure> {
        #[cfg(test)]
        let runtime = {
            let mut runtime = self;
            runtime.failpoint = None;
            runtime
        };
        #[cfg(not(test))]
        let runtime = self;
        match runtime.finish_once() {
            Ok((core, published, _reply)) => Ok(ReadyDurableRuntime {
                core,
                published,
                #[cfg(test)]
                failpoint: None,
            }),
            Err(FinishFailure::Recovering { runtime, error }) => Err(RecoveryFailure::Fatal(
                Box::new(FatalDurableRuntime::new(runtime.core, error)),
            )),
            Err(FinishFailure::Fatal(runtime)) => Err(RecoveryFailure::Fatal(runtime)),
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "恢复流程需要连续消费并归还同一组 writer capability"
    )]
    fn finish_once(
        mut self: Box<Self>,
    ) -> Result<(RuntimeCore, DurableReadView, Vec<u8>), FinishFailure> {
        #[cfg(test)]
        if self.failpoint == Some(RuntimeFailpoint::Project) && self.prepared.project_plan.is_some()
        {
            return Err(FinishFailure::Recovering {
                runtime: self,
                error: injected("Project append"),
            });
        }
        let project_last = if let Some(plan) = self.prepared.project_plan.clone() {
            let Some(writer) = self.execution.project_writer.take() else {
                return Err(FinishFailure::Fatal(Box::new(FatalDurableRuntime::new(
                    self.core,
                    component_error("Project execution", "writer capability unavailable"),
                ))));
            };
            let result = match self.core.components() {
                Ok(components) => redo_project_plan(
                    writer,
                    plan,
                    &components.artifact_store,
                    &components.artifact_recovery_guard,
                    &self.prepared.global_tx_id,
                    &self.prepared.artifact_audit_plans,
                ),
                Err(error) => {
                    return Err(FinishFailure::Fatal(Box::new(FatalDurableRuntime::new(
                        self.core, error,
                    ))));
                }
            };
            match result {
                Ok((writer, commit)) => {
                    self.execution.project_writer = Some(writer);
                    Some(commit)
                }
                Err(error) => {
                    return Err(FinishFailure::Fatal(Box::new(FatalDurableRuntime::new(
                        self.core, error,
                    ))));
                }
            }
        } else {
            None
        };
        #[cfg(test)]
        if self.failpoint == Some(RuntimeFailpoint::Session) && self.prepared.session_plan.is_some()
        {
            return Err(FinishFailure::Recovering {
                runtime: self,
                error: injected("Session append"),
            });
        }
        let session_last = if let Some(plan) = self.prepared.session_plan.clone() {
            let Some(writer) = self.execution.session_writer.take() else {
                return Err(FinishFailure::Fatal(Box::new(FatalDurableRuntime::new(
                    self.core,
                    component_error("Session execution", "writer capability unavailable"),
                ))));
            };
            match redo_session_plan(writer, plan) {
                Ok((writer, commit)) => {
                    self.execution.session_writer = Some(writer);
                    Some(commit)
                }
                Err(error) => {
                    return Err(FinishFailure::Fatal(Box::new(FatalDurableRuntime::new(
                        self.core, error,
                    ))));
                }
            }
        } else {
            None
        };
        #[cfg(test)]
        if self.failpoint == Some(RuntimeFailpoint::Commit) {
            return Err(FinishFailure::Recovering {
                runtime: self,
                error: injected("control commit"),
            });
        }
        #[cfg(test)]
        let fail_recovery_sync = self.failpoint == Some(RuntimeFailpoint::CommitRecoverySync);
        #[cfg(not(test))]
        let fail_recovery_sync = false;
        if let Err(error) = self.core.commit_prepared_with_anchors(
            &self.prepared,
            project_last,
            session_last,
            fail_recovery_sync,
        ) {
            return Err(FinishFailure::Fatal(Box::new(FatalDurableRuntime::new(
                self.core, error,
            ))));
        }
        let published = match committed_read_view_candidate(
            &self.base_published,
            self.core.control().map(ReadyControlWriter::projection),
            &self.prepared,
            &self.execution,
        ) {
            Ok(view) => view,
            Err(error) => {
                return Err(FinishFailure::Fatal(Box::new(FatalDurableRuntime::new(
                    self.core, error,
                ))));
            }
        };
        #[cfg(test)]
        if self.failpoint == Some(RuntimeFailpoint::Publish) {
            return Err(FinishFailure::Recovering {
                runtime: self,
                error: injected("published view"),
            });
        }
        let reply = match self.prepared.stable_reply() {
            Ok(reply) => reply,
            Err(error) => {
                return Err(FinishFailure::Fatal(Box::new(FatalDurableRuntime::new(
                    self.core,
                    component_error("stable reply", error),
                ))));
            }
        };
        #[cfg(test)]
        if self.failpoint == Some(RuntimeFailpoint::Response) {
            return Err(FinishFailure::Recovering {
                runtime: self,
                error: injected("response"),
            });
        }
        Ok((self.core, published, reply))
    }
}

impl FatalDurableRuntime {
    fn new(core: RuntimeCore, error: DurableRuntimeError) -> Self {
        core.health.invalidate();
        Self { _core: core, error }
    }

    pub(crate) fn error(&self) -> &DurableRuntimeError {
        &self.error
    }
}

impl RuntimeCore {
    fn components(&self) -> Result<&RuntimeComponents, DurableRuntimeError> {
        self.components
            .as_ref()
            .ok_or_else(|| component_error("durable runtime", "components unavailable"))
    }

    fn components_mut(&mut self) -> Result<&mut RuntimeComponents, DurableRuntimeError> {
        self.components
            .as_mut()
            .ok_or_else(|| component_error("durable runtime", "components unavailable"))
    }

    fn require_lock(&self) -> Result<(), DurableRuntimeError> {
        if self.instance_lock.is_none() {
            return Err(DurableRuntimeError::InstanceLockLost);
        }
        self.health.require_live()
    }

    fn control(&self) -> Result<&ReadyControlWriter, DurableRuntimeError> {
        self.components()?
            .control_writer
            .as_ref()
            .ok_or_else(|| component_error("control store", "writer unavailable"))
    }

    fn take_control(&mut self) -> Result<ReadyControlWriter, DurableRuntimeError> {
        self.components_mut()?
            .control_writer
            .take()
            .ok_or_else(|| component_error("control store", "writer unavailable"))
    }

    fn put_control(&mut self, writer: ReadyControlWriter) -> Result<(), DurableRuntimeError> {
        let session_catalog_context = writer.session_allocation_catalog_context();
        let components = self.components_mut()?;
        let slot = &mut components.control_writer;
        if slot.is_some() {
            return Err(component_error("control store", "writer already present"));
        }
        *slot = Some(writer);
        components.session_catalog_context = session_catalog_context;
        Ok(())
    }

    fn session_catalog_context(
        &self,
    ) -> Result<&SessionAllocationCatalogContext, DurableRuntimeError> {
        Ok(&self.components()?.session_catalog_context)
    }

    fn prepared_for_command(&self, key: &(String, String)) -> Option<&PreparedTransactionV1> {
        let projection = self.control().ok()?.projection();
        let command = projection.commands.get(key)?;
        projection.prepared.get(&command.global_tx_id)
    }

    fn is_committed(&self, global_tx_id: &str) -> bool {
        self.control()
            .is_ok_and(|writer| writer.projection().committed.contains_key(global_tx_id))
    }

    fn open_startup_aggregates(&self) -> Result<StartupAggregates, DurableRuntimeError> {
        let (project_ids, session_ids) = {
            let control = self.control()?.projection();
            (
                control.projects.clone(),
                control.sessions.keys().cloned().collect::<BTreeSet<_>>(),
            )
        };
        let state_store = &self.components()?.state_store;
        let session_catalog_context = self.session_catalog_context()?.clone();
        state_store
            .validate_project_directory_catalog(&project_ids, false)
            .map_err(|_| DurableRuntimeError::CatalogMismatch)?;
        state_store
            .validate_session_directory_catalog(&session_ids, false)
            .map_err(|_| DurableRuntimeError::CatalogMismatch)?;

        let mut projects = BTreeMap::new();
        for project_id in project_ids {
            let parsed = DomainProjectId::parse(project_id.clone())
                .map_err(|error| component_error("Project catalog", error))?;
            let writer = match state_store
                .open_project_writer(parsed)
                .map_err(|error| component_error("Project startup", error))?
            {
                OpenProjectWriter::Ready(writer) => writer,
                OpenProjectWriter::RepairRequired(writer) => writer
                    .repair()
                    .map_err(|_| component_error("Project startup", "unrepairable final tail"))?,
            };
            projects.insert(project_id, writer);
        }
        let mut sessions = BTreeMap::new();
        for session_id in session_ids {
            let writer = match state_store
                .open_session_writer_with_catalog(
                    SessionId(session_id.clone()),
                    session_catalog_context.clone(),
                )
                .map_err(|error| component_error("Session startup", error))?
            {
                OpenSessionWriter::Ready(writer) => writer,
                OpenSessionWriter::RepairRequired(writer) => writer
                    .repair()
                    .map_err(|_| component_error("Session startup", "unrepairable final tail"))?,
            };
            sessions.insert(session_id, writer);
        }
        state_store
            .validate_project_directory_catalog(
                &projects.keys().cloned().collect::<BTreeSet<_>>(),
                true,
            )
            .map_err(|_| DurableRuntimeError::CatalogMismatch)?;
        state_store
            .validate_session_directory_catalog(
                &sessions.keys().cloned().collect::<BTreeSet<_>>(),
                true,
            )
            .map_err(|_| DurableRuntimeError::CatalogMismatch)?;
        Ok(StartupAggregates { projects, sessions })
    }

    fn redo_startup_project(
        &self,
        aggregates: &mut StartupAggregates,
        prepared: &PreparedTransactionV1,
    ) -> Result<Option<TransactionCommit>, DurableRuntimeError> {
        let Some(plan) = prepared.project_plan.clone() else {
            return Ok(None);
        };
        let project_id = plan
            .project_id()
            .map_err(|error| component_error("Project plan", error))?
            .as_str()
            .to_owned();
        let writer = aggregates
            .projects
            .remove(&project_id)
            .ok_or(DurableRuntimeError::CatalogMismatch)?;
        let components = self.components()?;
        let (writer, commit) = redo_project_plan(
            writer,
            plan,
            &components.artifact_store,
            &components.artifact_recovery_guard,
            &prepared.global_tx_id,
            &prepared.artifact_audit_plans,
        )?;
        aggregates.projects.insert(project_id, writer);
        Ok(Some(commit))
    }

    fn redo_startup_session(
        aggregates: &mut StartupAggregates,
        prepared: &PreparedTransactionV1,
    ) -> Result<Option<TransactionCommit>, DurableRuntimeError> {
        let Some(plan) = prepared.session_plan.clone() else {
            return Ok(None);
        };
        let session_id = plan
            .session_id()
            .map_err(|error| component_error("Session plan", error))?
            .0;
        let writer = aggregates
            .sessions
            .remove(&session_id)
            .ok_or(DurableRuntimeError::CatalogMismatch)?;
        let (writer, commit) = redo_session_plan(writer, plan)?;
        aggregates.sessions.insert(session_id, writer);
        Ok(Some(commit))
    }

    fn commit_prepared_with_anchors(
        &mut self,
        prepared: &PreparedTransactionV1,
        project_last: Option<TransactionCommit>,
        session_last: Option<TransactionCommit>,
        fail_recovery_sync: bool,
    ) -> Result<(), DurableRuntimeError> {
        if project_last.is_some() != prepared.project_plan.is_some()
            || session_last.is_some() != prepared.session_plan.is_some()
        {
            return Err(DurableRuntimeError::TransactionConflict);
        }
        let request = CommitControlRequest {
            global_tx_id: prepared.global_tx_id.clone(),
            project_last: project_last.map(AggregateCommitV1::from),
            session_last: session_last.map(AggregateCommitV1::from),
        };
        self.commit_control_request(prepared, request, fail_recovery_sync)
    }

    fn plan_startup_restarts(
        &self,
        aggregates: &StartupAggregates,
    ) -> Result<Vec<PrepareControlRequest>, DurableRuntimeError> {
        let instance_id = self.components()?.state_store.instance_id();
        let mut requests = Vec::new();
        for writer in aggregates.sessions.values() {
            let Some(restart) =
                plan_coordinated_restart_reconciliation(instance_id, writer.projection())
                    .map_err(|error| component_error("restart planning", error))?
            else {
                continue;
            };
            let snapshot = writer
                .snapshot()
                .map_err(|error| component_error("restart planning", error))?;
            let (pre_sequence, pre_checksum) = writer.head();
            let session_plan = StoredSessionPlanV1::from_append_request(
                &snapshot.session_id,
                &snapshot.project_id,
                pre_sequence,
                pre_checksum.map(ToOwned::to_owned),
                &restart.append_request(),
            )
            .map_err(|error| component_error("restart planning", error))?;
            let prepared = PreparedTransactionV1::new(
                restart.global_tx_id,
                restart.command_record,
                None,
                Some(session_plan),
                Vec::new(),
            )
            .map_err(|error| component_error("restart planning", error))?;
            requests.push(PrepareControlRequest {
                project_allocation: None,
                session_allocation: None,
                prepared,
            });
        }
        self.control()?
            .projection()
            .require_internal_restart_capacity(requests.len())
            .map_err(|error| component_error("restart capacity", error))?;
        Ok(requests)
    }

    fn prepare_startup_control(
        &mut self,
        request: PrepareControlRequest,
    ) -> Result<(), DurableRuntimeError> {
        let writer = self.take_control()?;
        match writer.prepare(request) {
            Ok((writer, _)) => {
                self.put_control(writer)?;
                Ok(())
            }
            Err(ControlAppendFailure::Rejected { writer, error }) => {
                self.put_control(writer)?;
                Err(component_error("restart control prepare", error))
            }
            Err(ControlAppendFailure::Poisoned { error, .. }) => {
                Err(component_error("restart control prepare", error))
            }
        }
    }

    fn redo_and_commit(
        &mut self,
        prepared: &PreparedTransactionV1,
    ) -> Result<(), DurableRuntimeError> {
        self.redo_project(prepared)?;
        self.redo_session(prepared)?;
        self.commit_prepared(prepared, false)
    }

    fn redo_project(
        &mut self,
        prepared: &PreparedTransactionV1,
    ) -> Result<Option<TransactionCommit>, DurableRuntimeError> {
        self.require_lock()?;
        let Some(plan) = prepared.project_plan.clone() else {
            return Ok(None);
        };
        let project_id = plan
            .project_id()
            .map_err(|error| component_error("Project plan", error))?;
        let writer = match self
            .components()?
            .state_store
            .open_project_writer(project_id.clone())
            .map_err(|error| component_error("Project open", error))?
        {
            OpenProjectWriter::Ready(writer) => writer,
            OpenProjectWriter::RepairRequired(writer) => writer
                .repair()
                .map_err(|_| component_error("Project repair", "unrepairable final tail"))?,
        };
        let components = self.components()?;
        let result = redo_project_plan(
            writer,
            plan,
            &components.artifact_store,
            &components.artifact_recovery_guard,
            &prepared.global_tx_id,
            &prepared.artifact_audit_plans,
        );
        result.map(|(_writer, commit)| Some(commit))
    }

    fn redo_session(
        &mut self,
        prepared: &PreparedTransactionV1,
    ) -> Result<Option<TransactionCommit>, DurableRuntimeError> {
        self.require_lock()?;
        let Some(plan) = prepared.session_plan.clone() else {
            return Ok(None);
        };
        let session_id = plan
            .session_id()
            .map_err(|error| component_error("Session plan", error))?;
        let writer = match self
            .components()?
            .state_store
            .open_session_writer_with_catalog(session_id, self.session_catalog_context()?.clone())
            .map_err(|error| component_error("Session open", error))?
        {
            OpenSessionWriter::Ready(writer) => writer,
            OpenSessionWriter::RepairRequired(writer) => writer
                .repair()
                .map_err(|_| component_error("Session repair", "unrepairable final tail"))?,
        };
        redo_session_plan(writer, plan).map(|(_writer, commit)| Some(commit))
    }

    fn commit_prepared(
        &mut self,
        prepared: &PreparedTransactionV1,
        fail_recovery_sync: bool,
    ) -> Result<(), DurableRuntimeError> {
        #[cfg(not(test))]
        let _ = fail_recovery_sync;
        self.require_lock()?;
        if self.is_committed(&prepared.global_tx_id) {
            return Ok(());
        }
        let project_last = prepared
            .project_plan
            .as_ref()
            .map(|plan| self.probe_project_commit(plan))
            .transpose()?
            .map(AggregateCommitV1::from);
        let session_last = prepared
            .session_plan
            .as_ref()
            .map(|plan| self.probe_session_commit(plan))
            .transpose()?
            .map(AggregateCommitV1::from);
        let request = CommitControlRequest {
            global_tx_id: prepared.global_tx_id.clone(),
            project_last,
            session_last,
        };
        self.commit_control_request(prepared, request, fail_recovery_sync)
    }

    fn commit_control_request(
        &mut self,
        prepared: &PreparedTransactionV1,
        request: CommitControlRequest,
        fail_recovery_sync: bool,
    ) -> Result<(), DurableRuntimeError> {
        #[cfg(not(test))]
        let _ = fail_recovery_sync;
        #[cfg(test)]
        let mut writer = self.take_control()?;
        #[cfg(not(test))]
        let writer = self.take_control()?;
        #[cfg(test)]
        if fail_recovery_sync {
            writer.set_failpoint(ControlAppendFailpoint::FileSync);
        }
        match writer.commit(request) {
            Ok((writer, _)) => {
                self.put_control(writer)?;
                Ok(())
            }
            Err(ControlAppendFailure::Rejected { writer, error }) => {
                self.put_control(writer)?;
                Err(component_error("control commit", error))
            }
            Err(ControlAppendFailure::Poisoned { writer, error }) => {
                #[cfg(test)]
                let mut writer = writer;
                let original = component_error("control commit", error);
                #[cfg(test)]
                if fail_recovery_sync {
                    writer.set_recovery_failpoint(ControlRecoveryFailpoint::FileSync);
                }
                let writer = recover_control_ready(writer.recover())?;
                self.put_control(writer)?;
                if self.is_committed(&prepared.global_tx_id) {
                    Ok(())
                } else {
                    Err(original)
                }
            }
        }
    }

    fn probe_project_commit(
        &self,
        plan: &StoredProjectPlanV1,
    ) -> Result<TransactionCommit, DurableRuntimeError> {
        let project_id = plan
            .project_id()
            .map_err(|error| component_error("Project plan", error))?;
        let writer = match self
            .components()?
            .state_store
            .open_project_writer(project_id)
            .map_err(|error| component_error("Project probe", error))?
        {
            OpenProjectWriter::Ready(writer) => writer,
            OpenProjectWriter::RepairRequired(_) => {
                return Err(component_error(
                    "Project probe",
                    "repair required after redo",
                ));
            }
        };
        match writer.probe_transaction(plan.transaction_id(), plan.canonical_plan_digest()) {
            TransactionProbe::SamePlanCommitted(commit) => Ok(commit),
            TransactionProbe::Absent | TransactionProbe::ConflictingPlan => {
                Err(DurableRuntimeError::TransactionConflict)
            }
        }
    }

    fn probe_session_commit(
        &self,
        plan: &StoredSessionPlanV1,
    ) -> Result<TransactionCommit, DurableRuntimeError> {
        let session_id = plan
            .session_id()
            .map_err(|error| component_error("Session plan", error))?;
        let writer = match self
            .components()?
            .state_store
            .open_session_writer_with_catalog(session_id, self.session_catalog_context()?.clone())
            .map_err(|error| component_error("Session probe", error))?
        {
            OpenSessionWriter::Ready(writer) => writer,
            OpenSessionWriter::RepairRequired(_) => {
                return Err(component_error(
                    "Session probe",
                    "repair required after redo",
                ));
            }
        };
        match writer.probe_transaction(plan.transaction_id(), plan.canonical_plan_digest()) {
            TransactionProbe::SamePlanCommitted(commit) => Ok(commit),
            TransactionProbe::Absent | TransactionProbe::ConflictingPlan => {
                Err(DurableRuntimeError::TransactionConflict)
            }
        }
    }

    fn audit_startup_transactions(
        &self,
        aggregates: &StartupAggregates,
    ) -> Result<(), DurableRuntimeError> {
        let (expected_projects, expected_sessions) =
            expected_transactions(self.control()?.projection())?;
        for (project_id, writer) in &aggregates.projects {
            let expected = expected_projects
                .get(project_id)
                .cloned()
                .unwrap_or_default();
            audit_transaction_index(writer.transaction_index(), &expected, false)?;
        }
        for (session_id, writer) in &aggregates.sessions {
            let expected = expected_sessions
                .get(session_id)
                .cloned()
                .unwrap_or_default();
            audit_transaction_index(writer.transaction_index(), &expected, true)?;
        }
        if expected_projects
            .keys()
            .any(|project_id| !aggregates.projects.contains_key(project_id))
            || expected_sessions
                .keys()
                .any(|session_id| !aggregates.sessions.contains_key(session_id))
        {
            return Err(DurableRuntimeError::TransactionConflict);
        }
        Ok(())
    }

    fn startup_read_view(
        &self,
        aggregates: &StartupAggregates,
    ) -> Result<DurableReadView, DurableRuntimeError> {
        let control = self.control()?.projection();
        if aggregates.projects.len() != control.projects.len()
            || aggregates.sessions.len() != control.sessions.len()
        {
            return Err(DurableRuntimeError::CatalogMismatch);
        }
        let mut projects = BTreeMap::new();
        for (project_id, writer) in &aggregates.projects {
            let state = ProjectReadState::from_writer(writer)?;
            if state
                .snapshot
                .project_id
                .as_ref()
                .map(DomainProjectId::as_str)
                != Some(project_id.as_str())
            {
                return Err(DurableRuntimeError::CatalogMismatch);
            }
            projects.insert(project_id.clone(), state);
        }
        let mut sessions = BTreeMap::new();
        for (session_id, writer) in &aggregates.sessions {
            let state = writer
                .published_read_state()
                .map_err(|error| component_error("Session projection", error))?;
            if state.snapshot().session_id.0 != *session_id
                || control.sessions.get(session_id) != Some(&state.snapshot().project_id.0)
            {
                return Err(DurableRuntimeError::CatalogMismatch);
            }
            sessions.insert(session_id.clone(), state);
        }
        let mut candidate = DurableReadView {
            projects,
            sessions,
            owners: OwnerIndex::default(),
            project_metadata: BTreeMap::new(),
            occurrence_metadata: BTreeMap::new(),
            reachable_artifact_hashes: BTreeSet::new(),
            generation: 1,
        };
        candidate.refresh_derived_indexes(control)?;
        candidate.validate(control)?;
        Ok(candidate)
    }

    fn validate_catalog(&self, strict: bool) -> Result<(), DurableRuntimeError> {
        let control = self.control()?.projection();
        let projects = self
            .components()?
            .state_store
            .list_projects()
            .map_err(|error| component_error("Project catalog", error))?;
        let sessions = self
            .components()?
            .state_store
            .list_sessions_with_catalog(self.session_catalog_context()?)
            .map_err(|error| component_error("Session catalog", error))?;

        for project_id in projects.projects.keys() {
            if !control.projects.contains(project_id) {
                return Err(DurableRuntimeError::CatalogMismatch);
            }
        }
        for (session_id, snapshot) in &sessions.sessions {
            if control.sessions.get(session_id) != Some(&snapshot.project_id.0) {
                return Err(DurableRuntimeError::CatalogMismatch);
            }
        }
        if strict
            && (projects.projects.len() != control.projects.len()
                || sessions.sessions.len() != control.sessions.len())
        {
            return Err(DurableRuntimeError::CatalogMismatch);
        }
        Ok(())
    }

    /// 证明每个 control `Committed` anchor 仍指向 aggregate log 记录的精确
    /// Project/Session transaction。仅有 catalog 记录并不充分，因为完整 aggregate tail
    /// 可能在目录仍存在时被截断。
    fn audit_committed_transactions(&self) -> Result<(), DurableRuntimeError> {
        let (prepared, committed) = {
            let projection = self.control()?.projection();
            (projection.prepared.clone(), projection.committed.clone())
        };
        if prepared.len() != committed.len() {
            return Err(DurableRuntimeError::TransactionConflict);
        }
        let mut expected_projects = BTreeMap::<String, BTreeMap<String, TransactionCommit>>::new();
        let mut expected_sessions = BTreeMap::<String, BTreeMap<String, TransactionCommit>>::new();
        for (global_tx_id, prepared) in prepared {
            let committed = committed
                .get(&global_tx_id)
                .ok_or(DurableRuntimeError::TransactionConflict)?;
            match (&prepared.project_plan, &committed.project_last) {
                (Some(plan), Some(anchor)) => {
                    let project_id = plan
                        .project_id()
                        .map_err(|error| component_error("Project plan", error))?;
                    insert_expected_transaction(
                        expected_projects
                            .entry(project_id.as_str().to_owned())
                            .or_default(),
                        plan.transaction_id(),
                        plan.canonical_plan_digest(),
                        anchor,
                    )?;
                }
                (None, None) => {}
                _ => return Err(DurableRuntimeError::TransactionConflict),
            }
            match (&prepared.session_plan, &committed.session_last) {
                (Some(plan), Some(anchor)) => {
                    let session_id = plan
                        .session_id()
                        .map_err(|error| component_error("Session plan", error))?;
                    insert_expected_transaction(
                        expected_sessions.entry(session_id.0).or_default(),
                        plan.transaction_id(),
                        plan.canonical_plan_digest(),
                        anchor,
                    )?;
                }
                (None, None) => {}
                _ => return Err(DurableRuntimeError::TransactionConflict),
            }
        }

        for (project_id, expected) in expected_projects {
            let project_id = DomainProjectId::parse(project_id)
                .map_err(|error| component_error("Project audit", error))?;
            let writer = match self
                .components()?
                .state_store
                .open_project_writer(project_id)
                .map_err(|error| component_error("Project audit", error))?
            {
                OpenProjectWriter::Ready(writer) => writer,
                OpenProjectWriter::RepairRequired(_) => {
                    return Err(component_error("Project audit", "repair required"));
                }
            };
            audit_transaction_index(writer.transaction_index(), &expected, false)?;
        }
        for (session_id, expected) in expected_sessions {
            let writer = match self
                .components()?
                .state_store
                .open_session_writer_with_catalog(
                    crate::protocol::SessionId(session_id),
                    self.session_catalog_context()?.clone(),
                )
                .map_err(|error| component_error("Session audit", error))?
            {
                OpenSessionWriter::Ready(writer) => writer,
                OpenSessionWriter::RepairRequired(_) => {
                    return Err(component_error("Session audit", "repair required"));
                }
            };
            audit_transaction_index(writer.transaction_index(), &expected, true)?;
        }
        Ok(())
    }
}

type ExpectedTransactions = BTreeMap<String, BTreeMap<String, TransactionCommit>>;

fn expected_transactions(
    control: &ControlProjection,
) -> Result<(ExpectedTransactions, ExpectedTransactions), DurableRuntimeError> {
    if control.prepared.len() != control.committed.len() {
        return Err(DurableRuntimeError::TransactionConflict);
    }
    let mut expected_projects = BTreeMap::new();
    let mut expected_sessions = BTreeMap::new();
    for (global_tx_id, prepared) in &control.prepared {
        let committed = control
            .committed
            .get(global_tx_id)
            .ok_or(DurableRuntimeError::TransactionConflict)?;
        match (&prepared.project_plan, &committed.project_last) {
            (Some(plan), Some(anchor)) => {
                let project_id = plan
                    .project_id()
                    .map_err(|error| component_error("Project plan", error))?;
                insert_expected_transaction(
                    expected_projects
                        .entry(project_id.as_str().to_owned())
                        .or_default(),
                    plan.transaction_id(),
                    plan.canonical_plan_digest(),
                    anchor,
                )?;
            }
            (None, None) => {}
            _ => return Err(DurableRuntimeError::TransactionConflict),
        }
        match (&prepared.session_plan, &committed.session_last) {
            (Some(plan), Some(anchor)) => {
                let session_id = plan
                    .session_id()
                    .map_err(|error| component_error("Session plan", error))?;
                insert_expected_transaction(
                    expected_sessions.entry(session_id.0).or_default(),
                    plan.transaction_id(),
                    plan.canonical_plan_digest(),
                    anchor,
                )?;
            }
            (None, None) => {}
            _ => return Err(DurableRuntimeError::TransactionConflict),
        }
    }
    Ok((expected_projects, expected_sessions))
}

fn insert_expected_transaction(
    expected: &mut BTreeMap<String, TransactionCommit>,
    transaction_id: &str,
    canonical_plan_digest: &str,
    anchor: &AggregateCommitV1,
) -> Result<(), DurableRuntimeError> {
    let commit = TransactionCommit {
        canonical_plan_digest: canonical_plan_digest.to_owned(),
        resulting_last_sequence: anchor.resulting_last_sequence,
        resulting_batch_checksum: anchor.resulting_batch_checksum.clone(),
    };
    if expected.insert(transaction_id.to_owned(), commit).is_some() {
        return Err(DurableRuntimeError::TransactionConflict);
    }
    Ok(())
}

fn audit_transaction_index(
    actual: &BTreeMap<String, TransactionCommit>,
    expected: &BTreeMap<String, TransactionCommit>,
    allow_legacy_restart: bool,
) -> Result<(), DurableRuntimeError> {
    for (transaction_id, expected_commit) in expected {
        if actual.get(transaction_id) != Some(expected_commit) {
            return Err(DurableRuntimeError::TransactionConflict);
        }
    }
    for transaction_id in actual.keys() {
        if expected.contains_key(transaction_id)
            || (allow_legacy_restart && is_legacy_restart_transaction_id(transaction_id))
        {
            continue;
        }
        return Err(DurableRuntimeError::TransactionConflict);
    }
    Ok(())
}

fn is_legacy_restart_transaction_id(transaction_id: &str) -> bool {
    let Some(rest) = transaction_id.strip_prefix("restart-v1:") else {
        return false;
    };
    let mut parts = rest.split(':');
    let (Some(instance_id), Some(session_id), Some(pre_head), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    instance_id.len() == 32
        && instance_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && !session_id.is_empty()
        && session_id.len() <= 256
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        && pre_head.parse::<u64>().is_ok()
}

fn redo_project_plan(
    writer: ReadyProjectWriter,
    plan: StoredProjectPlanV1,
    artifact_store: &ArtifactStore,
    recovery_guard: &ArtifactRecoveryGuard,
    global_tx_id: &str,
    audit_plans: &[crate::artifact_store::ArtifactAuditPlanV1],
) -> Result<(ReadyProjectWriter, TransactionCommit), DurableRuntimeError> {
    let digest = plan.canonical_plan_digest().to_owned();
    let transaction_id = plan.transaction_id().to_owned();
    match writer.probe_transaction(&transaction_id, &digest) {
        TransactionProbe::SamePlanCommitted(commit) => return Ok((writer, commit)),
        TransactionProbe::ConflictingPlan => {
            return Err(DurableRuntimeError::TransactionConflict);
        }
        TransactionProbe::Absent => {}
    }
    let expected_head = (
        plan.expected_pre_sequence(),
        plan.expected_pre_batch_checksum().map(ToOwned::to_owned),
    );
    let actual_head = writer.head().map_checksum(ToOwned::to_owned);
    if actual_head != expected_head {
        return Err(DurableRuntimeError::TransactionConflict);
    }

    let stored_artifacts = plan.registered_artifact_events();
    if stored_artifacts.len() != audit_plans.len() {
        return Err(DurableRuntimeError::TransactionConflict);
    }
    let mut recovered_artifacts = Vec::with_capacity(stored_artifacts.len());
    for (stored_event, audit_plan) in stored_artifacts.into_iter().zip(audit_plans) {
        match recover_artifact_for_project_plan(
            &writer,
            &transaction_id,
            &digest,
            artifact_store,
            recovery_guard,
            global_tx_id,
            stored_event,
            audit_plan,
        )
        .map_err(|error| component_error("Artifact recovery", error))?
        {
            RecoveredArtifactProjectHandoff::Append(event) => recovered_artifacts.push(event),
            RecoveredArtifactProjectHandoff::AlreadyCommitted(commit) => {
                return Ok((writer, commit));
            }
        }
    }
    let request = plan
        .into_append_request(recovered_artifacts)
        .map_err(|error| component_error("Project plan", error))?;
    let writer = match writer.append(request) {
        Ok((writer, _)) => writer,
        Err(AppendFailure::Rejected { error, .. }) => {
            return Err(component_error("Project append", error));
        }
        Err(AppendFailure::Poisoned { writer, error }) => {
            let original = component_error("Project append", error);
            let writer = recover_project_ready(writer.recover())?;
            return match writer.probe_transaction(&transaction_id, &digest) {
                TransactionProbe::SamePlanCommitted(commit) => Ok((writer, commit)),
                TransactionProbe::Absent => Err(original),
                TransactionProbe::ConflictingPlan => Err(DurableRuntimeError::TransactionConflict),
            };
        }
    };
    match writer.probe_transaction(&transaction_id, &digest) {
        TransactionProbe::SamePlanCommitted(commit) => Ok((writer, commit)),
        TransactionProbe::Absent | TransactionProbe::ConflictingPlan => {
            Err(DurableRuntimeError::TransactionConflict)
        }
    }
}

fn redo_session_plan(
    writer: ReadySessionWriter,
    plan: StoredSessionPlanV1,
) -> Result<(ReadySessionWriter, TransactionCommit), DurableRuntimeError> {
    let digest = plan.canonical_plan_digest().to_owned();
    let transaction_id = plan.transaction_id().to_owned();
    match writer.probe_transaction(&transaction_id, &digest) {
        TransactionProbe::SamePlanCommitted(commit) => return Ok((writer, commit)),
        TransactionProbe::ConflictingPlan => {
            return Err(DurableRuntimeError::TransactionConflict);
        }
        TransactionProbe::Absent => {}
    }
    let expected_head = (
        plan.expected_pre_sequence(),
        plan.expected_pre_batch_checksum().map(ToOwned::to_owned),
    );
    let actual_head = writer.head().map_checksum(ToOwned::to_owned);
    if actual_head != expected_head {
        return Err(DurableRuntimeError::TransactionConflict);
    }
    let request = plan
        .into_append_request()
        .map_err(|error| component_error("Session plan", error))?;
    let writer = match writer.append(request) {
        Ok((writer, _)) => writer,
        Err(SessionAppendFailure::Rejected { error, .. }) => {
            return Err(component_error("Session append", error));
        }
        Err(SessionAppendFailure::Poisoned { writer, error }) => {
            let original = component_error("Session append", error);
            let writer = recover_session_ready(writer.recover())?;
            return match writer.probe_transaction(&transaction_id, &digest) {
                TransactionProbe::SamePlanCommitted(commit) => Ok((writer, commit)),
                TransactionProbe::Absent => Err(original),
                TransactionProbe::ConflictingPlan => Err(DurableRuntimeError::TransactionConflict),
            };
        }
    };
    match writer.probe_transaction(&transaction_id, &digest) {
        TransactionProbe::SamePlanCommitted(commit) => Ok((writer, commit)),
        TransactionProbe::Absent | TransactionProbe::ConflictingPlan => {
            Err(DurableRuntimeError::TransactionConflict)
        }
    }
}

trait HeadChecksumExt {
    fn map_checksum(self, map: impl FnOnce(&str) -> String) -> (u64, Option<String>);
}

impl HeadChecksumExt for (u64, Option<&str>) {
    fn map_checksum(self, map: impl FnOnce(&str) -> String) -> (u64, Option<String>) {
        (self.0, self.1.map(map))
    }
}

fn recover_project_ready(
    outcome: RecoveryOutcome,
) -> Result<ReadyProjectWriter, DurableRuntimeError> {
    match outcome {
        RecoveryOutcome::Ready(writer) => Ok(writer),
        RecoveryOutcome::RepairRequired(writer) => writer
            .repair()
            .map_err(|_| component_error("Project recovery", "unrepairable final tail")),
        RecoveryOutcome::Corrupt(_) => Err(component_error("Project recovery", "corrupt stream")),
    }
}

fn recover_session_ready(
    outcome: SessionRecoveryOutcome,
) -> Result<ReadySessionWriter, DurableRuntimeError> {
    match outcome {
        SessionRecoveryOutcome::Ready(writer) => Ok(writer),
        SessionRecoveryOutcome::RepairRequired(writer) => writer
            .repair()
            .map_err(|_| component_error("Session recovery", "unrepairable final tail")),
        SessionRecoveryOutcome::Corrupt(_) => {
            Err(component_error("Session recovery", "corrupt stream"))
        }
    }
}

fn recover_control_ready(
    outcome: ControlRecoveryOutcome,
) -> Result<ReadyControlWriter, DurableRuntimeError> {
    match outcome {
        ControlRecoveryOutcome::Ready(writer) => Ok(writer),
        ControlRecoveryOutcome::RepairRequired(writer) => writer
            .repair()
            .map_err(|_| component_error("control recovery", "unrepairable final tail")),
        ControlRecoveryOutcome::Corrupt(_) => {
            Err(component_error("control recovery", "corrupt stream"))
        }
    }
}

#[cfg(test)]
fn injected(stage: &'static str) -> DurableRuntimeError {
    DurableRuntimeError::InjectedFailure { stage }
}

fn open_absolute_private_root(path: &Path) -> Result<OwnedFd, DurableRuntimeError> {
    if !path.is_absolute() {
        return Err(DurableRuntimeError::InvalidDataRoot);
    }
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() <= 1
        || bytes[1..]
            .split(|byte| *byte == b'/')
            .any(|component| component.is_empty() || component == b"." || component == b"..")
    {
        return Err(DurableRuntimeError::InvalidDataRoot);
    }
    let mut current = openat(CWD, "/", DIRECTORY_FLAGS, Mode::empty())
        .map_err(|source| runtime_io("open filesystem root", source))?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                current = openat(&current, name, DIRECTORY_FLAGS, Mode::empty())
                    .map_err(|source| runtime_io("open data root component", source))?;
            }
            _ => return Err(DurableRuntimeError::InvalidDataRoot),
        }
    }
    let stat = fstat(&current).map_err(|source| runtime_io("inspect data root", source))?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_dir()
        || stat.st_uid != rustix::process::getuid().as_raw()
        || stat.st_mode & 0o777 != ROOT_MODE
    {
        return Err(DurableRuntimeError::InvalidDataRoot);
    }
    Ok(current)
}

fn validate_lock_file(file: &File) -> Result<(), DurableRuntimeError> {
    let stat = fstat(file).map_err(|source| runtime_io("inspect instance lock", source))?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file()
        || stat.st_uid != rustix::process::getuid().as_raw()
        || stat.st_mode & 0o777 != LOCK_MODE
    {
        return Err(DurableRuntimeError::InvalidDataRoot);
    }
    Ok(())
}

fn runtime_io(operation: &'static str, source: impl Into<std::io::Error>) -> DurableRuntimeError {
    DurableRuntimeError::Io {
        operation,
        source: source.into(),
    }
}

fn random_hex_128() -> String {
    let mut bytes = [0_u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes
        .iter()
        .fold(String::with_capacity(32), |mut value, byte| {
            use std::fmt::Write as _;
            let _ignored = write!(value, "{byte:02x}");
            value
        })
}

#[cfg(test)]
fn inject_lock(
    actual: Option<LockFailpoint>,
    expected: LockFailpoint,
    operation: &'static str,
) -> Result<(), DurableRuntimeError> {
    if actual == Some(expected) {
        return Err(runtime_io(operation, std::io::Error::other("injected")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{Cursor, Read as _};
    use std::os::unix::fs::PermissionsExt as _;

    use sha2::Digest as _;

    use crate::artifact_store::ArtifactAuditPlanV1;
    use crate::control_store::{
        PrepareControlRequest, PreparedTransactionV1, SessionAllocation, project_transaction_id,
        session_transaction_id,
    };
    use crate::domain::{
        ArtifactRecord, BranchId, BriefRevisionId, CreativeBrief, DomainProjectId, ProjectEvent,
        ScoreId, TakeId,
    };
    use crate::protocol::{
        ApprovalId, ApprovalPayload, ArtifactHash as ProtocolArtifactHash, ArtifactOccurrenceId,
        ClientCommand, ClientCommandId, CommandReply, EffectClass, PROTOCOL_VERSION, ProjectId,
        ProtocolErrorCode, SessionId, TurnId, TurnStatus, approval_subject_digest_v1,
        external_command_payload_digest,
    };
    use crate::state_store::session::{
        ApprovalSubjectInputsV1, CommandOnlyReasonV1,
        RecoveryFailpoint as SessionRecoveryFailpoint, SessionAppendRequest, SessionRolloutEvent,
        StoredCommandOnlyAuthorizationV1, plan_restart_reconciliation,
    };
    use crate::state_store::{
        AppendFailpoint, AppendRequest, OpenProjectWriter,
        RecoveryFailpoint as ProjectRecoveryFailpoint, StoredCommandRecordV1, StoredProjectPlanV1,
        StoredSessionPlanV1,
    };

    use super::*;

    const GLOBAL_TX: &str = "global-22222222222222222222222222222222";

    fn private_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("temp root");
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(ROOT_MODE))
            .expect("private root");
        root
    }

    fn domain_project(value: &str) -> DomainProjectId {
        DomainProjectId::parse(value).expect("Project ID")
    }

    fn stable_reply(command_id: &str) -> Vec<u8> {
        serde_json::to_vec(&crate::protocol::CommandReply::error(
            ClientCommandId(command_id.to_owned()),
            ProtocolErrorCode::InvalidRequest,
            "stable durable reply",
        ))
        .expect("canonical reply")
    }

    fn project_created_reply(command_id: &str, project_id: &str, name: &str) -> Vec<u8> {
        serde_json::to_vec(&CommandReply::success(
            ClientCommandId(command_id.to_owned()),
            CommandResult::ProjectCreated(ProtocolProjectSnapshot {
                project_id: ProjectId(project_id.to_owned()),
                name: name.to_owned(),
                version: 1,
            }),
        ))
        .expect("ProjectCreated reply")
    }

    fn project_create_prepare_for(
        global_tx_id: &str,
        command_id: &str,
        project_value: &str,
        fixture_suffix: &str,
    ) -> PrepareControlRequest {
        let command = StoredCommandRecordV1::new(
            "client-runtime",
            command_id,
            format!("sha256:{}", "2".repeat(64)),
            &project_created_reply(command_id, project_value, fixture_suffix),
        )
        .expect("Project create command");
        let project_id = domain_project(project_value);
        let request = AppendRequest {
            transaction_id: project_transaction_id(global_tx_id),
            command_record: Some(command.clone()),
            events: vec![ProjectEvent::ProjectInitialized {
                project_id: project_id.clone(),
                score_id: ScoreId::parse(format!("score-{fixture_suffix}")).expect("score"),
                default_take_id: TakeId::parse(format!("take-{fixture_suffix}")).expect("take"),
                default_branch_id: BranchId::parse(format!("branch-{fixture_suffix}"))
                    .expect("branch"),
            }],
        };
        let project_plan = StoredProjectPlanV1::from_append_request(&project_id, 0, None, &request)
            .expect("Project create plan");
        PrepareControlRequest {
            project_allocation: Some(project_id),
            session_allocation: None,
            prepared: PreparedTransactionV1::new(
                global_tx_id.to_owned(),
                command,
                Some(project_plan),
                None,
                Vec::new(),
            )
            .expect("Project create Prepared"),
        }
    }

    fn session_start_prepare_for(
        global_tx_id: &str,
        command_id: &str,
        project_value: &str,
        session_value: &str,
    ) -> PrepareControlRequest {
        let command = StoredCommandRecordV1::new(
            "client-runtime",
            command_id,
            format!("sha256:{}", "4".repeat(64)),
            &stable_reply(command_id),
        )
        .expect("Session start command");
        let session_id = SessionId(session_value.to_owned());
        let project_id = ProjectId(project_value.to_owned());
        let request = SessionAppendRequest::new(
            session_transaction_id(global_tx_id),
            Some(command.clone()),
            vec![SessionRolloutEvent::SessionStarted {
                session_id: session_id.clone(),
                project_id: project_id.clone(),
            }],
        );
        let session_plan =
            StoredSessionPlanV1::from_append_request(&session_id, &project_id, 0, None, &request)
                .expect("Session start plan");
        PrepareControlRequest {
            project_allocation: None,
            session_allocation: Some(SessionAllocation {
                session_id,
                project_id,
            }),
            prepared: PreparedTransactionV1::new(
                global_tx_id.to_owned(),
                command,
                None,
                Some(session_plan),
                Vec::new(),
            )
            .expect("Session start Prepared"),
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "测试基线显式区分 Project create 与 Session start 两个权威事务"
    )]
    fn seed_project_session_for(
        runtime: ReadyDurableRuntime,
        project_global_tx_id: &str,
        session_global_tx_id: &str,
        command_prefix: &str,
        project_value: &str,
        session_value: &str,
        fixture_suffix: &str,
    ) -> ReadyDurableRuntime {
        let (runtime, _) = submit_ok(
            runtime,
            project_create_prepare_for(
                project_global_tx_id,
                &format!("command-{command_prefix}-project"),
                project_value,
                fixture_suffix,
            ),
        );
        submit_ok(
            runtime,
            session_start_prepare_for(
                session_global_tx_id,
                &format!("command-{command_prefix}-session"),
                project_value,
                session_value,
            ),
        )
        .0
    }

    fn combined_mutation_prepare_for(
        runtime: &ReadyDurableRuntime,
        global_tx_id: &str,
        command_id: &str,
        project_value: &str,
        session_value: &str,
        fixture_suffix: &str,
    ) -> PrepareControlRequest {
        let command = StoredCommandRecordV1::new(
            "client-runtime",
            command_id,
            format!("sha256:{}", "3".repeat(64)),
            &stable_reply(command_id),
        )
        .expect("combined mutation command");
        let project_id = domain_project(project_value);
        let session_id = SessionId(session_value.to_owned());
        let state_store = &runtime.core.components().expect("components").state_store;
        let project_writer = match state_store
            .open_project_writer(project_id.clone())
            .expect("open Project head")
        {
            OpenProjectWriter::Ready(writer) => writer,
            OpenProjectWriter::RepairRequired(_) => panic!("clean Project head"),
        };
        let (project_sequence, project_checksum) = project_writer.head();
        let project_checksum = project_checksum.map(ToOwned::to_owned);
        drop(project_writer);
        let project_request = AppendRequest {
            transaction_id: project_transaction_id(global_tx_id),
            command_record: Some(command.clone()),
            events: vec![ProjectEvent::BriefRevisionCreated(CreativeBrief {
                id: BriefRevisionId::parse(format!("brief-{fixture_suffix}"))
                    .expect("combined brief"),
                project_id: project_id.clone(),
                previous: None,
                user_description: "combined transaction fixture".to_owned(),
                goals: vec!["验证 Project/Session 原子提交".to_owned()],
                instrumentation: vec!["piano".to_owned()],
                open_questions: Vec::new(),
            })],
        };
        let project_plan = StoredProjectPlanV1::from_append_request(
            &project_id,
            project_sequence,
            project_checksum,
            &project_request,
        )
        .expect("combined Project plan");
        let session_writer = match state_store
            .open_session_writer(session_id.clone())
            .expect("open Session head")
        {
            OpenSessionWriter::Ready(writer) => writer,
            OpenSessionWriter::RepairRequired(_) => panic!("clean Session head"),
        };
        let (session_sequence, session_checksum) = session_writer.head();
        let session_checksum = session_checksum.map(ToOwned::to_owned);
        drop(session_writer);
        let session_request = SessionAppendRequest::new(
            session_transaction_id(global_tx_id),
            Some(command.clone()),
            vec![SessionRolloutEvent::TurnStarted {
                turn_id: TurnId(format!("turn-{fixture_suffix}")),
                canonical_prompt: "combined transaction fixture".to_owned(),
            }],
        );
        let session_plan = StoredSessionPlanV1::from_append_request(
            &session_id,
            &ProjectId(project_value.to_owned()),
            session_sequence,
            session_checksum,
            &session_request,
        )
        .expect("combined Session plan");
        PrepareControlRequest {
            project_allocation: None,
            session_allocation: None,
            prepared: PreparedTransactionV1::new(
                global_tx_id.to_owned(),
                command,
                Some(project_plan),
                Some(session_plan),
                Vec::new(),
            )
            .expect("combined mutation Prepared"),
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "metadata 对抗 fixture 需逐项独立控制 wire、版本、pre-head 与事件"
    )]
    fn metadata_project_prepared(
        global_tx_id: &str,
        command_id: &str,
        plan_project_id: &str,
        wire_project_id: &str,
        reply_protocol_version: u32,
        expected_pre_sequence: u64,
        expected_pre_checksum: Option<String>,
        events: Vec<ProjectEvent>,
        project_created_reply: bool,
    ) -> PreparedTransactionV1 {
        let reply = if project_created_reply {
            CommandReply {
                protocol_version: reply_protocol_version,
                client_command_id: ClientCommandId(command_id.to_owned()),
                outcome: CommandOutcome::Success {
                    result: CommandResult::ProjectCreated(ProtocolProjectSnapshot {
                        project_id: ProjectId(wire_project_id.to_owned()),
                        name: "metadata fixture".to_owned(),
                        version: 1,
                    }),
                },
            }
        } else {
            CommandReply::error(
                ClientCommandId(command_id.to_owned()),
                ProtocolErrorCode::InvalidRequest,
                "orphan initialization",
            )
        };
        let reply = serde_json::to_vec(&reply).expect("metadata reply");
        let command = StoredCommandRecordV1::new(
            "client-metadata",
            command_id,
            format!("sha256:{}", "9".repeat(64)),
            &reply,
        )
        .expect("metadata command");
        let project_id = domain_project(plan_project_id);
        let request = AppendRequest {
            transaction_id: project_transaction_id(global_tx_id),
            command_record: Some(command.clone()),
            events,
        };
        let plan = StoredProjectPlanV1::from_append_request(
            &project_id,
            expected_pre_sequence,
            expected_pre_checksum,
            &request,
        )
        .expect("metadata Project plan");
        PreparedTransactionV1::new(
            global_tx_id.to_owned(),
            command,
            Some(plan),
            None,
            Vec::new(),
        )
        .expect("metadata Prepared")
    }

    fn add_metadata_committed(
        projection: &mut ControlProjection,
        prepared: PreparedTransactionV1,
        resulting_last_sequence: u64,
    ) {
        let global_tx_id = prepared.global_tx_id.clone();
        projection.prepared_order.push(global_tx_id.clone());
        assert!(
            projection
                .prepared
                .insert(global_tx_id.clone(), prepared)
                .is_none()
        );
        assert!(
            projection
                .committed
                .insert(
                    global_tx_id,
                    crate::control_store::CommittedTransactionV1 {
                        project_last: Some(AggregateCommitV1 {
                            resulting_last_sequence,
                            resulting_batch_checksum: format!("sha256:{}", "a".repeat(64)),
                        }),
                        session_last: None,
                    },
                )
                .is_none()
        );
    }

    #[derive(Clone)]
    struct OccurrenceRecipe {
        global_tx_id: String,
        project_id: DomainProjectId,
        session_id: SessionId,
        approval: PendingApproval,
        artifact_record: ArtifactRecord,
        audit_plan: ArtifactAuditPlanV1,
        manifest: ArtifactManifest,
        project_pre_sequence: u64,
        project_pre_checksum: Option<String>,
        session_pre_sequence: u64,
        session_pre_checksum: Option<String>,
    }

    type ManifestMutation = (&'static str, fn(&mut ArtifactManifest));

    impl OccurrenceRecipe {
        fn decided_approval(&self, decision: ApprovalDecision) -> PendingApproval {
            let mut approval = self.approval.clone();
            approval.status = match decision {
                ApprovalDecision::Approve => ApprovalStatus::Approved,
                ApprovalDecision::Deny => ApprovalStatus::Denied,
            };
            approval.terminal_sequence = Some(self.session_pre_sequence + 1);
            approval.decision = Some(decision);
            approval.responder_client_id = Some(ClientId("client-approver".to_owned()));
            approval
        }

        fn resolved_event(&self, decision: ApprovalDecision) -> SessionRolloutEvent {
            SessionRolloutEvent::ApprovalResolved {
                approval_id: self.approval.approval_id.clone(),
                approval_subject_digest: self.approval.approval_subject_digest.clone(),
                decision,
                responder_client_id: ClientId("client-approver".to_owned()),
            }
        }

        fn decision_reply(
            &self,
            command_id: &str,
            protocol_version: u32,
            decision: ApprovalDecision,
            manifest: Option<ArtifactManifest>,
        ) -> CommandReply {
            CommandReply {
                protocol_version,
                client_command_id: ClientCommandId(command_id.to_owned()),
                outcome: CommandOutcome::Success {
                    result: CommandResult::ApprovalDecided {
                        approval: self.decided_approval(decision),
                        artifact_manifest: manifest,
                    },
                },
            }
        }

        fn prepared(
            &self,
            command_id: &str,
            reply: &CommandReply,
            session_events: Vec<SessionRolloutEvent>,
            with_artifact: bool,
        ) -> PreparedTransactionV1 {
            let reply = serde_json::to_vec(reply).expect("occurrence reply");
            let command = StoredCommandRecordV1::new(
                "client-occurrence",
                command_id,
                format!("sha256:{}", "e".repeat(64)),
                &reply,
            )
            .expect("occurrence command");
            let project_plan = with_artifact.then(|| {
                StoredProjectPlanV1::from_append_request(
                    &self.project_id,
                    self.project_pre_sequence,
                    self.project_pre_checksum.clone(),
                    &AppendRequest {
                        transaction_id: project_transaction_id(&self.global_tx_id),
                        command_record: Some(command.clone()),
                        events: vec![ProjectEvent::ArtifactRegistered(
                            self.artifact_record.clone(),
                        )],
                    },
                )
                .expect("occurrence Project plan")
            });
            let session_plan = (!session_events.is_empty()).then(|| {
                StoredSessionPlanV1::from_append_request(
                    &self.session_id,
                    &ProjectId(self.project_id.as_str().to_owned()),
                    self.session_pre_sequence,
                    self.session_pre_checksum.clone(),
                    &SessionAppendRequest::new(
                        session_transaction_id(&self.global_tx_id),
                        Some(command.clone()),
                        session_events,
                    ),
                )
                .expect("occurrence Session plan")
            });
            PreparedTransactionV1::new(
                self.global_tx_id.clone(),
                command,
                project_plan,
                session_plan,
                if with_artifact {
                    vec![self.audit_plan.clone()]
                } else {
                    Vec::new()
                },
            )
            .expect("occurrence Prepared")
        }

        fn approved_prepared(
            &self,
            command_id: &str,
            protocol_version: u32,
            manifest: Option<ArtifactManifest>,
            with_artifact: bool,
        ) -> PreparedTransactionV1 {
            self.prepared(
                command_id,
                &self.decision_reply(
                    command_id,
                    protocol_version,
                    ApprovalDecision::Approve,
                    manifest,
                ),
                vec![self.resolved_event(ApprovalDecision::Approve)],
                with_artifact,
            )
        }
    }

    fn seed_pending_occurrence_approval(
        root: &tempfile::TempDir,
        suffix: &str,
    ) -> ReadyDurableRuntime {
        let project_id = format!("project-occurrence-{suffix}");
        let session_id = format!("session-occurrence-{suffix}");
        let turn_id = TurnId(format!("turn-occurrence-{suffix}"));
        let approval_id = ApprovalId(format!("approval-occurrence-{suffix}"));
        let prompt = "生成确定性 Alda source";
        let subject_inputs =
            ApprovalSubjectInputsV1::canonical("fake-provider-fixture-v1", ["alda_source"])
                .expect("approval subject inputs");
        let fields = subject_inputs
            .egress_field_names
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let digest =
            approval_subject_digest_v1(&subject_inputs.provider_origin, &fields, &turn_id, prompt);
        let runtime = ReadyDurableRuntime::open(root.path()).expect("open occurrence root");
        let runtime = seed_project_session_for(
            runtime,
            &format!("global-{}", "1".repeat(32)),
            &format!("global-{}", "2".repeat(32)),
            &format!("occurrence-{suffix}"),
            &project_id,
            &session_id,
            suffix,
        );
        let request = session_only_prepare(
            &runtime,
            &format!("global-{}", "3".repeat(32)),
            &format!("command-occurrence-pending-{suffix}"),
            &session_id,
            &project_id,
            vec![
                SessionRolloutEvent::TurnStarted {
                    turn_id: turn_id.clone(),
                    canonical_prompt: prompt.to_owned(),
                },
                SessionRolloutEvent::ApprovalRequested {
                    approval_id,
                    session_id: SessionId(session_id.clone()),
                    owner_turn_id: turn_id,
                    payload: ApprovalPayload {
                        action: "生成乐谱".to_owned(),
                        effect: EffectClass::ModelEgress,
                        target: "Alda source".to_owned(),
                        scope: "当前 Turn".to_owned(),
                        estimated_impact: "写入一个本地 Artifact".to_owned(),
                    },
                    subject_inputs,
                    approval_subject_digest: digest,
                },
            ],
        );
        submit_ok(runtime, request).0
    }

    fn occurrence_recipe(
        runtime: &ReadyDurableRuntime,
        global_tx_id: &str,
        occurrence_id: &str,
    ) -> OccurrenceRecipe {
        let project_id = runtime
            .read_view()
            .projects
            .keys()
            .next()
            .map(|value| domain_project(value))
            .expect("occurrence Project");
        let session_id = runtime
            .read_view()
            .sessions
            .keys()
            .next()
            .map(|value| SessionId(value.clone()))
            .expect("occurrence Session");
        let approval = runtime
            .read_view()
            .sessions
            .get(&session_id.0)
            .expect("occurrence Session state")
            .snapshot()
            .approvals
            .first()
            .cloned()
            .expect("pending approval");
        let state_store = &runtime.core.components().expect("components").state_store;
        let project_writer = match state_store
            .open_project_writer(project_id.clone())
            .expect("open occurrence Project")
        {
            OpenProjectWriter::Ready(writer) => writer,
            OpenProjectWriter::RepairRequired(_) => panic!("clean occurrence Project"),
        };
        let (project_pre_sequence, project_pre_checksum) =
            project_writer.head().map_checksum(ToOwned::to_owned);
        drop(project_writer);
        let session_writer = match state_store
            .open_session_writer(session_id.clone())
            .expect("open occurrence Session")
        {
            OpenSessionWriter::Ready(writer) => writer,
            OpenSessionWriter::RepairRequired(_) => panic!("clean occurrence Session"),
        };
        let (session_pre_sequence, session_pre_checksum) = session_writer.head();
        let session_pre_checksum = session_pre_checksum.map(ToOwned::to_owned);
        drop(session_writer);
        let receipt = runtime
            .core
            .components()
            .expect("components")
            .artifact_store
            .put(Cursor::new(DURABLE_FIXTURE_BYTES), None)
            .expect("put durable fixture");
        let audit_plan = receipt
            .recovery_audit_plan(global_tx_id)
            .expect("occurrence audit plan");
        let artifact_record = receipt.into_record().expect("occurrence record");
        let manifest = ArtifactManifest {
            artifact_occurrence_id: ArtifactOccurrenceId(occurrence_id.to_owned()),
            artifact_hash: ProtocolArtifactHash::parse(DURABLE_FIXTURE_HASH)
                .expect("fixture protocol hash"),
            kind: DURABLE_FIXTURE_KIND,
            mime_type: DURABLE_FIXTURE_MIME_TYPE.to_owned(),
            size_bytes: DURABLE_FIXTURE_SIZE_BYTES,
            producer: DURABLE_FIXTURE_PRODUCER,
            project_id: ProjectId(project_id.as_str().to_owned()),
            source_session_id: session_id.clone(),
            source_turn_id: approval.owner_turn_id.clone(),
            fixture_version: DURABLE_FIXTURE_VERSION,
            created_sequence: session_pre_sequence + 1,
            provenance_label: DURABLE_FIXTURE_PROVENANCE_LABEL.to_owned(),
            durability: DURABLE_FIXTURE_DURABILITY,
        };
        OccurrenceRecipe {
            global_tx_id: global_tx_id.to_owned(),
            project_id,
            session_id,
            approval,
            artifact_record,
            audit_plan,
            manifest,
            project_pre_sequence,
            project_pre_checksum,
            session_pre_sequence,
            session_pre_checksum,
        }
    }

    fn replace_occurrence_prepared(
        baseline: &ControlProjection,
        global_tx_id: &str,
        prepared: PreparedTransactionV1,
    ) -> ControlProjection {
        let mut candidate = baseline.clone();
        assert!(
            candidate
                .prepared
                .insert(global_tx_id.to_owned(), prepared)
                .is_some()
        );
        candidate
    }

    fn submit_ok(
        runtime: ReadyDurableRuntime,
        request: PrepareControlRequest,
    ) -> (ReadyDurableRuntime, Vec<u8>) {
        match runtime.submit(request) {
            Ok(value) => value,
            Err(
                SubmitFailure::Rejected { error, .. } | SubmitFailure::Recovering { error, .. },
            ) => {
                panic!("runtime submit must succeed: {error}")
            }
            Err(SubmitFailure::Fatal(runtime)) => {
                panic!("runtime submit must succeed: {}", runtime.error())
            }
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum ClosureAggregate {
        Project,
        Session,
    }

    #[derive(Clone, Copy, Debug)]
    enum ClosureMutation {
        Missing,
        WrongSuffix,
        Digest,
        Sequence,
        Checksum,
    }

    fn prepare_control_direct(
        runtime: &mut ReadyDurableRuntime,
        request: PrepareControlRequest,
    ) -> PreparedTransactionV1 {
        let prepared = request.prepared.clone();
        let writer = runtime.core.take_control().expect("take control writer");
        let Ok((writer, _)) = writer.prepare(request) else {
            panic!("persist control Prepared");
        };
        runtime
            .core
            .put_control(writer)
            .expect("return control writer");
        prepared
    }

    fn commit_control_direct(
        runtime: &mut ReadyDurableRuntime,
        global_tx_id: &str,
        project_last: Option<AggregateCommitV1>,
        session_last: Option<AggregateCommitV1>,
    ) {
        let writer = runtime.core.take_control().expect("take control writer");
        let Ok((writer, _)) = writer.commit(CommitControlRequest {
            global_tx_id: global_tx_id.to_owned(),
            project_last,
            session_last,
        }) else {
            panic!("persist control Committed");
        };
        runtime
            .core
            .put_control(writer)
            .expect("return control writer");
    }

    fn append_project_direct(
        runtime: &ReadyDurableRuntime,
        project_id: DomainProjectId,
        request: AppendRequest,
    ) -> TransactionCommit {
        let transaction_id = request.transaction_id.clone();
        let digest = request
            .canonical_plan_digest(&project_id)
            .expect("Project plan digest");
        let state_store = &runtime
            .core
            .components()
            .expect("runtime components")
            .state_store;
        let writer = match state_store
            .open_project_writer(project_id)
            .expect("open Project writer")
        {
            OpenProjectWriter::Ready(writer) => writer,
            OpenProjectWriter::RepairRequired(_) => panic!("clean Project writer"),
        };
        let writer = match writer.append(request) {
            Ok((writer, _)) => writer,
            Err(AppendFailure::Rejected { error, .. }) => {
                panic!("persist Project transaction {transaction_id}: {error}")
            }
            Err(AppendFailure::Poisoned { error, .. }) => {
                panic!("sync Project transaction {transaction_id}: {error}")
            }
        };
        match writer.probe_transaction(&transaction_id, &digest) {
            TransactionProbe::SamePlanCommitted(commit) => commit,
            TransactionProbe::Absent | TransactionProbe::ConflictingPlan => {
                panic!("Project transaction must be indexed")
            }
        }
    }

    fn append_session_direct(
        runtime: &ReadyDurableRuntime,
        session_id: SessionId,
        request: SessionAppendRequest,
    ) -> TransactionCommit {
        let transaction_id = request.transaction_id.clone();
        let digest = request
            .canonical_plan_digest(&session_id)
            .expect("Session plan digest");
        let state_store = &runtime
            .core
            .components()
            .expect("runtime components")
            .state_store;
        let writer = match state_store
            .open_session_writer(session_id)
            .expect("open Session writer")
        {
            OpenSessionWriter::Ready(writer) => writer,
            OpenSessionWriter::RepairRequired(_) => panic!("clean Session writer"),
        };
        let writer = match writer.append(request) {
            Ok((writer, _)) => writer,
            Err(SessionAppendFailure::Rejected { error, .. }) => {
                panic!("persist Session transaction {transaction_id}: {error}")
            }
            Err(SessionAppendFailure::Poisoned { error, .. }) => {
                panic!("sync Session transaction {transaction_id}: {error}")
            }
        };
        match writer.probe_transaction(&transaction_id, &digest) {
            TransactionProbe::SamePlanCommitted(commit) => commit,
            TransactionProbe::Absent | TransactionProbe::ConflictingPlan => {
                panic!("Session transaction must be indexed")
            }
        }
    }

    fn write_closure_checkpoints(
        runtime: &ReadyDurableRuntime,
        projects: &[&str],
        sessions: &[&str],
    ) {
        runtime
            .core
            .control()
            .expect("control writer")
            .write_checkpoint()
            .expect("control checkpoint");
        let state_store = &runtime
            .core
            .components()
            .expect("runtime components")
            .state_store;
        for project_id in projects {
            let writer = match state_store
                .open_project_writer(domain_project(project_id))
                .expect("open Project checkpoint writer")
            {
                OpenProjectWriter::Ready(writer) => writer,
                OpenProjectWriter::RepairRequired(_) => panic!("clean Project checkpoint"),
            };
            writer.write_checkpoint().expect("Project checkpoint");
        }
        for session_id in sessions {
            let writer = match state_store
                .open_session_writer(SessionId((*session_id).to_owned()))
                .expect("open Session checkpoint writer")
            {
                OpenSessionWriter::Ready(writer) => writer,
                OpenSessionWriter::RepairRequired(_) => panic!("clean Session checkpoint"),
            };
            writer.write_checkpoint().expect("Session checkpoint");
        }
    }

    fn assert_runtime_transaction_conflict(root: &Path) {
        assert!(matches!(
            ReadyDurableRuntime::open(root),
            Err(DurableRuntimeError::TransactionConflict)
        ));
    }

    fn mismatched_command() -> StoredCommandRecordV1 {
        let command_id = "command-closure-digest-mismatch";
        StoredCommandRecordV1::new(
            "client-closure-mismatch",
            command_id,
            format!("sha256:{}", "d".repeat(64)),
            &stable_reply(command_id),
        )
        .expect("mismatched command")
    }

    fn mutate_anchor(anchor: &mut AggregateCommitV1, mutation: ClosureMutation) {
        match mutation {
            ClosureMutation::Sequence => {
                anchor.resulting_last_sequence = anchor
                    .resulting_last_sequence
                    .checked_add(1)
                    .expect("test sequence");
            }
            ClosureMutation::Checksum => {
                anchor.resulting_batch_checksum = format!("sha256:{}", "e".repeat(64));
            }
            ClosureMutation::Missing | ClosureMutation::WrongSuffix | ClosureMutation::Digest => {}
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "单个独立根必须完整构造 control、aggregate、checkpoint 与 reopen 边界"
    )]
    fn persist_closure_mutation(
        aggregate: ClosureAggregate,
        mutation: ClosureMutation,
        with_checkpoint: bool,
    ) {
        const CLOSURE_TX: &str = "global-88888888888888888888888888888888";
        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("open closure root");
        let mut runtime = seed_project_session_for(
            runtime,
            &format!("global-{}", "1".repeat(32)),
            &format!("global-{}", "3".repeat(32)),
            "closure-base",
            "project-runtime",
            "session-runtime",
            "closure",
        );
        let control_request = match aggregate {
            ClosureAggregate::Project => {
                let project_id = domain_project("project-runtime");
                project_only_prepare(
                    &runtime,
                    CLOSURE_TX,
                    "command-closure-project",
                    project_id.as_str(),
                    vec![ProjectEvent::BriefRevisionCreated(CreativeBrief {
                        id: BriefRevisionId::parse("brief-closure-project").expect("brief"),
                        project_id: project_id.clone(),
                        previous: None,
                        user_description: "closure Project transaction".to_owned(),
                        goals: vec!["验证 closure".to_owned()],
                        instrumentation: vec!["piano".to_owned()],
                        open_questions: Vec::new(),
                    })],
                )
            }
            ClosureAggregate::Session => session_only_prepare(
                &runtime,
                CLOSURE_TX,
                "command-closure-session",
                "session-runtime",
                "project-runtime",
                vec![
                    SessionRolloutEvent::TurnStarted {
                        turn_id: TurnId("turn-closure".to_owned()),
                        canonical_prompt: "closure Session transaction".to_owned(),
                    },
                    SessionRolloutEvent::TurnCancelRequested {
                        turn_id: TurnId("turn-closure".to_owned()),
                    },
                    SessionRolloutEvent::TurnCompleted {
                        turn_id: TurnId("turn-closure".to_owned()),
                        status: TurnStatus::Cancelled,
                    },
                ],
            ),
        };
        let prepared = prepare_control_direct(&mut runtime, control_request);
        let missing_anchor = AggregateCommitV1 {
            resulting_last_sequence: match aggregate {
                ClosureAggregate::Project => 2,
                ClosureAggregate::Session => 4,
            },
            resulting_batch_checksum: format!("sha256:{}", "a".repeat(64)),
        };
        let mut anchor = match aggregate {
            ClosureAggregate::Project => {
                let mut request = prepared
                    .project_plan
                    .clone()
                    .expect("Project plan")
                    .into_append_request(Vec::new())
                    .expect("Project append request");
                match mutation {
                    ClosureMutation::Missing => missing_anchor,
                    ClosureMutation::WrongSuffix => {
                        request.transaction_id = session_transaction_id(CLOSURE_TX);
                        AggregateCommitV1::from(append_project_direct(
                            &runtime,
                            domain_project("project-runtime"),
                            request,
                        ))
                    }
                    ClosureMutation::Digest => {
                        request.command_record = Some(mismatched_command());
                        AggregateCommitV1::from(append_project_direct(
                            &runtime,
                            domain_project("project-runtime"),
                            request,
                        ))
                    }
                    ClosureMutation::Sequence | ClosureMutation::Checksum => {
                        AggregateCommitV1::from(append_project_direct(
                            &runtime,
                            domain_project("project-runtime"),
                            request,
                        ))
                    }
                }
            }
            ClosureAggregate::Session => {
                let mut request = prepared
                    .session_plan
                    .clone()
                    .expect("Session plan")
                    .into_append_request()
                    .expect("Session append request");
                match mutation {
                    ClosureMutation::Missing => missing_anchor,
                    ClosureMutation::WrongSuffix => {
                        request.transaction_id = project_transaction_id(CLOSURE_TX);
                        AggregateCommitV1::from(append_session_direct(
                            &runtime,
                            SessionId("session-runtime".to_owned()),
                            request,
                        ))
                    }
                    ClosureMutation::Digest => {
                        request.command_record = Some(mismatched_command());
                        AggregateCommitV1::from(append_session_direct(
                            &runtime,
                            SessionId("session-runtime".to_owned()),
                            request,
                        ))
                    }
                    ClosureMutation::Sequence | ClosureMutation::Checksum => {
                        AggregateCommitV1::from(append_session_direct(
                            &runtime,
                            SessionId("session-runtime".to_owned()),
                            request,
                        ))
                    }
                }
            }
        };
        mutate_anchor(&mut anchor, mutation);
        let (project_last, session_last) = match aggregate {
            ClosureAggregate::Project => (Some(anchor), None),
            ClosureAggregate::Session => (None, Some(anchor)),
        };
        commit_control_direct(
            &mut runtime,
            &prepared.global_tx_id,
            project_last,
            session_last,
        );
        if with_checkpoint {
            write_closure_checkpoints(&runtime, &["project-runtime"], &["session-runtime"]);
        }
        drop(runtime);
        match ReadyDurableRuntime::open(root.path()) {
            Err(DurableRuntimeError::TransactionConflict) => {}
            Err(error) => panic!(
                "{aggregate:?} {mutation:?} checkpoint={with_checkpoint} 返回了错误类型：{error}"
            ),
            Ok(_) => {
                panic!("{aggregate:?} {mutation:?} checkpoint={with_checkpoint} 不得发布 read view")
            }
        }
    }

    fn project_only_prepare(
        runtime: &ReadyDurableRuntime,
        global_tx_id: &str,
        command_id: &str,
        project_id: &str,
        events: Vec<ProjectEvent>,
    ) -> PrepareControlRequest {
        let command = StoredCommandRecordV1::new(
            "client-runtime",
            command_id,
            format!("sha256:{}", "6".repeat(64)),
            &stable_reply(command_id),
        )
        .expect("Project command");
        let project_id = domain_project(project_id);
        let state_store = &runtime
            .core
            .components()
            .expect("runtime components")
            .state_store;
        let writer = match state_store
            .open_project_writer(project_id.clone())
            .expect("open Project head")
        {
            OpenProjectWriter::Ready(writer) => writer,
            OpenProjectWriter::RepairRequired(_) => panic!("clean Project head"),
        };
        let (pre_sequence, pre_checksum) = writer.head().map_checksum(ToOwned::to_owned);
        drop(writer);
        let request = AppendRequest {
            transaction_id: project_transaction_id(global_tx_id),
            command_record: Some(command.clone()),
            events,
        };
        let plan = StoredProjectPlanV1::from_append_request(
            &project_id,
            pre_sequence,
            pre_checksum,
            &request,
        )
        .expect("Project plan");
        let prepared = PreparedTransactionV1::new(
            global_tx_id.to_owned(),
            command,
            Some(plan),
            None,
            Vec::new(),
        )
        .expect("Project Prepared");
        PrepareControlRequest {
            project_allocation: None,
            session_allocation: None,
            prepared,
        }
    }

    fn session_only_prepare(
        runtime: &ReadyDurableRuntime,
        global_tx_id: &str,
        command_id: &str,
        session_id: &str,
        project_id: &str,
        events: Vec<SessionRolloutEvent>,
    ) -> PrepareControlRequest {
        let command = StoredCommandRecordV1::new(
            "client-runtime",
            command_id,
            format!("sha256:{}", "7".repeat(64)),
            &stable_reply(command_id),
        )
        .expect("Session command");
        let session_id = SessionId(session_id.to_owned());
        let state_store = &runtime
            .core
            .components()
            .expect("runtime components")
            .state_store;
        let writer = match state_store
            .open_session_writer(session_id.clone())
            .expect("open Session head")
        {
            OpenSessionWriter::Ready(writer) => writer,
            OpenSessionWriter::RepairRequired(_) => panic!("clean Session head"),
        };
        let (pre_sequence, pre_checksum) = writer.head();
        let pre_checksum = pre_checksum.map(ToOwned::to_owned);
        drop(writer);
        let request = SessionAppendRequest::new(
            session_transaction_id(global_tx_id),
            Some(command.clone()),
            events,
        );
        let plan = StoredSessionPlanV1::from_append_request(
            &session_id,
            &ProjectId(project_id.to_owned()),
            pre_sequence,
            pre_checksum,
            &request,
        )
        .expect("Session plan");
        let prepared = PreparedTransactionV1::new(
            global_tx_id.to_owned(),
            command,
            None,
            Some(plan),
            Vec::new(),
        )
        .expect("Session Prepared");
        PrepareControlRequest {
            project_allocation: None,
            session_allocation: None,
            prepared,
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "同类型 owner 负例必须在一个独立根内保留两组完整 catalog 与 transaction"
    )]
    fn persist_wrong_owner(aggregate: ClosureAggregate, with_checkpoint: bool) {
        const OWNER_TX: &str = "global-55555555555555555555555555555555";
        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("open owner root");
        let runtime = seed_project_session_for(
            runtime,
            &format!("global-{}", "a".repeat(32)),
            &format!("global-{}", "b".repeat(32)),
            "owner-primary",
            "project-owner-primary",
            "session-owner-primary",
            "owner-primary",
        );
        let mut runtime = seed_project_session_for(
            runtime,
            &format!("global-{}", "c".repeat(32)),
            &format!("global-{}", "d".repeat(32)),
            "owner-secondary",
            "project-owner-secondary",
            "session-owner-secondary",
            "owner-secondary",
        );

        match aggregate {
            ClosureAggregate::Project => {
                let primary_id = domain_project("project-owner-primary");
                let control_request = project_only_prepare(
                    &runtime,
                    OWNER_TX,
                    "command-owner-project",
                    primary_id.as_str(),
                    vec![ProjectEvent::BriefRevisionCreated(CreativeBrief {
                        id: BriefRevisionId::parse("brief-owner-primary").expect("brief"),
                        project_id: primary_id.clone(),
                        previous: None,
                        user_description: "control 归属 primary Project".to_owned(),
                        goals: vec!["验证 owner".to_owned()],
                        instrumentation: vec!["piano".to_owned()],
                        open_questions: Vec::new(),
                    })],
                );
                let prepared = prepare_control_direct(&mut runtime, control_request);
                let command = prepared.command_record.clone();
                let secondary_id = domain_project("project-owner-secondary");
                let commit = append_project_direct(
                    &runtime,
                    secondary_id.clone(),
                    AppendRequest {
                        transaction_id: project_transaction_id(OWNER_TX),
                        command_record: Some(command),
                        events: vec![ProjectEvent::BriefRevisionCreated(CreativeBrief {
                            id: BriefRevisionId::parse("brief-owner-secondary").expect("brief"),
                            project_id: secondary_id,
                            previous: None,
                            user_description: "同 suffix transaction 落入 secondary Project"
                                .to_owned(),
                            goals: vec!["验证 owner".to_owned()],
                            instrumentation: vec!["piano".to_owned()],
                            open_questions: Vec::new(),
                        })],
                    },
                );
                commit_control_direct(
                    &mut runtime,
                    OWNER_TX,
                    Some(AggregateCommitV1::from(commit)),
                    None,
                );
            }
            ClosureAggregate::Session => {
                let events = vec![
                    SessionRolloutEvent::TurnStarted {
                        turn_id: TurnId("turn-owner".to_owned()),
                        canonical_prompt: "验证同类型 owner".to_owned(),
                    },
                    SessionRolloutEvent::TurnCancelRequested {
                        turn_id: TurnId("turn-owner".to_owned()),
                    },
                    SessionRolloutEvent::TurnCompleted {
                        turn_id: TurnId("turn-owner".to_owned()),
                        status: TurnStatus::Cancelled,
                    },
                ];
                let control_request = session_only_prepare(
                    &runtime,
                    OWNER_TX,
                    "command-owner-session",
                    "session-owner-primary",
                    "project-owner-primary",
                    events.clone(),
                );
                let prepared = prepare_control_direct(&mut runtime, control_request);
                let commit = append_session_direct(
                    &runtime,
                    SessionId("session-owner-secondary".to_owned()),
                    SessionAppendRequest::new(
                        session_transaction_id(OWNER_TX),
                        Some(prepared.command_record.clone()),
                        events,
                    ),
                );
                commit_control_direct(
                    &mut runtime,
                    OWNER_TX,
                    None,
                    Some(AggregateCommitV1::from(commit)),
                );
            }
        }
        if with_checkpoint {
            write_closure_checkpoints(
                &runtime,
                &["project-owner-primary", "project-owner-secondary"],
                &["session-owner-primary", "session-owner-secondary"],
            );
        }
        drop(runtime);
        match ReadyDurableRuntime::open(root.path()) {
            Err(DurableRuntimeError::TransactionConflict) => {}
            Err(error) => {
                panic!("{aggregate:?} owner checkpoint={with_checkpoint} 返回了错误类型：{error}")
            }
            Ok(_) => panic!("{aggregate:?} owner checkpoint={with_checkpoint} 不得发布 read view"),
        }
    }

    fn running_legacy_runtime() -> (tempfile::TempDir, ReadyDurableRuntime) {
        const RUNNING_TX: &str = "global-66666666666666666666666666666666";
        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("open legacy root");
        let runtime = seed_project_session_for(
            runtime,
            &format!("global-{}", "1".repeat(32)),
            &format!("global-{}", "2".repeat(32)),
            "legacy-base",
            "project-runtime",
            "session-runtime",
            "legacy",
        );
        let request = session_only_prepare(
            &runtime,
            RUNNING_TX,
            "command-legacy-running",
            "session-runtime",
            "project-runtime",
            vec![SessionRolloutEvent::TurnStarted {
                turn_id: TurnId("turn-legacy".to_owned()),
                canonical_prompt: "等待 legacy restart".to_owned(),
            }],
        );
        let (runtime, _) = submit_ok(runtime, request);
        (root, runtime)
    }

    fn multiple_running_sessions(count: usize) -> tempfile::TempDir {
        let root = private_root();
        let mut runtime = ReadyDurableRuntime::open(root.path()).expect("open multi Session root");
        for index in 0..count {
            let project_id = format!("project-restart-{index}");
            let session_id = format!("session-restart-{index}");
            let create_tx = format!("global-{:032x}", index * 3 + 1);
            let session_tx = format!("global-{:032x}", index * 3 + 2);
            let run_tx = format!("global-{:032x}", index * 3 + 3);
            let create_command = format!("command-create-{index}");
            let run_command = format!("command-run-{index}");
            runtime = seed_project_session_for(
                runtime,
                &create_tx,
                &session_tx,
                &create_command,
                &project_id,
                &session_id,
                &format!("restart-{index}"),
            );
            let request = session_only_prepare(
                &runtime,
                &run_tx,
                &run_command,
                &session_id,
                &project_id,
                vec![SessionRolloutEvent::TurnStarted {
                    turn_id: TurnId(format!("turn-restart-{index}")),
                    canonical_prompt: "等待协调重启".to_owned(),
                }],
            );
            (runtime, _) = submit_ok(runtime, request);
        }
        drop(runtime);
        root
    }

    fn startup_capacity_external_request(
        writer: &ReadySessionWriter,
        index: usize,
    ) -> PrepareControlRequest {
        let global_tx_id = format!("global-c{index:031x}");
        let command_id = format!("command-startup-capacity-{index}");
        let command = StoredCommandRecordV1::new(
            "client-startup-capacity",
            &command_id,
            format!("sha256:{}", "c".repeat(64)),
            &stable_reply(&command_id),
        )
        .expect("startup capacity command");
        let (pre_sequence, pre_checksum) = writer.head();
        let request = SessionAppendRequest::new(
            session_transaction_id(&global_tx_id),
            Some(command.clone()),
            vec![SessionRolloutEvent::TurnCompleted {
                turn_id: TurnId("turn-restart-0".to_owned()),
                status: TurnStatus::AbortedByRestart,
            }],
        );
        let plan = StoredSessionPlanV1::from_append_request(
            &SessionId("session-restart-0".to_owned()),
            &ProjectId("project-restart-0".to_owned()),
            pre_sequence,
            pre_checksum.map(ToOwned::to_owned),
            &request,
        )
        .expect("startup capacity Session plan");
        PrepareControlRequest {
            project_allocation: None,
            session_allocation: None,
            prepared: PreparedTransactionV1::new(
                global_tx_id,
                command,
                None,
                Some(plan),
                Vec::new(),
            )
            .expect("startup external Prepared"),
        }
    }

    fn startup_capacity_internal_request(
        writer: &ReadySessionWriter,
        index: usize,
    ) -> PrepareControlRequest {
        let instance_id = format!("{index:032x}");
        let coordinated =
            plan_coordinated_restart_reconciliation(&instance_id, writer.projection())
                .expect("startup capacity restart planner")
                .expect("startup capacity restart obligation");
        let (pre_sequence, pre_checksum) = writer.head();
        let plan = StoredSessionPlanV1::from_append_request(
            &SessionId("session-restart-0".to_owned()),
            &ProjectId("project-restart-0".to_owned()),
            pre_sequence,
            pre_checksum.map(ToOwned::to_owned),
            &coordinated.append_request(),
        )
        .expect("startup internal Session plan");
        PrepareControlRequest {
            project_allocation: None,
            session_allocation: None,
            prepared: PreparedTransactionV1::new(
                coordinated.global_tx_id,
                coordinated.command_record,
                None,
                Some(plan),
                Vec::new(),
            )
            .expect("startup internal Prepared"),
        }
    }

    fn extend_real_startup_capacity(root: &Path, external_target: usize, internal_target: usize) {
        let state_store = StateStore::open(root, StateStoreInstanceLease::for_tests())
            .expect("open startup capacity StateStore");
        let session_writer = match state_store
            .open_session_writer(SessionId("session-restart-0".to_owned()))
            .expect("open startup capacity Session")
        {
            OpenSessionWriter::Ready(writer) => writer,
            OpenSessionWriter::RepairRequired(_) => {
                panic!("startup capacity Session 不应需要修复")
            }
        };
        let health = Arc::new(LockHealth::new());
        let writer = match open_control_writer(root, Arc::downgrade(&health))
            .expect("open startup capacity control")
        {
            OpenControlWriter::Ready(writer) => writer,
            OpenControlWriter::RepairRequired(_) => {
                panic!("startup capacity control 不应需要修复")
            }
        };
        let initial = writer.projection().capacity();
        assert!(initial.external <= external_target);
        assert!(initial.internal_restart <= internal_target);
        let writer = append_validated_control_fixture(
            writer,
            (initial.external..external_target)
                .map(|index| startup_capacity_external_request(&session_writer, index)),
            true,
        )
        .expect("append real external startup capacity");
        let writer = append_validated_control_fixture(
            writer,
            (initial.internal_restart..internal_target)
                .map(|index| startup_capacity_internal_request(&session_writer, index)),
            true,
        )
        .expect("append real internal startup capacity");
        assert_eq!(
            writer.projection().capacity(),
            ControlCapacity {
                external: external_target,
                internal_restart: internal_target,
                total: external_target + internal_target,
            }
        );
    }

    #[derive(Debug, Eq, PartialEq)]
    struct StartupPersistenceSnapshot {
        control_capacity: ControlCapacity,
        prepared_count: usize,
        committed_count: usize,
        control_log: Vec<u8>,
        session_logs: BTreeMap<String, Vec<u8>>,
        session_heads: BTreeMap<String, (u64, Option<String>)>,
    }

    fn startup_persistence_snapshot(root: &Path) -> StartupPersistenceSnapshot {
        let health = Arc::new(LockHealth::new());
        let writer = match open_control_writer(root, Arc::downgrade(&health))
            .expect("读取启动前后 control")
        {
            OpenControlWriter::Ready(writer) => writer,
            OpenControlWriter::RepairRequired(_) => panic!("control 不应存在不完整尾部"),
        };
        let projection = writer.projection();
        let control_capacity = projection.capacity();
        let prepared_count = projection.prepared.len();
        let committed_count = projection.committed.len();
        drop(writer);

        let control_log = std::fs::read(
            root.join("state-v1")
                .join("control")
                .join("control-v1.jsonl"),
        )
        .expect("读取 control log");
        let session_root = root.join("state-v1").join("sessions");
        let session_logs = std::fs::read_dir(session_root)
            .expect("读取 Session 目录")
            .map(|entry| {
                let entry = entry.expect("读取 Session 条目");
                let key = entry.file_name().to_string_lossy().into_owned();
                let bytes = std::fs::read(entry.path().join("rollout-v1.jsonl"))
                    .expect("读取 Session rollout");
                (key, bytes)
            })
            .collect();
        let state_store = StateStore::open(root, StateStoreInstanceLease::for_tests())
            .expect("读取启动前后 Session head");
        let session_heads = state_store
            .list_sessions()
            .expect("枚举启动前后 Session")
            .sessions
            .keys()
            .map(|session_id| {
                let writer = match state_store
                    .open_session_writer(SessionId(session_id.clone()))
                    .expect("读取 Session head")
                {
                    OpenSessionWriter::Ready(writer) => writer,
                    OpenSessionWriter::RepairRequired(_) => {
                        panic!("Session head 不应需要修复")
                    }
                };
                let head = writer.head().map_checksum(ToOwned::to_owned);
                (session_id.clone(), head)
            })
            .collect();
        StartupPersistenceSnapshot {
            control_capacity,
            prepared_count,
            committed_count,
            control_log,
            session_logs,
            session_heads,
        }
    }

    fn append_trusted_legacy(runtime: &ReadyDurableRuntime) -> String {
        let state_store = &runtime
            .core
            .components()
            .expect("runtime components")
            .state_store;
        let writer = match state_store
            .open_session_writer(SessionId("session-runtime".to_owned()))
            .expect("open legacy Session")
        {
            OpenSessionWriter::Ready(writer) => writer,
            OpenSessionWriter::RepairRequired(_) => panic!("clean legacy Session"),
        };
        let plan = plan_restart_reconciliation(state_store.instance_id(), writer.projection())
            .expect("trusted legacy planner")
            .expect("legacy restart obligation");
        let transaction_id = plan.transaction_id.clone();
        let request = plan.into_append_request();
        drop(writer);
        let _commit =
            append_session_direct(runtime, SessionId("session-runtime".to_owned()), request);
        transaction_id
    }

    fn only_session_log(root: &Path) -> std::path::PathBuf {
        let sessions = root.join("state-v1").join("sessions");
        let mut entries = std::fs::read_dir(sessions).expect("read Session layout");
        let session = entries
            .next()
            .expect("one Session directory")
            .expect("Session directory");
        assert!(entries.next().is_none());
        session.path().join("rollout-v1.jsonl")
    }

    fn entropy_bytes(value: u8) -> [u8; 16] {
        [value; 16]
    }

    fn candidate(kind: DomainIdKind, value: u8) -> String {
        format!(
            "{}-{}",
            kind.prefix(),
            lowercase_hex_128(entropy_bytes(value))
        )
    }

    fn empty_read_view() -> DurableReadView {
        DurableReadView {
            projects: BTreeMap::new(),
            sessions: BTreeMap::new(),
            owners: OwnerIndex::default(),
            project_metadata: BTreeMap::new(),
            occurrence_metadata: BTreeMap::new(),
            reachable_artifact_hashes: BTreeSet::new(),
            generation: 1,
        }
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "九类索引必须在同一矩阵中逐项证明，避免漏掉任一碰撞域"
    )]
    fn typed_random_id_checks_all_nine_indexes_and_local_reservations() {
        let mut control = ControlProjection::default();
        let mut published = empty_read_view();
        let collision_entropy = entropy_bytes(0x11);
        let success_entropy = entropy_bytes(0x22);

        control
            .projects
            .insert(candidate(DomainIdKind::Project, 0x11));
        control.sessions.insert(
            candidate(DomainIdKind::Session, 0x11),
            candidate(DomainIdKind::Project, 0x11),
        );

        let score_id = ScoreId::parse(candidate(DomainIdKind::Score, 0x11)).expect("Score ID");
        let take_id = TakeId::parse(candidate(DomainIdKind::Take, 0x11)).expect("Take ID");
        let branch_id = BranchId::parse(candidate(DomainIdKind::Branch, 0x11)).expect("Branch ID");
        let mut snapshot = ProjectSnapshot {
            score_id: Some(score_id.clone()),
            ..ProjectSnapshot::default()
        };
        snapshot.takes.insert(
            take_id.clone(),
            crate::state::TakeProjection {
                score_id: score_id.clone(),
                common_base: None,
                branches: BTreeSet::from([branch_id.clone()]),
            },
        );
        snapshot.branches.insert(
            branch_id,
            crate::state::BranchProjection {
                score_id,
                take_id,
                fork_base: None,
                head: None,
            },
        );
        published.projects.insert(
            "project-index-owner".to_owned(),
            ProjectReadState {
                snapshot,
                last_sequence: 0,
                last_checksum: format!("sha256:{}", "0".repeat(64)),
            },
        );
        published.owners.turns.insert(
            candidate(DomainIdKind::Turn, 0x11),
            "session-index-owner".to_owned(),
        );
        published.owners.questions.insert(
            candidate(DomainIdKind::Question, 0x11),
            "session-index-owner".to_owned(),
        );
        published.owners.approvals.insert(
            candidate(DomainIdKind::Approval, 0x11),
            "session-index-owner".to_owned(),
        );
        published.occurrence_metadata.insert(
            candidate(DomainIdKind::Occurrence, 0x11),
            ArtifactManifest {
                artifact_occurrence_id: ArtifactOccurrenceId("occurrence-index-owner".to_owned()),
                artifact_hash: ProtocolArtifactHash::parse(DURABLE_FIXTURE_HASH)
                    .expect("fixture hash"),
                kind: DURABLE_FIXTURE_KIND,
                mime_type: DURABLE_FIXTURE_MIME_TYPE.to_owned(),
                size_bytes: DURABLE_FIXTURE_SIZE_BYTES,
                producer: DURABLE_FIXTURE_PRODUCER,
                project_id: ProjectId("project-index-owner".to_owned()),
                source_session_id: SessionId("session-index-owner".to_owned()),
                source_turn_id: TurnId("turn-index-owner".to_owned()),
                fixture_version: DURABLE_FIXTURE_VERSION,
                created_sequence: 1,
                provenance_label: DURABLE_FIXTURE_PROVENANCE_LABEL.to_owned(),
                durability: DURABLE_FIXTURE_DURABILITY,
            },
        );

        let kinds = [
            DomainIdKind::Project,
            DomainIdKind::Score,
            DomainIdKind::Take,
            DomainIdKind::Branch,
            DomainIdKind::Session,
            DomainIdKind::Turn,
            DomainIdKind::Question,
            DomainIdKind::Approval,
            DomainIdKind::Occurrence,
        ];
        for kind in kinds {
            let mut entropy = VecDeque::from([collision_entropy, success_entropy]);
            let allocated =
                allocate_domain_id_with(&control, &published, kind, &BTreeSet::new(), || {
                    entropy.pop_front().ok_or(())
                })
                .expect("索引碰撞后应换用下一候选");
            assert_eq!(allocated.as_str(), candidate(kind, 0x22));
        }

        let reserved_candidate = candidate(DomainIdKind::Project, 0x33);
        let reserved = BTreeSet::from([reserved_candidate]);
        let mut entropy = VecDeque::from([entropy_bytes(0x33), entropy_bytes(0x44)]);
        let allocated = allocate_domain_id_with(
            &control,
            &published,
            DomainIdKind::Project,
            &reserved,
            || entropy.pop_front().ok_or(()),
        )
        .expect("局部 reservation 碰撞后应换用下一候选");
        assert_eq!(allocated.as_str(), candidate(DomainIdKind::Project, 0x44));
    }

    #[test]
    fn typed_random_id_allows_attempt_32_and_reports_entropy_or_exhaustion() {
        let control = ControlProjection::default();
        let published = empty_read_view();
        let collision = candidate(DomainIdKind::Turn, 0x55);
        let reserved = BTreeSet::from([collision]);
        let mut attempts = 0_usize;
        let allocated =
            allocate_domain_id_with(&control, &published, DomainIdKind::Turn, &reserved, || {
                attempts += 1;
                Ok(if attempts == ID_ALLOCATION_ATTEMPTS {
                    entropy_bytes(0x66)
                } else {
                    entropy_bytes(0x55)
                })
            })
            .expect("第 32 次候选应成功");
        assert_eq!(attempts, ID_ALLOCATION_ATTEMPTS);
        assert_eq!(allocated.as_str(), candidate(DomainIdKind::Turn, 0x66));

        assert_eq!(
            allocate_domain_id_with(&control, &published, DomainIdKind::Turn, &reserved, || Ok(
                entropy_bytes(0x55)
            ),),
            Err(DomainIdAllocationError::Exhausted)
        );
        assert_eq!(
            allocate_domain_id_with(
                &control,
                &published,
                DomainIdKind::Turn,
                &BTreeSet::new(),
                || Err(()),
            ),
            Err(DomainIdAllocationError::EntropyUnavailable)
        );
        assert_eq!(
            DomainIdAllocationError::Exhausted.protocol_code(),
            ProtocolErrorCode::ServiceUnavailable
        );

        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("open typed ID runtime");
        for kind in [
            DomainIdKind::Project,
            DomainIdKind::Score,
            DomainIdKind::Take,
            DomainIdKind::Branch,
            DomainIdKind::Session,
            DomainIdKind::Turn,
            DomainIdKind::Question,
            DomainIdKind::Approval,
            DomainIdKind::Occurrence,
        ] {
            let id = runtime
                .allocate_id(kind, &BTreeSet::new())
                .expect("OS CSPRNG typed ID");
            assert!(is_prefixed_hex_128(id.as_str(), kind.prefix()));
        }
    }

    #[test]
    fn durable_project_create_id_collision_exhaustion_maps_to_service_unavailable() {
        let control = ControlProjection::default();
        let published = empty_read_view();
        let collision = candidate(DomainIdKind::Project, 0x77);
        let reserved = BTreeSet::from([collision]);
        let mut attempts = 0_usize;
        let result = allocate_domain_id_with(
            &control,
            &published,
            DomainIdKind::Project,
            &reserved,
            || {
                attempts += 1;
                Ok(entropy_bytes(0x77))
            },
        );

        assert_eq!(attempts, ID_ALLOCATION_ATTEMPTS);
        assert_eq!(result, Err(DomainIdAllocationError::Exhausted));
        assert_eq!(
            result.expect_err("32 次碰撞必须耗尽").protocol_code(),
            ProtocolErrorCode::ServiceUnavailable
        );
    }

    #[test]
    fn durable_session_start_id_allocation_checks_catalog_and_owner_collisions() {
        let mut published = empty_read_view();
        let owner_collision = candidate(DomainIdKind::Session, 0x44);
        let catalog_collision = candidate(DomainIdKind::Session, 0x55);
        published
            .owners
            .turns
            .insert(owner_collision.clone(), "session-owner".to_owned());
        let mut control = ControlProjection::default();
        control
            .sessions
            .insert(catalog_collision.clone(), "project-catalog".to_owned());
        let mut entropy = VecDeque::from([
            entropy_bytes(0x44),
            entropy_bytes(0x55),
            entropy_bytes(0x66),
        ]);

        let allocated = allocate_domain_id_with(
            &control,
            &published,
            DomainIdKind::Session,
            &BTreeSet::new(),
            || entropy.pop_front().ok_or(()),
        )
        .expect("Session ID 必须跳过 owner 与 catalog 碰撞");
        assert_eq!(allocated.as_str(), candidate(DomainIdKind::Session, 0x66));

        let exhausted = allocate_domain_id_with(
            &control,
            &published,
            DomainIdKind::Session,
            &BTreeSet::from([owner_collision]),
            || Ok(entropy_bytes(0x55)),
        );
        assert_eq!(exhausted, Err(DomainIdAllocationError::Exhausted));
        assert_eq!(
            exhausted
                .expect_err("32 次 Session catalog 碰撞必须耗尽")
                .protocol_code(),
            ProtocolErrorCode::ServiceUnavailable
        );
    }

    #[test]
    fn durable_backend_capacity_external_preflight_preserves_internal_slot() {
        let root = private_root();
        let runtime = ReadyDurableRuntime::open_with_startup_test_capacity(
            root.path(),
            ControlCapacity {
                external: MAX_EXTERNAL_PREPARED - 1,
                internal_restart: MAX_INTERNAL_RESTART_PREPARED,
                total: MAX_TOTAL_PREPARED - 1,
            },
        )
        .expect("open last external slot runtime");
        assert_eq!(runtime.require_external_capacity(1), Ok(()));
        drop(runtime);

        for capacity in [
            ControlCapacity {
                external: MAX_EXTERNAL_PREPARED,
                internal_restart: 0,
                total: MAX_EXTERNAL_PREPARED,
            },
            ControlCapacity {
                external: MAX_EXTERNAL_PREPARED - 1,
                internal_restart: MAX_INTERNAL_RESTART_PREPARED,
                total: MAX_TOTAL_PREPARED,
            },
        ] {
            let runtime =
                ReadyDurableRuntime::open_with_startup_test_capacity(root.path(), capacity)
                    .expect("open full capacity runtime");
            let mut domain_allocations = 0_usize;
            let mut global_allocations = 0_usize;
            let mut artifact_puts = 0_usize;
            if runtime.require_external_capacity(1).is_ok() {
                domain_allocations += 1;
                global_allocations += 1;
                artifact_puts += 1;
            }
            assert_eq!(
                runtime.require_external_capacity(1),
                Err(ExternalCapacityError)
            );
            assert_eq!(
                (domain_allocations, global_allocations, artifact_puts),
                (0, 0, 0)
            );
            drop(runtime);
        }
        let runtime = ReadyDurableRuntime::open_with_startup_test_capacity(
            root.path(),
            ControlCapacity::default(),
        )
        .expect("open overflow capacity runtime");
        assert_eq!(
            runtime.require_external_capacity(usize::MAX),
            Err(ExternalCapacityError)
        );
        assert_eq!(
            ExternalCapacityError.protocol_code(),
            ProtocolErrorCode::ServiceUnavailable
        );
    }

    #[test]
    fn external_global_tx_id_is_validated_bounded_and_collision_checked() {
        let collision = format!("global-{}", lowercase_hex_128(entropy_bytes(0x77)));
        let prepared = BTreeSet::from([collision]);
        let mut attempts = 0_usize;
        let allocated = allocate_global_transaction_id_with(
            |candidate| prepared.contains(candidate),
            &BTreeSet::new(),
            || {
                attempts += 1;
                Ok(if attempts == ID_ALLOCATION_ATTEMPTS {
                    entropy_bytes(0x88)
                } else {
                    entropy_bytes(0x77)
                })
            },
        )
        .expect("第 32 次 global ID 候选应成功");
        assert_eq!(attempts, ID_ALLOCATION_ATTEMPTS);
        assert_eq!(
            allocated.as_str(),
            format!("global-{}", lowercase_hex_128(entropy_bytes(0x88)))
        );
        assert_eq!(
            allocated.project_transaction_id(),
            format!("{}:project", allocated.as_str())
        );
        assert_eq!(
            allocated.session_transaction_id(),
            format!("{}:session", allocated.as_str())
        );
        assert_eq!(allocated.clone().into_inner(), allocated.as_str());

        assert_eq!(
            allocate_global_transaction_id_with(
                |candidate| prepared.contains(candidate),
                &BTreeSet::new(),
                || Ok(entropy_bytes(0x77)),
            ),
            Err(GlobalTransactionIdError::Exhausted)
        );
        let reserved_value = format!("global-{}", lowercase_hex_128(entropy_bytes(0x99)));
        let reserved = BTreeSet::from([reserved_value]);
        let mut entropy = VecDeque::from([entropy_bytes(0x99), entropy_bytes(0xaa)]);
        let allocated = allocate_global_transaction_id_with(
            |_| false,
            &reserved,
            || entropy.pop_front().ok_or(()),
        )
        .expect("局部 global reservation 碰撞后应换用下一候选");
        assert_eq!(
            allocated.as_str(),
            format!("global-{}", lowercase_hex_128(entropy_bytes(0xaa)))
        );
        assert_eq!(
            allocate_global_transaction_id_with(|_| false, &BTreeSet::new(), || Err(()),),
            Err(GlobalTransactionIdError::EntropyUnavailable)
        );
        assert!(GlobalTransactionId::new("global-not-hex".to_owned()).is_none());
        assert_eq!(
            GlobalTransactionIdError::EntropyUnavailable.protocol_code(),
            ProtocolErrorCode::ServiceUnavailable
        );

        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("open global ID runtime");
        let generated = runtime
            .allocate_external_global_tx_id(&BTreeSet::new())
            .expect("OS CSPRNG global ID");
        assert!(is_prefixed_hex_128(generated.as_str(), "global"));
    }

    #[test]
    fn durable_runtime_artifact_keeps_same_handle_and_put_failure_has_zero_projection() {
        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("open Artifact runtime");
        let global_tx_id = GlobalTransactionId::new(format!("global-{}", "a".repeat(32)))
            .expect("fixed global ID");
        let prepared = runtime
            .put_fixed_alda_fixture(&global_tx_id)
            .expect("put fixed fixture");
        let (record, audit_plan) = prepared.into_prepared_facts();
        assert_eq!(record.hash().as_str(), DURABLE_FIXTURE_HASH);
        assert_eq!(record.size(), DURABLE_FIXTURE_SIZE_BYTES);
        assert_eq!(audit_plan.control_transaction_id(), global_tx_id.as_str());

        let mut opened = runtime
            .read_artifact(record.hash())
            .expect("open verified same handle");
        let hex = record
            .hash()
            .as_str()
            .strip_prefix("sha256:")
            .expect("canonical fixture hash");
        let blob_path = root
            .path()
            .join("artifacts-v1/blobs/sha256")
            .join(&hex[..2])
            .join(hex);
        std::fs::remove_file(&blob_path).expect("remove verified path");
        std::fs::write(&blob_path, b"replacement").expect("replace verified path");

        let mut bytes = Vec::new();
        opened
            .read_to_end(&mut bytes)
            .expect("read original verified inode");
        assert_eq!(bytes, DURABLE_FIXTURE_BYTES);

        let before_view = runtime.read_view().clone();
        let before_control = runtime
            .core
            .control()
            .expect("control before failed put")
            .projection()
            .clone();
        assert!(matches!(
            runtime.put_fixed_alda_fixture(&global_tx_id),
            Err(StoreError::ExistingBlobCorrupt)
        ));
        assert_eq!(runtime.read_view(), &before_view);
        assert_eq!(
            runtime
                .core
                .control()
                .expect("control after failed put")
                .projection(),
            &before_control
        );
    }

    #[test]
    fn artifact_reference_disposition_uses_same_generation_and_global_reachability() {
        let root = private_root();
        let mut runtime = ReadyDurableRuntime::open(root.path()).expect("open disposition runtime");
        let global_tx_id = GlobalTransactionId::new(format!("global-{}", "b".repeat(32)))
            .expect("fixed global ID");

        let first = runtime
            .put_fixed_alda_fixture(&global_tx_id)
            .expect("first fixed fixture");
        let first_disposition = runtime
            .classify_artifact_reference_after_prepared_rejection(first)
            .expect("same generation classification");
        let ArtifactReferenceDisposition::OrphanCandidate(candidate) = first_disposition else {
            panic!("first unreferenced hash must be an orphan candidate");
        };
        assert_eq!(candidate.hash().as_str(), DURABLE_FIXTURE_HASH);

        let stale = runtime
            .put_fixed_alda_fixture(&global_tx_id)
            .expect("stale fixed fixture");
        runtime.published.generation += 1;
        assert!(
            runtime
                .classify_artifact_reference_after_prepared_rejection(stale)
                .is_none()
        );

        let referenced = runtime
            .put_fixed_alda_fixture(&global_tx_id)
            .expect("referenced fixed fixture");
        let referenced_record = referenced.record.clone();
        let project_id = domain_project("project-other-reference");
        let mut snapshot = ProjectSnapshot {
            project_id: Some(project_id.clone()),
            last_sequence: 1,
            ..ProjectSnapshot::default()
        };
        snapshot
            .artifacts
            .insert(referenced_record.hash().clone(), referenced_record);
        runtime.published.projects.insert(
            project_id.as_str().to_owned(),
            ProjectReadState {
                snapshot,
                last_sequence: 1,
                last_checksum: format!("sha256:{}", "c".repeat(64)),
            },
        );
        let (_, reachable) = runtime
            .published
            .derived_indexes()
            .expect("derive global reachability");
        runtime.published.reachable_artifact_hashes = reachable;

        assert_eq!(
            runtime.classify_artifact_reference_after_prepared_rejection(referenced),
            Some(ArtifactReferenceDisposition::AlreadyReachable)
        );
    }

    #[test]
    fn instance_lock_is_exclusive_reopenable_and_private() {
        let root = private_root();
        let first = InstanceLock::acquire(root.path()).expect("first lock");
        assert!(matches!(
            InstanceLock::acquire(root.path()),
            Err(DurableRuntimeError::InstanceAlreadyRunning)
        ));
        let metadata =
            std::fs::metadata(root.path().join(INSTANCE_LOCK_FILE)).expect("lock metadata");
        assert_eq!(metadata.permissions().mode() & 0o777, LOCK_MODE);
        drop(first);
        let _reopened = InstanceLock::acquire(root.path()).expect("lock released");
    }

    #[test]
    fn lock_rejects_relative_nonprivate_and_special_targets() {
        assert!(matches!(
            InstanceLock::acquire(Path::new("relative")),
            Err(DurableRuntimeError::InvalidDataRoot)
        ));
        let root = tempfile::tempdir().expect("temp root");
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755))
            .expect("public root");
        assert!(matches!(
            InstanceLock::acquire(root.path()),
            Err(DurableRuntimeError::InvalidDataRoot)
        ));

        let root = private_root();
        std::fs::create_dir(root.path().join(INSTANCE_LOCK_FILE)).expect("special target");
        assert!(InstanceLock::acquire(root.path()).is_err());
    }

    #[test]
    fn every_lock_failpoint_releases_the_kernel_lock() {
        for failpoint in [
            LockFailpoint::Create,
            LockFailpoint::Write,
            LockFailpoint::FileSync,
            LockFailpoint::DirectorySync,
        ] {
            let root = private_root();
            assert!(InstanceLock::acquire_inner(root.path(), Some(failpoint)).is_err());
            let _reopened = InstanceLock::acquire(root.path()).expect("failed open releases lock");
        }
    }

    #[test]
    fn lock_health_is_one_way() {
        let health = LockHealth::new();
        health.require_live().expect("initially live");
        health.invalidate();
        assert!(matches!(
            health.require_live(),
            Err(DurableRuntimeError::InstanceLockLost)
        ));
    }

    #[test]
    fn combined_redo_publishes_both_aggregates_and_exact_reply_after_reopen() {
        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("open runtime");
        let runtime = seed_project_session_for(
            runtime,
            &format!("global-{}", "8".repeat(32)),
            &format!("global-{}", "9".repeat(32)),
            "runtime-base",
            "project-runtime",
            "session-runtime",
            "runtime",
        );
        let request = combined_mutation_prepare_for(
            &runtime,
            GLOBAL_TX,
            "command-runtime",
            "project-runtime",
            "session-runtime",
            "runtime-combined",
        );
        let expected_reply = stable_reply("command-runtime");
        let (runtime, reply) = submit_ok(runtime, request.clone());
        assert_eq!(reply, expected_reply);
        assert_eq!(runtime.read_view().projects.len(), 1);
        assert_eq!(runtime.read_view().sessions.len(), 1);
        drop(runtime);

        let runtime = ReadyDurableRuntime::open(root.path()).expect("reopen runtime");
        let open_counts = runtime
            .core
            .components()
            .expect("runtime components")
            .state_store
            .writer_open_counts();
        assert_eq!(open_counts.len(), 2);
        assert!(open_counts.values().all(|count| *count == 1));
        let (runtime, retry_reply) = submit_ok(runtime, request);
        assert_eq!(retry_reply, expected_reply);
        assert_eq!(runtime.read_view().projects.len(), 1);
        assert_eq!(runtime.read_view().sessions.len(), 1);
    }

    #[test]
    fn durable_read_view_publish_builds_atomic_generations_and_derived_indexes() {
        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("open runtime");
        assert_eq!(runtime.read_view().generation, 1);

        let base_request = project_create_prepare_for(
            &format!("global-{}", "1".repeat(32)),
            "command-publish-base",
            "project-runtime",
            "publish",
        );
        let (runtime, _) = submit_ok(runtime, base_request.clone());
        assert_eq!(runtime.read_view().generation, 2);
        let (runtime, _) = submit_ok(runtime, base_request);
        assert_eq!(runtime.read_view().generation, 2);
        let (runtime, _) = submit_ok(
            runtime,
            session_start_prepare_for(
                &format!("global-{}", "2".repeat(32)),
                "command-publish-session",
                "project-runtime",
                "session-runtime",
            ),
        );
        assert_eq!(runtime.read_view().generation, 3);
        let request = session_only_prepare(
            &runtime,
            &format!("global-{}", "8".repeat(32)),
            "command-publish-turn",
            "session-runtime",
            "project-runtime",
            vec![SessionRolloutEvent::TurnStarted {
                turn_id: TurnId("turn-publish".to_owned()),
                canonical_prompt: "一次发布".to_owned(),
            }],
        );
        let (runtime, _) = submit_ok(runtime, request);
        let view = runtime.read_view();
        assert_eq!(view.generation, 4);
        assert_eq!(
            view.owners.turns.get("turn-publish").map(String::as_str),
            Some("session-runtime")
        );
        assert!(view.owners.questions.is_empty());
        assert!(view.owners.approvals.is_empty());
        assert!(view.reachable_artifact_hashes.is_empty());
        view.validate(runtime.core.control().expect("control").projection())
            .expect("published candidate");

        let counts = runtime
            .core
            .components()
            .expect("components")
            .state_store
            .writer_open_counts();
        let mut counts = counts.values().copied().collect::<Vec<_>>();
        counts.sort_unstable();
        assert_eq!(counts, vec![1, 3]);

        let mut owner_tampered = view.clone();
        owner_tampered
            .owners
            .turns
            .insert("turn-forged".to_owned(), "session-runtime".to_owned());
        assert!(
            owner_tampered
                .validate(runtime.core.control().expect("control").projection())
                .is_err()
        );
        let mut metadata_tampered = view.clone();
        metadata_tampered
            .project_metadata
            .get_mut("project-runtime")
            .expect("Project metadata")
            .version = 2;
        assert!(
            metadata_tampered
                .validate(runtime.core.control().expect("control").projection())
                .is_err()
        );

        drop(runtime);
        let reopened = ReadyDurableRuntime::open(root.path()).expect("startup publication");
        let startup_counts = reopened
            .core
            .components()
            .expect("components")
            .state_store
            .writer_open_counts();
        assert_eq!(startup_counts.len(), 2);
        assert!(startup_counts.values().all(|count| *count == 1));
        assert_eq!(reopened.read_view().generation, 1);
    }

    #[test]
    fn durable_runtime_read_exposes_only_published_semantic_facts() {
        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("open read runtime");
        let runtime = seed_project_session_for(
            runtime,
            &format!("global-{}", "1".repeat(32)),
            &format!("global-{}", "2".repeat(32)),
            "read",
            "project-read",
            "session-read",
            "read",
        );
        let turn_id = TurnId("turn-read".to_owned());
        let request = session_only_prepare(
            &runtime,
            &format!("global-{}", "3".repeat(32)),
            "command-read-turn",
            "session-read",
            "project-read",
            vec![SessionRolloutEvent::TurnStarted {
                turn_id: turn_id.clone(),
                canonical_prompt: "只返回 canonical prompt".to_owned(),
            }],
        );
        let (runtime, _) = submit_ok(runtime, request);
        let project_id = ProjectId("project-read".to_owned());
        let session_id = SessionId("session-read".to_owned());

        assert_eq!(
            runtime
                .project_projection(&project_id)
                .and_then(|snapshot| snapshot.project_id.as_ref())
                .map(DomainProjectId::as_str),
            Some("project-read")
        );
        assert_eq!(
            runtime.project_metadata(&project_id),
            Some(&ProtocolProjectSnapshot {
                project_id: project_id.clone(),
                name: "read".to_owned(),
                version: 1,
            })
        );
        let snapshot = runtime
            .session_snapshot(&session_id)
            .expect("published Session snapshot");
        assert_eq!(snapshot.covered_through_sequence, 2);
        assert_eq!(snapshot.turns[0].turn_id, turn_id);
        assert_eq!(
            runtime
                .project_head(&project_id)
                .map(|head| head.last_sequence),
            Some(1)
        );
        let session_head = runtime.session_head(&session_id).expect("Session head");
        assert_eq!(session_head.last_sequence, 2);
        assert!(is_sha256(&session_head.last_checksum));
        assert_eq!(
            runtime.owner_of(SessionObjectRef::Turn(&turn_id)),
            Some(&session_id)
        );
        assert_eq!(
            runtime.owner_of(SessionObjectRef::Question(&QuestionId(
                "missing".to_owned()
            ))),
            None
        );
        assert_eq!(
            runtime.owner_of(SessionObjectRef::Approval(&ApprovalId(
                "missing".to_owned()
            ))),
            None
        );
        assert_eq!(
            runtime.canonical_prompt(&session_id, &turn_id),
            Ok("只返回 canonical prompt")
        );
        assert_eq!(
            runtime.occurrence(
                &project_id,
                &ArtifactOccurrenceId("occurrence-missing".to_owned())
            ),
            None
        );
    }

    #[test]
    fn durable_runtime_read_prompt_truth_table_and_fail_closed_order_are_stable() {
        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("open prompt runtime");
        let runtime = seed_project_session_for(
            runtime,
            &format!("global-{}", "a".repeat(32)),
            &format!("global-{}", "b".repeat(32)),
            "prompt-primary",
            "project-prompt-primary",
            "session-prompt-primary",
            "prompt-primary",
        );
        let runtime = seed_project_session_for(
            runtime,
            &format!("global-{}", "c".repeat(32)),
            &format!("global-{}", "d".repeat(32)),
            "prompt-secondary",
            "project-prompt-secondary",
            "session-prompt-secondary",
            "prompt-secondary",
        );
        let turn_id = TurnId("turn-prompt-secondary".to_owned());
        let request = session_only_prepare(
            &runtime,
            &format!("global-{}", "e".repeat(32)),
            "command-prompt-secondary",
            "session-prompt-secondary",
            "project-prompt-secondary",
            vec![SessionRolloutEvent::TurnStarted {
                turn_id: turn_id.clone(),
                canonical_prompt: "canonical secondary prompt".to_owned(),
            }],
        );
        let (mut runtime, _) = submit_ok(runtime, request);
        let primary = SessionId("session-prompt-primary".to_owned());
        let secondary = SessionId("session-prompt-secondary".to_owned());

        assert_eq!(
            runtime.canonical_prompt(&SessionId("missing-session".to_owned()), &turn_id),
            Err(SessionReadError::SessionNotFound)
        );
        assert_eq!(
            runtime.canonical_prompt(&primary, &TurnId("missing-turn".to_owned())),
            Err(SessionReadError::TurnNotFound)
        );
        assert_eq!(
            runtime.canonical_prompt(&primary, &turn_id),
            Err(SessionReadError::TurnOwnershipMismatch)
        );
        assert_eq!(
            runtime.canonical_prompt(&secondary, &turn_id),
            Ok("canonical secondary prompt")
        );

        runtime.published.owners.turns.remove(&turn_id.0);
        assert_eq!(
            runtime.canonical_prompt(&secondary, &turn_id),
            Err(SessionReadError::CorruptPublishedView)
        );
    }

    #[test]
    fn cursor_truth_table_uses_one_durable_published_generation() {
        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("open cursor runtime");
        let runtime = seed_project_session_for(
            runtime,
            &format!("global-{}", "1".repeat(32)),
            &format!("global-{}", "2".repeat(32)),
            "cursor",
            "project-cursor",
            "session-cursor",
            "cursor",
        );
        let request = session_only_prepare(
            &runtime,
            &format!("global-{}", "3".repeat(32)),
            "command-cursor-turn",
            "session-cursor",
            "project-cursor",
            vec![SessionRolloutEvent::TurnStarted {
                turn_id: TurnId("turn-cursor-runtime".to_owned()),
                canonical_prompt: "cursor prompt".to_owned(),
            }],
        );
        let (runtime, _) = submit_ok(runtime, request);
        let cursor = |stream_kind, stream_id: &str, epoch, after_sequence| StreamCursor {
            stream_kind,
            stream_id: stream_id.to_owned(),
            epoch,
            after_sequence,
        };

        assert_eq!(
            runtime.resume_events(&cursor(
                crate::protocol::StreamKind::ProjectEvent,
                "missing-session",
                crate::protocol::SESSION_STREAM_EPOCH,
                0,
            )),
            Err(DurableCursorError::UnsupportedStreamKind)
        );
        assert_eq!(
            runtime.resume_events(&cursor(
                crate::protocol::StreamKind::SessionRollout,
                "missing-session",
                crate::protocol::SESSION_STREAM_EPOCH,
                0,
            )),
            Err(DurableCursorError::SessionNotFound)
        );
        assert_eq!(
            runtime.resume_events(&cursor(
                crate::protocol::StreamKind::SessionRollout,
                "session-cursor",
                2,
                0,
            )),
            Err(DurableCursorError::EpochMismatch {
                expected_epoch: crate::protocol::SESSION_STREAM_EPOCH,
                actual_epoch: 2,
                head_sequence: 2,
            })
        );
        assert_eq!(
            runtime.resume_events(&cursor(
                crate::protocol::StreamKind::SessionRollout,
                "session-cursor",
                crate::protocol::SESSION_STREAM_EPOCH,
                3,
            )),
            Err(DurableCursorError::Future { head_sequence: 2 })
        );

        let page = runtime
            .resume_events(&cursor(
                crate::protocol::StreamKind::SessionRollout,
                "session-cursor",
                crate::protocol::SESSION_STREAM_EPOCH,
                1,
            ))
            .expect("resume published events");
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].sequence, 2);
        assert_eq!(page.head_sequence, 2);
        assert_eq!(page.next_after_sequence, 2);
        assert_eq!(
            runtime
                .session_head(&SessionId("session-cursor".to_owned()))
                .map(|head| head.last_sequence),
            Some(page.head_sequence)
        );
    }

    #[test]
    fn project_metadata_rebuild_accepts_committed_create_and_ignores_pending() {
        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("open metadata root");
        let request = project_create_prepare_for(
            &format!("global-{}", "1".repeat(32)),
            "command-metadata-create",
            "project-metadata",
            "metadata name",
        );
        let (mut runtime, _) = submit_ok(runtime, request);
        assert_eq!(
            runtime.read_view().project_metadata.get("project-metadata"),
            Some(&ProtocolProjectSnapshot {
                project_id: ProjectId("project-metadata".to_owned()),
                name: "metadata name".to_owned(),
                version: 1,
            })
        );

        let pending = project_only_prepare(
            &runtime,
            &format!("global-{}", "2".repeat(32)),
            "command-metadata-pending",
            "project-metadata",
            vec![ProjectEvent::BriefRevisionCreated(CreativeBrief {
                id: BriefRevisionId::parse("brief-metadata-pending").expect("brief"),
                project_id: domain_project("project-metadata"),
                previous: None,
                user_description: "pending 不得贡献 metadata".to_owned(),
                goals: vec!["保持已提交创建事实".to_owned()],
                instrumentation: vec!["piano".to_owned()],
                open_questions: Vec::new(),
            })],
        );
        prepare_control_direct(&mut runtime, pending);
        let rebuilt = rebuild_project_metadata(
            runtime.core.control().expect("control").projection(),
            &runtime.published.projects,
        )
        .expect("pending mutation does not alter metadata");
        assert_eq!(rebuilt, runtime.published.project_metadata);
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "同一已验证基线需逐项证明 duplicate、orphan、variant、wire 与 protocol 均 fail closed"
    )]
    fn project_metadata_rebuild_rejects_incomplete_or_conflicting_creation_facts() {
        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("open metadata root");
        let request = project_create_prepare_for(
            &format!("global-{}", "1".repeat(32)),
            "command-metadata-base",
            "project-metadata",
            "metadata",
        );
        let (runtime, _) = submit_ok(runtime, request);
        let baseline = runtime
            .core
            .control()
            .expect("control")
            .projection()
            .clone();
        let projects = runtime.published.projects.clone();
        let initialized = || ProjectEvent::ProjectInitialized {
            project_id: domain_project("project-metadata"),
            score_id: ScoreId::parse("score-metadata").expect("score"),
            default_take_id: TakeId::parse("take-metadata").expect("take"),
            default_branch_id: BranchId::parse("branch-metadata").expect("branch"),
        };

        let mut missing = baseline.clone();
        missing.committed.clear();
        assert!(rebuild_project_metadata(&missing, &projects).is_err());

        let mut duplicate = baseline.clone();
        add_metadata_committed(
            &mut duplicate,
            metadata_project_prepared(
                &format!("global-{}", "3".repeat(32)),
                "command-metadata-duplicate",
                "project-metadata",
                "project-metadata",
                PROTOCOL_VERSION,
                0,
                None,
                vec![initialized()],
                true,
            ),
            1,
        );
        assert!(rebuild_project_metadata(&duplicate, &projects).is_err());

        let mut orphan = baseline.clone();
        add_metadata_committed(
            &mut orphan,
            metadata_project_prepared(
                &format!("global-{}", "4".repeat(32)),
                "command-metadata-orphan",
                "project-metadata",
                "project-metadata",
                PROTOCOL_VERSION,
                0,
                None,
                vec![initialized()],
                false,
            ),
            1,
        );
        assert!(rebuild_project_metadata(&orphan, &projects).is_err());

        let mut wrong_wire = baseline.clone();
        add_metadata_committed(
            &mut wrong_wire,
            metadata_project_prepared(
                &format!("global-{}", "5".repeat(32)),
                "command-metadata-wire",
                "project-metadata",
                "project-other",
                PROTOCOL_VERSION,
                0,
                None,
                vec![initialized()],
                true,
            ),
            1,
        );
        assert!(rebuild_project_metadata(&wrong_wire, &projects).is_err());

        let mut wrong_protocol = baseline.clone();
        add_metadata_committed(
            &mut wrong_protocol,
            metadata_project_prepared(
                &format!("global-{}", "6".repeat(32)),
                "command-metadata-protocol",
                "project-metadata",
                "project-metadata",
                PROTOCOL_VERSION + 1,
                0,
                None,
                vec![initialized()],
                true,
            ),
            1,
        );
        assert!(rebuild_project_metadata(&wrong_protocol, &projects).is_err());

        let state = projects.get("project-metadata").expect("Project state");
        let mut wrong_variant = baseline.clone();
        add_metadata_committed(
            &mut wrong_variant,
            metadata_project_prepared(
                &format!("global-{}", "7".repeat(32)),
                "command-metadata-variant",
                "project-metadata",
                "project-metadata",
                PROTOCOL_VERSION,
                state.last_sequence,
                Some(state.last_checksum.clone()),
                vec![ProjectEvent::BriefRevisionCreated(CreativeBrief {
                    id: BriefRevisionId::parse("brief-metadata-variant").expect("brief"),
                    project_id: domain_project("project-metadata"),
                    previous: None,
                    user_description: "non-create plan".to_owned(),
                    goals: vec!["拒绝错误 reply variant".to_owned()],
                    instrumentation: vec!["piano".to_owned()],
                    open_questions: Vec::new(),
                })],
                true,
            ),
            state.last_sequence + 1,
        );
        assert!(rebuild_project_metadata(&wrong_variant, &projects).is_err());
    }

    #[test]
    fn fixture_provenance_vector_is_unique_and_publishes_durable_local() {
        assert_eq!(
            format!("sha256:{:x}", sha2::Sha256::digest(DURABLE_FIXTURE_BYTES)),
            DURABLE_FIXTURE_HASH
        );
        assert_eq!(
            u64::try_from(DURABLE_FIXTURE_BYTES.len()).expect("fixture size"),
            DURABLE_FIXTURE_SIZE_BYTES
        );

        let root = private_root();
        let runtime = seed_pending_occurrence_approval(&root, "fixture");
        let recipe = occurrence_recipe(
            &runtime,
            &format!("global-{}", "4".repeat(32)),
            "occurrence-fixture",
        );
        let prepared = recipe.approved_prepared(
            "command-occurrence-fixture",
            PROTOCOL_VERSION,
            Some(recipe.manifest.clone()),
            true,
        );
        let (runtime, _) = submit_ok(
            runtime,
            PrepareControlRequest {
                project_allocation: None,
                session_allocation: None,
                prepared,
            },
        );
        assert_eq!(
            runtime
                .read_view()
                .occurrence_metadata
                .get("occurrence-fixture"),
            Some(&recipe.manifest)
        );
        assert_eq!(
            runtime.occurrence(
                &ProjectId("project-occurrence-fixture".to_owned()),
                &ArtifactOccurrenceId("occurrence-fixture".to_owned()),
            ),
            Some(&recipe.manifest)
        );
        assert_eq!(
            runtime.occurrence(
                &ProjectId("project-other".to_owned()),
                &ArtifactOccurrenceId("occurrence-fixture".to_owned()),
            ),
            None
        );

        let mut wrong_kind = serde_json::to_value(&recipe.manifest).expect("manifest JSON");
        wrong_kind["kind"] = serde_json::Value::String("midi".to_owned());
        assert!(serde_json::from_value::<ArtifactManifest>(wrong_kind).is_err());
        let mut wrong_producer = serde_json::to_value(&recipe.manifest).expect("manifest JSON");
        wrong_producer["producer"] = serde_json::Value::String("untrusted_provider".to_owned());
        assert!(serde_json::from_value::<ArtifactManifest>(wrong_producer).is_err());
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "同一正向闭包需逐项翻转 occurrence 的静态、动态与一对一 provenance"
    )]
    fn occurrence_metadata_rebuild_rejects_duplicate_orphan_version_and_every_tamper() {
        let root = private_root();
        let runtime = seed_pending_occurrence_approval(&root, "rebuild");
        let global_tx_id = format!("global-{}", "4".repeat(32));
        let recipe = occurrence_recipe(&runtime, &global_tx_id, "occurrence-rebuild");
        let duplicate_recipe = occurrence_recipe(
            &runtime,
            &format!("global-{}", "5".repeat(32)),
            "occurrence-rebuild",
        );
        let command_id = "command-occurrence-rebuild";
        let valid = recipe.approved_prepared(
            command_id,
            PROTOCOL_VERSION,
            Some(recipe.manifest.clone()),
            true,
        );
        let (runtime, _) = submit_ok(
            runtime,
            PrepareControlRequest {
                project_allocation: None,
                session_allocation: None,
                prepared: valid,
            },
        );
        let baseline = runtime
            .core
            .control()
            .expect("control")
            .projection()
            .clone();
        let projects = runtime.published.projects.clone();
        let sessions = runtime.published.sessions.clone();
        let rebuilt = rebuild_occurrence_metadata(&baseline, &projects, &sessions)
            .expect("valid occurrence metadata");
        assert_eq!(rebuilt.get("occurrence-rebuild"), Some(&recipe.manifest));

        let manifest_mutations: Vec<ManifestMutation> = vec![
            ("hash", |manifest| {
                manifest.artifact_hash =
                    ProtocolArtifactHash::parse(&format!("sha256:{}", "0".repeat(64)))
                        .expect("wrong hash");
            }),
            ("size", |manifest| manifest.size_bytes += 1),
            ("mime", |manifest| {
                manifest.mime_type = "text/plain".to_owned();
            }),
            ("producer-version", |manifest| manifest.fixture_version += 1),
            ("label", |manifest| {
                manifest.provenance_label.push_str(" tampered");
            }),
            ("durability", |manifest| {
                manifest.durability = ArtifactDurability::ProcessLifetimeFixture;
            }),
            ("project", |manifest| {
                manifest.project_id = ProjectId("project-other".to_owned());
            }),
            ("session", |manifest| {
                manifest.source_session_id = SessionId("session-other".to_owned());
            }),
            ("turn", |manifest| {
                manifest.source_turn_id = TurnId("turn-other".to_owned());
            }),
            ("sequence", |manifest| manifest.created_sequence += 1),
            ("occurrence-id", |manifest| {
                manifest.artifact_occurrence_id = ArtifactOccurrenceId("bad id".to_owned());
            }),
        ];
        for (label, mutate) in manifest_mutations {
            let mut manifest = recipe.manifest.clone();
            mutate(&mut manifest);
            let prepared =
                recipe.approved_prepared(command_id, PROTOCOL_VERSION, Some(manifest), true);
            let candidate = replace_occurrence_prepared(&baseline, &global_tx_id, prepared);
            assert!(
                rebuild_occurrence_metadata(&candidate, &projects, &sessions).is_err(),
                "{label} tamper must fail closed"
            );
        }

        let wrong_protocol = recipe.approved_prepared(
            command_id,
            PROTOCOL_VERSION + 1,
            Some(recipe.manifest.clone()),
            true,
        );
        assert!(
            rebuild_occurrence_metadata(
                &replace_occurrence_prepared(&baseline, &global_tx_id, wrong_protocol),
                &projects,
                &sessions,
            )
            .is_err()
        );

        let wrong_variant_reply = CommandReply::error(
            ClientCommandId(command_id.to_owned()),
            ProtocolErrorCode::InvalidRequest,
            "wrong occurrence reply variant",
        );
        let wrong_variant = recipe.prepared(
            command_id,
            &wrong_variant_reply,
            vec![recipe.resolved_event(ApprovalDecision::Approve)],
            true,
        );
        assert!(
            rebuild_occurrence_metadata(
                &replace_occurrence_prepared(&baseline, &global_tx_id, wrong_variant),
                &projects,
                &sessions,
            )
            .is_err()
        );

        let approve_without_manifest =
            recipe.approved_prepared(command_id, PROTOCOL_VERSION, None, false);
        assert!(
            rebuild_occurrence_metadata(
                &replace_occurrence_prepared(&baseline, &global_tx_id, approve_without_manifest,),
                &projects,
                &sessions,
            )
            .is_err()
        );

        let valid_reply = recipe.decision_reply(
            command_id,
            PROTOCOL_VERSION,
            ApprovalDecision::Approve,
            Some(recipe.manifest.clone()),
        );
        let orphan_event = recipe.prepared(
            command_id,
            &wrong_variant_reply,
            vec![recipe.resolved_event(ApprovalDecision::Approve)],
            false,
        );
        assert!(
            rebuild_occurrence_metadata(
                &replace_occurrence_prepared(&baseline, &global_tx_id, orphan_event),
                &projects,
                &sessions,
            )
            .is_err()
        );
        let orphan_audit = recipe.prepared(command_id, &wrong_variant_reply, Vec::new(), true);
        assert!(
            rebuild_occurrence_metadata(
                &replace_occurrence_prepared(&baseline, &global_tx_id, orphan_audit),
                &projects,
                &sessions,
            )
            .is_err()
        );
        let wrong_event = SessionRolloutEvent::ApprovalResolved {
            approval_id: ApprovalId("approval-other".to_owned()),
            approval_subject_digest: recipe.approval.approval_subject_digest.clone(),
            decision: ApprovalDecision::Approve,
            responder_client_id: ClientId("client-approver".to_owned()),
        };
        let wrong_event = recipe.prepared(command_id, &valid_reply, vec![wrong_event], true);
        assert!(
            rebuild_occurrence_metadata(
                &replace_occurrence_prepared(&baseline, &global_tx_id, wrong_event),
                &projects,
                &sessions,
            )
            .is_err()
        );
        let wrong_decision_reply = recipe.decision_reply(
            command_id,
            PROTOCOL_VERSION,
            ApprovalDecision::Deny,
            Some(recipe.manifest.clone()),
        );
        let wrong_decision = recipe.prepared(
            command_id,
            &wrong_decision_reply,
            vec![recipe.resolved_event(ApprovalDecision::Approve)],
            true,
        );
        assert!(
            rebuild_occurrence_metadata(
                &replace_occurrence_prepared(&baseline, &global_tx_id, wrong_decision),
                &projects,
                &sessions,
            )
            .is_err()
        );

        let mut wrong_approval = recipe.decided_approval(ApprovalDecision::Approve);
        wrong_approval.approval_subject_digest.value = "0".repeat(64);
        let wrong_digest_reply = CommandReply {
            protocol_version: PROTOCOL_VERSION,
            client_command_id: ClientCommandId(command_id.to_owned()),
            outcome: CommandOutcome::Success {
                result: CommandResult::ApprovalDecided {
                    approval: wrong_approval,
                    artifact_manifest: Some(recipe.manifest.clone()),
                },
            },
        };
        let wrong_digest = recipe.prepared(
            command_id,
            &wrong_digest_reply,
            vec![recipe.resolved_event(ApprovalDecision::Approve)],
            true,
        );
        assert!(
            rebuild_occurrence_metadata(
                &replace_occurrence_prepared(&baseline, &global_tx_id, wrong_digest),
                &projects,
                &sessions,
            )
            .is_err()
        );

        let mut wrong_projection_reply = recipe.decided_approval(ApprovalDecision::Approve);
        wrong_projection_reply.payload.estimated_impact = "tampered impact".to_owned();
        let wrong_projection_reply = CommandReply {
            protocol_version: PROTOCOL_VERSION,
            client_command_id: ClientCommandId(command_id.to_owned()),
            outcome: CommandOutcome::Success {
                result: CommandResult::ApprovalDecided {
                    approval: wrong_projection_reply,
                    artifact_manifest: Some(recipe.manifest.clone()),
                },
            },
        };
        let wrong_projection_reply = recipe.prepared(
            command_id,
            &wrong_projection_reply,
            vec![recipe.resolved_event(ApprovalDecision::Approve)],
            true,
        );
        assert!(
            rebuild_occurrence_metadata(
                &replace_occurrence_prepared(&baseline, &global_tx_id, wrong_projection_reply),
                &projects,
                &sessions,
            )
            .is_err()
        );

        let mut wrong_allocation = baseline.clone();
        wrong_allocation.sessions.insert(
            recipe.session_id.0.clone(),
            "project-wrong-allocation".to_owned(),
        );
        assert!(rebuild_occurrence_metadata(&wrong_allocation, &projects, &sessions).is_err());
        let mut wrong_session_anchor = baseline.clone();
        wrong_session_anchor
            .committed
            .get_mut(&global_tx_id)
            .and_then(|anchor| anchor.session_last.as_mut())
            .expect("Session anchor")
            .resulting_last_sequence += 1;
        assert!(rebuild_occurrence_metadata(&wrong_session_anchor, &projects, &sessions).is_err());
        let mut missing_project_anchor = baseline.clone();
        missing_project_anchor
            .committed
            .get_mut(&global_tx_id)
            .expect("occurrence anchor")
            .project_last = None;
        assert!(
            rebuild_occurrence_metadata(&missing_project_anchor, &projects, &sessions).is_err()
        );

        let mut duplicate = baseline.clone();
        let duplicate_prepared = duplicate_recipe.approved_prepared(
            "command-occurrence-duplicate",
            PROTOCOL_VERSION,
            Some(duplicate_recipe.manifest.clone()),
            true,
        );
        duplicate
            .prepared_order
            .push(duplicate_recipe.global_tx_id.clone());
        assert!(
            duplicate
                .prepared
                .insert(duplicate_recipe.global_tx_id.clone(), duplicate_prepared,)
                .is_none()
        );
        let occurrence_anchor = duplicate
            .committed
            .get(&global_tx_id)
            .cloned()
            .expect("occurrence anchor");
        assert!(
            duplicate
                .committed
                .insert(duplicate_recipe.global_tx_id.clone(), occurrence_anchor)
                .is_none()
        );
        assert!(rebuild_occurrence_metadata(&duplicate, &projects, &sessions).is_err());

        let pending_recipe = occurrence_recipe(
            &runtime,
            &format!("global-{}", "6".repeat(32)),
            "occurrence-pending",
        );
        let mut pending = baseline.clone();
        pending
            .prepared_order
            .push(pending_recipe.global_tx_id.clone());
        assert!(
            pending
                .prepared
                .insert(
                    pending_recipe.global_tx_id.clone(),
                    pending_recipe.approved_prepared(
                        "command-occurrence-pending",
                        PROTOCOL_VERSION,
                        Some(pending_recipe.manifest.clone()),
                        true,
                    ),
                )
                .is_none()
        );
        assert_eq!(
            rebuild_occurrence_metadata(&pending, &projects, &sessions)
                .expect("pending occurrence ignored"),
            rebuilt
        );

        let old_root = private_root();
        let (old_store, _old_guard) =
            ArtifactStore::open_for_durable_runtime(old_root.path()).expect("old Store");
        let old_record = old_store
            .put(Cursor::new(DURABLE_FIXTURE_BYTES), None)
            .expect("old same-hash put")
            .into_record()
            .expect("old same-hash record");
        assert_eq!(old_record.hash(), recipe.artifact_record.hash());
        assert_ne!(
            old_record.store_instance_id(),
            recipe.artifact_record.store_instance_id()
        );
        let mut old_projects = projects.clone();
        old_projects
            .get_mut(recipe.project_id.as_str())
            .expect("occurrence Project")
            .snapshot
            .artifacts
            .insert(old_record.hash().clone(), old_record);
        assert!(rebuild_occurrence_metadata(&baseline, &old_projects, &sessions).is_err());
    }

    #[test]
    fn occurrence_metadata_rebuild_ignores_deny_without_artifact_plan() {
        let root = private_root();
        let runtime = seed_pending_occurrence_approval(&root, "deny");
        let recipe = occurrence_recipe(
            &runtime,
            &format!("global-{}", "4".repeat(32)),
            "occurrence-deny-unused",
        );
        let command_id = "command-occurrence-deny";
        let reply =
            recipe.decision_reply(command_id, PROTOCOL_VERSION, ApprovalDecision::Deny, None);
        let prepared = recipe.prepared(
            command_id,
            &reply,
            vec![recipe.resolved_event(ApprovalDecision::Deny)],
            false,
        );
        let (runtime, _) = submit_ok(
            runtime,
            PrepareControlRequest {
                project_allocation: None,
                session_allocation: None,
                prepared,
            },
        );
        assert!(runtime.read_view().occurrence_metadata.is_empty());
    }

    #[test]
    fn durable_read_view_publish_cut_never_returns_a_half_view_and_committed_candidate_failure_is_fatal()
     {
        let cut_root = private_root();
        let mut runtime = ReadyDurableRuntime::open(cut_root.path()).expect("open cut runtime");
        runtime.set_failpoint(RuntimeFailpoint::Publish);
        let Err(SubmitFailure::Recovering { runtime, .. }) =
            runtime.submit(project_create_prepare_for(
                &format!("global-{}", "3".repeat(32)),
                "command-publish-cut",
                "project-publish-cut",
                "publish-cut",
            ))
        else {
            panic!("Publish cut must expose only Recovering typestate");
        };
        let Ok(runtime) = runtime.recover() else {
            panic!("finish candidate publication");
        };
        assert_eq!(runtime.read_view().generation, 2);
        assert_eq!(runtime.read_view().projects.len(), 1);
        assert!(runtime.read_view().sessions.is_empty());

        let fatal_root = private_root();
        let mut runtime = ReadyDurableRuntime::open(fatal_root.path()).expect("open fatal runtime");
        runtime.published.generation = u64::MAX;
        let Err(SubmitFailure::Fatal(fatal)) = runtime.submit(project_create_prepare_for(
            &format!("global-{}", "4".repeat(32)),
            "command-candidate-fatal",
            "project-candidate-fatal",
            "candidate-fatal",
        )) else {
            panic!("Committed 后候选失败必须进入 Fatal");
        };
        assert!(matches!(
            fatal.error(),
            DurableRuntimeError::CatalogMismatch
        ));
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "测试完整覆盖 Prepared 到启动 redo 的单一场景"
    )]
    fn command_only_authorization_pending_redo_uses_verified_control_catalog() {
        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("open runtime");
        let runtime = seed_project_session_for(
            runtime,
            &format!("global-{}", "1".repeat(32)),
            &format!("global-{}", "2".repeat(32)),
            "owner-base",
            "project-runtime",
            "session-runtime",
            "runtime",
        );
        let runtime = seed_project_session_for(
            runtime,
            &format!("global-{}", "3".repeat(32)),
            &format!("global-{}", "4".repeat(32)),
            "requested-base",
            "project-requested",
            "session-requested",
            "requested",
        );

        let session_id = SessionId("session-runtime".to_owned());
        let project_id = ProjectId("project-runtime".to_owned());
        let state_store = &runtime
            .core
            .components()
            .expect("runtime components")
            .state_store;
        let writer = match state_store
            .open_session_writer_with_catalog(
                session_id.clone(),
                runtime
                    .core
                    .session_catalog_context()
                    .expect("catalog")
                    .clone(),
            )
            .expect("open owner Session")
        {
            OpenSessionWriter::Ready(writer) => writer,
            OpenSessionWriter::RepairRequired(_) => panic!("clean owner Session"),
        };
        let (pre_sequence, pre_checksum) = writer.head();
        let pre_checksum = pre_checksum.map(ToOwned::to_owned);
        drop(writer);
        let turn_global = format!("global-{}", "5".repeat(32));
        let turn_record = StoredCommandRecordV1::new(
            "client-runtime",
            "command-turn-base",
            format!("sha256:{}", "5".repeat(64)),
            &stable_reply("command-turn-base"),
        )
        .expect("turn command");
        let turn_request = SessionAppendRequest::new(
            session_transaction_id(&turn_global),
            Some(turn_record.clone()),
            vec![SessionRolloutEvent::TurnStarted {
                turn_id: TurnId("turn-command-only-pending".to_owned()),
                canonical_prompt: "pending redo".to_owned(),
            }],
        );
        let turn_plan = StoredSessionPlanV1::from_append_request(
            &session_id,
            &project_id,
            pre_sequence,
            pre_checksum,
            &turn_request,
        )
        .expect("turn plan");
        let turn_prepared =
            PreparedTransactionV1::new(turn_global, turn_record, None, Some(turn_plan), Vec::new())
                .expect("turn Prepared");
        let (mut runtime, _) = submit_ok(
            runtime,
            PrepareControlRequest {
                project_allocation: None,
                session_allocation: None,
                prepared: turn_prepared,
            },
        );

        let writer = match runtime
            .core
            .components()
            .expect("runtime components")
            .state_store
            .open_session_writer_with_catalog(
                session_id.clone(),
                runtime
                    .core
                    .session_catalog_context()
                    .expect("catalog")
                    .clone(),
            )
            .expect("open owner Session")
        {
            OpenSessionWriter::Ready(writer) => writer,
            OpenSessionWriter::RepairRequired(_) => panic!("clean owner Session"),
        };
        let (pre_sequence, pre_checksum) = writer.head();
        let pre_checksum = pre_checksum.map(ToOwned::to_owned);
        drop(writer);
        let requested = SessionId("session-requested".to_owned());
        let typed_command = ClientCommand::TurnCancel {
            session_id: requested.clone(),
            turn_id: TurnId("turn-command-only-pending".to_owned()),
        };
        let command_id = ClientCommandId("command-only-pending".to_owned());
        let reply = CommandReply::error(
            command_id.clone(),
            ProtocolErrorCode::TurnOwnershipMismatch,
            format!(
                "turn `turn-command-only-pending` does not belong to session `{}`",
                requested.0
            ),
        );
        let raw_reply = serde_json::to_vec(&reply).expect("canonical reply");
        let payload_digest = external_command_payload_digest(PROTOCOL_VERSION, &typed_command)
            .expect("typed digest");
        let command_record = StoredCommandRecordV1::new(
            "client-runtime",
            &command_id.0,
            &payload_digest,
            &raw_reply,
        )
        .expect("command-only record");
        let global_tx_id = format!("global-{}", "6".repeat(32));
        let request = SessionAppendRequest::new_command_only(
            session_transaction_id(&global_tx_id),
            command_record.clone(),
            StoredCommandOnlyAuthorizationV1::new(
                typed_command,
                CommandOnlyReasonV1::TurnOwnershipMismatch,
            ),
        );
        let plan = StoredSessionPlanV1::from_append_request(
            &session_id,
            &project_id,
            pre_sequence,
            pre_checksum,
            &request,
        )
        .expect("command-only plan");
        let prepared =
            PreparedTransactionV1::new(global_tx_id, command_record, None, Some(plan), Vec::new())
                .expect("command-only Prepared");
        prepare_control_direct(
            &mut runtime,
            PrepareControlRequest {
                project_allocation: None,
                session_allocation: None,
                prepared,
            },
        );
        drop(runtime);

        let runtime = ReadyDurableRuntime::open(root.path()).expect("pending redo");
        assert_eq!(
            runtime
                .lookup_command(
                    &ClientId("client-runtime".to_owned()),
                    &command_id,
                    &payload_digest,
                )
                .expect("committed lookup"),
            CommandLookup::ExactReply(raw_reply)
        );
    }

    #[test]
    fn durable_command_lookup_distinguishes_unseen_exact_and_conflict_without_side_effects() {
        let root = private_root();
        let request = project_create_prepare_for(
            &format!("global-{}", "1".repeat(32)),
            "command-lookup",
            "project-lookup",
            "lookup",
        );
        let payload_digest = request.prepared.command_record.payload_digest.clone();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("打开 lookup runtime");
        let (runtime, expected_reply) = submit_ok(runtime, request);
        let client_id = ClientId("client-runtime".to_owned());
        let command_id = ClientCommandId("command-lookup".to_owned());

        assert_eq!(
            runtime
                .lookup_command(
                    &ClientId("unseen-client".to_owned()),
                    &ClientCommandId("unseen-command".to_owned()),
                    &payload_digest,
                )
                .expect("查询未见命令"),
            CommandLookup::Unseen
        );
        assert!(matches!(
            runtime.lookup_command(
                &client_id,
                &command_id,
                &format!("sha256:{}", "f".repeat(64)),
            ),
            Err(CommandLookupError::IdempotencyConflict)
        ));

        let before_files = authoritative_snapshot(root.path());
        let before_view = runtime.read_view().clone();
        let before_projection = runtime
            .core
            .control()
            .expect("读取 lookup 前 control")
            .projection()
            .clone();
        let before_open_counts = runtime
            .core
            .components()
            .expect("读取 lookup 前 components")
            .state_store
            .writer_open_counts();
        assert_eq!(
            runtime
                .lookup_command(&client_id, &command_id, &payload_digest)
                .expect("查询 exact reply"),
            CommandLookup::ExactReply(expected_reply)
        );
        assert_eq!(authoritative_snapshot(root.path()), before_files);
        assert_eq!(runtime.read_view(), &before_view);
        assert_eq!(
            runtime
                .core
                .control()
                .expect("读取 lookup 后 control")
                .projection(),
            &before_projection
        );
        assert_eq!(
            runtime
                .core
                .components()
                .expect("读取 lookup 后 components")
                .state_store
                .writer_open_counts(),
            before_open_counts
        );
    }

    #[test]
    fn durable_command_lookup_rejects_missing_prepared_or_committed_index_entries() {
        let root = private_root();
        let request = project_create_prepare_for(
            &format!("global-{}", "1".repeat(32)),
            "command-lookup-index",
            "project-lookup-index",
            "lookup-index",
        );
        let payload_digest = request.prepared.command_record.payload_digest.clone();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("打开 lookup index runtime");
        let (runtime, _) = submit_ok(runtime, request);
        let client_id = ClientId("client-runtime".to_owned());
        let command_id = ClientCommandId("command-lookup-index".to_owned());
        let key = (client_id.0.clone(), command_id.0.clone());
        let projection = runtime
            .core
            .control()
            .expect("读取 lookup index")
            .projection()
            .clone();
        let global_tx_id = projection.commands[&key].global_tx_id.clone();

        let mut missing_prepared = projection.clone();
        missing_prepared.prepared.remove(&global_tx_id);
        assert!(matches!(
            lookup_command_in_projection(
                &missing_prepared,
                &client_id,
                &command_id,
                &payload_digest,
            ),
            Err(CommandLookupError::CorruptCommittedIndex(_))
        ));

        let mut missing_committed = projection;
        missing_committed.committed.remove(&global_tx_id);
        assert!(matches!(
            lookup_command_in_projection(
                &missing_committed,
                &client_id,
                &command_id,
                &payload_digest,
            ),
            Err(CommandLookupError::CorruptCommittedIndex(_))
        ));
    }

    #[test]
    fn durable_command_lookup_binds_exact_reply_to_the_current_protocol() {
        let root = private_root();
        let request = project_create_prepare_for(
            &format!("global-{}", "1".repeat(32)),
            "command-lookup-version",
            "project-lookup-version",
            "lookup-version",
        );
        let payload_digest = request.prepared.command_record.payload_digest.clone();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("打开 lookup version runtime");
        let (runtime, _) = submit_ok(runtime, request);
        let client_id = ClientId("client-runtime".to_owned());
        let command_id = ClientCommandId("command-lookup-version".to_owned());
        let key = (client_id.0.clone(), command_id.0.clone());
        let mut projection = runtime
            .core
            .control()
            .expect("读取 lookup version index")
            .projection()
            .clone();
        let global_tx_id = projection.commands[&key].global_tx_id.clone();

        let mut reply = crate::protocol::CommandReply::error(
            command_id.clone(),
            ProtocolErrorCode::InvalidRequest,
            "未知协议 lookup fixture",
        );
        reply.protocol_version = PROTOCOL_VERSION + 1;
        let raw = serde_json::to_vec(&reply).expect("编码未知协议 lookup reply");
        let versioned_record = StoredCommandRecordV1::new(
            client_id.0.clone(),
            command_id.0.clone(),
            payload_digest.clone(),
            &raw,
        )
        .expect("构造未知协议 lookup record");
        projection
            .commands
            .get_mut(&key)
            .expect("global command")
            .command_record = versioned_record.clone();
        projection
            .prepared
            .get_mut(&global_tx_id)
            .expect("Prepared")
            .command_record = versioned_record;

        assert!(matches!(
            lookup_command_in_projection(&projection, &client_id, &command_id, &payload_digest),
            Err(CommandLookupError::CorruptCommittedIndex(_))
        ));
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "测试在同一场景内构造逆字典序事务并验证完整启动恢复"
    )]
    fn startup_replays_pending_in_control_order_with_one_open_per_aggregate() {
        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("open runtime");
        let project_request = project_create_prepare_for(
            &format!("global-{}", "1".repeat(32)),
            "command-base-project",
            "project-runtime",
            "base",
        );
        let (runtime, _) = submit_ok(runtime, project_request);
        let base_request = session_start_prepare_for(
            &format!("global-{}", "2".repeat(32)),
            "command-base-session",
            "project-runtime",
            "session-runtime",
        );
        let (runtime, _) = submit_ok(runtime, base_request.clone());
        drop(runtime);

        let planning_root = private_root();
        let planning_store =
            StateStore::open(planning_root.path(), StateStoreInstanceLease::for_tests())
                .expect("planning store");
        let session_id = SessionId("session-runtime".to_owned());
        let project_id = ProjectId("project-runtime".to_owned());
        let planning_writer = match planning_store
            .open_session_writer(session_id.clone())
            .expect("planning Session writer")
        {
            OpenSessionWriter::Ready(writer) => writer,
            OpenSessionWriter::RepairRequired(_) => panic!("fresh planning Session"),
        };
        let base_plan = base_request
            .prepared
            .session_plan
            .clone()
            .expect("base Session plan");
        let Ok((planning_writer, _)) = planning_writer.append(
            base_plan
                .into_append_request()
                .expect("base Session request"),
        ) else {
            panic!("planning base append");
        };

        let first_global = format!("global-{}", "f".repeat(32));
        let first_command = StoredCommandRecordV1::new(
            "client-runtime",
            "command-first",
            format!("sha256:{}", "a".repeat(64)),
            &stable_reply("command-first"),
        )
        .expect("first command");
        let (first_pre_sequence, first_pre_checksum) = planning_writer.head();
        let first_request = SessionAppendRequest::new(
            session_transaction_id(&first_global),
            Some(first_command.clone()),
            vec![SessionRolloutEvent::TurnStarted {
                turn_id: TurnId("turn-ordered".to_owned()),
                canonical_prompt: "ordered recovery".to_owned(),
            }],
        );
        let first_plan = StoredSessionPlanV1::from_append_request(
            &session_id,
            &project_id,
            first_pre_sequence,
            first_pre_checksum.map(ToOwned::to_owned),
            &first_request,
        )
        .expect("first Session plan");
        let Ok((planning_writer, _)) = planning_writer.append(first_request) else {
            panic!("planning first append");
        };

        let second_global = format!("global-{}", "0".repeat(32));
        let second_command = StoredCommandRecordV1::new(
            "client-runtime",
            "command-second",
            format!("sha256:{}", "b".repeat(64)),
            &stable_reply("command-second"),
        )
        .expect("second command");
        let (second_pre_sequence, second_pre_checksum) = planning_writer.head();
        let second_request = SessionAppendRequest::new(
            session_transaction_id(&second_global),
            Some(second_command.clone()),
            vec![SessionRolloutEvent::TurnCancelRequested {
                turn_id: TurnId("turn-ordered".to_owned()),
            }],
        );
        let second_plan = StoredSessionPlanV1::from_append_request(
            &session_id,
            &project_id,
            second_pre_sequence,
            second_pre_checksum.map(ToOwned::to_owned),
            &second_request,
        )
        .expect("second Session plan");
        drop(planning_writer);
        drop(planning_store);

        let mut runtime = ReadyDurableRuntime::open(root.path()).expect("reopen runtime");
        let writer = runtime.core.take_control().expect("control writer");
        let first = PreparedTransactionV1::new(
            first_global,
            first_command,
            None,
            Some(first_plan),
            Vec::new(),
        )
        .expect("first Prepared");
        let Ok((writer, _)) = writer.prepare(PrepareControlRequest {
            project_allocation: None,
            session_allocation: None,
            prepared: first,
        }) else {
            panic!("first control prepare");
        };
        let second = PreparedTransactionV1::new(
            second_global,
            second_command,
            None,
            Some(second_plan),
            Vec::new(),
        )
        .expect("second Prepared");
        let Ok((writer, _)) = writer.prepare(PrepareControlRequest {
            project_allocation: None,
            session_allocation: None,
            prepared: second,
        }) else {
            panic!("second control prepare");
        };
        runtime
            .core
            .put_control(writer)
            .expect("return control writer");
        drop(runtime);

        let runtime = ReadyDurableRuntime::open(root.path()).expect("ordered startup recovery");
        assert_eq!(
            runtime.read_view().sessions["session-runtime"].covered_through_sequence,
            4
        );
        let counts = runtime
            .core
            .components()
            .expect("runtime components")
            .state_store
            .writer_open_counts();
        assert_eq!(counts.len(), 2);
        assert!(counts.values().all(|count| *count == 1));
    }

    #[test]
    fn startup_preflights_and_commits_coordinated_restart_once() {
        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("open runtime");
        let runtime = seed_project_session_for(
            runtime,
            &format!("global-{}", "1".repeat(32)),
            &format!("global-{}", "2".repeat(32)),
            "restart-base",
            "project-runtime",
            "session-runtime",
            "restart",
        );
        let session_id = SessionId("session-runtime".to_owned());
        let session_writer = match runtime
            .core
            .components()
            .expect("runtime components")
            .state_store
            .open_session_writer(session_id.clone())
            .expect("open Session head")
        {
            OpenSessionWriter::Ready(writer) => writer,
            OpenSessionWriter::RepairRequired(_) => panic!("clean Session"),
        };
        let (pre_sequence, pre_checksum) = session_writer.head().map_checksum(ToOwned::to_owned);
        drop(session_writer);
        let global_tx = format!("global-{}", "7".repeat(32));
        let command = StoredCommandRecordV1::new(
            "client-runtime",
            "command-turn",
            format!("sha256:{}", "7".repeat(64)),
            &stable_reply("command-turn"),
        )
        .expect("turn command");
        let request = SessionAppendRequest::new(
            session_transaction_id(&global_tx),
            Some(command.clone()),
            vec![SessionRolloutEvent::TurnStarted {
                turn_id: TurnId("turn-restart".to_owned()),
                canonical_prompt: "restart me".to_owned(),
            }],
        );
        let plan = StoredSessionPlanV1::from_append_request(
            &session_id,
            &ProjectId("project-runtime".to_owned()),
            pre_sequence,
            pre_checksum,
            &request,
        )
        .expect("Turn plan");
        let prepared = PreparedTransactionV1::new(global_tx, command, None, Some(plan), Vec::new())
            .expect("Turn Prepared");
        let (runtime, _) = submit_ok(
            runtime,
            PrepareControlRequest {
                project_allocation: None,
                session_allocation: None,
                prepared,
            },
        );
        assert_eq!(
            runtime.read_view().sessions["session-runtime"].turns[0].status,
            TurnStatus::Running
        );
        drop(runtime);

        let runtime = ReadyDurableRuntime::open(root.path()).expect("restart reconciliation");
        assert_eq!(
            runtime.read_view().sessions["session-runtime"].turns[0].status,
            TurnStatus::AbortedByRestart
        );
        assert_eq!(
            runtime
                .core
                .control()
                .expect("control")
                .projection()
                .capacity(),
            crate::control_store::ControlCapacity {
                external: 3,
                internal_restart: 1,
                total: 4,
            }
        );
        drop(runtime);

        let runtime = ReadyDurableRuntime::open(root.path()).expect("idempotent second restart");
        assert_eq!(
            runtime
                .core
                .control()
                .expect("control")
                .projection()
                .capacity(),
            crate::control_store::ControlCapacity {
                external: 3,
                internal_restart: 1,
                total: 4,
            }
        );
    }

    #[test]
    fn durable_backend_capacity_restart_cuts_do_not_consume_duplicate_internal_slots() {
        for stage in [
            StartupStage::BeforePrepare,
            StartupStage::AfterPrepare,
            StartupStage::AfterSession,
            StartupStage::AfterCommit,
        ] {
            let root = multiple_running_sessions(3);
            let Err(failure) = ReadyDurableRuntime::open_with_startup_failpoint(
                root.path(),
                StartupFailpoint {
                    stage,
                    occurrence: 1,
                },
            ) else {
                panic!("startup cut must fail");
            };
            assert!(matches!(
                failure,
                DurableRuntimeError::InjectedFailure { .. }
            ));

            let runtime =
                ReadyDurableRuntime::open(root.path()).expect("restart after startup cut");
            assert!(runtime.read_view().sessions.values().all(|session| {
                session.turns.len() == 1 && session.turns[0].status == TurnStatus::AbortedByRestart
            }));
            let capacity = runtime
                .core
                .control()
                .expect("control")
                .projection()
                .capacity();
            assert_eq!(capacity.external, 9);
            assert_eq!(capacity.internal_restart, 3);
            assert_eq!(capacity.total, 12);
            drop(runtime);

            let runtime = ReadyDurableRuntime::open(root.path()).expect("idempotent restart");
            assert_eq!(
                runtime
                    .core
                    .control()
                    .expect("control")
                    .projection()
                    .capacity(),
                capacity
            );
        }
    }

    #[test]
    fn durable_backend_capacity_synthetic_startup_preflight_is_atomic() {
        // 此测试只保留小规模 startup 算法注入覆盖；真实精确边界由下一测试从 JSONL replay。
        let cases = [
            (
                "internal-9_999-plus-2",
                ControlCapacity {
                    external: 0,
                    internal_restart: 9_999,
                    total: 9_999,
                },
                2,
            ),
            (
                "internal-10_000-plus-1",
                ControlCapacity {
                    external: 0,
                    internal_restart: 10_000,
                    total: 10_000,
                },
                1,
            ),
            (
                "internal-10_001-plus-1",
                ControlCapacity {
                    external: 0,
                    internal_restart: 10_001,
                    total: 10_001,
                },
                1,
            ),
            (
                "total-19_999-plus-2",
                ControlCapacity {
                    external: 10_000,
                    internal_restart: 9_999,
                    total: 19_999,
                },
                2,
            ),
            (
                "total-20_000-plus-1",
                ControlCapacity {
                    external: 10_000,
                    internal_restart: 10_000,
                    total: 20_000,
                },
                1,
            ),
            (
                "total-20_001-plus-1",
                ControlCapacity {
                    external: 10_000,
                    internal_restart: 10_001,
                    total: 20_001,
                },
                1,
            ),
        ];

        for (label, capacity, session_count) in cases {
            let root = multiple_running_sessions(session_count);
            let before = startup_persistence_snapshot(root.path());
            let Err(failure) =
                ReadyDurableRuntime::open_with_startup_test_capacity(root.path(), capacity)
            else {
                panic!("{label} 容量超限必须在启动 reconciliation 前拒绝");
            };
            assert!(
                matches!(
                    failure,
                    DurableRuntimeError::Component {
                        component: "restart capacity",
                        ..
                    }
                ),
                "{label} 返回错误：{failure}"
            );
            let after = startup_persistence_snapshot(root.path());
            assert_eq!(after, before, "{label} 不得新增 control 或 Session 事实");
        }
    }

    #[test]
    fn durable_backend_capacity_real_multi_session_startup_preflight_is_atomic() {
        let cases = [
            (
                "internal-9_999-plus-2",
                ControlCapacity {
                    external: 6,
                    internal_restart: 9_999,
                    total: 10_005,
                },
            ),
            (
                "total-19_999-plus-2",
                ControlCapacity {
                    external: 10_000,
                    internal_restart: 9_999,
                    total: 19_999,
                },
            ),
        ];

        for (label, expected_capacity) in cases {
            let root = multiple_running_sessions(2);
            extend_real_startup_capacity(
                root.path(),
                expected_capacity.external,
                expected_capacity.internal_restart,
            );
            let before = startup_persistence_snapshot(root.path());
            assert_eq!(before.control_capacity, expected_capacity, "{label}");
            assert_eq!(before.prepared_count, expected_capacity.total, "{label}");
            assert_eq!(before.committed_count, expected_capacity.total, "{label}");
            assert_eq!(before.session_heads.len(), 2, "{label}");

            let Err(failure) = ReadyDurableRuntime::open(root.path()) else {
                panic!("{label} 必须在多 Session restart append 前拒绝");
            };
            assert!(
                matches!(
                    failure,
                    DurableRuntimeError::Component {
                        component: "restart capacity",
                        ..
                    }
                ),
                "{label} 返回错误：{failure}"
            );
            let after = startup_persistence_snapshot(root.path());
            assert_eq!(
                after, before,
                "{label} 不得新增 control 或任一 Session 字节"
            );
            println!(
                "{label}: replay Prepared/Committed={}/{}, capacity={:?}, sessions={}，零新增",
                after.prepared_count,
                after.committed_count,
                after.control_capacity,
                after.session_heads.len()
            );
        }
    }

    #[test]
    fn durable_backend_fault_every_post_prepare_cut_converges_before_ready() {
        for failpoint in [
            RuntimeFailpoint::Project,
            RuntimeFailpoint::Session,
            RuntimeFailpoint::Commit,
            RuntimeFailpoint::Publish,
            RuntimeFailpoint::Response,
        ] {
            let root = private_root();
            let runtime = ReadyDurableRuntime::open(root.path()).expect("open runtime");
            let runtime = seed_project_session_for(
                runtime,
                &format!("global-{}", "1".repeat(32)),
                &format!("global-{}", "2".repeat(32)),
                "cut-base",
                "project-runtime",
                "session-runtime",
                "cut",
            );
            let request = combined_mutation_prepare_for(
                &runtime,
                &format!("global-{}", "3".repeat(32)),
                "command-runtime",
                "project-runtime",
                "session-runtime",
                "cut-target",
            );
            let mut runtime = runtime;
            runtime.set_failpoint(failpoint);
            let Err(failure) = runtime.submit(request.clone()) else {
                panic!("injected cut must fail");
            };
            let SubmitFailure::Recovering { runtime, .. } = failure else {
                panic!("post-prepare cut must enter Recovering");
            };
            drop(runtime);

            let runtime = ReadyDurableRuntime::open(root.path()).expect("startup redo");
            assert_eq!(runtime.read_view().projects.len(), 1);
            assert_eq!(runtime.read_view().sessions.len(), 1);
            let (_runtime, reply) = submit_ok(runtime, request);
            assert_eq!(reply, stable_reply("command-runtime"));
        }
    }

    #[test]
    fn durable_backend_fault_control_recovery_capability_loss_is_fatal_without_panic() {
        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("open runtime");
        let runtime = seed_project_session_for(
            runtime,
            &format!("global-{}", "1".repeat(32)),
            &format!("global-{}", "2".repeat(32)),
            "control-sync-base",
            "project-runtime",
            "session-runtime",
            "control-sync",
        );
        let request = combined_mutation_prepare_for(
            &runtime,
            &format!("global-{}", "3".repeat(32)),
            "command-control-recovery-sync",
            "project-runtime",
            "session-runtime",
            "control-sync-target",
        );
        let mut runtime = runtime;
        runtime.set_failpoint(RuntimeFailpoint::CommitRecoverySync);
        let Err(failure) = runtime.submit(request.clone()) else {
            panic!("double control sync failure must fail");
        };
        let SubmitFailure::Fatal(fatal) = failure else {
            panic!("lost control writer must be terminal");
        };
        assert!(matches!(
            fatal.error(),
            DurableRuntimeError::Component {
                component: "control recovery",
                ..
            }
        ));
        drop(fatal);

        let runtime = ReadyDurableRuntime::open(root.path()).expect("ordinary open reconfirms log");
        assert_eq!(runtime.read_view().projects.len(), 1);
        assert_eq!(runtime.read_view().sessions.len(), 1);
        let (_runtime, reply) = submit_ok(runtime, request);
        assert_eq!(reply, stable_reply("command-control-recovery-sync"));
    }

    #[test]
    fn durable_backend_fault_prepared_before_rejection_stays_ready_without_pollution() {
        let root = private_root();
        let request = project_create_prepare_for(
            &format!("global-{}", "1".repeat(32)),
            "command-runtime",
            "project-runtime",
            "pre-prepare",
        );
        let expected_reply = request.prepared.stable_reply().expect("stable reply");
        let mut runtime = ReadyDurableRuntime::open(root.path()).expect("open runtime");
        runtime.set_failpoint(RuntimeFailpoint::Prepare);
        let Err(failure) = runtime.submit(request.clone()) else {
            panic!("prepare cut must fail");
        };
        let SubmitFailure::Rejected { runtime, .. } = failure else {
            panic!("pre-prepare cut stays Ready");
        };
        assert!(runtime.read_view().projects.is_empty());
        assert!(runtime.read_view().sessions.is_empty());
        drop(runtime);

        let runtime = ReadyDurableRuntime::open(root.path()).expect("clean reopen");
        assert!(runtime.read_view().projects.is_empty());
        let (_runtime, reply) = submit_ok(runtime, request);
        assert_eq!(reply, expected_reply);
    }

    #[test]
    fn durable_backend_fault_recovering_retries_only_authoritative_transaction() {
        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("open runtime");
        let runtime = seed_project_session_for(
            runtime,
            &format!("global-{}", "1".repeat(32)),
            &format!("global-{}", "2".repeat(32)),
            "recover-base",
            "project-runtime",
            "session-runtime",
            "recover",
        );
        let request = combined_mutation_prepare_for(
            &runtime,
            &format!("global-{}", "3".repeat(32)),
            "command-runtime",
            "project-runtime",
            "session-runtime",
            "recover-target",
        );
        let mut runtime = runtime;
        runtime.set_failpoint(RuntimeFailpoint::Session);
        let Err(failure) = runtime.submit(request) else {
            panic!("Session cut must fail");
        };
        let SubmitFailure::Recovering { runtime, .. } = failure else {
            panic!("must recover");
        };
        let before_retry = runtime
            .core
            .components()
            .expect("runtime components")
            .state_store
            .writer_open_counts();
        assert_eq!(before_retry.len(), 2);
        assert!(before_retry.values().all(|count| *count == 3));
        let Ok(runtime) = runtime.recover() else {
            panic!("same transaction redo must succeed");
        };
        assert_eq!(runtime.read_view().projects.len(), 1);
        assert_eq!(runtime.read_view().sessions.len(), 1);
        assert_eq!(
            runtime
                .core
                .components()
                .expect("runtime components")
                .state_store
                .writer_open_counts(),
            before_retry
        );
    }

    #[test]
    fn unknown_aggregate_directory_fails_catalog_validation() {
        let root = private_root();
        drop(ReadyDurableRuntime::open(root.path()).expect("initialize"));
        let state = StateStore::open(root.path(), StateStoreInstanceLease::for_tests())
            .expect("direct state fixture");
        let project_id = domain_project("unknown-project");
        let writer = match state
            .open_project_writer(project_id.clone())
            .expect("open unknown Project")
        {
            OpenProjectWriter::Ready(writer) => writer,
            OpenProjectWriter::RepairRequired(_) => panic!("unexpected tail"),
        };
        let request = AppendRequest {
            transaction_id: "fixture-unknown-project".to_owned(),
            command_record: None,
            events: vec![ProjectEvent::ProjectInitialized {
                project_id,
                score_id: ScoreId::parse("score-unknown").expect("score"),
                default_take_id: TakeId::parse("take-unknown").expect("take"),
                default_branch_id: BranchId::parse("branch-unknown").expect("branch"),
            }],
        };
        let Ok((_writer, _)) = writer.append(request) else {
            panic!("append unknown Project");
        };
        drop(state);
        assert!(matches!(
            ReadyDurableRuntime::open(root.path()),
            Err(DurableRuntimeError::CatalogMismatch)
        ));
    }

    #[test]
    fn transaction_closure_rejects_missing_suffix_owner_and_anchor_mismatches() {
        for with_checkpoint in [false, true] {
            for aggregate in [ClosureAggregate::Project, ClosureAggregate::Session] {
                for mutation in [
                    ClosureMutation::Missing,
                    ClosureMutation::WrongSuffix,
                    ClosureMutation::Digest,
                    ClosureMutation::Sequence,
                    ClosureMutation::Checksum,
                ] {
                    persist_closure_mutation(aggregate, mutation, with_checkpoint);
                }
                persist_wrong_owner(aggregate, with_checkpoint);
            }
        }
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "trusted、Project 伪 marker 与跨实例伪 legacy 必须分别使用真实持久根"
    )]
    fn transaction_closure_legacy_exception_requires_trusted_persisted_replay() {
        for with_checkpoint in [false, true] {
            let (trusted_root, trusted_runtime) = running_legacy_runtime();
            let trusted_transaction = append_trusted_legacy(&trusted_runtime);
            assert!(trusted_transaction.starts_with("restart-v1:"));
            if with_checkpoint {
                write_closure_checkpoints(
                    &trusted_runtime,
                    &["project-runtime"],
                    &["session-runtime"],
                );
            }
            drop(trusted_runtime);
            let trusted_runtime = ReadyDurableRuntime::open(trusted_root.path())
                .expect("trusted legacy replay must publish");
            assert_eq!(
                trusted_runtime.read_view().sessions["session-runtime"].turns[0].status,
                TurnStatus::AbortedByRestart
            );
            drop(trusted_runtime);

            let project_root = private_root();
            let project_runtime =
                ReadyDurableRuntime::open(project_root.path()).expect("open Project legacy root");
            let project_runtime = seed_project_session_for(
                project_runtime,
                &format!("global-{}", "1".repeat(32)),
                &format!("global-{}", "2".repeat(32)),
                "project-legacy",
                "project-runtime",
                "session-runtime",
                "project-legacy",
            );
            let state_store = &project_runtime
                .core
                .components()
                .expect("runtime components")
                .state_store;
            let legacy_marker =
                format!("restart-v1:{}:session-runtime:1", state_store.instance_id());
            let project_id = domain_project("project-runtime");
            let _commit = append_project_direct(
                &project_runtime,
                project_id.clone(),
                AppendRequest {
                    transaction_id: legacy_marker,
                    command_record: None,
                    events: vec![ProjectEvent::BriefRevisionCreated(CreativeBrief {
                        id: BriefRevisionId::parse("brief-project-legacy").expect("brief"),
                        project_id,
                        previous: None,
                        user_description: "Project 不得接纳 legacy marker".to_owned(),
                        goals: vec!["启动拒绝".to_owned()],
                        instrumentation: vec!["piano".to_owned()],
                        open_questions: Vec::new(),
                    })],
                },
            );
            if with_checkpoint {
                write_closure_checkpoints(
                    &project_runtime,
                    &["project-runtime"],
                    &["session-runtime"],
                );
            }
            drop(project_runtime);
            assert_runtime_transaction_conflict(project_root.path());

            let (source_root, source_runtime) = running_legacy_runtime();
            let (target_root, target_runtime) = running_legacy_runtime();
            if with_checkpoint {
                write_closure_checkpoints(
                    &target_runtime,
                    &["project-runtime"],
                    &["session-runtime"],
                );
            }
            let _source_transaction = append_trusted_legacy(&source_runtime);
            drop(source_runtime);
            drop(target_runtime);

            let source_log = std::fs::read(only_session_log(source_root.path()))
                .expect("read source Session log");
            let target_log_path = only_session_log(target_root.path());
            let target_prefix =
                std::fs::read(&target_log_path).expect("read target Session prefix");
            assert!(source_log.starts_with(&target_prefix));
            let forged_tail = &source_log[target_prefix.len()..];
            assert!(!forged_tail.is_empty());
            let mut target_log = std::fs::OpenOptions::new()
                .append(true)
                .open(target_log_path)
                .expect("open target Session log");
            target_log
                .write_all(forged_tail)
                .expect("persist forged legacy tail");
            target_log.sync_all().expect("sync forged legacy tail");
            drop(target_log);
            assert!(matches!(
                ReadyDurableRuntime::open(target_root.path()),
                Err(DurableRuntimeError::Component {
                    component: "Session startup",
                    ..
                })
            ));
        }
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "Project 与 Session 额外 global 必须以独立根分别证明启动拒绝"
    )]
    fn project_and_session_extra_global_transactions_fail_independently_on_replay() {
        for with_checkpoint in [false, true] {
            let project_root = private_root();
            let runtime =
                ReadyDurableRuntime::open(project_root.path()).expect("open Project root");
            let runtime = seed_project_session_for(
                runtime,
                &format!("global-{}", "1".repeat(32)),
                &format!("global-{}", "2".repeat(32)),
                "project-extra",
                "project-runtime",
                "session-runtime",
                "project-extra",
            );
            if with_checkpoint {
                runtime
                    .core
                    .control()
                    .expect("control")
                    .write_checkpoint()
                    .expect("control checkpoint");
            }
            let state_store = &runtime
                .core
                .components()
                .expect("runtime components")
                .state_store;
            let project_id = domain_project("project-runtime");
            let project_writer = match state_store
                .open_project_writer(project_id.clone())
                .expect("open Project")
            {
                OpenProjectWriter::Ready(writer) => writer,
                OpenProjectWriter::RepairRequired(_) => panic!("clean Project"),
            };
            let command = StoredCommandRecordV1::new(
                "client-extra",
                "command-extra-project",
                format!("sha256:{}", "8".repeat(64)),
                &stable_reply("command-extra-project"),
            )
            .expect("Project command");
            let Ok((project_writer, _)) = project_writer.append(AppendRequest {
                transaction_id: format!("global-{}:project", "8".repeat(32)),
                command_record: Some(command),
                events: vec![ProjectEvent::BriefRevisionCreated(CreativeBrief {
                    id: BriefRevisionId::parse("brief-extra-project").expect("brief"),
                    project_id,
                    previous: None,
                    user_description: "额外 Project global".to_owned(),
                    goals: vec!["启动拒绝".to_owned()],
                    instrumentation: vec!["piano".to_owned()],
                    open_questions: Vec::new(),
                })],
            }) else {
                panic!("append extra Project transaction");
            };
            if with_checkpoint {
                project_writer
                    .write_checkpoint()
                    .expect("Project checkpoint");
            }
            drop(project_writer);
            drop(runtime);
            assert!(matches!(
                ReadyDurableRuntime::open(project_root.path()),
                Err(DurableRuntimeError::TransactionConflict)
            ));

            let session_root = private_root();
            let runtime =
                ReadyDurableRuntime::open(session_root.path()).expect("open Session root");
            let runtime = seed_project_session_for(
                runtime,
                &format!("global-{}", "1".repeat(32)),
                &format!("global-{}", "2".repeat(32)),
                "session-extra",
                "project-runtime",
                "session-runtime",
                "session-extra",
            );
            if with_checkpoint {
                runtime
                    .core
                    .control()
                    .expect("control")
                    .write_checkpoint()
                    .expect("control checkpoint");
            }
            let state_store = &runtime
                .core
                .components()
                .expect("runtime components")
                .state_store;
            let session_writer = match state_store
                .open_session_writer(SessionId("session-runtime".to_owned()))
                .expect("open Session")
            {
                OpenSessionWriter::Ready(writer) => writer,
                OpenSessionWriter::RepairRequired(_) => panic!("clean Session"),
            };
            let command = StoredCommandRecordV1::new(
                "client-extra",
                "command-extra-session",
                format!("sha256:{}", "9".repeat(64)),
                &stable_reply("command-extra-session"),
            )
            .expect("Session command");
            let Ok((session_writer, _)) = session_writer.append(SessionAppendRequest::new(
                format!("global-{}:session", "9".repeat(32)),
                Some(command),
                vec![SessionRolloutEvent::TurnStarted {
                    turn_id: TurnId("turn-extra-session".to_owned()),
                    canonical_prompt: "extra Session transaction".to_owned(),
                }],
            )) else {
                panic!("append extra Session transaction");
            };
            if with_checkpoint {
                session_writer
                    .write_checkpoint()
                    .expect("Session checkpoint");
            }
            drop(session_writer);
            drop(runtime);
            assert!(matches!(
                ReadyDurableRuntime::open(session_root.path()),
                Err(DurableRuntimeError::TransactionConflict)
            ));
        }
    }

    #[test]
    fn aggregate_transactions_without_committed_control_anchors_fail_startup() {
        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("open runtime");
        let runtime = seed_project_session_for(
            runtime,
            &format!("global-{}", "1".repeat(32)),
            &format!("global-{}", "2".repeat(32)),
            "orphan-base",
            "project-runtime",
            "session-runtime",
            "orphan",
        );
        drop(runtime);

        let state = StateStore::open(root.path(), StateStoreInstanceLease::for_tests())
            .expect("direct state fixture");
        let project_id = domain_project("project-runtime");
        let project_writer = match state
            .open_project_writer(project_id.clone())
            .expect("open Project")
        {
            OpenProjectWriter::Ready(writer) => writer,
            OpenProjectWriter::RepairRequired(_) => panic!("unexpected Project tail"),
        };
        let project_command = StoredCommandRecordV1::new(
            "client-orphan",
            "command-orphan-project",
            format!("sha256:{}", "8".repeat(64)),
            &stable_reply("command-orphan-project"),
        )
        .expect("Project command");
        let project_append = project_writer.append(AppendRequest {
            transaction_id: format!("global-{}:project", "8".repeat(32)),
            command_record: Some(project_command),
            events: vec![ProjectEvent::BriefRevisionCreated(CreativeBrief {
                id: BriefRevisionId::parse("brief-orphan").expect("brief"),
                project_id,
                previous: None,
                user_description: "orphan control transaction".to_owned(),
                goals: vec!["fail closed".to_owned()],
                instrumentation: vec!["piano".to_owned()],
                open_questions: Vec::new(),
            })],
        });
        let Ok((project_writer, _)) = project_append else {
            panic!("append unanchored Project transaction");
        };
        drop(project_writer);

        let session_id = SessionId("session-runtime".to_owned());
        let session_writer = match state
            .open_session_writer(session_id.clone())
            .expect("open Session")
        {
            OpenSessionWriter::Ready(writer) => writer,
            OpenSessionWriter::RepairRequired(_) => panic!("unexpected Session tail"),
        };
        let session_command = StoredCommandRecordV1::new(
            "client-orphan",
            "command-orphan-session",
            format!("sha256:{}", "9".repeat(64)),
            &stable_reply("command-orphan-session"),
        )
        .expect("Session command");
        let session_append = session_writer.append(SessionAppendRequest::new(
            format!("global-{}:session", "9".repeat(32)),
            Some(session_command),
            vec![SessionRolloutEvent::TurnStarted {
                turn_id: TurnId("turn-orphan-session".to_owned()),
                canonical_prompt: "orphan Session transaction".to_owned(),
            }],
        ));
        let Ok((session_writer, _)) = session_append else {
            panic!("append unanchored Session transaction");
        };
        drop(session_writer);
        drop(state);

        assert!(matches!(
            ReadyDurableRuntime::open(root.path()),
            Err(DurableRuntimeError::TransactionConflict)
        ));
    }

    #[test]
    fn lock_loss_is_terminal_and_releases_root_on_drop() {
        let root = private_root();
        let request = project_create_prepare_for(
            &format!("global-{}", "1".repeat(32)),
            "command-runtime",
            "project-runtime",
            "lock-loss",
        );
        let runtime = ReadyDurableRuntime::open(root.path()).expect("open runtime");
        runtime.core.health.invalidate();
        let Err(failure) = runtime.submit(request) else {
            panic!("lock loss must fail");
        };
        let SubmitFailure::Fatal(fatal) = failure else {
            panic!("lock loss must be Fatal");
        };
        assert!(matches!(
            fatal.error(),
            DurableRuntimeError::InstanceLockLost
        ));
        drop(fatal);
        let _reopened = ReadyDurableRuntime::open(root.path()).expect("kernel lock released");
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum CrashStage {
        ControlPrepare,
        ProjectAppend,
        SessionAppend,
        ControlCommit,
    }

    impl CrashStage {
        const ALL: [Self; 4] = [
            Self::ControlPrepare,
            Self::ProjectAppend,
            Self::SessionAppend,
            Self::ControlCommit,
        ];

        const fn name(self) -> &'static str {
            match self {
                Self::ControlPrepare => "control_prepare",
                Self::ProjectAppend => "project_append",
                Self::SessionAppend => "session_append",
                Self::ControlCommit => "control_commit",
            }
        }

        fn parse(value: &str) -> Self {
            Self::ALL
                .into_iter()
                .find(|stage| stage.name() == value)
                .unwrap_or_else(|| panic!("未知 crash stage: {value}"))
        }

        const fn startup_failpoint(self) -> StartupSyncFailpoint {
            match self {
                Self::ControlPrepare | Self::ControlCommit => StartupSyncFailpoint::Control,
                Self::ProjectAppend => StartupSyncFailpoint::Project,
                Self::SessionAppend => StartupSyncFailpoint::Session,
            }
        }

        const fn expected_startup_error(self) -> (&'static str, &'static str) {
            match self {
                Self::ControlPrepare | Self::ControlCommit => (
                    "control store",
                    "control filesystem operation failed: test control log open file sync",
                ),
                Self::ProjectAppend => (
                    "Project startup",
                    "filesystem operation failed: test Project events file sync",
                ),
                Self::SessionAppend => (
                    "Session startup",
                    "filesystem operation failed: test Session rollout file sync",
                ),
            }
        }

        const fn target_log_name(self) -> &'static str {
            match self {
                Self::ControlPrepare | Self::ControlCommit => "control-v1.jsonl",
                Self::ProjectAppend => "events-v1.jsonl",
                Self::SessionAppend => "rollout-v1.jsonl",
            }
        }

        const fn checkpoint_name(self) -> &'static str {
            match self {
                Self::ControlPrepare | Self::ControlCommit => "control-checkpoint-v1.json",
                Self::ProjectAppend => "checkpoint-v1.json",
                Self::SessionAppend => "session-checkpoint-v1.json",
            }
        }
    }

    #[derive(Debug, serde::Deserialize, serde::Serialize)]
    #[serde(deny_unknown_fields)]
    struct CrashMarker {
        schema_version: u32,
        stage: String,
        baseline: String,
        checkpoint: String,
        target_log: String,
        pre_target_len: u64,
        crash_target_len: u64,
        checkpoint_path: Option<String>,
        checkpoint_covered_boundary: Option<u64>,
        checkpoint_adopted: bool,
        first_order: String,
        second_order: Option<String>,
    }

    fn target_global_tx_id(baseline: bool) -> String {
        format!("global-{}", if baseline { "6" } else { "5" }.repeat(32))
    }

    fn find_unique_named(root: &Path, target_name: &str) -> std::path::PathBuf {
        fn walk(dir: &Path, target_name: &str, matches: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("枚举 crash 场景目录") {
                let path = entry.expect("读取 crash 场景目录项").path();
                if path.is_dir() {
                    walk(&path, target_name, matches);
                } else if path.file_name().and_then(|name| name.to_str()) == Some(target_name) {
                    matches.push(path);
                }
            }
        }
        let mut matches = Vec::new();
        walk(root, target_name, &mut matches);
        assert_eq!(matches.len(), 1, "目标文件必须唯一: {target_name}");
        matches.pop().expect("唯一目标文件")
    }

    fn read_optional_unique_named(root: &Path, target_name: &str) -> Vec<u8> {
        fn walk(dir: &Path, target_name: &str, matches: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("枚举 crash 场景目录") {
                let path = entry.expect("读取 crash 场景目录项").path();
                if path.is_dir() {
                    walk(&path, target_name, matches);
                } else if path.file_name().and_then(|name| name.to_str()) == Some(target_name) {
                    matches.push(path);
                }
            }
        }
        let mut matches = Vec::new();
        walk(root, target_name, &mut matches);
        assert!(matches.len() <= 1, "目标文件不得重复: {target_name}");
        matches.pop().map_or_else(Vec::new, |path| {
            std::fs::read(path).expect("读取可选目标日志")
        })
    }

    fn relative_path(root: &Path, path: &Path) -> String {
        path.strip_prefix(root)
            .expect("目标文件必须位于场景目录")
            .to_string_lossy()
            .into_owned()
    }

    fn marker_path(root: &Path, relative: &str) -> std::path::PathBuf {
        let relative = Path::new(relative);
        assert!(
            relative
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
            "marker 相对路径必须是普通组件"
        );
        root.join(relative)
    }

    fn write_durable_marker(path: &Path, marker: &CrashMarker) {
        let parent = path.parent().expect("marker 父目录");
        let temp = parent.join(format!("c0-marker-{}.tmp", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        let mut file = options
            .write(true)
            .create_new(true)
            .open(&temp)
            .expect("创建 marker 临时文件");
        let mut bytes = serde_json::to_vec(marker).expect("编码结构化 marker");
        bytes.push(b'\n');
        file.write_all(&bytes).expect("写入结构化 marker");
        file.sync_all().expect("同步结构化 marker");
        std::fs::rename(&temp, path).expect("安装结构化 marker");
        File::open(parent)
            .expect("打开 marker 目录")
            .sync_all()
            .expect("同步 marker 目录");
    }

    fn read_crash_marker(path: &Path) -> CrashMarker {
        serde_json::from_slice(&std::fs::read(path).expect("读取结构化 marker"))
            .expect("解码结构化 marker")
    }

    fn assert_marker_identity(
        marker: &CrashMarker,
        stage: CrashStage,
        baseline: bool,
        checkpoint: bool,
    ) {
        assert_eq!(marker.schema_version, 1);
        assert_eq!(marker.stage, stage.name());
        assert_eq!(marker.baseline, if baseline { "nonempty" } else { "empty" });
        assert_eq!(
            marker.checkpoint,
            if checkpoint { "present" } else { "absent" }
        );
        assert_eq!(
            marker.first_order,
            "append_sync_failed_clean_rescan_confirmation_failed"
        );
        assert_eq!(marker.checkpoint_adopted, checkpoint);
        assert_eq!(marker.checkpoint_path.is_some(), checkpoint);
        assert_eq!(marker.checkpoint_covered_boundary.is_some(), checkpoint);
    }

    fn crash_target(
        runtime: &ReadyDurableRuntime,
        stage: CrashStage,
        baseline: bool,
    ) -> PrepareControlRequest {
        if baseline {
            return combined_mutation_prepare_for(
                runtime,
                &format!("global-{}", "6".repeat(32)),
                "command-c0-target-nonempty",
                "project-c0-crash",
                "session-c0-crash",
                "c0-target",
            );
        }
        if stage == CrashStage::SessionAppend {
            session_start_prepare_for(
                &format!("global-{}", "5".repeat(32)),
                "command-c0-target-empty",
                "project-c0-crash",
                "session-c0-crash",
            )
        } else {
            project_create_prepare_for(
                &format!("global-{}", "5".repeat(32)),
                "command-c0-target-empty",
                "project-c0-crash",
                "c0-target",
            )
        }
    }

    fn persist_prepared(runtime: &mut ReadyDurableRuntime, request: PrepareControlRequest) {
        let writer = runtime.core.take_control().expect("take control");
        let Ok((writer, _)) = writer.prepare(request) else {
            panic!("persist Prepared")
        };
        runtime.core.put_control(writer).expect("return control");
    }

    fn checkpoint_target(
        runtime: &ReadyDurableRuntime,
        root: &Path,
        stage: CrashStage,
    ) -> (String, u64) {
        match stage {
            CrashStage::ControlPrepare | CrashStage::ControlCommit => {
                runtime
                    .core
                    .control()
                    .expect("control")
                    .write_checkpoint()
                    .expect("Control checkpoint");
                crate::control_store::reset_checkpoint_load_observed();
                let reopened = open_control_writer(root, Arc::downgrade(&runtime.core.health))
                    .expect("reopen Control for checkpoint telemetry");
                assert!(matches!(reopened, OpenControlWriter::Ready(_)));
                assert!(crate::control_store::checkpoint_load_observed());
            }
            CrashStage::ProjectAppend => {
                let store = &runtime.core.components().expect("components").state_store;
                let writer = match store
                    .open_project_writer(domain_project("project-c0-crash"))
                    .expect("Project")
                {
                    OpenProjectWriter::Ready(writer) => writer,
                    OpenProjectWriter::RepairRequired(_) => panic!("clean Project"),
                };
                writer.write_checkpoint().expect("Project checkpoint");
                drop(writer);
                crate::state_store::reset_project_checkpoint_load_observed();
                let reopened = store
                    .open_project_writer(domain_project("project-c0-crash"))
                    .expect("reopen Project");
                assert!(matches!(reopened, OpenProjectWriter::Ready(_)));
                assert!(crate::state_store::project_checkpoint_load_observed());
            }
            CrashStage::SessionAppend => {
                let store = &runtime.core.components().expect("components").state_store;
                let writer = match store
                    .open_session_writer(SessionId("session-c0-crash".to_owned()))
                    .expect("Session")
                {
                    OpenSessionWriter::Ready(writer) => writer,
                    OpenSessionWriter::RepairRequired(_) => panic!("clean Session"),
                };
                writer.write_checkpoint().expect("Session checkpoint");
                drop(writer);
                crate::state_store::session::reset_checkpoint_load_observed();
                let reopened = store
                    .open_session_writer(SessionId("session-c0-crash".to_owned()))
                    .expect("reopen Session");
                assert!(matches!(reopened, OpenSessionWriter::Ready(_)));
                assert!(crate::state_store::session::checkpoint_load_observed());
            }
        }
        let target_log = find_unique_named(root, stage.target_log_name());
        let checkpoint = target_log
            .parent()
            .expect("目标日志父目录")
            .join(stage.checkpoint_name());
        assert!(checkpoint.is_file(), "目标 checkpoint 必须存在");
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&checkpoint).expect("读取目标 checkpoint"))
                .expect("解码目标 checkpoint");
        let covered = value["covered_valid_bytes"]
            .as_u64()
            .expect("checkpoint covered_valid_bytes");
        (relative_path(root, &checkpoint), covered)
    }

    fn assert_target_boundary(
        stage: CrashStage,
        baseline: bool,
        pre_target: &[u8],
        crash_target: &[u8],
    ) {
        let newline_count =
            |bytes: &[u8]| bytes.split(|byte| *byte == b'\n').count().saturating_sub(1);
        match (stage, baseline) {
            (CrashStage::ControlCommit, false) => {
                assert_eq!(newline_count(pre_target), 1);
            }
            (CrashStage::ControlCommit, true) => {
                assert_eq!(newline_count(pre_target), 5);
            }
            (_, false) => assert!(pre_target.is_empty()),
            (_, true) => assert!(!pre_target.is_empty()),
        }
        assert!(crash_target.starts_with(pre_target));
        let appended = &crash_target[pre_target.len()..];
        assert!(!appended.is_empty());
        assert!(appended.ends_with(b"\n"));
        assert_eq!(newline_count(appended), 1);
        let line: serde_json::Value =
            serde_json::from_slice(&appended[..appended.len() - 1]).expect("解码目标完整行");
        let checksum = line["batch_checksum"]
            .as_str()
            .expect("目标行必须包含 batch checksum");
        let checksum = checksum
            .strip_prefix("sha256:")
            .expect("目标行 checksum 必须使用 sha256");
        assert_eq!(checksum.len(), 64);
        assert!(checksum.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    struct TargetBoundary {
        log: std::path::PathBuf,
        pre_bytes: Vec<u8>,
        checkpoint_path: Option<String>,
        checkpoint_covered_boundary: Option<u64>,
    }

    fn prepare_target_boundary(
        runtime: &ReadyDurableRuntime,
        root: &Path,
        stage: CrashStage,
        checkpoint: bool,
    ) -> TargetBoundary {
        let checkpoint_metadata = checkpoint.then(|| checkpoint_target(runtime, root, stage));
        let log = find_unique_named(root, stage.target_log_name());
        let pre_bytes = std::fs::read(&log).expect("读取目标日志旧边界");
        let checkpoint_file = log
            .parent()
            .expect("目标日志父目录")
            .join(stage.checkpoint_name());
        if let Some((_, covered)) = &checkpoint_metadata {
            assert_eq!(
                *covered,
                u64::try_from(pre_bytes.len()).expect("目标日志长度适配 u64")
            );
        } else {
            assert!(
                !checkpoint_file.exists(),
                "Absent 场景不得存在目标 checkpoint"
            );
        }
        TargetBoundary {
            log,
            pre_bytes,
            checkpoint_path: checkpoint_metadata
                .as_ref()
                .map(|metadata| metadata.0.clone()),
            checkpoint_covered_boundary: checkpoint_metadata.map(|metadata| metadata.1),
        }
    }

    #[allow(clippy::too_many_lines, reason = "四个切点共享同一严格子进程阶段协议")]
    fn seed_second_order_crash(
        root: &Path,
        marker_path: &Path,
        stage: CrashStage,
        baseline: bool,
        checkpoint: bool,
    ) -> ! {
        let mut runtime = ReadyDurableRuntime::open(root).expect("seed open");
        if baseline {
            runtime = seed_project_session_for(
                runtime,
                &format!("global-{}", "4".repeat(32)),
                &format!("global-{}", "5".repeat(32)),
                "c0-baseline",
                "project-c0-crash",
                "session-c0-crash",
                "c0-baseline",
            );
        } else if stage == CrashStage::SessionAppend {
            (runtime, _) = submit_ok(
                runtime,
                project_create_prepare_for(
                    &format!("global-{}", "4".repeat(32)),
                    "command-c0-empty-project",
                    "project-c0-crash",
                    "c0-empty",
                ),
            );
        }
        let request = crash_target(&runtime, stage, baseline);
        let prepared = request.prepared.clone();
        let boundary;
        match stage {
            CrashStage::ControlPrepare => {
                boundary = prepare_target_boundary(&runtime, root, stage, checkpoint);
                let mut writer = runtime.core.take_control().expect("control");
                writer.set_failpoint(ControlAppendFailpoint::FileSync);
                let Err(crate::control_store::ControlAppendFailure::Poisoned {
                    mut writer, ..
                }) = writer.prepare(request)
                else {
                    panic!("prepare must poison")
                };
                writer.set_recovery_failpoint(ControlRecoveryFailpoint::FileSync);
                assert!(matches!(
                    writer.recover(),
                    ControlRecoveryOutcome::Corrupt(_)
                ));
            }
            CrashStage::ProjectAppend => {
                persist_prepared(&mut runtime, request);
                let store = &runtime.core.components().expect("components").state_store;
                let plan = prepared.project_plan.expect("Project plan");
                let project_id = plan.project_id().expect("ID");
                let writer = match store
                    .open_project_writer(project_id.clone())
                    .expect("Project")
                {
                    OpenProjectWriter::Ready(writer) => writer,
                    OpenProjectWriter::RepairRequired(_) => panic!("clean"),
                };
                drop(writer);
                boundary = prepare_target_boundary(&runtime, root, stage, checkpoint);
                let mut writer = match store.open_project_writer(project_id).expect("Project") {
                    OpenProjectWriter::Ready(writer) => writer,
                    OpenProjectWriter::RepairRequired(_) => panic!("clean"),
                };
                writer.set_failpoint(AppendFailpoint::FileSyncError);
                let Err(AppendFailure::Poisoned { mut writer, .. }) =
                    writer.append(plan.into_append_request(Vec::new()).expect("request"))
                else {
                    panic!("Project must poison")
                };
                writer.set_recovery_failpoint(ProjectRecoveryFailpoint::FileSync);
                assert!(matches!(writer.recover(), RecoveryOutcome::Corrupt(_)));
            }
            CrashStage::SessionAppend => {
                persist_prepared(&mut runtime, request);
                if let Some(project_plan) = prepared.project_plan.clone() {
                    append_project_direct(
                        &runtime,
                        project_plan.project_id().expect("ID"),
                        project_plan
                            .into_append_request(Vec::new())
                            .expect("request"),
                    );
                }
                let store = &runtime.core.components().expect("components").state_store;
                let plan = prepared.session_plan.expect("Session plan");
                let session_id = plan.session_id().expect("ID");
                let writer = match store
                    .open_session_writer(session_id.clone())
                    .expect("Session")
                {
                    OpenSessionWriter::Ready(writer) => writer,
                    OpenSessionWriter::RepairRequired(_) => panic!("clean"),
                };
                drop(writer);
                boundary = prepare_target_boundary(&runtime, root, stage, checkpoint);
                let mut writer = match store.open_session_writer(session_id).expect("Session") {
                    OpenSessionWriter::Ready(writer) => writer,
                    OpenSessionWriter::RepairRequired(_) => panic!("clean"),
                };
                writer.set_failpoint(AppendFailpoint::FileSyncError);
                let Err(SessionAppendFailure::Poisoned { mut writer, .. }) =
                    writer.append(plan.into_append_request().expect("request"))
                else {
                    panic!("Session must poison")
                };
                writer.set_recovery_failpoint(SessionRecoveryFailpoint::FileSync);
                assert!(matches!(
                    writer.recover(),
                    SessionRecoveryOutcome::Corrupt(_)
                ));
            }
            CrashStage::ControlCommit => {
                persist_prepared(&mut runtime, request);
                let project_last = prepared.project_plan.clone().map(|project_plan| {
                    append_project_direct(
                        &runtime,
                        project_plan.project_id().expect("ID"),
                        project_plan
                            .into_append_request(Vec::new())
                            .expect("request"),
                    )
                });
                let session_last = prepared.session_plan.clone().map(|session_plan| {
                    append_session_direct(
                        &runtime,
                        session_plan.session_id().expect("ID"),
                        session_plan.into_append_request().expect("request"),
                    )
                });
                boundary = prepare_target_boundary(&runtime, root, stage, checkpoint);
                let mut writer = runtime.core.take_control().expect("control");
                writer.set_failpoint(ControlAppendFailpoint::FileSync);
                let Err(crate::control_store::ControlAppendFailure::Poisoned {
                    mut writer, ..
                }) = writer.commit(CommitControlRequest {
                    global_tx_id: prepared.global_tx_id,
                    project_last: project_last.map(Into::into),
                    session_last: session_last.map(Into::into),
                })
                else {
                    panic!("commit must poison")
                };
                writer.set_recovery_failpoint(ControlRecoveryFailpoint::FileSync);
                assert!(matches!(
                    writer.recover(),
                    ControlRecoveryOutcome::Corrupt(_)
                ));
            }
        }
        let crash_target = std::fs::read(&boundary.log).expect("读取 crash 后目标日志");
        assert_target_boundary(stage, baseline, &boundary.pre_bytes, &crash_target);
        let marker = CrashMarker {
            schema_version: 1,
            stage: stage.name().to_owned(),
            baseline: if baseline { "nonempty" } else { "empty" }.to_owned(),
            checkpoint: if checkpoint { "present" } else { "absent" }.to_owned(),
            target_log: relative_path(root, &boundary.log),
            pre_target_len: u64::try_from(boundary.pre_bytes.len()).expect("旧边界适配 u64"),
            crash_target_len: u64::try_from(crash_target.len()).expect("新边界适配 u64"),
            checkpoint_path: boundary.checkpoint_path,
            checkpoint_covered_boundary: boundary.checkpoint_covered_boundary,
            checkpoint_adopted: checkpoint,
            first_order: "append_sync_failed_clean_rescan_confirmation_failed".to_owned(),
            second_order: None,
        };
        write_durable_marker(marker_path, &marker);
        std::process::exit(86)
    }

    fn authoritative_snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
            for entry in std::fs::read_dir(dir).expect("read data root") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    walk(root, &path, out);
                    continue;
                }
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                let relative = path.strip_prefix(root).expect("relative");
                if name == "control-v1.jsonl"
                    || name == "events-v1.jsonl"
                    || name == "rollout-v1.jsonl"
                    || relative
                        .components()
                        .next()
                        .is_some_and(|component| component.as_os_str() == "artifacts-v1")
                {
                    out.insert(
                        relative.to_string_lossy().into_owned(),
                        std::fs::read(path).expect("read log"),
                    );
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, root, &mut out);
        out
    }

    fn line_transaction_count(bytes: &[u8], transaction_id: &str) -> usize {
        bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<serde_json::Value>(line).expect("解码权威日志行"))
            .filter(|line| line["transaction_id"].as_str() == Some(transaction_id))
            .count()
    }

    fn committed_count(bytes: &[u8], global_tx_id: &str) -> usize {
        bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<serde_json::Value>(line).expect("解码 Control 行"))
            .flat_map(|line| line["facts"].as_array().cloned().unwrap_or_default())
            .filter(|fact| {
                fact["type"].as_str() == Some("command_committed_v1")
                    && fact["value"]["global_tx_id"].as_str() == Some(global_tx_id)
            })
            .count()
    }

    fn assert_stage_boundaries(root: &Path, stage: CrashStage, baseline: bool) {
        let global_tx_id = target_global_tx_id(baseline);
        let project_tx = project_transaction_id(&global_tx_id);
        let session_tx = session_transaction_id(&global_tx_id);
        let control = std::fs::read(find_unique_named(root, "control-v1.jsonl"))
            .expect("读取 Control 权威日志");
        let project = read_optional_unique_named(root, "events-v1.jsonl");
        let session = read_optional_unique_named(root, "rollout-v1.jsonl");
        let project_count = line_transaction_count(&project, &project_tx);
        let session_count = line_transaction_count(&session, &session_tx);
        let commit_count = committed_count(&control, &global_tx_id);
        match stage {
            CrashStage::ControlPrepare => {
                assert_eq!(project_count, 0);
                assert_eq!(session_count, 0);
                assert_eq!(commit_count, 0);
            }
            CrashStage::ProjectAppend => {
                assert_eq!(project_count, 1);
                assert_eq!(session_count, 0);
                assert_eq!(commit_count, 0);
            }
            CrashStage::SessionAppend => {
                assert_eq!(project_count, usize::from(baseline));
                assert_eq!(session_count, 1);
                assert_eq!(commit_count, 0);
            }
            CrashStage::ControlCommit => {
                assert_eq!(project_count, 1);
                assert_eq!(session_count, usize::from(baseline));
                assert_eq!(commit_count, 1);
            }
        }
    }

    fn spawn_c0_child(mut command: std::process::Command) -> std::process::Child {
        let gate = test_process_spawn_gate();
        let mut state = gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while state.live_instance_locks != 0 || state.spawning {
            state = gate
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        state.spawning = true;
        drop(state);
        let child = command.spawn();
        let mut state = gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.spawning = false;
        gate.changed.notify_all();
        drop(state);
        child.expect("spawn child")
    }

    fn assert_marker_files(root: &Path, stage: CrashStage, marker: &CrashMarker) {
        let target = marker_path(root, &marker.target_log);
        assert_eq!(
            std::fs::metadata(&target)
                .expect("检查 marker 目标日志")
                .len(),
            marker.crash_target_len
        );
        assert!(marker.crash_target_len > marker.pre_target_len);
        if let (Some(checkpoint), Some(covered)) = (
            marker.checkpoint_path.as_deref(),
            marker.checkpoint_covered_boundary,
        ) {
            let checkpoint = marker_path(root, checkpoint);
            assert_eq!(
                checkpoint.file_name().and_then(|name| name.to_str()),
                Some(stage.checkpoint_name())
            );
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(checkpoint).expect("读取 marker checkpoint"))
                    .expect("解码 marker checkpoint");
            assert_eq!(value["covered_valid_bytes"].as_u64(), Some(covered));
            assert_eq!(covered, marker.pre_target_len);
        }
    }

    fn write_publish_sentinel(path: &Path) {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .expect("创建 publish sentinel");
        file.write_all(b"published\n").expect("写 publish sentinel");
        file.sync_all().expect("同步 publish sentinel");
        File::open(path.parent().expect("sentinel 父目录"))
            .expect("打开 sentinel 目录")
            .sync_all()
            .expect("同步 sentinel 目录");
    }

    fn ordinary_confirm_fail(
        root: &Path,
        marker_path: &Path,
        publish_sentinel: &Path,
        stage: CrashStage,
        baseline: bool,
        checkpoint: bool,
    ) {
        let mut marker = read_crash_marker(marker_path);
        assert_marker_identity(&marker, stage, baseline, checkpoint);
        assert_marker_files(root, stage, &marker);
        assert!(marker.second_order.is_none());
        let (expected_component, expected_detail) = stage.expected_startup_error();
        match ReadyDurableRuntime::open_with_startup_sync_failpoint(root, stage.startup_failpoint())
        {
            Err(DurableRuntimeError::Component { component, detail }) => {
                assert_eq!(component, expected_component);
                assert_eq!(detail, expected_detail);
            }
            Err(error) => panic!("ordinary-open 命中了错误边界: {error}"),
            Ok(_) => {
                write_publish_sentinel(publish_sentinel);
                panic!("ordinary-open 不得发布 read view");
            }
        }
        marker.second_order = Some("ordinary_open_confirmation_failed_exact_stage".to_owned());
        write_durable_marker(marker_path, &marker);
    }

    fn wait_child(mut child: std::process::Child, expected: Option<i32>, label: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().expect("wait child") {
                assert_eq!(status.code(), expected, "{label}");
                return;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().expect("kill timeout");
                child.wait().expect("reap timeout");
                panic!("{label}: 子进程超时");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn cross_process_second_order_crash_matrix() {
        if let Ok(phase) = std::env::var("ALDA_C0_CHILD_PHASE") {
            let stage = CrashStage::parse(&std::env::var("ALDA_C0_STAGE").expect("stage"));
            let root = std::path::PathBuf::from(std::env::var("ALDA_C0_DATA_ROOT").expect("root"));
            let marker = std::path::PathBuf::from(std::env::var("ALDA_C0_MARKER").expect("marker"));
            let baseline = match std::env::var("ALDA_C0_BASELINE")
                .expect("baseline")
                .as_str()
            {
                "empty" => false,
                "nonempty" => true,
                value => panic!("未知 baseline: {value}"),
            };
            let checkpoint = match std::env::var("ALDA_C0_CHECKPOINT")
                .expect("checkpoint")
                .as_str()
            {
                "absent" => false,
                "present" => true,
                value => panic!("未知 checkpoint: {value}"),
            };
            match phase.as_str() {
                "seed_crash" => {
                    seed_second_order_crash(&root, &marker, stage, baseline, checkpoint)
                }
                "ordinary_confirm_fail" => {
                    let publish_sentinel = std::path::PathBuf::from(
                        std::env::var("ALDA_C0_PUBLISH_MARKER").expect("publish marker"),
                    );
                    ordinary_confirm_fail(
                        &root,
                        &marker,
                        &publish_sentinel,
                        stage,
                        baseline,
                        checkpoint,
                    );
                    return;
                }
                value => panic!("未知 child phase: {value}"),
            }
        }
        let exe = std::env::current_exe().expect("test executable");
        let mut passed = 0;
        for stage in CrashStage::ALL {
            for (baseline_name, baseline) in [("empty", false), ("nonempty", true)] {
                for checkpoint_name in ["absent", "present"] {
                    let checkpoint = checkpoint_name == "present";
                    let root = private_root();
                    let marker = root.path().join("c0-marker");
                    let publish_sentinel = root.path().join("c0-published");
                    let label = format!("{}/{baseline_name}/{checkpoint_name}", stage.name());
                    let spawn = |phase: &str| {
                        let mut command = std::process::Command::new(&exe);
                        command
                            .arg("--exact")
                            .arg("durable_runtime::tests::cross_process_second_order_crash_matrix")
                            .arg("--nocapture")
                            .arg("--test-threads=1")
                            .env("ALDA_C0_CHILD_PHASE", phase)
                            .env("ALDA_C0_STAGE", stage.name())
                            .env("ALDA_C0_BASELINE", baseline_name)
                            .env("ALDA_C0_CHECKPOINT", checkpoint_name)
                            .env("ALDA_C0_DATA_ROOT", root.path())
                            .env("ALDA_C0_MARKER", &marker)
                            .env("ALDA_C0_PUBLISH_MARKER", &publish_sentinel);
                        spawn_c0_child(command)
                    };
                    wait_child(spawn("seed_crash"), Some(86), &label);
                    assert!(marker.is_file(), "{label}");
                    let first_marker = read_crash_marker(&marker);
                    assert_marker_identity(&first_marker, stage, baseline, checkpoint);
                    assert!(first_marker.second_order.is_none());
                    assert_marker_files(root.path(), stage, &first_marker);
                    assert_stage_boundaries(root.path(), stage, baseline);
                    assert!(!publish_sentinel.exists(), "{label}");
                    let before = authoritative_snapshot(root.path());
                    wait_child(spawn("ordinary_confirm_fail"), Some(0), &label);
                    assert_eq!(authoritative_snapshot(root.path()), before, "{label}");
                    let second_marker = read_crash_marker(&marker);
                    assert_marker_identity(&second_marker, stage, baseline, checkpoint);
                    assert_eq!(
                        second_marker.second_order.as_deref(),
                        Some("ordinary_open_confirmation_failed_exact_stage")
                    );
                    assert_marker_files(root.path(), stage, &second_marker);
                    assert_stage_boundaries(root.path(), stage, baseline);
                    assert!(!publish_sentinel.exists(), "{label}");
                    passed += 1;
                }
            }
        }
        println!("c0 second-order crash matrix: {passed}/16 passed");
        assert_eq!(passed, 16);
    }
}
