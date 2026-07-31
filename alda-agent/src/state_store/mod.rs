//! Linux-only B3a Project transaction log.
//!
//! Stored codecs are deliberately separate from live domain capabilities.
#![allow(
    dead_code,
    reason = "B3a freezes the store API before B4 production integration"
)]
#![allow(
    clippy::missing_errors_doc,
    reason = "public codec/store methods return the typed StateStoreError contract"
)]
#![allow(
    clippy::large_enum_variant,
    clippy::manual_let_else,
    clippy::match_same_arms,
    clippy::match_wild_err_arm,
    clippy::result_large_err,
    clippy::too_many_lines,
    reason = "B3a typestates return ownership in every outcome and keep audit flows linear"
)]

mod project_codec;
pub(crate) mod session;

use std::collections::{BTreeMap, HashSet};
use std::ffi::CStr;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path};
use std::str;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use rand::RngCore;
use rustix::fs::{
    CWD, Dir, Mode, OFlags, RenameFlags, fstat, fsync, mkdirat, openat, renameat, renameat_with,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::domain::{DomainProjectId, ProjectEvent, SchemaVersion, SequencedProjectEvent};
use crate::protocol::CommandReply;
use crate::state::{ProjectSnapshot, replay};

use self::project_codec::StoredProjectEventV1;
pub(crate) use self::project_codec::{
    RecoveredArtifactProjectHandoff, StoredProjectPlanV1, recover_artifact_for_project_plan,
};
pub(crate) use self::session::StoredSessionPlanV1;

const STATE_LAYOUT: &str = "state-v1";
const STATE_MANIFEST: &str = "state-manifest-v1.json";
const EVENTS_FILE: &str = "events-v1.jsonl";
const CHECKPOINT_FILE: &str = "checkpoint-v1.json";
const MAX_LINE_BYTES: usize = 1024 * 1024;
const MAX_EVENTS: usize = 256;
const MAX_REPLY_BYTES: usize = 64 * 1024;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_CHECKPOINT_BYTES: u64 = 1024 * 1024;
const MAX_PROJECTS: usize = 100_000;
const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const FILE_MODE: Mode = Mode::from_raw_mode(0o600);
const DIRECTORY_MODE: Mode = Mode::from_raw_mode(0o700);

#[derive(Debug, Error)]
pub enum StateStoreError {
    #[error("unsafe state root")]
    UnsafeRoot,
    #[error("a Project writer lease is required or already held")]
    WriterLockRequired,
    #[error("Project stream identity does not match")]
    StreamMismatch,
    #[error("Project event sequence does not match")]
    SequenceMismatch,
    #[error("stored batch exceeds limits")]
    BatchTooLarge,
    #[error("stable reply exceeds limits")]
    ReplyTooLarge,
    #[error("batch checksum mismatch")]
    ChecksumMismatch,
    #[error("batch checksum chain mismatch")]
    ChecksumChainMismatch,
    #[error("committed-area corruption")]
    MiddleCorruption,
    #[error("incomplete final tail requires repair")]
    RecoverableIncompleteTail {
        valid_bytes: u64,
        damaged_bytes: u64,
    },
    #[error("stored schema is incompatible")]
    IncompatibleSchema,
    #[error("stored command ID has a different payload")]
    IdempotencyConflict,
    #[error("Project reducer rejected stored facts")]
    ProjectionRejected,
    #[error("recovered Artifact capability does not match the stored Project plan")]
    ArtifactRecoveryRejected,
    #[error("Project writer is poisoned")]
    WriterPoisoned,
    #[error("filesystem operation failed: {operation}")]
    Io {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StateManifestBody {
    schema_version: u32,
    layout_version: u32,
    instance_id: String,
    durability: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StateManifest {
    body: StateManifestBody,
    checksum: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StoredCommandRecordV1 {
    pub client_id: String,
    pub client_command_id: String,
    pub payload_digest: String,
    pub stable_reply_protocol_version: u32,
    pub stable_reply_raw_len: u64,
    pub stable_reply_base64: String,
}

impl StoredCommandRecordV1 {
    /// Constructs and validates a byte-exact stable command reply record.
    pub fn new(
        client_id: impl Into<String>,
        client_command_id: impl Into<String>,
        payload_digest: impl Into<String>,
        raw_reply: &[u8],
    ) -> Result<Self, StateStoreError> {
        if raw_reply.len() > MAX_REPLY_BYTES {
            return Err(StateStoreError::ReplyTooLarge);
        }
        let client_id = client_id.into();
        let client_command_id = client_command_id.into();
        let reply: CommandReply =
            serde_json::from_slice(raw_reply).map_err(|_| StateStoreError::IncompatibleSchema)?;
        if reply.client_command_id.0 != client_command_id {
            return Err(StateStoreError::IncompatibleSchema);
        }
        let canonical =
            serde_json::to_vec(&reply).map_err(|_| StateStoreError::IncompatibleSchema)?;
        if canonical != raw_reply {
            return Err(StateStoreError::IncompatibleSchema);
        }
        let payload_digest = payload_digest.into();
        validate_sha256(&payload_digest)?;
        Ok(Self {
            client_id,
            client_command_id,
            payload_digest,
            stable_reply_protocol_version: reply.protocol_version,
            stable_reply_raw_len: u64::try_from(raw_reply.len())
                .map_err(|_| StateStoreError::ReplyTooLarge)?,
            stable_reply_base64: BASE64_STANDARD.encode(raw_reply),
        })
    }

    pub(crate) fn decode_reply(&self) -> Result<Vec<u8>, StateStoreError> {
        let bytes = BASE64_STANDARD
            .decode(&self.stable_reply_base64)
            .map_err(|_| StateStoreError::IncompatibleSchema)?;
        if bytes.len() > MAX_REPLY_BYTES
            || u64::try_from(bytes.len()).map_err(|_| StateStoreError::ReplyTooLarge)?
                != self.stable_reply_raw_len
        {
            return Err(StateStoreError::ReplyTooLarge);
        }
        let reply: CommandReply =
            serde_json::from_slice(&bytes).map_err(|_| StateStoreError::IncompatibleSchema)?;
        if reply.protocol_version != self.stable_reply_protocol_version
            || reply.client_command_id.0 != self.client_command_id
            || serde_json::to_vec(&reply).map_err(|_| StateStoreError::IncompatibleSchema)? != bytes
        {
            return Err(StateStoreError::IncompatibleSchema);
        }
        validate_sha256(&self.payload_digest)?;
        Ok(bytes)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredProjectBatchV1 {
    schema_version: u32,
    project_id: String,
    stream_id: String,
    epoch: u64,
    transaction_id: String,
    first_sequence: u64,
    last_sequence: u64,
    command_record: Option<StoredCommandRecordV1>,
    events: Vec<StoredProjectEventV1>,
    previous_batch_checksum: Option<String>,
    batch_checksum: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredTransactionCommitV1 {
    transaction_id: String,
    canonical_plan_digest: String,
    resulting_last_sequence: u64,
    resulting_batch_checksum: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredCheckpointV1 {
    schema_version: u32,
    projection_schema_version: u32,
    project_id: String,
    stream_id: String,
    epoch: u64,
    covered_sequence: u64,
    covered_batch_checksum: Option<String>,
    covered_valid_bytes: u64,
    projection_digest: String,
    projection: serde_json::Value,
    events: Vec<StoredProjectEventV1>,
    command_index: Vec<StoredCommandRecordV1>,
    transaction_index: Vec<StoredTransactionCommitV1>,
    checksum: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppendOutcome {
    pub last_sequence: u64,
    pub stable_reply: Option<Vec<u8>>,
    pub appended: bool,
}

pub(crate) struct AppendRequest {
    pub transaction_id: String,
    pub command_record: Option<StoredCommandRecordV1>,
    pub events: Vec<ProjectEvent>,
}

impl AppendRequest {
    pub(crate) fn canonical_plan_digest(
        &self,
        project_id: &DomainProjectId,
    ) -> Result<String, StateStoreError> {
        let events = self
            .events
            .iter()
            .map(StoredProjectEventV1::from_domain)
            .collect::<Vec<_>>();
        project_plan_digest(
            project_id,
            &self.transaction_id,
            self.command_record.as_ref(),
            &events,
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TransactionCommit {
    pub canonical_plan_digest: String,
    pub resulting_last_sequence: u64,
    pub resulting_batch_checksum: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TransactionProbe {
    Absent,
    SamePlanCommitted(TransactionCommit),
    ConflictingPlan,
}

#[derive(Clone, Debug)]
struct RecoveredState {
    project_id: DomainProjectId,
    stream_id: String,
    last_sequence: u64,
    last_checksum: Option<String>,
    projection: ProjectSnapshot,
    events: Vec<SequencedProjectEvent>,
    commands: BTreeMap<(String, String), StoredCommandRecordV1>,
    transactions: BTreeMap<String, TransactionCommit>,
    valid_bytes: u64,
}

struct StoreInner {
    projects: OwnedFd,
    sessions: OwnedFd,
    instance_id: String,
    project_registry: Mutex<HashSet<String>>,
    session_registry: Mutex<HashSet<String>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InitFailpoint {
    LayoutCreate,
    LayoutChildSync,
    LayoutParentSync,
    ManifestTempCreate,
    ManifestWrite,
    ManifestFileSync,
    ManifestInstall,
    ManifestDirectorySync,
    ProjectsCreate,
    ProjectsChildSync,
    ProjectsParentSync,
    SessionsCreate,
    SessionsChildSync,
    SessionsParentSync,
    ProjectDirectoryCreate,
    ProjectDirectoryChildSync,
    ProjectDirectoryParentSync,
    SessionDirectoryCreate,
    SessionDirectoryChildSync,
    SessionDirectoryParentSync,
    EventsCreate,
    EventsFileSync,
    EventsDirectorySync,
    RolloutCreate,
    RolloutFileSync,
    RolloutDirectorySync,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirectoryKind {
    Layout,
    Projects,
    Sessions,
    Project,
    Session,
}

/// Sealed B4 instance-lock capability. B3a only mints it in tests.
pub struct StateStoreInstanceLease {
    _private: (),
}

impl StateStoreInstanceLease {
    /// Mints the sealed lease at the durable composition root after it has
    /// acquired the process-wide instance lock.
    pub(crate) const fn for_durable_runtime() -> Self {
        Self { _private: () }
    }

    #[cfg(test)]
    pub(crate) const fn for_tests() -> Self {
        Self { _private: () }
    }
}

pub struct StateStore {
    _root: OwnedFd,
    _layout: OwnedFd,
    _instance_lease: StateStoreInstanceLease,
    inner: Arc<StoreInner>,
    instance_id: String,
    init_failpoint: Option<InitFailpoint>,
}

impl StateStore {
    /// Opens the Linux state layout under an already-existing private root.
    pub fn open(
        root: &Path,
        instance_lease: StateStoreInstanceLease,
    ) -> Result<Self, StateStoreError> {
        Self::open_inner(root, instance_lease, None)
    }

    fn open_inner(
        root: &Path,
        instance_lease: StateStoreInstanceLease,
        init_failpoint: Option<InitFailpoint>,
    ) -> Result<Self, StateStoreError> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (root, instance_lease, init_failpoint);
            return Err(StateStoreError::UnsafeRoot);
        }
        #[cfg(target_os = "linux")]
        {
            let root = open_absolute_directory(root)?;
            validate_directory(&root, true)?;
            let layout =
                ensure_directory(&root, STATE_LAYOUT, DirectoryKind::Layout, init_failpoint)?;
            let manifest = load_or_create_manifest(&layout, init_failpoint)?;
            let projects =
                ensure_directory(&layout, "projects", DirectoryKind::Projects, init_failpoint)?;
            let sessions =
                ensure_directory(&layout, "sessions", DirectoryKind::Sessions, init_failpoint)?;
            Ok(Self {
                _root: root,
                _layout: layout,
                _instance_lease: instance_lease,
                inner: Arc::new(StoreInner {
                    projects,
                    sessions,
                    instance_id: manifest.body.instance_id.clone(),
                    project_registry: Mutex::new(HashSet::new()),
                    session_registry: Mutex::new(HashSet::new()),
                }),
                instance_id: manifest.body.instance_id,
                init_failpoint,
            })
        }
    }

    #[cfg(test)]
    fn open_with_failpoint(
        root: &Path,
        instance_lease: StateStoreInstanceLease,
        failpoint: InitFailpoint,
    ) -> Result<Self, StateStoreError> {
        Self::open_inner(root, instance_lease, Some(failpoint))
    }

    #[must_use]
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub(crate) fn open_project_writer(
        &self,
        project_id: DomainProjectId,
    ) -> Result<OpenProjectWriter, StateStoreError> {
        let key = project_key(&project_id);
        {
            let mut registry = self
                .inner
                .project_registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !registry.insert(key.clone()) {
                return Err(StateStoreError::WriterLockRequired);
            }
        }
        let lease = ProjectWriterLease {
            inner: Arc::clone(&self.inner),
            key: key.clone(),
        };
        match open_writer_with_lease(lease, project_id, &key, self.init_failpoint) {
            Ok(writer) => Ok(writer),
            Err((lease, error)) => {
                drop(lease);
                Err(error)
            }
        }
    }

    /// Rebuilds the Project catalog from descriptor-relative, replayed facts.
    ///
    /// Directory names are treated only as hashes. The canonical Project
    /// identity is recovered from the first committed batch and then hashed
    /// again, so a renamed, empty, weak, or special entry fails closed.
    pub(crate) fn list_projects(&self) -> Result<ProjectCatalog, StateStoreError> {
        let mut directory = Dir::read_from(&self.inner.projects)
            .map_err(|source| io_error("list projects", source))?;
        let mut catalog = ProjectCatalog::default();
        for entry in &mut directory {
            let entry = entry.map_err(|source| io_error("read projects entry", source))?;
            let name = entry.file_name();
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            let key = canonical_project_directory_name(name)?;
            if catalog.projects.len() >= MAX_PROJECTS {
                return Err(StateStoreError::BatchTooLarge);
            }
            let project_dir = open_project_directory(&self.inner.projects, &key)?;
            let mut file = open_events_read(&project_dir)?;
            let expected = discover_project_id(&mut file)?;
            if project_key(&expected) != key {
                return Err(StateStoreError::StreamMismatch);
            }
            let state = match load_checkpoint(&project_dir, &mut file, &expected) {
                Ok(Some(state)) => match scan_log_from(&mut file, state)? {
                    ScanOutcome::Clean(state) => state,
                    ScanOutcome::Incomplete {
                        state,
                        damaged_bytes,
                        ..
                    } => {
                        return Err(StateStoreError::RecoverableIncompleteTail {
                            valid_bytes: state.valid_bytes,
                            damaged_bytes,
                        });
                    }
                },
                Ok(None) | Err(_) => match scan_log(&mut file, &expected)? {
                    ScanOutcome::Clean(state) => state,
                    ScanOutcome::Incomplete {
                        state,
                        damaged_bytes,
                        ..
                    } => {
                        return Err(StateStoreError::RecoverableIncompleteTail {
                            valid_bytes: state.valid_bytes,
                            damaged_bytes,
                        });
                    }
                },
            };
            if catalog
                .projects
                .insert(expected.as_str().to_owned(), state.projection)
                .is_some()
            {
                return Err(StateStoreError::ProjectionRejected);
            }
        }
        Ok(catalog)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProjectCatalog {
    pub projects: BTreeMap<String, ProjectSnapshot>,
}

struct ProjectWriterLease {
    inner: Arc<StoreInner>,
    key: String,
}

impl ProjectWriterLease {
    fn matches(&self, project_id: &DomainProjectId) -> bool {
        self.key == project_key(project_id)
    }
}

impl Drop for ProjectWriterLease {
    fn drop(&mut self) {
        self.inner
            .project_registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.key);
    }
}

pub(crate) enum OpenProjectWriter {
    Ready(ReadyProjectWriter),
    RepairRequired(RepairRequiredWriter),
}

pub(crate) struct ReadyProjectWriter {
    lease: ProjectWriterLease,
    project_dir: OwnedFd,
    file: File,
    state: RecoveredState,
    #[cfg(test)]
    failpoint: Option<AppendFailpoint>,
    #[cfg(test)]
    checkpoint_failpoint: Option<CheckpointFailpoint>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AppendFailpoint {
    BeforeWrite,
    PartialWrite(usize),
    AfterNewlineBeforeSync,
    FileSyncError,
    AfterSyncBeforeUpdate,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CheckpointFailpoint {
    TempCreate,
    TempWrite,
    FileSync,
    BeforeInstall,
    AfterInstall,
    DirectorySyncError,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RepairFailpoint {
    RescanRace,
    TruncateError,
    FileSyncError,
    DirectorySyncError,
}

pub(crate) enum AppendFailure {
    Rejected {
        writer: ReadyProjectWriter,
        error: StateStoreError,
    },
    Poisoned {
        writer: PoisonedProjectWriter,
        error: StateStoreError,
    },
}

impl ReadyProjectWriter {
    #[cfg(test)]
    pub(crate) fn set_failpoint(&mut self, failpoint: AppendFailpoint) {
        self.failpoint = Some(failpoint);
    }

    #[cfg(test)]
    pub(crate) fn set_checkpoint_failpoint(&mut self, failpoint: CheckpointFailpoint) {
        self.checkpoint_failpoint = Some(failpoint);
    }

    #[must_use]
    pub(crate) fn snapshot(&self) -> &ProjectSnapshot {
        &self.state.projection
    }

    pub(crate) fn head(&self) -> (u64, Option<&str>) {
        (
            self.state.last_sequence,
            self.state.last_checksum.as_deref(),
        )
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

    pub(crate) fn append(
        mut self,
        request: AppendRequest,
    ) -> Result<(Self, AppendOutcome), AppendFailure> {
        let prepared = match prepare_batch(&self.state, request) {
            Ok(PreparedAppend::Idempotent(outcome)) => return Ok((self, outcome)),
            Ok(PreparedAppend::Batch(batch, next_state)) => (batch, next_state),
            Err(error) => {
                return Err(AppendFailure::Rejected {
                    writer: self,
                    error,
                });
            }
        };
        let (batch, mut next_state) = prepared;
        let mut bytes = match serde_json::to_vec(&batch) {
            Ok(bytes) => bytes,
            Err(_) => {
                return Err(AppendFailure::Rejected {
                    writer: self,
                    error: StateStoreError::IncompatibleSchema,
                });
            }
        };
        bytes.push(b'\n');
        if bytes.len() > MAX_LINE_BYTES {
            return Err(AppendFailure::Rejected {
                writer: self,
                error: StateStoreError::BatchTooLarge,
            });
        }
        let Ok(line_len) = u64::try_from(bytes.len()) else {
            return Err(AppendFailure::Rejected {
                writer: self,
                error: StateStoreError::BatchTooLarge,
            });
        };
        next_state.valid_bytes = match next_state.valid_bytes.checked_add(line_len) {
            Some(value) => value,
            None => {
                return Err(AppendFailure::Rejected {
                    writer: self,
                    error: StateStoreError::BatchTooLarge,
                });
            }
        };
        #[cfg(test)]
        if self.failpoint == Some(AppendFailpoint::BeforeWrite) {
            return Err(AppendFailure::Rejected {
                writer: self,
                error: StateStoreError::Io {
                    operation: "test before write",
                    source: std::io::Error::other("injected"),
                },
            });
        }
        #[cfg(test)]
        if let Some(AppendFailpoint::PartialWrite(count)) = self.failpoint {
            let limit = count.min(bytes.len());
            let _ignored = self.file.write_all(&bytes[..limit]);
            return Err(AppendFailure::Poisoned {
                writer: self.into_poisoned(),
                error: StateStoreError::Io {
                    operation: "test partial write",
                    source: std::io::Error::other("injected"),
                },
            });
        }
        if let Err(source) = self.file.write_all(&bytes) {
            return Err(AppendFailure::Poisoned {
                writer: self.into_poisoned(),
                error: io_error("append Project batch", source),
            });
        }
        #[cfg(test)]
        if self.failpoint == Some(AppendFailpoint::AfterNewlineBeforeSync) {
            return Err(AppendFailure::Poisoned {
                writer: self.into_poisoned(),
                error: StateStoreError::Io {
                    operation: "test after newline",
                    source: std::io::Error::other("injected"),
                },
            });
        }
        if let Err(source) = self.file.flush() {
            return Err(AppendFailure::Poisoned {
                writer: self.into_poisoned(),
                error: io_error("flush Project batch", source),
            });
        }
        #[cfg(test)]
        if self.failpoint == Some(AppendFailpoint::FileSyncError) {
            return Err(AppendFailure::Poisoned {
                writer: self.into_poisoned(),
                error: io_error("sync Project batch", std::io::Error::other("injected")),
            });
        }
        if let Err(source) = self.file.sync_all() {
            return Err(AppendFailure::Poisoned {
                writer: self.into_poisoned(),
                error: io_error("sync Project batch", source),
            });
        }
        #[cfg(test)]
        if self.failpoint == Some(AppendFailpoint::AfterSyncBeforeUpdate) {
            return Err(AppendFailure::Poisoned {
                writer: self.into_poisoned(),
                error: StateStoreError::Io {
                    operation: "test after sync",
                    source: std::io::Error::other("injected"),
                },
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
                return Err(AppendFailure::Poisoned {
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
        let projection_digest = self
            .state
            .projection
            .canonical_digest()
            .map_err(|_| StateStoreError::ProjectionRejected)?;
        let projection = serde_json::to_value(&self.state.projection)
            .map_err(|_| StateStoreError::IncompatibleSchema)?;
        let mut command_index = self.state.commands.values().cloned().collect::<Vec<_>>();
        command_index.sort();
        let events = self
            .state
            .events
            .iter()
            .map(|event| StoredProjectEventV1::from_domain(&event.event))
            .collect();
        let transaction_index = stored_transaction_index(&self.state.transactions);
        for command in &command_index {
            command.decode_reply()?;
        }
        let mut checkpoint = StoredCheckpointV1 {
            schema_version: 1,
            projection_schema_version: 1,
            project_id: self.state.project_id.as_str().to_owned(),
            stream_id: self.state.stream_id.clone(),
            epoch: 1,
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
        checkpoint.checksum = checkpoint_checksum(&checkpoint)?;
        let bytes =
            serde_json::to_vec(&checkpoint).map_err(|_| StateStoreError::IncompatibleSchema)?;
        let temp_name = format!("checkpoint-{}.tmp", random_hex_128());
        #[cfg(test)]
        if self.checkpoint_failpoint == Some(CheckpointFailpoint::TempCreate) {
            return Err(io_error(
                "test checkpoint temp create",
                std::io::Error::other("injected"),
            ));
        }
        let fd = openat(
            &self.project_dir,
            temp_name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            FILE_MODE,
        )
        .map_err(|source| io_error("create checkpoint temp", source))?;
        let mut file = File::from(fd);
        #[cfg(test)]
        if self.checkpoint_failpoint == Some(CheckpointFailpoint::TempWrite) {
            return Err(io_error(
                "test checkpoint temp write",
                std::io::Error::other("injected"),
            ));
        }
        file.write_all(&bytes)
            .map_err(|source| io_error("write checkpoint", source))?;
        #[cfg(test)]
        if self.checkpoint_failpoint == Some(CheckpointFailpoint::FileSync) {
            return Err(io_error(
                "test checkpoint sync",
                std::io::Error::other("injected"),
            ));
        }
        file.sync_all()
            .map_err(|source| io_error("sync checkpoint", source))?;
        #[cfg(test)]
        if self.checkpoint_failpoint == Some(CheckpointFailpoint::BeforeInstall) {
            return Err(io_error(
                "test checkpoint install",
                std::io::Error::other("injected"),
            ));
        }
        renameat(
            &self.project_dir,
            temp_name.as_str(),
            &self.project_dir,
            CHECKPOINT_FILE,
        )
        .map_err(|source| io_error("install checkpoint", source))?;
        #[cfg(test)]
        if self.checkpoint_failpoint == Some(CheckpointFailpoint::AfterInstall) {
            return Err(io_error(
                "test checkpoint after install",
                std::io::Error::other("injected"),
            ));
        }
        #[cfg(test)]
        if self.checkpoint_failpoint == Some(CheckpointFailpoint::DirectorySyncError) {
            return Err(io_error(
                "test checkpoint directory sync",
                std::io::Error::other("injected"),
            ));
        }
        fsync(&self.project_dir).map_err(|source| io_error("sync checkpoint directory", source))
    }

    fn into_poisoned(self) -> PoisonedProjectWriter {
        PoisonedProjectWriter {
            lease: self.lease,
            project_dir: self.project_dir,
            project_id: self.state.project_id,
            #[cfg(test)]
            recovery_failpoint: None,
        }
    }
}

pub(crate) struct PoisonedProjectWriter {
    lease: ProjectWriterLease,
    project_dir: OwnedFd,
    project_id: DomainProjectId,
    #[cfg(test)]
    recovery_failpoint: Option<RecoveryFailpoint>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryFailpoint {
    FileSync,
}

pub(crate) enum RecoveryOutcome {
    Ready(ReadyProjectWriter),
    RepairRequired(RepairRequiredWriter),
    Corrupt(CorruptProjectWriter),
}

impl PoisonedProjectWriter {
    #[cfg(test)]
    fn set_recovery_failpoint(&mut self, failpoint: RecoveryFailpoint) {
        self.recovery_failpoint = Some(failpoint);
    }

    pub(crate) fn recover(self) -> RecoveryOutcome {
        if !self.lease.matches(&self.project_id) {
            return RecoveryOutcome::Corrupt(CorruptProjectWriter {
                _lease: self.lease,
                _project_dir: self.project_dir,
                _project_id: self.project_id,
            });
        }
        recover_with_lease(
            self.lease,
            self.project_dir,
            self.project_id,
            #[cfg(test)]
            self.recovery_failpoint,
        )
    }
}

pub(crate) struct RepairRequiredWriter {
    lease: ProjectWriterLease,
    project_dir: OwnedFd,
    project_id: DomainProjectId,
    valid_bytes: u64,
    damaged_bytes: u64,
    tail_digest: String,
    #[cfg(test)]
    repair_failpoint: Option<RepairFailpoint>,
}

impl RepairRequiredWriter {
    #[cfg(test)]
    pub(crate) fn set_failpoint(&mut self, failpoint: RepairFailpoint) {
        self.repair_failpoint = Some(failpoint);
    }

    pub(crate) fn repair(self) -> Result<ReadyProjectWriter, CorruptProjectWriter> {
        if !self.lease.matches(&self.project_id) {
            return Err(self.into_corrupt());
        }
        let mut file = match open_events(&self.project_dir) {
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
        let scan = match scan_log(&mut file, &self.project_id) {
            Ok(ScanOutcome::Incomplete {
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
        if fsync(&self.project_dir).is_err() || file.seek(SeekFrom::End(0)).is_err() {
            return Err(self.into_corrupt());
        }
        Ok(ReadyProjectWriter {
            lease: self.lease,
            project_dir: self.project_dir,
            file,
            state: scan,
            #[cfg(test)]
            failpoint: None,
            #[cfg(test)]
            checkpoint_failpoint: None,
        })
    }

    fn into_corrupt(self) -> CorruptProjectWriter {
        CorruptProjectWriter {
            _lease: self.lease,
            _project_dir: self.project_dir,
            _project_id: self.project_id,
        }
    }
}

pub(crate) struct CorruptProjectWriter {
    _lease: ProjectWriterLease,
    _project_dir: OwnedFd,
    _project_id: DomainProjectId,
}

enum PreparedAppend {
    Idempotent(AppendOutcome),
    Batch(StoredProjectBatchV1, RecoveredState),
}

fn prepare_batch(
    state: &RecoveredState,
    request: AppendRequest,
) -> Result<PreparedAppend, StateStoreError> {
    if request.events.is_empty() || request.events.len() > MAX_EVENTS {
        return Err(StateStoreError::BatchTooLarge);
    }
    let stored_events = request
        .events
        .iter()
        .map(StoredProjectEventV1::from_domain)
        .collect::<Vec<_>>();
    let canonical_plan_digest = project_plan_digest(
        &state.project_id,
        &request.transaction_id,
        request.command_record.as_ref(),
        &stored_events,
    )?;
    match probe_transaction_index(
        &state.transactions,
        &request.transaction_id,
        &canonical_plan_digest,
    ) {
        TransactionProbe::Absent => {}
        TransactionProbe::SamePlanCommitted(committed) => {
            return Ok(PreparedAppend::Idempotent(AppendOutcome {
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
            return Ok(PreparedAppend::Idempotent(AppendOutcome {
                last_sequence: state.last_sequence,
                stable_reply: Some(existing.decode_reply()?),
                appended: false,
            }));
        }
    }
    let first = state
        .last_sequence
        .checked_add(1)
        .ok_or(StateStoreError::SequenceMismatch)?;
    let last = first
        .checked_add(
            u64::try_from(request.events.len()).map_err(|_| StateStoreError::BatchTooLarge)? - 1,
        )
        .ok_or(StateStoreError::SequenceMismatch)?;
    let mut batch = StoredProjectBatchV1 {
        schema_version: 1,
        project_id: state.project_id.as_str().to_owned(),
        stream_id: state.stream_id.clone(),
        epoch: 1,
        transaction_id: request.transaction_id,
        first_sequence: first,
        last_sequence: last,
        command_record: request.command_record,
        events: stored_events,
        previous_batch_checksum: state.last_checksum.clone(),
        batch_checksum: String::new(),
    };
    batch.batch_checksum = batch_checksum(&batch)?;
    let mut next = state.clone();
    apply_batch(&mut next, &batch)?;
    Ok(PreparedAppend::Batch(batch, next))
}

enum ScanOutcome {
    Clean(RecoveredState),
    Incomplete {
        state: RecoveredState,
        damaged_bytes: u64,
        tail_digest: String,
    },
}

fn scan_log(
    file: &mut File,
    expected_project: &DomainProjectId,
) -> Result<ScanOutcome, StateStoreError> {
    scan_log_from(file, empty_recovered(expected_project.clone()))
}

fn scan_log_from(
    file: &mut File,
    mut state: RecoveredState,
) -> Result<ScanOutcome, StateStoreError> {
    let mut offset = state.valid_bytes;
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.seek(SeekFrom::Start(offset)))
        .map_err(|source| io_error("seek Project log", source))?;
    let mut reader = BufReader::new(file);
    loop {
        let mut line = Vec::new();
        let count = reader
            .by_ref()
            .take(u64::try_from(MAX_LINE_BYTES).expect("line limit fits u64") + 1)
            .read_until(b'\n', &mut line)
            .map_err(|source| io_error("read Project log", source))?;
        if count == 0 {
            state.valid_bytes = offset;
            return Ok(ScanOutcome::Clean(state));
        }
        if count > MAX_LINE_BYTES {
            return Err(StateStoreError::BatchTooLarge);
        }
        if !line.ends_with(b"\n") {
            let damaged_bytes =
                u64::try_from(line.len()).map_err(|_| StateStoreError::BatchTooLarge)?;
            let tail_digest = format!("sha256:{:x}", Sha256::digest(&line));
            state.valid_bytes = offset;
            return Ok(ScanOutcome::Incomplete {
                state,
                damaged_bytes,
                tail_digest,
            });
        }
        line.pop();
        let batch: StoredProjectBatchV1 =
            serde_json::from_slice(&line).map_err(|_| StateStoreError::MiddleCorruption)?;
        apply_batch(&mut state, &batch)?;
        offset = offset
            .checked_add(u64::try_from(count).map_err(|_| StateStoreError::BatchTooLarge)?)
            .ok_or(StateStoreError::BatchTooLarge)?;
        state.valid_bytes = offset;
    }
}

fn apply_batch(
    state: &mut RecoveredState,
    batch: &StoredProjectBatchV1,
) -> Result<(), StateStoreError> {
    if batch.schema_version != 1 || batch.epoch != 1 {
        return Err(StateStoreError::IncompatibleSchema);
    }
    let batch_project = DomainProjectId::parse(batch.project_id.clone())
        .map_err(|_| StateStoreError::IncompatibleSchema)?;
    if batch_project != state.project_id {
        return Err(StateStoreError::StreamMismatch);
    }
    if state.last_sequence == 0 {
        state.stream_id.clone_from(&batch.stream_id);
    } else if batch.stream_id != state.stream_id {
        return Err(StateStoreError::StreamMismatch);
    }
    if batch.events.is_empty() || batch.events.len() > MAX_EVENTS {
        return Err(StateStoreError::SequenceMismatch);
    }
    let expected_last = batch
        .first_sequence
        .checked_add(
            u64::try_from(batch.events.len()).map_err(|_| StateStoreError::BatchTooLarge)? - 1,
        )
        .ok_or(StateStoreError::SequenceMismatch)?;
    if batch.first_sequence != state.last_sequence + 1 || batch.last_sequence != expected_last {
        return Err(StateStoreError::SequenceMismatch);
    }
    if batch.transaction_id.is_empty() {
        return Err(StateStoreError::SequenceMismatch);
    }
    if batch.previous_batch_checksum != state.last_checksum {
        return Err(StateStoreError::ChecksumChainMismatch);
    }
    if batch.batch_checksum != batch_checksum(batch)? {
        return Err(StateStoreError::ChecksumMismatch);
    }
    let mut next_events = state.events.clone();
    for (index, stored) in batch.events.clone().into_iter().enumerate() {
        next_events.push(SequencedProjectEvent {
            schema_version: SchemaVersion::project_event_v1(),
            sequence: batch.first_sequence
                + u64::try_from(index).map_err(|_| StateStoreError::BatchTooLarge)?,
            event: stored
                .into_domain()
                .map_err(|_| StateStoreError::ProjectionRejected)?,
        });
    }
    let projection = replay(&next_events).map_err(|_| StateStoreError::ProjectionRejected)?;
    if let Some(command) = &batch.command_record {
        command.decode_reply()?;
        let key = (command.client_id.clone(), command.client_command_id.clone());
        if state.commands.contains_key(&key) {
            return Err(StateStoreError::IdempotencyConflict);
        }
        state.commands.insert(key, command.clone());
    }
    state.events = next_events;
    state.projection = projection;
    state.last_sequence = batch.last_sequence;
    state.last_checksum = Some(batch.batch_checksum.clone());
    let plan_digest = project_plan_digest(
        &state.project_id,
        &batch.transaction_id,
        batch.command_record.as_ref(),
        &batch.events,
    )?;
    let commit = TransactionCommit {
        canonical_plan_digest: plan_digest,
        resulting_last_sequence: batch.last_sequence,
        resulting_batch_checksum: batch.batch_checksum.clone(),
    };
    if state
        .transactions
        .insert(batch.transaction_id.clone(), commit)
        .is_some()
    {
        return Err(StateStoreError::SequenceMismatch);
    }
    Ok(())
}

fn project_plan_digest(
    project_id: &DomainProjectId,
    transaction_id: &str,
    command_record: Option<&StoredCommandRecordV1>,
    events: &[StoredProjectEventV1],
) -> Result<String, StateStoreError> {
    if transaction_id.is_empty() {
        return Err(StateStoreError::SequenceMismatch);
    }
    let canonical = serde_json::to_vec(&(
        "alda-project-plan-v1",
        project_id.as_str(),
        transaction_id,
        command_record,
        events,
    ))
    .map_err(|_| StateStoreError::IncompatibleSchema)?;
    Ok(format!("sha256:{:x}", Sha256::digest(canonical)))
}

fn probe_transaction_index(
    transactions: &BTreeMap<String, TransactionCommit>,
    transaction_id: &str,
    canonical_plan_digest: &str,
) -> TransactionProbe {
    match transactions.get(transaction_id) {
        None => TransactionProbe::Absent,
        Some(commit) if commit.canonical_plan_digest == canonical_plan_digest => {
            TransactionProbe::SamePlanCommitted(commit.clone())
        }
        Some(_) => TransactionProbe::ConflictingPlan,
    }
}

fn stored_transaction_index(
    transactions: &BTreeMap<String, TransactionCommit>,
) -> Vec<StoredTransactionCommitV1> {
    transactions
        .iter()
        .map(|(transaction_id, commit)| StoredTransactionCommitV1 {
            transaction_id: transaction_id.clone(),
            canonical_plan_digest: commit.canonical_plan_digest.clone(),
            resulting_last_sequence: commit.resulting_last_sequence,
            resulting_batch_checksum: commit.resulting_batch_checksum.clone(),
        })
        .collect()
}

fn batch_checksum(batch: &StoredProjectBatchV1) -> Result<String, StateStoreError> {
    let canonical = serde_json::to_vec(&(
        "alda-project-batch-v1",
        batch.schema_version,
        &batch.project_id,
        &batch.stream_id,
        batch.epoch,
        &batch.transaction_id,
        batch.first_sequence,
        batch.last_sequence,
        &batch.command_record,
        &batch.events,
        &batch.previous_batch_checksum,
    ))
    .map_err(|_| StateStoreError::IncompatibleSchema)?;
    Ok(format!("sha256:{:x}", Sha256::digest(canonical)))
}

fn checkpoint_checksum(checkpoint: &StoredCheckpointV1) -> Result<String, StateStoreError> {
    let canonical = serde_json::to_vec(&(
        "alda-project-checkpoint-v1",
        checkpoint.schema_version,
        checkpoint.projection_schema_version,
        &checkpoint.project_id,
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

fn load_checkpoint(
    project_dir: &OwnedFd,
    events_file: &mut File,
    expected_project: &DomainProjectId,
) -> Result<Option<RecoveredState>, StateStoreError> {
    let fd = match openat(
        project_dir,
        CHECKPOINT_FILE,
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
        .map_err(|source| io_error("read checkpoint", source))?;
    if u64::try_from(bytes.len()).map_or(true, |length| length > MAX_CHECKPOINT_BYTES) {
        return Ok(None);
    }
    let Ok(checkpoint) = serde_json::from_slice::<StoredCheckpointV1>(&bytes) else {
        return Ok(None);
    };
    if checkpoint.schema_version != 1
        || checkpoint.projection_schema_version != 1
        || DomainProjectId::parse(checkpoint.project_id.clone())
            .map_or(true, |project| project != *expected_project)
        || checkpoint.epoch != 1
        || checkpoint.checksum != checkpoint_checksum(&checkpoint)?
    {
        return Ok(None);
    }
    let mut events = Vec::with_capacity(checkpoint.events.len());
    for (index, stored) in checkpoint.events.iter().cloned().enumerate() {
        events.push(SequencedProjectEvent {
            schema_version: SchemaVersion::project_event_v1(),
            sequence: u64::try_from(index).map_err(|_| StateStoreError::BatchTooLarge)? + 1,
            event: stored
                .into_domain()
                .map_err(|_| StateStoreError::ProjectionRejected)?,
        });
    }
    let projection = replay(&events).map_err(|_| StateStoreError::ProjectionRejected)?;
    if checkpoint.covered_sequence
        != u64::try_from(events.len()).map_err(|_| StateStoreError::BatchTooLarge)?
        || checkpoint.projection_digest
            != projection
                .canonical_digest()
                .map_err(|_| StateStoreError::ProjectionRejected)?
        || checkpoint.projection
            != serde_json::to_value(&projection).map_err(|_| StateStoreError::IncompatibleSchema)?
    {
        return Ok(None);
    }
    let mut commands = BTreeMap::new();
    for command in &checkpoint.command_index {
        command.decode_reply()?;
        let key = (command.client_id.clone(), command.client_command_id.clone());
        if commands.insert(key, command.clone()).is_some() {
            return Ok(None);
        }
    }
    let mut transactions = BTreeMap::new();
    for stored in &checkpoint.transaction_index {
        if stored.transaction_id.is_empty()
            || validate_sha256(&stored.canonical_plan_digest).is_err()
            || validate_sha256(&stored.resulting_batch_checksum).is_err()
            || stored.resulting_last_sequence > checkpoint.covered_sequence
            || transactions
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
    let mut anchored = empty_recovered(expected_project.clone());
    loop {
        if anchored.valid_bytes == checkpoint.covered_valid_bytes {
            break;
        }
        if anchored.valid_bytes > checkpoint.covered_valid_bytes {
            return Ok(None);
        }
        events_file
            .seek(SeekFrom::Start(anchored.valid_bytes))
            .map_err(|source| io_error("seek checkpoint anchor", source))?;
        let mut reader = BufReader::new(&mut *events_file);
        let mut line = Vec::new();
        let count = reader
            .by_ref()
            .take(u64::try_from(MAX_LINE_BYTES).expect("line limit fits u64") + 1)
            .read_until(b'\n', &mut line)
            .map_err(|source| io_error("read checkpoint anchor", source))?;
        if count == 0 || count > MAX_LINE_BYTES || !line.ends_with(b"\n") {
            return Ok(None);
        }
        line.pop();
        let Ok(batch) = serde_json::from_slice::<StoredProjectBatchV1>(&line) else {
            return Ok(None);
        };
        if apply_batch(&mut anchored, &batch).is_err() {
            return Ok(None);
        }
        anchored.valid_bytes = anchored
            .valid_bytes
            .checked_add(u64::try_from(count).map_err(|_| StateStoreError::BatchTooLarge)?)
            .ok_or(StateStoreError::BatchTooLarge)?;
    }
    if anchored.last_sequence != checkpoint.covered_sequence
        || anchored.last_checksum != checkpoint.covered_batch_checksum
        || anchored.stream_id != checkpoint.stream_id
        || anchored.commands != commands
        || anchored.transactions != transactions
    {
        return Ok(None);
    }
    Ok(Some(RecoveredState {
        project_id: expected_project.clone(),
        stream_id: checkpoint.stream_id,
        last_sequence: checkpoint.covered_sequence,
        last_checksum: checkpoint.covered_batch_checksum,
        projection,
        events,
        commands,
        transactions,
        valid_bytes: checkpoint.covered_valid_bytes,
    }))
}

fn open_writer_with_lease(
    lease: ProjectWriterLease,
    project_id: DomainProjectId,
    key: &str,
    init_failpoint: Option<InitFailpoint>,
) -> Result<OpenProjectWriter, (ProjectWriterLease, StateStoreError)> {
    let project_dir = match ensure_directory(
        &lease.inner.projects,
        key,
        DirectoryKind::Project,
        init_failpoint,
    ) {
        Ok(directory) => directory,
        Err(error) => return Err((lease, error)),
    };
    let mut file = match open_or_create_events(&project_dir, init_failpoint) {
        Ok(file) => file,
        Err(error) => return Err((lease, error)),
    };
    let scan = match load_checkpoint(&project_dir, &mut file, &project_id) {
        Ok(Some(state)) => scan_log_from(&mut file, state),
        Ok(None) | Err(_) => scan_log(&mut file, &project_id),
    };
    match scan {
        Ok(ScanOutcome::Clean(mut state)) => {
            if state.stream_id.is_empty() {
                state.stream_id = random_hex_128();
            }
            if let Err(source) = file.seek(SeekFrom::End(0)) {
                return Err((lease, io_error("seek Project log end", source)));
            }
            Ok(OpenProjectWriter::Ready(ReadyProjectWriter {
                lease,
                project_dir,
                file,
                state,
                #[cfg(test)]
                failpoint: None,
                #[cfg(test)]
                checkpoint_failpoint: None,
            }))
        }
        Ok(ScanOutcome::Incomplete {
            state,
            damaged_bytes,
            tail_digest,
        }) => Ok(OpenProjectWriter::RepairRequired(RepairRequiredWriter {
            lease,
            project_dir,
            project_id,
            valid_bytes: state.valid_bytes,
            damaged_bytes,
            tail_digest,
            #[cfg(test)]
            repair_failpoint: None,
        })),
        Err(error) => Err((lease, error)),
    }
}

fn recover_with_lease(
    lease: ProjectWriterLease,
    project_dir: OwnedFd,
    project_id: DomainProjectId,
    #[cfg(test)] recovery_failpoint: Option<RecoveryFailpoint>,
) -> RecoveryOutcome {
    let mut file = match open_events(&project_dir) {
        Ok(file) => file,
        Err(_) => {
            return RecoveryOutcome::Corrupt(CorruptProjectWriter {
                _lease: lease,
                _project_dir: project_dir,
                _project_id: project_id,
            });
        }
    };
    let scan = match load_checkpoint(&project_dir, &mut file, &project_id) {
        Ok(Some(state)) => scan_log_from(&mut file, state),
        Ok(None) | Err(_) => scan_log(&mut file, &project_id),
    };
    match scan {
        Ok(ScanOutcome::Clean(state))
            if {
                #[cfg(test)]
                let sync_allowed = recovery_failpoint != Some(RecoveryFailpoint::FileSync);
                #[cfg(not(test))]
                let sync_allowed = true;
                sync_allowed && file.sync_all().is_ok() && file.seek(SeekFrom::End(0)).is_ok()
            } =>
        {
            RecoveryOutcome::Ready(ReadyProjectWriter {
                lease,
                project_dir,
                file,
                state,
                #[cfg(test)]
                failpoint: None,
                #[cfg(test)]
                checkpoint_failpoint: None,
            })
        }
        Ok(ScanOutcome::Incomplete {
            state,
            damaged_bytes,
            tail_digest,
        }) => RecoveryOutcome::RepairRequired(RepairRequiredWriter {
            lease,
            project_dir,
            project_id,
            valid_bytes: state.valid_bytes,
            damaged_bytes,
            tail_digest,
            #[cfg(test)]
            repair_failpoint: None,
        }),
        _ => RecoveryOutcome::Corrupt(CorruptProjectWriter {
            _lease: lease,
            _project_dir: project_dir,
            _project_id: project_id,
        }),
    }
}

fn empty_recovered(project_id: DomainProjectId) -> RecoveredState {
    RecoveredState {
        project_id,
        stream_id: String::new(),
        last_sequence: 0,
        last_checksum: None,
        projection: ProjectSnapshot::default(),
        events: vec![],
        commands: BTreeMap::new(),
        transactions: BTreeMap::new(),
        valid_bytes: 0,
    }
}

fn project_key(project_id: &DomainProjectId) -> String {
    format!("{:x}", Sha256::digest(project_id.as_str().as_bytes()))
}

fn canonical_project_directory_name(name: &CStr) -> Result<String, StateStoreError> {
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

fn open_project_directory(parent: &OwnedFd, name: &str) -> Result<OwnedFd, StateStoreError> {
    let fd = openat(
        parent,
        name,
        DIRECTORY_FLAGS | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|source| io_error("open Project directory", source))?;
    validate_directory(&fd, false)?;
    Ok(fd)
}

fn open_events_read(project_dir: &OwnedFd) -> Result<File, StateStoreError> {
    let fd = openat(
        project_dir,
        EVENTS_FILE,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|source| io_error("read Project events", source))?;
    let file = File::from(fd);
    validate_regular_file(&file, None)?;
    Ok(file)
}

fn discover_project_id(file: &mut File) -> Result<DomainProjectId, StateStoreError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|source| io_error("seek Project first batch", source))?;
    let mut line = Vec::new();
    let count = BufReader::new(&mut *file)
        .take(u64::try_from(MAX_LINE_BYTES).expect("line limit fits u64") + 1)
        .read_until(b'\n', &mut line)
        .map_err(|source| io_error("read Project first batch", source))?;
    if count == 0 || count > MAX_LINE_BYTES || !line.ends_with(b"\n") {
        return Err(StateStoreError::MiddleCorruption);
    }
    line.pop();
    let batch: StoredProjectBatchV1 =
        serde_json::from_slice(&line).map_err(|_| StateStoreError::MiddleCorruption)?;
    let id =
        DomainProjectId::parse(batch.project_id).map_err(|_| StateStoreError::MiddleCorruption)?;
    file.seek(SeekFrom::Start(0))
        .map_err(|source| io_error("rewind Project events", source))?;
    Ok(id)
}

fn open_absolute_directory(path: &Path) -> Result<OwnedFd, StateStoreError> {
    if !path.is_absolute() {
        return Err(StateStoreError::UnsafeRoot);
    }
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() <= 1
        || bytes[1..]
            .split(|byte| *byte == b'/')
            .any(|component| component.is_empty() || component == b"." || component == b"..")
    {
        return Err(StateStoreError::UnsafeRoot);
    }
    let mut current = openat(CWD, "/", DIRECTORY_FLAGS, Mode::empty())
        .map_err(|source| io_error("open filesystem root", source))?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                current = openat(&current, name, DIRECTORY_FLAGS, Mode::empty())
                    .map_err(|source| io_error("open state root component", source))?;
            }
            _ => return Err(StateStoreError::UnsafeRoot),
        }
    }
    Ok(current)
}

fn validate_directory(fd: &OwnedFd, _root: bool) -> Result<(), StateStoreError> {
    let stat = fstat(fd).map_err(|source| io_error("inspect state directory", source))?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_dir()
        || !has_private_current_owner(stat.st_mode, stat.st_uid)
    {
        return Err(StateStoreError::UnsafeRoot);
    }
    Ok(())
}

fn ensure_directory(
    parent: &OwnedFd,
    name: &str,
    kind: DirectoryKind,
    failpoint: Option<InitFailpoint>,
) -> Result<OwnedFd, StateStoreError> {
    let (create, child_sync, parent_sync) = match kind {
        DirectoryKind::Layout => (
            InitFailpoint::LayoutCreate,
            InitFailpoint::LayoutChildSync,
            InitFailpoint::LayoutParentSync,
        ),
        DirectoryKind::Projects => (
            InitFailpoint::ProjectsCreate,
            InitFailpoint::ProjectsChildSync,
            InitFailpoint::ProjectsParentSync,
        ),
        DirectoryKind::Sessions => (
            InitFailpoint::SessionsCreate,
            InitFailpoint::SessionsChildSync,
            InitFailpoint::SessionsParentSync,
        ),
        DirectoryKind::Project => (
            InitFailpoint::ProjectDirectoryCreate,
            InitFailpoint::ProjectDirectoryChildSync,
            InitFailpoint::ProjectDirectoryParentSync,
        ),
        DirectoryKind::Session => (
            InitFailpoint::SessionDirectoryCreate,
            InitFailpoint::SessionDirectoryChildSync,
            InitFailpoint::SessionDirectoryParentSync,
        ),
    };
    inject_init(failpoint, create, "test state directory create")?;
    match mkdirat(parent, name, DIRECTORY_MODE) {
        Ok(()) => {
            let child = open_directory(parent, name)?;
            inject_init(failpoint, child_sync, "test state child directory sync")?;
            fsync(&child).map_err(|source| io_error("sync state directory", source))?;
            inject_init(failpoint, parent_sync, "test state parent directory sync")?;
            fsync(parent).map_err(|source| io_error("sync state parent directory", source))?;
            Ok(child)
        }
        Err(rustix::io::Errno::EXIST) => open_directory(parent, name),
        Err(source) => Err(io_error("create state directory", source)),
    }
}

fn open_directory(parent: &OwnedFd, name: &str) -> Result<OwnedFd, StateStoreError> {
    let fd = openat(parent, name, DIRECTORY_FLAGS, Mode::empty())
        .map_err(|source| io_error("open state directory", source))?;
    validate_directory(&fd, false)?;
    Ok(fd)
}

fn open_or_create_events(
    directory: &OwnedFd,
    failpoint: Option<InitFailpoint>,
) -> Result<File, StateStoreError> {
    inject_init(
        failpoint,
        InitFailpoint::EventsCreate,
        "test Project events create",
    )?;
    let fd = openat(
        directory,
        EVENTS_FILE,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        FILE_MODE,
    )
    .map_err(|source| io_error("open Project events file", source))?;
    let file = File::from(fd);
    validate_regular_file(&file, None)?;
    inject_init(
        failpoint,
        InitFailpoint::EventsFileSync,
        "test Project events file sync",
    )?;
    file.sync_all()
        .map_err(|source| io_error("sync Project events file", source))?;
    inject_init(
        failpoint,
        InitFailpoint::EventsDirectorySync,
        "test Project events directory sync",
    )?;
    fsync(directory).map_err(|source| io_error("sync Project directory", source))?;
    Ok(file)
}

fn open_events(directory: &OwnedFd) -> Result<File, StateStoreError> {
    let fd = openat(
        directory,
        EVENTS_FILE,
        OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|source| io_error("reopen Project events file", source))?;
    let file = File::from(fd);
    validate_regular_file(&file, None)?;
    Ok(file)
}

fn load_or_create_manifest(
    layout: &OwnedFd,
    failpoint: Option<InitFailpoint>,
) -> Result<StateManifest, StateStoreError> {
    match openat(
        layout,
        STATE_MANIFEST,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => read_manifest(File::from(fd)),
        Err(rustix::io::Errno::NOENT) => {
            let body = StateManifestBody {
                schema_version: 1,
                layout_version: 1,
                instance_id: random_hex_128(),
                durability: "linux_file_and_directory_synced".to_owned(),
            };
            let checksum = manifest_checksum(&body)?;
            let manifest = StateManifest { body, checksum };
            let temp_name = format!("manifest-{}.tmp", random_hex_128());
            inject_init(
                failpoint,
                InitFailpoint::ManifestTempCreate,
                "test state manifest temp create",
            )?;
            let fd = openat(
                layout,
                temp_name.as_str(),
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                FILE_MODE,
            )
            .map_err(|source| io_error("create state manifest temp", source))?;
            let mut file = File::from(fd);
            inject_init(
                failpoint,
                InitFailpoint::ManifestWrite,
                "test state manifest write",
            )?;
            serde_json::to_writer(&mut file, &manifest)
                .map_err(|_| StateStoreError::IncompatibleSchema)?;
            inject_init(
                failpoint,
                InitFailpoint::ManifestFileSync,
                "test state manifest file sync",
            )?;
            file.sync_all()
                .map_err(|source| io_error("sync state manifest", source))?;
            inject_init(
                failpoint,
                InitFailpoint::ManifestInstall,
                "test state manifest install",
            )?;
            renameat_with(
                layout,
                temp_name.as_str(),
                layout,
                STATE_MANIFEST,
                RenameFlags::NOREPLACE,
            )
            .map_err(|source| io_error("install state manifest", source))?;
            inject_init(
                failpoint,
                InitFailpoint::ManifestDirectorySync,
                "test state manifest directory sync",
            )?;
            fsync(layout).map_err(|source| io_error("sync state layout", source))?;
            Ok(manifest)
        }
        Err(source) => Err(io_error("open state manifest", source)),
    }
}

fn inject_init(
    actual: Option<InitFailpoint>,
    expected: InitFailpoint,
    operation: &'static str,
) -> Result<(), StateStoreError> {
    if actual == Some(expected) {
        return Err(io_error(operation, std::io::Error::other("injected")));
    }
    Ok(())
}

fn read_manifest(file: File) -> Result<StateManifest, StateStoreError> {
    validate_regular_file(&file, Some(MAX_MANIFEST_BYTES))?;
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| io_error("read state manifest", source))?;
    if u64::try_from(bytes.len()).map_or(true, |length| length > MAX_MANIFEST_BYTES) {
        return Err(StateStoreError::BatchTooLarge);
    }
    let manifest: StateManifest =
        serde_json::from_slice(&bytes).map_err(|_| StateStoreError::IncompatibleSchema)?;
    if manifest.body.schema_version != 1
        || manifest.body.layout_version != 1
        || manifest.body.durability != "linux_file_and_directory_synced"
        || manifest.body.instance_id.len() != 32
        || manifest.checksum != manifest_checksum(&manifest.body)?
    {
        return Err(StateStoreError::IncompatibleSchema);
    }
    Ok(manifest)
}

fn validate_regular_file(file: &File, max_bytes: Option<u64>) -> Result<(), StateStoreError> {
    let stat = fstat(file).map_err(|source| io_error("inspect state file", source))?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file()
        || !has_private_current_owner(stat.st_mode, stat.st_uid)
        || max_bytes
            .is_some_and(|limit| u64::try_from(stat.st_size).map_or(true, |size| size > limit))
    {
        return Err(StateStoreError::UnsafeRoot);
    }
    Ok(())
}

fn has_private_current_owner(mode: u32, uid: u32) -> bool {
    uid == rustix::process::getuid().as_raw() && mode.trailing_zeros() >= 6
}

fn manifest_checksum(body: &StateManifestBody) -> Result<String, StateStoreError> {
    let bytes = serde_json::to_vec(&("alda-state-manifest-v1", body))
        .map_err(|_| StateStoreError::IncompatibleSchema)?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

pub(crate) fn validate_sha256(value: &str) -> Result<(), StateStoreError> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(StateStoreError::IncompatibleSchema);
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(StateStoreError::IncompatibleSchema);
    }
    Ok(())
}

fn random_hex_128() -> String {
    let mut bytes = [0_u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes
        .iter()
        .fold(String::with_capacity(32), |mut value, byte| {
            write!(value, "{byte:02x}").expect("writing to String cannot fail");
            value
        })
}

fn io_error(operation: &'static str, source: impl Into<std::io::Error>) -> StateStoreError {
    StateStoreError::Io {
        operation,
        source: source.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Cursor;
    use std::os::unix::fs::PermissionsExt;

    use crate::artifact_store::ArtifactStore;
    use crate::domain::{BranchId, BriefRevisionId, CreativeBrief, ScoreId, TakeId};
    use crate::protocol::{ClientCommandId, ProtocolErrorCode};

    use super::project_codec::{
        RecoveredArtifactProjectHandoff, StoredProjectEventV1, recover_artifact_for_project_plan,
    };
    use super::*;

    fn project(value: &str) -> DomainProjectId {
        DomainProjectId::parse(value).expect("project ID")
    }

    fn initialized(project_id: &DomainProjectId) -> ProjectEvent {
        ProjectEvent::ProjectInitialized {
            project_id: project_id.clone(),
            score_id: ScoreId::parse("score-1").expect("score"),
            default_take_id: TakeId::parse("take-1").expect("take"),
            default_branch_id: BranchId::parse("branch-1").expect("branch"),
        }
    }

    fn brief(project_id: &DomainProjectId) -> ProjectEvent {
        ProjectEvent::BriefRevisionCreated(CreativeBrief {
            id: BriefRevisionId::parse("brief-1").expect("brief"),
            project_id: project_id.clone(),
            previous: None,
            user_description: "Write an etude".to_owned(),
            goals: vec!["clarity".to_owned()],
            instrumentation: vec!["piano".to_owned()],
            open_questions: Vec::new(),
        })
    }

    fn reply(command_id: &str) -> Vec<u8> {
        serde_json::to_vec(&CommandReply::error(
            ClientCommandId(command_id.to_owned()),
            ProtocolErrorCode::InvalidRequest,
            "fixture",
        ))
        .expect("canonical reply")
    }

    fn command(command_id: &str, digest_byte: char) -> StoredCommandRecordV1 {
        StoredCommandRecordV1::new(
            "client-1",
            command_id,
            format!("sha256:{}", digest_byte.to_string().repeat(64)),
            &reply(command_id),
        )
        .expect("command record")
    }

    fn make_private(root: &tempfile::TempDir) {
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))
            .expect("make test state root private");
    }

    fn open_store(root: &tempfile::TempDir) -> StateStore {
        make_private(root);
        StateStore::open(root.path(), StateStoreInstanceLease::for_tests()).expect("state store")
    }

    fn project_directory(
        root: &tempfile::TempDir,
        project_id: &DomainProjectId,
    ) -> std::path::PathBuf {
        root.path()
            .join(STATE_LAYOUT)
            .join("projects")
            .join(project_key(project_id))
    }

    fn ready(store: &StateStore, project_id: DomainProjectId) -> ReadyProjectWriter {
        match store
            .open_project_writer(project_id)
            .expect("open Project writer")
        {
            OpenProjectWriter::Ready(writer) => writer,
            OpenProjectWriter::RepairRequired(_) => panic!("unexpected incomplete tail"),
        }
    }

    fn append_ok(
        writer: ReadyProjectWriter,
        request: AppendRequest,
    ) -> (ReadyProjectWriter, AppendOutcome) {
        match writer.append(request) {
            Ok(value) => value,
            Err(_) => panic!("append must succeed"),
        }
    }

    #[test]
    fn stable_reply_codec_is_byte_exact_and_rejects_noncanonical_json() {
        let raw = reply("command-1");
        let record = command("command-1", 'a');
        assert_eq!(record.decode_reply().expect("decode"), raw);
        assert_eq!(record.stable_reply_base64, BASE64_STANDARD.encode(&raw));
        let mut noncanonical = raw;
        noncanonical.push(b' ');
        assert!(matches!(
            StoredCommandRecordV1::new(
                "client-1",
                "command-1",
                format!("sha256:{}", "a".repeat(64)),
                &noncanonical
            ),
            Err(StateStoreError::IncompatibleSchema)
        ));
    }

    #[test]
    fn registry_is_atomic_per_project_and_releases_on_drop() {
        let root = tempfile::tempdir().expect("root");
        let store = open_store(&root);
        let first = ready(&store, project("project-1"));
        assert!(matches!(
            store.open_project_writer(project("project-1")),
            Err(StateStoreError::WriterLockRequired)
        ));
        let other = ready(&store, project("project-2"));
        drop(other);
        drop(first);
        drop(ready(&store, project("project-1")));
    }

    #[test]
    fn initialization_failpoints_leave_only_reopenable_or_explicitly_incomplete_state() {
        let store_open_points = [
            InitFailpoint::LayoutCreate,
            InitFailpoint::LayoutChildSync,
            InitFailpoint::LayoutParentSync,
            InitFailpoint::ManifestTempCreate,
            InitFailpoint::ManifestWrite,
            InitFailpoint::ManifestFileSync,
            InitFailpoint::ManifestInstall,
            InitFailpoint::ManifestDirectorySync,
            InitFailpoint::ProjectsCreate,
            InitFailpoint::ProjectsChildSync,
            InitFailpoint::ProjectsParentSync,
        ];
        for failpoint in store_open_points {
            let root = tempfile::tempdir().expect("root");
            make_private(&root);
            assert!(matches!(
                StateStore::open_with_failpoint(
                    root.path(),
                    StateStoreInstanceLease::for_tests(),
                    failpoint,
                ),
                Err(StateStoreError::Io { .. })
            ));
            let reopened = open_store(&root);
            drop(ready(&reopened, project("project-1")));
        }

        let writer_open_points = [
            InitFailpoint::ProjectDirectoryCreate,
            InitFailpoint::ProjectDirectoryChildSync,
            InitFailpoint::ProjectDirectoryParentSync,
            InitFailpoint::EventsCreate,
            InitFailpoint::EventsFileSync,
            InitFailpoint::EventsDirectorySync,
        ];
        for failpoint in writer_open_points {
            let root = tempfile::tempdir().expect("root");
            make_private(&root);
            let store = StateStore::open_with_failpoint(
                root.path(),
                StateStoreInstanceLease::for_tests(),
                failpoint,
            )
            .expect("store opens before Project-scoped failpoint");
            assert!(matches!(
                store.open_project_writer(project("project-1")),
                Err(StateStoreError::Io { .. })
            ));
            drop(store);
            let reopened = open_store(&root);
            drop(ready(&reopened, project("project-1")));
        }
    }

    #[test]
    fn managed_directories_and_manifest_fail_closed_on_unsafe_metadata() {
        let weak_root = tempfile::tempdir().expect("root");
        fs::set_permissions(weak_root.path(), fs::Permissions::from_mode(0o750))
            .expect("weaken root");
        assert!(matches!(
            StateStore::open(weak_root.path(), StateStoreInstanceLease::for_tests()),
            Err(StateStoreError::UnsafeRoot)
        ));

        let weak_layout = tempfile::tempdir().expect("root");
        make_private(&weak_layout);
        let layout_path = weak_layout.path().join(STATE_LAYOUT);
        fs::create_dir(&layout_path).expect("layout");
        fs::set_permissions(&layout_path, fs::Permissions::from_mode(0o750))
            .expect("weaken layout");
        assert!(matches!(
            StateStore::open(weak_layout.path(), StateStoreInstanceLease::for_tests()),
            Err(StateStoreError::UnsafeRoot)
        ));

        let weak_projects = tempfile::tempdir().expect("root");
        let store = open_store(&weak_projects);
        drop(store);
        let projects_path = weak_projects.path().join(STATE_LAYOUT).join("projects");
        fs::set_permissions(&projects_path, fs::Permissions::from_mode(0o750))
            .expect("weaken projects");
        assert!(matches!(
            StateStore::open(weak_projects.path(), StateStoreInstanceLease::for_tests()),
            Err(StateStoreError::UnsafeRoot)
        ));

        assert!(!has_private_current_owner(
            0o100_600,
            rustix::process::getuid().as_raw().wrapping_add(1)
        ));

        for fixture in ["fifo", "directory", "symlink", "weak_mode", "oversized"] {
            let root = tempfile::tempdir().expect("root");
            let store = open_store(&root);
            drop(store);
            let manifest = root.path().join(STATE_LAYOUT).join(STATE_MANIFEST);
            fs::remove_file(&manifest).expect("remove manifest");
            match fixture {
                "fifo" => {
                    rustix::fs::mkfifoat(CWD, &manifest, FILE_MODE).expect("create manifest FIFO");
                }
                "directory" => {
                    fs::create_dir(&manifest).expect("create manifest directory");
                    fs::set_permissions(&manifest, fs::Permissions::from_mode(0o700))
                        .expect("private manifest directory");
                }
                "symlink" => {
                    std::os::unix::fs::symlink("/dev/null", &manifest)
                        .expect("create manifest symlink");
                }
                "weak_mode" => {
                    fs::write(&manifest, b"{}").expect("write weak manifest");
                    fs::set_permissions(&manifest, fs::Permissions::from_mode(0o640))
                        .expect("weaken manifest");
                }
                "oversized" => {
                    fs::write(
                        &manifest,
                        vec![
                            b'x';
                            usize::try_from(MAX_MANIFEST_BYTES).expect("limit fits usize") + 1
                        ],
                    )
                    .expect("write oversized manifest");
                    fs::set_permissions(&manifest, fs::Permissions::from_mode(0o600))
                        .expect("private oversized manifest");
                }
                _ => unreachable!(),
            }
            assert!(
                StateStore::open(root.path(), StateStoreInstanceLease::for_tests()).is_err(),
                "unsafe manifest fixture {fixture} must fail closed"
            );
        }
    }

    #[test]
    fn project_directory_and_events_file_metadata_are_private_regular_and_bounded() {
        let project_id = project("project-1");
        for fixture in [
            "weak_project_dir",
            "fifo",
            "directory",
            "symlink",
            "weak_mode",
            "oversized_line",
        ] {
            let root = tempfile::tempdir().expect("root");
            let store = open_store(&root);
            drop(ready(&store, project_id.clone()));
            drop(store);
            let project_dir = project_directory(&root, &project_id);
            if fixture == "weak_project_dir" {
                fs::set_permissions(&project_dir, fs::Permissions::from_mode(0o750))
                    .expect("weaken Project directory");
            } else {
                let events = project_dir.join(EVENTS_FILE);
                fs::remove_file(&events).expect("remove events");
                match fixture {
                    "fifo" => {
                        rustix::fs::mkfifoat(CWD, &events, FILE_MODE).expect("create events FIFO");
                    }
                    "directory" => {
                        fs::create_dir(&events).expect("create events directory");
                        fs::set_permissions(&events, fs::Permissions::from_mode(0o700))
                            .expect("private events directory");
                    }
                    "symlink" => {
                        std::os::unix::fs::symlink("/dev/null", &events)
                            .expect("create events symlink");
                    }
                    "weak_mode" => {
                        fs::write(&events, b"").expect("write weak events");
                        fs::set_permissions(&events, fs::Permissions::from_mode(0o640))
                            .expect("weaken events");
                    }
                    "oversized_line" => {
                        fs::write(&events, vec![b'x'; MAX_LINE_BYTES + 1])
                            .expect("write oversized line");
                        fs::set_permissions(&events, fs::Permissions::from_mode(0o600))
                            .expect("private oversized events");
                    }
                    _ => unreachable!(),
                }
            }
            let reopened = open_store(&root);
            assert!(
                reopened.open_project_writer(project_id.clone()).is_err(),
                "unsafe events fixture {fixture} must fail closed"
            );
        }
    }

    #[test]
    fn unsafe_checkpoint_cache_is_ignored_without_blocking_or_replacing_log_truth() {
        for fixture in ["fifo", "directory", "symlink", "weak_mode", "oversized"] {
            let root = tempfile::tempdir().expect("root");
            let store = open_store(&root);
            let project_id = project("project-1");
            let (writer, _) = append_ok(
                ready(&store, project_id.clone()),
                AppendRequest {
                    transaction_id: "transaction-1".to_owned(),
                    command_record: Some(command("command-1", 'a')),
                    events: vec![initialized(&project_id)],
                },
            );
            writer.write_checkpoint().expect("checkpoint");
            drop(writer);
            drop(store);
            let checkpoint = project_directory(&root, &project_id).join(CHECKPOINT_FILE);
            fs::remove_file(&checkpoint).expect("remove checkpoint");
            match fixture {
                "fifo" => rustix::fs::mkfifoat(CWD, &checkpoint, FILE_MODE)
                    .expect("create checkpoint FIFO"),
                "directory" => {
                    fs::create_dir(&checkpoint).expect("create checkpoint directory");
                    fs::set_permissions(&checkpoint, fs::Permissions::from_mode(0o700))
                        .expect("private checkpoint directory");
                }
                "symlink" => {
                    std::os::unix::fs::symlink("/dev/null", &checkpoint)
                        .expect("create checkpoint symlink");
                }
                "weak_mode" => {
                    fs::write(&checkpoint, b"{}").expect("write weak checkpoint");
                    fs::set_permissions(&checkpoint, fs::Permissions::from_mode(0o640))
                        .expect("weaken checkpoint");
                }
                "oversized" => {
                    fs::write(
                        &checkpoint,
                        vec![
                            b'x';
                            usize::try_from(MAX_CHECKPOINT_BYTES).expect("limit fits usize") + 1
                        ],
                    )
                    .expect("write oversized checkpoint");
                    fs::set_permissions(&checkpoint, fs::Permissions::from_mode(0o600))
                        .expect("private oversized checkpoint");
                }
                _ => unreachable!(),
            }
            let reopened = open_store(&root);
            let writer = ready(&reopened, project_id);
            assert_eq!(writer.snapshot().last_sequence, 1);
        }
    }

    #[test]
    fn append_reopen_replay_and_command_idempotency_are_exact() {
        let root = tempfile::tempdir().expect("root");
        let store = open_store(&root);
        let project_id = project("project-1");
        let writer = ready(&store, project_id.clone());
        let request = AppendRequest {
            transaction_id: "transaction-1".to_owned(),
            command_record: Some(command("command-1", 'a')),
            events: vec![initialized(&project_id)],
        };
        let (writer, first) = append_ok(writer, request);
        assert!(first.appended);
        assert_eq!(first.last_sequence, 1);
        assert_eq!(first.stable_reply, Some(reply("command-1")));
        let same_transaction_different_plan = AppendRequest {
            transaction_id: "transaction-1".to_owned(),
            command_record: Some(command("command-1", 'a')),
            events: vec![brief(&project_id)],
        };
        let writer = match writer.append(same_transaction_different_plan) {
            Err(AppendFailure::Rejected { writer, error }) => {
                assert!(matches!(error, StateStoreError::IdempotencyConflict));
                writer
            }
            _ => panic!("same transaction and command must not hide a different Project plan"),
        };
        let request = AppendRequest {
            transaction_id: "retry-is-not-written".to_owned(),
            command_record: Some(command("command-1", 'a')),
            events: vec![initialized(&project_id)],
        };
        let (writer, retry) = append_ok(writer, request);
        assert!(!retry.appended);
        assert_eq!(retry.stable_reply, Some(reply("command-1")));
        let before = writer.snapshot().clone();
        let conflict = AppendRequest {
            transaction_id: "conflict".to_owned(),
            command_record: Some(command("command-1", 'b')),
            events: vec![initialized(&project_id)],
        };
        let writer = match writer.append(conflict) {
            Err(AppendFailure::Rejected { writer, error }) => {
                assert!(matches!(error, StateStoreError::IdempotencyConflict));
                writer
            }
            _ => panic!("different payload must be rejected"),
        };
        assert_eq!(writer.snapshot(), &before);
        drop(writer);

        let reopened = ready(&store, project_id);
        assert_eq!(reopened.snapshot(), &before);
    }

    #[test]
    fn transaction_probe_same_plan_conflict_and_checkpoint_replay_are_exact() {
        let root = tempfile::tempdir().expect("root");
        let store = open_store(&root);
        let project_id = project("project-transaction-vector");
        let request = AppendRequest {
            transaction_id: "control-project-transaction-1".to_owned(),
            command_record: None,
            events: vec![initialized(&project_id)],
        };
        let digest = request
            .canonical_plan_digest(&project_id)
            .expect("canonical Project plan digest");
        assert_eq!(
            ready(&store, project_id.clone())
                .probe_transaction("control-project-transaction-1", &digest),
            TransactionProbe::Absent
        );

        let writer = ready(&store, project_id.clone());
        let (writer, first) = append_ok(writer, request);
        assert!(first.appended);
        let committed = match writer.probe_transaction("control-project-transaction-1", &digest) {
            TransactionProbe::SamePlanCommitted(committed) => committed,
            other => panic!("expected same committed Project plan, got {other:?}"),
        };
        assert_eq!(
            digest,
            "sha256:e40db4e92bca46fd05c2abc1db8640b6232dd8cf5d927100566b6da6641e5c84"
        );
        validate_sha256(&committed.resulting_batch_checksum)
            .expect("batch checksum uses the canonical SHA-256 form");
        assert_eq!(committed.resulting_last_sequence, 1);

        let retry = AppendRequest {
            transaction_id: "control-project-transaction-1".to_owned(),
            command_record: None,
            events: vec![initialized(&project_id)],
        };
        let (writer, retry) = append_ok(writer, retry);
        assert!(!retry.appended);
        assert_eq!(retry.last_sequence, 1);
        assert_eq!(
            writer.probe_transaction(
                "control-project-transaction-1",
                &format!("sha256:{}", "f".repeat(64))
            ),
            TransactionProbe::ConflictingPlan
        );

        let conflict = AppendRequest {
            transaction_id: "control-project-transaction-1".to_owned(),
            command_record: None,
            events: vec![brief(&project_id)],
        };
        let writer = match writer.append(conflict) {
            Err(AppendFailure::Rejected { writer, error }) => {
                assert!(matches!(error, StateStoreError::IdempotencyConflict));
                writer
            }
            _ => panic!("same transaction with a different plan must conflict"),
        };
        writer
            .write_checkpoint()
            .expect("checkpoint transaction index");
        let expected_transactions = writer.state.transactions.clone();
        drop(writer);
        drop(store);

        let store = open_store(&root);
        let checkpoint = ready(&store, project_id.clone());
        assert_eq!(checkpoint.state.transactions, expected_transactions);
        assert!(matches!(
            checkpoint.probe_transaction("control-project-transaction-1", &digest),
            TransactionProbe::SamePlanCommitted(_)
        ));
        drop(checkpoint);
        drop(store);

        let checkpoint_path = project_directory(&root, &project_id).join(CHECKPOINT_FILE);
        let mut tampered: StoredCheckpointV1 =
            serde_json::from_slice(&fs::read(&checkpoint_path).expect("read checkpoint"))
                .expect("decode checkpoint");
        tampered.transaction_index[0].canonical_plan_digest = format!("sha256:{}", "e".repeat(64));
        tampered.checksum = checkpoint_checksum(&tampered).expect("rechecksum tampered cache");
        fs::write(
            &checkpoint_path,
            serde_json::to_vec(&tampered).expect("encode tampered checkpoint"),
        )
        .expect("tamper checkpoint");
        let store = open_store(&root);
        let full_replay = ready(&store, project_id);
        assert_eq!(full_replay.state.transactions, expected_transactions);
    }

    #[test]
    fn artifact_recovery_remints_only_after_an_absent_probe_and_stops_after_commit() {
        let state_root = tempfile::tempdir().expect("state root");
        let artifact_root = tempfile::tempdir().expect("Artifact root");
        let state_store = open_store(&state_root);
        let (artifact_store, recovery_guard) =
            ArtifactStore::open_for_durable_runtime(artifact_root.path())
                .expect("runtime Artifact Store");
        let receipt = artifact_store
            .put(Cursor::new(b"fixture"), None)
            .expect("put");
        let audit_plan = receipt
            .recovery_audit_plan("control-tx:project-retry")
            .expect("audit plan");
        let artifact_record = receipt.into_record().expect("live receipt record");
        let stored_artifact = StoredProjectEventV1::from_domain(&ProjectEvent::ArtifactRegistered(
            artifact_record.clone(),
        ));
        let project_id = project("project-artifact-retry");
        let transaction_id = "control-project:artifact-retry";
        let planned_request = AppendRequest {
            transaction_id: transaction_id.to_owned(),
            command_record: None,
            events: vec![
                initialized(&project_id),
                ProjectEvent::ArtifactRegistered(artifact_record),
            ],
        };
        let plan_digest = planned_request
            .canonical_plan_digest(&project_id)
            .expect("plan digest");

        let mut writer = ready(&state_store, project_id.clone());
        assert_eq!(
            writer.probe_transaction(transaction_id, &plan_digest),
            TransactionProbe::Absent
        );
        let first_artifact_event = match recover_artifact_for_project_plan(
            &writer,
            transaction_id,
            &plan_digest,
            &artifact_store,
            &recovery_guard,
            audit_plan.control_transaction_id(),
            stored_artifact.clone(),
            &audit_plan,
        )
        .expect("recover absent Project plan")
        {
            RecoveredArtifactProjectHandoff::Append(event) => event,
            RecoveredArtifactProjectHandoff::AlreadyCommitted(_) => {
                panic!("absent transaction must require append")
            }
        };
        writer.set_failpoint(AppendFailpoint::BeforeWrite);
        let writer = match writer.append(AppendRequest {
            transaction_id: transaction_id.to_owned(),
            command_record: None,
            events: vec![initialized(&project_id), first_artifact_event],
        }) {
            Err(AppendFailure::Rejected { writer, .. }) => writer,
            _ => panic!("injected pre-write failure must reject without committing"),
        };
        drop(writer);

        let writer = ready(&state_store, project_id.clone());
        assert_eq!(
            writer.probe_transaction(transaction_id, &plan_digest),
            TransactionProbe::Absent
        );
        let retry_artifact_event = match recover_artifact_for_project_plan(
            &writer,
            transaction_id,
            &plan_digest,
            &artifact_store,
            &recovery_guard,
            audit_plan.control_transaction_id(),
            stored_artifact,
            &audit_plan,
        )
        .expect("recover retry Project plan")
        {
            RecoveredArtifactProjectHandoff::Append(event) => event,
            RecoveredArtifactProjectHandoff::AlreadyCommitted(_) => {
                panic!("absent retry must re-mint")
            }
        };
        let (writer, outcome) = append_ok(
            writer,
            AppendRequest {
                transaction_id: transaction_id.to_owned(),
                command_record: None,
                events: vec![initialized(&project_id), retry_artifact_event],
            },
        );
        assert!(outcome.appended);
        assert!(matches!(
            writer.probe_transaction(transaction_id, &plan_digest),
            TransactionProbe::SamePlanCommitted(_)
        ));
        let committed_record = writer
            .snapshot()
            .artifacts
            .values()
            .next()
            .expect("registered Artifact")
            .clone();
        assert!(matches!(
            recover_artifact_for_project_plan(
                &writer,
                transaction_id,
                &plan_digest,
                &artifact_store,
                &recovery_guard,
                audit_plan.control_transaction_id(),
                StoredProjectEventV1::from_domain(&ProjectEvent::ArtifactRegistered(
                    committed_record
                )),
                &audit_plan,
            )
            .expect("same plan is already committed"),
            RecoveredArtifactProjectHandoff::AlreadyCommitted(_)
        ));
        assert_eq!(writer.snapshot().artifacts.len(), 1);
    }

    #[test]
    fn checksum_recomputed_invalid_stored_values_still_fail_domain_conversion() {
        let fixtures = [
            serde_json::json!({
                "type": "project_initialized",
                "value": {
                    "project_id": "../escape",
                    "score_id": "score-1",
                    "default_take_id": "take-1",
                    "default_branch_id": "branch-1"
                }
            }),
            serde_json::json!({
                "type": "brief_revision_created",
                "value": {
                    "id": "brief-1",
                    "project_id": "project-1",
                    "previous": null,
                    "user_description": "line\nbreak",
                    "goals": [],
                    "instrumentation": [],
                    "open_questions": []
                }
            }),
            serde_json::json!({
                "type": "fixture_artifact_declared",
                "value": {
                    "hash": "sha256:NOT-CANONICAL",
                    "size": 1,
                    "availability": "FixtureOnly",
                    "layout_version": null,
                    "store_instance_id": null,
                    "durability": null,
                    "store_commit_identity": null
                }
            }),
            serde_json::json!({
                "type": "constraint_declared",
                "value": {
                    "id": "constraint-1",
                    "brief_revision_id": "brief-1",
                    "strength": "Hard",
                    "description": "playable",
                    "machine_rule": ["range", 0],
                    "scope": "WholeScore"
                }
            }),
            serde_json::json!({
                "type": "constraint_declared",
                "value": {
                    "id": "constraint-1",
                    "brief_revision_id": "brief-1",
                    "strength": "Hard",
                    "description": "playable",
                    "machine_rule": ["range", 1],
                    "scope": {"StablePart": "../part"}
                }
            }),
            serde_json::json!({
                "type": "revision_created",
                "value": {
                    "id": "revision-1",
                    "project_id": "project-1",
                    "score_id": "score-1",
                    "take_id": "take-1",
                    "branch_id": "branch-1",
                    "parents": ["bad/parent"],
                    "brief_revision_id": "brief-1",
                    "source_artifact": format!("sha256:{}", "a".repeat(64)),
                    "ir_artifact": null,
                    "origin": "Agent"
                }
            }),
            serde_json::json!({
                "type": "evidence_recorded",
                "value": {
                    "id": "evidence-1",
                    "revision_id": "revision-1",
                    "subject_hash": format!("sha256:{}", "a".repeat(64)),
                    "subject": {"Constraint": ["bad/id", "WholeScore"]},
                    "outcome": "Pass",
                    "producer": "Tool",
                    "method": "lint",
                    "artifact_refs": [],
                    "created_at": "now"
                }
            }),
        ];
        for event in fixtures {
            let stored = serde_json::from_value::<StoredProjectEventV1>(event)
                .expect("primitive stored event shape");
            let mut batch = StoredProjectBatchV1 {
                schema_version: 1,
                project_id: "project-1".to_owned(),
                stream_id: "stream-1".to_owned(),
                epoch: 1,
                transaction_id: "transaction-1".to_owned(),
                first_sequence: 1,
                last_sequence: 1,
                command_record: None,
                events: vec![stored],
                previous_batch_checksum: None,
                batch_checksum: String::new(),
            };
            batch.batch_checksum = batch_checksum(&batch).expect("recompute checksum");
            let mut state = empty_recovered(project("project-1"));
            assert!(matches!(
                apply_batch(&mut state, &batch),
                Err(StateStoreError::ProjectionRejected)
            ));
            assert_eq!(state.last_sequence, 0);
            assert!(state.events.is_empty());
        }
    }

    #[test]
    fn partial_tail_poison_holds_lease_until_consuming_repair() {
        let root = tempfile::tempdir().expect("root");
        let store = open_store(&root);
        let project_id = project("project-1");
        let mut writer = ready(&store, project_id.clone());
        writer.set_failpoint(AppendFailpoint::PartialWrite(17));
        let poisoned = match writer.append(AppendRequest {
            transaction_id: "transaction-1".to_owned(),
            command_record: Some(command("command-1", 'a')),
            events: vec![initialized(&project_id)],
        }) {
            Err(AppendFailure::Poisoned { writer, .. }) => writer,
            _ => panic!("partial write must poison"),
        };
        assert_eq!(poisoned.project_id, project_id);
        assert!(poisoned.lease.matches(&project_id));
        assert!(matches!(
            store.open_project_writer(project_id.clone()),
            Err(StateStoreError::WriterLockRequired)
        ));
        let unrelated = ready(&store, project("project-2"));
        assert_eq!(unrelated.snapshot().last_sequence, 0);
        drop(unrelated);
        let repair = match poisoned.recover() {
            RecoveryOutcome::RepairRequired(repair) => repair,
            _ => panic!("partial tail must require repair"),
        };
        assert!(matches!(
            store.open_project_writer(project_id.clone()),
            Err(StateStoreError::WriterLockRequired)
        ));
        let writer = match repair.repair() {
            Ok(writer) => writer,
            Err(_) => panic!("repair must succeed"),
        };
        let (writer, outcome) = append_ok(
            writer,
            AppendRequest {
                transaction_id: "transaction-1".to_owned(),
                command_record: Some(command("command-1", 'a')),
                events: vec![initialized(&project_id)],
            },
        );
        assert_eq!(outcome.last_sequence, 1);
        drop(writer);
        assert_eq!(ready(&store, project_id).snapshot().last_sequence, 1);
        assert_eq!(
            ready(&store, project("project-2")).snapshot().last_sequence,
            0
        );
    }

    #[test]
    fn synced_line_before_response_recovers_committed_reply_without_duplicate() {
        let root = tempfile::tempdir().expect("root");
        let store = open_store(&root);
        let project_id = project("project-1");
        let mut writer = ready(&store, project_id.clone());
        writer.set_failpoint(AppendFailpoint::AfterSyncBeforeUpdate);
        let poisoned = match writer.append(AppendRequest {
            transaction_id: "transaction-1".to_owned(),
            command_record: Some(command("command-1", 'a')),
            events: vec![initialized(&project_id)],
        }) {
            Err(AppendFailure::Poisoned { writer, .. }) => writer,
            _ => panic!("response failure must poison"),
        };
        let writer = match poisoned.recover() {
            RecoveryOutcome::Ready(writer) => writer,
            _ => panic!("synced complete line must recover"),
        };
        let (writer, retry) = append_ok(
            writer,
            AppendRequest {
                transaction_id: "retry".to_owned(),
                command_record: Some(command("command-1", 'a')),
                events: vec![initialized(&project_id)],
            },
        );
        assert!(!retry.appended);
        assert_eq!(retry.stable_reply, Some(reply("command-1")));
        assert_eq!(writer.snapshot().last_sequence, 1);
    }

    #[test]
    fn append_file_sync_error_never_returns_success_and_requires_rescan() {
        let root = tempfile::tempdir().expect("root");
        let store = open_store(&root);
        let project_id = project("project-1");
        let mut writer = ready(&store, project_id.clone());
        writer.set_failpoint(AppendFailpoint::FileSyncError);
        let poisoned = match writer.append(AppendRequest {
            transaction_id: "transaction-1".to_owned(),
            command_record: Some(command("command-1", 'a')),
            events: vec![initialized(&project_id)],
        }) {
            Err(AppendFailure::Poisoned { writer, error }) => {
                assert!(matches!(
                    error,
                    StateStoreError::Io {
                        operation: "sync Project batch",
                        ..
                    }
                ));
                writer
            }
            _ => panic!("injected file sync error must poison without success"),
        };
        assert!(matches!(
            store.open_project_writer(project_id.clone()),
            Err(StateStoreError::WriterLockRequired)
        ));
        let writer = match poisoned.recover() {
            RecoveryOutcome::Ready(writer) => writer,
            _ => panic!("complete visible line must be decided only by rescan"),
        };
        let (writer, retry) = append_ok(
            writer,
            AppendRequest {
                transaction_id: "retry-not-written".to_owned(),
                command_record: Some(command("command-1", 'a')),
                events: vec![initialized(&project_id)],
            },
        );
        assert!(!retry.appended);
        assert_eq!(retry.stable_reply, Some(reply("command-1")));
        assert_eq!(writer.snapshot().last_sequence, 1);
    }

    #[test]
    fn complete_project_line_requires_a_successful_recovery_sync() {
        let root = tempfile::tempdir().expect("root");
        let store = open_store(&root);
        let project_id = project("project-recovery-sync");
        let mut writer = ready(&store, project_id.clone());
        writer.set_failpoint(AppendFailpoint::FileSyncError);
        let Err(AppendFailure::Poisoned {
            writer: mut poisoned,
            ..
        }) = writer.append(AppendRequest {
            transaction_id: "transaction-recovery-sync".to_owned(),
            command_record: Some(command("command-recovery-sync", 'a')),
            events: vec![initialized(&project_id)],
        })
        else {
            panic!("file sync failure must poison a complete Project line");
        };
        poisoned.set_recovery_failpoint(RecoveryFailpoint::FileSync);
        assert!(matches!(poisoned.recover(), RecoveryOutcome::Corrupt(_)));
    }

    #[test]
    fn newline_terminated_corruption_is_not_repairable_tail() {
        let root = tempfile::tempdir().expect("root");
        let store = open_store(&root);
        let project_id = project("project-1");
        let writer = ready(&store, project_id.clone());
        let (writer, _) = append_ok(
            writer,
            AppendRequest {
                transaction_id: "transaction-1".to_owned(),
                command_record: Some(command("command-1", 'a')),
                events: vec![initialized(&project_id)],
            },
        );
        drop(writer);
        let key = project_key(&project_id);
        let path = root
            .path()
            .join(STATE_LAYOUT)
            .join("projects")
            .join(key)
            .join(EVENTS_FILE);
        fs::write(&path, b"{\"corrupt\":true}\n").expect("tamper complete line");
        assert!(matches!(
            store.open_project_writer(project_id),
            Err(StateStoreError::MiddleCorruption)
        ));
    }

    #[test]
    fn repair_failpoints_preserve_committed_prefix_and_never_return_ready_on_error() {
        for failpoint in [
            RepairFailpoint::RescanRace,
            RepairFailpoint::TruncateError,
            RepairFailpoint::FileSyncError,
            RepairFailpoint::DirectorySyncError,
        ] {
            let root = tempfile::tempdir().expect("root");
            let store = open_store(&root);
            let project_id = project("project-1");
            let (mut writer, _) = append_ok(
                ready(&store, project_id.clone()),
                AppendRequest {
                    transaction_id: "transaction-1".to_owned(),
                    command_record: Some(command("command-1", 'a')),
                    events: vec![initialized(&project_id)],
                },
            );
            writer.set_failpoint(AppendFailpoint::PartialWrite(19));
            let poisoned = match writer.append(AppendRequest {
                transaction_id: "transaction-2".to_owned(),
                command_record: Some(command("command-2", 'b')),
                events: vec![brief(&project_id)],
            }) {
                Err(AppendFailure::Poisoned { writer, .. }) => writer,
                _ => panic!("partial second batch must poison"),
            };
            let mut repair = match poisoned.recover() {
                RecoveryOutcome::RepairRequired(repair) => repair,
                _ => panic!("partial second batch must require repair"),
            };
            repair.set_failpoint(failpoint);
            let corrupt = match repair.repair() {
                Err(corrupt) => corrupt,
                Ok(_) => panic!("repair failpoint must never return Ready"),
            };
            assert!(matches!(
                store.open_project_writer(project_id.clone()),
                Err(StateStoreError::WriterLockRequired)
            ));
            drop(corrupt);

            match store
                .open_project_writer(project_id.clone())
                .expect("reopen after dropping corrupt typestate")
            {
                OpenProjectWriter::Ready(writer) => {
                    assert!(matches!(
                        failpoint,
                        RepairFailpoint::FileSyncError | RepairFailpoint::DirectorySyncError
                    ));
                    assert_eq!(writer.snapshot().last_sequence, 1);
                    drop(writer);
                }
                OpenProjectWriter::RepairRequired(repair) => {
                    assert!(matches!(
                        failpoint,
                        RepairFailpoint::RescanRace | RepairFailpoint::TruncateError
                    ));
                    drop(repair);
                }
            }
        }
    }

    #[test]
    fn checkpoint_replays_valid_tail_and_preserves_exact_command_index() {
        let root = tempfile::tempdir().expect("root");
        let store = open_store(&root);
        let project_id = project("project-1");
        let (writer, _) = append_ok(
            ready(&store, project_id.clone()),
            AppendRequest {
                transaction_id: "transaction-1".to_owned(),
                command_record: Some(command("command-1", 'a')),
                events: vec![initialized(&project_id)],
            },
        );
        writer.write_checkpoint().expect("checkpoint");
        let (writer, _) = append_ok(
            writer,
            AppendRequest {
                transaction_id: "transaction-2".to_owned(),
                command_record: Some(command("command-2", 'b')),
                events: vec![brief(&project_id)],
            },
        );
        let expected = writer.snapshot().clone();
        drop(writer);

        let writer = ready(&store, project_id.clone());
        assert_eq!(writer.snapshot(), &expected);
        let (writer, retry) = append_ok(
            writer,
            AppendRequest {
                transaction_id: "not-written".to_owned(),
                command_record: Some(command("command-1", 'a')),
                events: vec![initialized(&project_id)],
            },
        );
        assert!(!retry.appended);
        assert_eq!(retry.stable_reply, Some(reply("command-1")));
        drop(writer);
    }

    #[test]
    fn corrupt_checkpoint_falls_back_to_full_log_replay() {
        let root = tempfile::tempdir().expect("root");
        let store = open_store(&root);
        let project_id = project("project-1");
        let (writer, _) = append_ok(
            ready(&store, project_id.clone()),
            AppendRequest {
                transaction_id: "transaction-1".to_owned(),
                command_record: Some(command("command-1", 'a')),
                events: vec![initialized(&project_id)],
            },
        );
        let expected = writer.snapshot().clone();
        writer.write_checkpoint().expect("checkpoint");
        drop(writer);
        let checkpoint = root
            .path()
            .join(STATE_LAYOUT)
            .join("projects")
            .join(project_key(&project_id))
            .join(CHECKPOINT_FILE);
        fs::write(checkpoint, b"{not-json").expect("corrupt checkpoint");
        let reopened = ready(&store, project_id);
        assert_eq!(reopened.snapshot(), &expected);
    }

    #[test]
    fn checkpoint_failures_never_damage_authoritative_log() {
        for failpoint in [
            CheckpointFailpoint::TempCreate,
            CheckpointFailpoint::TempWrite,
            CheckpointFailpoint::FileSync,
            CheckpointFailpoint::BeforeInstall,
            CheckpointFailpoint::AfterInstall,
            CheckpointFailpoint::DirectorySyncError,
        ] {
            let root = tempfile::tempdir().expect("root");
            let store = open_store(&root);
            let project_id = project("project-1");
            let (mut writer, _) = append_ok(
                ready(&store, project_id.clone()),
                AppendRequest {
                    transaction_id: "transaction-1".to_owned(),
                    command_record: Some(command("command-1", 'a')),
                    events: vec![initialized(&project_id)],
                },
            );
            let expected = writer.snapshot().clone();
            writer.set_checkpoint_failpoint(failpoint);
            assert!(writer.write_checkpoint().is_err());
            drop(writer);
            let reopened = ready(&store, project_id);
            assert_eq!(reopened.snapshot(), &expected);
        }
    }
}
