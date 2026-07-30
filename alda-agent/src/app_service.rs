use std::collections::HashMap;
use std::num::NonZeroUsize;

use thiserror::Error;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::protocol::ClientCommand;
use crate::protocol::ClientCommandId;
use crate::protocol::ClientId;
use crate::protocol::CommandEnvelope;
use crate::protocol::CommandReply;
use crate::protocol::CommandResult;
use crate::protocol::PROTOCOL_VERSION;
use crate::protocol::ProjectId;
use crate::protocol::ProjectSnapshot;
use crate::protocol::ProtocolErrorCode;

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

#[derive(Clone, Debug)]
pub struct AppService {
    command_tx: mpsc::Sender<QueuedCommand>,
}

impl AppService {
    #[must_use]
    pub fn build(capacity: QueueCapacity) -> (Self, AppServiceRunner) {
        let (command_tx, command_rx) = mpsc::channel(capacity.get());
        (
            Self { command_tx },
            AppServiceRunner {
                command_rx,
                state: ServiceState::default(),
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
    state: ServiceState,
}

impl AppServiceRunner {
    pub async fn run(mut self) {
        while let Some(queued) = self.command_rx.recv().await {
            let reply = self.state.handle(queued.envelope);
            let _reply_was_dropped = queued.reply_tx.send(reply);
        }
    }
}

struct QueuedCommand {
    envelope: CommandEnvelope,
    reply_tx: oneshot::Sender<CommandReply>,
}

#[derive(Default)]
struct ServiceState {
    next_project_number: u64,
    projects: HashMap<ProjectId, ProjectSnapshot>,
    idempotency: HashMap<(ClientId, ClientCommandId), StoredReply>,
}

#[derive(Clone)]
struct StoredReply {
    request_fingerprint: String,
    reply: CommandReply,
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
        self.idempotency.insert(
            key,
            StoredReply {
                request_fingerprint,
                reply: reply.clone(),
            },
        );
        reply
    }

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
                    capabilities: vec!["project.create".to_owned(), "project.snapshot".to_owned()],
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
                self.projects
                    .insert(snapshot.project_id.clone(), snapshot.clone());
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
        }
    }
}

fn request_fingerprint(envelope: &CommandEnvelope) -> String {
    serde_json::to_string(&(envelope.protocol_version, &envelope.command))
        .expect("serializing a typed command envelope should not fail")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::CommandOutcome;

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
}
