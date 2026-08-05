use crate::alda::{AldaCheck, AldaRunner, CheckStatus};
use crate::deepseek::{DeepSeekClient, FunctionDef, Message, StreamEvent, Tool};
use anyhow::{Context, Result, bail};
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
                        "description": "完整的 Alda 乐谱代码"
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

pub struct CreationResult {
    /// 实际使用的修正轮数
    pub rounds: usize,
    /// 是否通过必要检查
    pub success: bool,
    /// 最后一轮的检查结果
    pub checks: Vec<AldaCheck>,
    /// 通过时的 Alda 源码
    pub alda_code: Option<String>,
    /// 模型的解读文本
    pub interpretation: String,
    /// 是否被截断
    pub was_truncated: bool,
}

// ============================================================
// Agent
// ============================================================

pub struct Agent {
    client: DeepSeekClient,
    runner: AldaRunner,
}

impl Agent {
    pub fn new(client: DeepSeekClient, runner: AldaRunner) -> Self {
        Agent { client, runner }
    }

    pub async fn create(&self, request: CreationRequest) -> Result<CreationResult> {
        let max_rounds = request.max_rounds.max(1);
        let target_duration_ms = request.target_duration_secs.map(|s| s * 1000.0);

        // 构建初始消息
        let mut messages = Vec::new();

        // system
        messages.push(Message {
            role: "system".to_string(),
            content: Some(SYSTEM_PROMPT.to_string()),
            tool_calls: None,
            tool_call_id: None,
        });

        // user
        let user_content = build_user_message(&request);
        messages.push(Message {
            role: "user".to_string(),
            content: Some(user_content),
            tool_calls: None,
            tool_call_id: None,
        });

        let mut interpretation = String::new();
        let mut last_alda_code: Option<String> = None;
        let mut last_checks: Vec<AldaCheck> = Vec::new();
        let mut was_truncated = false;

        for round in 0..max_rounds {
            let tools = vec![submit_alda_tool()];

            let events = self
                .client
                .chat_stream(messages.clone(), Some(tools))
                .await?;

            // 收集文本和工具调用
            let mut tool_call_args: Option<(Option<String>, String, String)> = None; // (id, name, args)

            for event in &events {
                match event {
                    StreamEvent::Text(text) => {
                        interpretation.push_str(text);
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
            let (tool_id, _, tool_args) = match tool_call_args {
                Some(args) => args,
                None => {
                    // 模型没有提交乐谱
                    bail!("模型未提交 Alda 乐谱（未调用 submit_alda 工具）")
                }
            };

            // 解析 alda_code
            let alda_code = parse_alda_code_from_args(&tool_args)?;

            // 写入临时文件并校验
            let tmp_dir = tempfile::tempdir().context("创建临时目录失败")?;
            let tmp_score = tmp_dir.path().join("candidate.alda");
            fs::write(&tmp_score, &alda_code)?;

            let mut checks = self.runner.validate(
                &tmp_score,
                &request.included_instruments,
                &request.excluded_instruments,
                target_duration_ms,
                10.0,
            );

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
            last_checks = checks.clone();

            if all_pass {
                return Ok(CreationResult {
                    rounds: round + 1,
                    success: true,
                    checks,
                    alda_code: Some(alda_code),
                    interpretation,
                    was_truncated,
                });
            }

            // 构建反馈消息
            let feedback = build_tool_feedback(&checks, tool_id.as_deref());

            // 追加 assistant 消息（工具调用）
            let tc_id = tool_id.clone().unwrap_or_else(|| "call_1".to_string());
            messages.push(Message {
                role: "assistant".to_string(),
                content: None,
                tool_calls: Some(vec![crate::deepseek::ToolCallMsg {
                    id: tc_id,
                    ty: "function".to_string(),
                    function: crate::deepseek::FunctionCallArgs {
                        name: "submit_alda".to_string(),
                        arguments: tool_args,
                    },
                }]),
                tool_call_id: None,
            });

            // 追加 tool 消息（校验结果）
            messages.push(Message {
                role: "tool".to_string(),
                content: Some(feedback),
                tool_calls: None,
                tool_call_id: tool_id,
            });
        }

        // 达到上限
        Ok(CreationResult {
            rounds: max_rounds,
            success: false,
            checks: last_checks,
            alda_code: last_alda_code,
            interpretation,
            was_truncated,
        })
    }
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

    msg.push_str("【模式】");
    msg.push_str(request.mode.description());
    msg.push('\n');

    if let Some(dur) = request.target_duration_secs {
        msg.push_str(&format!("【目标时长】约 {} 分钟\n", dur / 60.0));
    }

    if !request.included_instruments.is_empty() {
        msg.push_str(&format!(
            "【必须包含的乐器】{}\n",
            request.included_instruments.join("、")
        ));
    }

    if !request.excluded_instruments.is_empty() {
        msg.push_str(&format!(
            "【必须排除的乐器】{}\n",
            request.excluded_instruments.join("、")
        ));
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
        .map(|s| s.trim())
        .unwrap_or(code);
    let code = code.strip_suffix("```").map(|s| s.trim()).unwrap_or(code);

    Ok(code.to_string())
}

fn build_tool_feedback(checks: &[AldaCheck], tool_call_id: Option<&str>) -> String {
    let failures: Vec<_> = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Fail)
        .collect();

    if failures.is_empty() {
        return "✅ 所有检查通过，作品已保存。".to_string();
    }

    let mut msg = format!(
        "❌ 校验失败 ({}/{} 项)  call_id: {:?}。请根据以下诊断修改作品：\n\n",
        failures.len(),
        checks.len(),
        tool_call_id
    );
    for c in checks {
        msg.push_str(&format!("- {}: {} — {}\n", c.name, c.status, c.detail));
    }
    msg.push_str("\n请修改 Alda 乐谱后重新提交。");
    msg
}
