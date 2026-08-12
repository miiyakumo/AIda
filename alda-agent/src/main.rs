use alda_agent::{Cli, Command};
use anyhow::Context;
use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::List => list_projects(),
        Command::Repl {
            project,
            name,
            mode,
            duration,
            include,
            exclude,
        } => {
            let project = if let Some(project) = project {
                project
            } else if let Some(name) = name {
                alda_agent::project::default_project_dir(&name)?
            } else {
                std::env::current_dir()?
            };
            run_cancelable(alda_agent::repl::run_repl(
                project,
                alda_agent::repl::ReplSettings {
                    mode,
                    target_duration_secs: duration,
                    included_instruments: include,
                    excluded_instruments: exclude,
                },
            ))
            .await
        }
        Command::Doctor => alda_agent::doctor::run(),
        Command::Smoke => run_cancelable(smoke()).await,
        Command::AldaSmoke => {
            run_cancelable(async {
                tokio::task::spawn_blocking(smoke_alda)
                    .await
                    .context("Alda smoke 任务异常退出")??;
                Ok(())
            })
            .await
        }
        Command::Create {
            file,
            mode,
            duration,
            include,
            exclude,
            output,
        } => run_cancelable(create(file, mode, duration, include, exclude, output)).await,
    }
}

async fn run_cancelable<F>(operation: F) -> anyhow::Result<()>
where
    F: std::future::Future<Output = anyhow::Result<()>>,
{
    alda_agent::alda::reset_cancellation();
    tokio::select! {
        result = operation => result,
        signal = tokio::signal::ctrl_c() => {
            signal.context("无法监听 Ctrl+C")?;
            alda_agent::alda::request_cancellation();
            anyhow::bail!("操作已由用户取消")
        }
    }
}

fn list_projects() -> anyhow::Result<()> {
    let projects = alda_agent::project::list_projects()?;
    if projects.is_empty() {
        println!("默认目录中没有项目。");
    }
    for (name, path) in projects {
        println!("{}\t{}", name, path.display());
    }
    Ok(())
}

// A linear transcript keeps the explicit smoke output easy to compare across runs.
#[allow(clippy::too_many_lines)]
async fn smoke() -> anyhow::Result<()> {
    use alda_agent::config::Config;
    use alda_agent::deepseek::{DeepSeekClient, Message};

    let config = Config::from_env_file()?;

    println!("=== DeepSeek 连通测试 ===\n");
    println!("模型: {}", config.model);
    println!("端点: {}", config.base_url);
    println!("Thinking: {}", config.thinking);
    println!();

    let client = DeepSeekClient::new_with_thinking(
        config.api_key.clone(),
        config.base_url.clone(),
        config.model.clone(),
        config.thinking.clone(),
    )?;
    let mut failures = 0_u32;

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
            println!("  收到 {text_count} 个文本事件, done={done}");
            if text_count == 0 || !done {
                failures += 1;
            }
        }
        Err(e) => {
            println!("  失败: {e}");
            failures += 1;
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

    match client.chat_stream(messages, Some(tools.clone())).await {
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
            if tool_calls.is_empty() || !done {
                failures += 1;
            }
        }
        Err(e) => {
            println!("  失败: {e}");
            failures += 1;
        }
    }
    println!();

    println!("--- 测试 3: 长工具输出与截断状态 ---");
    let messages = vec![Message {
        role: "user".to_string(),
        content: Some(
            "调用 submit_alda，提交一份至少 200 行、结构完整的 Alda 乐谱，用于验证长输出。"
                .to_string(),
        ),
        tool_calls: None,
        tool_call_id: None,
    }];
    match client.chat_stream(messages, Some(tools)).await {
        Ok(events) => {
            let argument_bytes = events
                .iter()
                .filter_map(|event| {
                    if let alda_agent::deepseek::StreamEvent::ToolCall { arguments, .. } = event {
                        Some(arguments.len())
                    } else {
                        None
                    }
                })
                .sum::<usize>();
            let truncated = events.iter().any(|event| {
                matches!(
                    event,
                    alda_agent::deepseek::StreamEvent::Done { finish_reason }
                        if finish_reason == "length"
                )
            });
            println!("  工具参数: {argument_bytes} 字节, truncated={truncated}");
            if argument_bytes < 1_000 || truncated {
                failures += 1;
            }
        }
        Err(error) => {
            println!("  失败: {error}");
            failures += 1;
        }
    }
    println!();

    println!("=== 测试摘要 ===");
    println!("本测试仅验证 API 连通性，不包含完整素材内容。");
    if failures > 0 {
        anyhow::bail!("DeepSeek 连通测试有 {failures} 项失败");
    }
    Ok(())
}

fn smoke_alda() -> anyhow::Result<()> {
    use alda_agent::alda::{AldaRunner, find_alda};
    use std::path::PathBuf;

    println!("=== Alda 工具连通测试 ===\n");

    let alda_path = find_alda().ok_or_else(|| anyhow::anyhow!("未找到 alda，请先安装"))?;
    println!("alda 路径: {}", alda_path.display());

    let runner = AldaRunner::new(alda_path);
    let mut failures = 0_u32;

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
        Err(e) => {
            println!("  ❌ 合法文件解析失败: {e}");
            failures += 1;
        }
    }

    match runner.parse(&invalid) {
        Ok(_) => {
            println!("  ❌ 非法文件未报错");
            failures += 1;
        }
        Err(e) => println!("  ✅ 非法文件正确报错: {e}"),
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
                println!("    - {inst}");
            }
        }
        Err(e) => {
            println!("  ❌ 错误: {e}");
            failures += 1;
        }
    }

    // 测试 3: 乐器列表
    println!("\n--- 测试 3: 乐器列表 ---");
    match runner.list_instruments() {
        Ok(instruments) => println!("  ✅ {} 种可用乐器", instruments.len()),
        Err(e) => {
            println!("  ❌ 错误: {e}");
            failures += 1;
        }
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
    let temporary = tempfile::tempdir()?;
    let tmp = temporary.path().join("smoke.mid");
    match runner.export_midi(&valid, &tmp) {
        Ok(path) => {
            let size = std::fs::metadata(&path).map_or(0, |m| m.len());
            println!("  ✅ MIDI 导出成功: {} ({} 字节)", path.display(), size);
        }
        Err(e) => {
            println!("  ❌ 导出失败: {e}");
            failures += 1;
        }
    }

    println!("\n--- 测试 7: 播放与停止 ---");
    if let Err(error) = runner.play(&valid).and_then(|()| runner.stop()) {
        println!("  ❌ 播放/停止失败: {error}");
        failures += 1;
    } else {
        println!("  ✅ 播放与停止命令成功");
    }

    println!("\n=== Alda 工具测试完成 ===");
    if failures > 0 {
        anyhow::bail!("Alda 工具测试有 {failures} 项失败");
    }
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
    let client = DeepSeekClient::new_with_thinking(
        config.api_key.clone(),
        config.base_url.clone(),
        config.model.clone(),
        config.thinking.clone(),
    )?;
    let alda_path = find_alda().ok_or_else(|| anyhow::anyhow!("未找到 alda，请先安装"))?;
    let runner = AldaRunner::new(alda_path);
    let agent = Agent::new(client, runner);

    let creation_mode = match mode.as_str() {
        "full" => CreationMode::FullPiece,
        "improv" => CreationMode::Improvisation,
        other => anyhow::bail!("无效的创作模式: {other}（应为 full 或 improv）"),
    };

    let request = CreationRequest {
        source_material,
        instructions: String::new(),
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

    if result.needs_input {
        anyhow::bail!("模型需要补充信息；请使用 repl 交互回答澄清问题");
    } else if result.success {
        if let Some(ref code) = result.alda_code {
            let output_file = output.join("current.alda");
            std::fs::write(&output_file, code)?;
            println!("\n作品已保存到: {}", output_file.display());
        }
    } else {
        if result.was_truncated {
            println!("\n⚠️  模型输出被截断，作品可能不完整。");
        }
        anyhow::bail!("作品在 {} 轮校验后仍未通过，未保存有效版本", result.rounds);
    }

    Ok(())
}
