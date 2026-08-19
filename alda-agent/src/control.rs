use crate::agent::{AgentEvent, AgentReporter, AgentResultKind};
use crate::application::{ActionResult, Application, ConversationView, ProjectView};
use crate::command::{
    AldaAction, ConfigAction, ExportFormat, ProjectAction, ScoreTarget, UserAction,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlRequest {
    id: String,
    action: ControlAction,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ControlAction {
    Agent {
        prompt: String,
    },
    AldaPlay {
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        section_id: Option<String>,
        #[serde(default = "default_play_context_secs")]
        context_secs: u32,
    },
    AldaStop,
    AldaCheck {
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        file: Option<PathBuf>,
    },
    AldaExport {
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        version: Option<u32>,
        #[serde(default)]
        format: ControlExportFormat,
    },
    ProjectOverview,
    ProjectInstructions,
    ProjectSkills,
    ProjectSkillEnable {
        id: String,
    },
    ProjectSkillDisable {
        id: String,
    },
    ProjectVersions,
    ProjectSwitch {
        version: u32,
    },
    ProjectAdopt {
        path: PathBuf,
    },
    ProjectAccept,
    ProjectDiscard,
    AgentMode {
        mode: String,
    },
    ConfigShow,
    ConfigMode {
        mode: String,
    },
    ConfigDuration {
        seconds: Option<f64>,
    },
    ConfigInclude {
        instruments: Vec<String>,
    },
    ConfigExclude {
        instruments: Vec<String>,
    },
    ConfigModel {
        model: String,
    },
    ConfigUrl {
        url: String,
    },
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ControlExportFormat {
    Alda,
    Midi,
    Wav,
    #[default]
    All,
}

impl ControlAction {
    fn into_user_action(self) -> Result<UserAction> {
        let action = match self {
            Self::Agent { prompt } => {
                if prompt.trim().is_empty() {
                    bail!("agent prompt 不能为空");
                }
                UserAction::Agent(prompt)
            }
            Self::AldaPlay {
                target,
                section_id,
                context_secs,
            } => match section_id {
                Some(section_id) => UserAction::Alda(AldaAction::PlaySection {
                    target: parse_play_target(target.as_deref())?,
                    section_id,
                    context_secs: context_secs.clamp(5, 15),
                }),
                None => UserAction::Alda(AldaAction::Play(parse_play_target(target.as_deref())?)),
            },
            Self::AldaStop => UserAction::Alda(AldaAction::Stop),
            Self::AldaCheck { target, file } => {
                let target = match (target.as_deref(), file) {
                    (Some(_), Some(_)) => bail!("alda_check 的 target 和 file 不能同时设置"),
                    (None, Some(path)) => ScoreTarget::File(path),
                    (target, None) => parse_score_target(target)?,
                };
                UserAction::Alda(AldaAction::Check(target))
            }
            Self::AldaExport {
                target,
                version,
                format,
            } => UserAction::Alda(AldaAction::Export {
                target: match (target.as_deref(), valid_optional_version(version)?) {
                    (Some(_), Some(_)) => bail!("alda_export 的 target 和 version 不能同时设置"),
                    (Some(target), None) => parse_score_target(Some(target))?,
                    (None, Some(version)) => ScoreTarget::Version(Some(version)),
                    (None, None) => ScoreTarget::Version(None),
                },
                format: match format {
                    ControlExportFormat::Alda => ExportFormat::Alda,
                    ControlExportFormat::Midi => ExportFormat::Midi,
                    ControlExportFormat::Wav => ExportFormat::Wav,
                    ControlExportFormat::All => ExportFormat::All,
                },
            }),
            Self::ProjectOverview => UserAction::Project(ProjectAction::Overview),
            Self::ProjectInstructions => UserAction::Project(ProjectAction::Instructions),
            Self::ProjectSkills => UserAction::Project(ProjectAction::Skills),
            Self::ProjectSkillEnable { id } => UserAction::Project(ProjectAction::SkillEnable(id)),
            Self::ProjectSkillDisable { id } => {
                UserAction::Project(ProjectAction::SkillDisable(id))
            }
            Self::ProjectVersions => UserAction::Project(ProjectAction::Versions),
            Self::ProjectSwitch { version } => {
                UserAction::Project(ProjectAction::Switch(valid_version(version)?))
            }
            Self::ProjectAdopt { path } => UserAction::Project(ProjectAction::Adopt(path)),
            Self::ProjectAccept => UserAction::Project(ProjectAction::Accept),
            Self::ProjectDiscard => UserAction::Project(ProjectAction::Discard),
            Self::AgentMode { mode } => UserAction::Project(ProjectAction::AgentMode(Some(mode))),
            Self::ConfigShow => UserAction::Project(ProjectAction::Config(ConfigAction::Show)),
            Self::ConfigMode { mode } => {
                UserAction::Project(ProjectAction::Config(ConfigAction::Mode(mode)))
            }
            Self::ConfigDuration { seconds } => {
                UserAction::Project(ProjectAction::Config(ConfigAction::Duration(seconds)))
            }
            Self::ConfigInclude { instruments } => {
                UserAction::Project(ProjectAction::Config(ConfigAction::Include(instruments)))
            }
            Self::ConfigExclude { instruments } => {
                UserAction::Project(ProjectAction::Config(ConfigAction::Exclude(instruments)))
            }
            Self::ConfigModel { model } => {
                UserAction::Project(ProjectAction::Config(ConfigAction::Model(model)))
            }
            Self::ConfigUrl { url } => {
                UserAction::Project(ProjectAction::Config(ConfigAction::Url(url)))
            }
        };
        Ok(action)
    }
}

fn default_play_context_secs() -> u32 {
    10
}

fn valid_version(version: u32) -> Result<u32> {
    if version == 0 {
        bail!("version 必须是正整数");
    }
    Ok(version)
}

fn valid_optional_version(version: Option<u32>) -> Result<Option<u32>> {
    version.map(valid_version).transpose()
}

fn parse_play_target(target: Option<&str>) -> Result<ScoreTarget> {
    let target = parse_score_target(target)?;
    if matches!(target, ScoreTarget::Working | ScoreTarget::Version(_)) {
        Ok(target)
    } else {
        bail!("alda_play 不支持外部文件");
    }
}

fn parse_score_target(target: Option<&str>) -> Result<ScoreTarget> {
    match target.unwrap_or("current") {
        "current" => Ok(ScoreTarget::Version(None)),
        "work" => Ok(ScoreTarget::Working),
        value => {
            let version = value.strip_prefix('v').unwrap_or(value);
            let version = version
                .parse::<u32>()
                .ok()
                .and_then(|value| (value > 0).then_some(value))
                .with_context(|| "target 必须是 current、work 或 vN")?;
            Ok(ScoreTarget::Version(Some(version)))
        }
    }
}

#[derive(Serialize)]
struct ControlEnvelope<'a, T: Serialize> {
    id: Option<&'a str>,
    #[serde(flatten)]
    body: T,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ControlResponse<'a> {
    Result {
        result: ControlResult<'a>,
        project: &'a ProjectView,
        conversation: &'a ConversationView,
    },
    Error {
        error: ControlError,
        project: &'a ProjectView,
        conversation: &'a ConversationView,
    },
}

#[derive(Serialize)]
struct ControlError {
    kind: &'static str,
    message: String,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ControlResult<'a> {
    Message {
        message: &'a str,
    },
    Checks {
        checks: &'a [crate::alda::AldaCheck],
    },
    AgentCompleted {
        result_kind: &'static str,
        success: bool,
        rounds: usize,
        stats: crate::agent::GenerationStats,
        needs_input: bool,
        recovery_checkpoint: Option<crate::agent::RecoveryCheckpoint>,
        working_score_changed: bool,
        working_score_status: &'a str,
    },
    Quit,
    None,
}

impl<'a> From<&'a ActionResult> for ControlResult<'a> {
    fn from(result: &'a ActionResult) -> Self {
        match result {
            ActionResult::Message(message) => Self::Message { message },
            ActionResult::Checks(checks) => Self::Checks { checks },
            ActionResult::AgentCompleted {
                kind,
                success,
                rounds,
                stats,
                needs_input,
                recovery_checkpoint,
                working_score_changed,
                working_score_status,
            } => Self::AgentCompleted {
                result_kind: agent_result_kind(*kind),
                success: *success,
                rounds: *rounds,
                stats: *stats,
                needs_input: *needs_input,
                recovery_checkpoint: *recovery_checkpoint,
                working_score_changed: *working_score_changed,
                working_score_status,
            },
            ActionResult::Quit => Self::Quit,
            ActionResult::None => Self::None,
        }
    }
}

fn agent_result_kind(kind: AgentResultKind) -> &'static str {
    match kind {
        AgentResultKind::Answer => "answer",
        AgentResultKind::Clarification => "clarification",
        AgentResultKind::Plan => "plan",
        AgentResultKind::Draft => "draft",
        AgentResultKind::Candidate => "candidate",
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ControlEvent<'a> {
    Event {
        #[serde(flatten)]
        event: EventBody<'a>,
    },
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum EventBody<'a> {
    PrivacyNotice,
    RoundStarted {
        attempt: usize,
    },
    ToolContinuationStarted {
        turn: usize,
    },
    ToolProtocolRetry {
        call_count: usize,
    },
    ToolCallMissingRetry,
    ToolArgumentsRetry {
        tool_name: &'a str,
    },
    ModelText {
        text: &'a str,
    },
    ValidationStarted {
        attempt: usize,
    },
    ValidationCompleted {
        checks: &'a [crate::alda::AldaCheck],
    },
    RevisionStarted {
        next_attempt: usize,
        failures: usize,
    },
}

struct ControlReporter<'a, W: Write> {
    id: &'a str,
    writer: &'a mut W,
    write_error: Option<anyhow::Error>,
}

impl<W: Write> AgentReporter for ControlReporter<'_, W> {
    fn report(&mut self, event: AgentEvent) {
        if self.write_error.is_some() {
            return;
        }
        let event = match &event {
            AgentEvent::PrivacyNotice => EventBody::PrivacyNotice,
            AgentEvent::RoundStarted { attempt } => EventBody::RoundStarted { attempt: *attempt },
            AgentEvent::ToolContinuationStarted { turn } => {
                EventBody::ToolContinuationStarted { turn: *turn }
            }
            AgentEvent::ToolProtocolRetry { call_count } => EventBody::ToolProtocolRetry {
                call_count: *call_count,
            },
            AgentEvent::ToolCallMissingRetry => EventBody::ToolCallMissingRetry,
            AgentEvent::ToolArgumentsRetry { tool_name } => {
                EventBody::ToolArgumentsRetry { tool_name }
            }
            AgentEvent::ModelText(text) => EventBody::ModelText { text },
            AgentEvent::ValidationStarted { attempt } => {
                EventBody::ValidationStarted { attempt: *attempt }
            }
            AgentEvent::ValidationCompleted(checks) => EventBody::ValidationCompleted { checks },
            AgentEvent::RevisionStarted {
                next_attempt,
                failures,
            } => EventBody::RevisionStarted {
                next_attempt: *next_attempt,
                failures: *failures,
            },
        };
        let envelope = ControlEnvelope {
            id: Some(self.id),
            body: ControlEvent::Event { event },
        };
        if let Err(error) = write_json_line(self.writer, &envelope) {
            self.write_error = Some(error);
        }
    }
}

pub async fn run(project_dir: PathBuf, name: String) -> Result<()> {
    let mut application = Application::open(project_dir, &name)?;
    run_io(
        &mut application,
        std::io::stdin().lock(),
        std::io::stdout().lock(),
    )
    .await
}

pub async fn run_io<R: BufRead, W: Write>(
    application: &mut Application,
    reader: R,
    mut writer: W,
) -> Result<()> {
    for line in reader.lines() {
        let line = line.context("无法读取 control 请求")?;
        if line.trim().is_empty() {
            continue;
        }
        let request = match serde_json::from_str::<ControlRequest>(&line) {
            Ok(request) => request,
            Err(error) => {
                let id = request_id(&line);
                write_error_response(
                    application,
                    &mut writer,
                    id.as_deref(),
                    "invalid_request",
                    error.to_string(),
                )?;
                continue;
            }
        };
        let action = match request.action.into_user_action() {
            Ok(action) => action,
            Err(error) => {
                write_error_response(
                    application,
                    &mut writer,
                    Some(&request.id),
                    "invalid_action",
                    format!("{error:#}"),
                )?;
                continue;
            }
        };
        let result = {
            let mut reporter = ControlReporter {
                id: &request.id,
                writer: &mut writer,
                write_error: None,
            };
            let result = application.execute(action, &mut reporter).await;
            if let Some(error) = reporter.write_error {
                return Err(error);
            }
            result
        };
        match result {
            Ok(result) => {
                let project = application.project_view();
                let conversation = application.conversation_view();
                let envelope = ControlEnvelope {
                    id: Some(&request.id),
                    body: ControlResponse::Result {
                        result: (&result).into(),
                        project: &project,
                        conversation: &conversation,
                    },
                };
                write_json_line(&mut writer, &envelope)?;
            }
            Err(error) => write_error_response(
                application,
                &mut writer,
                Some(&request.id),
                "execution",
                format!("{error:#}"),
            )?,
        }
    }
    Ok(())
}

fn request_id(line: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()?
        .get("id")?
        .as_str()
        .map(ToString::to_string)
}

fn write_error_response(
    application: &Application,
    writer: &mut impl Write,
    id: Option<&str>,
    kind: &'static str,
    message: String,
) -> Result<()> {
    let project = application.project_view();
    let conversation = application.conversation_view();
    write_json_line(
        writer,
        &ControlEnvelope {
            id,
            body: ControlResponse::Error {
                error: ControlError { kind, message },
                project: &project,
                conversation: &conversation,
            },
        },
    )
}

fn write_json_line(writer: &mut impl Write, value: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *writer, value).context("无法序列化 control 响应")?;
    writeln!(writer).context("无法写入 control 响应")?;
    writer.flush().context("无法刷新 control 响应")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Project;

    #[test]
    fn agent_completed_json_contains_generation_stats() {
        let action = ActionResult::AgentCompleted {
            kind: AgentResultKind::Candidate,
            success: false,
            rounds: 0,
            stats: crate::agent::GenerationStats {
                model_calls: 24,
                tool_turns: 19,
                protocol_recoveries: 5,
                submissions: 0,
            },
            needs_input: false,
            recovery_checkpoint: Some(crate::agent::RecoveryCheckpoint::InspectedCandidate),
            working_score_changed: false,
            working_score_status: "当前没有工作乐谱".to_string(),
        };

        let json = serde_json::to_value(ControlResult::from(&action)).unwrap();

        assert_eq!(json["kind"], "agent_completed");
        assert_eq!(json["recovery_checkpoint"], "inspected_candidate");
        assert_eq!(json["stats"]["model_calls"], 24);
        assert_eq!(json["stats"]["tool_turns"], 19);
        assert_eq!(json["stats"]["protocol_recoveries"], 5);
        assert_eq!(json["stats"]["submissions"], 0);
    }

    #[tokio::test]
    async fn processes_requests_and_continues_after_errors() {
        let directory = tempfile::tempdir().unwrap();
        let project_root = directory.path().join("control-test");
        let skill_root = project_root.join("skills/phrasing");
        std::fs::create_dir_all(&skill_root).unwrap();
        std::fs::write(
            skill_root.join("SKILL.md"),
            "---\nname: phrasing\ndescription: 乐句建议\nkind: advisory\n---\n让乐句有清晰呼吸。\n",
        )
        .unwrap();
        let project = Project::load_or_create(project_root, "control-test", "").unwrap();
        let mut application = Application::from_project(project, None);
        let input = concat!(
            "not-json\n",
            r#"{"id":"one","action":{"type":"config_duration","seconds":90.0}}"#,
            "\n",
            r#"{"id":"two","action":{"type":"project_overview"}}"#,
            "\n",
            r#"{"id":"three","action":{"type":"project_switch","version":0}}"#,
            "\n",
            r#"{"id":"four","action":{"type":"project_skills"}}"#,
            "\n",
            r#"{"id":"five","action":{"type":"project_skill_enable","id":"project:phrasing"}}"#,
            "\n",
            r#"{"id":"six","action":{"type":"project_instructions"}}"#,
            "\n"
        );
        let mut output = Vec::new();
        run_io(&mut application, input.as_bytes(), &mut output)
            .await
            .unwrap();

        let responses = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(responses.len(), 7);
        assert_eq!(responses[0]["type"], "error");
        assert!(responses[0]["id"].is_null());
        assert_eq!(responses[1]["id"], "one");
        assert_eq!(responses[1]["project"]["target_duration_secs"], 90.0);
        assert_eq!(responses[2]["result"]["kind"], "message");
        assert_eq!(responses[3]["error"]["kind"], "invalid_action");
        assert!(
            responses[4]["result"]["message"]
                .as_str()
                .unwrap()
                .contains("project:phrasing")
        );
        assert_eq!(
            responses[5]["project"]["enabled_advisory_skills"][0],
            "project:phrasing"
        );
        assert!(
            responses[6]["result"]["message"]
                .as_str()
                .unwrap()
                .contains("Fingerprint")
        );
    }

    #[test]
    fn rejects_api_key_actions() {
        assert!(
            serde_json::from_str::<ControlRequest>(
                r#"{"id":"1","action":{"type":"config_api_key","key":"secret"}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn reporter_emits_correlated_structured_events() {
        let mut output = Vec::new();
        let mut reporter = ControlReporter {
            id: "request-7",
            writer: &mut output,
            write_error: None,
        };
        reporter.report(AgentEvent::RoundStarted { attempt: 1 });
        reporter.report(AgentEvent::ModelText("正在发展主题".to_string()));
        assert!(reporter.write_error.is_none());
        drop(reporter);

        let events = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(events[0]["id"], "request-7");
        assert_eq!(events[0]["type"], "event");
        assert_eq!(events[0]["event"], "round_started");
        assert_eq!(events[0]["attempt"], 1);
        assert_eq!(events[1]["event"], "model_text");
        assert_eq!(events[1]["text"], "正在发展主题");
    }
}
