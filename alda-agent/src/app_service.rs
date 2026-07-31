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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueueCapacity(NonZeroUsize);

impl QueueCapacity {
    /// Creates a non-zero queue capacity.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidQueueCapacity`] when `value` is zero.
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
    /// Creates a non-zero internal query queue capacity.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidQueueCapacity`] when `value` is zero.
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

    /// Enqueues a command without waiting for the actor to process it.
    ///
    /// # Errors
    ///
    /// Returns [`SubmitError::Overloaded`] when the bounded queue is full, or
    /// [`SubmitError::Closed`] after the service runner has stopped.
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

    /// Enqueues a command and waits for its protocol reply.
    ///
    /// # Errors
    ///
    /// Returns a [`SubmitError`] when the command cannot be enqueued or when
    /// the runner stops before replying.
    pub async fn execute(&self, envelope: CommandEnvelope) -> Result<CommandReply, SubmitError> {
        self.enqueue(envelope)?.wait().await
    }

    /// Resolves an authenticated HTTP download through the same bounded actor
    /// that owns Artifact state.
    ///
    /// # Errors
    ///
    /// Returns a [`SubmitError`] if the actor queue is unavailable.
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

    /// Resolves Session events via the bounded, lower-priority query channel.
    ///
    /// # Errors
    ///
    /// Returns a [`SubmitError`] when the query cannot be enqueued or replied.
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
    /// Waits for the application service to process an enqueued command.
    ///
    /// # Errors
    ///
    /// Returns [`SubmitError::ReplyDropped`] when the runner stops without
    /// producing a reply.
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

    // Keeping the exhaustive command dispatch together makes the wire surface
    // auditable; state transitions remain in the single actor.
    #[allow(clippy::too_many_lines)]
    fn process(&mut self, envelope: &CommandEnvelope) -> CommandReply {
        if envelope.protocol_version != PROTOCOL_VERSION {
            return CommandReply::error(
                envelope.client_command_id.clone(),
                ProtocolErrorCode::InvalidProtocolVersion,
                format!(
                    "unsupported protocol version {}; expected {PROTOCOL_VERSION}",
                    envelope.protocol_version
                ),
            );
        }

        match &envelope.command {
            ClientCommand::Initialize => CommandReply::success(
                envelope.client_command_id.clone(),
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
            ),
            ClientCommand::ProjectCreate { name } => {
                let name = name.trim();
                if name.is_empty() || name.chars().count() > 120 {
                    return CommandReply::error(
                        envelope.client_command_id.clone(),
                        ProtocolErrorCode::InvalidRequest,
                        "project name must contain 1 to 120 characters",
                    );
                }

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
                let prompt = prompt.trim();
                if prompt.is_empty() || prompt.chars().count() > 8_000 {
                    return CommandReply::error(
                        envelope.client_command_id.clone(),
                        ProtocolErrorCode::InvalidRequest,
                        "turn prompt must contain 1 to 8000 characters",
                    );
                }
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

    // The cancellation transaction keeps validation and the complete,
    // auditable event ordering adjacent.
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

    // The response transaction validates before appending either of its two
    // linked facts (resolution and the next approval request).
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

    // Validation, local fixture preparation, atomic store/fact commit, and the
    // stable reply are kept adjacent to make the transition auditable.
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

fn request_fingerprint(envelope: &CommandEnvelope) -> String {
    serde_json::to_string(&(envelope.protocol_version, &envelope.command))
        .expect("serializing a typed command envelope should not fail")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::CommandOutcome;
    use crate::protocol::StreamCursor;

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
