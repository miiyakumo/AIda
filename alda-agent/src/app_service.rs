#![allow(
    clippy::missing_errors_doc,
    reason = "Rustdoc 依照仓库语言规范使用中文“错误”标题"
)]

use std::collections::HashMap;
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicBool;
#[cfg(test)]
use std::sync::atomic::Ordering;

use sha2::Digest;
use sha2::Sha256;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::durable_runtime::DurableCursorError;
use crate::durable_runtime::FatalDurableRuntime;
use crate::durable_runtime::ReadyDurableRuntime;
use crate::durable_runtime::RecoveryFailure;
use crate::durable_runtime::SessionObjectRef;
use crate::durable_runtime::SubmitFailure;
use crate::protocol::ApprovalDecision;
use crate::protocol::ApprovalId;
use crate::protocol::ApprovalPayload;
use crate::protocol::ApprovalStatus;
use crate::protocol::ApprovalSubjectDigest;
use crate::protocol::ArtifactDurability;
use crate::protocol::ArtifactHash;
use crate::protocol::ArtifactKind;
use crate::protocol::ArtifactManifest;
use crate::protocol::ArtifactOccurrenceId;
use crate::protocol::ArtifactProducer;
use crate::protocol::BranchSummaryV1;
use crate::protocol::ChoiceId;
use crate::protocol::ClientCommand;
use crate::protocol::ClientCommandId;
use crate::protocol::ClientId;
use crate::protocol::CommandEnvelope;
use crate::protocol::CommandReply;
use crate::protocol::CommandResult;
use crate::protocol::DomainProjectSnapshotV1;
use crate::protocol::EffectClass;
use crate::protocol::PROTOCOL_VERSION;
use crate::protocol::PendingApproval;
use crate::protocol::PendingQuestion;
use crate::protocol::ProjectId;
use crate::protocol::ProjectSnapshot;
use crate::protocol::ProtocolErrorCode;
use crate::protocol::QuestionAnswer;
use crate::protocol::QuestionChoice;
use crate::protocol::QuestionId;
use crate::protocol::QuestionStatus;
use crate::protocol::RevisionDetailV1;
use crate::protocol::RevisionSummaryV1;
use crate::protocol::SESSION_STREAM_EPOCH;
use crate::protocol::ScoreRevisionId;
use crate::protocol::SessionEvent;
use crate::protocol::SessionEventKind;
use crate::protocol::SessionId;
use crate::protocol::SessionSnapshot;
use crate::protocol::StreamKind;
use crate::protocol::TakeSummaryV1;
use crate::protocol::TurnId;
use crate::protocol::TurnSnapshot;
use crate::protocol::TurnStatus;
use crate::protocol::{EVENT_PAGE_LIMIT, EventPage, ProtocolErrorDetails, RecoveryAction};
use crate::state_store::session::INTERNAL_CLIENT_PREFIX;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueueCapacity(NonZeroUsize);

impl QueueCapacity {
    /// 创建非零队列容量。
    ///
    /// # 错误
    ///
    /// `value` 为零时返回 [`InvalidQueueCapacity`]。
    pub fn new(value: usize) -> Result<Self, InvalidQueueCapacity> {
        NonZeroUsize::new(value)
            .map(Self)
            .ok_or(InvalidQueueCapacity)
    }

    #[must_use]
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("application service queue capacity must be greater than zero")]
pub struct InvalidQueueCapacity;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueryQueueCapacity(NonZeroUsize);

#[cfg(test)]
type QueryAttempt = (crate::protocol::StreamCursor, std::time::Instant);
#[cfg(test)]
type QueryAttemptProbe = Arc<std::sync::Mutex<Option<mpsc::UnboundedSender<QueryAttempt>>>>;

impl QueryQueueCapacity {
    /// 创建非零内部查询队列容量。
    ///
    /// # 错误
    ///
    /// `value` 为零时返回 [`InvalidQueueCapacity`]。
    pub fn new(value: usize) -> Result<Self, InvalidQueueCapacity> {
        NonZeroUsize::new(value)
            .map(Self)
            .ok_or(InvalidQueueCapacity)
    }

    #[must_use]
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

#[derive(Clone, Debug)]
pub struct AppService {
    command_tx: mpsc::Sender<QueuedCommand>,
    query_tx: mpsc::Sender<QueryMessage>,
    #[cfg(test)]
    query_paused: Arc<AtomicBool>,
    #[cfg(test)]
    query_attempt_probe: QueryAttemptProbe,
}

impl AppService {
    #[must_use]
    pub fn build(capacity: QueueCapacity) -> (Self, AppServiceRunner) {
        Self::build_with_capacities(
            capacity,
            QueryQueueCapacity(NonZeroUsize::new(32).unwrap_or(NonZeroUsize::MIN)),
        )
    }

    #[must_use]
    pub fn build_with_capacities(
        command_capacity: QueueCapacity,
        query_capacity: QueryQueueCapacity,
    ) -> (Self, AppServiceRunner) {
        let (command_tx, command_rx) = mpsc::channel(command_capacity.get());
        let (query_tx, query_rx) = mpsc::channel(query_capacity.get());
        #[cfg(test)]
        let query_paused = Arc::new(AtomicBool::new(false));
        (
            Self {
                command_tx,
                query_tx,
                #[cfg(test)]
                query_paused: Arc::clone(&query_paused),
                #[cfg(test)]
                query_attempt_probe: Arc::new(std::sync::Mutex::new(None)),
            },
            AppServiceRunner {
                command_rx,
                query_rx,
                state: ServiceState::default(),
                #[cfg(test)]
                query_paused,
            },
        )
    }

    #[must_use]
    pub fn spawn(capacity: QueueCapacity) -> Self {
        let (service, runner) = Self::build(capacity);
        tokio::spawn(runner.run());
        service
    }

    /// 命令入队后立即返回，不等待 actor 处理。
    ///
    /// # 错误
    ///
    /// 有界队列已满时返回 [`SubmitError::Overloaded`]；service runner 停止后返回
    /// [`SubmitError::Closed`]。
    pub fn enqueue(&self, envelope: CommandEnvelope) -> Result<PendingReply, SubmitError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .try_send(QueuedCommand { envelope, reply_tx })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => SubmitError::Overloaded,
                mpsc::error::TrySendError::Closed(_) => SubmitError::Closed,
            })?;
        Ok(PendingReply { reply_rx })
    }

    /// 将命令入队并等待协议回复。
    ///
    /// # 错误
    ///
    /// 命令无法入队，或 runner 在回复前停止时返回 [`SubmitError`]。
    pub async fn execute(&self, envelope: CommandEnvelope) -> Result<CommandReply, SubmitError> {
        self.enqueue(envelope)?.wait().await
    }

    /// 通过持有 Artifact 状态的同一个有界 actor 解析已认证 HTTP 下载。
    ///
    /// # 错误
    ///
    /// actor 队列不可用时返回 [`SubmitError`]。
    pub async fn resolve_artifact_download(
        &self,
        project_id: ProjectId,
        hash: ArtifactHash,
        if_none_match: Option<String>,
    ) -> Result<DownloadResolution, SubmitError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.query_tx
            .try_send(QueryMessage::ResolveArtifactDownload {
                project_id,
                hash,
                if_none_match,
                reply_tx,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => SubmitError::Overloaded,
                mpsc::error::TrySendError::Closed(_) => SubmitError::Closed,
            })?;
        reply_rx.await.map_err(|_| SubmitError::ReplyDropped)
    }

    /// 通过有界、低优先级查询通道解析 Session 事件。
    ///
    /// # 错误
    ///
    /// 查询无法入队或无法回复时返回 [`SubmitError`]。
    pub async fn resolve_session_events(
        &self,
        cursor: crate::protocol::StreamCursor,
    ) -> Result<CommandReply, SubmitError> {
        #[cfg(test)]
        if let Some(probe) = self
            .query_attempt_probe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            let _ignored = probe.send((cursor.clone(), std::time::Instant::now()));
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        self.query_tx
            .try_send(QueryMessage::ResolveSessionEvents { cursor, reply_tx })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => SubmitError::Overloaded,
                mpsc::error::TrySendError::Closed(_) => SubmitError::Closed,
            })?;
        reply_rx.await.map_err(|_| SubmitError::ReplyDropped)
    }

    #[cfg(test)]
    pub(crate) fn pause_queries_for_test(&self, paused: bool) {
        self.query_paused.store(paused, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn install_query_attempt_probe_for_test(
        &self,
    ) -> mpsc::UnboundedReceiver<QueryAttempt> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self
            .query_attempt_probe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
        rx
    }

    #[cfg(test)]
    pub(crate) fn fill_query_queue_with_cursor_for_test(
        &self,
        cursor: crate::protocol::StreamCursor,
    ) {
        let (reply_tx, _reply_rx) = oneshot::channel();
        self.query_tx
            .try_send(QueryMessage::ResolveSessionEvents { cursor, reply_tx })
            .expect("test query queue has capacity");
    }

    #[cfg(test)]
    async fn artifact_stats_for_test(&self) -> ArtifactStats {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.query_tx
            .send(QueryMessage::ArtifactStats { reply_tx })
            .await
            .expect("test service is running");
        reply_rx.await.expect("test stats reply")
    }

    #[cfg(test)]
    async fn set_fixture_fault_for_test(&self, fault: FixtureFault) {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.query_tx
            .send(QueryMessage::SetFixtureFault { fault, reply_tx })
            .await
            .expect("test service is running");
        reply_rx.await.expect("fixture fault acknowledgement");
    }

    #[cfg(test)]
    pub(crate) fn spawn_with_corrupt_download_fixture_for_test(
        capacity: QueueCapacity,
    ) -> (Self, ProjectId, ArtifactHash) {
        let (command_tx, command_rx) = mpsc::channel(capacity.get());
        let (query_tx, query_rx) = mpsc::channel(8);
        let project_id = ProjectId("project-corrupt".to_owned());
        let hash = ArtifactHash::parse(FIXTURE_HASH).expect("fixed fixture hash");
        let mut state = ServiceState::default();
        state
            .reachability
            .insert((project_id.clone(), hash.clone()));
        state.blobs.insert(
            hash.clone(),
            BlobRecord {
                bytes: Arc::from(b"corrupt".as_slice()),
                size_bytes: FIXTURE_SIZE,
                mime_type: FIXTURE_MIME.to_owned(),
            },
        );
        tokio::spawn(
            AppServiceRunner {
                command_rx,
                query_rx,
                state,
                query_paused: Arc::new(AtomicBool::new(false)),
            }
            .run(),
        );
        (
            Self {
                command_tx,
                query_tx,
                query_paused: Arc::new(AtomicBool::new(false)),
                query_attempt_probe: Arc::new(std::sync::Mutex::new(None)),
            },
            project_id,
            hash,
        )
    }

    #[cfg(test)]
    pub(crate) fn fill_query_queue_for_test(&self) {
        let (reply_tx, _reply_rx) = oneshot::channel();
        self.query_tx
            .try_send(QueryMessage::ResolveSessionEvents {
                cursor: crate::protocol::StreamCursor {
                    stream_kind: StreamKind::SessionRollout,
                    stream_id: "session-test".to_owned(),
                    epoch: 1,
                    after_sequence: 0,
                },
                reply_tx,
            })
            .expect("test query queue has room");
    }
}

pub struct PendingReply {
    reply_rx: oneshot::Receiver<CommandReply>,
}

impl PendingReply {
    /// 等待 application service 处理已入队命令。
    ///
    /// # 错误
    ///
    /// runner 未产生回复便停止时返回 [`SubmitError::ReplyDropped`]。
    pub async fn wait(self) -> Result<CommandReply, SubmitError> {
        self.reply_rx.await.map_err(|_| SubmitError::ReplyDropped)
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SubmitError {
    #[error("application service is overloaded; retry later")]
    Overloaded,
    #[error("application service is not accepting commands")]
    Closed,
    #[error("application service dropped the command reply")]
    ReplyDropped,
}

pub struct AppServiceRunner {
    command_rx: mpsc::Receiver<QueuedCommand>,
    query_rx: mpsc::Receiver<QueryMessage>,
    state: ServiceState,
    #[cfg(test)]
    query_paused: Arc<AtomicBool>,
}

impl AppServiceRunner {
    pub async fn run(mut self) {
        loop {
            let mut progressed = false;
            for _ in 0..8 {
                let Ok(command) = self.command_rx.try_recv() else {
                    break;
                };
                self.handle_command(command);
                progressed = true;
            }
            if !self.queries_paused()
                && let Ok(query) = self.query_rx.try_recv()
            {
                self.handle_query(query);
                progressed = true;
            }
            if progressed {
                continue;
            }
            tokio::select! {
                command = self.command_rx.recv() => {
                    if let Some(command) = command {
                        self.handle_command(command);
                    } else if self.query_rx.is_closed() {
                        break;
                    }
                }
                query = self.query_rx.recv(), if !self.queries_paused() => {
                    if let Some(query) = query {
                        self.handle_query(query);
                    } else if self.command_rx.is_closed() {
                        break;
                    }
                }
            }
        }
    }

    #[cfg_attr(
        not(test),
        allow(clippy::unused_self, reason = "test builds read the pause field")
    )]
    fn queries_paused(&self) -> bool {
        #[cfg(test)]
        {
            self.query_paused.load(Ordering::SeqCst)
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    fn handle_command(&mut self, queued: QueuedCommand) {
        let reply = self.state.handle(queued.envelope);
        let _reply_was_dropped = queued.reply_tx.send(reply);
    }

    fn handle_query(&mut self, query: QueryMessage) {
        match query {
            QueryMessage::ResolveArtifactDownload {
                project_id,
                hash,
                if_none_match,
                reply_tx,
            } => {
                let reply =
                    self.state
                        .resolve_download(&project_id, &hash, if_none_match.as_deref());
                let _reply_was_dropped = reply_tx.send(reply);
            }
            QueryMessage::ResolveSessionEvents { cursor, reply_tx } => {
                let reply = self
                    .state
                    .resolve_events(&cursor, ClientCommandId("internal-query".to_owned()));
                let _reply_was_dropped = reply_tx.send(reply);
            }
            #[cfg(test)]
            QueryMessage::ArtifactStats { reply_tx } => {
                let _reply_was_dropped = reply_tx.send(ArtifactStats {
                    blobs: self.state.blobs.len(),
                    occurrences: self.state.occurrences.len(),
                    reachability: self.state.reachability.len(),
                });
            }
            #[cfg(test)]
            QueryMessage::SetFixtureFault { fault, reply_tx } => {
                self.state.fixture_fault = fault;
                let _reply_was_dropped = reply_tx.send(());
            }
        }
    }
}

enum QueryMessage {
    ResolveArtifactDownload {
        project_id: ProjectId,
        hash: ArtifactHash,
        if_none_match: Option<String>,
        reply_tx: oneshot::Sender<DownloadResolution>,
    },
    ResolveSessionEvents {
        cursor: crate::protocol::StreamCursor,
        reply_tx: oneshot::Sender<CommandReply>,
    },
    #[cfg(test)]
    ArtifactStats {
        reply_tx: oneshot::Sender<ArtifactStats>,
    },
    #[cfg(test)]
    SetFixtureFault {
        fault: FixtureFault,
        reply_tx: oneshot::Sender<()>,
    },
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ArtifactStats {
    blobs: usize,
    occurrences: usize,
    reachability: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedDownload {
    pub artifact_hash: ArtifactHash,
    pub mime_type: String,
    pub size_bytes: u64,
    pub bytes: Arc<[u8]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DownloadResolution {
    Verified(VerifiedDownload),
    NotModified(ArtifactHash),
    NotFound,
    Corrupt,
}

struct QueuedCommand {
    envelope: CommandEnvelope,
    reply_tx: oneshot::Sender<CommandReply>,
}

#[derive(Default)]
struct ServiceState {
    next_project_number: u64,
    next_session_number: u64,
    next_turn_number: u64,
    next_question_number: u64,
    next_approval_number: u64,
    next_artifact_occurrence_number: u64,
    projects: HashMap<ProjectId, ProjectSnapshot>,
    domain_projects: HashMap<ProjectId, crate::state::ProjectCoordinator>,
    sessions: HashMap<SessionId, SessionState>,
    turn_owners: HashMap<TurnId, SessionId>,
    question_owners: HashMap<QuestionId, SessionId>,
    approval_owners: HashMap<ApprovalId, SessionId>,
    turn_prompts: HashMap<TurnId, String>,
    blobs: HashMap<ArtifactHash, BlobRecord>,
    occurrences: HashMap<ArtifactOccurrenceId, ArtifactManifest>,
    reachability: HashSet<(ProjectId, ArtifactHash)>,
    fixture_fault: FixtureFault,
    idempotency: HashMap<(ClientId, ClientCommandId), StoredReply>,
}

struct BlobRecord {
    bytes: Arc<[u8]>,
    size_bytes: u64,
    mime_type: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum FixtureFault {
    #[default]
    None,
    HashMismatch,
    SizeMismatch,
}

struct SessionState {
    project_id: ProjectId,
    events: Vec<SessionEvent>,
    projection: SessionProjection,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct SessionProjection {
    turns: Vec<TurnSnapshot>,
    questions: Vec<PendingQuestion>,
    approvals: Vec<PendingApproval>,
}

#[derive(Clone)]
struct StoredReply {
    request_fingerprint: String,
    reply: CommandReply,
}

/// 只持有已完成启动收敛的 durable 窄能力，不包含内存 reducer 后备状态。
#[allow(
    dead_code,
    reason = "C2 首个叶子先冻结 backend 边界，后续叶子逐项接入 production mutation"
)]
pub(crate) struct DurableServiceState {
    runtime: DurableServiceRuntime,
}

enum DurableServiceRuntime {
    Ready(Box<ReadyDurableRuntime>),
    Fatal(Box<FatalDurableRuntime>),
    Transitioning,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
enum DurableServiceError {
    #[error("durable service is unavailable")]
    RuntimeUnavailable,
}

#[allow(
    dead_code,
    reason = "C2 首个叶子先冻结 backend 边界，后续叶子逐项接入 production mutation"
)]
impl DurableServiceState {
    pub(crate) fn new(runtime: ReadyDurableRuntime) -> Self {
        Self {
            runtime: DurableServiceRuntime::Ready(Box::new(runtime)),
        }
    }

    pub(crate) fn fatal_error(&self) -> Option<&crate::durable_runtime::DurableRuntimeError> {
        match &self.runtime {
            DurableServiceRuntime::Fatal(runtime) => Some(runtime.error()),
            DurableServiceRuntime::Ready(_) | DurableServiceRuntime::Transitioning => None,
        }
    }

    fn ready_runtime(&self) -> Option<&ReadyDurableRuntime> {
        match &self.runtime {
            DurableServiceRuntime::Ready(runtime) => Some(runtime),
            DurableServiceRuntime::Fatal(runtime) => {
                let _error = runtime.error();
                None
            }
            DurableServiceRuntime::Transitioning => None,
        }
    }

    fn take_ready_runtime(&mut self) -> Option<ReadyDurableRuntime> {
        let previous = std::mem::replace(&mut self.runtime, DurableServiceRuntime::Transitioning);
        match previous {
            DurableServiceRuntime::Ready(runtime) => Some(*runtime),
            unavailable => {
                self.runtime = unavailable;
                None
            }
        }
    }

    fn restore_ready_runtime(&mut self, runtime: ReadyDurableRuntime) {
        self.runtime = DurableServiceRuntime::Ready(Box::new(runtime));
    }

    #[cfg(test)]
    fn set_runtime_failpoint_for_test(
        &mut self,
        failpoint: crate::durable_runtime::RuntimeFailpoint,
    ) {
        let DurableServiceRuntime::Ready(runtime) = &mut self.runtime else {
            panic!("测试 failpoint 只能安装到 Ready durable backend");
        };
        runtime.set_failpoint(failpoint);
    }

    fn handle_project_create(&mut self, envelope: &CommandEnvelope, name: &str) -> CommandReply {
        if envelope.client_id.0.starts_with(INTERNAL_CLIENT_PREFIX) {
            return CommandReply::error(
                envelope.client_command_id.clone(),
                ProtocolErrorCode::InvalidRequest,
                "reserved internal client namespace is not available to external commands",
            );
        }
        let Ok(payload_digest) = crate::protocol::external_command_payload_digest(
            envelope.protocol_version,
            &envelope.command,
        ) else {
            return Self::runtime_unavailable_reply(envelope);
        };
        let lookup = match self.ready_runtime() {
            Some(runtime) => runtime.lookup_command(
                &envelope.client_id,
                &envelope.client_command_id,
                &payload_digest,
            ),
            None => return Self::runtime_unavailable_reply(envelope),
        };
        match lookup {
            Ok(crate::durable_runtime::CommandLookup::ExactReply(raw_reply)) => {
                return decode_durable_reply(envelope, &raw_reply);
            }
            Err(crate::durable_runtime::CommandLookupError::IdempotencyConflict) => {
                return CommandReply::error(
                    envelope.client_command_id.clone(),
                    ProtocolErrorCode::IdempotencyConflict,
                    "the client command ID was already used with a different payload",
                );
            }
            Err(crate::durable_runtime::CommandLookupError::CorruptCommittedIndex(_)) => {
                return Self::runtime_unavailable_reply(envelope);
            }
            Ok(crate::durable_runtime::CommandLookup::Unseen) => {}
        }

        let Some(name) = validated_project_name(name) else {
            return invalid_project_name_reply(envelope);
        };
        let Some(request) = self.ready_runtime().and_then(|runtime| {
            runtime
                .plan_project_create(
                    &envelope.client_id,
                    &envelope.client_command_id,
                    &payload_digest,
                    name,
                )
                .ok()
        }) else {
            return Self::runtime_unavailable_reply(envelope);
        };
        self.submit_durable_mutation(envelope, &payload_digest, request)
    }

    fn handle_session_start(
        &mut self,
        envelope: &CommandEnvelope,
        project_id: &ProjectId,
    ) -> CommandReply {
        if envelope.client_id.0.starts_with(INTERNAL_CLIENT_PREFIX) {
            return CommandReply::error(
                envelope.client_command_id.clone(),
                ProtocolErrorCode::InvalidRequest,
                "reserved internal client namespace is not available to external commands",
            );
        }
        let Ok(payload_digest) = crate::protocol::external_command_payload_digest(
            envelope.protocol_version,
            &envelope.command,
        ) else {
            return Self::runtime_unavailable_reply(envelope);
        };
        let lookup = match self.ready_runtime() {
            Some(runtime) => runtime.lookup_command(
                &envelope.client_id,
                &envelope.client_command_id,
                &payload_digest,
            ),
            None => return Self::runtime_unavailable_reply(envelope),
        };
        match lookup {
            Ok(crate::durable_runtime::CommandLookup::ExactReply(raw_reply)) => {
                return decode_durable_reply(envelope, &raw_reply);
            }
            Err(crate::durable_runtime::CommandLookupError::IdempotencyConflict) => {
                return CommandReply::error(
                    envelope.client_command_id.clone(),
                    ProtocolErrorCode::IdempotencyConflict,
                    "the client command ID was already used with a different payload",
                );
            }
            Err(crate::durable_runtime::CommandLookupError::CorruptCommittedIndex(_)) => {
                return Self::runtime_unavailable_reply(envelope);
            }
            Ok(crate::durable_runtime::CommandLookup::Unseen) => {}
        }

        let project_exists = self
            .ready_runtime()
            .is_some_and(|runtime| runtime.project_metadata(project_id).is_some());
        if !project_exists {
            return project_not_found(envelope, project_id);
        }
        let Some(request) = self.ready_runtime().and_then(|runtime| {
            runtime
                .plan_session_start(
                    &envelope.client_id,
                    &envelope.client_command_id,
                    &payload_digest,
                    project_id,
                )
                .ok()
        }) else {
            return Self::runtime_unavailable_reply(envelope);
        };
        self.submit_durable_mutation(envelope, &payload_digest, request)
    }

    fn handle_turn_start(
        &mut self,
        envelope: &CommandEnvelope,
        session_id: &SessionId,
        prompt: &str,
    ) -> CommandReply {
        if envelope.client_id.0.starts_with(INTERNAL_CLIENT_PREFIX) {
            return reserved_internal_namespace_reply(envelope);
        }
        let Ok(payload_digest) = crate::protocol::external_command_payload_digest(
            envelope.protocol_version,
            &envelope.command,
        ) else {
            return Self::runtime_unavailable_reply(envelope);
        };
        let lookup = match self.ready_runtime() {
            Some(runtime) => runtime.lookup_command(
                &envelope.client_id,
                &envelope.client_command_id,
                &payload_digest,
            ),
            None => return Self::runtime_unavailable_reply(envelope),
        };
        match lookup {
            Ok(crate::durable_runtime::CommandLookup::ExactReply(raw_reply)) => {
                return decode_durable_reply(envelope, &raw_reply);
            }
            Err(crate::durable_runtime::CommandLookupError::IdempotencyConflict) => {
                return idempotency_conflict_reply(envelope);
            }
            Err(crate::durable_runtime::CommandLookupError::CorruptCommittedIndex(_)) => {
                return Self::runtime_unavailable_reply(envelope);
            }
            Ok(crate::durable_runtime::CommandLookup::Unseen) => {}
        }

        let Some(prompt) = validated_turn_prompt(prompt) else {
            return invalid_turn_prompt_reply(envelope);
        };
        if self
            .ready_runtime()
            .is_none_or(|runtime| runtime.session_snapshot(session_id).is_none())
        {
            return session_not_found(envelope, session_id);
        }
        let Some(request) = self.ready_runtime().and_then(|runtime| {
            runtime
                .plan_turn_start(
                    &envelope.client_id,
                    &envelope.client_command_id,
                    &payload_digest,
                    session_id,
                    prompt,
                )
                .ok()
        }) else {
            return Self::runtime_unavailable_reply(envelope);
        };
        self.submit_durable_mutation(envelope, &payload_digest, request)
    }

    fn handle_turn_cancel(
        &mut self,
        envelope: &CommandEnvelope,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> CommandReply {
        if envelope.client_id.0.starts_with(INTERNAL_CLIENT_PREFIX) {
            return reserved_internal_namespace_reply(envelope);
        }
        let Ok(payload_digest) = crate::protocol::external_command_payload_digest(
            envelope.protocol_version,
            &envelope.command,
        ) else {
            return Self::runtime_unavailable_reply(envelope);
        };
        let lookup = match self.ready_runtime() {
            Some(runtime) => runtime.lookup_command(
                &envelope.client_id,
                &envelope.client_command_id,
                &payload_digest,
            ),
            None => return Self::runtime_unavailable_reply(envelope),
        };
        match lookup {
            Ok(crate::durable_runtime::CommandLookup::ExactReply(raw_reply)) => {
                return decode_durable_reply(envelope, &raw_reply);
            }
            Err(crate::durable_runtime::CommandLookupError::IdempotencyConflict) => {
                return idempotency_conflict_reply(envelope);
            }
            Err(crate::durable_runtime::CommandLookupError::CorruptCommittedIndex(_)) => {
                return Self::runtime_unavailable_reply(envelope);
            }
            Ok(crate::durable_runtime::CommandLookup::Unseen) => {}
        }

        let Some(runtime) = self.ready_runtime() else {
            return Self::runtime_unavailable_reply(envelope);
        };
        if runtime.session_snapshot(session_id).is_none() {
            return session_not_found(envelope, session_id);
        }
        if runtime.owner_of(SessionObjectRef::Turn(turn_id)).is_none() {
            return turn_not_found(envelope, turn_id);
        }
        let Some(request) = self.ready_runtime().and_then(|runtime| {
            runtime
                .plan_turn_cancel(
                    &envelope.client_id,
                    &envelope.client_command_id,
                    &payload_digest,
                    session_id,
                    turn_id,
                )
                .ok()
        }) else {
            return Self::runtime_unavailable_reply(envelope);
        };
        self.submit_durable_mutation(envelope, &payload_digest, request)
    }

    fn handle_question_respond(
        &mut self,
        envelope: &CommandEnvelope,
        session_id: &SessionId,
        question_id: &QuestionId,
        choice_id: &ChoiceId,
    ) -> CommandReply {
        if envelope.client_id.0.starts_with(INTERNAL_CLIENT_PREFIX) {
            return reserved_internal_namespace_reply(envelope);
        }
        let Ok(payload_digest) = crate::protocol::external_command_payload_digest(
            envelope.protocol_version,
            &envelope.command,
        ) else {
            return Self::runtime_unavailable_reply(envelope);
        };
        let lookup = match self.ready_runtime() {
            Some(runtime) => runtime.lookup_command(
                &envelope.client_id,
                &envelope.client_command_id,
                &payload_digest,
            ),
            None => return Self::runtime_unavailable_reply(envelope),
        };
        match lookup {
            Ok(crate::durable_runtime::CommandLookup::ExactReply(raw_reply)) => {
                return decode_durable_reply(envelope, &raw_reply);
            }
            Err(crate::durable_runtime::CommandLookupError::IdempotencyConflict) => {
                return idempotency_conflict_reply(envelope);
            }
            Err(crate::durable_runtime::CommandLookupError::CorruptCommittedIndex(_)) => {
                return Self::runtime_unavailable_reply(envelope);
            }
            Ok(crate::durable_runtime::CommandLookup::Unseen) => {}
        }

        let Some(runtime) = self.ready_runtime() else {
            return Self::runtime_unavailable_reply(envelope);
        };
        if runtime.session_snapshot(session_id).is_none() {
            return session_not_found(envelope, session_id);
        }
        let Some(owner_session_id) = runtime
            .owner_of(SessionObjectRef::Question(question_id))
            .cloned()
        else {
            return question_not_found(envelope, question_id);
        };
        let Some(owner_snapshot) = runtime.session_snapshot(&owner_session_id) else {
            return Self::runtime_unavailable_reply(envelope);
        };
        let Some(question) = owner_snapshot
            .questions
            .iter()
            .find(|question| question.question_id == *question_id)
        else {
            return Self::runtime_unavailable_reply(envelope);
        };
        if question.status == QuestionStatus::OwnerTurnAborted {
            return CommandReply::error(
                envelope.client_command_id.clone(),
                ProtocolErrorCode::RequestOwnerTurnAborted,
                "the question owner Turn is terminal",
            );
        }
        if choice_id.0.is_empty() || choice_id.0.chars().count() > 120 {
            return CommandReply::error(
                envelope.client_command_id.clone(),
                ProtocolErrorCode::InvalidRequest,
                "choice ID must contain 1 to 120 characters",
            );
        }
        if !question
            .choices
            .iter()
            .any(|choice| choice.choice_id == *choice_id)
        {
            return CommandReply::error(
                envelope.client_command_id.clone(),
                ProtocolErrorCode::InvalidQuestionChoice,
                "choice ID must identify one of the question choices",
            );
        }
        let Some(request) = self.ready_runtime().and_then(|runtime| {
            runtime
                .plan_question_respond(
                    &envelope.client_id,
                    &envelope.client_command_id,
                    &payload_digest,
                    session_id,
                    question_id,
                    choice_id,
                )
                .ok()
        }) else {
            return Self::runtime_unavailable_reply(envelope);
        };
        self.submit_durable_mutation(envelope, &payload_digest, request)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Approval 分派需连续保持 lookup、权威校验、approve capability 与 deny 边界"
    )]
    fn handle_approval_respond(
        &mut self,
        envelope: &CommandEnvelope,
        session_id: &SessionId,
        approval_id: &ApprovalId,
        approval_subject_digest: &ApprovalSubjectDigest,
        decision: ApprovalDecision,
    ) -> CommandReply {
        if envelope.client_id.0.starts_with(INTERNAL_CLIENT_PREFIX) {
            return reserved_internal_namespace_reply(envelope);
        }
        let Ok(payload_digest) = crate::protocol::external_command_payload_digest(
            envelope.protocol_version,
            &envelope.command,
        ) else {
            return Self::runtime_unavailable_reply(envelope);
        };
        let lookup = match self.ready_runtime() {
            Some(runtime) => runtime.lookup_command(
                &envelope.client_id,
                &envelope.client_command_id,
                &payload_digest,
            ),
            None => return Self::runtime_unavailable_reply(envelope),
        };
        match lookup {
            Ok(crate::durable_runtime::CommandLookup::ExactReply(raw_reply)) => {
                return decode_durable_reply(envelope, &raw_reply);
            }
            Err(crate::durable_runtime::CommandLookupError::IdempotencyConflict) => {
                return idempotency_conflict_reply(envelope);
            }
            Err(crate::durable_runtime::CommandLookupError::CorruptCommittedIndex(_)) => {
                return Self::runtime_unavailable_reply(envelope);
            }
            Ok(crate::durable_runtime::CommandLookup::Unseen) => {}
        }
        let Some(runtime) = self.ready_runtime() else {
            return Self::runtime_unavailable_reply(envelope);
        };
        if runtime.session_snapshot(session_id).is_none() {
            return session_not_found(envelope, session_id);
        }
        let Some(owner_session_id) = runtime
            .owner_of(SessionObjectRef::Approval(approval_id))
            .cloned()
        else {
            return approval_not_found(envelope, approval_id);
        };
        let Some(owner_snapshot) = runtime.session_snapshot(&owner_session_id) else {
            return Self::runtime_unavailable_reply(envelope);
        };
        let Some(approval) = owner_snapshot
            .approvals
            .iter()
            .find(|approval| approval.approval_id == *approval_id)
        else {
            return Self::runtime_unavailable_reply(envelope);
        };
        if approval.status == ApprovalStatus::OwnerTurnAborted {
            return CommandReply::error(
                envelope.client_command_id.clone(),
                ProtocolErrorCode::RequestOwnerTurnAborted,
                "the approval owner Turn is terminal",
            );
        }
        if approval.approval_subject_digest != *approval_subject_digest {
            return CommandReply::error(
                envelope.client_command_id.clone(),
                ProtocolErrorCode::ApprovalSubjectMismatch,
                "approval subject digest does not match the requested action",
            );
        }
        match decision {
            ApprovalDecision::Approve => {
                let Some(plan) = self.ready_runtime().and_then(|runtime| {
                    runtime
                        .plan_approval_approve(
                            &envelope.client_id,
                            &envelope.client_command_id,
                            &payload_digest,
                            session_id,
                            approval_id,
                            approval_subject_digest,
                        )
                        .ok()
                }) else {
                    return Self::runtime_unavailable_reply(envelope);
                };
                let (request, pending_reference) = plan.into_parts();
                self.submit_durable_mutation_with_artifact(
                    envelope,
                    &payload_digest,
                    request,
                    pending_reference,
                )
            }
            ApprovalDecision::Deny => {
                let Some(request) = self.ready_runtime().and_then(|runtime| {
                    runtime
                        .plan_approval_deny(
                            &envelope.client_id,
                            &envelope.client_command_id,
                            &payload_digest,
                            session_id,
                            approval_id,
                            approval_subject_digest,
                        )
                        .ok()
                }) else {
                    return Self::runtime_unavailable_reply(envelope);
                };
                self.submit_durable_mutation(envelope, &payload_digest, request)
            }
        }
    }

    fn submit_durable_mutation(
        &mut self,
        envelope: &CommandEnvelope,
        payload_digest: &str,
        request: crate::control_store::PrepareControlRequest,
    ) -> CommandReply {
        self.submit_durable_mutation_with_artifact(envelope, payload_digest, request, None)
    }

    fn submit_durable_mutation_with_artifact(
        &mut self,
        envelope: &CommandEnvelope,
        payload_digest: &str,
        request: crate::control_store::PrepareControlRequest,
        pending_reference: Option<crate::durable_runtime::PendingArtifactReference>,
    ) -> CommandReply {
        let Some(runtime) = self.take_ready_runtime() else {
            return Self::runtime_unavailable_reply(envelope);
        };
        match runtime.submit(request) {
            Ok((runtime, raw_reply)) => {
                self.restore_ready_runtime(runtime);
                decode_durable_reply(envelope, &raw_reply)
            }
            Err(SubmitFailure::Rejected { runtime, error: _ }) => {
                let disposition = pending_reference.and_then(|pending_reference| {
                    runtime.classify_pending_artifact_reference_after_prepared_rejection(
                        pending_reference,
                    )
                });
                self.restore_ready_runtime(*runtime);
                match disposition {
                    Some(
                        crate::durable_runtime::ArtifactReferenceDisposition::AlreadyReachable,
                    ) => CommandReply::error(
                        envelope.client_command_id.clone(),
                        ProtocolErrorCode::ServiceUnavailable,
                        "durable service is unavailable; prepared Artifact was already reachable",
                    ),
                    Some(
                        crate::durable_runtime::ArtifactReferenceDisposition::OrphanCandidate(
                            orphan,
                        ),
                    ) => {
                        let _hash = orphan.hash();
                        CommandReply::error(
                            envelope.client_command_id.clone(),
                            ProtocolErrorCode::ServiceUnavailable,
                            "durable service is unavailable; prepared Artifact remains an orphan candidate",
                        )
                    }
                    None => Self::runtime_unavailable_reply(envelope),
                }
            }
            Err(SubmitFailure::Recovering { runtime, error: _ }) => match runtime.recover() {
                Ok(runtime) => {
                    let reply = runtime.lookup_command(
                        &envelope.client_id,
                        &envelope.client_command_id,
                        payload_digest,
                    );
                    self.restore_ready_runtime(runtime);
                    match reply {
                        Ok(crate::durable_runtime::CommandLookup::ExactReply(raw_reply)) => {
                            decode_durable_reply(envelope, &raw_reply)
                        }
                        Ok(crate::durable_runtime::CommandLookup::Unseen)
                        | Err(
                            crate::durable_runtime::CommandLookupError::IdempotencyConflict
                            | crate::durable_runtime::CommandLookupError::CorruptCommittedIndex(_),
                        ) => Self::runtime_unavailable_reply(envelope),
                    }
                }
                Err(RecoveryFailure::Fatal(runtime)) => {
                    self.runtime = DurableServiceRuntime::Fatal(runtime);
                    Self::runtime_unavailable_reply(envelope)
                }
            },
            Err(SubmitFailure::Fatal(runtime)) => {
                self.runtime = DurableServiceRuntime::Fatal(runtime);
                Self::runtime_unavailable_reply(envelope)
            }
        }
    }

    fn runtime_unavailable_reply(envelope: &CommandEnvelope) -> CommandReply {
        DurableServiceError::RuntimeUnavailable.into_reply(envelope.client_command_id.clone())
    }

    pub(crate) fn resolve_artifact_download(
        &self,
        project_id: &ProjectId,
        hash: &ArtifactHash,
        if_none_match: Option<&str>,
    ) -> DownloadResolution {
        let Some(runtime) = self.ready_runtime() else {
            return DownloadResolution::Corrupt;
        };
        let mut opened = match runtime.read_project_artifact(project_id, hash) {
            Ok(Some(opened)) => opened,
            Ok(None) => return DownloadResolution::NotFound,
            Err(_) => return DownloadResolution::Corrupt,
        };
        if opened.verified().hash.as_str() != hash.as_str()
            || opened.verified().size > 64 * 1024 * 1024
        {
            return DownloadResolution::Corrupt;
        }
        let mut bytes = Vec::new();
        if std::io::Read::read_to_end(&mut opened, &mut bytes).is_err()
            || u64::try_from(bytes.len()).ok() != Some(opened.verified().size)
        {
            return DownloadResolution::Corrupt;
        }
        let etag = format!("\"{}\"", hash.as_str());
        if if_none_match == Some(etag.as_str()) {
            return DownloadResolution::NotModified(hash.clone());
        }
        DownloadResolution::Verified(VerifiedDownload {
            artifact_hash: hash.clone(),
            mime_type: FIXTURE_MIME.to_owned(),
            size_bytes: opened.verified().size,
            bytes: Arc::from(bytes),
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "只读分派保持穷尽可审计，mutation authority 仍与共享 helper 隔离"
    )]
    pub(crate) fn handle(&mut self, envelope: &CommandEnvelope) -> CommandReply {
        if let Some(reply) = validate_protocol_version(envelope) {
            return reply;
        }
        if self.ready_runtime().is_none() {
            return Self::runtime_unavailable_reply(envelope);
        }

        match &envelope.command {
            ClientCommand::Initialize => initialized_reply(envelope.client_command_id.clone()),
            ClientCommand::ProjectSnapshot { project_id } => {
                let Some(runtime) = self.ready_runtime() else {
                    return Self::runtime_unavailable_reply(envelope);
                };
                runtime.project_metadata(project_id).map_or_else(
                    || project_not_found(envelope, project_id),
                    |snapshot| {
                        CommandReply::success(
                            envelope.client_command_id.clone(),
                            CommandResult::ProjectSnapshot(snapshot.clone()),
                        )
                    },
                )
            }
            ClientCommand::ProjectDomainSnapshot { project_id } => {
                let Some(runtime) = self.ready_runtime() else {
                    return Self::runtime_unavailable_reply(envelope);
                };
                let Some(project) = runtime.project_projection(project_id) else {
                    return project_not_found(envelope, project_id);
                };
                match map_domain_snapshot(project_id, project) {
                    Ok(snapshot) => CommandReply::success(
                        envelope.client_command_id.clone(),
                        CommandResult::ProjectDomainSnapshot(snapshot),
                    ),
                    Err(error) => CommandReply::error(
                        envelope.client_command_id.clone(),
                        ProtocolErrorCode::ServiceUnavailable,
                        format!("failed to map the durable project domain: {error}"),
                    ),
                }
            }
            ClientCommand::RevisionList { project_id } => {
                let Some(runtime) = self.ready_runtime() else {
                    return Self::runtime_unavailable_reply(envelope);
                };
                let Some(project) = runtime.project_projection(project_id) else {
                    return project_not_found(envelope, project_id);
                };
                let revisions = project
                    .revisions
                    .values()
                    .map(|revision| map_revision_summary(project, revision))
                    .collect();
                CommandReply::success(
                    envelope.client_command_id.clone(),
                    CommandResult::RevisionList(revisions),
                )
            }
            ClientCommand::RevisionRead {
                project_id,
                revision_id,
            } => {
                let Some(runtime) = self.ready_runtime() else {
                    return Self::runtime_unavailable_reply(envelope);
                };
                let Some(project) = runtime.project_projection(project_id) else {
                    return project_not_found(envelope, project_id);
                };
                let Ok(domain_revision_id) =
                    crate::domain::RevisionId::parse(revision_id.0.clone())
                else {
                    return CommandReply::error(
                        envelope.client_command_id.clone(),
                        ProtocolErrorCode::InvalidRequest,
                        "revision ID is invalid",
                    );
                };
                let Some(revision) = project.revisions.get(&domain_revision_id) else {
                    return CommandReply::error(
                        envelope.client_command_id.clone(),
                        ProtocolErrorCode::RevisionNotFound,
                        format!("revision `{}` was not found", revision_id.0),
                    );
                };
                CommandReply::success(
                    envelope.client_command_id.clone(),
                    CommandResult::RevisionRead(map_revision_detail(project_id, project, revision)),
                )
            }
            ClientCommand::SessionSnapshot { session_id } => {
                let Some(runtime) = self.ready_runtime() else {
                    return Self::runtime_unavailable_reply(envelope);
                };
                runtime.session_snapshot(session_id).map_or_else(
                    || session_not_found(envelope, session_id),
                    |snapshot| {
                        CommandReply::success(
                            envelope.client_command_id.clone(),
                            CommandResult::SessionSnapshot(snapshot),
                        )
                    },
                )
            }
            ClientCommand::EventResume { cursor } => {
                let Some(runtime) = self.ready_runtime() else {
                    return Self::runtime_unavailable_reply(envelope);
                };
                match runtime.resume_events(cursor) {
                    Ok(page) => CommandReply::success(
                        envelope.client_command_id.clone(),
                        CommandResult::EventsResumed(page),
                    ),
                    Err(error) => {
                        durable_cursor_error(envelope.client_command_id.clone(), cursor, &error)
                    }
                }
            }
            ClientCommand::ArtifactManifest {
                project_id,
                artifact_occurrence_id,
            } => {
                let Some(runtime) = self.ready_runtime() else {
                    return Self::runtime_unavailable_reply(envelope);
                };
                runtime
                    .occurrence(project_id, artifact_occurrence_id)
                    .map_or_else(
                        || artifact_not_found(envelope),
                        |manifest| {
                            CommandReply::success(
                                envelope.client_command_id.clone(),
                                CommandResult::ArtifactManifest(manifest.clone()),
                            )
                        },
                    )
            }
            ClientCommand::ProjectCreate { name } => self.handle_project_create(envelope, name),
            ClientCommand::SessionStart { project_id } => {
                self.handle_session_start(envelope, project_id)
            }
            ClientCommand::TurnStart { session_id, prompt } => {
                self.handle_turn_start(envelope, session_id, prompt)
            }
            ClientCommand::TurnCancel {
                session_id,
                turn_id,
            } => self.handle_turn_cancel(envelope, session_id, turn_id),
            ClientCommand::QuestionRespond {
                session_id,
                question_id,
                choice_id,
            } => self.handle_question_respond(envelope, session_id, question_id, choice_id),
            ClientCommand::ApprovalRespond {
                session_id,
                approval_id,
                approval_subject_digest,
                decision,
            } => self.handle_approval_respond(
                envelope,
                session_id,
                approval_id,
                approval_subject_digest,
                *decision,
            ),
        }
    }
}

fn decode_durable_reply(envelope: &CommandEnvelope, raw_reply: &[u8]) -> CommandReply {
    serde_json::from_slice(raw_reply).unwrap_or_else(|_| {
        CommandReply::error(
            envelope.client_command_id.clone(),
            ProtocolErrorCode::ServiceUnavailable,
            "durable service is unavailable",
        )
    })
}

fn validated_project_name(name: &str) -> Option<&str> {
    let name = name.trim();
    (!name.is_empty() && name.chars().count() <= 120).then_some(name)
}

fn invalid_project_name_reply(envelope: &CommandEnvelope) -> CommandReply {
    CommandReply::error(
        envelope.client_command_id.clone(),
        ProtocolErrorCode::InvalidRequest,
        "project name must contain 1 to 120 characters",
    )
}

fn validated_turn_prompt(prompt: &str) -> Option<&str> {
    let prompt = prompt.trim();
    (!prompt.is_empty() && prompt.len() <= 8_000).then_some(prompt)
}

fn invalid_turn_prompt_reply(envelope: &CommandEnvelope) -> CommandReply {
    CommandReply::error(
        envelope.client_command_id.clone(),
        ProtocolErrorCode::InvalidRequest,
        "turn prompt must contain 1 to 8000 UTF-8 bytes",
    )
}

fn reserved_internal_namespace_reply(envelope: &CommandEnvelope) -> CommandReply {
    CommandReply::error(
        envelope.client_command_id.clone(),
        ProtocolErrorCode::InvalidRequest,
        "reserved internal client namespace is not available to external commands",
    )
}

fn idempotency_conflict_reply(envelope: &CommandEnvelope) -> CommandReply {
    CommandReply::error(
        envelope.client_command_id.clone(),
        ProtocolErrorCode::IdempotencyConflict,
        "the client command ID was already used with a different payload",
    )
}

impl DurableServiceError {
    fn into_reply(self, client_command_id: ClientCommandId) -> CommandReply {
        CommandReply::error(
            client_command_id,
            ProtocolErrorCode::ServiceUnavailable,
            self.to_string(),
        )
    }
}

fn validate_protocol_version(envelope: &CommandEnvelope) -> Option<CommandReply> {
    (envelope.protocol_version != PROTOCOL_VERSION).then(|| {
        CommandReply::error(
            envelope.client_command_id.clone(),
            ProtocolErrorCode::InvalidProtocolVersion,
            format!(
                "unsupported protocol version {}; expected {PROTOCOL_VERSION}",
                envelope.protocol_version
            ),
        )
    })
}

fn initialized_reply(client_command_id: ClientCommandId) -> CommandReply {
    CommandReply::success(
        client_command_id,
        CommandResult::Initialized {
            server_name: "alda-agent".to_owned(),
            protocol_version: PROTOCOL_VERSION,
            capabilities: vec![
                "project.create".to_owned(),
                "project.snapshot".to_owned(),
                "project.domain_snapshot".to_owned(),
                "revision.list".to_owned(),
                "revision.read".to_owned(),
                "session.start".to_owned(),
                "session.snapshot".to_owned(),
                "turn.start".to_owned(),
                "turn.cancel".to_owned(),
                "question.respond".to_owned(),
                "approval.respond".to_owned(),
                "artifact.manifest".to_owned(),
                "event.resume".to_owned(),
            ],
        },
    )
}

fn initialize_domain_project(
    project_id: &ProjectId,
    ordinal: u64,
) -> Result<crate::state::ProjectCoordinator, crate::domain::DomainError> {
    crate::state::ProjectCoordinator::initialize(
        crate::domain::DomainProjectId::parse(project_id.0.clone())?,
        crate::domain::ScoreId::parse(format!("score-{ordinal}"))?,
        crate::domain::TakeId::parse(format!("take-{ordinal}"))?,
        crate::domain::BranchId::parse(format!("branch-{ordinal}"))?,
    )
}

fn project_not_found(envelope: &CommandEnvelope, project_id: &ProjectId) -> CommandReply {
    CommandReply::error(
        envelope.client_command_id.clone(),
        ProtocolErrorCode::ProjectNotFound,
        format!("project `{}` was not found", project_id.0),
    )
}

fn map_domain_snapshot(
    project_id: &ProjectId,
    snapshot: &crate::state::ProjectSnapshot,
) -> Result<DomainProjectSnapshotV1, crate::domain::DomainError> {
    Ok(DomainProjectSnapshotV1 {
        schema_version: 1,
        project_id: project_id.clone(),
        score_id: snapshot
            .score_id
            .as_ref()
            .ok_or(crate::domain::DomainError::ProjectionCorrupt)?
            .as_str()
            .to_owned(),
        active_brief_id: snapshot
            .active_brief
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        accepted_revision_id: snapshot
            .accepted_revision
            .as_ref()
            .map(|id| ScoreRevisionId(id.as_str().to_owned())),
        takes: snapshot
            .takes
            .iter()
            .map(|(id, take)| TakeSummaryV1 {
                take_id: id.as_str().to_owned(),
                common_base_revision_id: take
                    .common_base
                    .as_ref()
                    .map(|id| ScoreRevisionId(id.as_str().to_owned())),
            })
            .collect(),
        branches: snapshot
            .branches
            .iter()
            .map(|(id, branch)| BranchSummaryV1 {
                branch_id: id.as_str().to_owned(),
                take_id: branch.take_id.as_str().to_owned(),
                head_revision_id: branch
                    .head
                    .as_ref()
                    .map(|id| ScoreRevisionId(id.as_str().to_owned())),
                fork_base_revision_id: branch
                    .fork_base
                    .as_ref()
                    .map(|id| ScoreRevisionId(id.as_str().to_owned())),
            })
            .collect(),
        revisions: snapshot
            .revisions
            .values()
            .map(|revision| map_revision_summary(snapshot, revision))
            .collect(),
        projection_digest: snapshot.canonical_digest()?,
    })
}

fn lifecycle_name(lifecycle: crate::domain::RevisionLifecycle) -> &'static str {
    match lifecycle {
        crate::domain::RevisionLifecycle::Draft => "draft",
        crate::domain::RevisionLifecycle::Candidate => "candidate",
        crate::domain::RevisionLifecycle::Accepted => "accepted",
        crate::domain::RevisionLifecycle::Rejected => "rejected",
        crate::domain::RevisionLifecycle::Aborted => "aborted",
    }
}

fn map_revision_summary(
    snapshot: &crate::state::ProjectSnapshot,
    revision: &crate::domain::ScoreRevision,
) -> RevisionSummaryV1 {
    RevisionSummaryV1 {
        revision_id: ScoreRevisionId(revision.id.as_str().to_owned()),
        take_id: revision.take_id.as_str().to_owned(),
        branch_id: revision.branch_id.as_str().to_owned(),
        parent_revision_ids: revision
            .parents
            .iter()
            .map(|id| ScoreRevisionId(id.as_str().to_owned()))
            .collect(),
        lifecycle: snapshot
            .lifecycle
            .get(&revision.id)
            .copied()
            .map_or("unknown", lifecycle_name)
            .to_owned(),
        source_artifact_hash: revision.source_artifact.as_str().to_owned(),
    }
}

fn map_revision_detail(
    project_id: &ProjectId,
    snapshot: &crate::state::ProjectSnapshot,
    revision: &crate::domain::ScoreRevision,
) -> RevisionDetailV1 {
    RevisionDetailV1 {
        summary: map_revision_summary(snapshot, revision),
        project_id: project_id.clone(),
        score_id: revision.score_id.as_str().to_owned(),
        brief_revision_id: revision.brief_revision_id.as_str().to_owned(),
        ir_artifact_hash: revision
            .ir_artifact
            .as_ref()
            .map(|hash| hash.as_str().to_owned()),
        origin: match revision.origin {
            crate::domain::RevisionOrigin::Human => "human",
            crate::domain::RevisionOrigin::Agent => "agent",
            crate::domain::RevisionOrigin::DeterministicFixture => "deterministic_fixture",
        }
        .to_owned(),
    }
}

impl ServiceState {
    fn handle(&mut self, envelope: CommandEnvelope) -> CommandReply {
        let key = (
            envelope.client_id.clone(),
            envelope.client_command_id.clone(),
        );
        let request_fingerprint = request_fingerprint(&envelope);

        if let Some(stored) = self.idempotency.get(&key) {
            if stored.request_fingerprint == request_fingerprint {
                return stored.reply.clone();
            }
            return CommandReply::error(
                envelope.client_command_id,
                ProtocolErrorCode::IdempotencyConflict,
                "the client command ID was already used with a different payload",
            );
        }

        let reply = self.process(&envelope);
        if !matches!(
            &reply.outcome,
            crate::protocol::CommandOutcome::Error {
                error: crate::protocol::ProtocolError {
                    code: ProtocolErrorCode::ArtifactPreparationFailed,
                    ..
                }
            }
        ) {
            self.idempotency.insert(
                key,
                StoredReply {
                    request_fingerprint,
                    reply: reply.clone(),
                },
            );
        }
        reply
    }

    // 集中保留穷尽的命令分派，便于审计 wire surface；状态迁移仍只发生在单一 actor 内。
    #[allow(clippy::too_many_lines)]
    fn process(&mut self, envelope: &CommandEnvelope) -> CommandReply {
        if let Some(reply) = validate_protocol_version(envelope) {
            return reply;
        }

        match &envelope.command {
            ClientCommand::Initialize => initialized_reply(envelope.client_command_id.clone()),
            ClientCommand::ProjectCreate { name } => {
                let Some(name) = validated_project_name(name) else {
                    return invalid_project_name_reply(envelope);
                };

                self.next_project_number += 1;
                let snapshot = ProjectSnapshot {
                    project_id: ProjectId(format!("project-{}", self.next_project_number)),
                    name: name.to_owned(),
                    version: 1,
                };
                let domain_project =
                    match initialize_domain_project(&snapshot.project_id, self.next_project_number)
                    {
                        Ok(project) => project,
                        Err(error) => {
                            return CommandReply::error(
                                envelope.client_command_id.clone(),
                                ProtocolErrorCode::ServiceUnavailable,
                                format!("failed to initialize the project domain: {error}"),
                            );
                        }
                    };
                self.projects
                    .insert(snapshot.project_id.clone(), snapshot.clone());
                self.domain_projects
                    .insert(snapshot.project_id.clone(), domain_project);
                CommandReply::success(
                    envelope.client_command_id.clone(),
                    CommandResult::ProjectCreated(snapshot),
                )
            }
            ClientCommand::ProjectSnapshot { project_id } => {
                if let Some(snapshot) = self.projects.get(project_id) {
                    CommandReply::success(
                        envelope.client_command_id.clone(),
                        CommandResult::ProjectSnapshot(snapshot.clone()),
                    )
                } else {
                    CommandReply::error(
                        envelope.client_command_id.clone(),
                        ProtocolErrorCode::ProjectNotFound,
                        format!("project `{}` was not found", project_id.0),
                    )
                }
            }
            ClientCommand::ProjectDomainSnapshot { project_id } => {
                let Some(project) = self.domain_projects.get(project_id) else {
                    return project_not_found(envelope, project_id);
                };
                match map_domain_snapshot(project_id, project.snapshot()) {
                    Ok(snapshot) => CommandReply::success(
                        envelope.client_command_id.clone(),
                        CommandResult::ProjectDomainSnapshot(snapshot),
                    ),
                    Err(error) => CommandReply::error(
                        envelope.client_command_id.clone(),
                        ProtocolErrorCode::ServiceUnavailable,
                        format!("failed to map the project domain: {error}"),
                    ),
                }
            }
            ClientCommand::RevisionList { project_id } => {
                let Some(project) = self.domain_projects.get(project_id) else {
                    return project_not_found(envelope, project_id);
                };
                let revisions = project
                    .snapshot()
                    .revisions
                    .values()
                    .map(|revision| map_revision_summary(project.snapshot(), revision))
                    .collect();
                CommandReply::success(
                    envelope.client_command_id.clone(),
                    CommandResult::RevisionList(revisions),
                )
            }
            ClientCommand::RevisionRead {
                project_id,
                revision_id,
            } => {
                let Some(project) = self.domain_projects.get(project_id) else {
                    return project_not_found(envelope, project_id);
                };
                let Ok(domain_revision_id) =
                    crate::domain::RevisionId::parse(revision_id.0.clone())
                else {
                    return CommandReply::error(
                        envelope.client_command_id.clone(),
                        ProtocolErrorCode::InvalidRequest,
                        "revision ID is invalid",
                    );
                };
                let Some(revision) = project.snapshot().revisions.get(&domain_revision_id) else {
                    return CommandReply::error(
                        envelope.client_command_id.clone(),
                        ProtocolErrorCode::RevisionNotFound,
                        format!("revision `{}` was not found", revision_id.0),
                    );
                };
                CommandReply::success(
                    envelope.client_command_id.clone(),
                    CommandResult::RevisionRead(map_revision_detail(
                        project_id,
                        project.snapshot(),
                        revision,
                    )),
                )
            }
            ClientCommand::SessionStart { project_id } => {
                if !self.projects.contains_key(project_id) {
                    return CommandReply::error(
                        envelope.client_command_id.clone(),
                        ProtocolErrorCode::ProjectNotFound,
                        format!("project `{}` was not found", project_id.0),
                    );
                }
                self.next_session_number += 1;
                let session_id = SessionId(format!("session-{}", self.next_session_number));
                let event = SessionEvent {
                    sequence: 1,
                    event: SessionEventKind::SessionStarted {
                        session_id: session_id.clone(),
                        project_id: project_id.clone(),
                    },
                };
                let state = SessionState {
                    project_id: project_id.clone(),
                    events: vec![event],
                    projection: SessionProjection::default(),
                };
                let snapshot = state.snapshot(session_id.clone());
                self.sessions.insert(session_id, state);
                CommandReply::success(
                    envelope.client_command_id.clone(),
                    CommandResult::SessionStarted(snapshot),
                )
            }
            ClientCommand::SessionSnapshot { session_id } => {
                let Some(session) = self.sessions.get(session_id) else {
                    return session_not_found(envelope, session_id);
                };
                CommandReply::success(
                    envelope.client_command_id.clone(),
                    CommandResult::SessionSnapshot(session.snapshot(session_id.clone())),
                )
            }
            ClientCommand::TurnStart { session_id, prompt } => {
                let Some(prompt) = validated_turn_prompt(prompt) else {
                    return invalid_turn_prompt_reply(envelope);
                };
                let Some(session) = self.sessions.get_mut(session_id) else {
                    return session_not_found(envelope, session_id);
                };
                self.next_turn_number += 1;
                self.next_question_number += 1;
                let turn_id = TurnId(format!("turn-{}", self.next_turn_number));
                let turn = TurnSnapshot {
                    turn_id: turn_id.clone(),
                    status: TurnStatus::Running,
                    terminal_sequence: None,
                };
                session.append(SessionEventKind::TurnStarted {
                    turn_id: turn_id.clone(),
                });
                let question_id = QuestionId(format!("question-{}", self.next_question_number));
                let question = PendingQuestion {
                    question_id: question_id.clone(),
                    session_id: session_id.clone(),
                    owner_turn_id: turn_id.clone(),
                    prompt: "请选择作品长度".to_owned(),
                    choices: vec![
                        QuestionChoice {
                            choice_id: ChoiceId("bars_8".to_owned()),
                            label: "8 bars".to_owned(),
                        },
                        QuestionChoice {
                            choice_id: ChoiceId("bars_16".to_owned()),
                            label: "16 bars".to_owned(),
                        },
                    ],
                    status: QuestionStatus::Pending,
                    created_sequence: session.head_sequence() + 1,
                    terminal_sequence: None,
                    answer: None,
                    responder_client_id: None,
                };
                session.append(SessionEventKind::QuestionRequested {
                    question: question.clone(),
                });
                self.turn_owners.insert(turn_id, session_id.clone());
                self.question_owners.insert(question_id, session_id.clone());
                self.turn_prompts
                    .insert(turn.turn_id.clone(), prompt.to_owned());
                CommandReply::success(
                    envelope.client_command_id.clone(),
                    CommandResult::TurnStarted(
                        session
                            .projection
                            .turns
                            .last()
                            .expect("turn start event creates a turn")
                            .clone(),
                    ),
                )
            }
            ClientCommand::TurnCancel {
                session_id,
                turn_id,
            } => self.cancel_turn(envelope, session_id, turn_id),
            ClientCommand::QuestionRespond {
                session_id,
                question_id,
                choice_id,
            } => self.respond_question(envelope, session_id, question_id, choice_id),
            ClientCommand::ApprovalRespond {
                session_id,
                approval_id,
                approval_subject_digest,
                decision,
            } => self.respond_approval(
                envelope,
                session_id,
                approval_id,
                approval_subject_digest,
                *decision,
            ),
            ClientCommand::ArtifactManifest {
                project_id,
                artifact_occurrence_id,
            } => {
                let Some(manifest) = self.occurrences.get(artifact_occurrence_id) else {
                    return artifact_not_found(envelope);
                };
                if &manifest.project_id != project_id {
                    return artifact_not_found(envelope);
                }
                CommandReply::success(
                    envelope.client_command_id.clone(),
                    CommandResult::ArtifactManifest(manifest.clone()),
                )
            }
            ClientCommand::EventResume { cursor } => {
                self.resolve_events(cursor, envelope.client_command_id.clone())
            }
        }
    }

    fn resolve_events(
        &self,
        cursor: &crate::protocol::StreamCursor,
        reply_id: ClientCommandId,
    ) -> CommandReply {
        if cursor.stream_kind != StreamKind::SessionRollout {
            return CommandReply::error_with_details(
                reply_id,
                ProtocolErrorCode::UnsupportedStreamKind,
                "A1 only supports the session_rollout stream kind",
                Some(ProtocolErrorDetails {
                    expected_epoch: None,
                    actual_epoch: None,
                    head_sequence: None,
                    recovery_action: RecoveryAction::UseSupportedStreamKind,
                }),
            );
        }
        let session_id = SessionId(cursor.stream_id.clone());
        let Some(session) = self.sessions.get(&session_id) else {
            return CommandReply::error_with_details(
                reply_id,
                ProtocolErrorCode::SessionNotFound,
                format!("session `{}` was not found", session_id.0),
                Some(ProtocolErrorDetails {
                    expected_epoch: None,
                    actual_epoch: None,
                    head_sequence: None,
                    recovery_action: RecoveryAction::None,
                }),
            );
        };
        let head = session.head_sequence();
        if cursor.epoch != SESSION_STREAM_EPOCH {
            return direct_cursor_error(
                reply_id,
                ProtocolErrorCode::CursorEpochMismatch,
                "session stream epoch does not match",
                &session_id,
                cursor.epoch,
                head,
            );
        }
        if cursor.after_sequence > head {
            return direct_cursor_error(
                reply_id,
                ProtocolErrorCode::InvalidCursor,
                "cursor is ahead of the session stream",
                &session_id,
                cursor.epoch,
                head,
            );
        }
        let events: Vec<_> = session
            .events
            .iter()
            .filter(|event| event.sequence > cursor.after_sequence)
            .take(EVENT_PAGE_LIMIT)
            .cloned()
            .collect();
        let next_after_sequence = events
            .last()
            .map_or(cursor.after_sequence, |event| event.sequence);
        CommandReply::success(
            reply_id,
            CommandResult::EventsResumed(EventPage {
                stream_kind: StreamKind::SessionRollout,
                stream_id: cursor.stream_id.clone(),
                epoch: SESSION_STREAM_EPOCH,
                head_sequence: head,
                events,
                next_after_sequence,
            }),
        )
    }

    // 取消事务将校验与完整、可审计的事件顺序相邻放置。
    #[allow(clippy::too_many_lines)]
    fn cancel_turn(
        &mut self,
        envelope: &CommandEnvelope,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> CommandReply {
        if !self.sessions.contains_key(session_id) {
            return session_not_found(envelope, session_id);
        }
        match self.turn_owners.get(turn_id) {
            None => {
                return CommandReply::error(
                    envelope.client_command_id.clone(),
                    ProtocolErrorCode::TurnNotFound,
                    format!("turn `{}` was not found", turn_id.0),
                );
            }
            Some(owner) if owner != session_id => {
                return CommandReply::error(
                    envelope.client_command_id.clone(),
                    ProtocolErrorCode::TurnOwnershipMismatch,
                    format!(
                        "turn `{}` does not belong to session `{}`",
                        turn_id.0, session_id.0
                    ),
                );
            }
            Some(_) => {}
        }

        let session = self
            .sessions
            .get_mut(session_id)
            .expect("session existence checked above");
        let turn_index = session
            .projection
            .turns
            .iter()
            .position(|turn| &turn.turn_id == turn_id)
            .expect("turn owner index and session projection must agree");
        if session.projection.turns[turn_index].status.is_terminal() {
            let turn = &session.projection.turns[turn_index];
            return CommandReply::success(
                envelope.client_command_id.clone(),
                CommandResult::TurnAlreadyTerminal {
                    turn_id: turn.turn_id.clone(),
                    terminal_status: turn.status,
                    terminal_sequence: turn
                        .terminal_sequence
                        .expect("terminal turn must record its terminal sequence"),
                },
            );
        }

        session.append(SessionEventKind::TurnCancelRequested {
            turn_id: turn_id.clone(),
        });

        let mut pending_objects: Vec<(u64, PendingObjectId)> = session
            .projection
            .questions
            .iter()
            .filter(|question| {
                question.owner_turn_id == *turn_id && question.status == QuestionStatus::Pending
            })
            .map(|question| {
                (
                    question.created_sequence,
                    PendingObjectId::Question(question.question_id.clone()),
                )
            })
            .chain(
                session
                    .projection
                    .approvals
                    .iter()
                    .filter(|approval| {
                        approval.owner_turn_id == *turn_id
                            && approval.status == ApprovalStatus::Pending
                    })
                    .map(|approval| {
                        (
                            approval.created_sequence,
                            PendingObjectId::Approval(approval.approval_id.clone()),
                        )
                    }),
            )
            .collect();
        pending_objects.sort_by_key(|(sequence, _)| *sequence);
        for (_, object_id) in pending_objects {
            match object_id {
                PendingObjectId::Question(question_id) => {
                    session.append(SessionEventKind::QuestionOwnerTurnAborted {
                        question_id,
                        owner_turn_id: turn_id.clone(),
                        owner_terminal_status: TurnStatus::Cancelled,
                    });
                }
                PendingObjectId::Approval(approval_id) => {
                    session.append(SessionEventKind::ApprovalOwnerTurnAborted {
                        approval_id,
                        owner_turn_id: turn_id.clone(),
                        owner_terminal_status: TurnStatus::Cancelled,
                    });
                }
            }
        }
        session.append(SessionEventKind::TurnCompleted {
            turn_id: turn_id.clone(),
            status: TurnStatus::Cancelled,
        });
        CommandReply::success(
            envelope.client_command_id.clone(),
            CommandResult::TurnCancelled(session.projection.turns[turn_index].clone()),
        )
    }

    // 响应事务先完成校验，再追加相互关联的 resolution 与下一项 approval request 事实。
    #[allow(clippy::too_many_lines)]
    fn respond_question(
        &mut self,
        envelope: &CommandEnvelope,
        session_id: &SessionId,
        question_id: &QuestionId,
        choice_id: &ChoiceId,
    ) -> CommandReply {
        if let Some(reply) = validate_object_owner(
            envelope,
            session_id,
            question_id,
            &self.sessions,
            &self.question_owners,
            (
                ProtocolErrorCode::QuestionNotFound,
                ProtocolErrorCode::QuestionOwnershipMismatch,
            ),
            "question",
        ) {
            return reply;
        }
        let session = self
            .sessions
            .get_mut(session_id)
            .expect("question owner validation confirms session");
        let question = session
            .projection
            .questions
            .iter()
            .find(|question| &question.question_id == question_id)
            .expect("question owner index and projection must agree")
            .clone();
        match question.status {
            QuestionStatus::OwnerTurnAborted => {
                return CommandReply::error(
                    envelope.client_command_id.clone(),
                    ProtocolErrorCode::RequestOwnerTurnAborted,
                    "the question owner Turn is terminal",
                );
            }
            QuestionStatus::Answered => {
                return CommandReply::success(
                    envelope.client_command_id.clone(),
                    CommandResult::QuestionAlreadyResolved(question),
                );
            }
            QuestionStatus::Pending => {}
        }
        if choice_id.0.is_empty() || choice_id.0.chars().count() > 120 {
            return CommandReply::error(
                envelope.client_command_id.clone(),
                ProtocolErrorCode::InvalidRequest,
                "choice ID must contain 1 to 120 characters",
            );
        }
        if !question
            .choices
            .iter()
            .any(|choice| &choice.choice_id == choice_id)
        {
            return CommandReply::error(
                envelope.client_command_id.clone(),
                ProtocolErrorCode::InvalidQuestionChoice,
                "choice ID must identify one of the question choices",
            );
        }
        session.append(SessionEventKind::QuestionResolved {
            question_id: question_id.clone(),
            choice_id: choice_id.clone(),
            responder_client_id: envelope.client_id.clone(),
        });

        self.next_approval_number += 1;
        let approval_id = ApprovalId(format!("approval-{}", self.next_approval_number));
        let prompt = self
            .turn_prompts
            .get(&question.owner_turn_id)
            .expect("active question owner must retain its prompt");
        let subject_digest = approval_subject_digest(
            "https://api.openai.com",
            &["constraints", "prompt", "constraints"],
            &question.owner_turn_id,
            prompt,
        );
        let approval = PendingApproval {
            approval_id: approval_id.clone(),
            session_id: session_id.clone(),
            owner_turn_id: question.owner_turn_id,
            payload: ApprovalPayload {
                action: "Send the Fake Action Plan fields to the configured model provider"
                    .to_owned(),
                effect: EffectClass::ModelEgress,
                target: "https://api.openai.com".to_owned(),
                scope: "prompt, constraints".to_owned(),
                estimated_impact: "The listed fields would leave the local process".to_owned(),
            },
            approval_subject_digest: subject_digest,
            status: ApprovalStatus::Pending,
            created_sequence: session.head_sequence() + 1,
            terminal_sequence: None,
            decision: None,
            responder_client_id: None,
        };
        session.append(SessionEventKind::ApprovalRequested {
            approval: approval.clone(),
        });
        self.approval_owners.insert(approval_id, session_id.clone());
        CommandReply::success(
            envelope.client_command_id.clone(),
            CommandResult::QuestionAnswered(
                session
                    .projection
                    .questions
                    .iter()
                    .find(|item| &item.question_id == question_id)
                    .expect("resolved question remains projected")
                    .clone(),
            ),
        )
    }

    // 校验、本地 fixture 准备、原子 store/事实提交与 stable reply 相邻放置，便于审计迁移。
    #[allow(clippy::too_many_lines)]
    fn respond_approval(
        &mut self,
        envelope: &CommandEnvelope,
        session_id: &SessionId,
        approval_id: &ApprovalId,
        supplied_digest: &ApprovalSubjectDigest,
        decision: ApprovalDecision,
    ) -> CommandReply {
        if let Some(reply) = validate_object_owner(
            envelope,
            session_id,
            approval_id,
            &self.sessions,
            &self.approval_owners,
            (
                ProtocolErrorCode::ApprovalNotFound,
                ProtocolErrorCode::ApprovalOwnershipMismatch,
            ),
            "approval",
        ) {
            return reply;
        }
        let session = self
            .sessions
            .get(session_id)
            .expect("approval owner validation confirms session");
        let approval = session
            .projection
            .approvals
            .iter()
            .find(|approval| &approval.approval_id == approval_id)
            .expect("approval owner index and projection must agree")
            .clone();
        if approval.status == ApprovalStatus::OwnerTurnAborted {
            return CommandReply::error(
                envelope.client_command_id.clone(),
                ProtocolErrorCode::RequestOwnerTurnAborted,
                "the approval owner Turn is terminal",
            );
        }
        if approval.status != ApprovalStatus::Pending {
            return CommandReply::success(
                envelope.client_command_id.clone(),
                CommandResult::ApprovalAlreadyResolved(approval),
            );
        }
        if supplied_digest != &approval.approval_subject_digest {
            return CommandReply::error(
                envelope.client_command_id.clone(),
                ProtocolErrorCode::ApprovalSubjectMismatch,
                "approval subject digest does not match the requested action",
            );
        }
        let prepared = if decision == ApprovalDecision::Approve {
            match prepare_fixture(self.fixture_fault) {
                Ok(prepared) => Some(prepared),
                Err(()) => {
                    return CommandReply::error(
                        envelope.client_command_id.clone(),
                        ProtocolErrorCode::ArtifactPreparationFailed,
                        "the Fake Provider fixture failed hash or size verification",
                    );
                }
            }
        } else {
            None
        };

        let project_id = session.project_id.clone();
        let created_sequence = session.head_sequence() + 1;
        let session = self
            .sessions
            .get_mut(session_id)
            .expect("approval owner validation confirms session");
        let artifact_manifest = prepared.map(|prepared| {
            self.next_artifact_occurrence_number += 1;
            let occurrence_id = ArtifactOccurrenceId(format!(
                "artifact-occurrence-{}",
                self.next_artifact_occurrence_number
            ));
            let manifest = ArtifactManifest {
                artifact_occurrence_id: occurrence_id.clone(),
                artifact_hash: prepared.hash.clone(),
                kind: ArtifactKind::AldaSource,
                mime_type: prepared.mime_type.clone(),
                size_bytes: prepared.size_bytes,
                producer: ArtifactProducer::FakeProviderFixtureV1,
                project_id: project_id.clone(),
                source_session_id: session_id.clone(),
                source_turn_id: approval.owner_turn_id.clone(),
                fixture_version: 1,
                created_sequence,
                provenance_label: "A3 deterministic Fake Provider Alda source fixture".to_owned(),
                durability: ArtifactDurability::ProcessLifetimeFixture,
            };
            self.blobs
                .entry(prepared.hash.clone())
                .or_insert(BlobRecord {
                    bytes: prepared.bytes,
                    size_bytes: prepared.size_bytes,
                    mime_type: prepared.mime_type,
                });
            self.reachability
                .insert((project_id.clone(), prepared.hash));
            self.occurrences.insert(occurrence_id, manifest.clone());
            manifest
        });
        session.append(SessionEventKind::ApprovalResolved {
            approval_id: approval_id.clone(),
            approval_subject_digest: supplied_digest.clone(),
            decision,
            responder_client_id: envelope.client_id.clone(),
        });
        let terminal_status = match decision {
            ApprovalDecision::Approve => TurnStatus::Succeeded,
            ApprovalDecision::Deny => TurnStatus::Failed,
        };
        session.append(SessionEventKind::TurnCompleted {
            turn_id: approval.owner_turn_id,
            status: terminal_status,
        });
        CommandReply::success(
            envelope.client_command_id.clone(),
            CommandResult::ApprovalDecided {
                approval: session
                    .projection
                    .approvals
                    .iter()
                    .find(|item| &item.approval_id == approval_id)
                    .expect("resolved approval remains projected")
                    .clone(),
                artifact_manifest,
            },
        )
    }

    fn resolve_download(
        &self,
        project_id: &ProjectId,
        hash: &ArtifactHash,
        if_none_match: Option<&str>,
    ) -> DownloadResolution {
        if !self
            .reachability
            .contains(&(project_id.clone(), hash.clone()))
        {
            return DownloadResolution::NotFound;
        }
        let Some(blob) = self.blobs.get(hash) else {
            return DownloadResolution::Corrupt;
        };
        let actual_size = u64::try_from(blob.bytes.len()).expect("fixture size fits u64");
        let actual_hash = artifact_hash(&blob.bytes);
        if actual_size != blob.size_bytes || &actual_hash != hash {
            return DownloadResolution::Corrupt;
        }
        let etag = format!("\"{}\"", hash.as_str());
        if if_none_match == Some(etag.as_str()) {
            return DownloadResolution::NotModified(hash.clone());
        }
        DownloadResolution::Verified(VerifiedDownload {
            artifact_hash: hash.clone(),
            mime_type: blob.mime_type.clone(),
            size_bytes: blob.size_bytes,
            bytes: Arc::clone(&blob.bytes),
        })
    }
}

struct PreparedArtifact {
    hash: ArtifactHash,
    bytes: Arc<[u8]>,
    size_bytes: u64,
    mime_type: String,
}

enum PendingObjectId {
    Question(QuestionId),
    Approval(ApprovalId),
}

impl SessionState {
    fn head_sequence(&self) -> u64 {
        u64::try_from(self.events.len()).expect("session event count must fit u64")
    }

    fn append(&mut self, event: SessionEventKind) -> u64 {
        let sequence = self.head_sequence() + 1;
        let event = SessionEvent { sequence, event };
        reduce(&mut self.projection, &event);
        self.events.push(event);
        sequence
    }

    fn snapshot(&self, session_id: SessionId) -> SessionSnapshot {
        SessionSnapshot {
            session_id,
            project_id: self.project_id.clone(),
            stream_epoch: SESSION_STREAM_EPOCH,
            covered_through_sequence: self.head_sequence(),
            turns: self.projection.turns.clone(),
            questions: self.projection.questions.clone(),
            approvals: self.projection.approvals.clone(),
        }
    }
}

fn reduce(projection: &mut SessionProjection, event: &SessionEvent) {
    match &event.event {
        SessionEventKind::SessionStarted { .. } => {}
        SessionEventKind::TurnStarted { turn_id } => projection.turns.push(TurnSnapshot {
            turn_id: turn_id.clone(),
            status: TurnStatus::Running,
            terminal_sequence: None,
        }),
        SessionEventKind::TurnCancelRequested { turn_id } => {
            find_turn_mut(projection, turn_id).status = TurnStatus::CancelRequested;
        }
        SessionEventKind::TurnCompleted { turn_id, status } => {
            let turn = find_turn_mut(projection, turn_id);
            turn.status = *status;
            turn.terminal_sequence = Some(event.sequence);
        }
        SessionEventKind::QuestionRequested { question } => {
            projection.questions.push(question.clone());
            find_turn_mut(projection, &question.owner_turn_id).status = TurnStatus::WaitingForInput;
        }
        SessionEventKind::QuestionResolved {
            question_id,
            choice_id,
            responder_client_id,
        } => {
            let owner_turn_id = {
                let question = projection
                    .questions
                    .iter_mut()
                    .find(|question| &question.question_id == question_id)
                    .expect("question resolved must follow its request");
                question.status = QuestionStatus::Answered;
                question.terminal_sequence = Some(event.sequence);
                question.answer = Some(QuestionAnswer {
                    choice_id: choice_id.clone(),
                });
                question.responder_client_id = Some(responder_client_id.clone());
                question.owner_turn_id.clone()
            };
            find_turn_mut(projection, &owner_turn_id).status = TurnStatus::Running;
        }
        SessionEventKind::ApprovalRequested { approval } => {
            projection.approvals.push(approval.clone());
            find_turn_mut(projection, &approval.owner_turn_id).status = TurnStatus::WaitingForInput;
        }
        SessionEventKind::ApprovalResolved {
            approval_id,
            approval_subject_digest,
            decision,
            responder_client_id,
        } => {
            let owner_turn_id = {
                let approval = projection
                    .approvals
                    .iter_mut()
                    .find(|approval| &approval.approval_id == approval_id)
                    .expect("approval resolved must follow its request");
                assert_eq!(
                    &approval.approval_subject_digest, approval_subject_digest,
                    "resolved approval digest must match requested subject"
                );
                approval.status = match decision {
                    ApprovalDecision::Approve => ApprovalStatus::Approved,
                    ApprovalDecision::Deny => ApprovalStatus::Denied,
                };
                approval.terminal_sequence = Some(event.sequence);
                approval.decision = Some(*decision);
                approval.responder_client_id = Some(responder_client_id.clone());
                approval.owner_turn_id.clone()
            };
            find_turn_mut(projection, &owner_turn_id).status = TurnStatus::Running;
        }
        SessionEventKind::QuestionOwnerTurnAborted {
            question_id,
            owner_turn_id,
            ..
        } => {
            let question = projection
                .questions
                .iter_mut()
                .find(|question| &question.question_id == question_id)
                .expect("question abort must follow its request");
            assert_eq!(&question.owner_turn_id, owner_turn_id);
            question.status = QuestionStatus::OwnerTurnAborted;
            question.terminal_sequence = Some(event.sequence);
        }
        SessionEventKind::ApprovalOwnerTurnAborted {
            approval_id,
            owner_turn_id,
            ..
        } => {
            let approval = projection
                .approvals
                .iter_mut()
                .find(|approval| &approval.approval_id == approval_id)
                .expect("approval abort must follow its request");
            assert_eq!(&approval.owner_turn_id, owner_turn_id);
            approval.status = ApprovalStatus::OwnerTurnAborted;
            approval.terminal_sequence = Some(event.sequence);
        }
    }
}

#[cfg(test)]
pub(crate) fn replay_session_events_for_test(
    events: &[SessionEvent],
) -> (
    Vec<TurnSnapshot>,
    Vec<PendingQuestion>,
    Vec<PendingApproval>,
) {
    let mut projection = SessionProjection::default();
    for event in events {
        reduce(&mut projection, event);
    }
    (projection.turns, projection.questions, projection.approvals)
}

fn find_turn_mut<'a>(
    projection: &'a mut SessionProjection,
    turn_id: &TurnId,
) -> &'a mut TurnSnapshot {
    projection
        .turns
        .iter_mut()
        .find(|turn| &turn.turn_id == turn_id)
        .expect("Turn lifecycle event must follow TurnStarted")
}

fn validate_object_owner<K>(
    envelope: &CommandEnvelope,
    session_id: &SessionId,
    object_id: &K,
    sessions: &HashMap<SessionId, SessionState>,
    owners: &HashMap<K, SessionId>,
    error_codes: (ProtocolErrorCode, ProtocolErrorCode),
    object_name: &str,
) -> Option<CommandReply>
where
    K: Eq + std::hash::Hash + std::fmt::Debug,
{
    if !sessions.contains_key(session_id) {
        return Some(session_not_found(envelope, session_id));
    }
    match owners.get(object_id) {
        None => Some(CommandReply::error(
            envelope.client_command_id.clone(),
            error_codes.0,
            format!("{object_name} `{object_id:?}` was not found"),
        )),
        Some(owner) if owner != session_id => Some(CommandReply::error(
            envelope.client_command_id.clone(),
            error_codes.1,
            format!(
                "{object_name} does not belong to session `{}`",
                session_id.0
            ),
        )),
        Some(_) => None,
    }
}

fn approval_subject_digest(
    provider_origin: &str,
    egress_field_names: &[&str],
    owner_turn_id: &TurnId,
    prompt: &str,
) -> ApprovalSubjectDigest {
    crate::protocol::approval_subject_digest_v1(
        provider_origin,
        egress_field_names,
        owner_turn_id,
        prompt,
    )
}

#[cfg(test)]
pub(crate) fn approval_subject_digest_for_test(
    provider_origin: &str,
    egress_field_names: &[&str],
    owner_turn_id: &TurnId,
    prompt: &str,
) -> ApprovalSubjectDigest {
    approval_subject_digest(provider_origin, egress_field_names, owner_turn_id, prompt)
}

const FIXTURE_BYTES: &[u8] = b"piano: o4 c8 d e f g a b > c\n";
const FIXTURE_MIME: &str = "text/x-alda; charset=utf-8";
const FIXTURE_SIZE: u64 = 29;
const FIXTURE_HASH: &str =
    "sha256:de66932c53e0e50127757614e9925d0b3675571c7298f944dc0c736f1b3a1be8";

fn artifact_hash(bytes: &[u8]) -> ArtifactHash {
    ArtifactHash::parse(&format!("sha256:{:x}", Sha256::digest(bytes)))
        .expect("SHA-256 formatter produces a valid ArtifactHash")
}

fn prepare_fixture(fault: FixtureFault) -> Result<PreparedArtifact, ()> {
    let bytes: Arc<[u8]> = Arc::from(FIXTURE_BYTES);
    let actual_hash = artifact_hash(&bytes);
    let actual_size = u64::try_from(bytes.len()).map_err(|_| ())?;
    let expected_hash = if fault == FixtureFault::HashMismatch {
        ArtifactHash::parse(
            "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        )
        .expect("test fault hash is valid")
    } else {
        ArtifactHash::parse(FIXTURE_HASH).expect("fixture hash constant must be valid")
    };
    let expected_size = if fault == FixtureFault::SizeMismatch {
        FIXTURE_SIZE + 1
    } else {
        FIXTURE_SIZE
    };
    if actual_hash != expected_hash || actual_size != expected_size {
        return Err(());
    }
    Ok(PreparedArtifact {
        hash: actual_hash,
        bytes,
        size_bytes: actual_size,
        mime_type: FIXTURE_MIME.to_owned(),
    })
}

fn artifact_not_found(envelope: &CommandEnvelope) -> CommandReply {
    CommandReply::error(
        envelope.client_command_id.clone(),
        ProtocolErrorCode::ArtifactNotFound,
        "artifact occurrence was not found",
    )
}

fn session_not_found(envelope: &CommandEnvelope, session_id: &SessionId) -> CommandReply {
    CommandReply::error_with_details(
        envelope.client_command_id.clone(),
        ProtocolErrorCode::SessionNotFound,
        format!("session `{}` was not found", session_id.0),
        Some(ProtocolErrorDetails {
            expected_epoch: None,
            actual_epoch: None,
            head_sequence: None,
            recovery_action: RecoveryAction::None,
        }),
    )
}

fn turn_not_found(envelope: &CommandEnvelope, turn_id: &TurnId) -> CommandReply {
    CommandReply::error(
        envelope.client_command_id.clone(),
        ProtocolErrorCode::TurnNotFound,
        format!("turn `{}` was not found", turn_id.0),
    )
}

fn question_not_found(envelope: &CommandEnvelope, question_id: &QuestionId) -> CommandReply {
    CommandReply::error(
        envelope.client_command_id.clone(),
        ProtocolErrorCode::QuestionNotFound,
        format!("question `{}` was not found", question_id.0),
    )
}

fn approval_not_found(envelope: &CommandEnvelope, approval_id: &ApprovalId) -> CommandReply {
    CommandReply::error(
        envelope.client_command_id.clone(),
        ProtocolErrorCode::ApprovalNotFound,
        format!("approval `{}` was not found", approval_id.0),
    )
}

fn direct_cursor_error(
    reply_id: ClientCommandId,
    code: ProtocolErrorCode,
    message: &str,
    session_id: &SessionId,
    actual_epoch: u64,
    head_sequence: u64,
) -> CommandReply {
    CommandReply::error_with_details(
        reply_id,
        code,
        message,
        Some(ProtocolErrorDetails {
            expected_epoch: Some(SESSION_STREAM_EPOCH),
            actual_epoch: Some(actual_epoch),
            head_sequence: Some(head_sequence),
            recovery_action: RecoveryAction::FetchSessionSnapshot(session_id.clone()),
        }),
    )
}

fn durable_cursor_error(
    reply_id: ClientCommandId,
    cursor: &crate::protocol::StreamCursor,
    error: &DurableCursorError,
) -> CommandReply {
    match error {
        DurableCursorError::UnsupportedStreamKind => CommandReply::error_with_details(
            reply_id,
            ProtocolErrorCode::UnsupportedStreamKind,
            "only the session_rollout stream kind is supported",
            Some(ProtocolErrorDetails {
                expected_epoch: None,
                actual_epoch: None,
                head_sequence: None,
                recovery_action: RecoveryAction::UseSupportedStreamKind,
            }),
        ),
        DurableCursorError::SessionNotFound => CommandReply::error_with_details(
            reply_id,
            ProtocolErrorCode::SessionNotFound,
            format!("session `{}` was not found", cursor.stream_id),
            Some(ProtocolErrorDetails {
                expected_epoch: None,
                actual_epoch: None,
                head_sequence: None,
                recovery_action: RecoveryAction::None,
            }),
        ),
        DurableCursorError::EpochMismatch {
            expected_epoch,
            actual_epoch,
            head_sequence,
        } => CommandReply::error_with_details(
            reply_id,
            ProtocolErrorCode::CursorEpochMismatch,
            "session stream epoch does not match",
            Some(ProtocolErrorDetails {
                expected_epoch: Some(*expected_epoch),
                actual_epoch: Some(*actual_epoch),
                head_sequence: Some(*head_sequence),
                recovery_action: RecoveryAction::FetchSessionSnapshot(SessionId(
                    cursor.stream_id.clone(),
                )),
            }),
        ),
        DurableCursorError::Future { head_sequence } => CommandReply::error_with_details(
            reply_id,
            ProtocolErrorCode::InvalidCursor,
            "cursor is ahead of the session stream",
            Some(ProtocolErrorDetails {
                expected_epoch: Some(SESSION_STREAM_EPOCH),
                actual_epoch: Some(cursor.epoch),
                head_sequence: Some(*head_sequence),
                recovery_action: RecoveryAction::FetchSessionSnapshot(SessionId(
                    cursor.stream_id.clone(),
                )),
            }),
        ),
        DurableCursorError::CorruptPublishedView => CommandReply::error(
            reply_id,
            ProtocolErrorCode::ServiceUnavailable,
            "durable published read view is inconsistent",
        ),
    }
}

fn request_fingerprint(envelope: &CommandEnvelope) -> String {
    serde_json::to_string(&(envelope.protocol_version, &envelope.command))
        .expect("serializing a typed command envelope should not fail")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable_runtime::CommandLookup;
    use crate::protocol::CommandOutcome;
    use crate::protocol::StreamCursor;
    use crate::protocol::external_command_payload_digest;

    fn durable_test_root() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::TempDir::new().expect("创建 durable backend 测试目录");
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
            .expect("设置 durable backend 测试目录权限");
        root
    }

    fn durable_file_snapshot(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
        fn visit(
            root: &std::path::Path,
            directory: &std::path::Path,
            files: &mut Vec<(std::path::PathBuf, Vec<u8>)>,
        ) {
            let mut entries = std::fs::read_dir(directory)
                .expect("读取 durable 测试目录")
                .collect::<Result<Vec<_>, _>>()
                .expect("枚举 durable 测试目录");
            entries.sort_unstable_by_key(std::fs::DirEntry::path);
            for entry in entries {
                let path = entry.path();
                let file_type = entry.file_type().expect("读取 durable 文件类型");
                if file_type.is_dir() {
                    visit(root, &path, files);
                } else if file_type.is_file() {
                    files.push((
                        path.strip_prefix(root)
                            .expect("durable 文件必须位于测试根目录")
                            .to_path_buf(),
                        std::fs::read(&path).expect("读取 durable 文件内容"),
                    ));
                }
            }
        }

        let mut files = Vec::new();
        visit(root, root, &mut files);
        files
    }

    fn assert_protocol_error(reply: &CommandReply, expected: ProtocolErrorCode) {
        assert!(matches!(
            &reply.outcome,
            CommandOutcome::Error { error } if error.code == expected
        ));
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "同一只读矩阵集中证明 published view、文件与 command index 均不改变"
    )]
    fn durable_service_backend_reads_do_not_write_control_wal() {
        let root = durable_test_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("打开 durable runtime");
        let mut state = DurableServiceState::new(runtime);
        let published_before = state
            .ready_runtime()
            .expect("durable backend 保持 Ready")
            .read_view()
            .clone();
        let files_before = durable_file_snapshot(root.path());
        let commands = [
            command("durable-initialize", ClientCommand::Initialize),
            command(
                "durable-project",
                ClientCommand::ProjectSnapshot {
                    project_id: ProjectId("project-missing".to_owned()),
                },
            ),
            command(
                "durable-domain",
                ClientCommand::ProjectDomainSnapshot {
                    project_id: ProjectId("project-missing".to_owned()),
                },
            ),
            command(
                "durable-revisions",
                ClientCommand::RevisionList {
                    project_id: ProjectId("project-missing".to_owned()),
                },
            ),
            command(
                "durable-revision",
                ClientCommand::RevisionRead {
                    project_id: ProjectId("project-missing".to_owned()),
                    revision_id: ScoreRevisionId("revision-missing".to_owned()),
                },
            ),
            command(
                "durable-session",
                ClientCommand::SessionSnapshot {
                    session_id: SessionId("session-missing".to_owned()),
                },
            ),
            command(
                "durable-resume",
                ClientCommand::EventResume {
                    cursor: StreamCursor {
                        stream_kind: StreamKind::SessionRollout,
                        stream_id: "session-missing".to_owned(),
                        epoch: SESSION_STREAM_EPOCH,
                        after_sequence: 0,
                    },
                },
            ),
            command(
                "durable-kind",
                ClientCommand::EventResume {
                    cursor: StreamCursor {
                        stream_kind: StreamKind::ProjectEvent,
                        stream_id: "session-missing".to_owned(),
                        epoch: SESSION_STREAM_EPOCH,
                        after_sequence: 0,
                    },
                },
            ),
        ];

        let replies = commands
            .iter()
            .map(|envelope| state.handle(envelope))
            .collect::<Vec<_>>();
        assert!(matches!(
            replies[0].outcome,
            CommandOutcome::Success {
                result: CommandResult::Initialized { .. }
            }
        ));
        for reply in &replies[1..5] {
            assert_protocol_error(reply, ProtocolErrorCode::ProjectNotFound);
        }
        assert_protocol_error(&replies[5], ProtocolErrorCode::SessionNotFound);
        assert_protocol_error(&replies[6], ProtocolErrorCode::SessionNotFound);
        assert_protocol_error(&replies[7], ProtocolErrorCode::UnsupportedStreamKind);

        assert_eq!(
            state
                .ready_runtime()
                .expect("只读命令后 durable backend 保持 Ready")
                .read_view(),
            &published_before
        );
        assert_eq!(durable_file_snapshot(root.path()), files_before);
        for envelope in &commands {
            let digest =
                external_command_payload_digest(envelope.protocol_version, &envelope.command)
                    .expect("计算 durable 只读命令摘要");
            assert_eq!(
                state
                    .ready_runtime()
                    .expect("只读命令后 durable backend 保持 Ready")
                    .lookup_command(&envelope.client_id, &envelope.client_command_id, &digest,)
                    .expect("查询 control command index"),
                CommandLookup::Unseen
            );
        }
    }

    #[test]
    fn durable_service_backend_rejects_missing_approval_without_memory_fallback() {
        let root = durable_test_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("打开 durable runtime");
        let mut state = DurableServiceState::new(runtime);
        let files_before = durable_file_snapshot(root.path());
        let mutation = command(
            "durable-approval-approve",
            ClientCommand::ApprovalRespond {
                session_id: SessionId("session-missing".to_owned()),
                approval_id: ApprovalId("approval-missing".to_owned()),
                approval_subject_digest: ApprovalSubjectDigest {
                    algorithm: "sha256".to_owned(),
                    schema_version: 1,
                    value: "0".repeat(64),
                },
                decision: ApprovalDecision::Approve,
            },
        );

        let reply = state.handle(&mutation);
        assert_protocol_error(&reply, ProtocolErrorCode::SessionNotFound);

        let query_command = command(
            "durable-question-respond-check",
            ClientCommand::ProjectSnapshot {
                project_id: ProjectId("project-1".to_owned()),
            },
        );
        let query = state.handle(&query_command);
        assert_protocol_error(&query, ProtocolErrorCode::ProjectNotFound);
        let digest = external_command_payload_digest(mutation.protocol_version, &mutation.command)
            .expect("计算未接线 mutation 摘要");
        assert_eq!(
            state
                .ready_runtime()
                .expect("拒绝 mutation 后 durable backend 保持 Ready")
                .lookup_command(&mutation.client_id, &mutation.client_command_id, &digest,)
                .expect("查询拒绝 mutation control index"),
            CommandLookup::Unseen
        );
        assert_eq!(durable_file_snapshot(root.path()), files_before);
    }

    fn assert_typed_hex_id(value: &str, prefix: &str) {
        let hex = value
            .strip_prefix(prefix)
            .and_then(|suffix| suffix.strip_prefix('-'))
            .expect("ID 使用冻结的 typed prefix");
        assert_eq!(hex.len(), 32);
        assert!(
            hex.bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
    }

    #[test]
    fn durable_project_create_commits_exact_reply_and_rebuilds_after_reopen() {
        let root = durable_test_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("打开 durable runtime");
        let mut state = DurableServiceState::new(runtime);
        let create = create_command("durable-project-create", "  Etude  ");

        let first = state.handle(&create);
        let CommandOutcome::Success {
            result: CommandResult::ProjectCreated(created),
        } = &first.outcome
        else {
            panic!("预期 durable ProjectCreated reply");
        };
        assert_eq!(created.name, "Etude");
        assert_eq!(created.version, 1);
        assert_typed_hex_id(&created.project_id.0, "project");

        let domain = state.handle(&command(
            "durable-project-domain",
            ClientCommand::ProjectDomainSnapshot {
                project_id: created.project_id.clone(),
            },
        ));
        let CommandOutcome::Success {
            result: CommandResult::ProjectDomainSnapshot(domain),
        } = domain.outcome
        else {
            panic!("预期已发布的 B1 Project projection");
        };
        assert_eq!(domain.project_id, created.project_id);
        assert_typed_hex_id(&domain.score_id, "score");
        assert_eq!(domain.takes.len(), 1);
        assert_typed_hex_id(&domain.takes[0].take_id, "take");
        assert_eq!(domain.branches.len(), 1);
        assert_typed_hex_id(&domain.branches[0].branch_id, "branch");

        let files_after_commit = durable_file_snapshot(root.path());
        assert_eq!(state.handle(&create), first);
        assert_eq!(durable_file_snapshot(root.path()), files_after_commit);

        let conflict = create_command("durable-project-create", "Nocturne");
        assert_protocol_error(
            &state.handle(&conflict),
            ProtocolErrorCode::IdempotencyConflict,
        );
        assert_eq!(durable_file_snapshot(root.path()), files_after_commit);

        let project_id = created.project_id.clone();
        drop(state);
        let runtime = ReadyDurableRuntime::open(root.path()).expect("重开 durable runtime");
        let mut reopened = DurableServiceState::new(runtime);
        let files_before_replay = durable_file_snapshot(root.path());
        assert_eq!(reopened.handle(&create), first);
        assert_eq!(durable_file_snapshot(root.path()), files_before_replay);
        let snapshot = reopened.handle(&command(
            "durable-project-snapshot",
            ClientCommand::ProjectSnapshot { project_id },
        ));
        assert!(matches!(
            snapshot.outcome,
            CommandOutcome::Success {
                result: CommandResult::ProjectSnapshot(ref value)
            } if value == created
        ));
    }

    #[test]
    fn durable_project_create_rejects_before_prepared_and_reserves_internal_namespace() {
        let root = durable_test_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("打开 durable runtime");
        let mut state = DurableServiceState::new(runtime);
        let files_before = durable_file_snapshot(root.path());
        let invalid = create_command("durable-project-invalid", "   ");

        assert_protocol_error(&state.handle(&invalid), ProtocolErrorCode::InvalidRequest);
        let invalid_digest =
            external_command_payload_digest(invalid.protocol_version, &invalid.command)
                .expect("计算无效 ProjectCreate 摘要");
        assert_eq!(
            state
                .ready_runtime()
                .expect("Prepared 前拒绝保持 Ready")
                .lookup_command(
                    &invalid.client_id,
                    &invalid.client_command_id,
                    &invalid_digest,
                )
                .expect("查询无效 ProjectCreate command index"),
            CommandLookup::Unseen
        );

        let internal = CommandEnvelope {
            protocol_version: PROTOCOL_VERSION,
            client_id: ClientId("__alda_internal_external-forgery".to_owned()),
            client_command_id: ClientCommandId("durable-project-internal".to_owned()),
            command: ClientCommand::ProjectCreate {
                name: "Etude".to_owned(),
            },
        };
        assert_protocol_error(&state.handle(&internal), ProtocolErrorCode::InvalidRequest);
        assert_eq!(durable_file_snapshot(root.path()), files_before);
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "同一测试连续证明 exact reply、发布 generation、事件页与重开幂等闭包"
    )]
    fn durable_session_start_commits_exact_reply_and_rebuilds_after_reopen() {
        let root = durable_test_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("打开 durable runtime");
        let mut state = DurableServiceState::new(runtime);
        let project = state.handle(&create_command("session-project", "Etude"));
        let CommandOutcome::Success {
            result: CommandResult::ProjectCreated(project),
        } = project.outcome
        else {
            panic!("预期 durable ProjectCreated reply");
        };
        let start = command(
            "durable-session-start",
            ClientCommand::SessionStart {
                project_id: project.project_id.clone(),
            },
        );

        let first = state.handle(&start);
        let CommandOutcome::Success {
            result: CommandResult::SessionStarted(created),
        } = &first.outcome
        else {
            panic!("预期 durable SessionStarted reply");
        };
        assert_typed_hex_id(&created.session_id.0, "session");
        assert_eq!(created.project_id, project.project_id);
        assert_eq!(created.stream_epoch, SESSION_STREAM_EPOCH);
        assert_eq!(created.covered_through_sequence, 1);
        assert!(created.turns.is_empty());
        assert!(created.questions.is_empty());
        assert!(created.approvals.is_empty());

        let published = state
            .ready_runtime()
            .expect("SessionStart 后 durable backend 保持 Ready")
            .read_view();
        assert_eq!(published.sessions.len(), 1);
        assert_eq!(
            published.sessions[&created.session_id.0].snapshot(),
            created
        );
        let snapshot = state.handle(&command(
            "durable-session-snapshot",
            ClientCommand::SessionSnapshot {
                session_id: created.session_id.clone(),
            },
        ));
        assert!(matches!(
            snapshot.outcome,
            CommandOutcome::Success {
                result: CommandResult::SessionSnapshot(ref value)
            } if value == created
        ));
        let resumed = state.handle(&command(
            "durable-session-resume",
            ClientCommand::EventResume {
                cursor: StreamCursor {
                    stream_kind: StreamKind::SessionRollout,
                    stream_id: created.session_id.0.clone(),
                    epoch: SESSION_STREAM_EPOCH,
                    after_sequence: 0,
                },
            },
        ));
        assert!(matches!(
            resumed.outcome,
            CommandOutcome::Success {
                result: CommandResult::EventsResumed(ref page)
            } if matches!(
                page.events.as_slice(),
                [SessionEvent {
                    sequence: 1,
                    event: SessionEventKind::SessionStarted { session_id, project_id },
                }] if session_id == &created.session_id && project_id == &created.project_id
            )
        ));

        let files_after_commit = durable_file_snapshot(root.path());
        assert_eq!(state.handle(&start), first);
        assert_eq!(durable_file_snapshot(root.path()), files_after_commit);
        let conflict = command(
            "durable-session-start",
            ClientCommand::SessionStart {
                project_id: ProjectId("project-different".to_owned()),
            },
        );
        assert_protocol_error(
            &state.handle(&conflict),
            ProtocolErrorCode::IdempotencyConflict,
        );
        assert_eq!(durable_file_snapshot(root.path()), files_after_commit);

        let session_id = created.session_id.clone();
        drop(state);
        let runtime = ReadyDurableRuntime::open(root.path()).expect("重开 durable runtime");
        let mut reopened = DurableServiceState::new(runtime);
        let files_before_replay = durable_file_snapshot(root.path());
        assert_eq!(reopened.handle(&start), first);
        assert_eq!(durable_file_snapshot(root.path()), files_before_replay);
        assert_eq!(
            reopened
                .ready_runtime()
                .expect("重开后 durable backend 保持 Ready")
                .session_snapshot(&session_id)
                .as_ref(),
            Some(created)
        );
    }

    #[test]
    fn durable_session_start_rejects_before_prepared_and_reserves_internal_namespace() {
        let root = durable_test_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("打开 durable runtime");
        let mut state = DurableServiceState::new(runtime);
        let files_before = durable_file_snapshot(root.path());
        let missing = command(
            "durable-session-missing-project",
            ClientCommand::SessionStart {
                project_id: ProjectId("project-missing".to_owned()),
            },
        );

        assert_protocol_error(&state.handle(&missing), ProtocolErrorCode::ProjectNotFound);
        let missing_digest =
            external_command_payload_digest(missing.protocol_version, &missing.command)
                .expect("计算缺失 Project 的 SessionStart 摘要");
        assert_eq!(
            state
                .ready_runtime()
                .expect("Prepared 前拒绝保持 Ready")
                .lookup_command(
                    &missing.client_id,
                    &missing.client_command_id,
                    &missing_digest,
                )
                .expect("查询拒绝后的 command index"),
            CommandLookup::Unseen
        );

        let internal = CommandEnvelope {
            protocol_version: PROTOCOL_VERSION,
            client_id: ClientId("__alda_internal_external-forgery".to_owned()),
            client_command_id: ClientCommandId("durable-session-internal".to_owned()),
            command: ClientCommand::SessionStart {
                project_id: ProjectId("project-missing".to_owned()),
            },
        };
        assert_protocol_error(&state.handle(&internal), ProtocolErrorCode::InvalidRequest);
        assert_eq!(durable_file_snapshot(root.path()), files_before);
        assert!(
            state
                .ready_runtime()
                .expect("Prepared 前拒绝不得发布 Session")
                .read_view()
                .sessions
                .is_empty()
        );
    }

    fn durable_chain_until_question(
        state: &mut DurableServiceState,
        command_prefix: &str,
        prompt: &str,
    ) -> (ProjectId, SessionId, TurnId, PendingQuestion) {
        let project = state.handle(&create_command(
            &format!("{command_prefix}-project"),
            "Etude",
        ));
        let CommandOutcome::Success {
            result: CommandResult::ProjectCreated(project),
        } = project.outcome
        else {
            panic!("预期 durable ProjectCreated reply");
        };
        let session = state.handle(&command(
            format!("{command_prefix}-session"),
            ClientCommand::SessionStart {
                project_id: project.project_id.clone(),
            },
        ));
        let CommandOutcome::Success {
            result: CommandResult::SessionStarted(session),
        } = session.outcome
        else {
            panic!("预期 durable SessionStarted reply");
        };
        let turn = state.handle(&command(
            format!("{command_prefix}-turn"),
            ClientCommand::TurnStart {
                session_id: session.session_id.clone(),
                prompt: prompt.to_owned(),
            },
        ));
        let CommandOutcome::Success {
            result: CommandResult::TurnStarted(turn),
        } = turn.outcome
        else {
            panic!("预期 durable TurnStarted reply");
        };
        let snapshot = state
            .ready_runtime()
            .expect("TurnStart 后 durable backend 保持 Ready")
            .session_snapshot(&session.session_id)
            .expect("TurnStart 发布 Session snapshot");
        let question = snapshot
            .questions
            .first()
            .expect("TurnStart 原子发布首个 Question")
            .clone();
        (
            project.project_id,
            session.session_id,
            turn.turn_id,
            question,
        )
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "同一向量闭合 TurnStart、pending Question cancel 与 command-only terminal 回复"
    )]
    fn durable_turn_mutation_preserves_order_exact_reply_and_terminal_authorization() {
        let root = durable_test_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("打开 durable runtime");
        let mut state = DurableServiceState::new(runtime);
        let (project_id, session_id, turn_id, question) =
            durable_chain_until_question(&mut state, "turn-chain", "  Write an etude  ");

        assert_typed_hex_id(&turn_id.0, "turn");
        assert_typed_hex_id(&question.question_id.0, "question");
        assert_eq!(question.owner_turn_id, turn_id);
        assert_eq!(question.created_sequence, 3);
        let snapshot = state
            .ready_runtime()
            .expect("TurnStart 后保持 Ready")
            .session_snapshot(&session_id)
            .expect("读取 TurnStart snapshot");
        assert_eq!(snapshot.covered_through_sequence, 3);
        assert_eq!(snapshot.turns[0].status, TurnStatus::WaitingForInput);
        assert_eq!(
            state
                .ready_runtime()
                .expect("TurnStart 后保持 Ready")
                .canonical_prompt(&session_id, &turn_id)
                .expect("读取 canonical prompt"),
            "Write an etude"
        );

        let second = state.handle(&command(
            "turn-chain-second-session",
            ClientCommand::SessionStart {
                project_id: project_id.clone(),
            },
        ));
        let CommandOutcome::Success {
            result: CommandResult::SessionStarted(second),
        } = second.outcome
        else {
            panic!("预期第二个 durable Session");
        };
        let mismatch = command(
            "turn-chain-owner-mismatch",
            ClientCommand::TurnCancel {
                session_id: second.session_id.clone(),
                turn_id: turn_id.clone(),
            },
        );
        assert_protocol_error(
            &state.handle(&mismatch),
            ProtocolErrorCode::TurnOwnershipMismatch,
        );
        assert_eq!(
            state
                .ready_runtime()
                .expect("ownership command-only 后保持 Ready")
                .session_snapshot(&session_id)
                .expect("读取 owner Session")
                .covered_through_sequence,
            3
        );

        let cancel = command(
            "turn-chain-cancel",
            ClientCommand::TurnCancel {
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
            },
        );
        let cancelled = state.handle(&cancel);
        assert!(matches!(
            cancelled.outcome,
            CommandOutcome::Success {
                result: CommandResult::TurnCancelled(ref turn)
            } if turn.status == TurnStatus::Cancelled && turn.terminal_sequence == Some(6)
        ));
        let files_after_cancel = durable_file_snapshot(root.path());
        assert_eq!(state.handle(&cancel), cancelled);
        assert_eq!(durable_file_snapshot(root.path()), files_after_cancel);

        let page = state.handle(&command(
            "turn-chain-events",
            ClientCommand::EventResume {
                cursor: StreamCursor {
                    stream_kind: StreamKind::SessionRollout,
                    stream_id: session_id.0.clone(),
                    epoch: SESSION_STREAM_EPOCH,
                    after_sequence: 3,
                },
            },
        ));
        assert!(matches!(
            page.outcome,
            CommandOutcome::Success {
                result: CommandResult::EventsResumed(ref page)
            } if matches!(
                page.events.as_slice(),
                [
                    SessionEvent { event: SessionEventKind::TurnCancelRequested { .. }, .. },
                    SessionEvent { event: SessionEventKind::QuestionOwnerTurnAborted { .. }, .. },
                    SessionEvent { event: SessionEventKind::TurnCompleted { status: TurnStatus::Cancelled, .. }, .. },
                ]
            )
        ));

        let terminal = command(
            "turn-chain-terminal",
            ClientCommand::TurnCancel {
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
            },
        );
        assert!(matches!(
            state.handle(&terminal).outcome,
            CommandOutcome::Success {
                result: CommandResult::TurnAlreadyTerminal {
                    terminal_status: TurnStatus::Cancelled,
                    terminal_sequence: 6,
                    ..
                }
            }
        ));
        assert_eq!(
            state
                .ready_runtime()
                .expect("terminal command-only 后保持 Ready")
                .session_snapshot(&session_id)
                .expect("读取 terminal Session")
                .covered_through_sequence,
            6
        );
    }

    #[test]
    fn durable_turn_mutation_cancels_pending_approval_in_order() {
        let root = durable_test_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("打开 durable runtime");
        let mut state = DurableServiceState::new(runtime);
        let (_project_id, session_id, turn_id, question) =
            durable_chain_until_question(&mut state, "turn-approval-cancel", "Approval cancel");
        let answered = state.handle(&command(
            "turn-approval-cancel-question",
            ClientCommand::QuestionRespond {
                session_id: session_id.clone(),
                question_id: question.question_id,
                choice_id: ChoiceId("bars_8".to_owned()),
            },
        ));
        assert!(matches!(
            answered.outcome,
            CommandOutcome::Success {
                result: CommandResult::QuestionAnswered(_)
            }
        ));

        let cancelled = state.handle(&command(
            "turn-approval-cancel-command",
            ClientCommand::TurnCancel {
                session_id: session_id.clone(),
                turn_id,
            },
        ));
        assert!(matches!(
            cancelled.outcome,
            CommandOutcome::Success {
                result: CommandResult::TurnCancelled(ref turn)
            } if turn.status == TurnStatus::Cancelled && turn.terminal_sequence == Some(8)
        ));
        let page = state.handle(&command(
            "turn-approval-cancel-events",
            ClientCommand::EventResume {
                cursor: StreamCursor {
                    stream_kind: StreamKind::SessionRollout,
                    stream_id: session_id.0,
                    epoch: SESSION_STREAM_EPOCH,
                    after_sequence: 5,
                },
            },
        ));
        assert!(matches!(
            page.outcome,
            CommandOutcome::Success {
                result: CommandResult::EventsResumed(ref page)
            } if matches!(
                page.events.as_slice(),
                [
                    SessionEvent { event: SessionEventKind::TurnCancelRequested { .. }, .. },
                    SessionEvent { event: SessionEventKind::ApprovalOwnerTurnAborted { .. }, .. },
                    SessionEvent { event: SessionEventKind::TurnCompleted { status: TurnStatus::Cancelled, .. }, .. },
                ]
            )
        ));
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "同一向量闭合 Question resolution、Approval subject、幂等与重启重建"
    )]
    fn durable_question_mutation_uses_canonical_subject_and_rebuilds_exact_reply() {
        let root = durable_test_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("打开 durable runtime");
        let mut state = DurableServiceState::new(runtime);
        let (project_id, session_id, turn_id, question) =
            durable_chain_until_question(&mut state, "question-chain", "  Canonical motif  ");
        let second = state.handle(&command(
            "question-chain-second-session",
            ClientCommand::SessionStart {
                project_id: project_id.clone(),
            },
        ));
        let CommandOutcome::Success {
            result: CommandResult::SessionStarted(second),
        } = second.outcome
        else {
            panic!("预期第二个 durable Session");
        };
        let mismatch = command(
            "question-chain-owner-mismatch",
            ClientCommand::QuestionRespond {
                session_id: second.session_id,
                question_id: question.question_id.clone(),
                choice_id: ChoiceId("bars_8".to_owned()),
            },
        );
        assert_protocol_error(
            &state.handle(&mismatch),
            ProtocolErrorCode::QuestionOwnershipMismatch,
        );
        assert_eq!(
            state
                .ready_runtime()
                .expect("Question ownership command-only 后保持 Ready")
                .session_snapshot(&session_id)
                .expect("读取 Question owner Session")
                .covered_through_sequence,
            3
        );
        let respond = command(
            "question-chain-respond",
            ClientCommand::QuestionRespond {
                session_id: session_id.clone(),
                question_id: question.question_id.clone(),
                choice_id: ChoiceId("bars_8".to_owned()),
            },
        );

        let answered = state.handle(&respond);
        assert!(matches!(
            answered.outcome,
            CommandOutcome::Success {
                result: CommandResult::QuestionAnswered(ref value)
            } if value.status == QuestionStatus::Answered
                && value.terminal_sequence == Some(4)
                && value.answer.as_ref().is_some_and(|answer| answer.choice_id.0 == "bars_8")
        ));
        let snapshot = state
            .ready_runtime()
            .expect("QuestionRespond 后保持 Ready")
            .session_snapshot(&session_id)
            .expect("读取 QuestionRespond snapshot");
        assert_eq!(snapshot.covered_through_sequence, 5);
        let approval = snapshot.approvals.first().expect("原子发布 Approval");
        assert_typed_hex_id(&approval.approval_id.0, "approval");
        assert_eq!(approval.created_sequence, 5);
        assert_eq!(approval.owner_turn_id, turn_id);
        assert_eq!(
            approval.approval_subject_digest,
            approval_subject_digest_for_test(
                "https://api.openai.com",
                &["constraints", "prompt"],
                &turn_id,
                "Canonical motif",
            )
        );

        let files_after_answer = durable_file_snapshot(root.path());
        assert_eq!(state.handle(&respond), answered);
        assert_eq!(durable_file_snapshot(root.path()), files_after_answer);
        let conflict = command(
            "question-chain-respond",
            ClientCommand::QuestionRespond {
                session_id: session_id.clone(),
                question_id: question.question_id.clone(),
                choice_id: ChoiceId("bars_16".to_owned()),
            },
        );
        assert_protocol_error(
            &state.handle(&conflict),
            ProtocolErrorCode::IdempotencyConflict,
        );

        let already = command(
            "question-chain-already",
            ClientCommand::QuestionRespond {
                session_id: session_id.clone(),
                question_id: question.question_id.clone(),
                choice_id: ChoiceId("bars_16".to_owned()),
            },
        );
        assert!(matches!(
            state.handle(&already).outcome,
            CommandOutcome::Success {
                result: CommandResult::QuestionAlreadyResolved(ref value)
            } if value.status == QuestionStatus::Answered && value.terminal_sequence == Some(4)
        ));
        assert_eq!(
            state
                .ready_runtime()
                .expect("Question command-only 后保持 Ready")
                .session_snapshot(&session_id)
                .expect("读取 Question command-only snapshot")
                .covered_through_sequence,
            5
        );

        drop(state);
        let runtime = ReadyDurableRuntime::open(root.path()).expect("重开 durable runtime");
        let mut reopened = DurableServiceState::new(runtime);
        let files_before_retry = durable_file_snapshot(root.path());
        assert_eq!(reopened.handle(&respond), answered);
        assert_eq!(durable_file_snapshot(root.path()), files_before_retry);
        assert_eq!(
            reopened
                .ready_runtime()
                .expect("重开后保持 Ready")
                .session_snapshot(&session_id)
                .expect("重建 QuestionRespond snapshot"),
            snapshot
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "同一向量闭合 deny Session facts、无 Artifact 边界、command-only 与重启重建"
    )]
    fn durable_approval_deny_writes_only_session_facts_and_rebuilds_exact_reply() {
        let root = durable_test_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("打开 durable runtime");
        let mut state = DurableServiceState::new(runtime);
        let (project_id, session_id, turn_id, question) =
            durable_chain_until_question(&mut state, "deny-chain", "Deny fixture");
        let answered = state.handle(&command(
            "deny-chain-question",
            ClientCommand::QuestionRespond {
                session_id: session_id.clone(),
                question_id: question.question_id,
                choice_id: ChoiceId("bars_8".to_owned()),
            },
        ));
        assert!(matches!(
            answered.outcome,
            CommandOutcome::Success {
                result: CommandResult::QuestionAnswered(_)
            }
        ));
        let before = state
            .ready_runtime()
            .expect("QuestionRespond 后保持 Ready")
            .session_snapshot(&session_id)
            .expect("读取 pending Approval");
        let approval = before.approvals[0].clone();
        let second = state.handle(&command(
            "deny-chain-second-session",
            ClientCommand::SessionStart {
                project_id: project_id.clone(),
            },
        ));
        let CommandOutcome::Success {
            result: CommandResult::SessionStarted(second),
        } = second.outcome
        else {
            panic!("预期第二个 durable Session");
        };
        let mismatch = command(
            "deny-chain-owner-mismatch",
            ClientCommand::ApprovalRespond {
                session_id: second.session_id,
                approval_id: approval.approval_id.clone(),
                approval_subject_digest: approval.approval_subject_digest.clone(),
                decision: ApprovalDecision::Deny,
            },
        );
        assert_protocol_error(
            &state.handle(&mismatch),
            ProtocolErrorCode::ApprovalOwnershipMismatch,
        );
        assert_eq!(
            state
                .ready_runtime()
                .expect("Approval ownership command-only 后保持 Ready")
                .session_snapshot(&session_id)
                .expect("读取 Approval owner Session")
                .covered_through_sequence,
            5
        );
        let project_head_before = state
            .ready_runtime()
            .expect("deny 前保持 Ready")
            .project_head(&project_id)
            .expect("读取 Project head");
        let deny = command(
            "deny-chain-approval",
            ClientCommand::ApprovalRespond {
                session_id: session_id.clone(),
                approval_id: approval.approval_id.clone(),
                approval_subject_digest: approval.approval_subject_digest.clone(),
                decision: ApprovalDecision::Deny,
            },
        );

        let denied = state.handle(&deny);
        assert!(matches!(
            denied.outcome,
            CommandOutcome::Success {
                result: CommandResult::ApprovalDecided {
                    approval: ref value,
                    artifact_manifest: None,
                }
            } if value.status == ApprovalStatus::Denied
                && value.terminal_sequence == Some(6)
                && value.decision == Some(ApprovalDecision::Deny)
        ));
        let snapshot = state
            .ready_runtime()
            .expect("deny 后保持 Ready")
            .session_snapshot(&session_id)
            .expect("读取 deny snapshot");
        assert_eq!(snapshot.covered_through_sequence, 7);
        assert_eq!(snapshot.turns[0].turn_id, turn_id);
        assert_eq!(snapshot.turns[0].status, TurnStatus::Failed);
        assert_eq!(snapshot.turns[0].terminal_sequence, Some(7));
        assert_eq!(
            state
                .ready_runtime()
                .expect("deny 后保持 Ready")
                .project_head(&project_id)
                .expect("读取 deny 后 Project head"),
            project_head_before
        );
        assert!(
            state
                .ready_runtime()
                .expect("deny 后保持 Ready")
                .project_projection(&project_id)
                .expect("读取 deny 后 Project projection")
                .artifacts
                .is_empty()
        );

        let files_after_deny = durable_file_snapshot(root.path());
        assert_eq!(state.handle(&deny), denied);
        assert_eq!(durable_file_snapshot(root.path()), files_after_deny);
        let already = command(
            "deny-chain-already",
            ClientCommand::ApprovalRespond {
                session_id: session_id.clone(),
                approval_id: approval.approval_id.clone(),
                approval_subject_digest: approval.approval_subject_digest.clone(),
                decision: ApprovalDecision::Deny,
            },
        );
        assert!(matches!(
            state.handle(&already).outcome,
            CommandOutcome::Success {
                result: CommandResult::ApprovalAlreadyResolved(ref value)
            } if value.status == ApprovalStatus::Denied && value.terminal_sequence == Some(6)
        ));
        assert_eq!(
            state
                .ready_runtime()
                .expect("Approval command-only 后保持 Ready")
                .session_snapshot(&session_id)
                .expect("读取 Approval command-only snapshot")
                .covered_through_sequence,
            7
        );

        drop(state);
        let runtime = ReadyDurableRuntime::open(root.path()).expect("重开 durable runtime");
        let mut reopened = DurableServiceState::new(runtime);
        let files_before_retry = durable_file_snapshot(root.path());
        assert_eq!(reopened.handle(&deny), denied);
        assert_eq!(durable_file_snapshot(root.path()), files_before_retry);
        assert_eq!(
            reopened
                .ready_runtime()
                .expect("重开 deny 后保持 Ready")
                .session_snapshot(&session_id)
                .expect("重建 deny snapshot"),
            snapshot
        );
    }

    fn durable_chain_until_approval(
        state: &mut DurableServiceState,
        command_prefix: &str,
    ) -> (ProjectId, SessionId, TurnId, PendingApproval) {
        let (project_id, session_id, turn_id, question) =
            durable_chain_until_question(state, command_prefix, "Durable approval fixture");
        let answered = state.handle(&command(
            format!("{command_prefix}-question"),
            ClientCommand::QuestionRespond {
                session_id: session_id.clone(),
                question_id: question.question_id,
                choice_id: ChoiceId("bars_8".to_owned()),
            },
        ));
        assert!(matches!(
            answered.outcome,
            CommandOutcome::Success {
                result: CommandResult::QuestionAnswered(_)
            }
        ));
        let approval = state
            .ready_runtime()
            .expect("QuestionRespond 后保持 Ready")
            .session_snapshot(&session_id)
            .expect("读取 pending Approval")
            .approvals[0]
            .clone();
        (project_id, session_id, turn_id, approval)
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "同一向量闭合 approve 双 aggregate、exact reply、digest conflict 与重启重建"
    )]
    fn durable_approval_artifact_commits_both_aggregates_and_exact_reply() {
        let root = durable_test_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("打开 durable runtime");
        let mut state = DurableServiceState::new(runtime);
        let (project_id, session_id, turn_id, approval) =
            durable_chain_until_approval(&mut state, "approve-chain");
        let project_head_before = state
            .ready_runtime()
            .expect("approve 前保持 Ready")
            .project_head(&project_id)
            .expect("读取 Project head");
        let approve = command(
            "approve-chain-command",
            ClientCommand::ApprovalRespond {
                session_id: session_id.clone(),
                approval_id: approval.approval_id.clone(),
                approval_subject_digest: approval.approval_subject_digest.clone(),
                decision: ApprovalDecision::Approve,
            },
        );

        let first = state.handle(&approve);
        let CommandOutcome::Success {
            result:
                CommandResult::ApprovalDecided {
                    approval: decided,
                    artifact_manifest: Some(manifest),
                },
        } = &first.outcome
        else {
            panic!("预期 durable Approval approve reply");
        };
        assert_eq!(decided.status, ApprovalStatus::Approved);
        assert_eq!(decided.decision, Some(ApprovalDecision::Approve));
        assert_eq!(decided.terminal_sequence, Some(6));
        assert_eq!(manifest.project_id, project_id);
        assert_eq!(manifest.source_session_id, session_id);
        assert_eq!(manifest.source_turn_id, turn_id);
        assert_eq!(manifest.created_sequence, 6);
        assert_eq!(manifest.durability, ArtifactDurability::DurableLocal);
        assert_typed_hex_id(&manifest.artifact_occurrence_id.0, "occurrence");
        assert_eq!(manifest.artifact_hash.as_str(), FIXTURE_HASH);

        let runtime = state.ready_runtime().expect("approve 后保持 Ready");
        let session = runtime
            .session_snapshot(&session_id)
            .expect("approve 原子发布 Session");
        assert_eq!(session.covered_through_sequence, 7);
        assert_eq!(session.approvals[0], *decided);
        assert_eq!(session.turns[0].status, TurnStatus::Succeeded);
        assert_eq!(session.turns[0].terminal_sequence, Some(7));
        let project_head_after = runtime
            .project_head(&project_id)
            .expect("读取 Project head");
        assert_eq!(
            project_head_after.last_sequence,
            project_head_before.last_sequence + 1
        );
        let project = runtime
            .project_projection(&project_id)
            .expect("approve 原子发布 Project");
        assert!(project.revisions.is_empty());
        assert_eq!(project.artifacts.len(), 1);
        assert!(
            runtime
                .occurrence(&project_id, &manifest.artifact_occurrence_id)
                .is_some_and(|stored| stored == manifest)
        );

        let files_after_commit = durable_file_snapshot(root.path());
        assert_eq!(state.handle(&approve), first);
        assert_eq!(durable_file_snapshot(root.path()), files_after_commit);
        let conflict = command(
            "approve-chain-command",
            ClientCommand::ApprovalRespond {
                session_id: session_id.clone(),
                approval_id: approval.approval_id,
                approval_subject_digest: approval.approval_subject_digest,
                decision: ApprovalDecision::Deny,
            },
        );
        assert_protocol_error(
            &state.handle(&conflict),
            ProtocolErrorCode::IdempotencyConflict,
        );
        assert_eq!(durable_file_snapshot(root.path()), files_after_commit);

        let expected_session = session;
        let expected_manifest = manifest.clone();
        drop(state);
        let runtime = ReadyDurableRuntime::open(root.path()).expect("重开 durable runtime");
        let mut reopened = DurableServiceState::new(runtime);
        let files_before_retry = durable_file_snapshot(root.path());
        assert_eq!(reopened.handle(&approve), first);
        assert_eq!(durable_file_snapshot(root.path()), files_before_retry);
        let runtime = reopened.ready_runtime().expect("重开后保持 Ready");
        assert_eq!(
            runtime
                .session_snapshot(&session_id)
                .expect("重建 approve Session"),
            expected_session
        );
        assert_eq!(
            runtime.occurrence(&project_id, &expected_manifest.artifact_occurrence_id),
            Some(&expected_manifest)
        );
    }

    #[test]
    fn durable_artifact_query_download_enforces_committed_project_reachability() {
        let root = durable_test_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("打开 durable runtime");
        let mut state = DurableServiceState::new(runtime);
        let (project_id, session_id, _turn_id, approval) =
            durable_chain_until_approval(&mut state, "artifact-query");
        let approve = command(
            "artifact-query-approve",
            ClientCommand::ApprovalRespond {
                session_id,
                approval_id: approval.approval_id,
                approval_subject_digest: approval.approval_subject_digest,
                decision: ApprovalDecision::Approve,
            },
        );
        let approved = state.handle(&approve);
        let CommandOutcome::Success {
            result:
                CommandResult::ApprovalDecided {
                    artifact_manifest: Some(manifest),
                    ..
                },
        } = approved.outcome
        else {
            panic!("预期 durable Artifact manifest");
        };

        let query = state.handle(&command(
            "artifact-query-manifest",
            ClientCommand::ArtifactManifest {
                project_id: project_id.clone(),
                artifact_occurrence_id: manifest.artifact_occurrence_id.clone(),
            },
        ));
        assert!(matches!(
            query.outcome,
            CommandOutcome::Success {
                result: CommandResult::ArtifactManifest(ref stored)
            } if stored == &manifest
        ));
        for (id, candidate_project, candidate_occurrence) in [
            (
                "artifact-query-wrong-project",
                ProjectId("project-missing".to_owned()),
                manifest.artifact_occurrence_id.clone(),
            ),
            (
                "artifact-query-missing-occurrence",
                project_id.clone(),
                ArtifactOccurrenceId("occurrence-missing".to_owned()),
            ),
        ] {
            assert_protocol_error(
                &state.handle(&command(
                    id,
                    ClientCommand::ArtifactManifest {
                        project_id: candidate_project,
                        artifact_occurrence_id: candidate_occurrence,
                    },
                )),
                ProtocolErrorCode::ArtifactNotFound,
            );
        }

        let verified = state.resolve_artifact_download(&project_id, &manifest.artifact_hash, None);
        let DownloadResolution::Verified(download) = verified else {
            panic!("预期 same-handle verified download");
        };
        assert_eq!(&*download.bytes, FIXTURE_BYTES);
        assert_eq!(download.size_bytes, FIXTURE_SIZE);
        assert_eq!(download.mime_type, FIXTURE_MIME);
        assert_eq!(
            state.resolve_artifact_download(
                &project_id,
                &manifest.artifact_hash,
                Some(&format!("\"{}\"", manifest.artifact_hash.as_str())),
            ),
            DownloadResolution::NotModified(manifest.artifact_hash.clone())
        );
        let unreachable = ArtifactHash::parse(
            "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        )
        .expect("有效的不可达 hash");
        assert_eq!(
            state.resolve_artifact_download(&project_id, &unreachable, None),
            DownloadResolution::NotFound
        );
        assert_eq!(
            state.resolve_artifact_download(
                &ProjectId("project-missing".to_owned()),
                &manifest.artifact_hash,
                None,
            ),
            DownloadResolution::NotFound
        );
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum BackendSemanticsScenario {
        Happy,
        Deny,
        QuestionCancel,
        ApprovalCancel,
    }

    impl BackendSemanticsScenario {
        const fn label(self) -> &'static str {
            match self {
                Self::Happy => "happy",
                Self::Deny => "deny",
                Self::QuestionCancel => "question-cancel",
                Self::ApprovalCancel => "approval-cancel",
            }
        }
    }

    enum SemanticsBackend {
        Memory(Box<ServiceState>),
        Durable(DurableServiceState),
    }

    impl SemanticsBackend {
        fn handle(&mut self, envelope: &CommandEnvelope) -> CommandReply {
            match self {
                Self::Memory(state) => state.handle(envelope.clone()),
                Self::Durable(state) => state.handle(envelope),
            }
        }

        fn download(&self, project_id: &ProjectId, hash: &ArtifactHash) -> DownloadResolution {
            match self {
                Self::Memory(state) => state.resolve_download(project_id, hash, None),
                Self::Durable(state) => state.resolve_artifact_download(project_id, hash, None),
            }
        }
    }

    #[derive(Debug, Eq, PartialEq)]
    struct ArtifactWireObservation {
        hash: String,
        kind: ArtifactKind,
        mime_type: String,
        size_bytes: u64,
        producer: ArtifactProducer,
        fixture_version: u32,
        created_sequence: u64,
        provenance_label: String,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct DownloadWireObservation {
        hash: String,
        mime_type: String,
        size_bytes: u64,
        bytes: Vec<u8>,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct BackendWireObservation {
        project_name: String,
        project_version: u64,
        covered_through_sequence: u64,
        turn_status: TurnStatus,
        turn_terminal_sequence: Option<u64>,
        question_status: QuestionStatus,
        question_terminal_sequence: Option<u64>,
        approval_status: Option<ApprovalStatus>,
        approval_terminal_sequence: Option<u64>,
        event_shape: Vec<(u64, &'static str)>,
        artifact: Option<ArtifactWireObservation>,
        download: Option<DownloadWireObservation>,
    }

    struct BackendConformanceResult {
        wire: BackendWireObservation,
        artifact_durability: Option<ArtifactDurability>,
    }

    fn successful_project(reply: CommandReply) -> ProjectSnapshot {
        let CommandOutcome::Success {
            result: CommandResult::ProjectCreated(project),
        } = reply.outcome
        else {
            panic!("语义向量预期 ProjectCreated");
        };
        project
    }

    fn successful_session(reply: CommandReply) -> SessionSnapshot {
        let CommandOutcome::Success {
            result: CommandResult::SessionStarted(session),
        } = reply.outcome
        else {
            panic!("语义向量预期 SessionStarted");
        };
        session
    }

    fn queried_session(reply: CommandReply) -> SessionSnapshot {
        let CommandOutcome::Success {
            result: CommandResult::SessionSnapshot(session),
        } = reply.outcome
        else {
            panic!("语义向量预期 SessionSnapshot");
        };
        session
    }

    const fn event_shape_name(event: &SessionEventKind) -> &'static str {
        match event {
            SessionEventKind::SessionStarted { .. } => "session_started",
            SessionEventKind::TurnStarted { .. } => "turn_started",
            SessionEventKind::TurnCancelRequested { .. } => "turn_cancel_requested",
            SessionEventKind::TurnCompleted { .. } => "turn_completed",
            SessionEventKind::QuestionRequested { .. } => "question_requested",
            SessionEventKind::QuestionResolved { .. } => "question_resolved",
            SessionEventKind::ApprovalRequested { .. } => "approval_requested",
            SessionEventKind::ApprovalResolved { .. } => "approval_resolved",
            SessionEventKind::QuestionOwnerTurnAborted { .. } => "question_owner_turn_aborted",
            SessionEventKind::ApprovalOwnerTurnAborted { .. } => "approval_owner_turn_aborted",
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "同一 table-driven 向量必须连续捕获两种 backend 的 wire 可观察结果"
    )]
    fn run_backend_semantics_vector(
        backend: &mut SemanticsBackend,
        scenario: BackendSemanticsScenario,
    ) -> BackendConformanceResult {
        let label = scenario.label();
        let create = command(
            format!("conformance-{label}-project"),
            ClientCommand::ProjectCreate {
                name: "Etude".to_owned(),
            },
        );
        let created_reply = backend.handle(&create);
        assert_eq!(
            backend.handle(&create),
            created_reply,
            "{label}: exact reply"
        );
        let project = successful_project(created_reply);

        let start = command(
            format!("conformance-{label}-session"),
            ClientCommand::SessionStart {
                project_id: project.project_id.clone(),
            },
        );
        let session = successful_session(backend.handle(&start));
        let turn = command(
            format!("conformance-{label}-turn"),
            ClientCommand::TurnStart {
                session_id: session.session_id.clone(),
                prompt: "Conformance motif".to_owned(),
            },
        );
        let turn_reply = backend.handle(&turn);
        let CommandOutcome::Success {
            result: CommandResult::TurnStarted(turn),
        } = turn_reply.outcome
        else {
            panic!("{label}: 语义向量预期 TurnStarted");
        };
        let pending = queried_session(backend.handle(&command(
            format!("conformance-{label}-pending"),
            ClientCommand::SessionSnapshot {
                session_id: session.session_id.clone(),
            },
        )));
        let question = pending.questions[0].clone();

        let mut approval = None;
        if scenario != BackendSemanticsScenario::QuestionCancel {
            let answered = backend.handle(&command(
                format!("conformance-{label}-question"),
                ClientCommand::QuestionRespond {
                    session_id: session.session_id.clone(),
                    question_id: question.question_id.clone(),
                    choice_id: ChoiceId("bars_8".to_owned()),
                },
            ));
            assert!(matches!(
                answered.outcome,
                CommandOutcome::Success {
                    result: CommandResult::QuestionAnswered(_)
                }
            ));
            approval = queried_session(backend.handle(&command(
                format!("conformance-{label}-approval-pending"),
                ClientCommand::SessionSnapshot {
                    session_id: session.session_id.clone(),
                },
            )))
            .approvals
            .into_iter()
            .next();
        }

        let terminal_id = format!("conformance-{label}-terminal");
        let terminal = match scenario {
            BackendSemanticsScenario::QuestionCancel | BackendSemanticsScenario::ApprovalCancel => {
                command(
                    terminal_id.clone(),
                    ClientCommand::TurnCancel {
                        session_id: session.session_id.clone(),
                        turn_id: turn.turn_id.clone(),
                    },
                )
            }
            BackendSemanticsScenario::Happy | BackendSemanticsScenario::Deny => {
                let approval = approval.as_ref().expect("语义向量已建立 pending Approval");
                command(
                    terminal_id.clone(),
                    ClientCommand::ApprovalRespond {
                        session_id: session.session_id.clone(),
                        approval_id: approval.approval_id.clone(),
                        approval_subject_digest: approval.approval_subject_digest.clone(),
                        decision: if scenario == BackendSemanticsScenario::Happy {
                            ApprovalDecision::Approve
                        } else {
                            ApprovalDecision::Deny
                        },
                    },
                )
            }
        };
        let terminal_reply = backend.handle(&terminal);
        assert_eq!(
            backend.handle(&terminal),
            terminal_reply,
            "{label}: terminal exact reply"
        );

        let conflicting = match &terminal.command {
            ClientCommand::TurnCancel { turn_id, .. } => command(
                terminal_id,
                ClientCommand::TurnCancel {
                    session_id: SessionId("session-different".to_owned()),
                    turn_id: turn_id.clone(),
                },
            ),
            ClientCommand::ApprovalRespond {
                session_id,
                approval_id,
                approval_subject_digest,
                decision,
            } => command(
                terminal_id,
                ClientCommand::ApprovalRespond {
                    session_id: session_id.clone(),
                    approval_id: approval_id.clone(),
                    approval_subject_digest: approval_subject_digest.clone(),
                    decision: if *decision == ApprovalDecision::Approve {
                        ApprovalDecision::Deny
                    } else {
                        ApprovalDecision::Approve
                    },
                },
            ),
            _ => panic!("语义向量 terminal command 必须是 cancel 或 approval"),
        };
        assert_protocol_error(
            &backend.handle(&conflicting),
            ProtocolErrorCode::IdempotencyConflict,
        );

        let final_snapshot = queried_session(backend.handle(&command(
            format!("conformance-{label}-final"),
            ClientCommand::SessionSnapshot {
                session_id: session.session_id.clone(),
            },
        )));
        let events = backend.handle(&command(
            format!("conformance-{label}-events"),
            ClientCommand::EventResume {
                cursor: StreamCursor {
                    stream_kind: StreamKind::SessionRollout,
                    stream_id: session.session_id.0.clone(),
                    epoch: SESSION_STREAM_EPOCH,
                    after_sequence: 0,
                },
            },
        ));
        let CommandOutcome::Success {
            result: CommandResult::EventsResumed(events),
        } = events.outcome
        else {
            panic!("{label}: 语义向量预期 EventsResumed");
        };

        let (artifact, artifact_durability, download) = match terminal_reply.outcome {
            CommandOutcome::Success {
                result:
                    CommandResult::ApprovalDecided {
                        artifact_manifest: Some(manifest),
                        ..
                    },
            } => {
                let queried = backend.handle(&command(
                    format!("conformance-{label}-manifest"),
                    ClientCommand::ArtifactManifest {
                        project_id: project.project_id.clone(),
                        artifact_occurrence_id: manifest.artifact_occurrence_id.clone(),
                    },
                ));
                assert!(matches!(
                    queried.outcome,
                    CommandOutcome::Success {
                        result: CommandResult::ArtifactManifest(ref value)
                    } if value == &manifest
                ));
                let DownloadResolution::Verified(download) =
                    backend.download(&project.project_id, &manifest.artifact_hash)
                else {
                    panic!("{label}: 语义向量预期 verified download");
                };
                (
                    Some(ArtifactWireObservation {
                        hash: manifest.artifact_hash.as_str().to_owned(),
                        kind: manifest.kind,
                        mime_type: manifest.mime_type.clone(),
                        size_bytes: manifest.size_bytes,
                        producer: manifest.producer,
                        fixture_version: manifest.fixture_version,
                        created_sequence: manifest.created_sequence,
                        provenance_label: manifest.provenance_label.clone(),
                    }),
                    Some(manifest.durability),
                    Some(DownloadWireObservation {
                        hash: download.artifact_hash.as_str().to_owned(),
                        mime_type: download.mime_type,
                        size_bytes: download.size_bytes,
                        bytes: download.bytes.to_vec(),
                    }),
                )
            }
            _ => (None, None, None),
        };

        let project_snapshot = backend.handle(&command(
            format!("conformance-{label}-project-snapshot"),
            ClientCommand::ProjectSnapshot {
                project_id: project.project_id,
            },
        ));
        let CommandOutcome::Success {
            result: CommandResult::ProjectSnapshot(project),
        } = project_snapshot.outcome
        else {
            panic!("{label}: 语义向量预期 ProjectSnapshot");
        };
        let turn = &final_snapshot.turns[0];
        let question = &final_snapshot.questions[0];
        let final_approval = final_snapshot.approvals.first();
        BackendConformanceResult {
            wire: BackendWireObservation {
                project_name: project.name,
                project_version: project.version,
                covered_through_sequence: final_snapshot.covered_through_sequence,
                turn_status: turn.status,
                turn_terminal_sequence: turn.terminal_sequence,
                question_status: question.status,
                question_terminal_sequence: question.terminal_sequence,
                approval_status: final_approval.map(|value| value.status),
                approval_terminal_sequence: final_approval
                    .and_then(|value| value.terminal_sequence),
                event_shape: events
                    .events
                    .iter()
                    .map(|event| (event.sequence, event_shape_name(&event.event)))
                    .collect(),
                artifact,
                download,
            },
            artifact_durability,
        }
    }

    #[test]
    fn durable_backend_semantics_table_matches_memory_wire_observations() {
        for scenario in [
            BackendSemanticsScenario::Happy,
            BackendSemanticsScenario::Deny,
            BackendSemanticsScenario::QuestionCancel,
            BackendSemanticsScenario::ApprovalCancel,
        ] {
            let mut memory = SemanticsBackend::Memory(Box::default());
            let memory_result = run_backend_semantics_vector(&mut memory, scenario);

            let root = durable_test_root();
            let runtime = ReadyDurableRuntime::open(root.path()).expect("打开 durable runtime");
            let mut durable = SemanticsBackend::Durable(DurableServiceState::new(runtime));
            let durable_result = run_backend_semantics_vector(&mut durable, scenario);

            assert_eq!(
                durable_result.wire,
                memory_result.wire,
                "{}: 两种 backend 的 wire 语义必须一致",
                scenario.label()
            );
            if scenario == BackendSemanticsScenario::Happy {
                assert_eq!(
                    memory_result.artifact_durability,
                    Some(ArtifactDurability::ProcessLifetimeFixture)
                );
                assert_eq!(
                    durable_result.artifact_durability,
                    Some(ArtifactDurability::DurableLocal)
                );
            } else {
                assert_eq!(memory_result.artifact_durability, None);
                assert_eq!(durable_result.artifact_durability, None);
            }
        }
    }

    #[test]
    fn durable_backend_fault_service_state_respects_ready_recovering_and_fatal_boundaries() {
        use crate::durable_runtime::RuntimeFailpoint;

        let rejected_root = durable_test_root();
        let runtime = ReadyDurableRuntime::open(rejected_root.path()).expect("打开 Ready runtime");
        let mut rejected = DurableServiceState::new(runtime);
        let before = durable_file_snapshot(rejected_root.path());
        rejected.set_runtime_failpoint_for_test(RuntimeFailpoint::Prepare);
        let create = create_command("fault-rejected-project", "Etude");
        assert_protocol_error(
            &rejected.handle(&create),
            ProtocolErrorCode::ServiceUnavailable,
        );
        assert_eq!(durable_file_snapshot(rejected_root.path()), before);
        assert!(rejected.ready_runtime().is_some());
        assert!(matches!(
            rejected
                .handle(&command("fault-rejected-query", ClientCommand::Initialize))
                .outcome,
            CommandOutcome::Success { .. }
        ));

        let recovering_root = durable_test_root();
        let runtime =
            ReadyDurableRuntime::open(recovering_root.path()).expect("打开 Recovering runtime");
        let mut recovering = DurableServiceState::new(runtime);
        let project = successful_project(
            recovering.handle(&create_command("fault-recovering-project", "Etude")),
        );
        let start = command(
            "fault-recovering-session",
            ClientCommand::SessionStart {
                project_id: project.project_id,
            },
        );
        recovering.set_runtime_failpoint_for_test(RuntimeFailpoint::Session);
        let recovered_reply = recovering.handle(&start);
        assert!(matches!(
            recovered_reply.outcome,
            CommandOutcome::Success {
                result: CommandResult::SessionStarted(_)
            }
        ));
        assert_eq!(recovering.handle(&start), recovered_reply);
        assert!(recovering.ready_runtime().is_some());

        let fatal_root = durable_test_root();
        let runtime = ReadyDurableRuntime::open(fatal_root.path()).expect("打开 Fatal runtime");
        let mut fatal = DurableServiceState::new(runtime);
        let project =
            successful_project(fatal.handle(&create_command("fault-fatal-project", "Etude")));
        let start = command(
            "fault-fatal-session",
            ClientCommand::SessionStart {
                project_id: project.project_id.clone(),
            },
        );
        fatal.set_runtime_failpoint_for_test(RuntimeFailpoint::CommitRecoverySync);
        assert_protocol_error(&fatal.handle(&start), ProtocolErrorCode::ServiceUnavailable);
        assert!(fatal.ready_runtime().is_none());
        assert_protocol_error(
            &fatal.handle(&command("fault-fatal-query", ClientCommand::Initialize)),
            ProtocolErrorCode::ServiceUnavailable,
        );
        let hash = ArtifactHash::parse(FIXTURE_HASH).expect("固定 Artifact hash");
        assert_eq!(
            fatal.resolve_artifact_download(&project.project_id, &hash, None),
            DownloadResolution::Corrupt
        );
        drop(fatal);

        let runtime = ReadyDurableRuntime::open(fatal_root.path())
            .expect("新进程 ordinary open 收敛同一 Prepared");
        let mut reopened = DurableServiceState::new(runtime);
        assert!(matches!(
            reopened.handle(&start).outcome,
            CommandOutcome::Success {
                result: CommandResult::SessionStarted(_)
            }
        ));
    }

    fn create_command(id: &str, name: &str) -> CommandEnvelope {
        CommandEnvelope {
            protocol_version: PROTOCOL_VERSION,
            client_id: ClientId("test-client".to_owned()),
            client_command_id: ClientCommandId(id.to_owned()),
            command: ClientCommand::ProjectCreate {
                name: name.to_owned(),
            },
        }
    }

    #[tokio::test]
    async fn create_is_idempotent_for_the_same_client_and_command_id() {
        let service = AppService::spawn(QueueCapacity::new(8).expect("valid capacity"));
        let command = create_command("create-1", "Etude");

        let first = service.execute(command.clone()).await.expect("first reply");
        let second = service.execute(command).await.expect("second reply");

        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn b1_read_dtos_are_mapped_without_exposing_domain_events_or_capabilities() {
        let service = AppService::spawn(QueueCapacity::new(8).expect("valid capacity"));
        service
            .execute(create_command("create-b1", "Etude"))
            .await
            .expect("create project");

        let snapshot_reply = service
            .execute(command(
                "domain-snapshot",
                ClientCommand::ProjectDomainSnapshot {
                    project_id: ProjectId("project-1".to_owned()),
                },
            ))
            .await
            .expect("domain snapshot");
        let CommandOutcome::Success {
            result: CommandResult::ProjectDomainSnapshot(snapshot),
        } = snapshot_reply.outcome
        else {
            panic!("expected domain snapshot");
        };
        assert_eq!(snapshot.schema_version, 1);
        assert_eq!(snapshot.score_id, "score-1");
        assert_eq!(snapshot.takes[0].take_id, "take-1");
        assert_eq!(snapshot.branches[0].branch_id, "branch-1");
        assert!(snapshot.revisions.is_empty());
        let json = serde_json::to_string(&snapshot).expect("wire snapshot");
        assert!(!json.contains("store_commit_identity"));
        assert!(!json.contains("project_initialized"));
        assert!(!json.contains("fixture_only"));

        let list = service
            .execute(command(
                "revision-list",
                ClientCommand::RevisionList {
                    project_id: ProjectId("project-1".to_owned()),
                },
            ))
            .await
            .expect("revision list");
        assert!(matches!(
            list.outcome,
            CommandOutcome::Success {
                result: CommandResult::RevisionList(ref revisions)
            } if revisions.is_empty()
        ));
    }

    #[tokio::test]
    async fn reusing_a_command_id_with_another_payload_is_rejected() {
        let service = AppService::spawn(QueueCapacity::new(8).expect("valid capacity"));
        service
            .execute(create_command("create-1", "Etude"))
            .await
            .expect("first reply");

        let reply = service
            .execute(create_command("create-1", "Nocturne"))
            .await
            .expect("conflict reply");

        assert!(matches!(
            reply.outcome,
            CommandOutcome::Error {
                error: crate::protocol::ProtocolError {
                    code: ProtocolErrorCode::IdempotencyConflict,
                    ..
                }
            }
        ));
    }

    #[tokio::test]
    async fn bounded_queue_reports_overload_before_the_runner_drains_it() {
        let (service, _runner) = AppService::build(QueueCapacity::new(1).expect("valid capacity"));
        let _first = service
            .enqueue(create_command("create-1", "Etude"))
            .expect("first command fits");

        let second = service.enqueue(create_command("create-2", "Nocturne"));

        assert!(matches!(second, Err(SubmitError::Overloaded)));
    }

    #[tokio::test]
    async fn command_and_query_queues_have_independent_capacity_one_contracts() {
        let (service, _runner) = AppService::build_with_capacities(
            QueueCapacity::new(1).expect("command capacity"),
            QueryQueueCapacity::new(1).expect("query capacity"),
        );
        let _command = service
            .enqueue(create_command("queued-command", "Etude"))
            .expect("first command fits");
        assert!(matches!(
            service.enqueue(create_command("overloaded-command", "Nocturne")),
            Err(SubmitError::Overloaded)
        ));

        let (reply_tx, _reply_rx) = oneshot::channel();
        service
            .query_tx
            .try_send(QueryMessage::ResolveSessionEvents {
                cursor: crate::protocol::StreamCursor {
                    stream_kind: StreamKind::SessionRollout,
                    stream_id: "session-1".to_owned(),
                    epoch: 1,
                    after_sequence: 0,
                },
                reply_tx,
            })
            .expect("first query fits");
        let (reply_tx, _reply_rx) = oneshot::channel();
        assert!(matches!(
            service
                .query_tx
                .try_send(QueryMessage::ResolveSessionEvents {
                    cursor: crate::protocol::StreamCursor {
                        stream_kind: StreamKind::SessionRollout,
                        stream_id: "session-1".to_owned(),
                        epoch: 1,
                        after_sequence: 0,
                    },
                    reply_tx,
                }),
            Err(mpsc::error::TrySendError::Full(_))
        ));
    }

    #[tokio::test]
    async fn weighted_runner_makes_progress_for_commands_and_queries() {
        let (service, runner) = AppService::build_with_capacities(
            QueueCapacity::new(16).expect("command capacity"),
            QueryQueueCapacity::new(2).expect("query capacity"),
        );
        let mut commands = Vec::new();
        for number in 0..9 {
            commands.push(
                service
                    .enqueue(create_command(&format!("fair-{number}"), "Etude"))
                    .expect("command fits"),
            );
        }
        let stats = {
            let service = service.clone();
            tokio::spawn(async move { service.artifact_stats_for_test().await })
        };
        tokio::spawn(runner.run());
        for command in commands {
            command.wait().await.expect("command progress");
        }
        assert_eq!(
            stats.await.expect("query task"),
            ArtifactStats {
                blobs: 0,
                occurrences: 0,
                reachability: 0,
            }
        );
    }

    fn command(id: impl Into<String>, command: ClientCommand) -> CommandEnvelope {
        CommandEnvelope {
            protocol_version: PROTOCOL_VERSION,
            client_id: ClientId("test-client".to_owned()),
            client_command_id: ClientCommandId(id.into()),
            command,
        }
    }

    async fn setup_session(service: &AppService) -> SessionId {
        service
            .execute(create_command("create-project", "Etude"))
            .await
            .expect("create project");
        let reply = service
            .execute(command(
                "start-session",
                ClientCommand::SessionStart {
                    project_id: ProjectId("project-1".to_owned()),
                },
            ))
            .await
            .expect("start session");
        let CommandOutcome::Success {
            result: CommandResult::SessionStarted(snapshot),
        } = reply.outcome
        else {
            panic!("expected session started");
        };
        snapshot.session_id
    }

    fn resumed_page(reply: CommandReply) -> EventPage {
        let CommandOutcome::Success {
            result: CommandResult::EventsResumed(page),
        } = reply.outcome
        else {
            panic!("expected resumed events");
        };
        page
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn lifecycle_events_snapshot_and_cancel_are_consistent() {
        let service = AppService::spawn(QueueCapacity::new(16).expect("valid capacity"));
        let session_id = setup_session(&service).await;
        let started = service
            .execute(command(
                "start-turn",
                ClientCommand::TurnStart {
                    session_id: session_id.clone(),
                    prompt: "Write an etude".to_owned(),
                },
            ))
            .await
            .expect("start turn");
        let CommandOutcome::Success {
            result: CommandResult::TurnStarted(turn),
        } = started.outcome
        else {
            panic!("expected turn started");
        };
        let cancel = command(
            "cancel-turn",
            ClientCommand::TurnCancel {
                session_id: session_id.clone(),
                turn_id: turn.turn_id.clone(),
            },
        );
        let first_cancel = service.execute(cancel.clone()).await.expect("cancel turn");
        let retry_cancel = service.execute(cancel).await.expect("retry cancel");
        assert_eq!(first_cancel, retry_cancel);

        let page = resumed_page(
            service
                .execute(command(
                    "resume",
                    ClientCommand::EventResume {
                        cursor: StreamCursor {
                            stream_kind: StreamKind::SessionRollout,
                            stream_id: session_id.0.clone(),
                            epoch: SESSION_STREAM_EPOCH,
                            after_sequence: 0,
                        },
                    },
                ))
                .await
                .expect("resume"),
        );
        assert_eq!(
            page.events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 6]
        );
        assert!(matches!(
            page.events[5].event,
            SessionEventKind::TurnCompleted {
                status: TurnStatus::Cancelled,
                ..
            }
        ));

        let snapshot = service
            .execute(command(
                "snapshot",
                ClientCommand::SessionSnapshot {
                    session_id: session_id.clone(),
                },
            ))
            .await
            .expect("snapshot");
        let CommandOutcome::Success {
            result: CommandResult::SessionSnapshot(snapshot),
        } = snapshot.outcome
        else {
            panic!("expected snapshot");
        };
        assert_eq!(snapshot.covered_through_sequence, page.head_sequence);
        assert_eq!(snapshot.stream_epoch, SESSION_STREAM_EPOCH);
        assert_eq!(snapshot.turns[0].status, TurnStatus::Cancelled);
        assert_eq!(snapshot.turns[0].terminal_sequence, Some(6));

        let repeated = service
            .execute(command(
                "cancel-again",
                ClientCommand::TurnCancel {
                    session_id,
                    turn_id: turn.turn_id,
                },
            ))
            .await
            .expect("business duplicate");
        assert_eq!(
            repeated.client_command_id,
            ClientCommandId("cancel-again".to_owned())
        );
        assert!(matches!(
            repeated.outcome,
            CommandOutcome::Success {
                result: CommandResult::TurnAlreadyTerminal {
                    terminal_status: TurnStatus::Cancelled,
                    terminal_sequence: 6,
                    ..
                }
            }
        ));
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn cursor_truth_table_and_session_isolation_are_typed() {
        let service = AppService::spawn(QueueCapacity::new(32).expect("valid capacity"));
        let first = setup_session(&service).await;
        let second_reply = service
            .execute(command(
                "second-session",
                ClientCommand::SessionStart {
                    project_id: ProjectId("project-1".to_owned()),
                },
            ))
            .await
            .expect("second session");
        let CommandOutcome::Success {
            result: CommandResult::SessionStarted(second),
        } = second_reply.outcome
        else {
            panic!("expected second session");
        };

        for (id, after, expected_count) in [("zero", 0, 1), ("head", 1, 0)] {
            let page = resumed_page(
                service
                    .execute(command(
                        id,
                        ClientCommand::EventResume {
                            cursor: StreamCursor {
                                stream_kind: StreamKind::SessionRollout,
                                stream_id: first.0.clone(),
                                epoch: SESSION_STREAM_EPOCH,
                                after_sequence: after,
                            },
                        },
                    ))
                    .await
                    .expect("valid cursor"),
            );
            assert_eq!(page.events.len(), expected_count);
            assert_eq!(page.next_after_sequence, 1);
        }

        let cases = [
            (
                "future",
                StreamKind::SessionRollout,
                first.0.clone(),
                SESSION_STREAM_EPOCH,
                2,
                ProtocolErrorCode::InvalidCursor,
                RecoveryAction::FetchSessionSnapshot(first.clone()),
            ),
            (
                "epoch",
                StreamKind::SessionRollout,
                first.0.clone(),
                2,
                0,
                ProtocolErrorCode::CursorEpochMismatch,
                RecoveryAction::FetchSessionSnapshot(first.clone()),
            ),
            (
                "kind",
                StreamKind::ProjectEvent,
                first.0.clone(),
                SESSION_STREAM_EPOCH,
                0,
                ProtocolErrorCode::UnsupportedStreamKind,
                RecoveryAction::UseSupportedStreamKind,
            ),
            (
                "missing",
                StreamKind::SessionRollout,
                "session-missing".to_owned(),
                SESSION_STREAM_EPOCH,
                0,
                ProtocolErrorCode::SessionNotFound,
                RecoveryAction::None,
            ),
        ];
        for (id, stream_kind, stream_id, epoch, after_sequence, code, recovery) in cases {
            let reply = service
                .execute(command(
                    id,
                    ClientCommand::EventResume {
                        cursor: StreamCursor {
                            stream_kind,
                            stream_id,
                            epoch,
                            after_sequence,
                        },
                    },
                ))
                .await
                .expect("typed cursor error");
            let CommandOutcome::Error { error } = reply.outcome else {
                panic!("expected cursor error");
            };
            assert_eq!(error.code, code);
            assert_eq!(
                error
                    .details
                    .expect("machine-readable details")
                    .recovery_action,
                recovery
            );
        }

        let second_page = resumed_page(
            service
                .execute(command(
                    "second-resume",
                    ClientCommand::EventResume {
                        cursor: StreamCursor {
                            stream_kind: StreamKind::SessionRollout,
                            stream_id: second.session_id.0.clone(),
                            epoch: SESSION_STREAM_EPOCH,
                            after_sequence: 0,
                        },
                    },
                ))
                .await
                .expect("second resume"),
        );
        assert!(matches!(
            &second_page.events[0].event,
            SessionEventKind::SessionStarted { session_id, .. } if session_id == &second.session_id
        ));
    }

    #[tokio::test]
    async fn event_pages_have_no_duplicates_or_omissions() {
        let service = AppService::spawn(QueueCapacity::new(512).expect("valid capacity"));
        let session_id = setup_session(&service).await;
        for number in 0..300 {
            service
                .execute(command(
                    format!("turn-{number}"),
                    ClientCommand::TurnStart {
                        session_id: session_id.clone(),
                        prompt: format!("prompt {number}"),
                    },
                ))
                .await
                .expect("start turn");
        }

        let mut after_sequence = 0;
        let mut sequences = Vec::new();
        loop {
            let page = resumed_page(
                service
                    .execute(command(
                        format!("page-{after_sequence}"),
                        ClientCommand::EventResume {
                            cursor: StreamCursor {
                                stream_kind: StreamKind::SessionRollout,
                                stream_id: session_id.0.clone(),
                                epoch: SESSION_STREAM_EPOCH,
                                after_sequence,
                            },
                        },
                    ))
                    .await
                    .expect("resume page"),
            );
            assert!(page.events.len() <= EVENT_PAGE_LIMIT);
            sequences.extend(page.events.iter().map(|event| event.sequence));
            after_sequence = page.next_after_sequence;
            if after_sequence == page.head_sequence {
                break;
            }
        }
        assert_eq!(sequences, (1..=601).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn ownership_prompt_validation_and_turn_idempotency_are_enforced() {
        let service = AppService::spawn(QueueCapacity::new(32).expect("valid capacity"));
        let first = setup_session(&service).await;
        let second_reply = service
            .execute(command(
                "second-session",
                ClientCommand::SessionStart {
                    project_id: ProjectId("project-1".to_owned()),
                },
            ))
            .await
            .expect("second session");
        let CommandOutcome::Success {
            result: CommandResult::SessionStarted(second),
        } = second_reply.outcome
        else {
            panic!("expected second session");
        };
        let start = command(
            "turn",
            ClientCommand::TurnStart {
                session_id: first.clone(),
                prompt: "valid".to_owned(),
            },
        );
        let first_reply = service.execute(start.clone()).await.expect("start");
        assert_eq!(first_reply, service.execute(start).await.expect("retry"));
        let CommandOutcome::Success {
            result: CommandResult::TurnStarted(turn),
        } = first_reply.outcome
        else {
            panic!("expected turn");
        };
        let mismatch = service
            .execute(command(
                "wrong-owner",
                ClientCommand::TurnCancel {
                    session_id: second.session_id,
                    turn_id: turn.turn_id,
                },
            ))
            .await
            .expect("ownership reply");
        assert!(matches!(
            mismatch.outcome,
            CommandOutcome::Error {
                error: crate::protocol::ProtocolError {
                    code: ProtocolErrorCode::TurnOwnershipMismatch,
                    ..
                }
            }
        ));
        let invalid = service
            .execute(command(
                "empty",
                ClientCommand::TurnStart {
                    session_id: first,
                    prompt: " ".to_owned(),
                },
            ))
            .await
            .expect("invalid prompt reply");
        assert!(matches!(
            invalid.outcome,
            CommandOutcome::Error {
                error: crate::protocol::ProtocolError {
                    code: ProtocolErrorCode::InvalidRequest,
                    ..
                }
            }
        ));
    }

    async fn start_waiting_turn(
        service: &AppService,
        session_id: &SessionId,
        id: &str,
    ) -> (TurnId, QuestionId) {
        let reply = service
            .execute(command(
                id,
                ClientCommand::TurnStart {
                    session_id: session_id.clone(),
                    prompt: "A bright chamber etude".to_owned(),
                },
            ))
            .await
            .expect("start waiting turn");
        let CommandOutcome::Success {
            result: CommandResult::TurnStarted(turn),
        } = reply.outcome
        else {
            panic!("expected turn");
        };
        let snapshot = session_snapshot(service, session_id, &format!("{id}-snapshot")).await;
        (
            turn.turn_id,
            snapshot
                .questions
                .last()
                .expect("question")
                .question_id
                .clone(),
        )
    }

    async fn session_snapshot(
        service: &AppService,
        session_id: &SessionId,
        id: &str,
    ) -> SessionSnapshot {
        let reply = service
            .execute(command(
                id,
                ClientCommand::SessionSnapshot {
                    session_id: session_id.clone(),
                },
            ))
            .await
            .expect("session snapshot");
        let CommandOutcome::Success {
            result: CommandResult::SessionSnapshot(snapshot),
        } = reply.outcome
        else {
            panic!("expected snapshot");
        };
        snapshot
    }

    async fn answer_question(
        service: &AppService,
        session_id: &SessionId,
        question_id: &QuestionId,
        id: &str,
    ) -> PendingApproval {
        service
            .execute(command(
                id,
                ClientCommand::QuestionRespond {
                    session_id: session_id.clone(),
                    question_id: question_id.clone(),
                    choice_id: ChoiceId("bars_8".to_owned()),
                },
            ))
            .await
            .expect("answer question");
        session_snapshot(service, session_id, &format!("{id}-snapshot"))
            .await
            .approvals
            .last()
            .expect("approval")
            .clone()
    }

    async fn approve_turn(
        service: &AppService,
        session_id: &SessionId,
        id: &str,
    ) -> ArtifactManifest {
        let (_, question_id) = start_waiting_turn(service, session_id, &format!("{id}-turn")).await;
        let approval =
            answer_question(service, session_id, &question_id, &format!("{id}-answer")).await;
        let reply = service
            .execute(command(
                format!("{id}-approve"),
                ClientCommand::ApprovalRespond {
                    session_id: session_id.clone(),
                    approval_id: approval.approval_id,
                    approval_subject_digest: approval.approval_subject_digest,
                    decision: ApprovalDecision::Approve,
                },
            ))
            .await
            .expect("approve turn");
        let CommandOutcome::Success {
            result:
                CommandResult::ApprovalDecided {
                    artifact_manifest: Some(manifest),
                    ..
                },
        } = reply.outcome
        else {
            panic!("expected artifact manifest");
        };
        manifest
    }

    async fn replay_projection(
        service: &AppService,
        session_id: &SessionId,
        id: &str,
    ) -> SessionProjection {
        let page = resumed_page(
            service
                .execute(command(
                    id,
                    ClientCommand::EventResume {
                        cursor: StreamCursor {
                            stream_kind: StreamKind::SessionRollout,
                            stream_id: session_id.0.clone(),
                            epoch: SESSION_STREAM_EPOCH,
                            after_sequence: 0,
                        },
                    },
                ))
                .await
                .expect("events for replay"),
        );
        let mut replayed = SessionProjection::default();
        for event in &page.events {
            reduce(&mut replayed, event);
        }
        replayed
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn a2_happy_path_snapshot_cursor_and_replay_are_identical() {
        let service = AppService::spawn(QueueCapacity::new(32).expect("valid capacity"));
        let session_id = setup_session(&service).await;
        let (_, question_id) = start_waiting_turn(&service, &session_id, "happy-turn").await;
        let before_answer = session_snapshot(&service, &session_id, "before-answer").await;
        assert_eq!(before_answer.covered_through_sequence, 3);
        assert_eq!(before_answer.questions[0].status, QuestionStatus::Pending);

        let approval = answer_question(&service, &session_id, &question_id, "happy-answer").await;
        let decided = service
            .execute(command(
                "happy-approve",
                ClientCommand::ApprovalRespond {
                    session_id: session_id.clone(),
                    approval_id: approval.approval_id.clone(),
                    approval_subject_digest: approval.approval_subject_digest.clone(),
                    decision: ApprovalDecision::Approve,
                },
            ))
            .await
            .expect("approve");
        let CommandOutcome::Success {
            result:
                CommandResult::ApprovalDecided {
                    approval: decided_approval,
                    artifact_manifest: Some(_),
                },
        } = decided.outcome
        else {
            panic!("expected approval decision");
        };
        assert_eq!(decided_approval.status, ApprovalStatus::Approved);
        assert_eq!(
            decided_approval.approval_subject_digest,
            approval.approval_subject_digest
        );

        let online = session_snapshot(&service, &session_id, "happy-final").await;
        assert_eq!(online.turns[0].status, TurnStatus::Succeeded);
        assert_eq!(
            online.questions[0]
                .answer
                .as_ref()
                .expect("answer")
                .choice_id
                .0,
            "bars_8"
        );
        assert_eq!(
            online.questions[0].responder_client_id,
            Some(ClientId("test-client".to_owned()))
        );
        assert_eq!(
            online.approvals[0].decision,
            Some(ApprovalDecision::Approve)
        );
        assert_eq!(online.covered_through_sequence, 7);

        let page = resumed_page(
            service
                .execute(command(
                    "happy-increment",
                    ClientCommand::EventResume {
                        cursor: StreamCursor {
                            stream_kind: StreamKind::SessionRollout,
                            stream_id: session_id.0,
                            epoch: SESSION_STREAM_EPOCH,
                            after_sequence: before_answer.covered_through_sequence,
                        },
                    },
                ))
                .await
                .expect("resume after snapshot"),
        );
        assert_eq!(
            page.events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![4, 5, 6, 7]
        );

        let all = resumed_page(
            service
                .execute(command(
                    "happy-all",
                    ClientCommand::EventResume {
                        cursor: StreamCursor {
                            stream_kind: StreamKind::SessionRollout,
                            stream_id: online.session_id.0.clone(),
                            epoch: SESSION_STREAM_EPOCH,
                            after_sequence: 0,
                        },
                    },
                ))
                .await
                .expect("all events"),
        );
        assert!(matches!(
            all.events.as_slice(),
            [
                SessionEvent {
                    event: SessionEventKind::SessionStarted { .. },
                    ..
                },
                SessionEvent {
                    event: SessionEventKind::TurnStarted { .. },
                    ..
                },
                SessionEvent {
                    event: SessionEventKind::QuestionRequested { .. },
                    ..
                },
                SessionEvent {
                    event: SessionEventKind::QuestionResolved { .. },
                    ..
                },
                SessionEvent {
                    event: SessionEventKind::ApprovalRequested { .. },
                    ..
                },
                SessionEvent {
                    event: SessionEventKind::ApprovalResolved {
                        decision: ApprovalDecision::Approve,
                        ..
                    },
                    ..
                },
                SessionEvent {
                    event: SessionEventKind::TurnCompleted {
                        status: TurnStatus::Succeeded,
                        ..
                    },
                    ..
                }
            ]
        ));
        let mut replayed = SessionProjection::default();
        for event in &all.events {
            reduce(&mut replayed, event);
        }
        assert_eq!(replayed.turns, online.turns);
        assert_eq!(replayed.questions, online.questions);
        assert_eq!(replayed.approvals, online.approvals);
    }

    #[tokio::test]
    async fn deny_path_fails_without_approved_state() {
        let service = AppService::spawn(QueueCapacity::new(16).expect("valid capacity"));
        let session_id = setup_session(&service).await;
        let (_, question_id) = start_waiting_turn(&service, &session_id, "deny-turn").await;
        let approval = answer_question(&service, &session_id, &question_id, "deny-answer").await;
        service
            .execute(command(
                "deny",
                ClientCommand::ApprovalRespond {
                    session_id: session_id.clone(),
                    approval_id: approval.approval_id,
                    approval_subject_digest: approval.approval_subject_digest,
                    decision: ApprovalDecision::Deny,
                },
            ))
            .await
            .expect("deny");
        let snapshot = session_snapshot(&service, &session_id, "deny-snapshot").await;
        assert_eq!(snapshot.turns[0].status, TurnStatus::Failed);
        assert_eq!(snapshot.approvals[0].status, ApprovalStatus::Denied);
        assert!(
            !snapshot
                .approvals
                .iter()
                .any(|item| item.status == ApprovalStatus::Approved)
        );
        assert_eq!(
            service.artifact_stats_for_test().await,
            ArtifactStats {
                blobs: 0,
                occurrences: 0,
                reachability: 0,
            }
        );
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn cancellation_aborts_pending_question_and_pending_approval_in_order() {
        let service = AppService::spawn(QueueCapacity::new(32).expect("valid capacity"));
        let question_session = setup_session(&service).await;
        let (turn_id, question_id) =
            start_waiting_turn(&service, &question_session, "question-cancel-turn").await;
        service
            .execute(command(
                "question-cancel",
                ClientCommand::TurnCancel {
                    session_id: question_session.clone(),
                    turn_id,
                },
            ))
            .await
            .expect("cancel question stage");
        let question_snapshot =
            session_snapshot(&service, &question_session, "question-cancel-snapshot").await;
        assert_eq!(
            question_snapshot.questions[0].status,
            QuestionStatus::OwnerTurnAborted
        );
        assert_eq!(question_snapshot.questions[0].terminal_sequence, Some(5));
        let question_events = resumed_page(
            service
                .execute(command(
                    "question-cancel-events",
                    ClientCommand::EventResume {
                        cursor: StreamCursor {
                            stream_kind: StreamKind::SessionRollout,
                            stream_id: question_session.0.clone(),
                            epoch: SESSION_STREAM_EPOCH,
                            after_sequence: 3,
                        },
                    },
                ))
                .await
                .expect("question cancel events"),
        );
        assert!(matches!(
            question_events.events.as_slice(),
            [
                SessionEvent {
                    event: SessionEventKind::TurnCancelRequested { .. },
                    ..
                },
                SessionEvent {
                    event: SessionEventKind::QuestionOwnerTurnAborted { .. },
                    ..
                },
                SessionEvent {
                    event: SessionEventKind::TurnCompleted {
                        status: TurnStatus::Cancelled,
                        ..
                    },
                    ..
                }
            ]
        ));
        let question_replay =
            replay_projection(&service, &question_session, "question-cancel-replay").await;
        assert_eq!(question_replay.turns, question_snapshot.turns);
        assert_eq!(question_replay.questions, question_snapshot.questions);
        assert_eq!(question_replay.approvals, question_snapshot.approvals);
        let aborted_question = service
            .execute(command(
                "question-after-abort",
                ClientCommand::QuestionRespond {
                    session_id: question_session,
                    question_id,
                    choice_id: ChoiceId("bars_8".to_owned()),
                },
            ))
            .await
            .expect("typed question abort reply");
        assert!(matches!(
            aborted_question.outcome,
            CommandOutcome::Error {
                error: crate::protocol::ProtocolError {
                    code: ProtocolErrorCode::RequestOwnerTurnAborted,
                    ..
                }
            }
        ));

        let approval_session_reply = service
            .execute(command(
                "approval-session",
                ClientCommand::SessionStart {
                    project_id: ProjectId("project-1".to_owned()),
                },
            ))
            .await
            .expect("approval session");
        let CommandOutcome::Success {
            result: CommandResult::SessionStarted(approval_session),
        } = approval_session_reply.outcome
        else {
            panic!("expected session");
        };
        let (turn_id, question_id) = start_waiting_turn(
            &service,
            &approval_session.session_id,
            "approval-cancel-turn",
        )
        .await;
        let approval = answer_question(
            &service,
            &approval_session.session_id,
            &question_id,
            "approval-cancel-answer",
        )
        .await;
        service
            .execute(command(
                "approval-cancel",
                ClientCommand::TurnCancel {
                    session_id: approval_session.session_id.clone(),
                    turn_id: turn_id.clone(),
                },
            ))
            .await
            .expect("cancel approval stage");
        let snapshot = session_snapshot(
            &service,
            &approval_session.session_id,
            "approval-cancel-snapshot",
        )
        .await;
        assert_eq!(
            snapshot.approvals[0].status,
            ApprovalStatus::OwnerTurnAborted
        );
        assert_eq!(snapshot.approvals[0].terminal_sequence, Some(7));
        let approval_events = resumed_page(
            service
                .execute(command(
                    "approval-cancel-events",
                    ClientCommand::EventResume {
                        cursor: StreamCursor {
                            stream_kind: StreamKind::SessionRollout,
                            stream_id: approval_session.session_id.0.clone(),
                            epoch: SESSION_STREAM_EPOCH,
                            after_sequence: 5,
                        },
                    },
                ))
                .await
                .expect("approval cancel events"),
        );
        assert!(matches!(
            approval_events.events.as_slice(),
            [
                SessionEvent {
                    event: SessionEventKind::TurnCancelRequested { .. },
                    ..
                },
                SessionEvent {
                    event: SessionEventKind::ApprovalOwnerTurnAborted { .. },
                    ..
                },
                SessionEvent {
                    event: SessionEventKind::TurnCompleted {
                        status: TurnStatus::Cancelled,
                        ..
                    },
                    ..
                }
            ]
        ));
        let approval_replay = replay_projection(
            &service,
            &approval_session.session_id,
            "approval-cancel-replay",
        )
        .await;
        assert_eq!(approval_replay.turns, snapshot.turns);
        assert_eq!(approval_replay.questions, snapshot.questions);
        assert_eq!(approval_replay.approvals, snapshot.approvals);
        let aborted_response = service
            .execute(command(
                "after-abort",
                ClientCommand::ApprovalRespond {
                    session_id: approval_session.session_id,
                    approval_id: approval.approval_id,
                    approval_subject_digest: approval.approval_subject_digest,
                    decision: ApprovalDecision::Approve,
                },
            ))
            .await
            .expect("typed abort reply");
        assert!(matches!(
            aborted_response.outcome,
            CommandOutcome::Error {
                error: crate::protocol::ProtocolError {
                    code: ProtocolErrorCode::RequestOwnerTurnAborted,
                    ..
                }
            }
        ));
        assert_eq!(
            service.artifact_stats_for_test().await,
            ArtifactStats {
                blobs: 0,
                occurrences: 0,
                reachability: 0,
            }
        );
    }

    #[tokio::test]
    async fn invalid_choice_and_digest_mismatch_do_not_append_or_mutate() {
        let service = AppService::spawn(QueueCapacity::new(24).expect("valid capacity"));
        let session_id = setup_session(&service).await;
        let (_, question_id) = start_waiting_turn(&service, &session_id, "invalid-turn").await;
        let invalid = service
            .execute(command(
                "invalid-choice",
                ClientCommand::QuestionRespond {
                    session_id: session_id.clone(),
                    question_id: question_id.clone(),
                    choice_id: ChoiceId("bars_32".to_owned()),
                },
            ))
            .await
            .expect("invalid choice");
        assert!(matches!(
            invalid.outcome,
            CommandOutcome::Error {
                error: crate::protocol::ProtocolError {
                    code: ProtocolErrorCode::InvalidQuestionChoice,
                    ..
                }
            }
        ));
        assert_eq!(
            session_snapshot(&service, &session_id, "after-invalid-choice")
                .await
                .covered_through_sequence,
            3
        );
        let empty = service
            .execute(command(
                "empty-choice",
                ClientCommand::QuestionRespond {
                    session_id: session_id.clone(),
                    question_id: question_id.clone(),
                    choice_id: ChoiceId(String::new()),
                },
            ))
            .await
            .expect("empty choice");
        assert!(matches!(
            empty.outcome,
            CommandOutcome::Error {
                error: crate::protocol::ProtocolError {
                    code: ProtocolErrorCode::InvalidRequest,
                    ..
                }
            }
        ));
        let approval = answer_question(&service, &session_id, &question_id, "valid-answer").await;
        let mismatch = service
            .execute(command(
                "digest-mismatch",
                ClientCommand::ApprovalRespond {
                    session_id: session_id.clone(),
                    approval_id: approval.approval_id,
                    approval_subject_digest: ApprovalSubjectDigest {
                        value: "00".repeat(32),
                        ..approval.approval_subject_digest
                    },
                    decision: ApprovalDecision::Approve,
                },
            ))
            .await
            .expect("digest mismatch");
        assert!(matches!(
            mismatch.outcome,
            CommandOutcome::Error {
                error: crate::protocol::ProtocolError {
                    code: ProtocolErrorCode::ApprovalSubjectMismatch,
                    ..
                }
            }
        ));
        let after = session_snapshot(&service, &session_id, "after-mismatch").await;
        assert_eq!(after.covered_through_sequence, 5);
        assert_eq!(after.approvals[0].status, ApprovalStatus::Pending);
        assert_eq!(
            service.artifact_stats_for_test().await,
            ArtifactStats {
                blobs: 0,
                occurrences: 0,
                reachability: 0,
            }
        );
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn a2_transport_business_idempotency_and_cross_session_ownership_are_distinct() {
        let service = AppService::spawn(QueueCapacity::new(32).expect("valid capacity"));
        let first = setup_session(&service).await;
        let (_, question_id) = start_waiting_turn(&service, &first, "idempotent-turn").await;
        let answer = command(
            "idempotent-answer",
            ClientCommand::QuestionRespond {
                session_id: first.clone(),
                question_id: question_id.clone(),
                choice_id: ChoiceId("bars_8".to_owned()),
            },
        );
        let first_answer = service.execute(answer.clone()).await.expect("first answer");
        assert_eq!(
            first_answer,
            service.execute(answer).await.expect("transport retry")
        );
        let repeated_answer = service
            .execute(command(
                "business-answer",
                ClientCommand::QuestionRespond {
                    session_id: first.clone(),
                    question_id: question_id.clone(),
                    choice_id: ChoiceId("bars_8".to_owned()),
                },
            ))
            .await
            .expect("business repeat");
        assert_eq!(
            repeated_answer.client_command_id,
            ClientCommandId("business-answer".to_owned())
        );
        assert!(matches!(
            repeated_answer.outcome,
            CommandOutcome::Success {
                result: CommandResult::QuestionAlreadyResolved(_)
            }
        ));

        let snapshot = session_snapshot(&service, &first, "idempotent-snapshot").await;
        assert_eq!(snapshot.covered_through_sequence, 5);
        let approval = snapshot.approvals[0].clone();
        let approval_command = command(
            "idempotent-approval",
            ClientCommand::ApprovalRespond {
                session_id: first.clone(),
                approval_id: approval.approval_id.clone(),
                approval_subject_digest: approval.approval_subject_digest.clone(),
                decision: ApprovalDecision::Approve,
            },
        );
        let first_decision = service
            .execute(approval_command.clone())
            .await
            .expect("first decision");
        assert_eq!(
            first_decision,
            service
                .execute(approval_command)
                .await
                .expect("decision transport retry")
        );
        let repeated_decision = service
            .execute(command(
                "business-approval",
                ClientCommand::ApprovalRespond {
                    session_id: first.clone(),
                    approval_id: approval.approval_id.clone(),
                    approval_subject_digest: approval.approval_subject_digest.clone(),
                    decision: ApprovalDecision::Approve,
                },
            ))
            .await
            .expect("business decision repeat");
        assert!(matches!(
            repeated_decision.outcome,
            CommandOutcome::Success {
                result: CommandResult::ApprovalAlreadyResolved(_)
            }
        ));
        assert_eq!(
            session_snapshot(&service, &first, "after-repeats")
                .await
                .covered_through_sequence,
            7
        );
        assert_eq!(
            service.artifact_stats_for_test().await,
            ArtifactStats {
                blobs: 1,
                occurrences: 1,
                reachability: 1,
            }
        );

        let second_reply = service
            .execute(command(
                "ownership-session",
                ClientCommand::SessionStart {
                    project_id: ProjectId("project-1".to_owned()),
                },
            ))
            .await
            .expect("second session");
        let CommandOutcome::Success {
            result: CommandResult::SessionStarted(second),
        } = second_reply.outcome
        else {
            panic!("expected second session");
        };
        let wrong_question = service
            .execute(command(
                "wrong-question-session",
                ClientCommand::QuestionRespond {
                    session_id: second.session_id.clone(),
                    question_id,
                    choice_id: ChoiceId("bars_8".to_owned()),
                },
            ))
            .await
            .expect("question ownership");
        assert!(matches!(
            wrong_question.outcome,
            CommandOutcome::Error {
                error: crate::protocol::ProtocolError {
                    code: ProtocolErrorCode::QuestionOwnershipMismatch,
                    ..
                }
            }
        ));
        let wrong_approval = service
            .execute(command(
                "wrong-approval-session",
                ClientCommand::ApprovalRespond {
                    session_id: second.session_id,
                    approval_id: approval.approval_id,
                    approval_subject_digest: approval.approval_subject_digest,
                    decision: ApprovalDecision::Approve,
                },
            ))
            .await
            .expect("approval ownership");
        assert!(matches!(
            wrong_approval.outcome,
            CommandOutcome::Error {
                error: crate::protocol::ProtocolError {
                    code: ProtocolErrorCode::ApprovalOwnershipMismatch,
                    ..
                }
            }
        ));
    }

    #[tokio::test]
    async fn artifact_occurrences_preserve_provenance_while_blobs_deduplicate() {
        let service = AppService::spawn(QueueCapacity::new(64).expect("valid capacity"));
        let first_session = setup_session(&service).await;
        let first = approve_turn(&service, &first_session, "first").await;
        let second = approve_turn(&service, &first_session, "second").await;
        assert_ne!(first.artifact_occurrence_id, second.artifact_occurrence_id);
        assert_ne!(first.source_turn_id, second.source_turn_id);
        assert_eq!(first.artifact_hash, second.artifact_hash);
        assert_eq!(
            service.artifact_stats_for_test().await,
            ArtifactStats {
                blobs: 1,
                occurrences: 2,
                reachability: 1,
            }
        );

        service
            .execute(create_command("second-project", "Nocturne"))
            .await
            .expect("second project");
        let second_session_reply = service
            .execute(command(
                "second-project-session",
                ClientCommand::SessionStart {
                    project_id: ProjectId("project-2".to_owned()),
                },
            ))
            .await
            .expect("second project session");
        let CommandOutcome::Success {
            result: CommandResult::SessionStarted(second_session),
        } = second_session_reply.outcome
        else {
            panic!("expected second project session");
        };
        let cross_project =
            approve_turn(&service, &second_session.session_id, "cross-project").await;
        assert_eq!(first.artifact_hash, cross_project.artifact_hash);
        assert_ne!(first.project_id, cross_project.project_id);
        assert_eq!(
            service.artifact_stats_for_test().await,
            ArtifactStats {
                blobs: 1,
                occurrences: 3,
                reachability: 2,
            }
        );

        let manifest_reply = service
            .execute(command(
                "manifest",
                ClientCommand::ArtifactManifest {
                    project_id: first.project_id.clone(),
                    artifact_occurrence_id: second.artifact_occurrence_id.clone(),
                },
            ))
            .await
            .expect("manifest query");
        assert!(matches!(
            manifest_reply.outcome,
            CommandOutcome::Success {
                result: CommandResult::ArtifactManifest(ref manifest)
            } if manifest == &second
        ));
        for (id, project_id, occurrence_id) in [
            (
                "cross-project-manifest",
                cross_project.project_id,
                first.artifact_occurrence_id,
            ),
            (
                "missing-manifest",
                first.project_id,
                ArtifactOccurrenceId("artifact-occurrence-missing".to_owned()),
            ),
        ] {
            let reply = service
                .execute(command(
                    id,
                    ClientCommand::ArtifactManifest {
                        project_id,
                        artifact_occurrence_id: occurrence_id,
                    },
                ))
                .await
                .expect("not found manifest");
            assert!(matches!(
                reply.outcome,
                CommandOutcome::Error {
                    error: crate::protocol::ProtocolError {
                        code: ProtocolErrorCode::ArtifactNotFound,
                        ..
                    }
                }
            ));
        }
    }

    #[tokio::test]
    async fn fixture_preparation_failure_has_no_partial_commit_or_cached_reply() {
        for fault in [FixtureFault::HashMismatch, FixtureFault::SizeMismatch] {
            let service = AppService::spawn(QueueCapacity::new(32).expect("valid capacity"));
            let session_id = setup_session(&service).await;
            let (_, question_id) = start_waiting_turn(&service, &session_id, "fault-turn").await;
            let approval =
                answer_question(&service, &session_id, &question_id, "fault-answer").await;
            service.set_fixture_fault_for_test(fault).await;
            let approve = command(
                "fault-approve",
                ClientCommand::ApprovalRespond {
                    session_id: session_id.clone(),
                    approval_id: approval.approval_id.clone(),
                    approval_subject_digest: approval.approval_subject_digest.clone(),
                    decision: ApprovalDecision::Approve,
                },
            );
            let failure = service.execute(approve.clone()).await.expect("fault reply");
            assert!(matches!(
                failure.outcome,
                CommandOutcome::Error {
                    error: crate::protocol::ProtocolError {
                        code: ProtocolErrorCode::ArtifactPreparationFailed,
                        ..
                    }
                }
            ));
            let snapshot = session_snapshot(&service, &session_id, "fault-snapshot").await;
            assert_eq!(snapshot.covered_through_sequence, 5);
            assert_eq!(snapshot.turns[0].status, TurnStatus::WaitingForInput);
            assert_eq!(snapshot.approvals[0].status, ApprovalStatus::Pending);
            assert_eq!(
                service.artifact_stats_for_test().await,
                ArtifactStats {
                    blobs: 0,
                    occurrences: 0,
                    reachability: 0,
                }
            );

            service.set_fixture_fault_for_test(FixtureFault::None).await;
            let retry = service.execute(approve).await.expect("retry after repair");
            assert!(matches!(
                retry.outcome,
                CommandOutcome::Success {
                    result: CommandResult::ApprovalDecided {
                        artifact_manifest: Some(_),
                        ..
                    }
                }
            ));
        }
    }

    #[tokio::test]
    async fn fixture_hash_size_and_internal_download_vector_are_fixed() {
        let prepared = prepare_fixture(FixtureFault::None).expect("valid fixture");
        assert_eq!(prepared.hash.as_str(), FIXTURE_HASH);
        assert_eq!(prepared.size_bytes, FIXTURE_SIZE);
        assert_eq!(&*prepared.bytes, FIXTURE_BYTES);

        let service = AppService::spawn(QueueCapacity::new(32).expect("valid capacity"));
        let session_id = setup_session(&service).await;
        let manifest = approve_turn(&service, &session_id, "download").await;
        let download = service
            .resolve_artifact_download(
                manifest.project_id.clone(),
                manifest.artifact_hash.clone(),
                None,
            )
            .await
            .expect("download query");
        let DownloadResolution::Verified(download) = download else {
            panic!("expected verified download");
        };
        assert_eq!(&*download.bytes, FIXTURE_BYTES);
        assert_eq!(download.size_bytes, FIXTURE_SIZE);
        assert_eq!(
            service
                .resolve_artifact_download(
                    manifest.project_id,
                    manifest.artifact_hash.clone(),
                    Some(format!("\"{}\"", manifest.artifact_hash.as_str())),
                )
                .await
                .expect("conditional query"),
            DownloadResolution::NotModified(manifest.artifact_hash)
        );
    }

    #[test]
    fn approval_subject_v1_has_a_fixed_canonical_test_vector() {
        let digest = approval_subject_digest(
            "https://api.openai.com",
            &["prompt", "constraints", "prompt"],
            &TurnId("turn-7".to_owned()),
            "A bright chamber etude",
        );
        assert_eq!(digest.algorithm, "sha256");
        assert_eq!(digest.schema_version, 1);
        assert_eq!(
            digest.value,
            "52d1282646e363a660cc68c98161d50d6fae7b5d9c02e7efbe4f82c104b6eb33"
        );
    }
}
