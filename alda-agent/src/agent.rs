use crate::alda::{AldaCheck, AldaRunner, CheckStatus, ScoreInfo, ScoreValidation};
use crate::audio::{ArtifactReport, AudioRenderer};
use crate::conversation::{ConversationMessage, ConversationRole, ConversationToolCall};
use crate::deepseek::{DeepSeekClient, FunctionDef, Message, StreamDelta, StreamEvent, Tool};
use crate::instructions::{CompiledInstructions, DurationConstraint};
use crate::project::FormPlan;
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
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

fn form_plan_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "target_duration_secs": { "type": "number", "exclusiveMinimum": 0 },
            "sections": {
                "type": "array",
                "minItems": 4,
                "maxItems": 10,
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "pattern": "^[a-z][a-z0-9_]*$" },
                        "target_start_secs": { "type": "number", "minimum": 0 },
                        "target_end_secs": { "type": "number", "exclusiveMinimum": 0 },
                        "function": { "type": "string", "minLength": 1 },
                        "material_action": { "type": "string", "enum": ["introduce", "develop", "contrast", "reprise", "close"] },
                        "energy": { "type": "string", "enum": ["low", "medium", "high", "peak"] }
                    },
                    "required": ["id", "target_start_secs", "target_end_secs", "function", "material_action", "energy"]
                }
            }
        },
        "required": ["target_duration_secs", "sections"]
    })
}

fn edit_scope_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "mode": { "type": "string", "enum": ["local", "global"] },
            "target_sections": {
                "type": "array",
                "items": { "type": "string", "pattern": "^[a-z][a-z0-9_]*$" },
                "uniqueItems": true
            },
            "intent": { "type": "string", "minLength": 1 }
        },
        "required": ["mode", "target_sections", "intent"]
    })
}

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
                    "candidate_ref": {
                        "type": "object",
                        "description": "kind=candidate 时可引用最近一次通过 inspect_alda_source(scope=candidate) 的检查点，避免重复完整源码",
                        "properties": {
                            "source_hash": { "type": "string", "pattern": "^[0-9a-f]{64}$" }
                        },
                        "required": ["source_hash"]
                    },
                    "form_plan": form_plan_schema(),
                    "edit_scope": edit_scope_schema(),
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

fn play_score_tool() -> Tool {
    Tool {
        ty: "function".to_string(),
        function: FunctionDef {
            name: "play_score".to_string(),
            description: "真实发起播放当前或工作乐谱。默认播放整曲；只有需要定位局部问题时才传 section_id，宿主会按 Marker 段落并附带前后上下文播放。只有工具成功后才能告诉用户已经播放。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": { "type": "string", "enum": ["work", "current"] },
                    "section_id": {
                        "type": "string",
                        "pattern": "^(section_)?[a-z][a-z0-9_]*$",
                        "description": "可选 form_plan 段落 id；可省略 section_ 前缀"
                    },
                    "context_secs": {
                        "type": "integer",
                        "minimum": 5,
                        "maximum": 15,
                        "default": 10,
                        "description": "局部播放时在段落前后附加的上下文秒数"
                    }
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
            description: "真实解析尚未提交的 Alda 临时源码，返回总时长、Marker 划分的段落边界/事件/声部覆盖、各声部结束时间，并分开报告硬失败与诊断。fragment 只检查局部材料且不保留；candidate 使用项目完整约束并作为故障恢复检查点，但不会保存工作乐谱、渲染或计作正式提交。".to_string(),
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
                    },
                    "form_plan": form_plan_schema()
                    ,"edit_scope": edit_scope_schema()
                },
                "required": ["alda_code", "scope"]
            }),
        },
    }
}

fn inspect_alda_fragment_tool() -> Tool {
    Tool {
        ty: "function".to_string(),
        function: FunctionDef {
            name: "inspect_alda_source".to_string(),
            description: "真实解析尚未提交的 Alda 临时片段，返回时长、Marker、事件、声部覆盖和语法检查；只允许 scope=fragment，不读取或更新项目候选检查点。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "alda_code": {
                        "type": "string",
                        "description": "需要独立检查的 Alda 临时片段",
                        "maxLength": MAX_INSPECT_ALDA_SOURCE_BYTES
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["fragment"],
                        "description": "固定为 fragment"
                    }
                },
                "required": ["alda_code", "scope"]
            }),
        },
    }
}

fn inspect_alda_patch_tool() -> Tool {
    Tool {
        ty: "function".to_string(),
        function: FunctionDef {
            name: "inspect_alda_patch".to_string(),
            description: "对 work/current 基线执行 1–8 个唯一文本替换，在内存中生成候选并运行完整候选、form_plan 与 edit_scope 检查；不会修改项目文件。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "base": {
                        "type": "object",
                        "properties": {
                            "kind": { "type": "string", "enum": ["work", "current"] },
                            "source_hash": { "type": "string", "pattern": "^[0-9a-f]{64}$" }
                        },
                        "required": ["kind", "source_hash"]
                    },
                    "replacements": {
                        "type": "array", "minItems": 1, "maxItems": 8,
                        "items": {
                            "type": "object",
                            "properties": {
                                "old": { "type": "string", "minLength": 1 },
                                "new": { "type": "string" }
                            },
                            "required": ["old", "new"]
                        }
                    },
                    "form_plan": form_plan_schema(),
                    "edit_scope": edit_scope_schema()
                },
                "required": ["base", "replacements", "form_plan", "edit_scope"]
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

fn delegate_tool() -> Tool {
    Tool {
        ty: "function".to_string(),
        function: FunctionDef {
            name: "delegate".to_string(),
            description: "把一个边界清晰的音乐设计、Alda 实现或只读复核任务交给独立 subagent，并取得其文本结果。subagent 不继承当前对话；请在 context 中提供完成任务所需的信息。它可查询 Alda 文档、检查临时片段，并在项目会话中只读检查 work/current；结果不会修改项目，由你判断、整合并通过现有完整检查。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "minLength": 1,
                        "description": "交给 subagent 的单一、可直接执行的任务"
                    },
                    "context": {
                        "type": "string",
                        "description": "可选；完成任务所需的规格、约束、相关 Alda 源码或待复核内容"
                    }
                },
                "required": ["task"]
            }),
        },
    }
}

fn delegate_messages(arguments: &str) -> Result<Vec<Message>> {
    let parsed = serde_json::from_str::<serde_json::Value>(arguments)?;
    let task = parsed["task"].as_str().context("task 缺失")?.trim();
    if task.is_empty() {
        bail!("task 不能为空");
    }
    let context = match parsed.get("context") {
        None => "",
        Some(value) => value.as_str().context("context 必须是字符串")?.trim(),
    };
    let user_message = if context.is_empty() {
        format!("【委派任务】\n{task}")
    } else {
        format!("【委派任务】\n{task}\n\n【上下文】\n{context}")
    };
    Ok(vec![
        Message {
            role: "system".to_string(),
            content: Some(include_str!("../prompts/subagent.md").to_string()),
            tool_calls: None,
            tool_call_id: None,
        },
        Message {
            role: "user".to_string(),
            content: Some(user_message),
            tool_calls: None,
            tool_call_id: None,
        },
    ])
}

fn model_tools(host_tools: bool) -> Vec<Tool> {
    let mut tools = vec![
        submit_result_tool(),
        delegate_tool(),
        lookup_docs_tool(),
        inspect_alda_source_tool(),
    ];
    if host_tools {
        tools.extend([
            inspect_alda_patch_tool(),
            score_tool("inspect_score", "真实解析并检查当前或工作乐谱，返回源码哈希、时长、段落、声部、事件、乐器和约束检查；源码哈希可直接作为 inspect_alda_patch 的基线。"),
            score_tool("render_score", "真实导出 MIDI 并用 FluidSynth 渲染 WAV，返回音频时长、采样率、峰值、RMS 和静音判断。"),
            play_score_tool(),
        ]);
    }
    tools
}

fn subagent_tools(project_context: bool) -> Vec<Tool> {
    let mut tools = vec![lookup_docs_tool(), inspect_alda_fragment_tool()];
    if project_context {
        tools.push(score_tool(
            "inspect_score",
            "只读解析并检查当前或工作乐谱，返回源码哈希、时长、段落、声部、事件、乐器和项目约束检查；不返回源码，也不修改项目。",
        ));
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
    /// 与候选一同验证并持久化的长曲结构计划。
    pub form_plan: Option<FormPlan>,
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
    pub delegations: usize,
    pub tool_turns: usize,
    pub protocol_recoveries: usize,
    pub submissions: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryCheckpoint {
    InspectedCandidate,
}

#[derive(Debug, Clone)]
struct CandidateCheckpoint {
    alda_code: String,
    checks: Vec<AldaCheck>,
    form_plan: Option<FormPlan>,
    source_hash: String,
    edit_scope: Option<EditScope>,
}

#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
struct EditScope {
    mode: EditMode,
    #[serde(default)]
    target_sections: Vec<String>,
    intent: String,
}

#[derive(Debug, Clone, Copy, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum EditMode {
    Local,
    Global,
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
    let checks = [AldaCheck {
        name: "源码大小",
        status: CheckStatus::Fail,
        detail: size_detail,
    }];
    let content = serde_json::json!({
        "scope": scope,
        "parse_ok": false,
        "duration_secs": null,
        "markers": [],
        "sections": [],
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
        candidate_checkpoint: None,
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
    pub form_plan: Option<FormPlan>,
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
    pub revision_path: Option<PathBuf>,
    pub form_plan: Option<FormPlan>,
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
        let mut delegations = 0_usize;
        let mut tool_turns = 0_usize;
        let mut protocol_recoveries = 0_usize;
        let mut interpretation = String::new();
        let mut last_alda_code = None;
        let mut last_form_plan = None;
        let mut last_checks = Vec::new();
        let mut last_was_truncated = false;
        let mut score_kind = None;
        let mut played_target = None;
        let mut rendered_wav = None;
        let mut candidate_artifacts = None;
        let mut candidate_checkpoint: Option<CandidateCheckpoint> = None;
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
                let outcome = if tool_name == "delegate" {
                    match delegate_messages(&tool_args) {
                        Err(error) => Err(error),
                        Ok(_) if model_calls.saturating_add(1) >= max_model_calls => {
                            Err(anyhow::anyhow!(
                                "delegate 还需要一次 subagent 调用和一次 Composer 续写，但本轮只剩不足两次模型调用额度"
                            ))
                        }
                        Ok(delegate_messages) => {
                            delegations += 1;
                            let subagent_call_limit =
                                max_model_calls.saturating_sub(model_calls.saturating_add(1));
                            self.execute_delegate(
                                delegate_messages,
                                validation.tool_context.as_ref(),
                                &validation.score,
                                subagent_call_limit,
                                &mut model_calls,
                                &mut tool_turns,
                            )
                            .await
                        }
                    }
                } else {
                    self.execute_model_tool(
                        &tool_name,
                        &tool_args,
                        validation.tool_context.as_ref(),
                        &validation.score,
                    )
                    .await
                };
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
                        last_form_plan.clone_from(&checkpoint.form_plan);
                        last_checks.clone_from(&checkpoint.checks);
                        last_was_truncated = was_truncated;
                        interpretation = "完整候选检查点（尚未正式提交）".to_string();
                        candidate_checkpoint = Some(checkpoint.clone());
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

            let submitted = match parse_submitted_result(&tool_args).and_then(|submitted| {
                resolve_candidate_reference(submitted, candidate_checkpoint.as_ref())
            }) {
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
                        delegations,
                        tool_turns,
                        protocol_recoveries,
                        submissions: round,
                    },
                    success: false,
                    needs_input: submitted.kind == AgentResultKind::Clarification,
                    kind: submitted.kind,
                    checks: Vec::new(),
                    alda_code: None,
                    form_plan: None,
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
            candidate_checkpoint = None;
            let alda_code = submitted
                .alda_code
                .context("草稿或完整候选缺少 alda_code")?;
            let form_plan = submitted.form_plan;
            let edit_scope = submitted.edit_scope;
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
            if submitted.kind == AgentResultKind::Candidate {
                let info = self.runner.parse(&tmp_score).ok();
                if let Some(check) = form_plan_check(
                    info.as_ref(),
                    form_plan.as_ref(),
                    requires_form_plan(&validation.score),
                ) {
                    checks.push(check);
                }
                if let Some(check) = edit_scope_check(
                    &self.runner,
                    validation.tool_context.as_ref(),
                    info.as_ref(),
                    form_plan.as_ref(),
                    edit_scope.as_ref(),
                ) {
                    checks.push(check);
                }
            }
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
            last_form_plan.clone_from(&form_plan);
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
                        delegations,
                        tool_turns,
                        protocol_recoveries,
                        submissions: round,
                    },
                    success: true,
                    needs_input: false,
                    kind: submitted.kind,
                    checks,
                    alda_code: Some(alda_code),
                    form_plan,
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
                delegations,
                tool_turns,
                protocol_recoveries,
                submissions: round,
            },
            success: false,
            needs_input: false,
            kind: if candidate_checkpoint.is_some() {
                AgentResultKind::Candidate
            } else {
                score_kind.unwrap_or(AgentResultKind::Candidate)
            },
            checks: last_checks,
            alda_code: last_alda_code,
            form_plan: last_form_plan,
            interpretation,
            was_truncated: last_was_truncated,
            conversation: messages,
            played_target,
            rendered_wav,
            candidate_artifacts: None,
            recovery_checkpoint: candidate_checkpoint
                .is_some()
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
            return self
                .inspect_alda_source(arguments, validation, context)
                .await;
        }
        if name == "inspect_alda_patch" {
            let context = context.context("当前调用没有项目乐谱上下文")?;
            return self
                .inspect_alda_patch(arguments, validation, context)
                .await;
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
                let source =
                    fs::read(&path).with_context(|| format!("无法读取乐谱 {}", path.display()))?;
                let source_hash = format!("{:x}", Sha256::digest(&source));
                let checks = self.runner.validate_async(path, validation.clone()).await?;
                Ok(ModelToolResult::content(
                    serde_json::json!({
                        "ok": true,
                        "source_hash": source_hash,
                        "info": info,
                        "checks": checks
                    })
                    .to_string(),
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
            "play_score" => self.execute_play_score(&parsed, target, path).await,
            _ => bail!("未知模型工具 {name:?}"),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_delegate(
        &self,
        mut messages: Vec<Message>,
        context: Option<&AgentToolContext>,
        validation: &ScoreValidation,
        call_limit: usize,
        model_calls: &mut usize,
        tool_turns: &mut usize,
    ) -> Result<ModelToolResult> {
        let tools = subagent_tools(context.is_some());
        for call in 1..=call_limit {
            *model_calls += 1;
            let events = self
                .client
                .chat_stream(messages.clone(), Some(tools.clone()))
                .await?;
            let mut result = String::new();
            let mut calls = Vec::new();
            let mut truncated = false;
            for event in events {
                match event {
                    StreamEvent::Text(text) => result.push_str(&text),
                    StreamEvent::ToolCall {
                        id,
                        name,
                        arguments,
                    } => calls.push((id, name, arguments)),
                    StreamEvent::Done { finish_reason } => {
                        truncated = finish_reason == "length";
                    }
                }
            }
            if calls.len() > 1 {
                bail!("subagent 一次响应返回了多个工具调用，全部未执行");
            }
            let Some((tool_id, tool_name, tool_args)) = calls.pop() else {
                if result.trim().is_empty() {
                    bail!("subagent 未返回结果");
                }
                return Ok(ModelToolResult::content(
                    serde_json::json!({
                        "ok": true,
                        "result": result,
                        "truncated": truncated
                    })
                    .to_string(),
                ));
            };
            if call == call_limit {
                bail!("subagent 已用完委派模型调用额度，无法继续处理最后一次工具结果");
            }
            *tool_turns += 1;
            let tool_call_id = tool_id.unwrap_or_else(|| format!("subagent_call_{call}"));
            messages.push(tool_call_message(
                &tool_call_id,
                &tool_name,
                &tool_args,
                result,
            ));
            let outcome = self
                .execute_subagent_tool(&tool_name, &tool_args, context, validation)
                .await;
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
        }
        bail!("subagent 未在委派模型调用额度内返回结果")
    }

    async fn execute_subagent_tool(
        &self,
        name: &str,
        arguments: &str,
        context: Option<&AgentToolContext>,
        validation: &ScoreValidation,
    ) -> Result<ModelToolResult> {
        match name {
            "lookup_alda_docs" => lookup_alda_docs(arguments).map(ModelToolResult::content),
            "inspect_alda_source" => {
                let parsed = serde_json::from_str::<serde_json::Value>(arguments)?;
                if parsed["scope"].as_str() != Some("fragment") {
                    bail!("subagent 的 inspect_alda_source 只允许 scope=fragment");
                }
                self.inspect_alda_source(arguments, validation, None).await
            }
            "inspect_score" if context.is_some() => {
                self.execute_model_tool(name, arguments, context, validation)
                    .await
            }
            "inspect_score" => bail!("当前委派没有项目乐谱上下文，不能调用 inspect_score"),
            _ => bail!("subagent 不允许调用工具 {name:?}"),
        }
    }

    async fn execute_play_score(
        &self,
        arguments: &serde_json::Value,
        target: &str,
        path: PathBuf,
    ) -> Result<ModelToolResult> {
        let Some(section_id) = arguments["section_id"].as_str() else {
            self.runner.play_async(path).await?;
            return Ok(ModelToolResult::content(
                serde_json::json!({ "ok": true, "played": target }).to_string(),
            ));
        };
        let context_secs = arguments["context_secs"]
            .as_u64()
            .and_then(|seconds| u32::try_from(seconds).ok())
            .unwrap_or(10)
            .clamp(5, 15);
        let marker_name = if section_id.starts_with("section_") {
            section_id.to_string()
        } else {
            format!("section_{section_id}")
        };
        let info = self.runner.parse(&path)?;
        let section = info
            .sections
            .iter()
            .find(|section| section.name == marker_name)
            .with_context(|| format!("乐谱中没有段落 {section_id:?}"))?;
        let context_ms = f64::from(context_secs) * 1000.0;
        let from_ms = (section.start_ms - context_ms).max(0.0);
        let to_ms = (section.end_ms + context_ms).min(info.duration_ms);
        self.runner
            .play_range_async(path, alda_time_marking(from_ms), alda_time_marking(to_ms))
            .await?;
        Ok(ModelToolResult::content(
            serde_json::json!({
                "ok": true,
                "played": target,
                "section_id": section_id,
                "from_secs": from_ms / 1000.0,
                "to_secs": to_ms / 1000.0,
                "context_secs": context_secs
            })
            .to_string(),
        ))
    }

    async fn inspect_alda_source(
        &self,
        arguments: &str,
        validation: &ScoreValidation,
        context: Option<&AgentToolContext>,
    ) -> Result<ModelToolResult> {
        let parsed = serde_json::from_str::<serde_json::Value>(arguments)?;
        let source = parsed["alda_code"].as_str().context("alda_code 缺失")?;
        let scope = parsed["scope"].as_str().context("scope 缺失")?;
        let candidate = match scope {
            "fragment" => false,
            "candidate" => true,
            _ => bail!("scope 必须是 fragment 或 candidate"),
        };
        let form_plan = parse_form_plan(&parsed["form_plan"])?;
        let edit_scope = parse_edit_scope(&parsed["edit_scope"])?;
        if !candidate && form_plan.is_some() {
            bail!("form_plan 只适用于 scope=candidate");
        }
        if !candidate && edit_scope.is_some() {
            bail!("edit_scope 只适用于 scope=candidate");
        }
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
        let mut checks = self
            .runner
            .validate_async(path.clone(), score_validation)
            .await?;
        let parse_ok = checks
            .iter()
            .any(|check| check.name == "Alda 语法" && check.status == CheckStatus::Pass);
        let info = parse_ok.then(|| self.runner.parse(&path)).transpose()?;
        if candidate {
            if let Some(check) = form_plan_check(
                info.as_ref(),
                form_plan.as_ref(),
                requires_form_plan(validation),
            ) {
                checks.push(check);
            }
        }
        if candidate {
            if let Some(check) = edit_scope_check(
                &self.runner,
                context,
                info.as_ref(),
                form_plan.as_ref(),
                edit_scope.as_ref(),
            ) {
                checks.push(check);
            }
        }
        let source_hash = (candidate
            && checks.iter().all(|check| check.status != CheckStatus::Fail))
        .then(|| format!("{:x}", Sha256::digest(source.as_bytes())));
        let inspection_json = alda_inspection_json(
            scope,
            parse_ok,
            info.as_ref(),
            source_hash.as_deref(),
            &checks,
        );
        Ok(ModelToolResult {
            content: inspection_json,
            candidate_checkpoint: source_hash.map(|source_hash| CandidateCheckpoint {
                alda_code: source.to_string(),
                checks,
                form_plan,
                source_hash,
                edit_scope,
            }),
        })
    }

    async fn inspect_alda_patch(
        &self,
        arguments: &str,
        validation: &ScoreValidation,
        context: &AgentToolContext,
    ) -> Result<ModelToolResult> {
        let mut parsed = serde_json::from_str::<serde_json::Value>(arguments)?;
        let base_kind = parsed["base"]["kind"].as_str().context("base.kind 缺失")?;
        let expected_hash = parsed["base"]["source_hash"]
            .as_str()
            .context("base.source_hash 缺失")?;
        let base_path = match base_kind {
            "work" => context.working_path.as_ref().context("项目没有工作乐谱")?,
            "current" => context
                .current_path
                .as_ref()
                .context("项目没有当前有效版本")?,
            _ => bail!("base.kind 必须是 work 或 current"),
        };
        let active_baseline = context
            .revision_path
            .as_ref()
            .or(context.working_path.as_ref())
            .or(context.current_path.as_ref())
            .context("项目没有可用的修改基线")?;
        if base_path != active_baseline {
            bail!("补丁必须基于最新工作基线；存在更新的恢复候选时请提交完整候选");
        }
        let source = fs::read_to_string(base_path)
            .with_context(|| format!("无法读取补丁基线 {}", base_path.display()))?;
        let actual_hash = format!("{:x}", Sha256::digest(source.as_bytes()));
        if actual_hash != expected_hash {
            bail!("补丁基线 source_hash 已失效；请重新读取当前乐谱");
        }
        let replacements = parsed["replacements"]
            .as_array()
            .context("replacements 缺失")?;
        if !(1..=8).contains(&replacements.len()) {
            bail!("replacements 必须包含 1–8 项");
        }
        let mut edits = Vec::with_capacity(replacements.len());
        for replacement in replacements {
            let old = replacement["old"]
                .as_str()
                .context("replacement.old 缺失")?;
            let new = replacement["new"]
                .as_str()
                .context("replacement.new 缺失")?;
            if old.is_empty() {
                bail!("replacement.old 不能为空");
            }
            let matches = source.match_indices(old).collect::<Vec<_>>();
            if matches.len() != 1 {
                bail!(
                    "每个 replacement.old 必须在基线中恰好出现一次；{old:?} 出现 {} 次",
                    matches.len()
                );
            }
            let start = matches[0].0;
            edits.push((start, start + old.len(), new));
        }
        edits.sort_by_key(|(start, _, _)| *start);
        if edits.windows(2).any(|pair| pair[0].1 > pair[1].0) {
            bail!("replacements 不能相互重叠");
        }
        let mut candidate = String::with_capacity(source.len());
        let mut cursor = 0;
        for (start, end, new) in edits {
            candidate.push_str(&source[cursor..start]);
            candidate.push_str(new);
            cursor = end;
        }
        candidate.push_str(&source[cursor..]);

        parsed["alda_code"] = serde_json::Value::String(candidate);
        parsed["scope"] = serde_json::Value::String("candidate".to_string());
        parsed
            .as_object_mut()
            .expect("tool arguments are an object")
            .remove("base");
        parsed
            .as_object_mut()
            .expect("tool arguments are an object")
            .remove("replacements");
        self.inspect_alda_source(&parsed.to_string(), validation, Some(context))
            .await
    }
}

fn alda_time_marking(milliseconds: f64) -> String {
    let total_seconds = milliseconds.max(0.0) / 1000.0;
    let minutes = (total_seconds / 60.0).floor();
    format!("{minutes:.0}:{:.3}", total_seconds - minutes * 60.0)
}

// ============================================================
// 辅助函数
// ============================================================

fn inspection_sections(info: &ScoreInfo) -> Vec<serde_json::Value> {
    info.sections
        .iter()
        .map(|section| {
            let parts = section
                .parts
                .iter()
                .map(|part| {
                    serde_json::json!({
                        "name": part.part,
                        "event_count": part.event_count,
                        "sounding_secs": part.sounding_ms / 1000.0,
                        "coverage_ratio": part.coverage_ratio
                    })
                })
                .collect::<Vec<_>>();
            serde_json::json!({
                "name": section.name,
                "start_secs": section.start_ms / 1000.0,
                "end_secs": section.end_ms / 1000.0,
                "event_count": section.event_count,
                "parts": parts
            })
        })
        .collect()
}

fn alda_inspection_json(
    scope: &str,
    parse_ok: bool,
    info: Option<&ScoreInfo>,
    source_hash: Option<&str>,
    checks: &[AldaCheck],
) -> String {
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
    let duration_secs = info.map(|score| score.duration_ms / 1000.0);
    let parts = info
        .map(|score| {
            score
                .timeline
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
        .map(|score| {
            score
                .markers
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
    let sections = info.map_or_else(Vec::new, inspection_sections);
    serde_json::json!({
        "scope": scope,
        "parse_ok": parse_ok,
        "duration_secs": duration_secs,
        "markers": markers,
        "sections": sections,
        "parts": parts,
        "source_hash": source_hash,
        "hard_failures": hard_failures,
        "diagnostics": diagnostics
    })
    .to_string()
}

pub(crate) fn requires_form_plan(validation: &ScoreValidation) -> bool {
    validation
        .target_duration()
        .is_some_and(|duration| match duration {
            DurationConstraint::Exact(seconds) => seconds >= 120.0,
            DurationConstraint::Range { min_secs, .. } => min_secs >= 120.0,
        })
}

fn parse_form_plan(value: &serde_json::Value) -> Result<Option<FormPlan>> {
    if value.is_null() {
        return Ok(None);
    }
    let plan = serde_json::from_value::<FormPlan>(value.clone()).context("form_plan 结构无效")?;
    plan.validate()?;
    Ok(Some(plan))
}

pub(crate) fn form_plan_check(
    info: Option<&ScoreInfo>,
    form_plan: Option<&FormPlan>,
    required: bool,
) -> Option<AldaCheck> {
    let Some(plan) = form_plan else {
        return required.then(|| AldaCheck {
            name: "曲式计划",
            status: CheckStatus::Fail,
            detail: "目标时长下限不少于 120 秒，完整候选必须提供 form_plan".to_string(),
        });
    };
    let Some(info) = info else {
        return Some(AldaCheck {
            name: "曲式计划",
            status: CheckStatus::Fail,
            detail: "Alda 解析失败，无法核对 form_plan 与 Marker".to_string(),
        });
    };
    let expected = plan
        .sections
        .iter()
        .map(|section| format!("section_{}", section.id))
        .collect::<Vec<_>>();
    let actual = info
        .markers
        .iter()
        .map(|marker| marker.name.clone())
        .collect::<Vec<_>>();
    if actual != expected {
        return Some(AldaCheck {
            name: "曲式计划",
            status: CheckStatus::Fail,
            detail: format!(
                "Marker 必须按计划精确对应；期望 {}，实际 {}",
                expected.join(", "),
                actual.join(", ")
            ),
        });
    }
    for ((section, marker), expected_name) in plan.sections.iter().zip(&info.markers).zip(&expected)
    {
        let target_duration = section.target_end_secs - section.target_start_secs;
        let tolerance = 2.0_f64.max(target_duration * 0.1);
        let actual_start = marker.offset_ms / 1000.0;
        if (actual_start - section.target_start_secs).abs() > tolerance {
            return Some(AldaCheck {
                name: "曲式计划",
                status: CheckStatus::Fail,
                detail: format!(
                    "%{expected_name} 位于 {actual_start:.1}秒，计划 {:.1}秒，超出 ±{tolerance:.1}秒容差",
                    section.target_start_secs
                ),
            });
        }
    }
    let final_section = plan
        .sections
        .last()
        .expect("form plan has at least four sections");
    let final_tolerance =
        2.0_f64.max((final_section.target_end_secs - final_section.target_start_secs) * 0.1);
    let actual_end = info.duration_ms / 1000.0;
    if (actual_end - final_section.target_end_secs).abs() > final_tolerance {
        return Some(AldaCheck {
            name: "曲式计划",
            status: CheckStatus::Fail,
            detail: format!(
                "全曲结束于 {actual_end:.1}秒，计划 {:.1}秒，超出 ±{final_tolerance:.1}秒容差",
                final_section.target_end_secs
            ),
        });
    }
    Some(AldaCheck {
        name: "曲式计划",
        status: CheckStatus::Pass,
        detail: format!("{} 个计划段落与 Marker 顺序及边界对齐", plan.sections.len()),
    })
}

fn edit_scope_check(
    runner: &AldaRunner,
    context: Option<&AgentToolContext>,
    candidate_info: Option<&ScoreInfo>,
    candidate_plan: Option<&FormPlan>,
    edit_scope: Option<&EditScope>,
) -> Option<AldaCheck> {
    let baseline_plan = context.and_then(|context| context.form_plan.as_ref());
    let Some(baseline_plan) = baseline_plan else {
        return edit_scope.map(|scope| AldaCheck {
            name: "修改范围",
            status: if scope.mode == EditMode::Global {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail
            },
            detail: if scope.mode == EditMode::Global {
                "首次结构化创作采用全局模式".to_string()
            } else {
                "没有带 form_plan 的基线，不能执行局部修改".to_string()
            },
        });
    };
    let Some(scope) = edit_scope else {
        return Some(AldaCheck {
            name: "修改范围",
            status: CheckStatus::Fail,
            detail: "已有结构化乐谱；新候选必须声明 local 或 global edit_scope".to_string(),
        });
    };
    if scope.mode == EditMode::Global {
        return Some(AldaCheck {
            name: "修改范围",
            status: CheckStatus::Pass,
            detail: format!("全局重写：{}", scope.intent.trim()),
        });
    }

    let targets = scope
        .target_sections
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    if let Err(failure) = check_local_plan(baseline_plan, candidate_plan, &targets) {
        return Some(failure);
    }
    Some(check_local_event_hashes(
        runner,
        context,
        candidate_info,
        baseline_plan,
        scope,
        &targets,
    ))
}

fn check_local_plan(
    baseline_plan: &FormPlan,
    candidate_plan: Option<&FormPlan>,
    targets: &std::collections::BTreeSet<&str>,
) -> std::result::Result<(), AldaCheck> {
    if let Some(unknown) = targets.iter().find(|id| {
        !baseline_plan
            .sections
            .iter()
            .any(|section| section.id == ***id)
    }) {
        return Err(edit_scope_failure(format!(
            "目标段落 {unknown:?} 不在基线 form_plan 中"
        )));
    }
    let Some(candidate_plan) = candidate_plan else {
        return Err(edit_scope_failure("局部修改缺少候选 form_plan"));
    };
    if baseline_plan.sections.len() != candidate_plan.sections.len()
        || baseline_plan
            .sections
            .iter()
            .zip(&candidate_plan.sections)
            .any(|(base, candidate)| base.id != candidate.id)
    {
        return Err(edit_scope_failure(
            "局部修改不能增加、删除或重排 form_plan 段落",
        ));
    }
    for (base, candidate) in baseline_plan.sections.iter().zip(&candidate_plan.sections) {
        if targets.contains(base.id.as_str()) {
            continue;
        }
        let base_duration = base.target_end_secs - base.target_start_secs;
        let candidate_duration = candidate.target_end_secs - candidate.target_start_secs;
        if base.function != candidate.function
            || base.material_action != candidate.material_action
            || base.energy != candidate.energy
            || (base_duration - candidate_duration).abs() > 0.001
        {
            return Err(edit_scope_failure(format!(
                "保持段落 {:?} 的职责、材料动作、能量或目标时长发生变化",
                base.id
            )));
        }
    }
    Ok(())
}

fn check_local_event_hashes(
    runner: &AldaRunner,
    context: Option<&AgentToolContext>,
    candidate_info: Option<&ScoreInfo>,
    baseline_plan: &FormPlan,
    scope: &EditScope,
    targets: &std::collections::BTreeSet<&str>,
) -> AldaCheck {
    let Some(context) = context else {
        return edit_scope_failure("局部修改没有项目基线上下文");
    };
    let baseline_path = context
        .revision_path
        .as_ref()
        .or(context.working_path.as_ref())
        .or(context.current_path.as_ref());
    let Some(baseline_path) = baseline_path else {
        return edit_scope_failure("局部修改没有可读取的基线乐谱");
    };
    let baseline_info = match runner.parse(baseline_path) {
        Ok(info) => info,
        Err(error) => return edit_scope_failure(format!("无法解析基线乐谱：{error}")),
    };
    let Some(candidate_info) = candidate_info else {
        return edit_scope_failure("无法解析候选乐谱");
    };
    let baseline_hashes = baseline_info
        .sections
        .iter()
        .map(|section| (section.name.as_str(), section.event_hash.as_str()))
        .collect::<BTreeMap<_, _>>();
    let candidate_hashes = candidate_info
        .sections
        .iter()
        .map(|section| (section.name.as_str(), section.event_hash.as_str()))
        .collect::<BTreeMap<_, _>>();
    let changed = baseline_plan
        .sections
        .iter()
        .filter(|section| !targets.contains(section.id.as_str()))
        .filter_map(|section| {
            let marker = format!("section_{}", section.id);
            (baseline_hashes.get(marker.as_str()) != candidate_hashes.get(marker.as_str()))
                .then_some(section.id.as_str())
        })
        .collect::<Vec<_>>();
    if changed.is_empty() {
        AldaCheck {
            name: "修改范围",
            status: CheckStatus::Pass,
            detail: format!(
                "仅允许修改 {}；其余段落事件保持",
                scope.target_sections.join(", ")
            ),
        }
    } else {
        edit_scope_failure(format!("非目标段落事件发生变化：{}", changed.join(", ")))
    }
}

fn edit_scope_failure(detail: impl Into<String>) -> AldaCheck {
    AldaCheck {
        name: "修改范围",
        status: CheckStatus::Fail,
        detail: detail.into(),
    }
}

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
    if let Some(form_plan) = &request.form_plan {
        message.push_str("\n【当前结构计划｜段落 id 对应 %section_<id>】\n");
        message.push_str(
            &serde_json::to_string(form_plan)
                .expect("serializing a validated form plan cannot fail"),
        );
        message.push('\n');
    }
    message
}

#[derive(Debug)]
struct SubmittedResult {
    kind: AgentResultKind,
    message: String,
    alda_code: Option<String>,
    candidate_ref: Option<String>,
    form_plan: Option<FormPlan>,
    edit_scope: Option<EditScope>,
}

fn parse_edit_scope(value: &serde_json::Value) -> Result<Option<EditScope>> {
    if value.is_null() {
        return Ok(None);
    }
    let scope =
        serde_json::from_value::<EditScope>(value.clone()).context("edit_scope 结构无效")?;
    if scope.intent.trim().is_empty() {
        bail!("edit_scope.intent 不能为空");
    }
    match scope.mode {
        EditMode::Local if scope.target_sections.is_empty() => {
            bail!("local edit_scope 必须指定 target_sections")
        }
        EditMode::Global if !scope.target_sections.is_empty() => {
            bail!("global edit_scope 的 target_sections 必须为空")
        }
        _ => {}
    }
    let unique = scope
        .target_sections
        .iter()
        .collect::<std::collections::BTreeSet<_>>();
    if unique.len() != scope.target_sections.len() {
        bail!("edit_scope.target_sections 不能重复");
    }
    Ok(Some(scope))
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
    let candidate_ref = parsed["candidate_ref"]["source_hash"]
        .as_str()
        .map(str::to_string);
    let form_plan = parse_form_plan(&parsed["form_plan"])?;
    let edit_scope = parse_edit_scope(&parsed["edit_scope"])?;
    match kind {
        AgentResultKind::Draft if code.is_none() => bail!("草稿缺少 alda_code"),
        AgentResultKind::Draft if candidate_ref.is_some() => {
            bail!("草稿不能使用 candidate_ref")
        }
        AgentResultKind::Candidate if code.is_some() == candidate_ref.is_some() => {
            bail!("完整候选必须且只能提供 alda_code 或 candidate_ref.source_hash 之一")
        }
        AgentResultKind::Candidate if candidate_ref.is_some() && form_plan.is_some() => {
            bail!("candidate_ref 已绑定检查点 form_plan，引用提交不能重复提供 form_plan")
        }
        AgentResultKind::Candidate if candidate_ref.is_some() && edit_scope.is_some() => {
            bail!("candidate_ref 已绑定检查点 edit_scope，引用提交不能重复提供 edit_scope")
        }
        AgentResultKind::Answer | AgentResultKind::Clarification | AgentResultKind::Plan
            if code.is_some()
                || candidate_ref.is_some()
                || form_plan.is_some()
                || edit_scope.is_some() =>
        {
            bail!("文本结果不能携带乐谱源码、候选引用或 form_plan")
        }
        _ => {}
    }
    let Some(code) = code else {
        return Ok(SubmittedResult {
            kind,
            message,
            alda_code: None,
            candidate_ref,
            form_plan,
            edit_scope,
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
        candidate_ref,
        form_plan,
        edit_scope,
    })
}

fn resolve_candidate_reference(
    mut submitted: SubmittedResult,
    checkpoint: Option<&CandidateCheckpoint>,
) -> Result<SubmittedResult> {
    let Some(source_hash) = submitted.candidate_ref.take() else {
        return Ok(submitted);
    };
    let checkpoint = checkpoint.context(
        "candidate_ref 不可用：本轮尚无通过 inspect_alda_source(scope=candidate) 的检查点",
    )?;
    if checkpoint.source_hash != source_hash {
        bail!("candidate_ref 只能引用本轮最近一次有效检查点的 source_hash");
    }
    submitted.alda_code = Some(checkpoint.alda_code.clone());
    submitted.form_plan.clone_from(&checkpoint.form_plan);
    submitted.edit_scope.clone_from(&checkpoint.edit_scope);
    Ok(submitted)
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

    fn section_runner() -> (tempfile::TempDir, AldaRunner) {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("alda");
        let markers =
            r#""section_intro":0,"section_theme":1000,"section_climax":2000,"section_coda":3000"#;
        let parts = r#""piano":{"name":"piano","stock-instrument":"midi-piano","tempo":120}"#;
        let baseline = format!(
            r#"{{"markers":{{{markers}}},"events":[{{"offset":100,"audible-duration":500,"midi-note":60,"part":"piano"}},{{"offset":1100,"audible-duration":500,"midi-note":60,"part":"piano"}},{{"offset":2100,"audible-duration":500,"midi-note":60,"part":"piano"}},{{"offset":3100,"audible-duration":500,"midi-note":60,"part":"piano"}}],"parts":{{{parts}}}}}"#
        );
        let target_changed = baseline.replacen(
            r#""offset":2100,"audible-duration":500,"midi-note":60"#,
            r#""offset":2100,"audible-duration":500,"midi-note":61"#,
            1,
        );
        let non_target_changed = baseline.replacen(
            r#""offset":1100,"audible-duration":500,"midi-note":60"#,
            r#""offset":1100,"audible-duration":500,"midi-note":62"#,
            1,
        );
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n  parse)\n    if grep -q non_target_changed \"$3\"; then printf '%s\\n' '{non_target_changed}'\n    elif grep -q target_changed \"$3\"; then printf '%s\\n' '{target_changed}'\n    else printf '%s\\n' '{baseline}'\n    fi ;;\n  play|stop) exit 0 ;;\n  *) exit 1 ;;\nesac\n"
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
                delegations: 0,
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

    #[test]
    fn delegate_tool_is_available_without_project_context() {
        let tools = model_tools(false);
        let tool = tools
            .iter()
            .find(|tool| tool.function.name == "delegate")
            .expect("delegate should not require project context");

        assert_eq!(
            tool.function.parameters["required"],
            serde_json::json!(["task"])
        );
        assert!(tool.function.parameters["properties"]["context"].is_object());
    }

    #[test]
    fn subagent_tools_are_read_only_and_project_aware() {
        let without_project = subagent_tools(false);
        let names = without_project
            .iter()
            .map(|tool| tool.function.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["lookup_alda_docs", "inspect_alda_source"]);
        assert_eq!(
            without_project[1].function.parameters["properties"]["scope"]["enum"],
            serde_json::json!(["fragment"])
        );
        assert!(
            without_project[1].function.parameters["properties"]
                .get("form_plan")
                .is_none()
        );

        let with_project = subagent_tools(true);
        let names = with_project
            .iter()
            .map(|tool| tool.function.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            ["lookup_alda_docs", "inspect_alda_source", "inspect_score"]
        );
        assert!(!names.iter().any(|name| matches!(
            *name,
            "submit_result" | "delegate" | "inspect_alda_patch" | "render_score" | "play_score"
        )));
    }

    #[tokio::test]
    async fn composer_can_delegate_and_receive_an_isolated_result() {
        let task = "为高潮设计核心动机的三种变形";
        let context = "D 小调，4/4，保持原有附点节奏";
        let delegated_result = "倒影、增值和移位模进三种方案";
        let (base_url, requests) = serve(vec![
            MockResponse::sse(host_tool_response(
                "delegate",
                &serde_json::json!({ "task": task, "context": context }),
            )),
            MockResponse::sse(plain_text_response(delegated_result)),
            MockResponse::sse(text_response("answer", "已整合委派结果")),
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
                    content: Some("分析高潮材料".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                }],
                ValidationRequest {
                    score: ScoreValidation::new(None, Vec::new(), Vec::new()),
                    run_policy: test_policy(3),
                    tool_context: None,
                    require_candidate: false,
                    forbid_clarification: false,
                },
                &mut SilentReporter,
            )
            .await
            .unwrap();

        assert!(!result.success);
        assert_eq!(result.kind, AgentResultKind::Answer);
        assert_eq!(result.interpretation, "已整合委派结果");
        assert_eq!(
            result.stats,
            GenerationStats {
                model_calls: 3,
                delegations: 1,
                tool_turns: 1,
                protocol_recoveries: 0,
                submissions: 1,
            }
        );

        let main_request = String::from_utf8(requests.recv().unwrap()).unwrap();
        let subagent_request = String::from_utf8(requests.recv().unwrap()).unwrap();
        let continuation_request = String::from_utf8(requests.recv().unwrap()).unwrap();
        assert!(main_request.contains("\"name\":\"delegate\""));
        assert!(subagent_request.contains(task));
        assert!(subagent_request.contains(context));
        assert!(subagent_request.contains("\"name\":\"lookup_alda_docs\""));
        assert!(subagent_request.contains("\"name\":\"inspect_alda_source\""));
        assert!(!subagent_request.contains("\"name\":\"inspect_score\""));
        assert!(!subagent_request.contains("\"name\":\"delegate\""));
        assert!(!subagent_request.contains("分析高潮材料"));
        assert!(continuation_request.contains(delegated_result));
    }

    #[tokio::test]
    async fn subagent_can_use_all_available_read_only_tools_before_returning() {
        let delegated_result = "已根据文档、片段解析和当前乐谱完成复核";
        let (base_url, requests) = serve(vec![
            MockResponse::sse(host_tool_response(
                "delegate",
                &serde_json::json!({ "task": "复核当前乐谱与新片段" }),
            )),
            MockResponse::sse(host_tool_response(
                "lookup_alda_docs",
                &serde_json::json!({ "topic": "notes" }),
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
            MockResponse::sse(plain_text_response(delegated_result)),
            MockResponse::sse(text_response("answer", "Composer 已整合复核结果")),
        ]);
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            base_url,
            "example-model".to_string(),
        )
        .unwrap();
        let (directory, runner) = fake_runner();
        let current_path = directory.path().join("current.alda");
        let current_source = "piano: c d e f";
        let current_hash = format!("{:x}", Sha256::digest(current_source.as_bytes()));
        fs::write(&current_path, current_source).unwrap();
        let result = Agent::new(client, runner)
            .run_generation(
                vec![Message {
                    role: "user".to_string(),
                    content: Some("分析当前乐谱".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                }],
                ValidationRequest {
                    score: ScoreValidation::new(None, Vec::new(), Vec::new()),
                    run_policy: test_policy(6),
                    tool_context: Some(AgentToolContext {
                        project_root: directory.path().to_path_buf(),
                        current_path: Some(current_path),
                        working_path: None,
                        revision_path: None,
                        form_plan: None,
                    }),
                    require_candidate: false,
                    forbid_clarification: false,
                },
                &mut SilentReporter,
            )
            .await
            .unwrap();

        assert_eq!(result.interpretation, "Composer 已整合复核结果");
        assert_eq!(result.stats.model_calls, 6);
        assert_eq!(result.stats.delegations, 1);
        assert_eq!(result.stats.tool_turns, 4);
        assert_eq!(result.stats.submissions, 1);

        let _main_request = requests.recv().unwrap();
        let subagent_request = String::from_utf8(requests.recv().unwrap()).unwrap();
        let after_docs = String::from_utf8(requests.recv().unwrap()).unwrap();
        let after_fragment = String::from_utf8(requests.recv().unwrap()).unwrap();
        let after_score = String::from_utf8(requests.recv().unwrap()).unwrap();
        let composer_continuation = String::from_utf8(requests.recv().unwrap()).unwrap();
        assert!(subagent_request.contains("\"name\":\"lookup_alda_docs\""));
        assert!(subagent_request.contains("\"name\":\"inspect_alda_source\""));
        assert!(subagent_request.contains("\"name\":\"inspect_score\""));
        assert!(after_docs.contains("\"role\":\"tool\""));
        assert!(after_fragment.contains("parse_ok"));
        assert!(after_score.contains(&current_hash));
        assert!(composer_continuation.contains(delegated_result));
        assert!(!composer_continuation.contains("parse_ok"));
        assert!(!composer_continuation.contains(&current_hash));
    }

    #[tokio::test]
    async fn subagent_cannot_inspect_candidate_scope() {
        let (base_url, requests) = serve(vec![
            MockResponse::sse(host_tool_response(
                "delegate",
                &serde_json::json!({ "task": "检查片段" }),
            )),
            MockResponse::sse(host_tool_response(
                "inspect_alda_source",
                &serde_json::json!({
                    "alda_code": "piano: c",
                    "scope": "candidate"
                }),
            )),
            MockResponse::sse(plain_text_response("已收到越权拒绝")),
            MockResponse::sse(text_response("answer", "Composer 已接管完整候选检查")),
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
                    content: Some("检查候选".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                }],
                ValidationRequest {
                    score: ScoreValidation::new(None, Vec::new(), Vec::new()),
                    run_policy: test_policy(4),
                    tool_context: None,
                    require_candidate: false,
                    forbid_clarification: false,
                },
                &mut SilentReporter,
            )
            .await
            .unwrap();

        assert_eq!(result.stats.model_calls, 4);
        assert_eq!(result.stats.delegations, 1);
        assert_eq!(result.stats.tool_turns, 2);
        assert!(result.recovery_checkpoint.is_none());

        let _main_request = requests.recv().unwrap();
        let _subagent_request = requests.recv().unwrap();
        let subagent_continuation = String::from_utf8(requests.recv().unwrap()).unwrap();
        assert!(subagent_continuation.contains("只允许 scope=fragment"));
    }

    #[tokio::test]
    async fn subagent_runtime_rejects_unlisted_and_projectless_tools() {
        let (base_url, _requests) = serve(Vec::new());
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            base_url,
            "example-model".to_string(),
        )
        .unwrap();
        let (_directory, runner) = fake_runner();
        let agent = Agent::new(client, runner);
        let validation = ScoreValidation::new(None, Vec::new(), Vec::new());

        for name in [
            "submit_result",
            "delegate",
            "inspect_alda_patch",
            "render_score",
            "play_score",
        ] {
            let error = agent
                .execute_subagent_tool(name, "{}", None, &validation)
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("不允许调用工具"),
                "unexpected error for {name}: {error:#}"
            );
        }

        let error = agent
            .execute_subagent_tool(
                "inspect_score",
                &serde_json::json!({ "target": "current" }).to_string(),
                None,
                &validation,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("没有项目乐谱上下文"));

        let error = agent
            .execute_subagent_tool(
                "inspect_alda_source",
                &serde_json::json!({
                    "alda_code": "piano: c",
                    "scope": "candidate",
                    "form_plan": []
                })
                .to_string(),
                None,
                &validation,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("只允许 scope=fragment"));
    }

    #[tokio::test]
    async fn subagent_parallel_tool_calls_are_not_executed() {
        let (base_url, requests) = serve(vec![
            MockResponse::sse(host_tool_response(
                "delegate",
                &serde_json::json!({ "task": "查询后复核" }),
            )),
            MockResponse::sse(parallel_tool_response()),
            MockResponse::sse(text_response("answer", "Composer 接管并行调用错误")),
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
                    content: Some("复核材料".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                }],
                ValidationRequest {
                    score: ScoreValidation::new(None, Vec::new(), Vec::new()),
                    run_policy: test_policy(3),
                    tool_context: None,
                    require_candidate: false,
                    forbid_clarification: false,
                },
                &mut SilentReporter,
            )
            .await
            .unwrap();

        assert_eq!(result.interpretation, "Composer 接管并行调用错误");
        assert_eq!(result.stats.model_calls, 3);
        assert_eq!(result.stats.delegations, 1);
        assert_eq!(result.stats.tool_turns, 1);

        let _main_request = requests.recv().unwrap();
        let _subagent_request = requests.recv().unwrap();
        let composer_continuation = String::from_utf8(requests.recv().unwrap()).unwrap();
        assert!(composer_continuation.contains("多个工具调用，全部未执行"));
    }

    #[tokio::test]
    async fn subagent_tool_loop_preserves_composer_continuation_budget() {
        let (base_url, requests) = serve(vec![
            MockResponse::sse(host_tool_response(
                "delegate",
                &serde_json::json!({ "task": "连续查询文档" }),
            )),
            MockResponse::sse(host_tool_response(
                "lookup_alda_docs",
                &serde_json::json!({ "topic": "notes" }),
            )),
            MockResponse::sse(host_tool_response(
                "lookup_alda_docs",
                &serde_json::json!({ "topic": "repeats" }),
            )),
            MockResponse::sse(text_response("answer", "Composer 使用剩余额度完成")),
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
                    content: Some("查询后回答".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                }],
                ValidationRequest {
                    score: ScoreValidation::new(None, Vec::new(), Vec::new()),
                    run_policy: test_policy(4),
                    tool_context: None,
                    require_candidate: false,
                    forbid_clarification: false,
                },
                &mut SilentReporter,
            )
            .await
            .unwrap();

        assert_eq!(result.interpretation, "Composer 使用剩余额度完成");
        assert_eq!(result.stats.model_calls, 4);
        assert_eq!(result.stats.delegations, 1);
        assert_eq!(result.stats.tool_turns, 2);

        let _main_request = requests.recv().unwrap();
        let _first_subagent_request = requests.recv().unwrap();
        let _second_subagent_request = requests.recv().unwrap();
        let composer_continuation = String::from_utf8(requests.recv().unwrap()).unwrap();
        assert!(composer_continuation.contains("无法继续处理最后一次工具结果"));
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn delegate_preserves_one_model_call_for_composer_continuation() {
        let (base_url, requests) = serve(vec![
            MockResponse::sse(host_tool_response(
                "delegate",
                &serde_json::json!({ "task": "复核高潮" }),
            )),
            MockResponse::sse(text_response("answer", "额度不足时由 Composer 继续完成")),
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
                    content: Some("分析高潮材料".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                }],
                ValidationRequest {
                    score: ScoreValidation::new(None, Vec::new(), Vec::new()),
                    run_policy: test_policy(2),
                    tool_context: None,
                    require_candidate: false,
                    forbid_clarification: false,
                },
                &mut SilentReporter,
            )
            .await
            .unwrap();

        assert_eq!(result.interpretation, "额度不足时由 Composer 继续完成");
        assert_eq!(result.stats.model_calls, 2);
        assert_eq!(result.stats.delegations, 0);
        assert_eq!(result.stats.tool_turns, 1);

        let _initial_request = requests.recv().unwrap();
        let continuation_request = String::from_utf8(requests.recv().unwrap()).unwrap();
        assert!(continuation_request.contains("只剩不足两次模型调用额度"));
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn invalid_delegate_arguments_do_not_count_as_a_model_call() {
        let (base_url, requests) = serve(vec![
            MockResponse::sse(host_tool_response(
                "delegate",
                &serde_json::json!({ "task": "" }),
            )),
            MockResponse::sse(text_response("answer", "参数失败后继续完成")),
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
                    content: Some("分析高潮材料".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                }],
                ValidationRequest {
                    score: ScoreValidation::new(None, Vec::new(), Vec::new()),
                    run_policy: test_policy(3),
                    tool_context: None,
                    require_candidate: false,
                    forbid_clarification: false,
                },
                &mut SilentReporter,
            )
            .await
            .unwrap();

        assert_eq!(result.interpretation, "参数失败后继续完成");
        assert_eq!(result.stats.model_calls, 2);
        assert_eq!(result.stats.delegations, 0);
        assert_eq!(result.stats.tool_turns, 1);

        let _initial_request = requests.recv().unwrap();
        let continuation_request = String::from_utf8(requests.recv().unwrap()).unwrap();
        assert!(continuation_request.contains("task 不能为空"));
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn play_score_tool_keeps_whole_score_default_and_exposes_optional_section_window() {
        let tools = model_tools(true);
        let tool = tools
            .iter()
            .find(|tool| tool.function.name == "play_score")
            .unwrap();

        assert_eq!(
            tool.function.parameters["required"],
            serde_json::json!(["target"])
        );
        assert_eq!(
            tool.function.parameters["properties"]["context_secs"]["minimum"],
            5
        );
        assert_eq!(
            tool.function.parameters["properties"]["context_secs"]["maximum"],
            15
        );
        assert!(tool.function.parameters["properties"]["section_id"].is_object());
    }

    fn test_form_plan() -> FormPlan {
        use crate::project::{FormSection, MaterialAction, SectionEnergy};

        FormPlan {
            target_duration_secs: 120.0,
            sections: vec![
                (
                    "intro",
                    0.0,
                    30.0,
                    MaterialAction::Introduce,
                    SectionEnergy::Low,
                ),
                (
                    "theme",
                    30.0,
                    60.0,
                    MaterialAction::Develop,
                    SectionEnergy::Medium,
                ),
                (
                    "climax",
                    60.0,
                    90.0,
                    MaterialAction::Contrast,
                    SectionEnergy::Peak,
                ),
                (
                    "coda",
                    90.0,
                    120.0,
                    MaterialAction::Close,
                    SectionEnergy::High,
                ),
            ]
            .into_iter()
            .map(|(id, start, end, material_action, energy)| FormSection {
                id: id.to_string(),
                target_start_secs: start,
                target_end_secs: end,
                function: id.to_string(),
                material_action,
                energy,
            })
            .collect(),
        }
    }

    fn score_with_markers(markers: &[(&str, f64)], duration_secs: f64) -> ScoreInfo {
        ScoreInfo {
            duration_ms: duration_secs * 1000.0,
            part_count: 1,
            event_count: 1,
            instruments: vec!["midi-piano".to_string()],
            tempo: 120.0,
            markers: markers
                .iter()
                .map(|(name, seconds)| crate::alda::ScoreMarker {
                    name: (*name).to_string(),
                    offset_ms: seconds * 1000.0,
                })
                .collect(),
            sections: Vec::new(),
            timeline: crate::alda::TimelineDiagnostics::default(),
        }
    }

    #[test]
    fn long_form_plan_requires_exact_marker_order_and_tolerant_boundaries() {
        let plan = test_form_plan();
        let valid = score_with_markers(
            &[
                ("section_intro", 0.0),
                ("section_theme", 31.0),
                ("section_climax", 59.0),
                ("section_coda", 90.0),
            ],
            119.0,
        );
        assert_eq!(
            form_plan_check(Some(&valid), Some(&plan), true)
                .unwrap()
                .status,
            CheckStatus::Pass
        );

        for invalid in [
            score_with_markers(
                &[
                    ("section_intro", 0.0),
                    ("section_theme", 30.0),
                    ("section_coda", 90.0),
                ],
                120.0,
            ),
            score_with_markers(
                &[
                    ("section_intro", 0.0),
                    ("section_climax", 60.0),
                    ("section_theme", 30.0),
                    ("section_coda", 90.0),
                ],
                120.0,
            ),
            score_with_markers(
                &[
                    ("section_intro", 0.0),
                    ("section_theme", 40.0),
                    ("section_climax", 60.0),
                    ("section_coda", 90.0),
                ],
                120.0,
            ),
        ] {
            assert_eq!(
                form_plan_check(Some(&invalid), Some(&plan), true)
                    .unwrap()
                    .status,
                CheckStatus::Fail
            );
        }

        assert_eq!(
            form_plan_check(Some(&valid), None, true).unwrap().status,
            CheckStatus::Fail
        );
    }

    #[test]
    fn candidate_reference_resolves_matching_checkpoint_and_rejects_invalid_ones() {
        let submitted = parse_submitted_result(
            &serde_json::json!({
                "kind": "candidate",
                "message": "引用预检候选",
                "candidate_ref": { "source_hash": "a".repeat(64) }
            })
            .to_string(),
        )
        .unwrap();
        assert!(resolve_candidate_reference(submitted, None).is_err());

        let checkpoint = CandidateCheckpoint {
            source_hash: "b".repeat(64),
            alda_code: "piano: c".to_string(),
            form_plan: None,
            edit_scope: None,
            checks: Vec::new(),
        };
        let submitted = parse_submitted_result(
            &serde_json::json!({
                "kind": "candidate",
                "message": "引用预检候选",
                "candidate_ref": { "source_hash": "a".repeat(64) }
            })
            .to_string(),
        )
        .unwrap();
        assert!(resolve_candidate_reference(submitted, Some(&checkpoint)).is_err());

        let plan = test_form_plan();
        let edit_scope = EditScope {
            mode: EditMode::Global,
            target_sections: Vec::new(),
            intent: "建立完整结构".to_string(),
        };
        let checkpoint = CandidateCheckpoint {
            source_hash: "c".repeat(64),
            alda_code: "piano: c d e f".to_string(),
            form_plan: Some(plan.clone()),
            edit_scope: Some(edit_scope.clone()),
            checks: Vec::new(),
        };
        let submitted = parse_submitted_result(
            &serde_json::json!({
                "kind": "candidate",
                "message": "提交预检候选",
                "candidate_ref": { "source_hash": "c".repeat(64) }
            })
            .to_string(),
        )
        .unwrap();
        let resolved = resolve_candidate_reference(submitted, Some(&checkpoint)).unwrap();
        assert_eq!(resolved.alda_code.as_deref(), Some("piano: c d e f"));
        assert_eq!(resolved.form_plan, Some(plan));
        assert_eq!(resolved.edit_scope, Some(edit_scope));
    }

    #[test]
    fn local_edit_scope_allows_target_changes_and_rejects_non_target_changes() {
        let (directory, runner) = section_runner();
        let baseline_path = directory.path().join("baseline.alda");
        let target_path = directory.path().join("target.alda");
        let non_target_path = directory.path().join("non-target.alda");
        fs::write(&baseline_path, "baseline").unwrap();
        fs::write(&target_path, "target_changed").unwrap();
        fs::write(&non_target_path, "non_target_changed").unwrap();
        let plan = test_form_plan();
        let context = AgentToolContext {
            project_root: directory.path().to_path_buf(),
            current_path: None,
            working_path: Some(baseline_path),
            revision_path: None,
            form_plan: Some(plan.clone()),
        };
        let local = EditScope {
            mode: EditMode::Local,
            target_sections: vec!["climax".to_string()],
            intent: "增强高潮".to_string(),
        };

        let target_info = runner.parse(&target_path).unwrap();
        assert_eq!(
            edit_scope_check(
                &runner,
                Some(&context),
                Some(&target_info),
                Some(&plan),
                Some(&local),
            )
            .unwrap()
            .status,
            CheckStatus::Pass
        );

        let non_target_info = runner.parse(&non_target_path).unwrap();
        let failure = edit_scope_check(
            &runner,
            Some(&context),
            Some(&non_target_info),
            Some(&plan),
            Some(&local),
        )
        .unwrap();
        assert_eq!(failure.status, CheckStatus::Fail);
        assert!(failure.detail.contains("theme"));

        let global = EditScope {
            mode: EditMode::Global,
            target_sections: Vec::new(),
            intent: "整体重写".to_string(),
        };
        assert_eq!(
            edit_scope_check(
                &runner,
                Some(&context),
                Some(&non_target_info),
                Some(&plan),
                Some(&global),
            )
            .unwrap()
            .status,
            CheckStatus::Pass
        );
    }

    #[tokio::test]
    async fn inspect_alda_patch_validates_in_memory_and_rejects_stale_baseline() {
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            "http://127.0.0.1:1".to_string(),
            "example-model".to_string(),
        )
        .unwrap();
        let (directory, runner) = section_runner();
        let baseline_path = directory.path().join("baseline.alda");
        let baseline_source = "baseline";
        fs::write(&baseline_path, baseline_source).unwrap();
        let mut plan = test_form_plan();
        plan.target_duration_secs = 3.6;
        for (section, (start, end)) in
            plan.sections
                .iter_mut()
                .zip([(0.0, 1.0), (1.0, 2.0), (2.0, 3.0), (3.0, 3.6)])
        {
            section.target_start_secs = start;
            section.target_end_secs = end;
        }
        let context = AgentToolContext {
            project_root: directory.path().to_path_buf(),
            current_path: None,
            working_path: Some(baseline_path.clone()),
            revision_path: None,
            form_plan: Some(plan.clone()),
        };
        let validation = ScoreValidation::new(None, Vec::new(), Vec::new());
        let agent = Agent::new(client, runner);
        let source_hash = format!("{:x}", Sha256::digest(baseline_source.as_bytes()));
        let arguments = serde_json::json!({
            "base": { "kind": "work", "source_hash": source_hash },
            "replacements": [{ "old": "baseline", "new": "target_changed" }],
            "form_plan": plan,
            "edit_scope": {
                "mode": "local",
                "target_sections": ["climax"],
                "intent": "增强高潮"
            }
        });

        let inspected = agent
            .execute_model_tool(
                "inspect_alda_patch",
                &arguments.to_string(),
                Some(&context),
                &validation,
            )
            .await
            .unwrap();
        let inspection: serde_json::Value = serde_json::from_str(&inspected.content).unwrap();
        assert_eq!(inspection["hard_failures"], serde_json::json!([]));
        assert!(inspected.candidate_checkpoint.is_some());
        assert_eq!(fs::read_to_string(&baseline_path).unwrap(), baseline_source);

        let mut stale = arguments.clone();
        stale["base"]["source_hash"] = serde_json::Value::String("0".repeat(64));
        let error = agent
            .execute_model_tool(
                "inspect_alda_patch",
                &stale.to_string(),
                Some(&context),
                &validation,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("source_hash 已失效"));

        let revision_path = directory.path().join("revision.alda");
        fs::write(&revision_path, "newer revision").unwrap();
        let newer_context = AgentToolContext {
            revision_path: Some(revision_path),
            ..context
        };
        let error = agent
            .execute_model_tool(
                "inspect_alda_patch",
                &arguments.to_string(),
                Some(&newer_context),
                &validation,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("最新工作基线"));
    }

    fn assert_valid_source_inspection(valid: &serde_json::Value) {
        assert_eq!(valid["parse_ok"], true);
        assert_eq!(valid["duration_secs"], 3.0);
        assert_eq!(
            valid["markers"],
            serde_json::json!([
                {"name": "intro", "offset_secs": 0.0},
                {"name": "theme", "offset_secs": 1.5}
            ])
        );
        assert_eq!(valid["sections"][0]["name"], "intro");
        assert_eq!(valid["sections"][0]["start_secs"], 0.0);
        assert_eq!(valid["sections"][0]["end_secs"], 1.5);
        assert_eq!(valid["sections"][0]["event_count"], 1);
        assert_eq!(valid["sections"][0]["parts"][0]["name"], "piano");
        assert_eq!(valid["sections"][0]["parts"][0]["sounding_secs"], 1.5);
        assert_eq!(valid["sections"][0]["parts"][0]["coverage_ratio"], 1.0);
        assert_eq!(valid["sections"][1]["name"], "theme");
        assert_eq!(valid["sections"][1]["event_count"], 0);
        assert_eq!(valid["sections"][1]["parts"][0]["sounding_secs"], 1.5);
        assert_eq!(valid["parts"][0]["name"], "piano");
        assert_eq!(valid["parts"][0]["end_secs"], 3.0);
        assert_eq!(valid["parts"][0]["event_count"], 1);
        assert_eq!(valid["hard_failures"], serde_json::json!([]));
        assert!(valid["diagnostics"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|item| item["name"] == "声部时间轴/事件空档")
        }));
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
        assert_valid_source_inspection(&valid);

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
        assert_eq!(invalid["sections"], serde_json::json!([]));
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
        assert_eq!(oversized["sections"], serde_json::json!([]));
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
    async fn inspect_score_returns_patch_ready_source_hash() {
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            "http://127.0.0.1:1".to_string(),
            "example-model".to_string(),
        )
        .unwrap();
        let (directory, runner) = progress_runner();
        let source = "midi-acoustic-grand-piano: target";
        let current = directory.path().join("current.alda");
        fs::write(&current, source).unwrap();
        let context = AgentToolContext {
            project_root: directory.path().to_path_buf(),
            current_path: Some(current),
            working_path: None,
            revision_path: None,
            form_plan: None,
        };
        let result = Agent::new(client, runner)
            .execute_model_tool(
                "inspect_score",
                &serde_json::json!({ "target": "current" }).to_string(),
                Some(&context),
                &ScoreValidation::new(None, Vec::new(), Vec::new()),
            )
            .await
            .unwrap();
        let result: serde_json::Value = serde_json::from_str(&result.content).unwrap();

        assert_eq!(
            result["source_hash"],
            format!("{:x}", Sha256::digest(source.as_bytes()))
        );
        assert_eq!(result["info"]["sections"][0]["name"], "intro");
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
        assert!(candidate.candidate_checkpoint.is_none());
        assert!(candidate_json["source_hash"].is_null());
        assert!(
            candidate_json["hard_failures"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["name"] == "时长"))
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
                delegations: 0,
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
                        revision_path: None,
                        form_plan: None,
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
                delegations: 0,
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
