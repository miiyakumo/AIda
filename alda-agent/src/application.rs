use crate::agent::{
    Agent, AgentReporter, AgentResultKind, CreationMode, ProjectPromptRequest,
    from_provider_messages,
};
use crate::alda::{AldaCheck, AldaRunner, CancellationToken, CheckStatus, find_alda};
use crate::command::{
    AldaAction, ConfigAction, ExportFormat, ProjectAction, ScoreTarget, UserAction, help,
};
use crate::config::ModelConfig;
use crate::conversation::{ConversationMessage, ConversationRole, ConversationState};
use crate::deepseek::{ChatError, DeepSeekClient};
use crate::project::{CheckRecord, Project, WorkingScoreKind};
use anyhow::{Context, Result, bail};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectView {
    pub name: String,
    pub first_request: Option<String>,
    pub current_version: Option<u32>,
    pub working_score: Option<String>,
    pub versions: Vec<VersionView>,
    pub mode: String,
    pub target_duration_secs: Option<f64>,
    pub included_instruments: Vec<String>,
    pub excluded_instruments: Vec<String>,
    pub creative_strategy: Option<String>,
    pub model_name: Option<String>,
    pub model_url: Option<String>,
    pub model_key_configured: bool,
    pub alda_available: bool,
    pub model_configured: bool,
    pub model_service_status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionView {
    pub version: u32,
    pub summary: String,
    pub checks: Vec<CheckRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationView {
    pub messages: Vec<ConversationMessage>,
    pub state: ConversationState,
    pub next_step: String,
}

#[derive(Debug, Clone)]
pub enum ActionResult {
    Message(String),
    Checks(Vec<AldaCheck>),
    AgentCompleted {
        kind: AgentResultKind,
        success: bool,
        rounds: usize,
        needs_input: bool,
    },
    Quit,
    None,
}

pub struct Application {
    project: Project,
    alda: Option<AldaRunner>,
    last_model_failure: Option<ModelFailure>,
    model_request_succeeded: bool,
    playback: Option<String>,
    privacy_shown: bool,
}

#[derive(Debug, Clone)]
enum ModelFailure {
    Auth(String),
    RateLimit(String),
    Network(String),
    Rejected(String),
}

impl ModelFailure {
    fn from_error(error: &anyhow::Error) -> Option<Self> {
        error.downcast_ref::<ChatError>().map(|error| match error {
            ChatError::Auth(message) => Self::Auth(message.clone()),
            ChatError::RateLimit(message) => Self::RateLimit(message.clone()),
            ChatError::Network(message) => Self::Network(message.clone()),
            ChatError::ModelReject(message) => Self::Rejected(message.clone()),
        })
    }

    fn status(&self) -> String {
        match self {
            Self::Auth(message) => format!("认证失败：{message}；请重新设置模型密钥"),
            Self::RateLimit(message) => format!("请求限流：{message}；稍后重试相同要求"),
            Self::Network(message) => format!("连接失败：{message}；请检查 API Base URL 或网络"),
            Self::Rejected(message) => format!("请求被模型拒绝：{message}"),
        }
    }
}

impl Application {
    pub fn open(root: PathBuf, name: &str) -> Result<Self> {
        let project = Project::load_or_create(root, name, "")?;
        let alda = find_alda().map(AldaRunner::new);
        Ok(Self {
            project,
            alda,
            last_model_failure: None,
            model_request_succeeded: false,
            playback: None,
            privacy_shown: false,
        })
    }

    #[must_use]
    pub fn from_project(project: Project, alda: Option<AldaRunner>) -> Self {
        Self {
            project,
            alda,
            last_model_failure: None,
            model_request_succeeded: false,
            playback: None,
            privacy_shown: false,
        }
    }

    pub fn set_cancellation(&mut self, cancellation: CancellationToken) {
        self.alda = self
            .alda
            .take()
            .map(|runner| runner.with_cancellation(cancellation));
    }

    #[must_use]
    pub fn project_view(&self) -> ProjectView {
        let model_config_result = ModelConfig::load(self.project.root());
        let model_config = model_config_result.as_ref().ok();
        let model_configured = model_config_result
            .as_ref()
            .ok()
            .and_then(|config| config.resolve().ok())
            .is_some();
        ProjectView {
            name: self.project.project_name.clone(),
            first_request: self.project.conversation().first_request().map(summarize),
            current_version: (self.project.current_version() > 0)
                .then(|| self.project.current_version()),
            working_score: self
                .project
                .working_score()
                .map(|working| working.kind.to_string()),
            versions: self
                .project
                .versions()
                .iter()
                .map(|(version, meta)| VersionView {
                    version: *version,
                    summary: meta.summary.clone(),
                    checks: meta.checks.clone(),
                })
                .collect(),
            mode: self.project.mode().to_string(),
            target_duration_secs: self.project.target_duration_secs(),
            included_instruments: self.project.included_instruments().to_vec(),
            excluded_instruments: self.project.excluded_instruments().to_vec(),
            creative_strategy: (!self.project.creative_strategy().is_empty())
                .then(|| self.project.creative_strategy().to_string()),
            model_name: model_config
                .and_then(|config| config.model())
                .map(ToString::to_string),
            model_url: model_config
                .and_then(|config| config.base_url())
                .map(ToString::to_string),
            model_key_configured: model_config.is_some_and(ModelConfig::has_api_key),
            alda_available: self.alda.is_some(),
            model_configured,
            model_service_status: self.last_model_failure.as_ref().map_or_else(
                || {
                    if self.model_request_succeeded {
                        "最近成功".to_string()
                    } else {
                        "未尝试".to_string()
                    }
                },
                ModelFailure::status,
            ),
        }
    }

    #[must_use]
    pub fn conversation_view(&self) -> ConversationView {
        let state = self.project.conversation().state();
        let config_error = ModelConfig::load(self.project.root())
            .and_then(|config| config.resolve())
            .err();
        let next_step = if let Some(label) = &self.playback {
            format!("播放已发起 · {label} · /alda stop")
        } else if config_error.is_some() {
            "仅本地 · 模型配置不可用 · /project config".to_string()
        } else if let Some(failure) = &self.last_model_failure {
            format!("模型服务最近失败 · {}", failure.status())
        } else if state == ConversationState::AwaitingInput {
            "等待补充信息 · 直接回答上面的问题".to_string()
        } else if state == ConversationState::RevisionAvailable {
            "修正未完成 · 输入“继续修正”或新的要求".to_string()
        } else if state == ConversationState::RequestPending {
            "上次请求未完成 · 重试原要求或输入补充".to_string()
        } else if let Some(working) = self.project.working_score() {
            match working.kind {
                WorkingScoreKind::Draft => {
                    "草稿待发展 · /alda play work · 继续输入创作要求".to_string()
                }
                WorkingScoreKind::Candidate => {
                    "完整候选待决定 · /alda play work · /project accept|discard".to_string()
                }
            }
        } else if self.project.current_version() == 0 {
            "新项目 · 描述作品或粘贴参考素材".to_string()
        } else {
            "就绪 · 输入修改要求 · /alda play · /help".to_string()
        };
        ConversationView {
            messages: self.project.conversation().messages().to_vec(),
            state,
            next_step,
        }
    }

    pub async fn execute(
        &mut self,
        action: UserAction,
        reporter: &mut impl AgentReporter,
    ) -> Result<ActionResult> {
        match action {
            UserAction::Empty => Ok(ActionResult::None),
            UserAction::Quit => Ok(ActionResult::Quit),
            UserAction::Help(path) => Ok(ActionResult::Message(help(&path))),
            UserAction::Agent(prompt) => self.execute_agent(prompt, reporter).await,
            UserAction::Alda(action) => self.execute_alda(action).await,
            UserAction::Project(action) => self.execute_project(action).await,
        }
    }

    async fn execute_agent(
        &mut self,
        prompt: String,
        reporter: &mut impl AgentReporter,
    ) -> Result<ActionResult> {
        self.project.prepare_user_message(&prompt)?;
        if !self.privacy_shown {
            reporter.report(crate::agent::AgentEvent::PrivacyNotice);
            self.privacy_shown = true;
        }
        let config =
            match ModelConfig::load(self.project.root()).and_then(|config| config.resolve()) {
                Ok(config) => config,
                Err(error) => return Err(error),
            };
        let runner = self
            .alda
            .clone()
            .ok_or_else(|| anyhow::anyhow!("未找到 alda；Agent 不能绕过校验保存版本"))?;
        let client = DeepSeekClient::new(config.api_key, config.base_url, config.model)?;
        let result = Agent::new(client, runner)
            .respond_with_reporter(
                ProjectPromptRequest {
                    conversation: self.project.conversation().messages().to_vec(),
                    current_alda: (self.project.current_version() > 0)
                        .then(|| self.project.current_code())
                        .transpose()?,
                    working_alda: self
                        .project
                        .working_score()
                        .map(|_| self.project.working_code())
                        .transpose()?,
                    creative_strategy: self.project.creative_strategy().to_string(),
                    mode: if self.project.mode() == "improv" {
                        CreationMode::Improvisation
                    } else {
                        CreationMode::FullPiece
                    },
                    target_duration_secs: self.project.target_duration_secs(),
                    included_instruments: self.project.included_instruments().to_vec(),
                    excluded_instruments: self.project.excluded_instruments().to_vec(),
                    max_rounds: 3,
                },
                reporter,
            )
            .await;
        let result = match result {
            Ok(result) => {
                self.last_model_failure = None;
                self.model_request_succeeded = true;
                result
            }
            Err(error) => {
                self.last_model_failure = ModelFailure::from_error(&error);
                return Err(error);
            }
        };
        if result.success {
            let kind = match result.kind {
                AgentResultKind::Draft => WorkingScoreKind::Draft,
                AgentResultKind::Candidate => WorkingScoreKind::Candidate,
                _ => bail!("文本结果不能标记为校验成功"),
            };
            self.project.save_working_score(
                result
                    .alda_code
                    .as_deref()
                    .context("成功结果缺少 Alda 代码")?,
                kind,
                &prompt,
                &result.checks,
            )?;
        }
        let state = if result.needs_input {
            ConversationState::AwaitingInput
        } else if result.success
            || matches!(result.kind, AgentResultKind::Answer | AgentResultKind::Plan)
        {
            ConversationState::Ready
        } else {
            ConversationState::RevisionAvailable
        };
        let mut messages = from_provider_messages(result.conversation);
        messages.retain(|message| message.role != ConversationRole::System);
        self.project.replace_conversation(messages, state)?;
        Ok(ActionResult::AgentCompleted {
            kind: result.kind,
            success: result.success,
            rounds: result.rounds,
            needs_input: result.needs_input,
        })
    }

    async fn execute_alda(&mut self, action: AldaAction) -> Result<ActionResult> {
        match action {
            AldaAction::Play(target) => {
                let (path, label) = match target {
                    ScoreTarget::Working => (self.project.working_path()?, "工作乐谱".to_string()),
                    ScoreTarget::Version(version) => {
                        let version = version.unwrap_or(self.require_current()?);
                        (
                            self.project.version_path_for(version)?,
                            format!("v{version}"),
                        )
                    }
                    ScoreTarget::File(_) => bail!("play 不支持外部文件"),
                };
                self.runner()?.play_async(path).await?;
                self.playback = Some(label.clone());
                Ok(ActionResult::Message(format!("✓ 已发起播放 {label}")))
            }
            AldaAction::Stop => {
                self.runner()?.stop_async().await?;
                self.playback = None;
                Ok(ActionResult::Message("✓ 已请求停止播放".to_string()))
            }
            AldaAction::Check(target) => {
                let target_duration_ms = if target == ScoreTarget::Working
                    && self
                        .project
                        .working_score()
                        .is_some_and(|working| working.kind == WorkingScoreKind::Draft)
                {
                    None
                } else {
                    self.project
                        .target_duration_secs()
                        .map(|value| value * 1000.0)
                };
                let path = self.score_path(target)?;
                let checks = self
                    .runner()?
                    .validate_async(
                        path,
                        self.project.included_instruments().to_vec(),
                        self.project.excluded_instruments().to_vec(),
                        target_duration_ms,
                        10.0,
                    )
                    .await?;
                Ok(ActionResult::Checks(checks))
            }
            AldaAction::Export { version, format } => {
                self.export(version.unwrap_or(self.require_current()?), format)
                    .await
            }
        }
    }

    async fn execute_project(&mut self, action: ProjectAction) -> Result<ActionResult> {
        match action {
            ProjectAction::Overview => {
                Ok(ActionResult::Message(render_project(&self.project_view())))
            }
            ProjectAction::Versions => {
                Ok(ActionResult::Message(render_versions(&self.project_view())))
            }
            ProjectAction::Switch(version) => {
                self.project.restore_version(version)?;
                Ok(ActionResult::Message(format!(
                    "✓ 已切换到 v{version}；后续历史未删除"
                )))
            }
            ProjectAction::Adopt(path) => {
                let runner = self.runner()?.clone();
                let code = std::fs::read_to_string(&path)
                    .with_context(|| format!("无法读取 {}", path.display()))?;
                let checks = runner
                    .validate_async(
                        path.clone(),
                        self.project.included_instruments().to_vec(),
                        self.project.excluded_instruments().to_vec(),
                        self.project
                            .target_duration_secs()
                            .map(|value| value * 1000.0),
                        10.0,
                    )
                    .await?;
                if checks.iter().any(|check| check.status == CheckStatus::Fail) {
                    return Ok(ActionResult::Checks(checks));
                }
                let version = self.project.save_version(
                    &code,
                    &format!("采用 {}", path.display()),
                    &checks,
                )?;
                Ok(ActionResult::Message(format!("✓ 已采用为 v{version}")))
            }
            ProjectAction::Accept => {
                match self.project.working_score() {
                    Some(working) if working.kind == WorkingScoreKind::Candidate => {}
                    Some(_) => bail!("当前工作乐谱是草稿，不能接受为有效版本"),
                    None => bail!("项目没有可接受的完整候选"),
                }
                let checks = self
                    .runner()?
                    .validate_async(
                        self.project.working_path()?,
                        self.project.included_instruments().to_vec(),
                        self.project.excluded_instruments().to_vec(),
                        self.project
                            .target_duration_secs()
                            .map(|value| value * 1000.0),
                        10.0,
                    )
                    .await?;
                self.project.update_working_checks(&checks)?;
                if checks.iter().any(|check| check.status == CheckStatus::Fail) {
                    return Ok(ActionResult::Checks(checks));
                }
                let version = self.project.accept_working_score()?;
                Ok(ActionResult::Message(format!(
                    "✓ 已接受完整候选为 v{version}"
                )))
            }
            ProjectAction::Discard => {
                self.project.discard_working_score()?;
                Ok(ActionResult::Message(
                    "✓ 已放弃工作乐谱；当前有效版本未改变".to_string(),
                ))
            }
            ProjectAction::Config(config) => self.configure(config),
        }
    }

    fn configure(&mut self, action: ConfigAction) -> Result<ActionResult> {
        if action == ConfigAction::Show {
            return Ok(ActionResult::Message(render_config(&self.project_view())));
        }
        let mut mode = self.project.mode().to_string();
        let mut duration = self.project.target_duration_secs();
        let mut include = self.project.included_instruments().to_vec();
        let mut exclude = self.project.excluded_instruments().to_vec();
        match action {
            ConfigAction::Mode(value) => mode = value,
            ConfigAction::Duration(value) => duration = value,
            ConfigAction::Include(value) => include = value,
            ConfigAction::Exclude(value) => exclude = value,
            ConfigAction::Strategy(value) => {
                self.project
                    .set_creative_strategy(value.as_deref().unwrap_or(""))?;
                return Ok(ActionResult::Message("✓ 已更新创作策略".to_string()));
            }
            ConfigAction::Model(value) => {
                return self.update_model_config(|config| config.set_model(&value));
            }
            ConfigAction::Url(value) => {
                return self.update_model_config(|config| config.set_base_url(&value));
            }
            ConfigAction::ApiKey(Some(value)) => {
                return self.update_model_config(|config| config.set_api_key(&value));
            }
            ConfigAction::ApiKey(None) => {
                bail!("模型密钥必须在交互终端中隐藏输入")
            }
            ConfigAction::Show => unreachable!(),
        }
        self.project.configure(&mode, duration, include, exclude)?;
        Ok(ActionResult::Message("✓ 已更新项目设置".to_string()))
    }

    fn update_model_config(
        &mut self,
        update: impl FnOnce(&mut ModelConfig) -> Result<()>,
    ) -> Result<ActionResult> {
        let mut config = ModelConfig::load(self.project.root())?;
        update(&mut config)?;
        config.save(self.project.root())?;
        self.last_model_failure = None;
        self.model_request_succeeded = false;
        Ok(ActionResult::Message("✓ 已更新项目模型配置".to_string()))
    }

    async fn export(&self, version: u32, format: ExportFormat) -> Result<ActionResult> {
        let mut paths = Vec::new();
        if matches!(format, ExportFormat::Alda | ExportFormat::All)
            || (format == ExportFormat::Midi && self.alda.is_none())
        {
            paths.push(
                self.project
                    .export_alda_version(version)?
                    .display()
                    .to_string(),
            );
        }
        if matches!(format, ExportFormat::Midi | ExportFormat::All) {
            let midi = match self.runner() {
                Ok(runner) => {
                    runner
                        .export_midi_async(
                            self.project.version_path_for(version)?,
                            self.project.midi_export_path_for(version)?,
                        )
                        .await
                }
                Err(error) => Err(error),
            };
            match midi {
                Ok(path) => paths.push(path.display().to_string()),
                Err(error) if matches!(format, ExportFormat::All | ExportFormat::Midi) => {
                    paths.push(format!("MIDI 未导出：{error}"));
                }
                Err(error) => return Err(error),
            }
        }
        Ok(ActionResult::Message(format!(
            "✓ 已导出 v{version}\n  {}",
            paths.join("\n  ")
        )))
    }

    fn score_path(&self, target: ScoreTarget) -> Result<PathBuf> {
        match target {
            ScoreTarget::Version(version) => self
                .project
                .version_path_for(version.unwrap_or(self.require_current()?)),
            ScoreTarget::Working => self.project.working_path(),
            ScoreTarget::File(path) => Ok(path),
        }
    }
    fn require_current(&self) -> Result<u32> {
        let version = self.project.current_version();
        if version == 0 {
            bail!("项目还没有有效版本；请先输入创作要求或 /project adopt PATH")
        }
        Ok(version)
    }
    fn runner(&self) -> Result<&AldaRunner> {
        self.alda.as_ref().ok_or_else(|| {
            anyhow::anyhow!("未找到 alda；Alda 源码仍可用 /alda export --format alda 导出")
        })
    }
}

fn summarize(value: &str) -> String {
    let mut chars = value.trim().chars();
    let summary = chars.by_ref().take(80).collect::<String>();
    if chars.next().is_some() {
        format!("{summary}…")
    } else {
        summary
    }
}
fn render_versions(view: &ProjectView) -> String {
    if view.versions.is_empty() {
        return "尚无有效版本".to_string();
    }
    view.versions
        .iter()
        .map(|item| {
            format!(
                "{} v{} · {}",
                if Some(item.version) == view.current_version {
                    "*"
                } else {
                    " "
                },
                item.version,
                item.summary
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn render_config(view: &ProjectView) -> String {
    format!(
        "模式：{}\n目标时长：{}\n包含乐器：{}\n排除乐器：{}\n创作策略：{}\n模型名称：{}\nAPI Base URL：{}\n模型密钥：{}",
        view.mode,
        view.target_duration_secs
            .map_or_else(|| "无".to_string(), |value| format!("{value} 秒")),
        empty(&view.included_instruments),
        empty(&view.excluded_instruments),
        view.creative_strategy.as_deref().unwrap_or("内置默认"),
        view.model_name.as_deref().unwrap_or("未设置"),
        view.model_url.as_deref().unwrap_or("未设置"),
        if view.model_key_configured {
            "已设置"
        } else {
            "未设置"
        }
    )
}
fn render_project(view: &ProjectView) -> String {
    format!(
        "项目：{}\n首次请求：{}\n当前版本：{}\n工作乐谱：{}\n{}\nAlda：{}\n模型配置：{}\n模型服务：{}",
        view.name,
        view.first_request.as_deref().unwrap_or("无"),
        view.current_version
            .map_or_else(|| "无".to_string(), |value| format!("v{value}")),
        view.working_score.as_deref().unwrap_or("无"),
        render_config(view),
        available(view.alda_available),
        available(view.model_configured),
        view.model_service_status
    )
}
fn empty(values: &[String]) -> String {
    if values.is_empty() {
        "无".to_string()
    } else {
        values.join("、")
    }
}
fn available(value: bool) -> &'static str {
    if value { "可用" } else { "不可用" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentEvent;
    use crate::test_support::{MockResponse, serve};

    struct Silent;
    impl AgentReporter for Silent {
        fn report(&mut self, _event: AgentEvent) {}
    }

    fn passing_checks() -> Vec<AldaCheck> {
        vec![AldaCheck {
            name: "Alda 语法",
            status: CheckStatus::Pass,
            detail: "解析成功".to_string(),
        }]
    }

    fn passing_runner() -> (tempfile::TempDir, AldaRunner) {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("alda");
        let json = r#"{"events":[{"offset":0,"duration":180000,"audible-duration":180000,"midi-note":60,"part":"piano"}],"parts":{"piano":{"name":"piano","stock-instrument":"midi-acoustic-grand-piano","tempo":120}}}"#;
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nif [ \"$1\" = parse ]; then printf '%s\\n' '{json}'; else exit 0; fi\n"
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
        (directory, AldaRunner::new(executable))
    }

    fn candidate_response() -> String {
        let arguments = serde_json::json!({
            "kind": "candidate",
            "message": "完整候选",
            "alda_code": "piano: c"
        })
        .to_string();
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": { "name": "submit_result", "arguments": arguments }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        format!("data: {chunk}\n\ndata: [DONE]\n")
    }

    #[test]
    fn model_settings_persist_and_render_without_secret() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("poem");
        let project = Project::load_or_create(root.clone(), "poem", "").unwrap();
        let mut application = Application::from_project(project, None);

        application
            .configure(ConfigAction::Model("example-model".to_string()))
            .unwrap();
        application
            .configure(ConfigAction::Url("https://api.example.com".to_string()))
            .unwrap();
        application
            .configure(ConfigAction::ApiKey(Some("secret-test-value".to_string())))
            .unwrap();

        let restarted = Application::open(root, "poem").unwrap();
        let rendered = render_config(&restarted.project_view());
        assert!(rendered.contains("模型名称：example-model"));
        assert!(rendered.contains("API Base URL：https://api.example.com"));
        assert!(rendered.contains("模型密钥：已设置"));
        assert!(!rendered.contains("secret-test-value"));
        assert!(restarted.project_view().model_configured);
    }

    #[test]
    fn service_failure_does_not_make_complete_configuration_invalid() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("poem");
        let project = Project::load_or_create(root, "poem", "").unwrap();
        let mut application = Application::from_project(project, None);
        application
            .configure(ConfigAction::Model("example-model".to_string()))
            .unwrap();
        application
            .configure(ConfigAction::Url("https://api.example.com".to_string()))
            .unwrap();
        application
            .configure(ConfigAction::ApiKey(Some("test-key".to_string())))
            .unwrap();
        application.last_model_failure = Some(ModelFailure::RateLimit("请稍后重试".to_string()));

        let view = application.project_view();
        assert!(view.model_configured);
        assert!(view.model_service_status.contains("限流"));
        assert!(
            application
                .conversation_view()
                .next_step
                .contains("稍后重试")
        );
    }

    #[tokio::test]
    async fn views_are_independent_and_midi_degrades_to_alda_export() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("poem");
        let mut project = Project::load_or_create(root.clone(), "poem", "").unwrap();
        project.add_user_message("创作一首机械感作品").unwrap();
        project
            .save_version("piano: c", "首次创作", &passing_checks())
            .unwrap();
        let mut application = Application::from_project(project, None);

        let project_view = application.project_view();
        assert_eq!(project_view.current_version, Some(1));
        assert_eq!(
            project_view.first_request.as_deref(),
            Some("创作一首机械感作品")
        );
        let conversation_view = application.conversation_view();
        assert_eq!(conversation_view.messages.len(), 1);
        assert_eq!(conversation_view.state, ConversationState::Ready);

        let result = application
            .execute(
                UserAction::Alda(AldaAction::Export {
                    version: None,
                    format: ExportFormat::Midi,
                }),
                &mut Silent,
            )
            .await
            .unwrap();
        let ActionResult::Message(message) = result else {
            panic!("expected export message");
        };
        assert!(message.contains("MIDI 未导出"));
        assert!(root.join("exports/version-0001.alda").is_file());
    }

    #[tokio::test]
    async fn user_message_is_persisted_before_agent_preconditions() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("poem");
        let project = Project::load_or_create(root.clone(), "poem", "").unwrap();
        let mut application = Application::from_project(project, None);
        let error = application
            .execute(UserAction::Agent("首次请求".to_string()), &mut Silent)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("模型配置不完整"));
        drop(application);
        let reloaded = Project::load_or_create(root, "ignored", "").unwrap();
        assert_eq!(reloaded.conversation().first_request(), Some("首次请求"));
        assert_eq!(
            reloaded.conversation().state(),
            ConversationState::RequestPending
        );
    }

    #[tokio::test]
    async fn explicit_accept_is_the_application_version_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("poem");
        let mut project = Project::load_or_create(root, "poem", "").unwrap();
        project
            .configure("full", Some(180.0), Vec::new(), Vec::new())
            .unwrap();
        let (_directory, runner) = passing_runner();
        let mut application = Application::from_project(project, Some(runner));
        let (base_url, _requests) = serve(vec![MockResponse::sse(candidate_response())]);
        application
            .configure(ConfigAction::Model("example-model".to_string()))
            .unwrap();
        application.configure(ConfigAction::Url(base_url)).unwrap();
        application
            .configure(ConfigAction::ApiKey(Some("test-key".to_string())))
            .unwrap();

        let result = application
            .execute(UserAction::Agent("完成整首作品".to_string()), &mut Silent)
            .await
            .unwrap();
        assert!(matches!(
            result,
            ActionResult::AgentCompleted {
                kind: AgentResultKind::Candidate,
                ..
            }
        ));
        assert_eq!(application.project_view().current_version, None);
        assert_eq!(
            application.project_view().working_score.as_deref(),
            Some("完整候选")
        );

        let result = application
            .execute(UserAction::Project(ProjectAction::Accept), &mut Silent)
            .await
            .unwrap();
        let ActionResult::Message(message) = result else {
            panic!("expected accept message");
        };
        assert!(message.contains("v1"));
        assert_eq!(application.project_view().current_version, Some(1));
        assert!(application.project_view().working_score.is_none());
    }
}
