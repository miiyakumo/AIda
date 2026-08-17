use crate::agent::{
    Agent, AgentReporter, AgentResultKind, AgentToolContext, CreationRequest, CreationResult,
    ProjectPromptRequest,
};
use crate::alda::{AldaCheck, AldaRunner, CancellationToken, CheckStatus, find_alda};
use crate::audio::AudioRenderer;
use crate::command::{
    AldaAction, ConfigAction, ExportFormat, ProjectAction, ScoreTarget, UserAction, help,
};
use crate::config::ModelConfig;
use crate::conversation::{ConversationMessage, ConversationState};
use crate::deepseek::{ChatError, DeepSeekClient};
use crate::instructions::{CompiledInstructions, DurationConstraint, ProjectPreferences};
use crate::project::{CheckRecord, Project, WorkingScoreKind};
use crate::skills::{QualifiedSkillId, SkillCatalog, SkillKind};
use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Serialize;
use std::path::PathBuf;

pub struct ComposeRequest {
    pub project_root: PathBuf,
    pub project_name: String,
    pub source_material: String,
    pub preferences: ProjectPreferences,
    pub max_rounds: usize,
}

pub struct PreparedCompose {
    agent: Agent,
    request: CreationRequest,
}

pub fn prepare_compose(request: ComposeRequest) -> Result<PreparedCompose> {
    if request.source_material.trim().is_empty() {
        bail!("素材不能为空");
    }
    let preferences = request.preferences.normalized();
    preferences.validate()?;
    let config = ModelConfig::load(&request.project_root)?.resolve()?;
    let client = DeepSeekClient::new(config.api_key, config.base_url, config.model)?;
    let alda_path = find_alda().ok_or_else(|| anyhow::anyhow!("未找到 alda，请先安装"))?;
    let project = Project::load_or_create(
        request.project_root,
        &request.project_name,
        &request.source_material,
    )?;
    let user_root = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".alda-agent").join("skills"));
    let catalog =
        SkillCatalog::discover(user_root.as_deref(), Some(&project.root().join("skills")))?;
    let compiled_instructions =
        CompiledInstructions::compile(&catalog, project.instruction_profile(), &preferences)?;
    Ok(PreparedCompose {
        agent: Agent::new(client, AldaRunner::new(alda_path)),
        request: CreationRequest {
            source_material: request.source_material,
            instructions: String::new(),
            compiled_instructions,
            max_rounds: request.max_rounds,
        },
    })
}

pub async fn compose_once(prepared: PreparedCompose) -> Result<CreationResult> {
    prepared.agent.create(prepared.request).await
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProjectView {
    pub name: String,
    pub first_request: Option<String>,
    pub current_version: Option<u32>,
    pub working_score: Option<String>,
    pub versions: Vec<VersionView>,
    pub mode: String,
    pub target_duration_secs: Option<DurationConstraint>,
    pub included_instruments: Vec<String>,
    pub excluded_instruments: Vec<String>,
    pub enabled_advisory_skills: Vec<String>,
    pub model_name: Option<String>,
    pub model_url: Option<String>,
    pub model_key_configured: bool,
    pub alda_available: bool,
    pub model_configured: bool,
    pub model_service_status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VersionView {
    pub version: u32,
    pub summary: String,
    pub checks: Vec<CheckRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
        working_score_changed: bool,
        working_score_status: String,
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
            enabled_advisory_skills: self
                .project
                .instruction_profile()
                .enabled_advisory_skills
                .iter()
                .map(ToString::to_string)
                .collect(),
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

    #[allow(clippy::too_many_lines)]
    async fn execute_agent(
        &mut self,
        prompt: String,
        reporter: &mut impl AgentReporter,
    ) -> Result<ActionResult> {
        let config = ModelConfig::load(self.project.root()).and_then(|config| config.resolve())?;
        let answered_clarification =
            self.project.conversation().state() == ConversationState::AwaitingInput;
        let require_candidate = requests_complete_candidate(&prompt)
            || self.project.conversation().pending_candidate()
            || self
                .project
                .pending_revision()
                .is_some_and(|revision| revision.kind == WorkingScoreKind::Candidate);
        self.project
            .prepare_user_message_with_requirement(&prompt, require_candidate)?;
        if let Some(duration) = explicit_duration_secs(&prompt) {
            let mut preferences = self.project.preferences().clone();
            preferences.target_duration_secs = Some(duration);
            self.project.configure(&preferences)?;
        }
        let previous_working_code = self
            .project
            .working_score()
            .map(|_| self.project.working_code())
            .transpose()?;
        if !self.privacy_shown {
            reporter.report(crate::agent::AgentEvent::PrivacyNotice);
            self.privacy_shown = true;
        }
        let runner = self
            .alda
            .clone()
            .ok_or_else(|| anyhow::anyhow!("未找到 alda；Agent 不能绕过校验保存版本"))?;
        let client = DeepSeekClient::new(config.api_key, config.base_url, config.model)?;
        let compiled_instructions = self.compile_instructions()?;
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
                    revision_alda: self
                        .project
                        .pending_revision()
                        .map(|_| self.project.revision_code())
                        .transpose()?,
                    compiled_instructions,
                    max_rounds: 3,
                    tool_context: Some(AgentToolContext {
                        project_root: self.project.root().to_path_buf(),
                        current_path: (self.project.current_version() > 0)
                            .then(|| self.project.current_version_path())
                            .transpose()?,
                        working_path: self
                            .project
                            .working_score()
                            .map(|_| self.project.working_path())
                            .transpose()?,
                    }),
                    require_candidate,
                    forbid_clarification: answered_clarification,
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
                &result.interpretation,
                &result.checks,
            )?;
        } else if let Some(alda_code) = result.alda_code.as_deref() {
            let kind = match result.kind {
                AgentResultKind::Draft => WorkingScoreKind::Draft,
                AgentResultKind::Candidate => WorkingScoreKind::Candidate,
                _ => bail!("文本结果不能保存为待修正候选"),
            };
            self.project.save_pending_revision(
                alda_code,
                kind,
                &result.interpretation,
                &result.checks,
            )?;
        }
        let working_score_changed = result.success
            && self.project.working_score().is_some()
            && previous_working_code.as_deref() != self.project.working_code().ok().as_deref();
        if let Some(target) = &result.played_target {
            self.playback = Some(agent_playback_label(target, working_score_changed));
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
        self.project
            .finish_agent_turn(result.interpretation.clone(), state)?;
        Ok(ActionResult::AgentCompleted {
            kind: result.kind,
            success: result.success,
            rounds: result.rounds,
            needs_input: result.needs_input,
            working_score_changed,
            working_score_status: render_working_status(&self.project),
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
                let check_duration = !(target == ScoreTarget::Working
                    && self
                        .project
                        .working_score()
                        .is_some_and(|working| working.kind == WorkingScoreKind::Draft));
                let path = self.score_path(target)?;
                let checks = self
                    .runner()?
                    .validate_async(
                        path,
                        self.project.preferences().score_validation(check_duration),
                    )
                    .await?;
                Ok(ActionResult::Checks(checks))
            }
            AldaAction::Export { target, format } => self.export(target, format).await,
        }
    }

    async fn execute_project(&mut self, action: ProjectAction) -> Result<ActionResult> {
        match action {
            ProjectAction::Overview => {
                Ok(ActionResult::Message(render_project(&self.project_view())))
            }
            ProjectAction::Instructions => Ok(ActionResult::Message(
                self.compile_instructions()?.summary().to_string(),
            )),
            ProjectAction::Skills => Ok(ActionResult::Message(self.render_skills()?)),
            ProjectAction::SkillEnable(value) => self.enable_advisory_skill(&value),
            ProjectAction::SkillDisable(value) => self.disable_advisory_skill(&value),
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
                        self.project.preferences().score_validation(true),
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
                        self.project.preferences().score_validation(true),
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
        let mut preferences = self.project.preferences().clone();
        match action {
            ConfigAction::Mode(value) => preferences.mode = value.parse()?,
            ConfigAction::Duration(value) => {
                preferences.target_duration_secs = value.map(DurationConstraint::exact);
            }
            ConfigAction::Include(value) => preferences.included_instruments = value,
            ConfigAction::Exclude(value) => preferences.excluded_instruments = value,
            ConfigAction::Model(value) => {
                return self.update_model_config(|config| config.set_model(&value));
            }
            ConfigAction::PromptModel => {
                return Ok(ActionResult::Message(
                    "请输入模型名称；交互终端会立即读取该值，也可使用 /project config model MODEL_NAME"
                        .to_string(),
                ));
            }
            ConfigAction::Url(value) => {
                return self.update_model_config(|config| config.set_base_url(&value));
            }
            ConfigAction::PromptUrl => {
                return Ok(ActionResult::Message(
                    "请输入 API Base URL；交互终端会立即读取该值，也可使用 /project config url URL"
                        .to_string(),
                ));
            }
            ConfigAction::ApiKey(Some(value)) => {
                return self.update_model_config(|config| config.set_api_key(&value));
            }
            ConfigAction::ApiKey(None) => {
                bail!("模型密钥必须在交互终端中隐藏输入")
            }
            ConfigAction::Show => unreachable!(),
        }
        self.project.configure(&preferences)?;
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

    fn skill_catalog(&self) -> Result<SkillCatalog> {
        let user_root = std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".alda-agent").join("skills"));
        SkillCatalog::discover(
            user_root.as_deref(),
            Some(&self.project.root().join("skills")),
        )
    }

    fn enable_advisory_skill(&mut self, value: &str) -> Result<ActionResult> {
        let id: QualifiedSkillId = value.parse()?;
        let catalog = self.skill_catalog()?;
        let descriptor = catalog
            .descriptor(&id)
            .ok_or_else(|| anyhow::anyhow!("未发现 Skill {id}"))?;
        if descriptor.kind != SkillKind::Advisory {
            bail!("Skill {id} 不是可启用的 advisory Skill");
        }
        catalog.load(&id)?;
        let changed = self.project.enable_advisory_skill(id.clone())?;
        Ok(ActionResult::Message(if changed {
            format!("✓ 已启用 {id}")
        } else {
            format!("{id} 已处于启用状态")
        }))
    }

    fn disable_advisory_skill(&mut self, value: &str) -> Result<ActionResult> {
        let id: QualifiedSkillId = value.parse()?;
        let changed = self.project.disable_advisory_skill(&id)?;
        Ok(ActionResult::Message(if changed {
            format!("✓ 已禁用 {id}")
        } else {
            format!("{id} 未启用")
        }))
    }

    fn compile_instructions(&self) -> Result<CompiledInstructions> {
        CompiledInstructions::compile(
            &self.skill_catalog()?,
            self.project.instruction_profile(),
            self.project.preferences(),
        )
    }

    fn render_skills(&self) -> Result<String> {
        let catalog = self.skill_catalog()?;
        let enabled = &self.project.instruction_profile().enabled_advisory_skills;
        let mut lines = catalog
            .descriptors()
            .into_iter()
            .map(|skill| {
                let state = if skill.kind == SkillKind::Workflow || enabled.contains(&skill.id) {
                    "enabled"
                } else {
                    "disabled"
                };
                format!(
                    "{} · {:?} · {state} · {}",
                    skill.id, skill.kind, skill.description
                )
            })
            .collect::<Vec<_>>();
        lines.extend(
            catalog
                .diagnostics()
                .iter()
                .map(|diagnostic| format!("! 无效 Skill · {diagnostic}")),
        );
        Ok(lines.join("\n"))
    }

    async fn export(&self, target: ScoreTarget, format: ExportFormat) -> Result<ActionResult> {
        if matches!(target, ScoreTarget::File(_)) {
            bail!("export 不支持外部文件");
        }
        let (source, label, stem) = match target {
            ScoreTarget::Working => (
                self.project.working_path()?,
                "工作乐谱".to_string(),
                "work".to_string(),
            ),
            ScoreTarget::Version(version) => {
                let version = version.unwrap_or(self.require_current()?);
                (
                    self.project.version_path_for(version)?,
                    format!("v{version}"),
                    format!("version-{version:04}"),
                )
            }
            ScoreTarget::File(_) => unreachable!(),
        };
        let export_dir = self.project.root().join("exports");
        std::fs::create_dir_all(&export_dir)?;
        let alda_path = export_dir.join(format!("{stem}.alda"));
        let midi_path = export_dir.join(format!("{stem}.mid"));
        let wav_path = export_dir.join(format!("{stem}.wav"));
        let mut paths = Vec::new();
        if matches!(format, ExportFormat::Alda | ExportFormat::All)
            || (format == ExportFormat::Midi && self.alda.is_none())
        {
            std::fs::copy(&source, &alda_path).with_context(|| {
                format!("无法从 {} 导出到 {}", source.display(), alda_path.display())
            })?;
            paths.push(alda_path.display().to_string());
        }
        if matches!(format, ExportFormat::Midi | ExportFormat::All) {
            if self.alda.is_none() && format == ExportFormat::Midi {
                paths.push("MIDI 未导出：未找到 alda".to_string());
                return Ok(ActionResult::Message(format!(
                    "! 已导出 {label} 的 Alda 源码，但 MIDI 未生成\n  {}",
                    paths.join("\n  ")
                )));
            }
            self.runner()?
                .export_midi_async(source.clone(), midi_path.clone())
                .await?;
            paths.push(midi_path.display().to_string());
        }
        if matches!(format, ExportFormat::Wav | ExportFormat::All) {
            let renderer = AudioRenderer::discover()?;
            let report = renderer
                .render_score_async(
                    self.runner()?.clone(),
                    source,
                    midi_path.clone(),
                    wav_path.clone(),
                )
                .await?;
            if !paths
                .iter()
                .any(|path| path == &midi_path.display().to_string())
                && matches!(format, ExportFormat::All)
            {
                paths.push(midi_path.display().to_string());
            }
            paths.push(format!(
                "{}（{:.2} 秒，{} Hz，{} 声道，peak {:.4}，RMS {:.4}）",
                wav_path.display(),
                report.wav.duration_secs,
                report.wav.sample_rate,
                report.wav.channels,
                report.wav.peak,
                report.wav.rms
            ));
        }
        Ok(ActionResult::Message(format!(
            "✓ 已导出 {label}\n  {}",
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

fn explicit_duration_secs(prompt: &str) -> Option<DurationConstraint> {
    let range = Regex::new(
        r"(?i)(\d+(?:\.\d+)?)\s*(?:-|–|—|~|到|至)\s*(\d+(?:\.\d+)?)\s*(分钟|分|min(?:ute)?s?|秒(?:钟)?|sec(?:ond)?s?|s)\b?",
    )
    .expect("duration range regex");
    let exact =
        Regex::new(r"(?i)(\d+(?:\.\d+)?)\s*(分钟|分|min(?:ute)?s?|秒钟|秒|sec(?:ond)?s?|s)\b?")
            .expect("duration regex");
    let intent_before = Regex::new(
        r"(?i)(?:时长|总长|长度|目标|预计|控制在|做成|写成|创作(?:一首)?|生成(?:一首)?|希望(?:是|为)?|约|大约|duration|length|about|around|last(?:ing)?)\s*$",
    )
    .expect("duration intent prefix regex");
    let intent_after = Regex::new(
        r"(?i)^\s*(?:左右|以内|上下|的(?:作品|音乐|曲子|乐曲|器乐|歌曲)|duration|long)\b?",
    )
    .expect("duration intent suffix regex");
    let positional_before =
        Regex::new(r"(?:第|开头|前|从\s*第?|每)\s*$").expect("duration position prefix regex");
    let positional_after = Regex::new(r"^\s*(?:后|时|处|开始|进入|加入|只用)")
        .expect("duration position suffix regex");
    let only_duration = Regex::new(
        r"(?i)^\s*(?:约|大约|about|around)?\s*\d+(?:\.\d+)?(?:\s*(?:-|–|—|~|到|至)\s*\d+(?:\.\d+)?)?\s*(?:分钟|分|min(?:ute)?s?|秒钟|秒|sec(?:ond)?s?|s)\s*(?:左右|以内|上下)?\s*[。.!！]?\s*$",
    )
    .expect("standalone duration regex");

    let has_duration_intent = |matched: regex::Match<'_>| {
        let before = &prompt[..matched.start()];
        let after = &prompt[matched.end()..];
        if positional_before.is_match(before) || positional_after.is_match(after) {
            return false;
        }
        let before_context = before
            .char_indices()
            .rev()
            .nth(23)
            .map_or(before, |(index, _)| &before[index..]);
        let after_context = after
            .char_indices()
            .nth(23)
            .map_or(after, |(index, _)| &after[..index]);
        only_duration.is_match(prompt)
            || intent_before.is_match(before_context)
            || intent_after.is_match(after_context)
    };

    if let Some(duration) = range.captures_iter(prompt).find_map(|captures| {
        let matched = captures.get(0)?;
        if !has_duration_intent(matched) {
            return None;
        }
        let first = captures.get(1)?.as_str().parse::<f64>().ok()?;
        let second = captures.get(2)?.as_str().parse::<f64>().ok()?;
        let unit = captures.get(3)?.as_str().to_ascii_lowercase();
        let multiplier = if unit.starts_with('分') || unit.starts_with("min") {
            60.0
        } else {
            1.0
        };
        let duration = DurationConstraint::range(first * multiplier, second * multiplier);
        duration.validate().ok().map(|()| duration)
    }) {
        return Some(duration);
    }

    exact.captures_iter(prompt).find_map(|captures| {
        let matched = captures.get(0)?;
        if !has_duration_intent(matched) {
            return None;
        }
        let value = captures.get(1)?.as_str().parse::<f64>().ok()?;
        let unit = captures.get(2)?.as_str().to_ascii_lowercase();
        let seconds = if unit.starts_with("分") || unit.starts_with("min") {
            value * 60.0
        } else {
            value
        };
        (seconds.is_finite() && seconds > 0.0).then_some(DurationConstraint::exact(seconds))
    })
}

fn requests_complete_candidate(prompt: &str) -> bool {
    let normalized = prompt.to_lowercase();
    if [
        "计划",
        "方案",
        "思路",
        "建议",
        "怎么编",
        "如何编",
        "先讲",
        "先说",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
    {
        return false;
    }
    [
        "编写曲目",
        "开始谱曲",
        "直接完成",
        "完成曲目",
        "完成整首",
        "生成完整曲目",
        "编曲",
        "作曲",
        "写曲",
        "开始创作",
        "写成",
        "写一首",
        "创作一首",
        "write the full piece",
        "complete the piece",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

fn render_working_status(project: &Project) -> String {
    let Some(working) = project.working_score() else {
        return "当前没有工作乐谱".to_string();
    };
    let duration = working
        .checks
        .iter()
        .find(|check| check.name == "时长")
        .map_or("时长未知", |check| check.detail.as_str());
    format!("当前工作稿仍为{}，{}", working.kind, duration)
}

fn agent_playback_label(target: &str, working_score_changed: bool) -> String {
    match target {
        "work" if working_score_changed => "修改前工作乐谱".to_string(),
        "work" => "工作乐谱".to_string(),
        "current" => "当前版本".to_string(),
        value => value.to_string(),
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
        "模式：{}\n目标时长：{}\n包含乐器：{}\n排除乐器：{}\n内建工作流：builtin:progressive-composition\nAdvisory Skills：{}\n模型名称：{}\nAPI Base URL：{}\n模型密钥：{}",
        view.mode,
        view.target_duration_secs
            .map_or_else(|| "无".to_string(), |value| value.to_string()),
        empty(&view.included_instruments),
        empty(&view.excluded_instruments),
        empty(&view.enabled_advisory_skills),
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

    #[test]
    fn natural_language_duration_distinguishes_total_ranges_from_timeline_positions() {
        assert_eq!(
            explicit_duration_secs("做成 3 分钟左右"),
            Some(DurationConstraint::exact(180.0))
        );
        assert_eq!(
            explicit_duration_secs("目标 150 秒"),
            Some(DurationConstraint::exact(150.0))
        );
        assert_eq!(
            explicit_duration_secs("about 2.5 minutes"),
            Some(DurationConstraint::exact(150.0))
        );
        assert_eq!(
            explicit_duration_secs("3 分钟"),
            Some(DurationConstraint::exact(180.0))
        );
        assert_eq!(
            explicit_duration_secs("写一首3分钟的作品"),
            Some(DurationConstraint::exact(180.0))
        );
        assert_eq!(explicit_duration_secs("预计时长应为数分钟"), None);
        assert_eq!(
            explicit_duration_secs("控制在 3-5 分钟"),
            Some(DurationConstraint::range(180.0, 300.0))
        );
        assert_eq!(
            explicit_duration_secs("控制在 3到5分钟"),
            Some(DurationConstraint::range(180.0, 300.0))
        );
        assert_eq!(
            explicit_duration_secs("控制在 3–5 min"),
            Some(DurationConstraint::range(180.0, 300.0))
        );
        assert_eq!(
            explicit_duration_secs("我想以此为引写一首咏叹调，时长2-3分钟"),
            Some(DurationConstraint::range(120.0, 180.0))
        );
        assert_eq!(explicit_duration_secs("在第 2-3 分钟加入小号"), None);
        assert_eq!(explicit_duration_secs("开头 2-3 分钟只用弦乐"), None);
        assert_eq!(explicit_duration_secs("在第 3 分钟加入小号"), None);
        assert_eq!(explicit_duration_secs("开头 30 秒只用弦乐"), None);
        assert_eq!(explicit_duration_secs("30 秒后进入副歌"), None);
        assert_eq!(
            explicit_duration_secs("目标时长 3 分钟，开头 30 秒只用弦乐"),
            Some(DurationConstraint::exact(180.0))
        );
    }

    #[test]
    fn explicit_completion_commands_require_a_candidate() {
        assert!(requests_complete_candidate("编写曲目"));
        assert!(requests_complete_candidate("现在开始谱曲"));
        assert!(requests_complete_candidate("不要再停顿，直接完成"));
        assert!(requests_complete_candidate("编曲"));
        assert!(requests_complete_candidate("开始作曲"));
        assert!(requests_complete_candidate("写曲"));
        assert!(requests_complete_candidate("*写成器乐圣咏"));
        assert!(requests_complete_candidate(
            "我想以此为引写一首咏叹调，时长2-3分钟"
        ));
        assert!(requests_complete_candidate("write the full piece"));
        assert!(!requests_complete_candidate("先说明你的创作计划"));
        assert!(!requests_complete_candidate("先说编曲计划"));
        assert!(!requests_complete_candidate("你的编曲思路是什么"));
        assert!(!requests_complete_candidate("我想写一首歌，你有什么建议"));
        assert!(!requests_complete_candidate("先做二十秒核心草稿"));
    }

    #[test]
    fn playback_label_does_not_misidentify_replaced_working_score() {
        assert_eq!(agent_playback_label("work", false), "工作乐谱");
        assert_eq!(agent_playback_label("work", true), "修改前工作乐谱");
        assert_eq!(agent_playback_label("current", true), "当前版本");
    }

    #[tokio::test]
    async fn exact_duration_is_not_persisted_when_model_preconditions_fail() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("duration-project");
        let project = Project::load_or_create(root.clone(), "duration-project", "").unwrap();
        let mut application = Application::from_project(project, None);
        let error = application
            .execute(UserAction::Agent("请做成 3 分钟".to_string()), &mut Silent)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("模型配置不完整"));
        let reloaded = Project::load_or_create(root, "ignored", "").unwrap();
        assert_eq!(reloaded.target_duration_secs(), None);
    }

    #[tokio::test]
    async fn timeline_positions_are_not_persisted_as_total_duration() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("timeline-project");
        let project = Project::load_or_create(root.clone(), "timeline-project", "").unwrap();
        let mut application = Application::from_project(project, None);
        let error = application
            .execute(
                UserAction::Agent("在第 3 分钟加入小号，开头 30 秒只用弦乐".to_string()),
                &mut Silent,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("模型配置不完整"));
        let reloaded = Project::load_or_create(root, "ignored", "").unwrap();
        assert_eq!(reloaded.target_duration_secs(), None);
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

    fn score_response(kind: &str) -> String {
        let arguments = serde_json::json!({
            "kind": kind,
            "message": "工作乐谱",
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

    fn candidate_response() -> String {
        score_response("candidate")
    }

    fn text_response(kind: &str, message: &str) -> String {
        let arguments = serde_json::json!({
            "kind": kind,
            "message": message
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
    async fn active_skill_is_fail_closed_and_can_be_disabled_locally() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("poem");
        let skill_root = root.join("skills/phrasing");
        std::fs::create_dir_all(&skill_root).unwrap();
        let skill_path = skill_root.join("SKILL.md");
        std::fs::write(
            &skill_path,
            "---\nname: phrasing\ndescription: 乐句建议\nkind: advisory\n---\n让乐句有清晰呼吸。\n",
        )
        .unwrap();
        let project = Project::load_or_create(root, "poem", "").unwrap();
        let mut application = Application::from_project(project, None);

        application
            .execute(
                UserAction::Project(ProjectAction::SkillEnable("project:phrasing".to_string())),
                &mut Silent,
            )
            .await
            .unwrap();
        let summary = application
            .execute(
                UserAction::Project(ProjectAction::Instructions),
                &mut Silent,
            )
            .await
            .unwrap();
        let ActionResult::Message(summary) = summary else {
            panic!("expected instruction summary");
        };
        assert!(summary.contains("project:phrasing"));

        std::fs::write(&skill_path, "not valid frontmatter\n").unwrap();
        assert!(
            application
                .execute(
                    UserAction::Project(ProjectAction::Instructions),
                    &mut Silent,
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("未发现 Skill")
        );
        application
            .execute(
                UserAction::Project(ProjectAction::SkillDisable("project:phrasing".to_string())),
                &mut Silent,
            )
            .await
            .unwrap();
        application
            .execute(
                UserAction::Project(ProjectAction::Instructions),
                &mut Silent,
            )
            .await
            .unwrap();
        let skills = application
            .execute(UserAction::Project(ProjectAction::Skills), &mut Silent)
            .await
            .unwrap();
        let ActionResult::Message(skills) = skills else {
            panic!("expected Skill catalog");
        };
        assert!(skills.contains("无效 Skill"));
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
                    target: ScoreTarget::Version(None),
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
    async fn user_message_is_not_persisted_before_agent_preconditions() {
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
        assert_eq!(reloaded.conversation().first_request(), None);
        assert_eq!(reloaded.conversation().state(), ConversationState::Ready);
    }

    #[tokio::test]
    async fn explicit_accept_is_the_application_version_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("poem");
        let mut project = Project::load_or_create(root, "poem", "").unwrap();
        project
            .configure(&ProjectPreferences {
                target_duration_secs: Some(DurationConstraint::exact(180.0)),
                ..ProjectPreferences::default()
            })
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

    #[tokio::test]
    async fn successful_turn_persists_only_semantic_history_and_result_summary() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("poem");
        let mut project = Project::load_or_create(root.clone(), "poem", "").unwrap();
        project
            .configure(&ProjectPreferences {
                target_duration_secs: Some(DurationConstraint::exact(180.0)),
                ..ProjectPreferences::default()
            })
            .unwrap();
        let (_runner_directory, runner) = passing_runner();
        let mut application = Application::from_project(project, Some(runner));
        let (base_url, _requests) = serve(vec![MockResponse::sse(candidate_response())]);
        application
            .configure(ConfigAction::Model("example-model".to_string()))
            .unwrap();
        application.configure(ConfigAction::Url(base_url)).unwrap();
        application
            .configure(ConfigAction::ApiKey(Some("test-key".to_string())))
            .unwrap();

        application
            .execute(UserAction::Agent("继续修正".to_string()), &mut Silent)
            .await
            .unwrap();

        let working = application.project.working_score().unwrap();
        assert_eq!(working.summary, "工作乐谱");
        let messages = application.project.conversation().messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content.as_deref(), Some("继续修正"));
        assert_eq!(messages[1].content.as_deref(), Some("工作乐谱"));
        assert!(
            messages
                .iter()
                .all(|message| { message.tool_calls.is_empty() && message.tool_call_id.is_none() })
        );

        let metadata = std::fs::read_to_string(root.join("project.json")).unwrap();
        assert!(!metadata.contains("tool_calls"));
        assert!(!metadata.contains("piano: c"));
    }

    #[tokio::test]
    async fn no_preference_resumes_the_pending_full_composition_without_another_pause() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("aria");
        let project = Project::load_or_create(root, "aria", "").unwrap();
        let (_runner_directory, runner) = passing_runner();
        let mut application = Application::from_project(project, Some(runner));
        let (base_url, _requests) = serve(vec![
            MockResponse::sse(text_response("clarification", "你的目标乐器有偏好吗？")),
            MockResponse::sse(text_response("clarification", "还要再选择一种配器吗？")),
            MockResponse::sse(candidate_response()),
        ]);
        application
            .configure(ConfigAction::Model("example-model".to_string()))
            .unwrap();
        application.configure(ConfigAction::Url(base_url)).unwrap();
        application
            .configure(ConfigAction::ApiKey(Some("test-key".to_string())))
            .unwrap();

        let first = application
            .execute(
                UserAction::Agent("我想以此为引写一首咏叹调，时长2-3分钟".to_string()),
                &mut Silent,
            )
            .await
            .unwrap();
        assert!(matches!(
            first,
            ActionResult::AgentCompleted {
                kind: AgentResultKind::Clarification,
                needs_input: true,
                ..
            }
        ));
        assert_eq!(
            application.conversation_view().state,
            ConversationState::AwaitingInput
        );

        let second = application
            .execute(UserAction::Agent("没有".to_string()), &mut Silent)
            .await
            .unwrap();
        assert!(matches!(
            second,
            ActionResult::AgentCompleted {
                kind: AgentResultKind::Candidate,
                success: true,
                rounds: 2,
                needs_input: false,
                ..
            }
        ));
        assert_eq!(
            application.project.target_duration_secs(),
            Some(DurationConstraint::range(120.0, 180.0))
        );
        assert_eq!(
            application.project_view().working_score.as_deref(),
            Some("完整候选")
        );
        assert!(
            !application
                .project
                .conversation()
                .messages()
                .iter()
                .filter_map(|message| message.content.as_deref())
                .any(|content| content.contains("还要再选择一种配器"))
        );
    }

    #[tokio::test]
    async fn clarification_answer_cannot_turn_a_pending_composition_into_a_refusal_loop() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("brand-hymn");
        let project = Project::load_or_create(root, "brand-hymn", "").unwrap();
        let (_runner_directory, runner) = passing_runner();
        let mut application = Application::from_project(project, Some(runner));
        let refusal = "我不能创作包含具体商业品牌名称的圣咏。如果愿意，可以去掉品牌名称吗？";
        let (base_url, _requests) = serve(vec![
            MockResponse::sse(text_response(
                "clarification",
                "作品中要出现具体品牌名称吗？",
            )),
            MockResponse::sse(text_response("answer", refusal)),
            MockResponse::sse(candidate_response()),
        ]);
        application
            .configure(ConfigAction::Model("example-model".to_string()))
            .unwrap();
        application.configure(ConfigAction::Url(base_url)).unwrap();
        application
            .configure(ConfigAction::ApiKey(Some("test-key".to_string())))
            .unwrap();

        let first = application
            .execute(
                UserAction::Agent("我想以华为口号为引写一首圣咏，时长2-3分钟".to_string()),
                &mut Silent,
            )
            .await
            .unwrap();
        assert!(matches!(
            first,
            ActionResult::AgentCompleted {
                kind: AgentResultKind::Clarification,
                needs_input: true,
                ..
            }
        ));
        assert!(application.project.conversation().pending_candidate());

        let second = application
            .execute(
                UserAction::Agent("是，出现具体品牌名称".to_string()),
                &mut Silent,
            )
            .await
            .unwrap();
        assert!(matches!(
            second,
            ActionResult::AgentCompleted {
                kind: AgentResultKind::Candidate,
                success: true,
                rounds: 2,
                needs_input: false,
                ..
            }
        ));
        assert!(!application.project.conversation().pending_candidate());
        assert!(
            !application
                .project
                .conversation()
                .messages()
                .iter()
                .filter_map(|message| message.content.as_deref())
                .any(|content| content.contains("我不能创作"))
        );
    }

    #[tokio::test]
    async fn draft_generation_and_work_check_ignore_only_duration() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("poem");
        let mut project = Project::load_or_create(root, "poem", "").unwrap();
        project
            .configure(&ProjectPreferences {
                target_duration_secs: Some(DurationConstraint::exact(60.0)),
                ..ProjectPreferences::default()
            })
            .unwrap();
        let (_directory, runner) = passing_runner();
        let mut application = Application::from_project(project, Some(runner));
        let (base_url, _requests) = serve(vec![MockResponse::sse(score_response("draft"))]);
        application
            .configure(ConfigAction::Model("example-model".to_string()))
            .unwrap();
        application.configure(ConfigAction::Url(base_url)).unwrap();
        application
            .configure(ConfigAction::ApiKey(Some("test-key".to_string())))
            .unwrap();

        let result = application
            .execute(UserAction::Agent("先写草稿".to_string()), &mut Silent)
            .await
            .unwrap();
        assert!(matches!(
            result,
            ActionResult::AgentCompleted {
                kind: AgentResultKind::Draft,
                success: true,
                ..
            }
        ));

        let ActionResult::Checks(checks) = application
            .execute(
                UserAction::Alda(AldaAction::Check(ScoreTarget::Working)),
                &mut Silent,
            )
            .await
            .unwrap()
        else {
            panic!("expected checks");
        };
        assert_eq!(
            checks
                .iter()
                .find(|check| check.name == "时长")
                .unwrap()
                .status,
            CheckStatus::Unchecked
        );
    }

    #[tokio::test]
    async fn candidate_check_adopt_and_accept_share_full_project_validation() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("poem");
        let mut project = Project::load_or_create(root.clone(), "poem", "").unwrap();
        project
            .configure(&ProjectPreferences {
                target_duration_secs: Some(DurationConstraint::exact(60.0)),
                ..ProjectPreferences::default()
            })
            .unwrap();
        project
            .save_working_score(
                "piano: c",
                WorkingScoreKind::Candidate,
                "candidate",
                &passing_checks(),
            )
            .unwrap();
        let external = directory.path().join("external.alda");
        std::fs::write(&external, "piano: d").unwrap();
        let (_directory, runner) = passing_runner();
        let mut application = Application::from_project(project, Some(runner));

        let ActionResult::Checks(work_checks) = application
            .execute(
                UserAction::Alda(AldaAction::Check(ScoreTarget::Working)),
                &mut Silent,
            )
            .await
            .unwrap()
        else {
            panic!("expected work checks");
        };
        let work_duration = work_checks
            .iter()
            .find(|check| check.name == "时长")
            .unwrap();
        assert_eq!(work_duration.status, CheckStatus::Fail);

        let ActionResult::Checks(adopt_checks) = application
            .execute(
                UserAction::Project(ProjectAction::Adopt(external)),
                &mut Silent,
            )
            .await
            .unwrap()
        else {
            panic!("expected adopt checks");
        };
        assert_eq!(
            adopt_checks.iter().find(|check| check.name == "时长"),
            Some(work_duration)
        );
        assert_eq!(application.project_view().current_version, None);

        let ActionResult::Checks(accept_checks) = application
            .execute(UserAction::Project(ProjectAction::Accept), &mut Silent)
            .await
            .unwrap()
        else {
            panic!("expected accept checks");
        };
        assert_eq!(
            accept_checks.iter().find(|check| check.name == "时长"),
            Some(work_duration)
        );
        assert_eq!(application.project_view().current_version, None);
        assert_eq!(
            application.project_view().working_score.as_deref(),
            Some("完整候选")
        );
        assert!(!root.join("versions/0001.alda").exists());
    }
}
