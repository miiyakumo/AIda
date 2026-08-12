use crate::alda::{AldaCheck, AldaRunner, CheckStatus};
use crate::deepseek::{DeepSeekClient, FunctionDef, Message, StreamEvent, Tool};
use anyhow::{Context, Result, bail};
use std::fmt::Write as _;
use std::fs;

// ============================================================
// 系统提示
// ============================================================

const SYSTEM_PROMPT: &str = include_str!("../prompts/system.md");

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
                        "description": "完整且紧凑的 Alda 乐谱代码；必须使用变量和重复，禁止展开重复段落",
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

pub enum CreationMode {
    FullPiece,
    Improvisation,
}

impl CreationMode {
    fn description(&self) -> &str {
        match self {
            CreationMode::FullPiece => "完整曲目，约 2-5 分钟，有明确的起承转合",
            CreationMode::Improvisation => "即兴片段，约 30 秒 - 2 分钟，自由发展",
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
    /// 可用于后续修改的完整消息上下文
    pub conversation: Vec<Message>,
}

pub struct ModifyRequest {
    pub current_alda: String,
    pub feedback: String,
    pub conversation: Vec<Message>,
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

impl Agent {
    #[must_use]
    pub fn new(client: DeepSeekClient, runner: AldaRunner) -> Self {
        Agent { client, runner }
    }

    pub async fn create(&self, request: CreationRequest) -> Result<CreationResult> {
        if request.source_material.trim().is_empty() && request.instructions.trim().is_empty() {
            bail!("创作素材与要求不能同时为空");
        }
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: Some(SYSTEM_PROMPT.to_string()),
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
        )
        .await
    }

    pub async fn modify(&self, request: ModifyRequest) -> Result<CreationResult> {
        if request.current_alda.trim().is_empty() {
            bail!("当前乐谱为空，无法修改");
        }
        if request.feedback.trim().is_empty() {
            bail!("修改要求不能为空");
        }

        let mut messages = request.conversation;
        if messages.is_empty() {
            messages.push(Message {
                role: "system".to_string(),
                content: Some(SYSTEM_PROMPT.to_string()),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        messages.push(Message {
            role: "user".to_string(),
            content: Some(format!(
                "请根据反馈修改当前作品。只改变反馈涉及的范围，并尽量保持其余内容不变。\n\n【反馈】\n{}\n\n【当前 Alda】\n{}",
                request.feedback, request.current_alda
            )),
            tool_calls: None,
            tool_call_id: None,
        });

        self.run_generation(
            messages,
            ValidationRequest {
                target_duration_ms: request.target_duration_secs.map(|seconds| seconds * 1000.0),
                included_instruments: request.included_instruments,
                excluded_instruments: request.excluded_instruments,
                max_rounds: request.max_rounds,
            },
        )
        .await
    }

    pub async fn continue_generation(&self, request: ContinueRequest) -> Result<CreationResult> {
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
        )
        .await
    }

    // Keeping the retry transcript in one loop makes protocol ordering auditable.
    #[allow(clippy::too_many_lines)]
    async fn run_generation(
        &self,
        mut messages: Vec<Message>,
        validation: ValidationRequest,
    ) -> Result<CreationResult> {
        validate_generation_constraints(&validation)?;
        let max_rounds = validation.max_rounds.max(1);

        let mut interpretation = String::new();
        let mut last_alda_code: Option<String> = None;
        let mut last_checks: Vec<AldaCheck> = Vec::new();
        let mut last_was_truncated = false;

        for round in 0..max_rounds {
            let mut was_truncated = false;
            let tools = vec![submit_alda_tool()];

            let events = self
                .client
                .chat_stream(messages.clone(), Some(tools))
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

    msg.push_str("创作要求：\n\n");

    if !request.source_material.is_empty() {
        msg.push_str("【素材】\n");
        msg.push_str(&request.source_material);
        msg.push_str("\n\n");
    }

    if !request.instructions.trim().is_empty() {
        msg.push_str("【用户要求】\n");
        msg.push_str(request.instructions.trim());
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

    msg.push_str("\n请按照工作流程创作：先解读素材并说明配器理由，然后提交完整的 Alda 乐谱。");
    msg
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

    // 如果有时长失败, 计算精确的倍数并给出具体操作建议
    if let Some(dur_check) = checks
        .iter()
        .find(|c| c.name == "时长" && c.status == CheckStatus::Fail)
        && let Some((actual, target)) = parse_duration_values(&dur_check.detail)
        && actual > 0.0
    {
        let multiplier = target / actual;
        let _ = writeln!(
            msg,
            "\n**时长修正指南**: 当前作品约 {actual:.0} 秒, 需要达到 {target:.0} 秒. 需要将内容量扩大到约 **{multiplier:.1}** 倍."
        );
        msg.push_str("具体做法:\n");
        let _ = writeln!(
            msg,
            "- 将所有反复次数乘以 {:.0} (如 `*2` 变成 `*{:.0}`)",
            multiplier.ceil(),
            (2.0 * multiplier).ceil()
        );
        let _ = writeln!(
            msg,
            "- 或者在现有 tempo 基础上降低, 例如 `(tempo {:.0})` 降低到 `(tempo {:.0})`",
            120.0,
            (120.0 / multiplier).ceil()
        );
        msg.push_str("- 或者增加新的变奏段落来扩展结构\n");
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
            mode: CreationMode::FullPiece,
            target_duration_secs: None,
            included_instruments: Vec::new(),
            excluded_instruments: Vec::new(),
            max_rounds,
        }
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
