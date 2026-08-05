use alda_agent::{Cli, Command};
use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Doctor => alda_agent::doctor::run(),
        Command::Smoke => smoke().await,
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
        content: "用一句话介绍你自己。".to_string(),
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
        content: "用 Alda 写一个两小节的 C 大调和弦进行。".to_string(),
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
                if let alda_agent::deepseek::StreamEvent::ToolCall { name, arguments } = tc {
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
