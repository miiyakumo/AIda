use crate::agent::{
    Agent, AgentReporter, AgentResultKind, AgentToolContext, CreationRequest, CreationResult,
    GenerationStats, ProjectPromptRequest, RecoveryCheckpoint, RunPolicy, form_plan_check,
    requires_form_plan,
};
use crate::alda::{AldaCheck, AldaRunner, CancellationToken, CheckStatus, find_alda};
use crate::audio::AudioRenderer;
use crate::command::{
    AldaAction, ConfigAction, ExportFormat, ProjectAction, ScoreTarget, UserAction, help,
};
use crate::config::ModelConfig;
use crate::conversation::{Conversation, ConversationMessage, ConversationRole, ConversationState};
use crate::deepseek::{ChatError, DeepSeekClient};
use crate::instructions::{
    CompiledInstructions, CreationMode, DurationConstraint, ProjectPreferences,
};
use crate::project::{AgentMode, CheckRecord, Project, WorkingScoreKind};
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
    let audio_renderer =
        AudioRenderer::discover().context("完整候选需要先配置可用的 FluidSynth 与 SoundFont")?;
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
        agent: Agent::new(client, AldaRunner::new(alda_path)).with_audio_renderer(audio_renderer),
        request: CreationRequest {
            source_material: request.source_material,
            instructions: String::new(),
            compiled_instructions,
            run_policy: RunPolicy::default(),
        },
    })
}

pub async fn compose_once(prepared: PreparedCompose) -> Result<CreationResult> {
    let mut result = prepared.agent.create(prepared.request).await?;
    if let Some(error) = result.terminal_error.take() {
        return Err(error);
    }
    Ok(result)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProjectView {
    pub name: String,
    pub first_request: Option<String>,
    pub current_version: Option<u32>,
    pub working_score: Option<String>,
    pub agent_mode: String,
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
        stats: GenerationStats,
        needs_input: bool,
        recovery_checkpoint: Option<RecoveryCheckpoint>,
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
    audio_renderer: Option<AudioRenderer>,
    audio_renderer_preflight_error: Option<String>,
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
            audio_renderer: None,
            audio_renderer_preflight_error: None,
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
            audio_renderer: None,
            audio_renderer_preflight_error: None,
        }
    }

    #[must_use]
    pub fn from_project_with_audio_renderer(
        project: Project,
        alda: Option<AldaRunner>,
        audio_renderer: AudioRenderer,
    ) -> Self {
        let mut application = Self::from_project(project, alda);
        application.audio_renderer = Some(audio_renderer);
        application
    }

    #[must_use]
    pub fn from_project_with_audio_renderer_failure(
        project: Project,
        alda: Option<AldaRunner>,
        error: impl Into<String>,
    ) -> Self {
        let mut application = Self::from_project(project, alda);
        application.audio_renderer_preflight_error = Some(error.into());
        application
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
            agent_mode: self.project.agent_mode().to_string(),
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
        if self.project.agent_mode() == AgentMode::CompositionAb {
            self.execute_composition_ab(prompt, reporter).await
        } else {
            self.execute_single_agent(prompt, reporter).await
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn execute_single_agent(
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
        let audio_renderer = if let Some(error) = &self.audio_renderer_preflight_error {
            if require_candidate {
                bail!("完整候选音频渲染环境不可用：{error}");
            }
            None
        } else if let Some(renderer) = &self.audio_renderer {
            Some(renderer.clone())
        } else if require_candidate {
            Some(
                AudioRenderer::discover()
                    .context("完整候选需要先配置可用的 FluidSynth 与 SoundFont")?,
            )
        } else {
            None
        };
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
        let agent = Agent::new(client, runner);
        let agent = if let Some(renderer) = audio_renderer {
            agent.with_audio_renderer(renderer)
        } else {
            agent
        };
        let result = agent
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
                    form_plan: self
                        .project
                        .pending_revision()
                        .and_then(|revision| revision.form_plan.as_ref())
                        .or_else(|| {
                            self.project
                                .working_score()
                                .and_then(|working| working.form_plan.as_ref())
                        })
                        .or_else(|| self.project.current_form_plan())
                        .cloned(),
                    compiled_instructions,
                    run_policy: RunPolicy::default(),
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
                        revision_path: self
                            .project
                            .pending_revision()
                            .map(|_| self.project.revision_path())
                            .transpose()?,
                        form_plan: self
                            .project
                            .pending_revision()
                            .and_then(|revision| revision.form_plan.as_ref())
                            .or_else(|| {
                                self.project
                                    .working_score()
                                    .and_then(|working| working.form_plan.as_ref())
                            })
                            .or_else(|| self.project.current_form_plan())
                            .cloned(),
                    }),
                    require_candidate,
                    forbid_clarification: answered_clarification,
                },
                reporter,
            )
            .await;
        let mut result = match result {
            Ok(result) => result,
            Err(error) => {
                self.last_model_failure = ModelFailure::from_error(&error);
                return Err(error);
            }
        };
        let terminal_error = result.terminal_error.take();
        if let Some(error) = terminal_error.as_ref() {
            self.last_model_failure = ModelFailure::from_error(error);
        } else {
            self.last_model_failure = None;
            self.model_request_succeeded = true;
        }
        if result.success {
            let kind = match result.kind {
                AgentResultKind::Draft => WorkingScoreKind::Draft,
                AgentResultKind::Candidate => WorkingScoreKind::Candidate,
                _ => bail!("文本结果不能标记为校验成功"),
            };
            let alda_code = result
                .alda_code
                .as_deref()
                .context("成功结果缺少 Alda 代码")?;
            match kind {
                WorkingScoreKind::Draft => self.project.save_working_score_with_plan(
                    alda_code,
                    kind,
                    &result.interpretation,
                    &result.checks,
                    result.form_plan.clone(),
                )?,
                WorkingScoreKind::Candidate => {
                    let artifacts = result
                        .candidate_artifacts
                        .as_ref()
                        .context("成功候选缺少已验证的 MIDI/WAV")?;
                    self.project.save_rendered_candidate_with_plan(
                        alda_code,
                        &result.interpretation,
                        &result.checks,
                        artifacts.midi_path(),
                        artifacts.wav_path(),
                        result.form_plan.clone(),
                    )?;
                }
            }
        } else if let Some(alda_code) = result.alda_code.as_deref() {
            let kind = match result.kind {
                AgentResultKind::Draft => WorkingScoreKind::Draft,
                AgentResultKind::Candidate => WorkingScoreKind::Candidate,
                _ => bail!("文本结果不能保存为待修正候选"),
            };
            self.project.save_pending_revision_with_plan(
                alda_code,
                kind,
                &result.interpretation,
                &result.checks,
                result.form_plan.clone(),
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
        if let Some(error) = terminal_error {
            return Err(error);
        }
        Ok(ActionResult::AgentCompleted {
            kind: result.kind,
            success: result.success,
            rounds: result.rounds,
            stats: result.stats,
            needs_input: result.needs_input,
            recovery_checkpoint: result.recovery_checkpoint,
            working_score_changed,
            working_score_status: render_working_status(&self.project),
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn execute_composition_ab(
        &mut self,
        prompt: String,
        reporter: &mut impl AgentReporter,
    ) -> Result<ActionResult> {
        let config = ModelConfig::load(self.project.root()).and_then(|config| config.resolve())?;
        let mut preferences = self.project.preferences().clone();
        let explicit_duration = explicit_duration_secs(&prompt);
        if let Some(duration) = explicit_duration {
            preferences.target_duration_secs = Some(duration);
        }
        if preferences.mode != CreationMode::Full {
            bail!("composition-ab 只生成完整曲目；请先使用 /project config mode full");
        }
        let duration = match preferences.target_duration_secs {
            Some(DurationConstraint::Exact(seconds)) => seconds,
            Some(DurationConstraint::Range { min_secs, max_secs }) => min_secs.midpoint(max_secs),
            None => bail!("composition-ab 需要目标时长；请先使用 /project config duration SECONDS"),
        };
        let runner = self
            .alda
            .clone()
            .ok_or_else(|| anyhow::anyhow!("未找到 alda；Agent 不能绕过校验保存候选"))?;
        let renderer = if let Some(error) = &self.audio_renderer_preflight_error {
            bail!("完整候选音频渲染环境不可用：{error}");
        } else if let Some(renderer) = &self.audio_renderer {
            renderer.clone()
        } else {
            AudioRenderer::discover().context("完整候选需要先配置可用的 FluidSynth 与 SoundFont")?
        };
        let existing_score = if self.project.working_score().is_some() {
            Some(self.project.working_code()?)
        } else if self.project.current_version() > 0 {
            Some(self.project.current_code()?)
        } else {
            None
        };
        let requirement = composition_ab_requirement(self.project.conversation(), &prompt);
        let task = existing_score.map_or_else(
            || requirement.clone(),
            |score| {
                format!(
                    "用户要求：\n{requirement}\n\n现有完整 Alda 乐谱如下。请保留要求中未被修改的音乐意图，并生成完整替换候选：\n```alda\n{score}\n```"
                )
            },
        );
        let previous_working_code = self
            .project
            .working_score()
            .map(|_| self.project.working_code())
            .transpose()?;
        if explicit_duration.is_some() {
            self.project.configure(&preferences)?;
        }
        self.project
            .prepare_user_message_with_requirement(&prompt, true)?;
        if !self.privacy_shown {
            reporter.report(crate::agent::AgentEvent::PrivacyNotice);
            self.privacy_shown = true;
        }
        reporter.report(crate::agent::AgentEvent::RoundStarted { attempt: 1 });
        let client = DeepSeekClient::new(config.api_key, config.base_url, config.model)?;
        let result = crate::composition_ab::run(
            &task,
            duration,
            &preferences.included_instruments,
            &preferences.excluded_instruments,
            client,
            runner,
            renderer,
        )
        .await;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                self.last_model_failure = ModelFailure::from_error(&error);
                return Err(error);
            }
        };
        reporter.report(crate::agent::AgentEvent::ValidationCompleted(
            result.checks.clone(),
        ));
        self.project.save_rendered_candidate_with_plan(
            &result.alda_code,
            &result.summary,
            &result.checks,
            result.midi_path(),
            result.wav_path(),
            Some(result.form_plan.clone()),
        )?;
        self.project
            .finish_agent_turn(result.summary.clone(), ConversationState::Ready)?;
        self.last_model_failure = None;
        self.model_request_succeeded = true;
        let working_score_changed =
            previous_working_code.as_deref() != self.project.working_code().ok().as_deref();
        Ok(ActionResult::AgentCompleted {
            kind: AgentResultKind::Candidate,
            success: true,
            rounds: result.stats.submissions,
            stats: result.stats,
            needs_input: false,
            recovery_checkpoint: None,
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
            AldaAction::PlaySection {
                target,
                section_id,
                context_secs,
            } => {
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
                let runner = self.runner()?;
                let info = runner.parse(&path)?;
                let marker_name = if section_id.starts_with("section_") {
                    section_id.clone()
                } else {
                    format!("section_{section_id}")
                };
                let section = info
                    .sections
                    .iter()
                    .find(|section| section.name == marker_name)
                    .with_context(|| format!("乐谱中没有段落 {section_id:?}"))?;
                let context_ms = f64::from(context_secs.clamp(5, 15)) * 1000.0;
                let from_ms = (section.start_ms - context_ms).max(0.0);
                let to_ms = (section.end_ms + context_ms).min(info.duration_ms);
                runner
                    .play_range_async(path, alda_time_marking(from_ms), alda_time_marking(to_ms))
                    .await?;
                let playback_label = format!("{label} 段落 {section_id}");
                self.playback = Some(playback_label.clone());
                Ok(ActionResult::Message(format!(
                    "✓ 已发起播放 {playback_label}（含前后 {} 秒上下文）",
                    context_secs.clamp(5, 15)
                )))
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
                let form_plan = match self.project.working_score() {
                    Some(working) if working.kind == WorkingScoreKind::Candidate => {
                        working.form_plan.clone()
                    }
                    Some(_) => bail!("当前工作乐谱是草稿，不能接受为有效版本"),
                    None => bail!("项目没有可接受的完整候选"),
                };
                let validation = self.project.preferences().score_validation(true);
                let runner = self.runner()?;
                let working_path = self.project.working_path()?;
                let mut checks = runner
                    .validate_async(working_path.clone(), validation.clone())
                    .await?;
                let info = runner.parse(&working_path).ok();
                if let Some(check) = form_plan_check(
                    info.as_ref(),
                    form_plan.as_ref(),
                    requires_form_plan(&validation),
                ) {
                    checks.push(check);
                }
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
            ProjectAction::AgentMode(mode) => {
                if let Some(mode) = mode {
                    self.project
                        .configure_agent_mode(mode.parse::<AgentMode>()?)?;
                    Ok(ActionResult::Message(format!(
                        "✓ Agent 模式已切换为 {mode}"
                    )))
                } else {
                    Ok(ActionResult::Message(format!(
                        "Agent 模式：{}",
                        self.project.agent_mode()
                    )))
                }
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

fn alda_time_marking(milliseconds: f64) -> String {
    let total_seconds = milliseconds.max(0.0) / 1000.0;
    let minutes = (total_seconds / 60.0).floor();
    format!("{minutes:.0}:{:.3}", total_seconds - minutes * 60.0)
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
    let rendered =
        if working.kind == WorkingScoreKind::Candidate && project.work_wav_path().is_file() {
            format!("，已渲染 WAV：{}", project.work_wav_path().display())
        } else {
            String::new()
        };
    format!("当前工作稿仍为{}，{}{}", working.kind, duration, rendered)
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

fn composition_ab_requirement(conversation: &Conversation, prompt: &str) -> String {
    if conversation.state() != ConversationState::RequestPending {
        return prompt.to_string();
    }
    let mut requirements = conversation
        .messages()
        .iter()
        .rev()
        .take_while(|message| message.role == ConversationRole::User)
        .filter_map(|message| message.content.as_deref())
        .collect::<Vec<_>>();
    requirements.reverse();
    if requirements.last().copied() != Some(prompt) {
        requirements.push(prompt);
    }
    if requirements.len() == 1 {
        return requirements[0].to_string();
    }
    let items = requirements
        .iter()
        .enumerate()
        .map(|(index, requirement)| format!("{}. {requirement}", index + 1))
        .collect::<Vec<_>>()
        .join("\n\n");
    format!("以下条目属于同一项尚未完成的创作请求，后续条目用于补充或重试前文：\n\n{items}")
}

fn render_config(view: &ProjectView) -> String {
    format!(
        "Agent 模式：{}\n创作模式：{}\n目标时长：{}\n包含乐器：{}\n排除乐器：{}\n内建工作流：builtin:progressive-composition\nAdvisory Skills：{}\n模型名称：{}\nAPI Base URL：{}\n模型密钥：{}",
        view.agent_mode,
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
    fn composition_ab_retry_keeps_the_unfinished_requirement() {
        let mut conversation = Conversation::default();
        conversation.add_user_message("根据完整故事创作五分钟主题曲".to_string());
        conversation.set_state(ConversationState::RequestPending);

        assert_eq!(
            composition_ab_requirement(&conversation, "根据完整故事创作五分钟主题曲"),
            "根据完整故事创作五分钟主题曲"
        );
        let retry = composition_ab_requirement(&conversation, "重试原要求");
        assert!(retry.contains("根据完整故事创作五分钟主题曲"));
        assert!(retry.contains("重试原要求"));
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
        let json = r#"{"markers":{"section_intro":0,"section_develop":45000,"section_contrast":90000,"section_close":135000},"events":[{"offset":0,"duration":180000,"audible-duration":180000,"midi-note":60,"part":"piano"}],"parts":{"piano":{"name":"piano","stock-instrument":"midi-acoustic-grand-piano","tempo":120}}}"#;
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n  parse) printf '%s\\n' '{json}' ;;\n  export) printf midi > \"$5\" ;;\n  *) exit 0 ;;\nesac\n"
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
        (directory, AldaRunner::new(executable))
    }

    fn passing_renderer(root: &std::path::Path) -> AudioRenderer {
        renderer_with_amplitude(root, 8_000)
    }

    fn renderer_with_amplitude(root: &std::path::Path, amplitude: i16) -> AudioRenderer {
        use hound::{SampleFormat, WavSpec, WavWriter};
        use std::os::unix::fs::PermissionsExt;

        let source_wav = root.join("source.wav");
        let mut writer = WavWriter::create(
            &source_wav,
            WavSpec {
                channels: 1,
                sample_rate: 8_000,
                bits_per_sample: 16,
                sample_format: SampleFormat::Int,
            },
        )
        .unwrap();
        for index in 0..800 {
            writer
                .write_sample(if index % 2 == 0 {
                    amplitude
                } else {
                    -amplitude
                })
                .unwrap();
        }
        writer.finalize().unwrap();
        let fluidsynth = root.join("fluidsynth");
        std::fs::write(
            &fluidsynth,
            format!("#!/bin/sh\ncp '{}' \"$4\"\n", source_wav.display()),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fluidsynth).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fluidsynth, permissions).unwrap();
        let soundfont = root.join("test.sf2");
        std::fs::write(&soundfont, "soundfont").unwrap();
        AudioRenderer::new(fluidsynth, soundfont)
    }

    fn score_response(kind: &str) -> String {
        let mut arguments = serde_json::json!({
            "kind": kind,
            "message": "工作乐谱",
            "alda_code": "piano: c"
        });
        if kind == "candidate" {
            arguments["form_plan"] = test_form_plan();
            arguments["edit_scope"] = serde_json::json!({
                "mode": "global",
                "target_sections": [],
                "intent": "测试完整候选"
            });
        }
        let arguments = arguments.to_string();
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

    fn test_form_plan() -> serde_json::Value {
        serde_json::json!({
            "target_duration_secs": 180.0,
            "sections": [
                { "id": "intro", "target_start_secs": 0.0, "target_end_secs": 45.0, "function": "引子", "material_action": "introduce", "energy": "low" },
                { "id": "develop", "target_start_secs": 45.0, "target_end_secs": 90.0, "function": "发展", "material_action": "develop", "energy": "medium" },
                { "id": "contrast", "target_start_secs": 90.0, "target_end_secs": 135.0, "function": "对比", "material_action": "contrast", "energy": "high" },
                { "id": "close", "target_start_secs": 135.0, "target_end_secs": 180.0, "function": "收束", "material_action": "close", "energy": "peak" }
            ]
        })
    }

    fn host_tool_response(name: &str, arguments: &serde_json::Value) -> String {
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_host",
                        "function": {
                            "name": name,
                            "arguments": arguments.to_string()
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        format!("data: {chunk}\n\ndata: [DONE]\n")
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
    async fn renderer_preflight_failure_does_not_persist_the_user_message() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("poem");
        let project = Project::load_or_create(root.clone(), "poem", "").unwrap();
        let (_runner_directory, runner) = passing_runner();
        let mut application = Application::from_project_with_audio_renderer_failure(
            project,
            Some(runner),
            "missing SoundFont",
        );
        application
            .configure(ConfigAction::Model("example-model".to_string()))
            .unwrap();
        application
            .configure(ConfigAction::Url("https://api.example.com".to_string()))
            .unwrap();
        application
            .configure(ConfigAction::ApiKey(Some("test-key".to_string())))
            .unwrap();
        let metadata_before = std::fs::read(root.join("project.json")).unwrap();

        let error = application
            .execute(UserAction::Agent("完成整首作品".to_string()), &mut Silent)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("missing SoundFont"));
        assert_eq!(
            std::fs::read(root.join("project.json")).unwrap(),
            metadata_before
        );
        assert!(application.project.conversation().messages().is_empty());
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
        let (runner_directory, runner) = passing_runner();
        let mut application = Application::from_project_with_audio_renderer(
            project,
            Some(runner),
            passing_renderer(runner_directory.path()),
        );
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
        let (runner_directory, runner) = passing_runner();
        let mut application = Application::from_project_with_audio_renderer(
            project,
            Some(runner),
            passing_renderer(runner_directory.path()),
        );
        let (base_url, _requests) = serve(vec![MockResponse::sse(candidate_response())]);
        application
            .configure(ConfigAction::Model("example-model".to_string()))
            .unwrap();
        application.configure(ConfigAction::Url(base_url)).unwrap();
        application
            .configure(ConfigAction::ApiKey(Some("test-key".to_string())))
            .unwrap();

        let result = application
            .execute(UserAction::Agent("继续修正".to_string()), &mut Silent)
            .await
            .unwrap();
        let ActionResult::AgentCompleted {
            working_score_status,
            ..
        } = result
        else {
            panic!("expected agent result");
        };
        assert!(working_score_status.contains("work.wav"));

        let working = application.project.working_score().unwrap();
        assert_eq!(working.summary, "工作乐谱");
        assert!(
            working
                .checks
                .iter()
                .any(|check| { check.name == "音频渲染" && check.status == CheckStatus::Pass })
        );
        assert_eq!(
            std::fs::read(application.project.work_midi_path()).unwrap(),
            b"midi"
        );
        assert!(application.project.work_wav_path().is_file());
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
    async fn model_failure_after_a_failed_candidate_persists_the_revision() {
        assert_failed_candidate_revision_survives(
            vec![
                MockResponse::sse(candidate_response()),
                MockResponse::error("500 Internal Server Error", "service unavailable"),
            ],
            "500",
        )
        .await;
    }

    #[tokio::test]
    async fn candidate_inspection_checkpoint_survives_model_limit_and_restart() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("checkpoint");
        let mut project = Project::load_or_create(root.clone(), "checkpoint", "").unwrap();
        project
            .configure(&ProjectPreferences {
                target_duration_secs: Some(DurationConstraint::exact(180.0)),
                ..ProjectPreferences::default()
            })
            .unwrap();
        let (runner_directory, runner) = passing_runner();
        let mut application = Application::from_project_with_audio_renderer(
            project,
            Some(runner),
            passing_renderer(runner_directory.path()),
        );
        let checkpoint_source = "piano: checkpoint";
        let responses = (0..RunPolicy::default().max_model_calls)
            .map(|_| {
                MockResponse::sse(host_tool_response(
                    "inspect_alda_source",
                    &serde_json::json!({
                        "alda_code": checkpoint_source,
                        "scope": "candidate",
                        "form_plan": test_form_plan()
                    }),
                ))
            })
            .collect();
        let (base_url, _requests) = serve(responses);
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
        let ActionResult::AgentCompleted {
            success,
            stats,
            recovery_checkpoint,
            ..
        } = result
        else {
            panic!("expected agent result");
        };
        assert!(!success);
        assert_eq!(
            recovery_checkpoint,
            Some(RecoveryCheckpoint::InspectedCandidate)
        );
        assert_eq!(stats.model_calls, RunPolicy::default().max_model_calls);
        assert_eq!(stats.tool_turns, RunPolicy::default().max_model_calls);
        assert_eq!(stats.submissions, 0);
        assert_eq!(
            application.project.revision_code().unwrap(),
            checkpoint_source
        );

        drop(application);
        let reloaded = Project::load_or_create(root, "ignored", "").unwrap();
        assert_eq!(reloaded.revision_code().unwrap(), checkpoint_source);
        let (runner_directory, runner) = passing_runner();
        let mut restarted = Application::from_project_with_audio_renderer(
            reloaded,
            Some(runner),
            passing_renderer(runner_directory.path()),
        );
        let (base_url, requests) = serve(vec![MockResponse::sse(candidate_response())]);
        restarted
            .configure(ConfigAction::Model("example-model".to_string()))
            .unwrap();
        restarted.configure(ConfigAction::Url(base_url)).unwrap();
        restarted
            .configure(ConfigAction::ApiKey(Some("test-key".to_string())))
            .unwrap();

        let resumed = restarted
            .execute(UserAction::Agent("继续修正".to_string()), &mut Silent)
            .await
            .unwrap();
        assert!(matches!(
            resumed,
            ActionResult::AgentCompleted { success: true, .. }
        ));
        let request = String::from_utf8(requests.recv().unwrap()).unwrap();
        assert!(request.contains(checkpoint_source));
    }

    #[tokio::test]
    async fn result_kind_change_after_a_failed_candidate_persists_the_revision() {
        let mut responses = vec![MockResponse::sse(candidate_response())];
        responses.extend(
            (0..=RunPolicy::default().max_protocol_recoveries)
                .map(|_| MockResponse::sse(score_response("draft"))),
        );
        assert_failed_candidate_revision_survives(responses, "协议恢复超过").await;
    }

    #[tokio::test]
    async fn missing_score_code_after_a_failed_candidate_persists_the_revision() {
        let mut responses = vec![MockResponse::sse(candidate_response())];
        responses.extend(
            (0..=RunPolicy::default().max_protocol_recoveries)
                .map(|_| MockResponse::sse(text_response("candidate", "缺少源码"))),
        );
        assert_failed_candidate_revision_survives(responses, "协议恢复超过").await;
    }

    async fn assert_failed_candidate_revision_survives(
        responses: Vec<MockResponse>,
        expected_error: &str,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("poem");
        let mut project = Project::load_or_create(root.clone(), "poem", "").unwrap();
        project
            .configure(&ProjectPreferences {
                target_duration_secs: Some(DurationConstraint::exact(300.0)),
                ..ProjectPreferences::default()
            })
            .unwrap();
        let (runner_directory, runner) = passing_runner();
        let mut application = Application::from_project_with_audio_renderer(
            project,
            Some(runner),
            passing_renderer(runner_directory.path()),
        );
        let (base_url, _requests) = serve(responses);
        application
            .configure(ConfigAction::Model("example-model".to_string()))
            .unwrap();
        application.configure(ConfigAction::Url(base_url)).unwrap();
        application
            .configure(ConfigAction::ApiKey(Some("test-key".to_string())))
            .unwrap();

        let error = application
            .execute(UserAction::Agent("继续修正".to_string()), &mut Silent)
            .await
            .unwrap_err();

        assert!(error.to_string().contains(expected_error));
        assert_eq!(
            application.conversation_view().state,
            ConversationState::RevisionAvailable
        );
        assert_eq!(application.project.revision_code().unwrap(), "piano: c");
        let metadata = std::fs::read_to_string(root.join("project.json")).unwrap();
        assert!(metadata.contains("pending_revision"));
        assert!(!metadata.contains("piano: c"));
        let reloaded = Project::load_or_create(root, "ignored", "").unwrap();
        assert_eq!(reloaded.revision_code().unwrap(), "piano: c");
    }

    #[tokio::test]
    async fn silent_candidate_preserves_existing_work_and_audio() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("poem");
        let mut project = Project::load_or_create(root.clone(), "poem", "").unwrap();
        project
            .configure(&ProjectPreferences {
                target_duration_secs: Some(DurationConstraint::exact(180.0)),
                ..ProjectPreferences::default()
            })
            .unwrap();
        let old_midi = directory.path().join("old.mid");
        let old_wav = directory.path().join("old.wav");
        std::fs::write(&old_midi, "old midi").unwrap();
        std::fs::write(&old_wav, "old wav").unwrap();
        project
            .save_rendered_candidate(
                "piano: old",
                "old candidate",
                &passing_checks(),
                &old_midi,
                &old_wav,
            )
            .unwrap();
        let (runner_directory, runner) = passing_runner();
        let mut application = Application::from_project_with_audio_renderer(
            project,
            Some(runner),
            renderer_with_amplitude(runner_directory.path(), 0),
        );
        let responses = (0..RunPolicy::default().max_model_calls)
            .map(|_| MockResponse::sse(candidate_response()))
            .collect();
        let (base_url, _requests) = serve(responses);
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
                success: false,
                ..
            }
        ));
        assert_eq!(application.project.working_code().unwrap(), "piano: old");
        assert_eq!(
            std::fs::read(application.project.work_midi_path()).unwrap(),
            b"old midi"
        );
        assert_eq!(
            std::fs::read(application.project.work_wav_path()).unwrap(),
            b"old wav"
        );
        assert!(application.project.pending_revision().is_some());
    }

    #[tokio::test]
    async fn no_preference_resumes_the_pending_full_composition_without_another_pause() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("aria");
        let project = Project::load_or_create(root, "aria", "").unwrap();
        let (runner_directory, runner) = passing_runner();
        let mut application = Application::from_project_with_audio_renderer(
            project,
            Some(runner),
            passing_renderer(runner_directory.path()),
        );
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
        let (runner_directory, runner) = passing_runner();
        let mut application = Application::from_project_with_audio_renderer(
            project,
            Some(runner),
            passing_renderer(runner_directory.path()),
        );
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
