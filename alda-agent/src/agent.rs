use crate::alda::{AldaCheck, AldaRunner, CheckStatus, ScoreValidation};
use crate::audio::{ArtifactReport, AudioRenderer};
use crate::conversation::{ConversationMessage, ConversationRole, ConversationToolCall};
use crate::deepseek::{DeepSeekClient, FunctionDef, Message, StreamDelta, StreamEvent, Tool};
use crate::instructions::CompiledInstructions;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// ============================================================
// 系统提示
// ============================================================

// ============================================================
// 工具定义
// ============================================================

fn submit_result_tool() -> Tool {
    Tool {
        ty: "function".to_string(),
        function: FunctionDef {
            name: "submit_result".to_string(),
            description: "明确提交普通回答、澄清、创作计划、草稿或完整候选。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["answer", "clarification", "plan", "draft", "candidate"]
                    },
                    "message": {
                        "type": "string",
                        "description": "回答、澄清或计划正文；乐谱结果的简短说明"
                    },
                    "alda_code": {
                        "type": "string",
                        "description": "draft 或 candidate 的紧凑 Alda 乐谱代码",
                        "maxLength": crate::deepseek::MAX_TOOL_ARGUMENT_BYTES
                    },
                    "plan": {
                        "type": "object",
                        "description": "kind=plan 时必填；必须自包含，不能引用工具外的隐藏文本",
                        "properties": {
                            "core_material": { "type": "string", "description": "核心音乐材料与主题动机" },
                            "form": { "type": "string", "description": "完整曲式与各段职责" },
                            "orchestration": { "type": "string", "description": "配器与声部角色" },
                            "development": { "type": "string", "description": "材料发展、对比与收束方式" }
                        },
                        "required": ["core_material", "form", "orchestration", "development"]
                    }
                },
                "required": ["kind", "message"]
            }),
        },
    }
}

fn score_tool(name: &str, description: &str) -> Tool {
    Tool {
        ty: "function".to_string(),
        function: FunctionDef {
            name: name.to_string(),
            description: description.to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": { "type": "string", "enum": ["work", "current"] }
                },
                "required": ["target"]
            }),
        },
    }
}

const MAX_INSPECT_ALDA_SOURCE_BYTES: usize = 32 * 1024;

fn inspect_alda_source_tool() -> Tool {
    Tool {
        ty: "function".to_string(),
        function: FunctionDef {
            name: "inspect_alda_source".to_string(),
            description: "真实解析尚未提交的 Alda 临时源码，返回总时长、Marker 实际位置、各声部结束时间和事件数，并分开报告硬失败与诊断。fragment 只检查局部材料且不保留；candidate 使用项目完整约束并作为故障恢复检查点，但不会保存工作乐谱、渲染或计作正式提交。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "alda_code": {
                        "type": "string",
                        "description": "尚未提交的 Alda 临时源码；candidate 仅可用于完整曲目",
                        "maxLength": MAX_INSPECT_ALDA_SOURCE_BYTES
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["fragment", "candidate"],
                        "description": "fragment 不检查项目目标时长或配器约束且不保留；candidate 检查完整项目约束并更新故障恢复检查点，但仍需自行调用 submit_result 正式提交"
                    }
                },
                "required": ["alda_code", "scope"]
            }),
        },
    }
}

fn lookup_docs_tool() -> Tool {
    Tool {
        ty: "function".to_string(),
        function: FunctionDef {
            name: "lookup_alda_docs".to_string(),
            description: "查询随应用固定版本保存的 Alda 官方手册章节。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "topic": {
                        "type": "string",
                        "enum": ["parts", "aliases", "notes", "attributes", "repeats", "variables", "sequences", "voices", "markers", "instruments"]
                    }
                },
                "required": ["topic"]
            }),
        },
    }
}

fn model_tools(host_tools: bool) -> Vec<Tool> {
    let mut tools = vec![
        submit_result_tool(),
        lookup_docs_tool(),
        inspect_alda_source_tool(),
    ];
    if host_tools {
        tools.extend([
            score_tool("inspect_score", "真实解析并检查当前或工作乐谱，返回时长、声部、事件、乐器和约束检查。"),
            score_tool("render_score", "真实导出 MIDI 并用 FluidSynth 渲染 WAV，返回音频时长、采样率、峰值、RMS 和静音判断。"),
            score_tool("play_score", "真实发起播放当前或工作乐谱。只有工具成功后才能告诉用户已经播放。"),
        ]);
    }
    tools
}

// ============================================================
// 创作请求
// ============================================================

pub struct CreationRequest {
    /// 素材文本
    pub source_material: String,
    /// 本次创作的自然语言要求
    pub instructions: String,
    /// 由宿主编译的不可变有效指示
    pub compiled_instructions: CompiledInstructions,
    pub run_policy: RunPolicy,
}

#[derive(Debug, Clone, Copy)]
pub struct RunPolicy {
    pub max_elapsed: Duration,
    pub max_model_calls: usize,
    pub max_protocol_recoveries: usize,
}

impl Default for RunPolicy {
    fn default() -> Self {
        Self {
            max_elapsed: Duration::from_secs(15 * 60),
            max_model_calls: 24,
            max_protocol_recoveries: 8,
        }
    }
}

// ============================================================
// 创作结果
// ============================================================

#[derive(Debug)]
pub struct CreationResult {
    /// 实际提交给宿主的结果次数
    pub rounds: usize,
    /// 本次生成的真实调用与提交计数。
    pub stats: GenerationStats,
    /// 是否通过必要检查
    pub success: bool,
    /// 模型提出澄清问题，尚未生成候选
    pub needs_input: bool,
    /// 模型显式声明的结果类型
    pub kind: AgentResultKind,
    /// 最后一轮的检查结果
    pub checks: Vec<AldaCheck>,
    /// 通过时的 Alda 源码
    pub alda_code: Option<String>,
    /// 模型的解读文本
    pub interpretation: String,
    /// 是否被截断
    pub was_truncated: bool,
    /// 仅用于当前自动修正或澄清往返；新的修改请求会重建干净上下文
    pub conversation: Vec<Message>,
    /// 本轮由模型通过宿主工具实际发起播放的目标。
    pub played_target: Option<String>,
    /// 本轮由模型通过宿主工具实际生成的 WAV。
    pub rendered_wav: Option<PathBuf>,
    /// 完整候选自动校验后生成的临时 MIDI/WAV；由调用方持久化。
    pub candidate_artifacts: Option<StagedCandidateArtifacts>,
    /// 失败结果中的源码恢复点来源。
    pub recovery_checkpoint: Option<RecoveryCheckpoint>,
    /// 已有失败候选后发生的终止错误；调用方先持久化候选，再返回该错误。
    pub(crate) terminal_error: Option<anyhow::Error>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct GenerationStats {
    pub model_calls: usize,
    pub tool_turns: usize,
    pub protocol_recoveries: usize,
    pub submissions: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryCheckpoint {
    InspectedCandidate,
}

#[derive(Debug)]
struct CandidateCheckpoint {
    alda_code: String,
    checks: Vec<AldaCheck>,
}

#[derive(Debug)]
struct ModelToolResult {
    content: String,
    candidate_checkpoint: Option<CandidateCheckpoint>,
}

impl ModelToolResult {
    fn content(content: String) -> Self {
        Self {
            content,
            candidate_checkpoint: None,
        }
    }
}

fn oversized_source_inspection(source: &str, scope: &str, candidate: bool) -> ModelToolResult {
    let size_detail = if candidate {
        format!(
            "源码为 {} 字节，超过 {} 字节上限；请缩减完整候选后再检查",
            source.len(),
            MAX_INSPECT_ALDA_SOURCE_BYTES
        )
    } else {
        format!(
            "源码为 {} 字节，超过 {} 字节上限；请一次检查一个 4–16 小节材料",
            source.len(),
            MAX_INSPECT_ALDA_SOURCE_BYTES
        )
    };
    let checks = vec![AldaCheck {
        name: "源码大小",
        status: CheckStatus::Fail,
        detail: size_detail,
    }];
    let content = serde_json::json!({
        "scope": scope,
        "parse_ok": false,
        "duration_secs": null,
        "markers": [],
        "parts": [],
        "hard_failures": [{
            "name": "源码大小",
            "detail": checks[0].detail
        }],
        "diagnostics": []
    })
    .to_string();
    ModelToolResult {
        content,
        candidate_checkpoint: candidate.then(|| CandidateCheckpoint {
            alda_code: source.to_string(),
            checks,
        }),
    }
}

#[derive(Debug)]
pub struct StagedCandidateArtifacts {
    _directory: tempfile::TempDir,
    report: ArtifactReport,
}

impl StagedCandidateArtifacts {
    #[must_use]
    pub fn report(&self) -> &ArtifactReport {
        &self.report
    }

    #[must_use]
    pub fn midi_path(&self) -> &Path {
        &self.report.midi_path
    }

    #[must_use]
    pub fn wav_path(&self) -> &Path {
        &self.report.wav_path
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentResultKind {
    Answer,
    Clarification,
    Plan,
    Draft,
    Candidate,
}

pub struct ProjectPromptRequest {
    pub conversation: Vec<ConversationMessage>,
    pub current_alda: Option<String>,
    pub working_alda: Option<String>,
    pub revision_alda: Option<String>,
    pub compiled_instructions: CompiledInstructions,
    pub run_policy: RunPolicy,
    pub tool_context: Option<AgentToolContext>,
    pub require_candidate: bool,
    pub forbid_clarification: bool,
}

#[derive(Debug, Clone)]
pub struct AgentToolContext {
    pub project_root: PathBuf,
    pub current_path: Option<PathBuf>,
    pub working_path: Option<PathBuf>,
}

struct ValidationRequest {
    score: ScoreValidation,
    run_policy: RunPolicy,
    tool_context: Option<AgentToolContext>,
    require_candidate: bool,
    forbid_clarification: bool,
}

// ============================================================
// Agent
// ============================================================

pub struct Agent {
    client: DeepSeekClient,
    runner: AldaRunner,
    audio_renderer: Option<AudioRenderer>,
}

pub trait AgentReporter {
    fn report(&mut self, event: AgentEvent);
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
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
        tool_name: String,
    },
    ModelText(String),
    ValidationStarted {
        attempt: usize,
    },
    ValidationCompleted(Vec<AldaCheck>),
    RevisionStarted {
        next_attempt: usize,
        failures: usize,
    },
}

struct SilentReporter;

impl AgentReporter for SilentReporter {
    fn report(&mut self, _event: AgentEvent) {}
}

#[must_use]
pub fn to_provider_messages(messages: &[ConversationMessage]) -> Vec<Message> {
    messages
        .iter()
        .map(|message| Message {
            role: match message.role {
                ConversationRole::User => "user",
                ConversationRole::Assistant => "assistant",
                ConversationRole::System => "system",
                ConversationRole::Tool => "tool",
            }
            .to_string(),
            content: message.content.clone(),
            tool_calls: (!message.tool_calls.is_empty()).then(|| {
                message
                    .tool_calls
                    .iter()
                    .map(|call| crate::deepseek::ToolCallMsg {
                        id: call.id.clone(),
                        ty: "function".to_string(),
                        function: crate::deepseek::FunctionCallArgs {
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                        },
                    })
                    .collect()
            }),
            tool_call_id: message.tool_call_id.clone(),
        })
        .collect()
}

#[must_use]
pub fn from_provider_messages(messages: Vec<Message>) -> Vec<ConversationMessage> {
    messages
        .into_iter()
        .map(|message| ConversationMessage {
            role: match message.role.as_str() {
                "user" => ConversationRole::User,
                "system" => ConversationRole::System,
                "tool" => ConversationRole::Tool,
                _ => ConversationRole::Assistant,
            },
            content: message.content,
            tool_calls: message
                .tool_calls
                .unwrap_or_default()
                .into_iter()
                .map(|call| ConversationToolCall {
                    id: call.id,
                    name: call.function.name,
                    arguments: call.function.arguments,
                })
                .collect(),
            tool_call_id: message.tool_call_id,
        })
        .collect()
}

impl Agent {
    #[must_use]
    pub fn new(client: DeepSeekClient, runner: AldaRunner) -> Self {
        Agent {
            client,
            runner,
            audio_renderer: None,
        }
    }

    #[must_use]
    pub fn with_audio_renderer(mut self, renderer: AudioRenderer) -> Self {
        self.audio_renderer = Some(renderer);
        self
    }

    pub async fn create(&self, request: CreationRequest) -> Result<CreationResult> {
        self.create_with_reporter(request, &mut SilentReporter)
            .await
    }

    pub async fn respond_with_reporter(
        &self,
        request: ProjectPromptRequest,
        reporter: &mut impl AgentReporter,
    ) -> Result<CreationResult> {
        if request.conversation.is_empty() {
            bail!("对话不能为空");
        }
        let validation = request
            .compiled_instructions
            .resolved_preferences()
            .score_validation(true);
        let mut messages = vec![Message {
            role: "system".to_string(),
            content: Some(request.compiled_instructions.rendered().to_string()),
            tool_calls: None,
            tool_call_id: None,
        }];
        messages.push(Message {
            role: "system".to_string(),
            content: Some(build_project_context(&request)),
            tool_calls: None,
            tool_call_id: None,
        });
        messages.extend(to_provider_messages(&request.conversation));
        self.run_generation(
            messages,
            ValidationRequest {
                score: validation,
                run_policy: request.run_policy,
                tool_context: request.tool_context,
                require_candidate: request.require_candidate,
                forbid_clarification: request.forbid_clarification,
            },
            reporter,
        )
        .await
    }

    pub async fn create_with_reporter(
        &self,
        request: CreationRequest,
        reporter: &mut impl AgentReporter,
    ) -> Result<CreationResult> {
        if request.source_material.trim().is_empty() && request.instructions.trim().is_empty() {
            bail!("创作素材与要求不能同时为空");
        }
        let validation = request
            .compiled_instructions
            .resolved_preferences()
            .score_validation(true);
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: Some(request.compiled_instructions.rendered().to_string()),
                tool_calls: None,
                tool_call_id: None,
            },
            Message {
                role: "user".to_string(),
                content: Some(build_user_message(&request)),
                tool_calls: None,
                tool_call_id: None,
            },
        ];

        self.run_generation(
            messages,
            ValidationRequest {
                score: validation,
                run_policy: request.run_policy,
                tool_context: None,
                require_candidate: false,
                forbid_clarification: false,
            },
            reporter,
        )
        .await
    }

    // Host tool turns and protocol recovery do not count as submitted results.
    #[allow(clippy::too_many_lines)]
    async fn run_generation(
        &self,
        mut messages: Vec<Message>,
        validation: ValidationRequest,
        reporter: &mut impl AgentReporter,
    ) -> Result<CreationResult> {
        let run_policy = validation.run_policy;
        let started = Instant::now();
        let max_model_calls = run_policy.max_model_calls.max(1);
        let max_protocol_recoveries = run_policy.max_protocol_recoveries.max(1);
        let max_elapsed = run_policy.max_elapsed.max(Duration::from_secs(1));
        let mut round = 0_usize;
        let mut model_calls = 0_usize;
        let mut tool_turns = 0_usize;
        let mut protocol_recoveries = 0_usize;
        let mut interpretation = String::new();
        let mut last_alda_code = None;
        let mut last_checks = Vec::new();
        let mut last_was_truncated = false;
        let mut score_kind = None;
        let mut played_target = None;
        let mut rendered_wav = None;
        let mut candidate_artifacts = None;
        let mut checkpointed_candidate = false;
        let mut terminal_error = None;
        let mut continuing_after_tool = false;

        loop {
            let stop_detail = if model_calls >= max_model_calls {
                Some(format!(
                    "已达到 {max_model_calls} 次模型调用安全上限；保留最后一份待修正结果"
                ))
            } else if started.elapsed() >= max_elapsed {
                Some(format!(
                    "自动修正已运行 {:.0} 秒，达到安全时限；保留最后一份待修正结果",
                    started.elapsed().as_secs_f64()
                ))
            } else {
                None
            };
            if let Some(detail) = stop_detail {
                last_checks.push(AldaCheck {
                    name: "运行策略",
                    status: CheckStatus::Fail,
                    detail,
                });
                break;
            }
            model_calls += 1;
            if std::mem::take(&mut continuing_after_tool) {
                reporter.report(AgentEvent::ToolContinuationStarted { turn: tool_turns });
            } else {
                reporter.report(AgentEvent::RoundStarted { attempt: round + 1 });
            }
            let mut streamed_text = StreamingModelText::default();
            let events = match self
                .client
                .chat_stream_with(
                    messages.clone(),
                    Some(model_tools(validation.tool_context.is_some())),
                    |delta| {
                        if let Some(text) = streamed_text.push(delta) {
                            reporter.report(AgentEvent::ModelText(text));
                        }
                    },
                )
                .await
            {
                Ok(events) => events,
                Err(error) if last_alda_code.is_some() => {
                    last_checks.push(AldaCheck {
                        name: "模型服务",
                        status: CheckStatus::Fail,
                        detail: format!("修正期间模型请求失败：{error:#}"),
                    });
                    terminal_error = Some(error);
                    break;
                }
                Err(error) => return Err(error),
            };
            let mut calls = Vec::new();
            let mut round_text = String::new();
            let mut was_truncated = false;
            for event in events {
                match event {
                    StreamEvent::Text(text) => round_text.push_str(&text),
                    StreamEvent::ToolCall {
                        id,
                        name,
                        arguments,
                    } => {
                        calls.push((id, name, arguments));
                    }
                    StreamEvent::Done { finish_reason } => {
                        was_truncated = finish_reason == "length";
                    }
                }
            }
            if calls.len() > 1 {
                tool_turns += 1;
                protocol_recoveries += 1;
                if protocol_recoveries > max_protocol_recoveries {
                    let error = anyhow::anyhow!(
                        "宿主工具协议恢复超过 {max_protocol_recoveries} 次，已停止以避免无进展循环"
                    );
                    if last_alda_code.is_none() {
                        return Err(error);
                    }
                    last_checks.push(AldaCheck {
                        name: "运行策略",
                        status: CheckStatus::Fail,
                        detail: error.to_string(),
                    });
                    terminal_error = Some(error);
                    break;
                }
                reporter.report(AgentEvent::ToolProtocolRetry {
                    call_count: calls.len(),
                });
                let normalized_calls = calls
                    .into_iter()
                    .enumerate()
                    .map(|(index, (id, name, arguments))| {
                        (
                            id.unwrap_or_else(|| format!("call_{}_{}", tool_turns, index + 1)),
                            name,
                            arguments,
                        )
                    })
                    .collect::<Vec<_>>();
                messages.push(tool_calls_message(&normalized_calls, round_text));
                for (id, _, _) in &normalized_calls {
                    messages.push(Message {
                        role: "tool".to_string(),
                        content: Some(
                            serde_json::json!({
                                "ok": false,
                                "error": "一次响应只允许一个工具调用；本次调用均未执行。请每次只调用一个工具，并等待结果后再继续。"
                            })
                            .to_string(),
                        ),
                        tool_calls: None,
                        tool_call_id: Some(id.clone()),
                    });
                }
                continuing_after_tool = true;
                continue;
            }
            if calls.is_empty() {
                tool_turns += 1;
                protocol_recoveries += 1;
                if protocol_recoveries > max_protocol_recoveries {
                    let error = anyhow::anyhow!(
                        "宿主工具协议恢复超过 {max_protocol_recoveries} 次，已停止以避免无进展循环"
                    );
                    if last_alda_code.is_none() {
                        return Err(error);
                    }
                    last_checks.push(AldaCheck {
                        name: "运行策略",
                        status: CheckStatus::Fail,
                        detail: error.to_string(),
                    });
                    terminal_error = Some(error);
                    break;
                }
                reporter.report(AgentEvent::ToolCallMissingRetry);
                messages.push(Message {
                    role: "assistant".to_string(),
                    content: Some(if round_text.trim().is_empty() {
                        "（模型未返回可执行结果）".to_string()
                    } else {
                        round_text
                    }),
                    tool_calls: None,
                    tool_call_id: None,
                });
                messages.push(Message {
                    role: "system".to_string(),
                    content: Some(
                        "宿主协议错误：本轮没有调用工具，结果未执行。每次响应必须只调用一个工具；请根据原始用户请求继续，并最终调用 submit_result。"
                            .to_string(),
                    ),
                    tool_calls: None,
                    tool_call_id: None,
                });
                continuing_after_tool = true;
                continue;
            }
            let Some((tool_id, tool_name, tool_args)) = calls.pop() else {
                unreachable!("已处理没有工具调用的响应");
            };
            let tool_call_id =
                tool_id.unwrap_or_else(|| format!("call_{}", tool_turns + round + 1));

            if tool_name != "submit_result" {
                tool_turns += 1;
                messages.push(tool_call_message(
                    &tool_call_id,
                    &tool_name,
                    &tool_args,
                    round_text,
                ));
                let outcome = self
                    .execute_model_tool(
                        &tool_name,
                        &tool_args,
                        validation.tool_context.as_ref(),
                        &validation.score,
                    )
                    .await;
                if let Ok(outcome) = &outcome {
                    let parsed = serde_json::from_str::<serde_json::Value>(&tool_args).ok();
                    let target = parsed
                        .as_ref()
                        .and_then(|value| value["target"].as_str())
                        .map(ToString::to_string);
                    if tool_name == "play_score" {
                        played_target = target;
                    } else if tool_name == "render_score" {
                        rendered_wav = target.map(|target| {
                            validation
                                .tool_context
                                .as_ref()
                                .expect("render_score requires context")
                                .project_root
                                .join("exports")
                                .join(format!("agent-{target}.wav"))
                        });
                    }
                    if let Some(checkpoint) = &outcome.candidate_checkpoint {
                        last_alda_code = Some(checkpoint.alda_code.clone());
                        last_checks.clone_from(&checkpoint.checks);
                        last_was_truncated = was_truncated;
                        interpretation = "完整候选检查点（尚未正式提交）".to_string();
                        checkpointed_candidate = true;
                    }
                }
                messages.push(Message {
                    role: "tool".to_string(),
                    content: Some(match outcome {
                        Ok(outcome) => outcome.content,
                        Err(error) => serde_json::json!({
                            "ok": false,
                            "error": format!("{error:#}")
                        })
                        .to_string(),
                    }),
                    tool_calls: None,
                    tool_call_id: Some(tool_call_id),
                });
                continuing_after_tool = true;
                continue;
            }

            let submitted = match parse_submitted_result(&tool_args) {
                Ok(submitted) => submitted,
                Err(error) => {
                    tool_turns += 1;
                    protocol_recoveries += 1;
                    if protocol_recoveries > max_protocol_recoveries {
                        let error = anyhow::anyhow!(
                            "宿主工具协议恢复超过 {max_protocol_recoveries} 次，已停止以避免无进展循环"
                        );
                        if last_alda_code.is_none() {
                            return Err(error);
                        }
                        last_checks.push(AldaCheck {
                            name: "运行策略",
                            status: CheckStatus::Fail,
                            detail: error.to_string(),
                        });
                        terminal_error = Some(error);
                        break;
                    }
                    reporter.report(AgentEvent::ToolArgumentsRetry {
                        tool_name: tool_name.clone(),
                    });
                    messages.push(tool_call_message(
                        &tool_call_id,
                        &tool_name,
                        &tool_args,
                        round_text,
                    ));
                    let detail = if was_truncated {
                        "模型响应被截断，submit_result 参数不是完整 JSON".to_string()
                    } else {
                        format!("submit_result 参数无效：{error:#}")
                    };
                    messages.push(Message {
                        role: "tool".to_string(),
                        content: Some(
                            serde_json::json!({
                                "ok": false,
                                "error": detail,
                                "instruction": "本次结果未执行且不计作候选提交。请重新调用 submit_result，并提交完整、有效的 JSON 参数。"
                            })
                            .to_string(),
                        ),
                        tool_calls: None,
                        tool_call_id: Some(tool_call_id),
                    });
                    continuing_after_tool = true;
                    continue;
                }
            };
            round += 1;
            let result_policy_failure = if validation.forbid_clarification
                && submitted.kind == AgentResultKind::Clarification
            {
                Some("用户已明确表示没有额外约束；请选择合理默认值继续，不得再次询问可选偏好")
            } else if validation.require_candidate
                && matches!(
                    submitted.kind,
                    AgentResultKind::Answer | AgentResultKind::Plan | AgentResultKind::Draft
                )
            {
                Some("用户已明确要求完成曲目；本轮必须提交 candidate，不能停在回答、计划或短草稿")
            } else {
                None
            };
            if let Some(detail) = result_policy_failure {
                let checks = vec![AldaCheck {
                    name: "结果类型",
                    status: CheckStatus::Fail,
                    detail: detail.to_string(),
                }];
                last_checks.clone_from(&checks);
                last_was_truncated = was_truncated;
                interpretation.clone_from(&submitted.message);
                messages.push(tool_call_message(
                    &tool_call_id,
                    "submit_result",
                    &tool_args,
                    round_text,
                ));
                messages.push(Message {
                    role: "tool".to_string(),
                    content: Some(build_tool_feedback(&checks, Some(&tool_call_id))),
                    tool_calls: None,
                    tool_call_id: Some(tool_call_id),
                });
                reporter.report(AgentEvent::ValidationCompleted(checks.clone()));
                reporter.report(AgentEvent::RevisionStarted {
                    next_attempt: round + 1,
                    failures: checks
                        .iter()
                        .filter(|check| check.status == CheckStatus::Fail)
                        .count(),
                });
                continue;
            }
            if matches!(
                submitted.kind,
                AgentResultKind::Answer | AgentResultKind::Clarification | AgentResultKind::Plan
            ) {
                messages.push(tool_call_message(
                    &tool_call_id,
                    "submit_result",
                    &tool_args,
                    round_text,
                ));
                messages.push(Message {
                    role: "tool".to_string(),
                    content: Some("宿主已接收文本结果；工作乐谱未改变。".to_string()),
                    tool_calls: None,
                    tool_call_id: Some(tool_call_id),
                });
                return Ok(CreationResult {
                    rounds: round,
                    stats: GenerationStats {
                        model_calls,
                        tool_turns,
                        protocol_recoveries,
                        submissions: round,
                    },
                    success: false,
                    needs_input: submitted.kind == AgentResultKind::Clarification,
                    kind: submitted.kind,
                    checks: Vec::new(),
                    alda_code: None,
                    interpretation: submitted.message,
                    was_truncated,
                    conversation: messages,
                    played_target,
                    rendered_wav,
                    candidate_artifacts: None,
                    recovery_checkpoint: None,
                    terminal_error: None,
                });
            }
            if score_kind.is_some_and(|previous| previous != submitted.kind) {
                tool_turns += 1;
                protocol_recoveries += 1;
                if protocol_recoveries > max_protocol_recoveries {
                    let error = anyhow::anyhow!(
                        "宿主工具协议恢复超过 {max_protocol_recoveries} 次，已停止以避免无进展循环"
                    );
                    if last_alda_code.is_none() {
                        return Err(error);
                    }
                    last_checks.push(AldaCheck {
                        name: "运行策略",
                        status: CheckStatus::Fail,
                        detail: error.to_string(),
                    });
                    terminal_error = Some(error);
                    break;
                }
                reporter.report(AgentEvent::ToolArgumentsRetry {
                    tool_name: tool_name.clone(),
                });
                messages.push(tool_call_message(
                    &tool_call_id,
                    &tool_name,
                    &tool_args,
                    round_text,
                ));
                messages.push(Message {
                    role: "tool".to_string(),
                    content: Some(
                        serde_json::json!({
                            "ok": false,
                            "error": "自动修正不能改变草稿/完整候选结果类型",
                            "instruction": "本次结果未执行且不计作候选提交。请保持首次乐谱结果类型并重新调用 submit_result。"
                        })
                        .to_string(),
                    ),
                    tool_calls: None,
                    tool_call_id: Some(tool_call_id),
                });
                continuing_after_tool = true;
                continue;
            }
            score_kind = Some(submitted.kind);
            checkpointed_candidate = false;
            let alda_code = submitted
                .alda_code
                .context("草稿或完整候选缺少 alda_code")?;
            let tmp_dir = tempfile::tempdir().context("创建临时目录失败")?;
            let tmp_score = tmp_dir.path().join("candidate.alda");
            fs::write(&tmp_score, &alda_code)?;
            reporter.report(AgentEvent::ValidationStarted { attempt: round });
            let score_validation = if submitted.kind == AgentResultKind::Candidate {
                validation.score.clone()
            } else {
                validation.score.clone().without_duration()
            };
            let mut checks = self
                .runner
                .validate_async(tmp_score.clone(), score_validation)
                .await?;
            if was_truncated {
                checks.push(AldaCheck {
                    name: "输出完整性",
                    status: CheckStatus::Fail,
                    detail: "模型输出被截断（达到 token 限制），作品可能不完整".to_string(),
                });
            }
            if submitted.kind == AgentResultKind::Candidate
                && checks.iter().all(|check| check.status != CheckStatus::Fail)
            {
                let renderer = match &self.audio_renderer {
                    Some(renderer) => renderer.clone(),
                    None => AudioRenderer::discover()?,
                };
                let midi_path = tmp_dir.path().join("candidate.mid");
                let wav_path = tmp_dir.path().join("candidate.wav");
                match renderer
                    .render_score_async(self.runner.clone(), tmp_score, midi_path, wav_path)
                    .await
                {
                    Ok(report) => {
                        checks.push(AldaCheck {
                            name: "音频渲染",
                            status: CheckStatus::Pass,
                            detail: format!(
                                "WAV {:.2}秒，{} Hz，{} 声道，peak={:.6}，RMS={:.6}，非静音；局部静音：开头 {:.1}秒，结尾 {:.1}秒，最长内部 {:.1}秒，占比 {:.1}%",
                                report.wav.duration_secs,
                                report.wav.sample_rate,
                                report.wav.channels,
                                report.wav.peak,
                                report.wav.rms,
                                report.wav.silence.leading_silence_ms / 1000.0,
                                report.wav.silence.trailing_silence_ms / 1000.0,
                                report.wav.silence.max_internal_silence_ms / 1000.0,
                                report.wav.silence.silent_ratio * 100.0,
                            ),
                        });
                        candidate_artifacts = Some(StagedCandidateArtifacts {
                            _directory: tmp_dir,
                            report,
                        });
                    }
                    Err(error) => {
                        let temporary_root = tmp_dir.path().display().to_string();
                        checks.push(AldaCheck {
                            name: "音频渲染",
                            status: CheckStatus::Fail,
                            detail: format!("{error:#}").replace(&temporary_root, "<temporary>"),
                        });
                    }
                }
            }
            reporter.report(AgentEvent::ValidationCompleted(checks.clone()));
            let all_pass = checks.iter().all(|check| check.status != CheckStatus::Fail);
            last_alda_code = Some(alda_code.clone());
            last_checks.clone_from(&checks);
            last_was_truncated = was_truncated;
            interpretation.clone_from(&submitted.message);
            messages.push(tool_call_message(
                &tool_call_id,
                "submit_result",
                &tool_args,
                round_text,
            ));
            messages.push(Message {
                role: "tool".to_string(),
                content: Some(build_tool_feedback(&checks, Some(&tool_call_id))),
                tool_calls: None,
                tool_call_id: Some(tool_call_id),
            });
            if all_pass {
                return Ok(CreationResult {
                    rounds: round,
                    stats: GenerationStats {
                        model_calls,
                        tool_turns,
                        protocol_recoveries,
                        submissions: round,
                    },
                    success: true,
                    needs_input: false,
                    kind: submitted.kind,
                    checks,
                    alda_code: Some(alda_code),
                    interpretation,
                    was_truncated,
                    conversation: messages,
                    played_target,
                    rendered_wav,
                    candidate_artifacts,
                    recovery_checkpoint: None,
                    terminal_error: None,
                });
            }
            reporter.report(AgentEvent::RevisionStarted {
                next_attempt: round + 1,
                failures: checks
                    .iter()
                    .filter(|check| check.status == CheckStatus::Fail)
                    .count(),
            });
        }
        Ok(CreationResult {
            rounds: round,
            stats: GenerationStats {
                model_calls,
                tool_turns,
                protocol_recoveries,
                submissions: round,
            },
            success: false,
            needs_input: false,
            kind: if checkpointed_candidate {
                AgentResultKind::Candidate
            } else {
                score_kind.unwrap_or(AgentResultKind::Candidate)
            },
            checks: last_checks,
            alda_code: last_alda_code,
            interpretation,
            was_truncated: last_was_truncated,
            conversation: messages,
            played_target,
            rendered_wav,
            candidate_artifacts: None,
            recovery_checkpoint: checkpointed_candidate
                .then_some(RecoveryCheckpoint::InspectedCandidate),
            terminal_error,
        })
    }

    async fn execute_model_tool(
        &self,
        name: &str,
        arguments: &str,
        context: Option<&AgentToolContext>,
        validation: &ScoreValidation,
    ) -> Result<ModelToolResult> {
        if name == "lookup_alda_docs" {
            return lookup_alda_docs(arguments).map(ModelToolResult::content);
        }
        if name == "inspect_alda_source" {
            return self.inspect_alda_source(arguments, validation).await;
        }
        let context = context.context("当前调用没有项目乐谱上下文")?;
        let parsed = serde_json::from_str::<serde_json::Value>(arguments)?;
        let target = parsed["target"].as_str().context("target 缺失")?;
        let path = match target {
            "work" => context.working_path.clone().context("项目没有工作乐谱")?,
            "current" => context
                .current_path
                .clone()
                .context("项目没有当前有效版本")?,
            _ => bail!("target 必须是 work 或 current"),
        };
        match name {
            "inspect_score" => {
                let info = self.runner.parse(&path)?;
                let checks = self.runner.validate_async(path, validation.clone()).await?;
                Ok(ModelToolResult::content(
                    serde_json::json!({ "ok": true, "info": info, "checks": checks }).to_string(),
                ))
            }
            "render_score" => {
                let export_dir = context.project_root.join("exports");
                let stem = format!("agent-{target}");
                let renderer = match &self.audio_renderer {
                    Some(renderer) => renderer.clone(),
                    None => AudioRenderer::discover()?,
                };
                let report = renderer
                    .render_score_async(
                        self.runner.clone(),
                        path,
                        export_dir.join(format!("{stem}.mid")),
                        export_dir.join(format!("{stem}.wav")),
                    )
                    .await?;
                Ok(ModelToolResult::content(
                    serde_json::json!({ "ok": true, "artifact": report }).to_string(),
                ))
            }
            "play_score" => {
                self.runner.play_async(path).await?;
                Ok(ModelToolResult::content(
                    serde_json::json!({ "ok": true, "played": target }).to_string(),
                ))
            }
            _ => bail!("未知模型工具 {name:?}"),
        }
    }

    async fn inspect_alda_source(
        &self,
        arguments: &str,
        validation: &ScoreValidation,
    ) -> Result<ModelToolResult> {
        let parsed = serde_json::from_str::<serde_json::Value>(arguments)?;
        let source = parsed["alda_code"].as_str().context("alda_code 缺失")?;
        let scope = parsed["scope"].as_str().context("scope 缺失")?;
        let candidate = match scope {
            "fragment" => false,
            "candidate" => true,
            _ => bail!("scope 必须是 fragment 或 candidate"),
        };
        if source.len() > MAX_INSPECT_ALDA_SOURCE_BYTES {
            return Ok(oversized_source_inspection(source, scope, candidate));
        }

        let temporary = tempfile::Builder::new()
            .prefix("alda-agent-inspect-")
            .suffix(".alda")
            .tempfile()
            .context("无法创建 Alda 临时检查文件")?;
        fs::write(temporary.path(), source).context("无法写入 Alda 临时检查文件")?;
        let path = temporary.path().to_path_buf();
        let score_validation = if candidate {
            validation.clone()
        } else {
            ScoreValidation::new(None, Vec::new(), Vec::new())
        };
        let checks = self
            .runner
            .validate_async(path.clone(), score_validation)
            .await?;
        let parse_ok = checks
            .iter()
            .any(|check| check.name == "Alda 语法" && check.status == CheckStatus::Pass);
        let hard_failures = checks
            .iter()
            .filter(|check| check.status == CheckStatus::Fail)
            .map(|check| serde_json::json!({ "name": check.name, "detail": check.detail }))
            .collect::<Vec<_>>();
        let diagnostics = checks
            .iter()
            .filter(|check| check.status == CheckStatus::Unchecked)
            .map(|check| serde_json::json!({ "name": check.name, "detail": check.detail }))
            .collect::<Vec<_>>();
        let info = parse_ok.then(|| self.runner.parse(&path)).transpose()?;
        let duration_secs = info.as_ref().map(|info| info.duration_ms / 1000.0);
        let parts = info
            .as_ref()
            .map(|info| {
                info.timeline
                    .parts
                    .iter()
                    .map(|part| {
                        serde_json::json!({
                            "name": part.part,
                            "end_secs": part.last_event_ms / 1000.0,
                            "event_count": part.event_count
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let markers = info
            .as_ref()
            .map(|info| {
                info.markers
                    .iter()
                    .map(|marker| {
                        serde_json::json!({
                            "name": marker.name,
                            "offset_secs": marker.offset_ms / 1000.0
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let content = serde_json::json!({
            "scope": scope,
            "parse_ok": parse_ok,
            "duration_secs": duration_secs,
            "markers": markers,
            "parts": parts,
            "hard_failures": hard_failures,
            "diagnostics": diagnostics
        })
        .to_string();
        Ok(ModelToolResult {
            content,
            candidate_checkpoint: candidate.then(|| CandidateCheckpoint {
                alda_code: source.to_string(),
                checks,
            }),
        })
    }
}

// ============================================================
// 辅助函数
// ============================================================

fn tool_call_message(id: &str, name: &str, arguments: &str, content: String) -> Message {
    Message {
        role: "assistant".to_string(),
        content: (!content.trim().is_empty()).then_some(content),
        tool_calls: Some(vec![crate::deepseek::ToolCallMsg {
            id: id.to_string(),
            ty: "function".to_string(),
            function: crate::deepseek::FunctionCallArgs {
                name: name.to_string(),
                arguments: arguments.to_string(),
            },
        }]),
        tool_call_id: None,
    }
}

fn tool_calls_message(calls: &[(String, String, String)], content: String) -> Message {
    Message {
        role: "assistant".to_string(),
        content: (!content.trim().is_empty()).then_some(content),
        tool_calls: Some(
            calls
                .iter()
                .map(|(id, name, arguments)| crate::deepseek::ToolCallMsg {
                    id: id.clone(),
                    ty: "function".to_string(),
                    function: crate::deepseek::FunctionCallArgs {
                        name: name.clone(),
                        arguments: arguments.clone(),
                    },
                })
                .collect(),
        ),
        tool_call_id: None,
    }
}

#[derive(Default)]
struct StreamingModelText {
    tool_calls: BTreeMap<i32, StreamingToolCall>,
    emitted_any: bool,
    last_source: Option<ModelTextSource>,
}

#[derive(Default)]
struct StreamingToolCall {
    name: String,
    arguments: String,
    emitted_message: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModelTextSource {
    Content,
    SubmittedMessage,
}

impl StreamingModelText {
    fn push(&mut self, delta: StreamDelta) -> Option<String> {
        match delta {
            StreamDelta::Text(text) => self.visible_chunk(ModelTextSource::Content, text),
            StreamDelta::ToolCall {
                index,
                name,
                arguments,
            } => {
                let call = self.tool_calls.entry(index).or_default();
                if let Some(name) = name {
                    call.name = name;
                }
                call.arguments.push_str(&arguments);
                if call.name != "submit_result" {
                    return None;
                }
                let message = json_string_field_prefix(&call.arguments, "message")?;
                let addition = message.strip_prefix(&call.emitted_message)?.to_string();
                call.emitted_message = message;
                self.visible_chunk(ModelTextSource::SubmittedMessage, addition)
            }
        }
    }

    fn visible_chunk(&mut self, source: ModelTextSource, text: String) -> Option<String> {
        if text.is_empty() {
            return None;
        }
        let separator = self.emitted_any && self.last_source != Some(source);
        self.emitted_any = true;
        self.last_source = Some(source);
        Some(if separator { format!("\n{text}") } else { text })
    }
}

fn json_string_field_prefix(input: &str, field: &str) -> Option<String> {
    let marker = format!("\"{field}\"");
    let mut value = input
        .get(input.find(&marker)? + marker.len()..)?
        .trim_start();
    value = value.strip_prefix(':')?.trim_start();
    Some(decode_json_string_prefix(value.strip_prefix('"')?))
}

fn decode_json_string_prefix(input: &str) -> String {
    let mut chars = input.chars();
    let mut output = String::new();
    while let Some(character) = chars.next() {
        match character {
            '"' => break,
            '\\' => {
                let Some(escape) = chars.next() else {
                    break;
                };
                match escape {
                    '"' | '\\' | '/' => output.push(escape),
                    'b' => output.push('\u{0008}'),
                    'f' => output.push('\u{000c}'),
                    'n' => output.push('\n'),
                    'r' => output.push('\r'),
                    't' => output.push('\t'),
                    'u' => {
                        let Some(high) = read_hex_unit(&mut chars) else {
                            break;
                        };
                        let codepoint = if (0xD800..=0xDBFF).contains(&high) {
                            let mut lookahead = chars.clone();
                            if lookahead.next() != Some('\\') || lookahead.next() != Some('u') {
                                break;
                            }
                            let Some(low) = read_hex_unit(&mut lookahead) else {
                                break;
                            };
                            if !(0xDC00..=0xDFFF).contains(&low) {
                                break;
                            }
                            chars = lookahead;
                            0x1_0000
                                + ((u32::from(high) - 0xD800) << 10)
                                + (u32::from(low) - 0xDC00)
                        } else {
                            u32::from(high)
                        };
                        let Some(character) = char::from_u32(codepoint) else {
                            break;
                        };
                        output.push(character);
                    }
                    _ => break,
                }
            }
            character => output.push(character),
        }
    }
    output
}

fn read_hex_unit(chars: &mut std::str::Chars<'_>) -> Option<u16> {
    let digits = chars.take(4).collect::<String>();
    (digits.len() == 4)
        .then(|| u16::from_str_radix(&digits, 16).ok())
        .flatten()
}

fn lookup_alda_docs(arguments: &str) -> Result<String> {
    const MAX_DOC_CHARS: usize = 16_000;

    let parsed = serde_json::from_str::<serde_json::Value>(arguments)?;
    let topic = parsed["topic"].as_str().context("topic 缺失")?;
    let (file, content) = match topic {
        "parts" => (
            "scores-and-parts.md",
            include_str!("../vendor/alda-docs/2.4.3/scores-and-parts.md"),
        ),
        "aliases" => (
            "instance-and-group-assignment.md",
            include_str!("../vendor/alda-docs/2.4.3/instance-and-group-assignment.md"),
        ),
        "notes" => (
            "notes.md",
            include_str!("../vendor/alda-docs/2.4.3/notes.md"),
        ),
        "attributes" => (
            "attributes.md",
            include_str!("../vendor/alda-docs/2.4.3/attributes.md"),
        ),
        "repeats" => (
            "repeats.md",
            include_str!("../vendor/alda-docs/2.4.3/repeats.md"),
        ),
        "variables" => (
            "variables.md",
            include_str!("../vendor/alda-docs/2.4.3/variables.md"),
        ),
        "sequences" => (
            "sequences.md",
            include_str!("../vendor/alda-docs/2.4.3/sequences.md"),
        ),
        "voices" => (
            "voices.md",
            include_str!("../vendor/alda-docs/2.4.3/voices.md"),
        ),
        "markers" => (
            "markers.md",
            include_str!("../vendor/alda-docs/2.4.3/markers.md"),
        ),
        "instruments" => (
            "list-of-instruments.md",
            include_str!("../vendor/alda-docs/2.4.3/list-of-instruments.md"),
        ),
        _ => bail!("未知 Alda 文档主题 {topic:?}"),
    };
    let excerpt = content.chars().take(MAX_DOC_CHARS).collect::<String>();
    Ok(serde_json::json!({
        "ok": true,
        "source": format!("Alda official release-2.4.3/{file}"),
        "runtime_compatibility": "examples are validated with the installed Alda runtime",
        "content": excerpt,
        "truncated": content.chars().count() > MAX_DOC_CHARS
    })
    .to_string())
}

fn build_user_message(request: &CreationRequest) -> String {
    let mut msg = String::new();
    let preferences = request.compiled_instructions.resolved_preferences();

    msg.push_str("【创作上下文】\n");

    if !request.source_material.is_empty() {
        msg.push_str("【素材】\n");
        msg.push_str(&request.source_material);
        msg.push_str("\n\n");
    }

    msg.push_str("【模式】");
    msg.push_str(preferences.mode.description());
    msg.push('\n');

    if let Some(dur) = preferences.target_duration_secs {
        let _ = writeln!(msg, "【目标时长】{dur}");
    }

    if !preferences.included_instruments.is_empty() {
        let _ = writeln!(
            msg,
            "【必须包含的乐器】{}",
            preferences.included_instruments.join("、")
        );
    }

    if !preferences.excluded_instruments.is_empty() {
        let _ = writeln!(
            msg,
            "【必须排除的乐器】{}",
            preferences.excluded_instruments.join("、")
        );
    }

    msg.push_str("\n【本轮要求｜来源：当前用户输入｜最高策略优先级】\n");
    if request.instructions.trim().is_empty() {
        msg.push_str("根据上述素材创作完整作品。");
    } else {
        msg.push_str(request.instructions.trim());
    }
    msg
}

fn build_project_context(request: &ProjectPromptRequest) -> String {
    let mut message = String::new();
    let preferences = request.compiled_instructions.resolved_preferences();
    message.push_str("【项目设置】\n【模式】");
    message.push_str(preferences.mode.description());
    message.push('\n');
    if let Some(duration) = preferences.target_duration_secs {
        let _ = writeln!(message, "【目标时长】{duration}");
    }
    if !preferences.included_instruments.is_empty() {
        let _ = writeln!(
            message,
            "【必须包含的乐器】{}",
            preferences.included_instruments.join("、")
        );
    }
    if !preferences.excluded_instruments.is_empty() {
        let _ = writeln!(
            message,
            "【必须排除的乐器】{}",
            preferences.excluded_instruments.join("、")
        );
    }
    if let Some(score) = &request.revision_alda {
        message.push_str("\n【上次未通过的待修正 Alda｜只修正反馈涉及的问题】\n");
        message.push_str(score);
    } else if let Some(score) = &request.working_alda {
        message.push_str("\n【当前工作 Alda｜优先继续发展】\n");
        message.push_str(score);
    } else if let Some(score) = &request.current_alda {
        message.push_str("\n【当前有效 Alda】\n");
        message.push_str(score);
    } else {
        message.push_str("\n项目尚无有效版本；请根据对话中的用户请求创作。\n");
    }
    message
}

#[derive(Debug)]
struct SubmittedResult {
    kind: AgentResultKind,
    message: String,
    alda_code: Option<String>,
}

fn parse_submitted_result(args: &str) -> Result<SubmittedResult> {
    let parsed: serde_json::Value =
        serde_json::from_str(args).context("无法解析 submit_result 参数")?;
    let mut kind = match parsed["kind"].as_str() {
        Some("answer") => AgentResultKind::Answer,
        Some("clarification") => AgentResultKind::Clarification,
        Some("plan") => AgentResultKind::Plan,
        Some("draft") => AgentResultKind::Draft,
        Some("candidate") => AgentResultKind::Candidate,
        _ => bail!("submit_result.kind 无效"),
    };
    let mut message = parsed["message"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("message 字段缺失或不是字符串"))?
        .trim()
        .to_string();
    if message.is_empty() {
        bail!("submit_result.message 不能为空");
    }
    if kind == AgentResultKind::Plan {
        let plan = parsed["plan"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("plan 结果必须提供结构化 plan 字段"))?;
        let field = |name: &str| -> Result<&str> {
            let value = plan
                .get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow::anyhow!("plan.{name} 不能为空"))?;
            Ok(value)
        };
        message = format!(
            "{}\n\n核心材料：{}\n曲式：{}\n配器：{}\n发展方式：{}",
            message,
            field("core_material")?,
            field("form")?,
            field("orchestration")?,
            field("development")?,
        );
    }
    if kind == AgentResultKind::Answer && requests_user_input(&message) {
        kind = AgentResultKind::Clarification;
    }
    let code = parsed["alda_code"].as_str();
    if matches!(kind, AgentResultKind::Draft | AgentResultKind::Candidate) && code.is_none() {
        bail!("草稿或完整候选缺少 alda_code");
    }
    let Some(code) = code else {
        return Ok(SubmittedResult {
            kind,
            message,
            alda_code: None,
        });
    };

    // 去除可能的 Markdown 代码块标记
    let code = code.trim();
    let code = code
        .strip_prefix("```alda")
        .or_else(|| code.strip_prefix("```"))
        .map_or(code, str::trim);
    let code = code.strip_suffix("```").map_or(code, str::trim);

    Ok(SubmittedResult {
        kind,
        message,
        alda_code: Some(code.to_string()),
    })
}

fn requests_user_input(message: &str) -> bool {
    message.contains('?')
        || message.contains('？')
        || [
            "请告诉我",
            "请确认",
            "请选择",
            "你希望",
            "你想要",
            "是否要",
            "能否提供",
        ]
        .iter()
        .any(|marker| message.contains(marker))
}

fn build_tool_feedback(checks: &[AldaCheck], _tool_call_id: Option<&str>) -> String {
    let failures: Vec<_> = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Fail)
        .collect();

    if failures.is_empty() {
        return "✅ 所有必要检查通过；未检查项和诊断不影响通过，也不要求归零。宿主将保存工作乐谱，但尚未播放。需要听到声音必须调用 play_score 或由用户执行 /alda play work；只有用户明确接受完整候选才会创建版本。"
            .to_string();
    }

    let mut msg = format!(
        "校验未通过（{}/{} 项硬失败）。只修正【必须修正】；诊断不是失败，不要把它优化成新的创作目标。\n\n【必须修正｜决定能否通过】\n",
        failures.len(),
        checks.len(),
    );

    for check in &failures {
        let _ = writeln!(msg, "❌ {}: {}", check.name, check.detail);
    }

    let diagnostics = checks
        .iter()
        .filter(|check| check.status == CheckStatus::Unchecked)
        .collect::<Vec<_>>();
    if !diagnostics.is_empty() {
        msg.push_str("\n【未检查或诊断｜不作为本轮修正目标】\n");
        for check in diagnostics {
            let _ = writeln!(msg, "- {}: {}", check.name, check.detail);
        }
    }

    let passed = checks
        .iter()
        .filter(|check| check.status == CheckStatus::Pass)
        .collect::<Vec<_>>();
    if !passed.is_empty() {
        msg.push_str("\n【已通过｜保持】\n");
        for check in passed {
            let _ = writeln!(msg, "✅ {}", check.name);
        }
    }

    let duration_bounds = checks
        .iter()
        .find(|c| c.name == "时长" && c.status == CheckStatus::Fail)
        .and_then(|check| parse_duration_bounds(&check.detail));
    if let Some((actual, min_target, max_target)) =
        duration_bounds.filter(|(actual, _, _)| *actual > 0.0)
    {
        let direction = if actual < min_target {
            format!("低于目标下限 {min_target:.0} 秒")
        } else if actual > max_target {
            format!("高于目标上限 {max_target:.0} 秒")
        } else {
            "不在项目允许范围内".to_string()
        };
        let _ = writeln!(
            msg,
            "\n【时长修正策略】当前约 {actual:.0} 秒，{direction}。过短时增加有职责的变奏、对比、发展、再现或尾声；过长时删减冗余循环和无结构作用的材料。不要把短循环按比例复制、持续铺满所有声部或只改 tempo。修改相关材料后先用 inspect_alda_source 读取实际时长。"
        );
    }

    msg.push_str("\n请保持已通过部分，只修正硬失败后重新提交。若无法可靠确认循环、片段或声部长度，停止手算并用 inspect_alda_source 检查 4–16 小节材料。");
    msg
}

/// 从时长检查的 detail 文本中解析实际值与有效目标区间。
/// 支持“约 46秒（目标 180秒，允许偏差 10%）”和“约 46秒（目标 120–180秒）”。
fn parse_duration_bounds(detail: &str) -> Option<(f64, f64, f64)> {
    let after_yue = detail.strip_prefix("约 ")?.split('秒').next()?;
    let actual: f64 = after_yue.trim().parse().ok()?;

    let target_start = detail.find("目标 ")?.checked_add("目标 ".len())?;
    let target_end = detail[target_start..].find('秒')?;
    let target = detail[target_start..target_start + target_end].trim();
    let mut bounds = target.split('–');
    let target_start: f64 = bounds.next()?.trim().parse().ok()?;
    let target_end = bounds
        .next()
        .map(str::trim)
        .map(str::parse)
        .transpose()
        .ok()?;
    if bounds.next().is_some() {
        return None;
    }

    if let Some(target_end) = target_end {
        if target_start > target_end {
            return None;
        }
        return Some((actual, target_start, target_end));
    }

    let tolerance_start = detail.find("允许偏差 ")?.checked_add("允许偏差 ".len())?;
    let tolerance_end = detail[tolerance_start..].find('%')?;
    let tolerance_pct: f64 = detail[tolerance_start..tolerance_start + tolerance_end]
        .trim()
        .parse()
        .ok()?;
    (tolerance_pct.is_finite() && tolerance_pct >= 0.0).then(|| {
        let tolerance = target_start * tolerance_pct / 100.0;
        (actual, target_start - tolerance, target_start + tolerance)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instructions::{CreationMode, ProjectPreferences};
    use crate::test_support::{MockResponse, serve};
    use hound::{SampleFormat, WavSpec, WavWriter};
    use std::os::unix::fs::PermissionsExt;

    fn tool_response(code: &str, finish_reason: &str) -> String {
        let arguments = serde_json::json!({
            "kind": "candidate",
            "message": "完整候选",
            "alda_code": code
        })
        .to_string();
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {
                    "content": "解读与配器说明",
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": { "name": "submit_result", "arguments": arguments }
                    }]
                },
                "finish_reason": finish_reason
            }]
        });
        format!("data: {chunk}\n\ndata: [DONE]\n")
    }

    fn text_response(kind: &str, text: &str) -> String {
        let arguments = serde_json::json!({ "kind": kind, "message": text }).to_string();
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

    fn split_text_response() -> String {
        let fragments = [
            (Some("submit_result"), r#"{"kind":"answer","message":"正在"#),
            (None, r"流式\n返"),
            (None, r#"回"}"#),
        ];
        let mut body = String::new();
        for (index, (name, arguments)) in fragments.into_iter().enumerate() {
            let chunk = serde_json::json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": (index == 0).then_some("call_1"),
                            "function": { "name": name, "arguments": arguments }
                        }]
                    },
                    "finish_reason": (index == 2).then_some("tool_calls")
                }]
            });
            writeln!(body, "data: {chunk}\n").unwrap();
        }
        body.push_str("data: [DONE]\n");
        body
    }

    fn draft_response(code: &str) -> String {
        let arguments = serde_json::json!({
            "kind": "draft",
            "message": "核心草稿",
            "alda_code": code
        })
        .to_string();
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_draft",
                        "function": { "name": "submit_result", "arguments": arguments }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        format!("data: {chunk}\n\ndata: [DONE]\n")
    }

    fn plain_text_response(text: &str) -> String {
        let chunk = serde_json::json!({
            "choices": [{
                "delta": { "content": text },
                "finish_reason": "stop"
            }]
        });
        format!("data: {chunk}\n\ndata: [DONE]\n")
    }

    fn host_tool_response(name: &str, arguments: &serde_json::Value) -> String {
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": format!("call_{name}"),
                        "function": { "name": name, "arguments": arguments.to_string() }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        format!("data: {chunk}\n\ndata: [DONE]\n")
    }

    fn parallel_tool_response() -> String {
        let first = serde_json::json!({
            "kind": "answer",
            "message": "第一个结果"
        })
        .to_string();
        let second = serde_json::json!({ "topic": "aliases" }).to_string();
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call_parallel_1",
                            "function": { "name": "submit_result", "arguments": first }
                        },
                        {
                            "index": 1,
                            "id": "call_parallel_2",
                            "function": { "name": "lookup_alda_docs", "arguments": second }
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }]
        });
        format!("data: {chunk}\n\ndata: [DONE]\n")
    }

    fn malformed_submit_result_response(finish_reason: &str) -> String {
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_malformed",
                        "function": {
                            "name": "submit_result",
                            "arguments": "{\"kind\":\"candidate\",\"message\":\"完整候选\",\"alda_code\":\"piano: c"
                        }
                    }]
                },
                "finish_reason": finish_reason
            }]
        });
        format!("data: {chunk}\n\ndata: [DONE]\n")
    }

    #[derive(Default)]
    struct RecordingReporter {
        events: Vec<AgentEvent>,
    }

    impl AgentReporter for RecordingReporter {
        fn report(&mut self, event: AgentEvent) {
            self.events.push(event);
        }
    }

    fn fake_runner() -> (tempfile::TempDir, AldaRunner) {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("alda");
        let json = r#"{"events":[{"offset":0,"duration":500,"audible-duration":450,"midi-note":60,"part":"piano"}],"parts":{"piano":{"name":"piano","stock-instrument":"midi-acoustic-grand-piano","tempo":120}}}"#;
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n  parse) if [ -s \"$3\" ]; then printf '%s\\n' '{json}'; else printf '%s\\n' '{{\"events\":[],\"parts\":{{}}}}'; fi ;;\n  export) printf 'midi' > \"$5\" ;;\n  play|stop) exit 0 ;;\n  *) exit 1 ;;\nesac\n"
        );
        fs::write(&executable, script).unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        (directory, AldaRunner::new(executable))
    }

    fn progress_runner() -> (tempfile::TempDir, AldaRunner) {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("alda");
        let short = r#"{"events":[{"offset":0,"audible-duration":1000,"part":"piano"}],"parts":{"piano":{"stock-instrument":"midi-acoustic-grand-piano","tempo":120}}}"#;
        let closer = r#"{"events":[{"offset":0,"audible-duration":2000,"part":"piano"}],"parts":{"piano":{"stock-instrument":"midi-acoustic-grand-piano","tempo":120}}}"#;
        let target = r#"{"markers":{"theme":1500,"intro":0},"events":[{"offset":0,"audible-duration":3000,"part":"piano"}],"parts":{"piano":{"stock-instrument":"midi-acoustic-grand-piano","tempo":120}}}"#;
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n  parse)\n    if grep -q syntax_bad \"$3\"; then echo invalid >&2; exit 1\n    elif grep -q closer \"$3\"; then printf '%s\\n' '{closer}'\n    elif grep -q target \"$3\"; then printf '%s\\n' '{target}'\n    else printf '%s\\n' '{short}'\n    fi ;;\n  export) printf midi > \"$5\" ;;\n  play|stop) exit 0 ;;\n  *) exit 1 ;;\nesac\n"
        );
        fs::write(&executable, script).unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        (directory, AldaRunner::new(executable))
    }

    fn fake_audio_renderer(root: &std::path::Path) -> AudioRenderer {
        fake_audio_renderer_with_amplitude(root, 8_000)
    }

    fn fake_audio_renderer_with_amplitude(root: &std::path::Path, amplitude: i16) -> AudioRenderer {
        let source_wav = root.join("render-source.wav");
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
        fs::write(
            &fluidsynth,
            format!("#!/bin/sh\ncp '{}' \"$4\"\n", source_wav.display()),
        )
        .unwrap();
        let mut permissions = fs::metadata(&fluidsynth).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fluidsynth, permissions).unwrap();
        let soundfont = root.join("test.sf2");
        fs::write(&soundfont, "soundfont").unwrap();
        AudioRenderer::new(fluidsynth, soundfont)
    }

    fn compiled_instructions(preferences: &ProjectPreferences) -> CompiledInstructions {
        let catalog = crate::skills::SkillCatalog::discover(None, None).unwrap();
        CompiledInstructions::compile(
            &catalog,
            &crate::instructions::InstructionProfile::default(),
            preferences,
        )
        .unwrap()
    }

    fn test_policy(max_model_calls: usize) -> RunPolicy {
        RunPolicy {
            max_model_calls,
            ..RunPolicy::default()
        }
    }

    fn request(max_model_calls: usize) -> CreationRequest {
        CreationRequest {
            source_material: "素材".to_string(),
            instructions: "创作完整器乐作品".to_string(),
            compiled_instructions: compiled_instructions(&ProjectPreferences::default()),
            run_policy: test_policy(max_model_calls),
        }
    }

    #[test]
    fn compiled_instructions_are_separate_from_the_current_task() {
        let compiled = compiled_instructions(&ProjectPreferences::default());
        assert!(compiled.rendered().contains("submit_result"));
        assert!(
            compiled
                .rendered()
                .contains("builtin:progressive-composition")
        );
        let message = build_user_message(&request(1));
        let current_position = message.find("【本轮要求").unwrap();
        assert!(!message.contains("builtin:progressive-composition"));
        assert!(current_position > 0);
        assert!(message.ends_with("创作完整器乐作品"));
    }

    #[test]
    fn creation_mode_describes_form_without_implying_duration() {
        let full = CreationMode::Full.description();
        let improv = CreationMode::Improv.description();

        assert!(full.contains("结构完整"));
        assert!(improv.contains("自由发展"));
        assert!(full.contains("不预设时长"));
        assert!(improv.contains("不预设时长"));
        assert!(!full.contains("分钟"));
        assert!(!improv.contains("分钟"));
    }

    #[test]
    fn parses_exact_and_range_duration_bounds() {
        let (actual, min_target, max_target) =
            parse_duration_bounds("约 46秒（目标 180秒，允许偏差 10%）").unwrap();
        assert!((actual - 46.0).abs() < f64::EPSILON);
        assert!((min_target - 162.0).abs() < f64::EPSILON);
        assert!((max_target - 198.0).abs() < f64::EPSILON);

        let (actual, min_target, max_target) =
            parse_duration_bounds("约 227秒（目标 120–180秒）").unwrap();
        assert!((actual - 227.0).abs() < f64::EPSILON);
        assert!((min_target - 120.0).abs() < f64::EPSILON);
        assert!((max_target - 180.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_parse_duration_no_match() {
        assert!(parse_duration_bounds("未检查").is_none());
        assert!(parse_duration_bounds("解析成功").is_none());
        assert!(parse_duration_bounds("约 10秒（目标 180–120秒）").is_none());
        assert!(parse_duration_bounds("约 10秒（目标 180秒）").is_none());
    }

    #[test]
    fn feedback_separates_hard_failures_diagnostics_and_passes() {
        let feedback = build_tool_feedback(
            &[
                AldaCheck {
                    name: "Alda 语法",
                    status: CheckStatus::Pass,
                    detail: "解析成功".to_string(),
                },
                AldaCheck {
                    name: "声部时间轴/事件空档",
                    status: CheckStatus::Unchecked,
                    detail: "结尾尾差 4.0秒".to_string(),
                },
                AldaCheck {
                    name: "时长",
                    status: CheckStatus::Fail,
                    detail: "约 227秒（目标 120–180秒）".to_string(),
                },
            ],
            None,
        );

        let failures = feedback.find("【必须修正").unwrap();
        let diagnostics = feedback.find("【未检查或诊断").unwrap();
        let passed = feedback.find("【已通过").unwrap();
        assert!(failures < diagnostics && diagnostics < passed);
        assert!(feedback.contains("❌ 时长: 约 227秒"));
        assert!(feedback.contains("- 声部时间轴/事件空档"));
        assert!(feedback.contains("诊断不是失败"));
        assert!(feedback.contains("高于目标上限 180 秒"));
        assert!(feedback.contains("删减冗余循环"));
        assert!(feedback.contains("inspect_alda_source"));
        assert!(!feedback.contains("精确计算得出"));
    }

    #[test]
    fn compiled_workflow_prevents_validator_driven_filler() {
        let compiled = compiled_instructions(&ProjectPreferences::default());
        let rendered = compiled.rendered();

        assert!(rendered.contains("只把硬失败作为自动修正目标"));
        assert!(rendered.contains("不得为了改善诊断而让所有声部持续铺满"));
        assert!(rendered.contains("不能把同一短循环按比例复制"));
        assert!(rendered.contains("不继续扩大整曲"));
    }

    #[tokio::test]
    async fn truncation_is_reset_after_a_successful_correction() {
        let (base_url, _requests) = serve(vec![
            MockResponse::sse(tool_response("piano: c", "length")),
            MockResponse::sse(tool_response("piano: c", "tool_calls")),
        ]);
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            base_url,
            "example-model".to_string(),
        )
        .unwrap();
        let (directory, runner) = fake_runner();
        let result = Agent::new(client, runner)
            .with_audio_renderer(fake_audio_renderer(directory.path()))
            .create(request(2))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.rounds, 2);
        assert!(!result.was_truncated);
        assert!(result.alda_code.is_some());
        let artifacts = result.candidate_artifacts.as_ref().unwrap();
        assert!(artifacts.midi_path().is_file());
        assert!(artifacts.wav_path().is_file());
        assert!(result.checks.iter().any(|check| {
            check.name == "音频渲染"
                && check.status == CheckStatus::Pass
                && check.detail.contains("非静音")
        }));
        assert!(result.conversation.len() >= 6);
    }

    #[tokio::test]
    async fn improving_candidate_can_succeed_after_more_than_three_submissions() {
        let (base_url, _requests) = serve(vec![
            MockResponse::sse(tool_response("syntax_bad", "tool_calls")),
            MockResponse::sse(tool_response("short", "tool_calls")),
            MockResponse::sse(tool_response("closer", "tool_calls")),
            MockResponse::sse(tool_response("target", "tool_calls")),
        ]);
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            base_url,
            "example-model".to_string(),
        )
        .unwrap();
        let (directory, runner) = progress_runner();
        let mut reporter = RecordingReporter::default();
        let result = Agent::new(client, runner)
            .with_audio_renderer(fake_audio_renderer(directory.path()))
            .run_generation(
                vec![Message {
                    role: "user".to_string(),
                    content: Some("完成三秒作品".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                }],
                ValidationRequest {
                    score: ScoreValidation::new(
                        Some(crate::instructions::DurationConstraint::exact(3.0)),
                        Vec::new(),
                        Vec::new(),
                    ),
                    run_policy: test_policy(8),
                    tool_context: None,
                    require_candidate: true,
                    forbid_clarification: false,
                },
                &mut reporter,
            )
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.rounds, 4);
        assert_eq!(result.alda_code.as_deref(), Some("target"));
        let artifacts = result.candidate_artifacts.as_ref().unwrap();
        assert!(artifacts.wav_path().is_file());
        assert!(result.checks.iter().any(|check| {
            check.name == "音频渲染"
                && check.status == CheckStatus::Pass
                && check.detail.contains("非静音")
        }));
        assert_eq!(
            reporter
                .events
                .iter()
                .filter(|event| matches!(event, AgentEvent::RoundStarted { .. }))
                .count(),
            4
        );
    }

    #[tokio::test]
    async fn silent_candidate_is_check_feedback_and_never_succeeds() {
        let (base_url, _requests) = serve(vec![
            MockResponse::sse(tool_response("piano: c", "tool_calls")),
            MockResponse::sse(tool_response("piano: c", "tool_calls")),
            MockResponse::sse(tool_response("piano: c", "tool_calls")),
        ]);
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            base_url,
            "example-model".to_string(),
        )
        .unwrap();
        let (directory, runner) = fake_runner();
        let result = Agent::new(client, runner)
            .with_audio_renderer(fake_audio_renderer_with_amplitude(directory.path(), 0))
            .create(request(3))
            .await
            .unwrap();

        assert!(!result.success);
        assert_eq!(result.rounds, 3);
        assert!(result.candidate_artifacts.is_none());
        assert!(result.checks.iter().any(|check| {
            check.name == "音频渲染"
                && check.status == CheckStatus::Fail
                && check.detail.contains("静音")
        }));
    }

    #[tokio::test]
    async fn draft_does_not_render_audio() {
        let (base_url, _requests) = serve(vec![MockResponse::sse(draft_response("piano: c"))]);
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            base_url,
            "example-model".to_string(),
        )
        .unwrap();
        let (directory, runner) = fake_runner();
        let unavailable = AudioRenderer::new(
            directory.path().join("missing-fluidsynth"),
            directory.path().join("missing.sf2"),
        );
        let result = Agent::new(client, runner)
            .with_audio_renderer(unavailable)
            .create(request(1))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.kind, AgentResultKind::Draft);
        assert!(result.candidate_artifacts.is_none());
        assert!(result.checks.iter().all(|check| check.name != "音频渲染"));
    }

    #[tokio::test]
    async fn failed_generation_never_claims_a_valid_score() {
        let (base_url, _requests) = serve(vec![MockResponse::sse(tool_response("", "tool_calls"))]);
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            base_url,
            "example-model".to_string(),
        )
        .unwrap();
        let (_directory, runner) = fake_runner();
        let result = Agent::new(client, runner).create(request(1)).await.unwrap();
        assert!(!result.success);
        assert!(
            result
                .checks
                .iter()
                .any(|check| check.name == "作品内容" && check.status == CheckStatus::Fail)
        );
    }

    #[tokio::test]
    async fn explicit_clarification_result_needs_input() {
        let (base_url, _requests) = serve(vec![MockResponse::sse(text_response(
            "clarification",
            "你指的是哪一个段落？",
        ))]);
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            base_url,
            "example-model".to_string(),
        )
        .unwrap();
        let (_directory, runner) = fake_runner();
        let result = Agent::new(client, runner).create(request(1)).await.unwrap();
        assert!(!result.success);
        assert!(result.needs_input);
        assert!(result.interpretation.contains("哪一个段落"));
        assert!(result.checks.is_empty());
    }

    #[tokio::test]
    async fn submitted_message_is_reported_as_streaming_text_deltas() {
        let (base_url, _requests) = serve(vec![MockResponse::sse(split_text_response())]);
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            base_url,
            "example-model".to_string(),
        )
        .unwrap();
        let (_directory, runner) = fake_runner();
        let mut reporter = RecordingReporter::default();
        let result = Agent::new(client, runner)
            .run_generation(
                vec![Message {
                    role: "user".to_string(),
                    content: Some("说明当前构思".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                }],
                ValidationRequest {
                    score: ScoreValidation::new(None, Vec::new(), Vec::new()),
                    run_policy: test_policy(1),
                    tool_context: None,
                    require_candidate: false,
                    forbid_clarification: false,
                },
                &mut reporter,
            )
            .await
            .unwrap();

        assert_eq!(result.kind, AgentResultKind::Answer);
        let streamed = reporter
            .events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ModelText(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(streamed, "正在流式\n返回");
    }

    #[tokio::test]
    async fn required_candidate_rejects_a_draft_and_retries_automatically() {
        let (base_url, _requests) = serve(vec![
            MockResponse::sse(draft_response("piano: c")),
            MockResponse::sse(tool_response("piano: c", "tool_calls")),
        ]);
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            base_url,
            "example-model".to_string(),
        )
        .unwrap();
        let (directory, runner) = fake_runner();
        let result = Agent::new(client, runner)
            .with_audio_renderer(fake_audio_renderer(directory.path()))
            .run_generation(
                vec![Message {
                    role: "user".to_string(),
                    content: Some("编写曲目".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                }],
                ValidationRequest {
                    score: ScoreValidation::new(None, Vec::new(), Vec::new()),
                    run_policy: test_policy(3),
                    tool_context: None,
                    require_candidate: true,
                    forbid_clarification: false,
                },
                &mut SilentReporter,
            )
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.kind, AgentResultKind::Candidate);
        assert_eq!(result.rounds, 2);
        assert!(result.conversation.iter().any(|message| {
            message
                .content
                .as_deref()
                .is_some_and(|content| content.contains("必须提交 candidate"))
        }));
    }

    #[tokio::test]
    async fn declined_optional_preferences_reject_another_clarification() {
        let (base_url, _requests) = serve(vec![
            MockResponse::sse(text_response("clarification", "你还希望选择哪一种配器？")),
            MockResponse::sse(tool_response("piano: c", "tool_calls")),
        ]);
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            base_url,
            "example-model".to_string(),
        )
        .unwrap();
        let (directory, runner) = fake_runner();
        let mut reporter = RecordingReporter::default();
        let result = Agent::new(client, runner)
            .with_audio_renderer(fake_audio_renderer(directory.path()))
            .run_generation(
                vec![Message {
                    role: "user".to_string(),
                    content: Some("没有".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                }],
                ValidationRequest {
                    score: ScoreValidation::new(None, Vec::new(), Vec::new()),
                    run_policy: test_policy(3),
                    tool_context: None,
                    require_candidate: true,
                    forbid_clarification: true,
                },
                &mut reporter,
            )
            .await
            .unwrap();

        assert!(result.success);
        assert!(!result.needs_input);
        assert_eq!(result.kind, AgentResultKind::Candidate);
        assert_eq!(result.rounds, 2);
        assert!(!result.interpretation.contains("选择哪一种配器"));
        assert!(result.conversation.iter().any(|message| {
            message
                .content
                .as_deref()
                .is_some_and(|content| content.contains("用户已明确表示没有额外约束"))
        }));
        assert!(reporter.events.iter().any(|event| {
            matches!(event, AgentEvent::ModelText(text) if text.contains("选择哪一种配器"))
        }));
    }

    #[tokio::test]
    async fn parallel_tool_calls_are_rejected_without_counting_as_submissions() {
        let (base_url, requests) = serve(vec![
            MockResponse::sse(parallel_tool_response()),
            MockResponse::sse(tool_response("piano: c", "tool_calls")),
        ]);
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            base_url,
            "example-model".to_string(),
        )
        .unwrap();
        let (directory, runner) = fake_runner();
        let mut reporter = RecordingReporter::default();
        let result = Agent::new(client, runner)
            .with_audio_renderer(fake_audio_renderer(directory.path()))
            .run_generation(
                vec![Message {
                    role: "user".to_string(),
                    content: Some("编曲".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                }],
                ValidationRequest {
                    score: ScoreValidation::new(None, Vec::new(), Vec::new()),
                    run_policy: test_policy(3),
                    tool_context: None,
                    require_candidate: true,
                    forbid_clarification: false,
                },
                &mut reporter,
            )
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.rounds, 1);
        assert_eq!(
            result
                .conversation
                .iter()
                .filter(|message| {
                    message.role == "tool"
                        && message
                            .content
                            .as_deref()
                            .is_some_and(|content| content.contains("本次调用均未执行"))
                })
                .count(),
            2
        );
        assert_eq!(
            reporter
                .events
                .iter()
                .filter(|event| matches!(event, AgentEvent::RoundStarted { .. }))
                .count(),
            1
        );
        assert!(
            reporter
                .events
                .iter()
                .any(|event| matches!(event, AgentEvent::ToolProtocolRetry { call_count: 2 }))
        );
        assert!(
            reporter
                .events
                .iter()
                .any(|event| matches!(event, AgentEvent::ToolContinuationStarted { turn: 1 }))
        );

        let _first_request = requests.recv().unwrap();
        let second_request = String::from_utf8(requests.recv().unwrap()).unwrap();
        assert_eq!(second_request.matches("本次调用均未执行").count(), 2);
    }

    #[tokio::test]
    async fn missing_tool_call_is_retried_without_counting_as_a_submission() {
        let (base_url, requests) = serve(vec![
            MockResponse::sse(plain_text_response("普通回答，没有工具调用")),
            MockResponse::sse(tool_response("piano: c", "tool_calls")),
        ]);
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            base_url,
            "example-model".to_string(),
        )
        .unwrap();
        let (directory, runner) = fake_runner();
        let mut reporter = RecordingReporter::default();
        let result = Agent::new(client, runner)
            .with_audio_renderer(fake_audio_renderer(directory.path()))
            .run_generation(
                vec![Message {
                    role: "user".to_string(),
                    content: Some("写成器乐圣咏".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                }],
                ValidationRequest {
                    score: ScoreValidation::new(None, Vec::new(), Vec::new()),
                    run_policy: test_policy(3),
                    tool_context: None,
                    require_candidate: true,
                    forbid_clarification: true,
                },
                &mut reporter,
            )
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.rounds, 1);
        assert_eq!(
            reporter
                .events
                .iter()
                .filter(|event| matches!(event, AgentEvent::RoundStarted { .. }))
                .count(),
            1
        );
        assert!(
            reporter
                .events
                .iter()
                .any(|event| matches!(event, AgentEvent::ToolCallMissingRetry))
        );
        assert!(
            reporter
                .events
                .iter()
                .any(|event| matches!(event, AgentEvent::ToolContinuationStarted { turn: 1 }))
        );
        assert!(reporter.events.iter().any(|event| {
            matches!(event, AgentEvent::ModelText(text) if text.contains("普通回答"))
        }));

        let _first_request = requests.recv().unwrap();
        let second_request = String::from_utf8(requests.recv().unwrap()).unwrap();
        assert!(second_request.contains("本轮没有调用工具"));
        assert!(second_request.contains("普通回答，没有工具调用"));
    }

    #[tokio::test]
    async fn model_call_guard_stops_protocol_recovery_without_counting_a_submission() {
        let (base_url, _requests) = serve(vec![
            MockResponse::sse(plain_text_response("仍未调用工具")),
            MockResponse::sse(plain_text_response("还是没有调用工具")),
        ]);
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            base_url,
            "example-model".to_string(),
        )
        .unwrap();
        let (_directory, runner) = fake_runner();
        let result = Agent::new(client, runner)
            .run_generation(
                vec![Message {
                    role: "user".to_string(),
                    content: Some("写成器乐圣咏".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                }],
                ValidationRequest {
                    score: ScoreValidation::new(None, Vec::new(), Vec::new()),
                    run_policy: test_policy(2),
                    tool_context: None,
                    require_candidate: true,
                    forbid_clarification: true,
                },
                &mut SilentReporter,
            )
            .await
            .unwrap();

        assert!(!result.success);
        assert_eq!(result.rounds, 0);
        assert_eq!(
            result.stats,
            GenerationStats {
                model_calls: 2,
                tool_turns: 2,
                protocol_recoveries: 2,
                submissions: 0,
            }
        );
        assert!(result.checks.iter().any(|check| {
            check.name == "运行策略" && check.detail.contains("2 次模型调用")
        }));
    }

    #[tokio::test]
    async fn protocol_guard_returns_the_last_failed_candidate() {
        let (base_url, _requests) = serve(vec![
            MockResponse::sse(tool_response("short", "tool_calls")),
            MockResponse::sse(plain_text_response("未调用工具")),
            MockResponse::sse(plain_text_response("仍未调用工具")),
        ]);
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            base_url,
            "example-model".to_string(),
        )
        .unwrap();
        let (_directory, runner) = progress_runner();
        let result = Agent::new(client, runner)
            .run_generation(
                vec![Message {
                    role: "user".to_string(),
                    content: Some("完成三秒作品".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                }],
                ValidationRequest {
                    score: ScoreValidation::new(
                        Some(crate::instructions::DurationConstraint::exact(3.0)),
                        Vec::new(),
                        Vec::new(),
                    ),
                    run_policy: RunPolicy {
                        max_model_calls: 4,
                        max_protocol_recoveries: 1,
                        ..RunPolicy::default()
                    },
                    tool_context: None,
                    require_candidate: true,
                    forbid_clarification: false,
                },
                &mut SilentReporter,
            )
            .await
            .unwrap();

        assert!(!result.success);
        assert_eq!(result.alda_code.as_deref(), Some("short"));
        assert!(result.terminal_error.is_some());
        assert!(result.checks.iter().any(|check| {
            check.name == "运行策略" && check.detail.contains("协议恢复超过 1 次")
        }));
    }

    #[tokio::test]
    async fn missing_score_code_recovery_returns_the_last_failed_candidate() {
        let (base_url, _requests) = serve(vec![
            MockResponse::sse(tool_response("short", "tool_calls")),
            MockResponse::sse(text_response("candidate", "缺少源码")),
            MockResponse::sse(text_response("candidate", "仍缺少源码")),
        ]);
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            base_url,
            "example-model".to_string(),
        )
        .unwrap();
        let (_directory, runner) = progress_runner();
        let result = Agent::new(client, runner)
            .run_generation(
                vec![Message {
                    role: "user".to_string(),
                    content: Some("完成三秒作品".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                }],
                ValidationRequest {
                    score: ScoreValidation::new(
                        Some(crate::instructions::DurationConstraint::exact(3.0)),
                        Vec::new(),
                        Vec::new(),
                    ),
                    run_policy: RunPolicy {
                        max_model_calls: 4,
                        max_protocol_recoveries: 1,
                        ..RunPolicy::default()
                    },
                    tool_context: None,
                    require_candidate: true,
                    forbid_clarification: false,
                },
                &mut SilentReporter,
            )
            .await
            .unwrap();

        assert!(!result.success);
        assert_eq!(result.alda_code.as_deref(), Some("short"));
        assert!(result.terminal_error.is_some());
        assert!(result.checks.iter().any(|check| {
            check.name == "运行策略" && check.detail.contains("协议恢复超过 1 次")
        }));
    }

    #[tokio::test]
    async fn malformed_submit_result_is_retried_without_counting_as_a_submission() {
        let (base_url, requests) = serve(vec![
            MockResponse::sse(malformed_submit_result_response("length")),
            MockResponse::sse(tool_response("piano: c", "tool_calls")),
        ]);
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            base_url,
            "example-model".to_string(),
        )
        .unwrap();
        let (directory, runner) = fake_runner();
        let mut reporter = RecordingReporter::default();
        let result = Agent::new(client, runner)
            .with_audio_renderer(fake_audio_renderer(directory.path()))
            .create_with_reporter(request(3), &mut reporter)
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.rounds, 1);
        assert!(reporter.events.iter().any(|event| {
            matches!(
                event,
                AgentEvent::ToolArgumentsRetry { tool_name }
                    if tool_name == "submit_result"
            )
        }));
        assert!(
            reporter
                .events
                .iter()
                .any(|event| matches!(event, AgentEvent::ToolContinuationStarted { turn: 1 }))
        );

        let _first_request = requests.recv().unwrap();
        let second_request = String::from_utf8(requests.recv().unwrap()).unwrap();
        assert!(second_request.contains("模型响应被截断"));
        assert!(second_request.contains("不计作候选提交"));
    }

    #[test]
    fn structured_plan_is_self_contained_and_visible() {
        let submitted = parse_submitted_result(
            &serde_json::json!({
                "kind": "plan",
                "message": "咏叹调创作计划",
                "plan": {
                    "core_material": "三句歌词各形成一个旋律短句",
                    "form": "引子—A—B—A'—尾声",
                    "orchestration": "大提琴主唱，竖琴与弦乐伴奏",
                    "development": "通过移调、扩展和织体增厚走向高潮"
                }
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(submitted.kind, AgentResultKind::Plan);
        assert!(submitted.message.contains("核心材料："));
        assert!(submitted.message.contains("曲式：引子—A—B—A'—尾声"));
        assert!(submitted.message.contains("配器："));
        assert!(submitted.message.contains("发展方式："));
    }

    #[test]
    fn incomplete_plan_is_rejected() {
        let error = parse_submitted_result(
            &serde_json::json!({ "kind": "plan", "message": "以上为创作计划" }).to_string(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("结构化 plan"));
    }

    #[test]
    fn inspect_alda_source_tool_is_always_available_and_bounded() {
        let tools = model_tools(false);
        let tool = tools
            .iter()
            .find(|tool| tool.function.name == "inspect_alda_source")
            .expect("temporary source inspection should not require project context");

        assert_eq!(
            tool.function.parameters["properties"]["alda_code"]["maxLength"],
            MAX_INSPECT_ALDA_SOURCE_BYTES
        );
        assert_eq!(
            tool.function.parameters["properties"]["scope"]["enum"],
            serde_json::json!(["fragment", "candidate"])
        );
        assert_eq!(
            tool.function.parameters["required"],
            serde_json::json!(["alda_code", "scope"])
        );
    }

    #[tokio::test]
    async fn inspect_alda_source_reports_real_timing_failures_and_size_limit() {
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            "http://127.0.0.1:1".to_string(),
            "example-model".to_string(),
        )
        .unwrap();
        let (directory, runner) = progress_runner();
        let agent = Agent::new(client, runner);
        let validation = ScoreValidation::new(None, Vec::new(), Vec::new());

        let valid = agent
            .execute_model_tool(
                "inspect_alda_source",
                &serde_json::json!({
                    "alda_code": "midi-acoustic-grand-piano: target",
                    "scope": "fragment"
                })
                .to_string(),
                None,
                &validation,
            )
            .await
            .unwrap();
        assert!(valid.candidate_checkpoint.is_none());
        let valid: serde_json::Value = serde_json::from_str(&valid.content).unwrap();
        assert_eq!(valid["parse_ok"], true);
        assert_eq!(valid["duration_secs"], 3.0);
        assert_eq!(
            valid["markers"],
            serde_json::json!([
                {"name": "intro", "offset_secs": 0.0},
                {"name": "theme", "offset_secs": 1.5}
            ])
        );
        assert_eq!(valid["parts"][0]["name"], "piano");
        assert_eq!(valid["parts"][0]["end_secs"], 3.0);
        assert_eq!(valid["parts"][0]["event_count"], 1);
        assert_eq!(valid["hard_failures"], serde_json::json!([]));
        assert!(valid["diagnostics"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|item| item["name"] == "声部时间轴/事件空档")
        }));

        let invalid = agent
            .execute_model_tool(
                "inspect_alda_source",
                &serde_json::json!({
                    "alda_code": "midi-piano: syntax_bad",
                    "scope": "fragment"
                })
                .to_string(),
                None,
                &validation,
            )
            .await
            .unwrap();
        let invalid: serde_json::Value = serde_json::from_str(&invalid.content).unwrap();
        assert_eq!(invalid["parse_ok"], false);
        assert!(invalid["duration_secs"].is_null());
        assert_eq!(invalid["markers"], serde_json::json!([]));
        assert!(
            invalid["hard_failures"]
                .as_array()
                .is_some_and(|items| { items.iter().any(|item| item["name"] == "Alda 语法") })
        );

        let oversized_source = "c".repeat(MAX_INSPECT_ALDA_SOURCE_BYTES + 1);
        let oversized = agent
            .execute_model_tool(
                "inspect_alda_source",
                &serde_json::json!({
                    "alda_code": oversized_source,
                    "scope": "fragment"
                })
                .to_string(),
                None,
                &validation,
            )
            .await
            .unwrap();
        let oversized: serde_json::Value = serde_json::from_str(&oversized.content).unwrap();
        assert_eq!(oversized["parse_ok"], false);
        assert_eq!(oversized["markers"], serde_json::json!([]));
        assert_eq!(oversized["hard_failures"][0]["name"], "源码大小");
        assert!(
            oversized["hard_failures"][0]["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("4–16 小节"))
        );

        let entries = fs::read_dir(directory.path()).unwrap().count();
        assert_eq!(
            entries, 1,
            "inspection must not persist source beside the project"
        );
    }

    #[tokio::test]
    async fn candidate_source_inspection_uses_project_constraints_but_fragment_does_not() {
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            "http://127.0.0.1:1".to_string(),
            "example-model".to_string(),
        )
        .unwrap();
        let (_directory, runner) = progress_runner();
        let agent = Agent::new(client, runner);
        let validation = ScoreValidation::new(
            Some(crate::instructions::DurationConstraint::exact(60.0)),
            vec!["midi-flute".to_string()],
            Vec::new(),
        );
        let arguments = |scope| {
            serde_json::json!({
                "alda_code": "midi-acoustic-grand-piano: target",
                "scope": scope
            })
            .to_string()
        };

        let fragment = agent
            .execute_model_tool(
                "inspect_alda_source",
                &arguments("fragment"),
                None,
                &validation,
            )
            .await
            .unwrap();
        let fragment_json: serde_json::Value = serde_json::from_str(&fragment.content).unwrap();
        assert!(fragment.candidate_checkpoint.is_none());
        assert_eq!(fragment_json["hard_failures"], serde_json::json!([]));

        let candidate = agent
            .execute_model_tool(
                "inspect_alda_source",
                &arguments("candidate"),
                None,
                &validation,
            )
            .await
            .unwrap();
        let candidate_json: serde_json::Value = serde_json::from_str(&candidate.content).unwrap();
        let checkpoint = candidate.candidate_checkpoint.unwrap();
        assert_eq!(checkpoint.alda_code, "midi-acoustic-grand-piano: target");
        assert!(
            checkpoint
                .checks
                .iter()
                .any(|check| { check.name == "时长" && check.status == CheckStatus::Fail })
        );
        assert!(
            candidate_json["hard_failures"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["name"] == "包含乐器"))
        );
    }

    #[tokio::test]
    async fn candidate_source_inspection_is_a_checkpoint_not_a_submission_gate() {
        let source = "midi-acoustic-grand-piano: target";
        let (base_url, _requests) = serve(vec![MockResponse::sse(host_tool_response(
            "inspect_alda_source",
            &serde_json::json!({ "alda_code": source, "scope": "candidate" }),
        ))]);
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            base_url,
            "example-model".to_string(),
        )
        .unwrap();
        let (_directory, runner) = progress_runner();
        let result = Agent::new(client, runner)
            .run_generation(
                vec![Message {
                    role: "user".to_string(),
                    content: Some("检查完整候选后继续工作".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                }],
                ValidationRequest {
                    score: ScoreValidation::new(
                        Some(crate::instructions::DurationConstraint::exact(3.0)),
                        Vec::new(),
                        Vec::new(),
                    ),
                    run_policy: test_policy(1),
                    tool_context: None,
                    require_candidate: true,
                    forbid_clarification: false,
                },
                &mut SilentReporter,
            )
            .await
            .unwrap();

        assert!(!result.success);
        assert_eq!(
            result.recovery_checkpoint,
            Some(RecoveryCheckpoint::InspectedCandidate)
        );
        assert_eq!(result.alda_code.as_deref(), Some(source));
        assert_eq!(result.rounds, 0);
        assert_eq!(
            result.stats,
            GenerationStats {
                model_calls: 1,
                tool_turns: 1,
                protocol_recoveries: 0,
                submissions: 0,
            }
        );
        assert!(result.checks.iter().any(|check| {
            check.name == "运行策略" && check.detail.contains("1 次模型调用")
        }));
    }

    #[test]
    fn answer_that_requests_input_becomes_clarification() {
        let submitted = parse_submitted_result(
            &serde_json::json!({
                "kind": "answer",
                "message": "你希望偏歌剧还是室内乐风格？"
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(submitted.kind, AgentResultKind::Clarification);
    }

    #[tokio::test]
    async fn host_tools_run_in_sequence_without_counting_as_submissions() {
        let (base_url, _requests) = serve(vec![
            MockResponse::sse(host_tool_response(
                "lookup_alda_docs",
                &serde_json::json!({ "topic": "aliases" }),
            )),
            MockResponse::sse(host_tool_response(
                "inspect_alda_source",
                &serde_json::json!({
                    "alda_code": "piano: c d e f",
                    "scope": "fragment"
                }),
            )),
            MockResponse::sse(host_tool_response(
                "inspect_score",
                &serde_json::json!({ "target": "current" }),
            )),
            MockResponse::sse(host_tool_response(
                "render_score",
                &serde_json::json!({ "target": "current" }),
            )),
            MockResponse::sse(host_tool_response(
                "play_score",
                &serde_json::json!({ "target": "current" }),
            )),
            MockResponse::sse(text_response("answer", "已检查、渲染并播放当前版本")),
        ]);
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            base_url,
            "example-model".to_string(),
        )
        .unwrap();
        let (directory, runner) = fake_runner();
        let current = directory.path().join("current.alda");
        fs::write(&current, "piano: c").unwrap();
        let agent = Agent {
            client,
            runner,
            audio_renderer: Some(fake_audio_renderer(directory.path())),
        };
        let result = agent
            .run_generation(
                vec![Message {
                    role: "user".to_string(),
                    content: Some("检查并试听".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                }],
                ValidationRequest {
                    score: ScoreValidation::new(None, Vec::new(), Vec::new()),
                    run_policy: RunPolicy {
                        max_model_calls: 6,
                        max_protocol_recoveries: 1,
                        ..RunPolicy::default()
                    },
                    tool_context: Some(AgentToolContext {
                        project_root: directory.path().to_path_buf(),
                        current_path: Some(current),
                        working_path: None,
                    }),
                    require_candidate: false,
                    forbid_clarification: false,
                },
                &mut SilentReporter,
            )
            .await
            .unwrap();

        assert_eq!(result.rounds, 1);
        assert_eq!(
            result.stats,
            GenerationStats {
                model_calls: 6,
                tool_turns: 5,
                protocol_recoveries: 0,
                submissions: 1,
            }
        );
        assert_eq!(result.kind, AgentResultKind::Answer);
        assert_eq!(result.played_target.as_deref(), Some("current"));
        assert!(
            result
                .rendered_wav
                .as_ref()
                .is_some_and(|path| path.is_file())
        );
        let tool_results = result
            .conversation
            .iter()
            .filter(|message| message.role == "tool")
            .filter_map(|message| message.content.as_deref())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(tool_results.contains("Alda official release-2.4.3"));
        assert!(tool_results.contains("\"duration_secs\":0.45"));
        assert!(tool_results.contains("event_count"));
        assert!(tool_results.contains("\"silent\":false"));
        assert!(tool_results.contains("\"played\":\"current\""));
    }

    #[tokio::test]
    async fn repeated_identical_failure_continues_until_the_model_call_guard() {
        let (base_url, _requests) = serve(vec![
            MockResponse::sse(tool_response("", "tool_calls")),
            MockResponse::sse(tool_response("", "tool_calls")),
            MockResponse::sse(tool_response("", "tool_calls")),
        ]);
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            base_url,
            "example-model".to_string(),
        )
        .unwrap();
        let (_directory, runner) = fake_runner();
        let result = Agent::new(client, runner).create(request(3)).await.unwrap();

        assert_eq!(result.rounds, 3);
        assert!(!result.success);
        assert!(!result.checks.iter().any(|check| check.name == "修正进展"));
        assert!(result.checks.iter().any(|check| {
            check.name == "运行策略" && check.detail.contains("3 次模型调用")
        }));
    }
}
