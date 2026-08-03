//! production durable actor 的线程、队列与生命周期边界。

#![allow(
    clippy::missing_errors_doc,
    reason = "Rustdoc 依照仓库语言规范使用中文“错误”标题"
)]

use std::any::Any;
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::thread;

use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch};

use crate::app_service::{
    DownloadResolution, DurableServiceState, QueryQueueCapacity, QueueCapacity,
};
use crate::durable_runtime::ReadyDurableRuntime;
use crate::protocol::{ArtifactHash, CommandEnvelope, CommandReply, ProjectId};

/// 与请求队列独立传播的 actor 生命周期。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActorLifecycle {
    Running,
    Stopping,
    Fatal,
}

/// 有界入口在入队或等待回复时可观察到的失败。
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ActorRequestError {
    #[error("durable actor queue is overloaded")]
    Overloaded,
    #[error("durable actor queue is closed")]
    Closed,
    #[error("durable actor is stopping")]
    Stopping,
    #[error("durable actor is in a fatal state")]
    Fatal,
}

/// actor OS 线程启动失败。
#[derive(Debug, Error)]
pub enum ActorStartError {
    #[error("failed to spawn the durable actor thread")]
    ThreadSpawn(#[source] std::io::Error),
}

/// 唯一 join 所有权产生的确定性结果。
#[derive(Debug, Error, Eq, PartialEq)]
pub enum ActorJoinError {
    #[error("durable actor join handle was already consumed")]
    AlreadyJoined,
    #[error("durable actor terminated after a fatal runtime error: {0}")]
    Fatal(String),
    #[error("durable actor panicked: {0}")]
    Panicked(String),
    #[error("durable actor thread panicked outside its guarded dispatch loop: {0}")]
    ThreadPanicked(String),
}

/// 可复制给 transport 的窄句柄；它不持有生命周期发送端或 join 所有权。
#[derive(Clone)]
pub struct DurableActorClient {
    command_tx: mpsc::Sender<CommandRequest>,
    query_tx: mpsc::Sender<QueryRequest>,
    lifecycle_rx: watch::Receiver<ActorLifecycle>,
}

impl DurableActorClient {
    /// 立即尝试向 mutation 队列提交命令。
    ///
    /// # 错误
    ///
    /// 队列已满或 actor 不再接收请求时返回明确错误。
    pub fn enqueue_command(
        &self,
        envelope: CommandEnvelope,
    ) -> Result<PendingCommandReply, ActorRequestError> {
        self.require_running()?;
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .try_send(CommandRequest { envelope, reply_tx })
            .map_err(|error| self.map_try_send(&error))?;
        Ok(PendingCommandReply { reply_rx })
    }

    /// 提交 mutation 并等待 actor 的协议回复。
    ///
    /// # 错误
    ///
    /// 入队失败或 actor 在回复前停止时返回明确错误。
    pub async fn execute_command(
        &self,
        envelope: CommandEnvelope,
    ) -> Result<CommandReply, ActorRequestError> {
        self.enqueue_command(envelope)?.wait().await
    }

    /// 立即尝试向只读队列提交协议查询。
    ///
    /// # 错误
    ///
    /// 队列已满或 actor 不再接收请求时返回明确错误。
    pub fn enqueue_query(
        &self,
        envelope: CommandEnvelope,
    ) -> Result<PendingCommandReply, ActorRequestError> {
        self.require_running()?;
        let (reply_tx, reply_rx) = oneshot::channel();
        self.query_tx
            .try_send(QueryRequest::Protocol { envelope, reply_tx })
            .map_err(|error| self.map_try_send(&error))?;
        Ok(PendingCommandReply { reply_rx })
    }

    /// 提交协议查询并等待 actor 回复。
    ///
    /// # 错误
    ///
    /// 入队失败或 actor 在回复前停止时返回明确错误。
    pub async fn execute_query(
        &self,
        envelope: CommandEnvelope,
    ) -> Result<CommandReply, ActorRequestError> {
        self.enqueue_query(envelope)?.wait().await
    }

    /// 通过只读队列解析 Artifact 下载。
    ///
    /// # 错误
    ///
    /// 队列已满或 actor 在回复前停止时返回明确错误。
    pub async fn resolve_artifact_download(
        &self,
        project_id: ProjectId,
        hash: ArtifactHash,
        if_none_match: Option<String>,
    ) -> Result<DownloadResolution, ActorRequestError> {
        self.require_running()?;
        let (reply_tx, reply_rx) = oneshot::channel();
        self.query_tx
            .try_send(QueryRequest::ArtifactDownload {
                project_id,
                hash,
                if_none_match,
                reply_tx,
            })
            .map_err(|error| self.map_try_send(&error))?;
        reply_rx.await.unwrap_or_else(|_| Err(self.closed_error()))
    }

    #[must_use]
    pub fn subscribe_lifecycle(&self) -> watch::Receiver<ActorLifecycle> {
        self.lifecycle_rx.clone()
    }

    fn require_running(&self) -> Result<(), ActorRequestError> {
        match *self.lifecycle_rx.borrow() {
            ActorLifecycle::Running => Ok(()),
            ActorLifecycle::Stopping => Err(ActorRequestError::Stopping),
            ActorLifecycle::Fatal => Err(ActorRequestError::Fatal),
        }
    }

    fn map_try_send<T>(&self, error: &mpsc::error::TrySendError<T>) -> ActorRequestError {
        match error {
            mpsc::error::TrySendError::Full(_) => ActorRequestError::Overloaded,
            mpsc::error::TrySendError::Closed(_) => self.closed_error(),
        }
    }

    fn closed_error(&self) -> ActorRequestError {
        match *self.lifecycle_rx.borrow() {
            ActorLifecycle::Running => ActorRequestError::Closed,
            ActorLifecycle::Stopping => ActorRequestError::Stopping,
            ActorLifecycle::Fatal => ActorRequestError::Fatal,
        }
    }

    #[cfg(test)]
    async fn panic_actor_for_test(&self) -> Result<(), ActorRequestError> {
        self.require_running()?;
        let (reply_tx, reply_rx) = oneshot::channel();
        self.query_tx
            .try_send(QueryRequest::PanicForTest { reply_tx })
            .map_err(|error| self.map_try_send(&error))?;
        reply_rx.await.unwrap_or_else(|_| Err(self.closed_error()))
    }

    #[cfg(test)]
    async fn actor_thread_id_for_test(&self) -> Result<thread::ThreadId, ActorRequestError> {
        self.require_running()?;
        let (reply_tx, reply_rx) = oneshot::channel();
        self.query_tx
            .try_send(QueryRequest::ThreadIdForTest { reply_tx })
            .map_err(|error| self.map_try_send(&error))?;
        reply_rx.await.unwrap_or_else(|_| Err(self.closed_error()))
    }
}

/// 等待已成功入队的协议命令回复。
pub struct PendingCommandReply {
    reply_rx: oneshot::Receiver<Result<CommandReply, ActorRequestError>>,
}

impl PendingCommandReply {
    /// # 错误
    ///
    /// actor 明确拒绝排队请求或未能发送回复时返回错误。
    pub async fn wait(self) -> Result<CommandReply, ActorRequestError> {
        self.reply_rx
            .await
            .unwrap_or(Err(ActorRequestError::Closed))
    }
}

/// production composition root 唯一持有的 actor 生命周期与 join 所有者。
pub struct DurableActorHost {
    client: DurableActorClient,
    control_tx: Option<mpsc::UnboundedSender<ControlMessage>>,
    join_handle: Option<thread::JoinHandle<Result<(), ActorThreadError>>>,
}

#[allow(
    dead_code,
    reason = "本叶先冻结 actor host，下一 C3 叶负责接入 production composition root"
)]
impl DurableActorHost {
    /// 在专用 OS 线程中启动 current-thread Tokio runtime。
    ///
    /// # 错误
    ///
    /// OS 无法创建专用线程时返回错误；此时传入的 durable runtime 会被释放。
    pub(crate) fn start(
        runtime: ReadyDurableRuntime,
        command_capacity: QueueCapacity,
        query_capacity: QueryQueueCapacity,
    ) -> Result<Self, ActorStartError> {
        Self::start_inner(
            runtime,
            command_capacity,
            query_capacity,
            #[cfg(test)]
            None,
            #[cfg(test)]
            None,
        )
    }

    fn start_inner(
        runtime: ReadyDurableRuntime,
        command_capacity: QueueCapacity,
        query_capacity: QueryQueueCapacity,
        #[cfg(test)] start_gate: Option<StartGate>,
        #[cfg(test)] fatal_drop_gate: Option<FatalDropGate>,
    ) -> Result<Self, ActorStartError> {
        let (command_tx, command_rx) = mpsc::channel(command_capacity.get());
        let (query_tx, query_rx) = mpsc::channel(query_capacity.get());
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let (lifecycle_tx, lifecycle_rx) = watch::channel(ActorLifecycle::Running);
        let thread_lifecycle_tx = lifecycle_tx.clone();
        let join_handle = thread::Builder::new()
            .name("alda-durable-actor".to_owned())
            .spawn(move || {
                #[cfg(test)]
                if let Some(gate) = start_gate {
                    let _released = gate.wait();
                }
                run_actor_thread(
                    runtime,
                    command_rx,
                    query_rx,
                    control_rx,
                    thread_lifecycle_tx,
                    #[cfg(test)]
                    fatal_drop_gate,
                )
            })
            .map_err(ActorStartError::ThreadSpawn)?;
        Ok(Self {
            client: DurableActorClient {
                command_tx,
                query_tx,
                lifecycle_rx,
            },
            control_tx: Some(control_tx),
            join_handle: Some(join_handle),
        })
    }

    #[must_use]
    pub fn client(&self) -> DurableActorClient {
        self.client.clone()
    }

    #[must_use]
    pub fn lifecycle(&self) -> ActorLifecycle {
        *self.client.lifecycle_rx.borrow()
    }

    /// 显式请求停止；重复调用不会依赖 sender 引用计数判断存活。
    pub fn shutdown(&mut self) {
        if let Some(control_tx) = self.control_tx.take() {
            let _actor_already_stopped = control_tx.send(ControlMessage::Shutdown);
        }
    }

    /// 消费唯一 join 所有权并传播 fatal 或 panic。
    ///
    /// # 错误
    ///
    /// 重复 join、actor fatal 或 panic 时返回可转换为 serve 错误的值。
    pub fn shutdown_and_join(&mut self) -> Result<(), ActorJoinError> {
        self.shutdown();
        let Some(join_handle) = self.join_handle.take() else {
            return Err(ActorJoinError::AlreadyJoined);
        };
        match join_handle.join() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(ActorThreadError::Fatal(detail))) => Err(ActorJoinError::Fatal(detail)),
            Ok(Err(ActorThreadError::Panicked(detail))) => Err(ActorJoinError::Panicked(detail)),
            Err(payload) => Err(ActorJoinError::ThreadPanicked(panic_detail(&*payload))),
        }
    }

    #[cfg(test)]
    fn start_paused_for_test(
        runtime: ReadyDurableRuntime,
        command_capacity: QueueCapacity,
        query_capacity: QueryQueueCapacity,
    ) -> Result<(Self, StartGateRelease), ActorStartError> {
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
        let host = Self::start_inner(
            runtime,
            command_capacity,
            query_capacity,
            Some(StartGate { release_rx }),
            None,
        )?;
        Ok((host, StartGateRelease { release_tx }))
    }

    #[cfg(test)]
    fn start_with_fatal_drop_gate_for_test(
        runtime: ReadyDurableRuntime,
        command_capacity: QueueCapacity,
        query_capacity: QueryQueueCapacity,
    ) -> Result<(Self, FatalDropGateRelease), ActorStartError> {
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
        let host = Self::start_inner(
            runtime,
            command_capacity,
            query_capacity,
            None,
            Some(FatalDropGate { release_rx }),
        )?;
        Ok((host, FatalDropGateRelease { release_tx }))
    }
}

impl Drop for DurableActorHost {
    fn drop(&mut self) {
        self.shutdown();
        if let Some(join_handle) = self.join_handle.take() {
            let _actor_result = join_handle.join();
        }
    }
}

struct CommandRequest {
    envelope: CommandEnvelope,
    reply_tx: oneshot::Sender<Result<CommandReply, ActorRequestError>>,
}

enum QueryRequest {
    Protocol {
        envelope: CommandEnvelope,
        reply_tx: oneshot::Sender<Result<CommandReply, ActorRequestError>>,
    },
    ArtifactDownload {
        project_id: ProjectId,
        hash: ArtifactHash,
        if_none_match: Option<String>,
        reply_tx: oneshot::Sender<Result<DownloadResolution, ActorRequestError>>,
    },
    #[cfg(test)]
    PanicForTest {
        reply_tx: oneshot::Sender<Result<(), ActorRequestError>>,
    },
    #[cfg(test)]
    ThreadIdForTest {
        reply_tx: oneshot::Sender<Result<thread::ThreadId, ActorRequestError>>,
    },
}

enum ControlMessage {
    Shutdown,
}

#[derive(Debug)]
enum ActorThreadError {
    Fatal(String),
    Panicked(String),
}

fn run_actor_thread(
    runtime: ReadyDurableRuntime,
    command_rx: mpsc::Receiver<CommandRequest>,
    query_rx: mpsc::Receiver<QueryRequest>,
    control_rx: mpsc::UnboundedReceiver<ControlMessage>,
    lifecycle_tx: watch::Sender<ActorLifecycle>,
    #[cfg(test)] fatal_drop_gate: Option<FatalDropGate>,
) -> Result<(), ActorThreadError> {
    let tokio_runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            lifecycle_tx.send_replace(ActorLifecycle::Fatal);
            drop(runtime);
            return Err(ActorThreadError::Fatal(format!(
                "failed to build current-thread runtime: {error}"
            )));
        }
    };
    let state = DurableServiceState::new(runtime);
    tokio_runtime.block_on(run_actor(
        state,
        command_rx,
        query_rx,
        control_rx,
        lifecycle_tx,
        #[cfg(test)]
        fatal_drop_gate,
    ))
}

async fn run_actor(
    mut state: DurableServiceState,
    mut command_rx: mpsc::Receiver<CommandRequest>,
    mut query_rx: mpsc::Receiver<QueryRequest>,
    mut control_rx: mpsc::UnboundedReceiver<ControlMessage>,
    lifecycle_tx: watch::Sender<ActorLifecycle>,
    #[cfg(test)] mut fatal_drop_gate: Option<FatalDropGate>,
) -> Result<(), ActorThreadError> {
    loop {
        tokio::select! {
            biased;
            control = control_rx.recv() => {
                if matches!(control, Some(ControlMessage::Shutdown)) {
                    stop_actor(
                        &mut command_rx,
                        &mut query_rx,
                        &lifecycle_tx,
                        ActorLifecycle::Stopping,
                    );
                    drop(state);
                    return Ok(());
                }
            }
            command = command_rx.recv() => {
                let Some(command) = command else {
                    continue;
                };
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    state.handle(&command.envelope)
                }));
                match result {
                    Ok(reply) if state.fatal_error().is_none() => {
                        let _receiver_was_dropped = command.reply_tx.send(Ok(reply));
                    }
                    Ok(_reply) => {
                        let detail = state
                            .fatal_error()
                            .map_or_else(|| "durable runtime entered Fatal".to_owned(), ToString::to_string);
                        let _receiver_was_dropped = command.reply_tx.send(Err(ActorRequestError::Fatal));
                        stop_actor(&mut command_rx, &mut query_rx, &lifecycle_tx, ActorLifecycle::Fatal);
                        #[cfg(test)]
                        wait_before_fatal_drop(&mut fatal_drop_gate);
                        drop(state);
                        return Err(ActorThreadError::Fatal(detail));
                    }
                    Err(payload) => {
                        let detail = panic_detail(&*payload);
                        let _receiver_was_dropped = command.reply_tx.send(Err(ActorRequestError::Fatal));
                        stop_actor(&mut command_rx, &mut query_rx, &lifecycle_tx, ActorLifecycle::Fatal);
                        #[cfg(test)]
                        wait_before_fatal_drop(&mut fatal_drop_gate);
                        drop(state);
                        return Err(ActorThreadError::Panicked(detail));
                    }
                }
            }
            query = query_rx.recv() => {
                let Some(query) = query else {
                    continue;
                };
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    dispatch_query(&mut state, query);
                }));
                match result {
                    Ok(()) if state.fatal_error().is_none() => {}
                    Ok(()) => {
                        let detail = state
                            .fatal_error()
                            .map_or_else(|| "durable runtime entered Fatal".to_owned(), ToString::to_string);
                        stop_actor(&mut command_rx, &mut query_rx, &lifecycle_tx, ActorLifecycle::Fatal);
                        #[cfg(test)]
                        wait_before_fatal_drop(&mut fatal_drop_gate);
                        drop(state);
                        return Err(ActorThreadError::Fatal(detail));
                    }
                    Err(payload) => {
                        let detail = panic_detail(&*payload);
                        stop_actor(&mut command_rx, &mut query_rx, &lifecycle_tx, ActorLifecycle::Fatal);
                        #[cfg(test)]
                        wait_before_fatal_drop(&mut fatal_drop_gate);
                        drop(state);
                        return Err(ActorThreadError::Panicked(detail));
                    }
                }
            }
        }
    }
}

fn dispatch_query(state: &mut DurableServiceState, query: QueryRequest) {
    match query {
        QueryRequest::Protocol { envelope, reply_tx } => {
            let reply = state.handle(&envelope);
            let _receiver_was_dropped = reply_tx.send(Ok(reply));
        }
        QueryRequest::ArtifactDownload {
            project_id,
            hash,
            if_none_match,
            reply_tx,
        } => {
            let reply =
                state.resolve_artifact_download(&project_id, &hash, if_none_match.as_deref());
            let _receiver_was_dropped = reply_tx.send(Ok(reply));
        }
        #[cfg(test)]
        QueryRequest::PanicForTest { reply_tx } => {
            let _keep_reply_live = reply_tx;
            panic!("c3 actor panic probe");
        }
        #[cfg(test)]
        QueryRequest::ThreadIdForTest { reply_tx } => {
            let _receiver_was_dropped = reply_tx.send(Ok(thread::current().id()));
        }
    }
}

fn stop_actor(
    command_rx: &mut mpsc::Receiver<CommandRequest>,
    query_rx: &mut mpsc::Receiver<QueryRequest>,
    lifecycle_tx: &watch::Sender<ActorLifecycle>,
    lifecycle: ActorLifecycle,
) {
    lifecycle_tx.send_replace(lifecycle);
    command_rx.close();
    query_rx.close();
    while let Ok(command) = command_rx.try_recv() {
        let _receiver_was_dropped = command.reply_tx.send(Err(lifecycle.into()));
    }
    while let Ok(query) = query_rx.try_recv() {
        reject_query(query, lifecycle.into());
    }
}

fn reject_query(query: QueryRequest, error: ActorRequestError) {
    match query {
        QueryRequest::Protocol { reply_tx, .. } => {
            let _receiver_was_dropped = reply_tx.send(Err(error));
        }
        QueryRequest::ArtifactDownload { reply_tx, .. } => {
            let _receiver_was_dropped = reply_tx.send(Err(error));
        }
        #[cfg(test)]
        QueryRequest::PanicForTest { reply_tx } => {
            let _receiver_was_dropped = reply_tx.send(Err(error));
        }
        #[cfg(test)]
        QueryRequest::ThreadIdForTest { reply_tx } => {
            let _receiver_was_dropped = reply_tx.send(Err(error));
        }
    }
}

impl From<ActorLifecycle> for ActorRequestError {
    fn from(value: ActorLifecycle) -> Self {
        match value {
            ActorLifecycle::Running => Self::Closed,
            ActorLifecycle::Stopping => Self::Stopping,
            ActorLifecycle::Fatal => Self::Fatal,
        }
    }
}

fn panic_detail(payload: &(dyn Any + Send)) -> String {
    payload.downcast_ref::<&str>().map_or_else(
        || {
            payload
                .downcast_ref::<String>()
                .map_or_else(|| "non-string panic payload".to_owned(), Clone::clone)
        },
        |detail| (*detail).to_owned(),
    )
}

#[cfg(test)]
struct StartGate {
    release_rx: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
impl StartGate {
    fn wait(self) -> Result<(), std::sync::mpsc::RecvError> {
        self.release_rx.recv()
    }
}

#[cfg(test)]
struct StartGateRelease {
    release_tx: std::sync::mpsc::SyncSender<()>,
}

#[cfg(test)]
impl StartGateRelease {
    fn release(self) {
        let _actor_was_dropped = self.release_tx.send(());
    }
}

#[cfg(test)]
struct FatalDropGate {
    release_rx: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
struct FatalDropGateRelease {
    release_tx: std::sync::mpsc::SyncSender<()>,
}

#[cfg(test)]
impl FatalDropGateRelease {
    fn release(self) {
        let _actor_was_dropped = self.release_tx.send(());
    }
}

#[cfg(test)]
fn wait_before_fatal_drop(gate: &mut Option<FatalDropGate>) {
    if let Some(gate) = gate.take() {
        let _released = gate.release_rx.recv();
    }
}

impl fmt::Debug for DurableActorClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableActorClient")
            .field("lifecycle", &*self.lifecycle_rx.borrow())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for DurableActorHost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableActorHost")
            .field("lifecycle", &self.lifecycle())
            .field("joined", &self.join_handle.is_none())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use crate::durable_runtime::{DurableRuntimeError, RuntimeFailpoint};
    use crate::protocol::{
        ClientCommand, ClientCommandId, ClientId, CommandEnvelope, CommandOutcome, CommandResult,
        PROTOCOL_VERSION,
    };

    use super::*;

    fn private_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("创建 actor 测试目录");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))
            .expect("设置 actor 测试目录权限");
        root
    }

    fn capacities() -> (QueueCapacity, QueryQueueCapacity) {
        (
            QueueCapacity::new(1).expect("非零 command 容量"),
            QueryQueueCapacity::new(1).expect("非零 query 容量"),
        )
    }

    fn command(id: &str, command: ClientCommand) -> CommandEnvelope {
        CommandEnvelope {
            protocol_version: PROTOCOL_VERSION,
            client_id: ClientId("c3-actor-client".to_owned()),
            client_command_id: ClientCommandId(id.to_owned()),
            command,
        }
    }

    fn initialize(id: &str) -> CommandEnvelope {
        command(id, ClientCommand::Initialize)
    }

    fn project_create(id: &str) -> CommandEnvelope {
        command(
            id,
            ClientCommand::ProjectCreate {
                name: "C3 Actor".to_owned(),
            },
        )
    }

    async fn wait_for_lifecycle(
        lifecycle_rx: &mut watch::Receiver<ActorLifecycle>,
        expected: ActorLifecycle,
    ) {
        loop {
            if *lifecycle_rx.borrow() == expected {
                return;
            }
            lifecycle_rx
                .changed()
                .await
                .expect("生命周期发送端在发布终态后才释放");
        }
    }

    #[tokio::test]
    async fn c3_actor_lifecycle_capacity_one_is_immediately_overloaded_per_lane() {
        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("打开 durable runtime");
        let (command_capacity, query_capacity) = capacities();
        let (mut host, release) =
            DurableActorHost::start_paused_for_test(runtime, command_capacity, query_capacity)
                .expect("启动暂停 actor");
        let client = host.client();

        let pending_command = client
            .enqueue_command(project_create("capacity-command-first"))
            .expect("首个 command 占用容量");
        assert!(matches!(
            client.enqueue_command(project_create("capacity-command-second")),
            Err(ActorRequestError::Overloaded)
        ));
        let pending_query = client
            .enqueue_query(initialize("capacity-query-first"))
            .expect("首个 query 占用容量");
        assert!(matches!(
            client.enqueue_query(initialize("capacity-query-second")),
            Err(ActorRequestError::Overloaded)
        ));

        host.shutdown();
        release.release();
        assert_eq!(
            pending_command.wait().await,
            Err(ActorRequestError::Stopping)
        );
        assert_eq!(pending_query.wait().await, Err(ActorRequestError::Stopping));
        host.shutdown_and_join().expect("停止 actor");
    }

    #[tokio::test]
    async fn c3_actor_lifecycle_stopping_publishes_then_rejects_queued_and_new_work() {
        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("打开 durable runtime");
        let (command_capacity, query_capacity) = capacities();
        let (mut host, release) =
            DurableActorHost::start_paused_for_test(runtime, command_capacity, query_capacity)
                .expect("启动暂停 actor");
        let client = host.client();
        let mut lifecycle_rx = client.subscribe_lifecycle();
        let pending_command = client
            .enqueue_command(project_create("stopping-command"))
            .expect("command 已排队");
        let pending_query = client
            .enqueue_query(initialize("stopping-query"))
            .expect("query 已排队");

        host.shutdown();
        release.release();
        wait_for_lifecycle(&mut lifecycle_rx, ActorLifecycle::Stopping).await;
        assert_eq!(
            pending_command.wait().await,
            Err(ActorRequestError::Stopping)
        );
        assert_eq!(pending_query.wait().await, Err(ActorRequestError::Stopping));
        assert!(matches!(
            client.enqueue_query(initialize("stopping-new")),
            Err(ActorRequestError::Stopping)
        ));
        host.shutdown_and_join().expect("停止 actor");
    }

    #[tokio::test]
    async fn c3_actor_lifecycle_fatal_publishes_before_runtime_drop_and_fails_closed() {
        let root = private_root();
        let mut runtime = ReadyDurableRuntime::open(root.path()).expect("打开 durable runtime");
        runtime.set_failpoint(RuntimeFailpoint::CommitRecoverySync);
        let (command_capacity, query_capacity) = capacities();
        let (mut host, release) = DurableActorHost::start_with_fatal_drop_gate_for_test(
            runtime,
            command_capacity,
            query_capacity,
        )
        .expect("启动 fatal 顺序 actor");
        let client = host.client();
        let mut lifecycle_rx = client.subscribe_lifecycle();

        assert_eq!(
            client
                .execute_command(project_create("fatal-project"))
                .await,
            Err(ActorRequestError::Fatal)
        );
        wait_for_lifecycle(&mut lifecycle_rx, ActorLifecycle::Fatal).await;
        assert!(matches!(
            ReadyDurableRuntime::open(root.path()),
            Err(DurableRuntimeError::InstanceAlreadyRunning)
        ));
        assert!(matches!(
            client.enqueue_query(initialize("fatal-new-query")),
            Err(ActorRequestError::Fatal)
        ));

        release.release();
        assert!(matches!(
            host.shutdown_and_join(),
            Err(ActorJoinError::Fatal(_))
        ));
        let _reopened = ReadyDurableRuntime::open(root.path()).expect("fatal drop 后立即重开");
    }

    #[test]
    fn c3_actor_lifecycle_join_handle_is_consumed_exactly_once() {
        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("打开 durable runtime");
        let (command_capacity, query_capacity) = capacities();
        let mut host =
            DurableActorHost::start(runtime, command_capacity, query_capacity).expect("启动 actor");

        host.shutdown_and_join().expect("首次 join 成功");
        assert_eq!(host.shutdown_and_join(), Err(ActorJoinError::AlreadyJoined));
    }

    #[tokio::test]
    async fn c3_actor_lifecycle_actor_panic_is_reported_as_join_error() {
        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("打开 durable runtime");
        let (command_capacity, query_capacity) = capacities();
        let mut host =
            DurableActorHost::start(runtime, command_capacity, query_capacity).expect("启动 actor");
        let client = host.client();
        let mut lifecycle_rx = client.subscribe_lifecycle();

        let _request_error = client.panic_actor_for_test().await;
        wait_for_lifecycle(&mut lifecycle_rx, ActorLifecycle::Fatal).await;
        assert!(matches!(
            host.shutdown_and_join(),
            Err(ActorJoinError::Panicked(detail)) if detail == "c3 actor panic probe"
        ));
    }

    #[test]
    fn c3_actor_lifecycle_host_drop_joins_and_allows_immediate_reopen() {
        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("打开 durable runtime");
        let (command_capacity, query_capacity) = capacities();
        let host =
            DurableActorHost::start(runtime, command_capacity, query_capacity).expect("启动 actor");

        drop(host);
        let _reopened = ReadyDurableRuntime::open(root.path()).expect("host Drop 后立即重开");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn c3_actor_lifecycle_axum_runtime_never_executes_synchronous_store_io() {
        let caller_thread = thread::current().id();
        let root = private_root();
        let runtime = ReadyDurableRuntime::open(root.path()).expect("bind 前打开 durable runtime");
        let (command_capacity, query_capacity) = capacities();
        let mut host =
            DurableActorHost::start(runtime, command_capacity, query_capacity).expect("启动 actor");
        let client = host.client();

        let reply = client
            .execute_command(project_create("thread-boundary-project"))
            .await
            .expect("actor 完成同步 durable 写入");
        assert!(matches!(
            reply.outcome,
            CommandOutcome::Success {
                result: CommandResult::ProjectCreated(_)
            }
        ));
        let actor_thread = client
            .actor_thread_id_for_test()
            .await
            .expect("读取 actor 线程标识");
        assert_ne!(actor_thread, caller_thread);

        host.shutdown_and_join().expect("停止 actor");
    }
}
