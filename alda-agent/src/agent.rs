use crate::alda::{AldaCheck, AldaRunner, CheckStatus};
use crate::conversation::{ConversationMessage, ConversationRole, ConversationToolCall};
use crate::deepseek::{DeepSeekClient, FunctionDef, Message, StreamEvent, Tool};
use anyhow::{Context, Result, bail};
use std::fmt::Write as _;
use std::fs;

// ============================================================
// 系统提示
// ============================================================

const PROTOCOL_PROMPT: &str = include_str!("../prompts/protocol.md");
const DEFAULT_CREATIVE_STRATEGY: &str = include_str!("../prompts/default-creative-strategy.md");

// ============================================================
// 工具定义
// ============================================================

fn submit_alda_tool() -> Tool {
    Tool {
        ty: "function".to_string(),
        function: FunctionDef {
            name: "submit_alda".to_string(),
            description: "提交一段 Alda 乐谱代码以供校验。校验通过后才能成为有效作品。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "alda_code": {
                        "type": "string",
                        "description": "完整且紧凑的 Alda 乐谱代码；复用材料时使用变量，禁止逐字展开重复段落",
                        "maxLength": crate::deepseek::MAX_TOOL_ARGUMENT_BYTES
                    }
                },
                "required": ["alda_code"]
            }),
        },
    }
}

// ============================================================
// 创作请求
// ============================================================

pub struct CreationRequest {
    /// 素材文本
    pub source_material: String,
    /// 本次创作的自然语言要求
    pub instructions: String,
    /// 项目级创作策略；为空时仅使用内置默认
    pub creative_strategy: String,
    /// 创作模式
    pub mode: CreationMode,
    /// 目标时长（秒），None 不检查
    pub target_duration_secs: Option<f64>,
    /// 必须包含的乐器（子串匹配）
    pub included_instruments: Vec<String>,
    /// 必须排除的乐器（子串匹配）
    pub excluded_instruments: Vec<String>,
    /// 最大自动修正轮数，默认 3
    pub max_rounds: usize,
}

#[derive(Debug, Clone, Copy)]
pub enum CreationMode {
    FullPiece,
    Improvisation,
}

impl CreationMode {
    fn description(&self) -> &str {
        match self {
            CreationMode::FullPiece => {
                "完整曲目：强调结构完整、材料发展和明确收束；模式本身不预设时长"
            }
            CreationMode::Improvisation => {
                "即兴片段：强调自由发展，允许开放式收束；模式本身不预设时长"
            }
        }
    }
}

// ============================================================
// 创作结果
// ============================================================

#[derive(Debug)]
pub struct CreationResult {
    /// 实际使用的修正轮数
    pub rounds: usize,
    /// 是否通过必要检查
    pub success: bool,
    /// 模型提出澄清问题，尚未生成候选
    pub needs_input: bool,
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
}

pub struct ModifyRequest {
    pub source_material: String,
    pub current_alda: String,
    pub feedback: String,
    pub creative_strategy: String,
    pub mode: CreationMode,
    pub target_duration_secs: Option<f64>,
    pub included_instruments: Vec<String>,
    pub excluded_instruments: Vec<String>,
    pub max_rounds: usize,
}

pub struct ContinueRequest {
    pub conversation: Vec<Message>,
    pub target_duration_secs: Option<f64>,
    pub included_instruments: Vec<String>,
    pub excluded_instruments: Vec<String>,
    pub max_rounds: usize,
}

pub struct ProjectPromptRequest {
    pub conversation: Vec<ConversationMessage>,
    pub current_alda: Option<String>,
    pub creative_strategy: String,
    pub mode: CreationMode,
    pub target_duration_secs: Option<f64>,
    pub included_instruments: Vec<String>,
    pub excluded_instruments: Vec<String>,
    pub max_rounds: usize,
}

struct ValidationRequest {
    target_duration_ms: Option<f64>,
    included_instruments: Vec<String>,
    excluded_instruments: Vec<String>,
    max_rounds: usize,
}

// ============================================================
// Agent
// ============================================================

pub struct Agent {
    client: DeepSeekClient,
    runner: AldaRunner,
}

pub trait AgentReporter {
    fn report(&mut self, event: AgentEvent);
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    PrivacyNotice,
    RoundStarted {
        round: usize,
        max_rounds: usize,
    },
    ModelText(String),
    ValidationStarted {
        round: usize,
        max_rounds: usize,
    },
    ValidationCompleted(Vec<AldaCheck>),
    RevisionStarted {
        next_round: usize,
        max_rounds: usize,
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
        Agent { client, runner }
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
        let mut messages = vec![Message {
            role: "system".to_string(),
            content: Some(PROTOCOL_PROMPT.to_string()),
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
                target_duration_ms: request.target_duration_secs.map(|seconds| seconds * 1000.0),
                included_instruments: request.included_instruments,
                excluded_instruments: request.excluded_instruments,
                max_rounds: request.max_rounds,
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
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: Some(PROTOCOL_PROMPT.to_string()),
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
                target_duration_ms: request.target_duration_secs.map(|seconds| seconds * 1000.0),
                included_instruments: request.included_instruments,
                excluded_instruments: request.excluded_instruments,
                max_rounds: request.max_rounds,
            },
            reporter,
        )
        .await
    }

    pub async fn modify(&self, request: ModifyRequest) -> Result<CreationResult> {
        self.modify_with_reporter(request, &mut SilentReporter)
            .await
    }

    pub async fn modify_with_reporter(
        &self,
        request: ModifyRequest,
        reporter: &mut impl AgentReporter,
    ) -> Result<CreationResult> {
        if request.current_alda.trim().is_empty() {
            bail!("当前乐谱为空，无法修改");
        }
        if request.feedback.trim().is_empty() {
            bail!("修改要求不能为空");
        }

        let messages = vec![
            Message {
                role: "system".to_string(),
                content: Some(PROTOCOL_PROMPT.to_string()),
                tool_calls: None,
                tool_call_id: None,
            },
            Message {
                role: "user".to_string(),
                content: Some(build_modify_message(&request)),
                tool_calls: None,
                tool_call_id: None,
            },
        ];

        self.run_generation(
            messages,
            ValidationRequest {
                target_duration_ms: request.target_duration_secs.map(|seconds| seconds * 1000.0),
                included_instruments: request.included_instruments,
                excluded_instruments: request.excluded_instruments,
                max_rounds: request.max_rounds,
            },
            reporter,
        )
        .await
    }

    pub async fn continue_generation(&self, request: ContinueRequest) -> Result<CreationResult> {
        self.continue_with_reporter(request, &mut SilentReporter)
            .await
    }

    pub async fn continue_with_reporter(
        &self,
        request: ContinueRequest,
        reporter: &mut impl AgentReporter,
    ) -> Result<CreationResult> {
        if request.conversation.is_empty() {
            bail!("没有可继续的生成上下文");
        }
        self.run_generation(
            request.conversation,
            ValidationRequest {
                target_duration_ms: request.target_duration_secs.map(|seconds| seconds * 1000.0),
                included_instruments: request.included_instruments,
                excluded_instruments: request.excluded_instruments,
                max_rounds: request.max_rounds,
            },
            reporter,
        )
        .await
    }

    // Keeping the retry transcript in one loop makes protocol ordering auditable.
    #[allow(clippy::too_many_lines)]
    async fn run_generation(
        &self,
        mut messages: Vec<Message>,
        validation: ValidationRequest,
        reporter: &mut impl AgentReporter,
    ) -> Result<CreationResult> {
        validate_generation_constraints(&validation)?;
        let max_rounds = validation.max_rounds.max(1);

        let mut interpretation = String::new();
        let mut last_alda_code: Option<String> = None;
        let mut last_checks: Vec<AldaCheck> = Vec::new();
        let mut last_was_truncated = false;

        for round in 0..max_rounds {
            reporter.report(AgentEvent::RoundStarted {
                round: round + 1,
                max_rounds,
            });
            let mut was_truncated = false;
            let tools = vec![submit_alda_tool()];

            let events = self
                .client
                .chat_stream_with(messages.clone(), Some(tools), |text| {
                    reporter.report(AgentEvent::ModelText(text.to_string()));
                })
                .await?;

            // 收集文本和工具调用
            let mut tool_call_args: Option<(Option<String>, String, String)> = None; // (id, name, args)
            let mut round_text = String::new();

            for event in &events {
                match event {
                    StreamEvent::Text(text) => {
                        interpretation.push_str(text);
                        round_text.push_str(text);
                    }
                    StreamEvent::ToolCall {
                        id,
                        name,
                        arguments,
                    } => {
                        if name == "submit_alda" {
                            tool_call_args = Some((id.clone(), name.clone(), arguments.clone()));
                        }
                    }
                    StreamEvent::Done { finish_reason } => {
                        if finish_reason == "length" {
                            was_truncated = true;
                        }
                    }
                }
            }

            // 检查是否有工具调用
            let Some((tool_id, _, tool_args)) = tool_call_args else {
                if round_text.trim().is_empty() {
                    bail!("模型既未提交 Alda 乐谱，也未返回可显示的澄清问题");
                }
                messages.push(Message {
                    role: "assistant".to_string(),
                    content: Some(round_text),
                    tool_calls: None,
                    tool_call_id: None,
                });
                return Ok(CreationResult {
                    rounds: round + 1,
                    success: false,
                    needs_input: true,
                    checks: Vec::new(),
                    alda_code: None,
                    interpretation,
                    was_truncated,
                    conversation: messages,
                });
            };

            // 解析 alda_code
            let alda_code = parse_alda_code_from_args(&tool_args)?;

            // 写入临时文件并校验
            let tmp_dir = tempfile::tempdir().context("创建临时目录失败")?;
            let tmp_score = tmp_dir.path().join("candidate.alda");
            fs::write(&tmp_score, &alda_code)?;

            reporter.report(AgentEvent::ValidationStarted {
                round: round + 1,
                max_rounds,
            });

            let mut checks = self
                .runner
                .validate_async(
                    tmp_score,
                    validation.included_instruments.clone(),
                    validation.excluded_instruments.clone(),
                    validation.target_duration_ms,
                    10.0,
                )
                .await?;

            // 如果截断，追加诊断
            if was_truncated {
                checks.push(AldaCheck {
                    name: "输出完整性",
                    status: CheckStatus::Fail,
                    detail: "模型输出被截断（达到 token 限制），作品可能不完整".to_string(),
                });
            }
            reporter.report(AgentEvent::ValidationCompleted(checks.clone()));

            // 检查是否全部通过
            let all_pass = checks.iter().all(|c| c.status != CheckStatus::Fail);

            last_alda_code = Some(alda_code.clone());
            last_checks.clone_from(&checks);
            last_was_truncated = was_truncated;

            let feedback = build_tool_feedback(&checks, tool_id.as_deref());
            let tool_call_id = tool_id.unwrap_or_else(|| "call_1".to_string());
            messages.push(Message {
                role: "assistant".to_string(),
                content: (!round_text.is_empty()).then_some(round_text),
                tool_calls: Some(vec![crate::deepseek::ToolCallMsg {
                    id: tool_call_id.clone(),
                    ty: "function".to_string(),
                    function: crate::deepseek::FunctionCallArgs {
                        name: "submit_alda".to_string(),
                        arguments: tool_args,
                    },
                }]),
                tool_call_id: None,
            });
            messages.push(Message {
                role: "tool".to_string(),
                content: Some(feedback),
                tool_calls: None,
                tool_call_id: Some(tool_call_id),
            });

            if all_pass {
                return Ok(CreationResult {
                    rounds: round + 1,
                    success: true,
                    needs_input: false,
                    checks,
                    alda_code: Some(alda_code),
                    interpretation,
                    was_truncated,
                    conversation: messages,
                });
            }
            reporter.report(AgentEvent::RevisionStarted {
                next_round: round + 2,
                max_rounds,
                failures: checks
                    .iter()
                    .filter(|check| check.status == CheckStatus::Fail)
                    .count(),
            });
        }

        // 达到上限
        Ok(CreationResult {
            rounds: max_rounds,
            success: false,
            needs_input: false,
            checks: last_checks,
            alda_code: last_alda_code,
            interpretation,
            was_truncated: last_was_truncated,
            conversation: messages,
        })
    }
}

fn validate_generation_constraints(validation: &ValidationRequest) -> Result<()> {
    if validation
        .target_duration_ms
        .is_some_and(|duration| !duration.is_finite() || duration <= 0.0)
    {
        bail!("目标时长必须是大于 0 的有限数值");
    }
    if validation
        .included_instruments
        .iter()
        .chain(&validation.excluded_instruments)
        .any(|instrument| instrument.trim().is_empty())
    {
        bail!("乐器约束不能为空");
    }
    if let Some(conflict) = validation.included_instruments.iter().find(|included| {
        validation
            .excluded_instruments
            .iter()
            .any(|excluded| included.eq_ignore_ascii_case(excluded))
    }) {
        bail!("乐器 {conflict} 不能同时要求包含和排除");
    }
    Ok(())
}

// ============================================================
// 辅助函数
// ============================================================

fn build_user_message(request: &CreationRequest) -> String {
    let mut msg = String::new();

    append_strategy_context(&mut msg, &request.creative_strategy);
    msg.push_str("【创作上下文】\n");

    if !request.source_material.is_empty() {
        msg.push_str("【素材】\n");
        msg.push_str(&request.source_material);
        msg.push_str("\n\n");
    }

    msg.push_str("【模式】");
    msg.push_str(request.mode.description());
    msg.push('\n');

    if let Some(dur) = request.target_duration_secs {
        let _ = writeln!(msg, "【目标时长】约 {} 分钟", dur / 60.0);
    }

    if !request.included_instruments.is_empty() {
        let _ = writeln!(
            msg,
            "【必须包含的乐器】{}",
            request.included_instruments.join("、")
        );
    }

    if !request.excluded_instruments.is_empty() {
        let _ = writeln!(
            msg,
            "【必须排除的乐器】{}",
            request.excluded_instruments.join("、")
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
    append_strategy_context(&mut message, &request.creative_strategy);
    message.push_str("【项目设置】\n【模式】");
    message.push_str(request.mode.description());
    message.push('\n');
    if let Some(duration) = request.target_duration_secs {
        let _ = writeln!(message, "【目标时长】约 {} 分钟", duration / 60.0);
    }
    if !request.included_instruments.is_empty() {
        let _ = writeln!(
            message,
            "【必须包含的乐器】{}",
            request.included_instruments.join("、")
        );
    }
    if !request.excluded_instruments.is_empty() {
        let _ = writeln!(
            message,
            "【必须排除的乐器】{}",
            request.excluded_instruments.join("、")
        );
    }
    if let Some(score) = &request.current_alda {
        message.push_str("\n【当前有效 Alda】\n");
        message.push_str(score);
    } else {
        message.push_str("\n项目尚无有效版本；请根据对话中的用户请求创作。\n");
    }
    message
}

fn build_modify_message(request: &ModifyRequest) -> String {
    let mut msg = String::new();
    append_strategy_context(&mut msg, &request.creative_strategy);
    msg.push_str("【修改上下文】\n这是一次独立的作品修改请求。不要假定或延续此前对话中的修改方案，只以本消息提供的素材、当前乐谱、最新反馈和约束为准。\n\n");

    if !request.source_material.trim().is_empty() {
        msg.push_str("【原始素材】\n");
        msg.push_str(request.source_material.trim());
        msg.push_str("\n\n");
    }
    msg.push_str("【模式】");
    msg.push_str(request.mode.description());
    msg.push('\n');

    if let Some(duration) = request.target_duration_secs {
        let _ = writeln!(msg, "【目标时长】约 {} 分钟", duration / 60.0);
    }
    if !request.included_instruments.is_empty() {
        let _ = writeln!(
            msg,
            "【必须包含的乐器】{}",
            request.included_instruments.join("、")
        );
    }
    if !request.excluded_instruments.is_empty() {
        let _ = writeln!(
            msg,
            "【必须排除的乐器】{}",
            request.excluded_instruments.join("、")
        );
    }

    msg.push_str("\n【当前 Alda】\n");
    msg.push_str(&request.current_alda);
    msg.push_str("\n\n【本轮要求｜来源：当前用户反馈｜最高策略优先级】\n");
    msg.push_str(request.feedback.trim());
    msg
}

fn append_strategy_context(msg: &mut String, project_strategy: &str) {
    msg.push_str("【默认创作策略｜来源：内置默认】\n");
    msg.push_str(DEFAULT_CREATIVE_STRATEGY.trim());
    msg.push_str("\n\n");
    if !project_strategy.trim().is_empty() {
        msg.push_str("【项目创作策略｜来源：用户项目配置｜覆盖冲突的默认策略】\n");
        msg.push_str(project_strategy.trim());
        msg.push_str("\n\n");
    }
}

fn parse_alda_code_from_args(args: &str) -> Result<String> {
    let parsed: serde_json::Value =
        serde_json::from_str(args).context("无法解析 submit_alda 参数")?;

    let code = parsed["alda_code"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("alda_code 字段缺失或不是字符串"))?;

    // 去除可能的 Markdown 代码块标记
    let code = code.trim();
    let code = code
        .strip_prefix("```alda")
        .or_else(|| code.strip_prefix("```"))
        .map_or(code, str::trim);
    let code = code.strip_suffix("```").map_or(code, str::trim);

    Ok(code.to_string())
}

fn build_tool_feedback(checks: &[AldaCheck], _tool_call_id: Option<&str>) -> String {
    let failures: Vec<_> = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Fail)
        .collect();

    if failures.is_empty() {
        return "✅ 所有检查通过，作品已保存。".to_string();
    }

    let mut msg = format!(
        "校验反馈 ({}/{} 项未通过):\n\n",
        failures.len(),
        checks.len(),
    );

    for c in checks {
        let icon = match c.status {
            CheckStatus::Pass => "✅",
            CheckStatus::Fail => "❌",
            CheckStatus::Unchecked => "⏭",
        };
        let _ = writeln!(msg, "{} {}: {}", icon, c.name, c.detail);
    }

    // 时长与 tempo 成反比。优先做确定性的 tempo 比例校准，避免模型反复重写已通过的结构。
    let duration_values = checks
        .iter()
        .find(|c| c.name == "时长" && c.status == CheckStatus::Fail)
        .and_then(|check| parse_duration_values(&check.detail));
    if let Some((actual, target)) = duration_values.filter(|(actual, _)| *actual > 0.0) {
        let tempo_multiplier = actual / target;
        let _ = writeln!(
            msg,
            "\n**时长修正指南**: 当前作品约 {actual:.0} 秒，目标 {target:.0} 秒。保持音符、段落和配器不变，只把每个显式 tempo 乘以 **{tempo_multiplier:.3}**。"
        );
        let _ = writeln!(
            msg,
            "公式：`新 tempo = 旧 tempo × {actual:.0} ÷ {target:.0}`。例如 `(tempo! 120)` 改为 `(tempo! {:.0})`。",
            120.0 * tempo_multiplier
        );
        msg.push_str(
            "作品过长时提高 tempo，过短时降低 tempo；不要重新规划、增删或展开乐谱内容。\n",
        );
    }

    msg.push_str("\n请根据以上反馈修改 Alda 乐谱后重新提交. 注意: 反馈中的具体数值和倍数建议是精确计算得出的, 请严格参考.");
    msg
}

/// 从时长检查的 detail 文本中解析实际值和目标值
/// detail 格式: "约 46秒（目标 180秒，偏差 74%，超出容差 10%）"
fn parse_duration_values(detail: &str) -> Option<(f64, f64)> {
    let after_yue = detail.strip_prefix("约 ")?.split('秒').next()?;
    let actual: f64 = after_yue.trim().parse().ok()?;

    let target_start = detail.find("目标 ")?.checked_add("目标 ".len())?;
    let target_end = detail[target_start..].find('秒')?;
    let target: f64 = detail[target_start..target_start + target_end]
        .trim()
        .parse()
        .ok()?;

    Some((actual, target))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MockResponse, serve};
    use std::os::unix::fs::PermissionsExt;

    fn tool_response(code: &str, finish_reason: &str) -> String {
        let arguments = serde_json::json!({ "alda_code": code }).to_string();
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {
                    "content": "解读与配器说明",
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": { "name": "submit_alda", "arguments": arguments }
                    }]
                },
                "finish_reason": finish_reason
            }]
        });
        format!("data: {chunk}\n\ndata: [DONE]\n")
    }

    fn text_response(text: &str) -> String {
        let chunk = serde_json::json!({
            "choices": [{
                "delta": { "content": text },
                "finish_reason": "stop"
            }]
        });
        format!("data: {chunk}\n\ndata: [DONE]\n")
    }

    fn fake_runner() -> (tempfile::TempDir, AldaRunner) {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("alda");
        let json = r#"{"events":[{"offset":0,"duration":500,"audible-duration":450,"midi-note":60,"part":"piano"}],"parts":{"piano":{"name":"piano","stock-instrument":"midi-acoustic-grand-piano","tempo":120}}}"#;
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = parse ]; then if [ -s \"$3\" ]; then printf '%s\\n' '{json}'; else printf '%s\\n' '{{\"events\":[],\"parts\":{{}}}}'; fi; else exit 1; fi\n"
        );
        fs::write(&executable, script).unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        (directory, AldaRunner::new(executable))
    }

    fn request(max_rounds: usize) -> CreationRequest {
        CreationRequest {
            source_material: "素材".to_string(),
            instructions: "创作完整器乐作品".to_string(),
            creative_strategy: "保持明亮欢快".to_string(),
            mode: CreationMode::FullPiece,
            target_duration_secs: None,
            included_instruments: Vec::new(),
            excluded_instruments: Vec::new(),
            max_rounds,
        }
    }

    fn modify_request() -> ModifyRequest {
        ModifyRequest {
            source_material: "原始诗歌".to_string(),
            current_alda: "midi-piano: c1".to_string(),
            feedback: "编排单调，艺术性不高".to_string(),
            creative_strategy: "保持明亮欢快".to_string(),
            mode: CreationMode::FullPiece,
            target_duration_secs: Some(180.0),
            included_instruments: vec!["midi-cello".to_string()],
            excluded_instruments: vec!["midi-timpani".to_string()],
            max_rounds: 1,
        }
    }

    #[test]
    fn modification_prompt_routes_scope_from_clean_project_context() {
        let message = build_modify_message(&modify_request());
        assert!(message.contains("不要假定或延续此前对话"));
        assert!(message.contains("【原始素材】\n原始诗歌"));
        assert!(message.contains("【默认创作策略｜来源：内置默认】"));
        assert!(message.contains("【项目创作策略｜来源：用户项目配置"));
        assert!(message.contains("保持明亮欢快"));
        assert!(message.contains("【本轮要求｜来源：当前用户反馈｜最高策略优先级】"));
        assert!(message.contains("可以重写曲式、主题发展、织体与配器"));
        assert!(message.contains("只提出一个简短澄清问题"));
        assert!(message.contains("【目标时长】约 3 分钟"));
        assert!(message.contains("【必须包含的乐器】midi-cello"));
        assert!(message.contains("【必须排除的乐器】midi-timpani"));
        assert!(message.ends_with("编排单调，艺术性不高"));
    }

    #[test]
    fn protocol_and_creative_strategies_have_separate_precedence_layers() {
        assert!(PROTOCOL_PROMPT.contains("submit_alda"));
        assert!(!PROTOCOL_PROMPT.contains("默认创作策略"));
        assert!(!PROTOCOL_PROMPT.contains("高潮由材料演变"));

        let message = build_user_message(&request(1));
        let default_position = message.find("【默认创作策略").unwrap();
        let project_position = message.find("【项目创作策略").unwrap();
        let current_position = message.find("【本轮要求").unwrap();
        assert!(default_position < project_position);
        assert!(project_position < current_position);
        assert!(message.ends_with("创作完整器乐作品"));
    }

    #[test]
    fn creation_mode_describes_form_without_implying_duration() {
        let full = CreationMode::FullPiece.description();
        let improv = CreationMode::Improvisation.description();

        assert!(full.contains("结构完整"));
        assert!(improv.contains("自由发展"));
        assert!(full.contains("不预设时长"));
        assert!(improv.contains("不预设时长"));
        assert!(!full.contains("分钟"));
        assert!(!improv.contains("分钟"));
    }

    #[tokio::test]
    async fn modification_starts_with_only_system_and_clean_request() {
        let (base_url, requests) = serve(vec![MockResponse::sse(text_response(
            "你希望整体重构还是只调整配器？",
        ))]);
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            base_url,
            "example-model".to_string(),
        )
        .unwrap();
        let (_directory, runner) = fake_runner();
        let result = Agent::new(client, runner)
            .modify(modify_request())
            .await
            .unwrap();
        assert!(result.needs_input);

        let request = requests.recv().unwrap();
        let request = String::from_utf8_lossy(&request);
        let body = request.split("\r\n\r\n").nth(1).unwrap();
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        assert!(
            messages[1]["content"]
                .as_str()
                .unwrap()
                .contains("原始诗歌")
        );
    }

    #[test]
    fn test_parse_duration() {
        let (actual, target) =
            parse_duration_values("约 46秒（目标 180秒，偏差 74%，超出容差 10%）").unwrap();
        assert!((actual - 46.0).abs() < f64::EPSILON);
        assert!((target - 180.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_parse_duration_no_match() {
        assert!(parse_duration_values("未检查").is_none());
        assert!(parse_duration_values("解析成功").is_none());
    }

    #[test]
    fn duration_failure_recommends_exact_tempo_scaling() {
        let feedback = build_tool_feedback(
            &[AldaCheck {
                name: "时长",
                status: CheckStatus::Fail,
                detail: "约 227秒（目标 180秒，偏差 26%，超出容差 10%）".to_string(),
            }],
            None,
        );

        assert!(feedback.contains("乘以 **1.261**"));
        assert!(feedback.contains("(tempo! 151)"));
        assert!(feedback.contains("不要重新规划、增删或展开乐谱内容"));
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
        let (_directory, runner) = fake_runner();
        let result = Agent::new(client, runner).create(request(2)).await.unwrap();
        assert!(result.success);
        assert_eq!(result.rounds, 2);
        assert!(!result.was_truncated);
        assert!(result.alda_code.is_some());
        assert!(result.conversation.len() >= 6);
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
    async fn text_without_tool_call_is_returned_as_clarification() {
        let (base_url, _requests) = serve(vec![MockResponse::sse(text_response(
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
    async fn invalid_constraints_fail_before_network_access() {
        let client = DeepSeekClient::new(
            "test-key".to_string(),
            "http://127.0.0.1:1".to_string(),
            "example-model".to_string(),
        )
        .unwrap();
        let (_directory, runner) = fake_runner();
        let mut invalid = request(1);
        invalid.target_duration_secs = Some(0.0);
        let error = Agent::new(client, runner)
            .create(invalid)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("目标时长"));
    }
}
