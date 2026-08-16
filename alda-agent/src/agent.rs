use crate::alda::{AldaCheck, AldaRunner, CheckStatus, ScoreValidation};
use crate::conversation::{ConversationMessage, ConversationRole, ConversationToolCall};
use crate::deepseek::{DeepSeekClient, FunctionDef, Message, StreamEvent, Tool};
use crate::instructions::CompiledInstructions;
use anyhow::{Context, Result, bail};
use std::fmt::Write as _;
use std::fs;

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
                    }
                },
                "required": ["kind", "message"]
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
    /// 由宿主编译的不可变有效指示
    pub compiled_instructions: CompiledInstructions,
    /// 最大自动修正轮数，默认 3
    pub max_rounds: usize,
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
    pub compiled_instructions: CompiledInstructions,
    pub max_rounds: usize,
}

struct ValidationRequest {
    score: ScoreValidation,
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
        let max_rounds = validation.max_rounds.max(1);

        let mut interpretation = String::new();
        let mut last_alda_code: Option<String> = None;
        let mut last_checks: Vec<AldaCheck> = Vec::new();
        let mut last_was_truncated = false;
        let mut score_kind: Option<AgentResultKind> = None;

        for round in 0..max_rounds {
            reporter.report(AgentEvent::RoundStarted {
                round: round + 1,
                max_rounds,
            });
            let mut was_truncated = false;
            let tools = vec![submit_result_tool()];

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
                        if name == "submit_result" {
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

            let Some((tool_id, _, tool_args)) = tool_call_args else {
                bail!("模型未通过 submit_result 明确返回结果类型");
            };

            let submitted = parse_submitted_result(&tool_args)?;
            if round_text.trim().is_empty() {
                reporter.report(AgentEvent::ModelText(submitted.message.clone()));
            }
            if matches!(
                submitted.kind,
                AgentResultKind::Answer | AgentResultKind::Clarification | AgentResultKind::Plan
            ) {
                messages.push(Message {
                    role: "assistant".to_string(),
                    content: Some(submitted.message.clone()),
                    tool_calls: None,
                    tool_call_id: None,
                });
                return Ok(CreationResult {
                    rounds: round + 1,
                    success: false,
                    needs_input: submitted.kind == AgentResultKind::Clarification,
                    kind: submitted.kind,
                    checks: Vec::new(),
                    alda_code: None,
                    interpretation: submitted.message,
                    was_truncated,
                    conversation: messages,
                });
            }

            if let Some(previous) = score_kind {
                if previous != submitted.kind {
                    bail!(
                        "自动修正不能把结果类型从 {previous:?} 改为 {:?}",
                        submitted.kind
                    );
                }
            }
            score_kind = Some(submitted.kind);

            let alda_code = submitted
                .alda_code
                .context("草稿或完整候选缺少 alda_code")?;

            // 写入临时文件并校验
            let tmp_dir = tempfile::tempdir().context("创建临时目录失败")?;
            let tmp_score = tmp_dir.path().join("candidate.alda");
            fs::write(&tmp_score, &alda_code)?;

            reporter.report(AgentEvent::ValidationStarted {
                round: round + 1,
                max_rounds,
            });

            let score_validation = if submitted.kind == AgentResultKind::Candidate {
                validation.score.clone()
            } else {
                validation.score.clone().without_duration()
            };
            let mut checks = self
                .runner
                .validate_async(tmp_score, score_validation)
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
                        name: "submit_result".to_string(),
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
                    kind: submitted.kind,
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
            kind: score_kind.unwrap_or(AgentResultKind::Candidate),
            checks: last_checks,
            alda_code: last_alda_code,
            interpretation,
            was_truncated: last_was_truncated,
            conversation: messages,
        })
    }
}

// ============================================================
// 辅助函数
// ============================================================

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
        let _ = writeln!(msg, "【目标时长】约 {} 分钟", dur / 60.0);
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
        let _ = writeln!(message, "【目标时长】约 {} 分钟", duration / 60.0);
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
    if let Some(score) = &request.working_alda {
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

struct SubmittedResult {
    kind: AgentResultKind,
    message: String,
    alda_code: Option<String>,
}

fn parse_submitted_result(args: &str) -> Result<SubmittedResult> {
    let parsed: serde_json::Value =
        serde_json::from_str(args).context("无法解析 submit_result 参数")?;
    let kind = match parsed["kind"].as_str() {
        Some("answer") => AgentResultKind::Answer,
        Some("clarification") => AgentResultKind::Clarification,
        Some("plan") => AgentResultKind::Plan,
        Some("draft") => AgentResultKind::Draft,
        Some("candidate") => AgentResultKind::Candidate,
        _ => bail!("submit_result.kind 无效"),
    };
    let message = parsed["message"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("message 字段缺失或不是字符串"))?
        .trim()
        .to_string();
    if message.is_empty() {
        bail!("submit_result.message 不能为空");
    }
    let Some(code) = parsed["alda_code"].as_str() else {
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

fn build_tool_feedback(checks: &[AldaCheck], _tool_call_id: Option<&str>) -> String {
    let failures: Vec<_> = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Fail)
        .collect();

    if failures.is_empty() {
        return "✅ 所有检查通过，工作乐谱可以试听；只有用户明确接受完整候选才会创建版本。"
            .to_string();
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

    // 过短通常说明内容或结构不足，不能仅靠降低 tempo 拉伸成完整作品。
    let duration_values = checks
        .iter()
        .find(|c| c.name == "时长" && c.status == CheckStatus::Fail)
        .and_then(|check| parse_duration_values(&check.detail));
    if let Some((actual, target)) = duration_values.filter(|(actual, _)| *actual > 0.0) {
        let _ = writeln!(
            msg,
            "\n**时长修正指南**: 当前作品约 {actual:.0} 秒，目标 {target:.0} 秒。作品过短时补充或发展材料、段落和整体结构；作品过长时优先删减冗余。仅在内容量已经合适且速度明显偏离意图时调整 tempo。"
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
    use crate::instructions::{CreationMode, ProjectPreferences};
    use crate::test_support::{MockResponse, serve};
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

    fn compiled_instructions(preferences: &ProjectPreferences) -> CompiledInstructions {
        let catalog = crate::skills::SkillCatalog::discover(None, None).unwrap();
        CompiledInstructions::compile(
            &catalog,
            &crate::instructions::InstructionProfile::default(),
            preferences,
        )
        .unwrap()
    }

    fn request(max_rounds: usize) -> CreationRequest {
        CreationRequest {
            source_material: "素材".to_string(),
            instructions: "创作完整器乐作品".to_string(),
            compiled_instructions: compiled_instructions(&ProjectPreferences::default()),
            max_rounds,
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
    fn duration_failure_recommends_developing_material() {
        let feedback = build_tool_feedback(
            &[AldaCheck {
                name: "时长",
                status: CheckStatus::Fail,
                detail: "约 227秒（目标 180秒，偏差 26%，超出容差 10%）".to_string(),
            }],
            None,
        );

        assert!(feedback.contains("补充或发展材料"));
        assert!(feedback.contains("删减冗余"));
        assert!(!feedback.contains("乘以"));
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
}
