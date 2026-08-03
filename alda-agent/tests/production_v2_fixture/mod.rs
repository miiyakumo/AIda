//! 仅由 production v2 集成测试 target 编译的 restart obligation 准备器。

use std::path::Path;

use crate::control_store::PrepareControlRequest;
use crate::durable_runtime::{ReadyDurableRuntime, SubmitFailure};
use crate::protocol::{
    ChoiceId, ClientCommand, ClientCommandId, ClientId, CommandResult, PROTOCOL_VERSION, ProjectId,
    QuestionChoice, QuestionId, SessionId, TurnId, TurnSnapshot, TurnStatus,
};
use crate::state_store::session::SessionRolloutEvent;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Fixture {
    pub(crate) running_session_id: alda_agent::protocol::SessionId,
    pub(crate) running_turn_id: alda_agent::protocol::TurnId,
    pub(crate) cancel_requested_session_id: alda_agent::protocol::SessionId,
    pub(crate) cancel_requested_turn_id: alda_agent::protocol::TurnId,
    pub(crate) cancel_requested_question_id: alda_agent::protocol::QuestionId,
}

pub(crate) fn prepare(root_path: &Path, project_id: &str) -> Result<Fixture, String> {
    let project_id = ProjectId(project_id.to_owned());
    let client_id = ClientId("c3-production-restart-fixture".to_owned());
    let mut runtime = ReadyDurableRuntime::open(root_path).map_err(|error| error.to_string())?;

    let (next, running_session_id) = prepare_session(
        runtime,
        &client_id,
        "c3-restart-running-session",
        &project_id,
    )?;
    runtime = next;
    let running_turn_id = TurnId("turn-c3-restart-running".to_owned());
    runtime = prepare_session_state(
        runtime,
        &client_id,
        "c3-restart-running-state",
        &running_session_id,
        &project_id,
        &running_turn_id,
        TurnStatus::Running,
        vec![SessionRolloutEvent::TurnStarted {
            turn_id: running_turn_id.clone(),
            canonical_prompt: "等待 production restart abort".to_owned(),
        }],
    )?;

    let (next, cancel_requested_session_id) = prepare_session(
        runtime,
        &client_id,
        "c3-restart-cancel-session",
        &project_id,
    )?;
    runtime = next;
    let cancel_requested_turn_id = TurnId("turn-c3-restart-cancel".to_owned());
    let cancel_requested_question_id = QuestionId("question-c3-restart-cancel".to_owned());
    runtime = prepare_session_state(
        runtime,
        &client_id,
        "c3-restart-cancel-state",
        &cancel_requested_session_id,
        &project_id,
        &cancel_requested_turn_id,
        TurnStatus::CancelRequested,
        vec![
            SessionRolloutEvent::TurnStarted {
                turn_id: cancel_requested_turn_id.clone(),
                canonical_prompt: "等待 production restart cancel".to_owned(),
            },
            SessionRolloutEvent::QuestionRequested {
                question_id: cancel_requested_question_id.clone(),
                session_id: cancel_requested_session_id.clone(),
                owner_turn_id: cancel_requested_turn_id.clone(),
                prompt: "取消前的待处理问题".to_owned(),
                choices: vec![QuestionChoice {
                    choice_id: ChoiceId("acknowledge".to_owned()),
                    label: "确认".to_owned(),
                }],
            },
            SessionRolloutEvent::TurnCancelRequested {
                turn_id: cancel_requested_turn_id.clone(),
            },
        ],
    )?;
    drop(runtime);

    Ok(Fixture {
        running_session_id: alda_agent::protocol::SessionId(running_session_id.0),
        running_turn_id: alda_agent::protocol::TurnId(running_turn_id.0),
        cancel_requested_session_id: alda_agent::protocol::SessionId(cancel_requested_session_id.0),
        cancel_requested_turn_id: alda_agent::protocol::TurnId(cancel_requested_turn_id.0),
        cancel_requested_question_id: alda_agent::protocol::QuestionId(
            cancel_requested_question_id.0,
        ),
    })
}

fn prepare_session(
    runtime: ReadyDurableRuntime,
    client_id: &ClientId,
    command_id: &str,
    project_id: &ProjectId,
) -> Result<(ReadyDurableRuntime, SessionId), String> {
    let client_command_id = ClientCommandId(command_id.to_owned());
    let command = ClientCommand::SessionStart {
        project_id: project_id.clone(),
    };
    let payload_digest =
        crate::protocol::external_command_payload_digest(PROTOCOL_VERSION, &command)
            .map_err(|error| error.to_string())?;
    let request = runtime
        .plan_session_start(client_id, &client_command_id, &payload_digest, project_id)
        .map_err(|error| error.to_string())?;
    let session_id = request
        .session_allocation
        .as_ref()
        .ok_or_else(|| "C3 restart fixture 缺少 Session allocation".to_owned())?
        .session_id
        .clone();
    submit(runtime, request).map(|runtime| (runtime, session_id))
}

#[allow(
    clippy::too_many_arguments,
    reason = "fixture 必须显式绑定命令 identity、Session owner、Turn 状态与完整事件 batch"
)]
fn prepare_session_state(
    runtime: ReadyDurableRuntime,
    client_id: &ClientId,
    command_id: &str,
    session_id: &SessionId,
    project_id: &ProjectId,
    turn_id: &TurnId,
    status: TurnStatus,
    events: Vec<SessionRolloutEvent>,
) -> Result<ReadyDurableRuntime, String> {
    let client_command_id = ClientCommandId(command_id.to_owned());
    let command = ClientCommand::TurnStart {
        session_id: session_id.clone(),
        prompt: "C3 production restart fixture".to_owned(),
    };
    let payload_digest =
        crate::protocol::external_command_payload_digest(PROTOCOL_VERSION, &command)
            .map_err(|error| error.to_string())?;
    let head = runtime
        .session_head(session_id)
        .ok_or_else(|| "C3 restart fixture 找不到 Session head".to_owned())?;
    let reply = crate::protocol::CommandReply::success(
        client_command_id.clone(),
        CommandResult::TurnStarted(TurnSnapshot {
            turn_id: turn_id.clone(),
            status,
            terminal_sequence: None,
        }),
    );
    let request = runtime
        .session_only_prepare(
            client_id,
            &client_command_id,
            &payload_digest,
            command,
            session_id,
            project_id,
            &head,
            &reply,
            events,
            None,
            "C3 production restart fixture",
        )
        .map_err(|error| error.to_string())?;
    submit(runtime, request)
}

fn submit(
    runtime: ReadyDurableRuntime,
    request: PrepareControlRequest,
) -> Result<ReadyDurableRuntime, String> {
    match runtime.submit(request) {
        Ok((runtime, _reply)) => Ok(runtime),
        Err(SubmitFailure::Rejected { error, .. } | SubmitFailure::Recovering { error, .. }) => {
            Err(error.to_string())
        }
        Err(SubmitFailure::Fatal(runtime)) => Err(runtime.error().to_string()),
    }
}
