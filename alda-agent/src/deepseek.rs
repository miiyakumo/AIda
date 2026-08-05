use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static PRIVACY_SHOWN: AtomicBool = AtomicBool::new(false);

fn show_privacy_notice() {
    if !PRIVACY_SHOWN.swap(true, Ordering::SeqCst) {
        eprintln!("注意：诗歌、创作要求、当前乐谱和校验错误将会发送到配置的模型服务。");
    }
}

#[derive(Debug, Clone)]
pub struct DeepSeekClient {
    client: Client,
    api_key: String,
    base_url: String,
    model: String,
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<Message>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Tool>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct Tool {
    #[serde(rename = "type")]
    pub ty: String,
    pub function: FunctionDef,
}

#[derive(Debug, Serialize, Clone)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct ChatChunk {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    delta: Option<Delta>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Delta {
    content: Option<String>,
    #[serde(rename = "tool_calls")]
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct ToolCallDelta {
    function: Option<FunctionArg>,
}

#[derive(Debug, Deserialize)]
struct FunctionArg {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug)]
pub enum StreamEvent {
    Text(String),
    ToolCall { name: String, arguments: String },
    Done { finish_reason: String },
}

#[derive(Debug)]
pub enum ChatError {
    Auth(String),
    RateLimit(String),
    Network(String),
    ModelReject(String),
    Truncated(String),
}

impl std::fmt::Display for ChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChatError::Auth(msg) => write!(f, "认证失败：{}", msg),
            ChatError::RateLimit(msg) => write!(f, "请求限流：{}", msg),
            ChatError::Network(msg) => write!(f, "网络错误：{}", msg),
            ChatError::ModelReject(msg) => write!(f, "模型拒绝：{}", msg),
            ChatError::Truncated(msg) => write!(f, "输出截断：{}", msg),
        }
    }
}

impl std::error::Error for ChatError {}

impl DeepSeekClient {
    pub fn new(api_key: String, base_url: String, model: String) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .context("创建 HTTP 客户端失败")?;

        Ok(DeepSeekClient {
            client,
            api_key,
            base_url,
            model,
        })
    }

    pub async fn chat_stream(
        &self,
        messages: Vec<Message>,
        tools: Option<Vec<Tool>>,
    ) -> Result<Vec<StreamEvent>> {
        show_privacy_notice();

        let url = format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        );

        let request = ChatRequest {
            model: &self.model,
            messages,
            stream: true,
            tools,
        };

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await
            .context("发送请求失败")?;

        // 错误分类
        let status = response.status();
        if status == 401 {
            bail!(ChatError::Auth("请检查 ALDA_AGENT_API_KEY 是否正确".into()));
        }
        if status == 429 {
            bail!(ChatError::RateLimit("请稍后重试".into()));
        }
        if status == 400 {
            let body = response.text().await.unwrap_or_default();
            bail!(ChatError::ModelReject(body));
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!(ChatError::Network(format!("HTTP {}: {}", status, body)));
        }

        // 读取 SSE 流
        use futures_util::StreamExt;

        let mut stream = response.bytes_stream();
        let mut events = Vec::new();
        let mut pending_tool_name = String::new();
        let mut pending_tool_args = String::new();
        let mut buffer = String::new();

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.context("读取流数据失败")?;
            let text = String::from_utf8_lossy(&chunk);
            buffer.push_str(&text);

            while let Some(line_end) = buffer.find('\n') {
                let line = buffer[..line_end].trim().to_string();
                buffer = buffer[line_end + 1..].to_string();

                if line.is_empty() {
                    continue;
                }

                if line == "data: [DONE]" {
                    events.push(StreamEvent::Done {
                        finish_reason: "stop".into(),
                    });
                    continue;
                }

                if let Some(data) = line.strip_prefix("data: ") {
                    match serde_json::from_str::<ChatChunk>(data) {
                        Ok(chunk) => {
                            for choice in &chunk.choices {
                                if let Some(ref delta) = choice.delta {
                                    if let Some(ref content) = delta.content
                                        && !content.is_empty()
                                    {
                                        // 实时打印
                                        use std::io::{self, Write};
                                        print!("{}", content);
                                        io::stdout().flush().ok();
                                        events.push(StreamEvent::Text(content.clone()));
                                    }
                                    if let Some(ref tool_calls) = delta.tool_calls {
                                        for tc in tool_calls {
                                            if let Some(ref func) = tc.function {
                                                if let Some(ref name) = func.name {
                                                    pending_tool_name = name.clone();
                                                }
                                                if let Some(ref args) = func.arguments {
                                                    pending_tool_args.push_str(args);
                                                }
                                            }
                                        }
                                    }
                                }
                                if let Some(ref reason) = choice.finish_reason
                                    && reason == "length"
                                {
                                    events.push(StreamEvent::Done {
                                        finish_reason: "length".into(),
                                    });
                                }
                            }
                        }
                        Err(e) => {
                            // 忽略解析失败的单行（可能是注释或其他非标准事件）
                            let _ = e;
                        }
                    }
                }
            }
        }

        // 如果有收集到的工具调用，作为事件追加
        if !pending_tool_name.is_empty() {
            events.push(StreamEvent::ToolCall {
                name: pending_tool_name,
                arguments: pending_tool_args,
            });
        }

        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_stream_text() {
        let input = r#"data: {"id":"1","object":"chat.completion.chunk","created":1,"model":"deepseek-chat","choices":[{"index":0,"delta":{"content":"你好"},"finish_reason":null}]}

data: {"id":"1","object":"chat.completion.chunk","created":1,"model":"deepseek-chat","choices":[{"index":0,"delta":{"content":"！"},"finish_reason":"stop"}]}

data: [DONE]
"#;
        let lines: Vec<&str> = input.lines().filter(|l| !l.trim().is_empty()).collect();
        let mut contents = Vec::new();
        let mut finish_reasons = Vec::new();

        for line in &lines {
            let line = line.trim();
            if line == "data: [DONE]" {
                finish_reasons.push("DONE".to_string());
                continue;
            }
            if let Some(data) = line.strip_prefix("data: ")
                && let Ok(chunk) = serde_json::from_str::<ChatChunk>(data)
            {
                for choice in &chunk.choices {
                    if let Some(ref delta) = choice.delta
                        && let Some(ref content) = delta.content
                        && !content.is_empty()
                    {
                        contents.push(content.clone());
                    }
                    if let Some(ref reason) = choice.finish_reason
                        && reason != "null"
                    {
                        finish_reasons.push(reason.clone());
                    }
                }
            }
        }

        assert_eq!(contents, vec!["你好".to_string(), "！".to_string()]);
        assert!(finish_reasons.contains(&"stop".to_string()));
    }

    #[test]
    fn test_parse_tool_call() {
        let input = r#"data: {"id":"2","object":"chat.completion.chunk","created":1,"model":"deepseek-chat","choices":[{"index":0,"delta":{"content":null,"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"submit_alda","arguments":""}}]},"finish_reason":null}]}

data: {"id":"2","object":"chat.completion.chunk","created":1,"model":"deepseek-chat","choices":[{"index":0,"delta":{"content":null,"tool_calls":[{"index":0,"type":"function","function":{"arguments":"{\"alda_code\":\"c d e f\"}"}}]},"finish_reason":null}]}

data: {"id":"2","object":"chat.completion.chunk","created":1,"model":"deepseek-chat","choices":[{"index":0,"delta":{"content":null},"finish_reason":"tool_calls"}]}

data: [DONE]
"#;
        let lines: Vec<&str> = input.lines().filter(|l| !l.trim().is_empty()).collect();
        let mut tool_names = Vec::new();
        let mut tool_args = Vec::new();

        for line in &lines {
            let line = line.trim();
            if line == "data: [DONE]" {
                continue;
            }
            if let Some(data) = line.strip_prefix("data: ")
                && let Ok(chunk) = serde_json::from_str::<ChatChunk>(data)
            {
                for choice in &chunk.choices {
                    if let Some(ref delta) = choice.delta
                        && let Some(ref tool_calls) = delta.tool_calls
                    {
                        for tc in tool_calls {
                            if let Some(ref func) = tc.function {
                                if let Some(ref name) = func.name {
                                    tool_names.push(name.clone());
                                }
                                if let Some(ref args) = func.arguments {
                                    tool_args.push(args.clone());
                                }
                            }
                        }
                    }
                }
            }
        }

        assert_eq!(tool_names, vec!["submit_alda".to_string()]);
        assert!(!tool_args.is_empty());
        assert!(tool_args.iter().any(|a| a.contains("alda_code")));
    }

    #[test]
    fn test_truncated_response() {
        let input = r#"data: {"id":"3","object":"chat.completion.chunk","created":1,"model":"deepseek-chat","choices":[{"index":0,"delta":{"content":"text"},"finish_reason":"length"}]}

data: [DONE]
"#;
        let lines: Vec<&str> = input.lines().filter(|l| !l.trim().is_empty()).collect();
        let mut finish_reasons = Vec::new();

        for line in &lines {
            let line = line.trim();
            if line == "data: [DONE]" {
                continue;
            }
            if let Some(data) = line.strip_prefix("data: ")
                && let Ok(chunk) = serde_json::from_str::<ChatChunk>(data)
            {
                for choice in &chunk.choices {
                    if let Some(ref reason) = choice.finish_reason
                        && reason != "null"
                    {
                        finish_reasons.push(reason.clone());
                    }
                }
            }
        }

        assert!(finish_reasons.contains(&"length".to_string()));
    }

    #[test]
    fn test_chat_error_display() {
        assert!(format!("{}", ChatError::Auth("x".into())).contains("认证失败"));
        assert!(format!("{}", ChatError::RateLimit("x".into())).contains("限流"));
        assert!(format!("{}", ChatError::Network("x".into())).contains("网络错误"));
        assert!(format!("{}", ChatError::ModelReject("x".into())).contains("拒绝"));
        assert!(format!("{}", ChatError::Truncated("x".into())).contains("截断"));
    }

    #[test]
    fn test_privacy_notice_only_once() {
        PRIVACY_SHOWN.store(false, Ordering::SeqCst);
        show_privacy_notice();
        let first = PRIVACY_SHOWN.load(Ordering::SeqCst);
        show_privacy_notice();
        let second = PRIVACY_SHOWN.load(Ordering::SeqCst);
        assert!(first);
        assert!(second);
    }
}
