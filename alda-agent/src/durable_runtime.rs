//! B4 durable composition root and instance-lock capability.
//!
//! Production command wiring is intentionally deferred to B4c. This module
//! owns the process-wide lock and the private health capability consumed by
//! the B2/B3/control writers.

#![allow(
    dead_code,
    reason = "B4b1 freezes the durable runtime foundation before B4c command wiring"
)]
#![allow(
    clippy::missing_errors_doc,
    reason = "the runtime exposes one typed startup/recovery error boundary"
)]

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rand::RngCore as _;
use rustix::fs::{CWD, FlockOperation, Mode, OFlags, flock, fstat, fsync, openat};
use thiserror::Error;

use crate::artifact_store::{ArtifactRecoveryGuard, ArtifactStore};
use crate::control_store::{
    AggregateCommitV1, CommitControlRequest, ControlAppendFailure, ControlRecoveryOutcome,
    OpenControlWriter, PrepareControlRequest, PreparedTransactionV1, ReadyControlWriter,
    open_control_writer,
};
#[cfg(test)]
use crate::control_store::{ControlAppendFailpoint, ControlRecoveryFailpoint};
use crate::domain::DomainProjectId;
use crate::protocol::SessionSnapshot;
use crate::state::ProjectSnapshot;
use crate::state_store::session::{
    OpenSessionWriter, ReadySessionWriter, SessionAppendFailure, SessionRecoveryOutcome,
};
use crate::state_store::{
    AppendFailure, OpenProjectWriter, ReadyProjectWriter, RecoveredArtifactProjectHandoff,
    RecoveryOutcome, StateStore, StateStoreInstanceLease, StoredProjectPlanV1, StoredSessionPlanV1,
    TransactionCommit, TransactionProbe, recover_artifact_for_project_plan,
};

const INSTANCE_LOCK_FILE: &str = "instance-lock-v1";
const ROOT_MODE: u32 = 0o700;
const LOCK_MODE: u32 = 0o600;
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

/// Shared liveness witness. Only the composition root owns the strong value;
/// stores receive `Weak<LockHealth>` and cannot mint or reactivate it.
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

/// The non-cloneable kernel advisory lock. Its file descriptor is never
/// exposed, duplicated, or moved into an individual store.
struct InstanceLock {
    root: OwnedFd,
    file: File,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LockFailpoint {
    Create,
    Write,
    FileSync,
    DirectorySync,
}

impl InstanceLock {
    fn acquire(root_path: &Path) -> Result<Self, DurableRuntimeError> {
        Self::acquire_inner(root_path, None)
    }

    fn acquire_inner(
        root_path: &Path,
        #[cfg_attr(not(test), allow(unused_variables))] failpoint: Option<LockFailpoint>,
    ) -> Result<Self, DurableRuntimeError> {
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
            Ok(Self { root, file })
        }
    }
}

/// Immutable state published only after the control commit and a strict
/// catalog replay both succeed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct DurableReadView {
    pub projects: BTreeMap<String, ProjectSnapshot>,
    pub sessions: BTreeMap<String, SessionSnapshot>,
}

/// The only owner of the non-cloneable lock and all durable components.
///
/// Components are held in an `Option` so `Drop` can enforce the required
/// shutdown order: writers/stores, health invalidation, then kernel lock.
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
}

impl Drop for RuntimeCore {
    fn drop(&mut self) {
        drop(self.components.take());
        self.health.invalidate();
        drop(self.instance_lock.take());
    }
}

/// Runtime typestate exposed after startup recovery has completely converged.
pub(crate) struct ReadyDurableRuntime {
    core: RuntimeCore,
    published: DurableReadView,
    #[cfg(test)]
    failpoint: Option<RuntimeFailpoint>,
}

/// Runtime typestate after a durable Prepared fact exists but publication has
/// not completed. It deliberately exposes no query or command methods.
pub(crate) struct RecoveringDurableRuntime {
    core: RuntimeCore,
    prepared: PreparedTransactionV1,
    #[cfg(test)]
    failpoint: Option<RuntimeFailpoint>,
}

/// Terminal typestate. It retains the composition root only long enough to
/// preserve orderly shutdown and exposes no state operation.
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
enum RuntimeFailpoint {
    Prepare,
    Project,
    Session,
    Commit,
    CommitRecoverySync,
    Publish,
    Response,
}

impl ReadyDurableRuntime {
    /// Acquires the instance lock, opens every durable component, completes
    /// pending redo in control order, then publishes one committed read view.
    pub(crate) fn open(root_path: &Path) -> Result<Self, DurableRuntimeError> {
        let instance_lock = InstanceLock::acquire(root_path)?;
        let health = Arc::new(LockHealth::new());
        let (artifact_store, artifact_recovery_guard) =
            ArtifactStore::open_for_durable_runtime(root_path)
                .map_err(|error| component_error("artifact store", error))?;
        let state_store =
            StateStore::open(root_path, StateStoreInstanceLease::for_durable_runtime())
                .map_err(|error| component_error("state store", error))?;
        let control_writer = match open_control_writer(root_path, Arc::downgrade(&health))
            .map_err(|error| component_error("control store", error))?
        {
            OpenControlWriter::Ready(writer) => writer,
            OpenControlWriter::RepairRequired(writer) => writer
                .repair()
                .map_err(|_| component_error("control store", "unrepairable final tail"))?,
        };
        let mut core = RuntimeCore {
            components: Some(RuntimeComponents {
                artifact_store,
                artifact_recovery_guard,
                state_store,
                control_writer: Some(control_writer),
            }),
            health,
            instance_lock: Some(instance_lock),
        };

        core.require_lock()?;
        core.validate_catalog(false)?;
        let pending = core.control()?.projection().pending();
        for prepared in pending {
            core.redo_and_commit(&prepared)?;
        }
        core.audit_committed_transactions()?;
        let published = core.rebuild_published_view()?;
        Ok(Self {
            core,
            published,
            #[cfg(test)]
            failpoint: None,
        })
    }

    pub(crate) fn read_view(&self) -> &DurableReadView {
        &self.published
    }

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
        let writer = self.core.take_control();
        let (writer, _outcome) = match writer.prepare(request) {
            Ok(success) => success,
            Err(ControlAppendFailure::Rejected { writer, error }) => {
                self.core.put_control(writer);
                return Err(SubmitFailure::Rejected {
                    runtime: Box::new(self),
                    error: component_error("control prepare", error),
                });
            }
            Err(ControlAppendFailure::Poisoned { writer, error }) => {
                let original = component_error("control prepare", error);
                match recover_control_ready(writer.recover()) {
                    Ok(writer) => {
                        self.core.put_control(writer);
                        let prepared = self.core.prepared_for_command(&command_key).cloned();
                        return match prepared {
                            Some(prepared) => Err(SubmitFailure::Recovering {
                                runtime: Box::new(RecoveringDurableRuntime {
                                    core: self.core,
                                    prepared,
                                    #[cfg(test)]
                                    failpoint: self.failpoint,
                                }),
                                error: original,
                            }),
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
        self.core.put_control(writer);

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

        let recovering = Box::new(RecoveringDurableRuntime {
            core: self.core,
            prepared: authoritative,
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
        mut self,
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
        self.published = match self.core.rebuild_published_view() {
            Ok(view) => view,
            Err(error) => {
                return Err(SubmitFailure::Fatal(Box::new(FatalDurableRuntime::new(
                    self.core, error,
                ))));
            }
        };
        Ok((self, reply))
    }

    #[cfg(test)]
    fn set_failpoint(&mut self, failpoint: RuntimeFailpoint) {
        self.failpoint = Some(failpoint);
    }
}

impl RecoveringDurableRuntime {
    /// Retries exactly the authoritative transaction retained by this state.
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
        if let Err(error) = self.core.redo_project(&self.prepared) {
            return Err(FinishFailure::Fatal(Box::new(FatalDurableRuntime::new(
                self.core, error,
            ))));
        }
        #[cfg(test)]
        if self.failpoint == Some(RuntimeFailpoint::Session) && self.prepared.session_plan.is_some()
        {
            return Err(FinishFailure::Recovering {
                runtime: self,
                error: injected("Session append"),
            });
        }
        if let Err(error) = self.core.redo_session(&self.prepared) {
            return Err(FinishFailure::Fatal(Box::new(FatalDurableRuntime::new(
                self.core, error,
            ))));
        }
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
        if let Err(error) = self
            .core
            .commit_prepared(&self.prepared, fail_recovery_sync)
        {
            return Err(FinishFailure::Fatal(Box::new(FatalDurableRuntime::new(
                self.core, error,
            ))));
        }
        #[cfg(test)]
        if self.failpoint == Some(RuntimeFailpoint::Publish) {
            return Err(FinishFailure::Recovering {
                runtime: self,
                error: injected("published view"),
            });
        }
        let published = match self.core.rebuild_published_view() {
            Ok(view) => view,
            Err(error) => {
                return Err(FinishFailure::Fatal(Box::new(FatalDurableRuntime::new(
                    self.core, error,
                ))));
            }
        };
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
        Self { _core: core, error }
    }

    pub(crate) fn error(&self) -> &DurableRuntimeError {
        &self.error
    }
}

impl RuntimeCore {
    fn components(&self) -> &RuntimeComponents {
        self.components
            .as_ref()
            .expect("runtime components exist until Drop")
    }

    fn components_mut(&mut self) -> &mut RuntimeComponents {
        self.components
            .as_mut()
            .expect("runtime components exist until Drop")
    }

    fn require_lock(&self) -> Result<(), DurableRuntimeError> {
        if self.instance_lock.is_none() {
            return Err(DurableRuntimeError::InstanceLockLost);
        }
        self.health.require_live()
    }

    fn control(&self) -> Result<&ReadyControlWriter, DurableRuntimeError> {
        self.components()
            .control_writer
            .as_ref()
            .ok_or_else(|| component_error("control store", "writer unavailable"))
    }

    fn take_control(&mut self) -> ReadyControlWriter {
        self.components_mut()
            .control_writer
            .take()
            .expect("Ready runtime owns control writer")
    }

    fn put_control(&mut self, writer: ReadyControlWriter) {
        let previous = self.components_mut().control_writer.replace(writer);
        debug_assert!(previous.is_none());
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
            .components()
            .state_store
            .open_project_writer(project_id.clone())
            .map_err(|error| component_error("Project open", error))?
        {
            OpenProjectWriter::Ready(writer) => writer,
            OpenProjectWriter::RepairRequired(writer) => writer
                .repair()
                .map_err(|_| component_error("Project repair", "unrepairable final tail"))?,
        };
        let result = redo_project_plan(
            writer,
            plan,
            &self.components().artifact_store,
            &self.components().artifact_recovery_guard,
            &prepared.global_tx_id,
            &prepared.artifact_audit_plans,
        );
        result.map(Some)
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
            .components()
            .state_store
            .open_session_writer(session_id)
            .map_err(|error| component_error("Session open", error))?
        {
            OpenSessionWriter::Ready(writer) => writer,
            OpenSessionWriter::RepairRequired(writer) => writer
                .repair()
                .map_err(|_| component_error("Session repair", "unrepairable final tail"))?,
        };
        redo_session_plan(writer, plan).map(Some)
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
        #[cfg(test)]
        let mut writer = self.take_control();
        #[cfg(not(test))]
        let writer = self.take_control();
        #[cfg(test)]
        if fail_recovery_sync {
            writer.set_failpoint(ControlAppendFailpoint::FileSync);
        }
        match writer.commit(request) {
            Ok((writer, _)) => {
                self.put_control(writer);
                Ok(())
            }
            Err(ControlAppendFailure::Rejected { writer, error }) => {
                self.put_control(writer);
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
                self.put_control(writer);
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
            .components()
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
            .components()
            .state_store
            .open_session_writer(session_id)
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

    fn validate_catalog(&self, strict: bool) -> Result<(), DurableRuntimeError> {
        let control = self.control()?.projection();
        let projects = self
            .components()
            .state_store
            .list_projects()
            .map_err(|error| component_error("Project catalog", error))?;
        let sessions = self
            .components()
            .state_store
            .list_sessions()
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

    /// Proves that every control `Committed` anchor still names the exact
    /// Project/Session transaction recorded by the aggregate log. Catalog
    /// presence alone is insufficient because a complete aggregate tail can
    /// be truncated without removing its directory.
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
                .components()
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
                .components()
                .state_store
                .open_session_writer(crate::protocol::SessionId(session_id))
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

    fn rebuild_published_view(&self) -> Result<DurableReadView, DurableRuntimeError> {
        self.validate_catalog(true)?;
        let projects = self
            .components()
            .state_store
            .list_projects()
            .map_err(|error| component_error("Project catalog", error))?
            .projects;
        let sessions = self
            .components()
            .state_store
            .list_sessions()
            .map_err(|error| component_error("Session catalog", error))?
            .sessions;
        Ok(DurableReadView { projects, sessions })
    }
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
            || (allow_legacy_restart && transaction_id.starts_with("restart-v1:"))
        {
            continue;
        }
        return Err(DurableRuntimeError::TransactionConflict);
    }
    Ok(())
}

fn redo_project_plan(
    writer: ReadyProjectWriter,
    plan: StoredProjectPlanV1,
    artifact_store: &ArtifactStore,
    recovery_guard: &ArtifactRecoveryGuard,
    global_tx_id: &str,
    audit_plans: &[crate::artifact_store::ArtifactAuditPlanV1],
) -> Result<TransactionCommit, DurableRuntimeError> {
    let digest = plan.canonical_plan_digest().to_owned();
    let transaction_id = plan.transaction_id().to_owned();
    match writer.probe_transaction(&transaction_id, &digest) {
        TransactionProbe::SamePlanCommitted(commit) => return Ok(commit),
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
            RecoveredArtifactProjectHandoff::AlreadyCommitted(commit) => return Ok(commit),
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
                TransactionProbe::SamePlanCommitted(commit) => Ok(commit),
                TransactionProbe::Absent => Err(original),
                TransactionProbe::ConflictingPlan => Err(DurableRuntimeError::TransactionConflict),
            };
        }
    };
    match writer.probe_transaction(&transaction_id, &digest) {
        TransactionProbe::SamePlanCommitted(commit) => Ok(commit),
        TransactionProbe::Absent | TransactionProbe::ConflictingPlan => {
            Err(DurableRuntimeError::TransactionConflict)
        }
    }
}

fn redo_session_plan(
    writer: ReadySessionWriter,
    plan: StoredSessionPlanV1,
) -> Result<TransactionCommit, DurableRuntimeError> {
    let digest = plan.canonical_plan_digest().to_owned();
    let transaction_id = plan.transaction_id().to_owned();
    match writer.probe_transaction(&transaction_id, &digest) {
        TransactionProbe::SamePlanCommitted(commit) => return Ok(commit),
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
                TransactionProbe::SamePlanCommitted(commit) => Ok(commit),
                TransactionProbe::Absent => Err(original),
                TransactionProbe::ConflictingPlan => Err(DurableRuntimeError::TransactionConflict),
            };
        }
    };
    match writer.probe_transaction(&transaction_id, &digest) {
        TransactionProbe::SamePlanCommitted(commit) => Ok(commit),
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
    use std::os::unix::fs::PermissionsExt as _;

    use crate::control_store::{
        PrepareControlRequest, PreparedTransactionV1, SessionAllocation, project_transaction_id,
        session_transaction_id,
    };
    use crate::domain::{
        BranchId, BriefRevisionId, CreativeBrief, DomainProjectId, ProjectEvent, ScoreId, TakeId,
    };
    use crate::protocol::{ClientCommandId, ProjectId, ProtocolErrorCode, SessionId};
    use crate::state_store::session::{SessionAppendRequest, SessionRolloutEvent};
    use crate::state_store::{
        AppendRequest, OpenProjectWriter, StoredCommandRecordV1, StoredProjectPlanV1,
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

    fn combined_prepare(command_id: &str) -> PrepareControlRequest {
        let reply = stable_reply(command_id);
        let command = StoredCommandRecordV1::new(
            "client-runtime",
            command_id,
            format!("sha256:{}", "3".repeat(64)),
            &reply,
        )
        .expect("command record");
        let project_id = domain_project("project-runtime");
        let project_request = AppendRequest {
            transaction_id: project_transaction_id(GLOBAL_TX),
            command_record: Some(command.clone()),
            events: vec![ProjectEvent::ProjectInitialized {
                project_id: project_id.clone(),
                score_id: ScoreId::parse("score-runtime").expect("score"),
                default_take_id: TakeId::parse("take-runtime").expect("take"),
                default_branch_id: BranchId::parse("branch-runtime").expect("branch"),
            }],
        };
        let project_plan =
            StoredProjectPlanV1::from_append_request(&project_id, 0, None, &project_request)
                .expect("Project plan");
        let session_id = SessionId("session-runtime".to_owned());
        let session_request = SessionAppendRequest::new(
            session_transaction_id(GLOBAL_TX),
            Some(command.clone()),
            vec![SessionRolloutEvent::SessionStarted {
                session_id: session_id.clone(),
                project_id: ProjectId(project_id.as_str().to_owned()),
            }],
        );
        let session_plan = StoredSessionPlanV1::from_append_request(
            &session_id,
            &ProjectId(project_id.as_str().to_owned()),
            0,
            None,
            &session_request,
        )
        .expect("Session plan");
        let prepared = PreparedTransactionV1::new(
            GLOBAL_TX.to_owned(),
            command,
            Some(project_plan),
            Some(session_plan),
            Vec::new(),
        )
        .expect("Prepared");
        PrepareControlRequest {
            project_allocation: Some(project_id),
            session_allocation: Some(SessionAllocation {
                session_id,
                project_id: ProjectId("project-runtime".to_owned()),
            }),
            prepared,
        }
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
        let request = combined_prepare("command-runtime");
        let expected_reply = stable_reply("command-runtime");
        let runtime = ReadyDurableRuntime::open(root.path()).expect("open runtime");
        let (runtime, reply) = submit_ok(runtime, request.clone());
        assert_eq!(reply, expected_reply);
        assert_eq!(runtime.read_view().projects.len(), 1);
        assert_eq!(runtime.read_view().sessions.len(), 1);
        drop(runtime);

        let runtime = ReadyDurableRuntime::open(root.path()).expect("reopen runtime");
        let (runtime, retry_reply) = submit_ok(runtime, request);
        assert_eq!(retry_reply, expected_reply);
        assert_eq!(runtime.read_view().projects.len(), 1);
        assert_eq!(runtime.read_view().sessions.len(), 1);
    }

    #[test]
    fn every_post_prepare_cut_is_completed_before_reopen_is_ready() {
        for failpoint in [
            RuntimeFailpoint::Project,
            RuntimeFailpoint::Session,
            RuntimeFailpoint::Commit,
            RuntimeFailpoint::Publish,
            RuntimeFailpoint::Response,
        ] {
            let root = private_root();
            let request = combined_prepare("command-runtime");
            let mut runtime = ReadyDurableRuntime::open(root.path()).expect("open runtime");
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
    fn control_commit_recovery_sync_failure_is_fatal_without_a_missing_writer_panic() {
        let root = private_root();
        let request = combined_prepare("command-control-recovery-sync");
        let mut runtime = ReadyDurableRuntime::open(root.path()).expect("open runtime");
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
    fn pre_prepare_failure_has_no_authoritative_catalog_or_transaction() {
        let root = private_root();
        let request = combined_prepare("command-runtime");
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
        assert_eq!(reply, stable_reply("command-runtime"));
    }

    #[test]
    fn recovering_retries_only_its_authoritative_transaction() {
        let root = private_root();
        let request = combined_prepare("command-runtime");
        let mut runtime = ReadyDurableRuntime::open(root.path()).expect("open runtime");
        runtime.set_failpoint(RuntimeFailpoint::Session);
        let Err(failure) = runtime.submit(request) else {
            panic!("Session cut must fail");
        };
        let SubmitFailure::Recovering { runtime, .. } = failure else {
            panic!("must recover");
        };
        let Ok(runtime) = runtime.recover() else {
            panic!("same transaction redo must succeed");
        };
        assert_eq!(runtime.read_view().projects.len(), 1);
        assert_eq!(runtime.read_view().sessions.len(), 1);
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
    fn aggregate_transactions_without_committed_control_anchors_fail_startup() {
        let root = private_root();
        let request = combined_prepare("command-runtime");
        let runtime = ReadyDurableRuntime::open(root.path()).expect("open runtime");
        let (runtime, _) = submit_ok(runtime, request);
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
            Vec::new(),
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
        let request = combined_prepare("command-runtime");
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
}
