use alda_agent::{Cli, Command};
use anyhow::Context;
use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Doctor => alda_agent::doctor::run(),
        Command::Smoke => smoke().await,
        Command::AldaSmoke => smoke_alda(),
        Command::Create {
            file,
            mode,
            duration,
            include,
            exclude,
            output,
        } => create(file, mode, duration, include, exclude, output).await,
    }
}

async fn smoke() -> anyhow::Result<()> {
    use alda_agent::config::Config;
    use alda_agent::deepseek::{DeepSeekClient, Message};

    let config = Config::from_env_file()?;

    println!("=== DeepSeek 连通测试 ===\n");
    println!("模型: {}", config.model);
    println!("端点: {}", config.base_url);
    println!();

    let client = DeepSeekClient::new(
        config.api_key.clone(),
        config.base_url.clone(),
        config.model.clone(),
    )?;

    // 测试 1: 简单流式对话
    println!("--- 测试 1: 简单流式对话 ---");
    let messages = vec![Message {
        role: "user".to_string(),
        content: Some("用一句话介绍你自己。".to_string()),
        tool_calls: None,
        tool_call_id: None,
    }];

    match client.chat_stream(messages, None).await {
        Ok(events) => {
            let text_count = events
                .iter()
                .filter(|e| matches!(e, alda_agent::deepseek::StreamEvent::Text(_)))
                .count();
            let done = events
                .iter()
                .any(|e| matches!(e, alda_agent::deepseek::StreamEvent::Done { .. }));
            println!();
            println!("  收到 {} 个文本事件, done={}", text_count, done);
        }
        Err(e) => {
            println!("  失败: {}", e);
        }
    }
    println!();

    // 测试 2: 带工具调用的请求
    println!("--- 测试 2: 工具调用 ---");
    let tools = vec![alda_agent::deepseek::Tool {
        ty: "function".to_string(),
        function: alda_agent::deepseek::FunctionDef {
            name: "submit_alda".to_string(),
            description: "提交一段 Alda 乐谱代码".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "alda_code": {
                        "type": "string",
                        "description": "Alda 乐谱代码"
                    }
                },
                "required": ["alda_code"]
            }),
        },
    }];

    let messages = vec![Message {
        role: "user".to_string(),
        content: Some("用 Alda 写一个两小节的 C 大调和弦进行。".to_string()),
        tool_calls: None,
        tool_call_id: None,
    }];

    match client.chat_stream(messages, Some(tools)).await {
        Ok(events) => {
            let tool_calls: Vec<_> = events
                .iter()
                .filter(|e| matches!(e, alda_agent::deepseek::StreamEvent::ToolCall { .. }))
                .collect();
            let done = events
                .iter()
                .any(|e| matches!(e, alda_agent::deepseek::StreamEvent::Done { .. }));
            println!();
            println!("  工具调用: {} 个, done={}", tool_calls.len(), done);
            for tc in &tool_calls {
                if let alda_agent::deepseek::StreamEvent::ToolCall {
                    name, arguments, ..
                } = tc
                {
                    println!("  工具: {}, 参数长度: {} 字节", name, arguments.len());
                }
            }
        }
        Err(e) => {
            println!("  失败: {}", e);
        }
    }
    println!();

    println!("=== 测试摘要 ===");
    println!("本测试仅验证 API 连通性，不包含完整素材内容。");
    Ok(())
}

fn smoke_alda() -> anyhow::Result<()> {
    use alda_agent::alda::{AldaRunner, find_alda};
    use std::path::PathBuf;

    println!("=== Alda 工具连通测试 ===\n");

    let alda_path = find_alda().ok_or_else(|| anyhow::anyhow!("未找到 alda，请先安装"))?;
    println!("alda 路径: {}", alda_path.display());

    let runner = AldaRunner::new(alda_path);

    // 测试 1: 语法检查
    println!("\n--- 测试 1: 语法检查 ---");
    let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let valid = fixture_dir.join("valid_simple.alda");
    let invalid = fixture_dir.join("invalid_syntax.alda");

    match runner.parse(&valid) {
        Ok(info) => println!(
            "  ✅ 合法文件解析成功: {} 声部, {:.1}秒",
            info.part_count,
            info.duration_ms / 1000.0
        ),
        Err(e) => println!("  ❌ 合法文件解析失败: {}", e),
    }

    match runner.parse(&invalid) {
        Ok(_) => println!("  ❌ 非法文件未报错"),
        Err(e) => println!("  ✅ 非法文件正确报错: {}", e),
    }

    // 测试 2: 时长推导
    println!("\n--- 测试 2: 时长推导 ---");
    let valid_long = fixture_dir.join("valid_multi_part.alda");
    match runner.parse(&valid_long) {
        Ok(info) => {
            println!(
                "  {} 声部, 时长 {:.2}秒",
                info.part_count,
                info.duration_ms / 1000.0
            );
            for inst in &info.instruments {
                println!("    - {}", inst);
            }
        }
        Err(e) => println!("  ❌ 错误: {}", e),
    }

    // 测试 3: 乐器列表
    println!("\n--- 测试 3: 乐器列表 ---");
    match runner.list_instruments() {
        Ok(instruments) => println!("  ✅ {} 种可用乐器", instruments.len()),
        Err(e) => println!("  ❌ 错误: {}", e),
    }

    // 测试 4: 乐器检查（validate）
    println!("\n--- 测试 4: 乐器检查 ---");
    let no_constraint = runner.validate(&valid, &[], &[], None, 10.0);
    for check in &no_constraint {
        println!("  {}: {} — {}", check.name, check.status, check.detail);
    }

    let exclude_piano = runner.validate(&valid, &[], &["piano".to_string()], None, 10.0);
    for check in &exclude_piano {
        println!("  {}: {} — {}", check.name, check.status, check.detail);
    }

    // 测试 5: 时长检查
    println!("\n--- 测试 5: 时长检查 ---");
    let info = runner.parse(&valid)?;
    let target = info.duration_ms as f64;
    let checks = runner.validate(&valid, &[], &[], Some(target), 10.0);
    for check in &checks {
        println!("  {}: {} — {}", check.name, check.status, check.detail);
    }

    // 测试 6: MIDI 导出
    println!("\n--- 测试 6: MIDI 导出 ---");
    let tmp = std::env::temp_dir().join(format!("alda_smoke_{}.mid", std::process::id()));
    match runner.export_midi(&valid, &tmp) {
        Ok(path) => {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            println!("  ✅ MIDI 导出成功: {} ({} 字节)", path.display(), size);
        }
        Err(e) => println!("  ❌ 导出失败: {}", e),
    }

    println!("\n=== Alda 工具测试完成 ===");
    Ok(())
}

async fn create(
    file: Option<std::path::PathBuf>,
    mode: String,
    duration: Option<f64>,
    include: Vec<String>,
    exclude: Vec<String>,
    output: std::path::PathBuf,
) -> anyhow::Result<()> {
    use alda_agent::agent::{Agent, CreationMode, CreationRequest};
    use alda_agent::alda::{AldaRunner, CheckStatus, find_alda};
    use alda_agent::config::Config;
    use alda_agent::deepseek::DeepSeekClient;
    use std::io::Read;

    // 读取素材
    let source_material = if let Some(ref path) = file {
        std::fs::read_to_string(path).context("无法读取素材文件")?
    } else {
        eprintln!("请输入创作素材（Ctrl+D 结束）:");
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("无法读取 stdin")?;
        buf.trim().to_string()
    };

    if source_material.is_empty() {
        anyhow::bail!("素材不能为空");
    }

    // 初始化
    let config = Config::from_env_file()?;
    let client = DeepSeekClient::new(
        config.api_key.clone(),
        config.base_url.clone(),
        config.model.clone(),
    )?;
    let alda_path = find_alda().ok_or_else(|| anyhow::anyhow!("未找到 alda，请先安装"))?;
    let runner = AldaRunner::new(alda_path);
    let agent = Agent::new(client, runner);

    let creation_mode = match mode.as_str() {
        "full" => CreationMode::FullPiece,
        "improv" => CreationMode::Improvisation,
        other => anyhow::bail!("无效的创作模式: {}（应为 full 或 improv）", other),
    };

    let request = CreationRequest {
        source_material,
        mode: creation_mode,
        target_duration_secs: duration,
        included_instruments: include,
        excluded_instruments: exclude,
        max_rounds: 3,
    };

    println!("\n=== 开始创作 ===\n");

    let result = agent.create(request).await?;

    println!("\n=== 创作完成 ({}/{} 轮) ===\n", result.rounds, 3);
    println!(
        "状态: {}",
        if result.success {
            "✅ 成功"
        } else {
            "❌ 失败"
        }
    );
    println!();

    println!("校验结果:");
    for check in &result.checks {
        let icon = match check.status {
            CheckStatus::Pass => "✅",
            CheckStatus::Fail => "❌",
            CheckStatus::Unchecked => "⏭ ",
        };
        println!("  {} {}: {}", icon, check.name, check.detail);
    }

    if result.success {
        if let Some(ref code) = result.alda_code {
            let output_file = output.join("current.alda");
            std::fs::write(&output_file, code)?;
            println!("\n作品已保存到: {}", output_file.display());
        }
    } else {
        if result.was_truncated {
            println!("\n⚠️  模型输出被截断，作品可能不完整。");
        }
    }

    Ok(())
}
