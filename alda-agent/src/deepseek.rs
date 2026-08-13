use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::error::Error as _;
use std::time::Duration;

const MAX_STREAM_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_TOOL_ARGUMENT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThinkingMode {
    Enabled,
    Disabled,
}

impl ThinkingMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReasoningEffort {
    Low,
    High,
    Max,
}

impl ReasoningEffort {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::High => "high",
            Self::Max => "max",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingOptions {
    mode: ThinkingMode,
    reasoning_effort: Option<ReasoningEffort>,
}

impl ThinkingOptions {
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            mode: ThinkingMode::Disabled,
            reasoning_effort: None,
        }
    }

    pub fn from_config(mode: Option<&str>, reasoning_effort: Option<&str>) -> Result<Self> {
        let mode = match mode.unwrap_or("disabled") {
            "enabled" => ThinkingMode::Enabled,
            "disabled" => ThinkingMode::Disabled,
            value => bail!("thinking 必须是 enabled 或 disabled，当前为 {value:?}"),
        };
        let reasoning_effort = reasoning_effort
            .map(|value| match value {
                "low" => Ok(ReasoningEffort::Low),
                "high" => Ok(ReasoningEffort::High),
                "max" => Ok(ReasoningEffort::Max),
                _ => bail!("reasoning effort 必须是 low、high 或 max，当前为 {value:?}"),
            })
            .transpose()?;
        if mode == ThinkingMode::Disabled && reasoning_effort.is_some() {
            bail!("thinking=disabled 时不能设置 reasoning effort");
        }
        Ok(Self {
            mode,
            reasoning_effort,
        })
    }

    #[must_use]
    pub const fn mode(&self) -> &'static str {
        self.mode.as_str()
    }

    #[must_use]
    pub const fn reasoning_effort(&self) -> Option<&'static str> {
        match self.reasoning_effort {
            Some(effort) => Some(effort.as_str()),
            None => None,
        }
    }
}

impl std::fmt::Display for ThinkingOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.reasoning_effort() {
            Some(effort) => write!(f, "{}（effort={effort}）", self.mode()),
            None => f.write_str(self.mode()),
        }
    }
}

fn reqwest_error_detail(error: &reqwest::Error) -> String {
    let mut detail = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let message = cause.to_string();
        if !detail.contains(&message) {
            detail.push_str(": ");
            detail.push_str(&message);
        }
        source = cause.source();
    }
    detail
}

#[derive(Debug, Clone)]
pub struct DeepSeekClient {
    client: Client,
    api_key: String,
    base_url: String,
    model: String,
    thinking: ThinkingOptions,
}

#[derive(Debug, Serialize)]
struct ThinkingRequest<'a> {
    #[serde(rename = "type")]
    ty: &'a str,
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<Message>,
    stream: bool,
    thinking: ThinkingRequest<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Tool>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallMsg>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ToolCallMsg {
    pub id: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub function: FunctionCallArgs,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FunctionCallArgs {
    pub name: String,
    pub arguments: String,
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
    #[allow(dead_code)]
    index: Option<i32>,
    id: Option<String>,
    function: Option<FunctionArg>,
}

#[derive(Debug, Deserialize)]
struct FunctionArg {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    Text(String),
    ToolCall {
        id: Option<String>,
        name: String,
        arguments: String,
    },
    Done {
        finish_reason: String,
    },
}

#[derive(Debug)]
pub enum ChatError {
    Auth(String),
    RateLimit(String),
    Network(String),
    ModelReject(String),
}

impl std::fmt::Display for ChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChatError::Auth(msg) => write!(f, "认证失败：{msg}"),
            ChatError::RateLimit(msg) => write!(f, "请求限流：{msg}"),
            ChatError::Network(msg) => write!(f, "网络错误：{msg}"),
            ChatError::ModelReject(msg) => write!(f, "模型拒绝：{msg}"),
        }
    }
}

impl std::error::Error for ChatError {}

#[derive(Default)]
struct SseParser {
    events: Vec<StreamEvent>,
    pending_tool_id: Option<String>,
    pending_tool_name: String,
    pending_tool_args: String,
    finish_reason: Option<String>,
}

impl SseParser {
    fn push_line(&mut self, line: &str) -> Result<Vec<String>> {
        let line = line.trim();
        if line.is_empty() || line.starts_with(':') || line == "data: [DONE]" {
            return Ok(Vec::new());
        }

        let Some(data) = line.strip_prefix("data:") else {
            return Ok(Vec::new());
        };
        let chunk: ChatChunk =
            serde_json::from_str(data.trim()).context("无法解析模型服务返回的 SSE 数据")?;
        let mut text_chunks = Vec::new();

        for choice in chunk.choices {
            if let Some(delta) = choice.delta {
                if let Some(content) = delta.content {
                    if !content.is_empty() {
                        text_chunks.push(content.clone());
                        self.events.push(StreamEvent::Text(content));
                    }
                }
                if let Some(tool_calls) = delta.tool_calls {
                    for tool_call in tool_calls {
                        if let Some(id) = tool_call.id {
                            self.pending_tool_id = Some(id);
                        }
                        if let Some(function) = tool_call.function {
                            if let Some(name) = function.name {
                                self.pending_tool_name = name;
                            }
                            if let Some(arguments) = function.arguments {
                                if self.pending_tool_args.len().saturating_add(arguments.len())
                                    > MAX_TOOL_ARGUMENT_BYTES
                                {
                                    bail!(
                                        "模型返回的工具参数超过 {} KiB 上限",
                                        MAX_TOOL_ARGUMENT_BYTES / 1024
                                    );
                                }
                                self.pending_tool_args.push_str(&arguments);
                            }
                        }
                    }
                }
            }
            if let Some(reason) = choice.finish_reason {
                self.finish_reason = Some(reason);
            }
        }

        Ok(text_chunks)
    }

    fn finish(mut self) -> Vec<StreamEvent> {
        if !self.pending_tool_name.is_empty() {
            self.events.push(StreamEvent::ToolCall {
                id: self.pending_tool_id,
                name: self.pending_tool_name,
                arguments: self.pending_tool_args,
            });
        }
        self.events.push(StreamEvent::Done {
            finish_reason: self.finish_reason.unwrap_or_else(|| "stop".to_string()),
        });
        self.events
    }
}

#[cfg(test)]
fn parse_sse(input: &str) -> Result<Vec<StreamEvent>> {
    let mut parser = SseParser::default();
    for line in input.lines() {
        parser.push_line(line)?;
    }
    Ok(parser.finish())
}

impl DeepSeekClient {
    pub fn new(api_key: String, base_url: String, model: String) -> Result<Self> {
        Self::new_with_thinking(api_key, base_url, model, ThinkingOptions::disabled())
    }

    pub fn new_with_thinking(
        api_key: String,
        base_url: String,
        model: String,
        thinking: ThinkingOptions,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .context("创建 HTTP 客户端失败")?;

        Ok(DeepSeekClient {
            client,
            api_key,
            base_url,
            model,
            thinking,
        })
    }

    pub async fn chat_stream(
        &self,
        messages: Vec<Message>,
        tools: Option<Vec<Tool>>,
    ) -> Result<Vec<StreamEvent>> {
        self.chat_stream_with(messages, tools, |_| {}).await
    }

    pub async fn chat_stream_with(
        &self,
        messages: Vec<Message>,
        tools: Option<Vec<Tool>>,
        mut on_text: impl FnMut(&str),
    ) -> Result<Vec<StreamEvent>> {
        let url = chat_completions_url(&self.base_url);

        let request = ChatRequest {
            model: &self.model,
            messages,
            stream: true,
            thinking: ThinkingRequest {
                ty: self.thinking.mode(),
            },
            reasoning_effort: self.thinking.reasoning_effort(),
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
            .map_err(|error| anyhow::anyhow!(ChatError::Network(reqwest_error_detail(&error))))?;

        // 错误分类
        let status = response.status();
        if status == 401 {
            bail!(ChatError::Auth("请检查当前项目的模型密钥是否正确".into()));
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
            bail!(ChatError::Network(format!("HTTP {status}: {body}")));
        }

        // 读取 SSE 流
        let mut stream = response.bytes_stream();
        let mut parser = SseParser::default();
        let mut buffer = String::new();
        let mut received_bytes = 0_usize;

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.map_err(|error| {
                anyhow::anyhow!(ChatError::Network(format!(
                    "{}（已接收 {received_bytes} 字节）",
                    reqwest_error_detail(&error)
                )))
            })?;
            received_bytes = received_bytes.saturating_add(chunk.len());
            if received_bytes > MAX_STREAM_BYTES {
                bail!(ChatError::Network(format!(
                    "流式响应超过 {} MiB 上限",
                    MAX_STREAM_BYTES / 1024 / 1024
                )));
            }
            let text = String::from_utf8_lossy(&chunk);
            buffer.push_str(&text);

            while let Some(line_end) = buffer.find('\n') {
                let line = buffer[..line_end].to_string();
                buffer = buffer[line_end + 1..].to_string();
                for text in parser.push_line(&line)? {
                    on_text(&text);
                }
            }
        }

        if !buffer.trim().is_empty() {
            for text in parser.push_line(&buffer)? {
                on_text(&text);
            }
        }

        Ok(parser.finish())
    }
}

fn chat_completions_url(base_url: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    if base_url.ends_with("/v1") {
        format!("{base_url}/chat/completions")
    } else {
        format!("{base_url}/v1/chat/completions")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MockResponse, serve};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_parse_stream_text() {
        let events = parse_sse(include_str!("../tests/fixtures/stream_text.txt")).unwrap();
        assert_eq!(
            events,
            vec![
                StreamEvent::Text("你好".to_string()),
                StreamEvent::Text("！".to_string()),
                StreamEvent::Done {
                    finish_reason: "stop".to_string()
                }
            ]
        );
    }

    #[test]
    fn chat_url_accepts_origin_or_versioned_base() {
        assert_eq!(
            chat_completions_url("https://service.example"),
            "https://service.example/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("https://service.example/v1/"),
            "https://service.example/v1/chat/completions"
        );
    }

    #[test]
    fn test_parse_tool_call() {
        let events = parse_sse(include_str!("../tests/fixtures/stream_tool_call.txt")).unwrap();
        assert!(matches!(
            &events[0],
            StreamEvent::ToolCall { id: Some(id), name, arguments }
                if id == "call_1" && name == "submit_alda" && arguments.contains("alda_code")
        ));
        assert!(matches!(
            &events[1],
            StreamEvent::Done { finish_reason } if finish_reason == "tool_calls"
        ));
    }

    #[test]
    fn test_truncated_response() {
        let events = parse_sse(include_str!("../tests/fixtures/truncated.txt")).unwrap();
        assert!(matches!(
            events.last(),
            Some(StreamEvent::Done { finish_reason }) if finish_reason == "length"
        ));
    }

    #[test]
    fn oversized_tool_arguments_are_rejected_while_streaming() {
        let arguments = "x".repeat(MAX_TOOL_ARGUMENT_BYTES + 1);
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "id": "call_1",
                        "function": {
                            "name": "submit_alda",
                            "arguments": arguments
                        }
                    }]
                },
                "finish_reason": null
            }]
        });
        let mut parser = SseParser::default();
        let error = parser.push_line(&format!("data: {chunk}")).unwrap_err();
        assert!(error.to_string().contains("64 KiB"));
    }

    #[test]
    fn test_chat_error_display() {
        assert!(format!("{}", ChatError::Auth("x".into())).contains("认证失败"));
        assert!(format!("{}", ChatError::RateLimit("x".into())).contains("限流"));
        assert!(format!("{}", ChatError::Network("x".into())).contains("网络错误"));
        assert!(format!("{}", ChatError::ModelReject("x".into())).contains("拒绝"));
    }

    #[tokio::test]
    async fn production_http_path_sends_configured_model_and_parses_stream() {
        let fixture = include_str!("../tests/fixtures/stream_text.txt");
        let (base_url, request) = serve(vec![MockResponse::sse(fixture.to_string())]);
        let client = DeepSeekClient::new(
            "secret-test-value".to_string(),
            base_url,
            "example-model".to_string(),
        )
        .unwrap();
        let events = client
            .chat_stream(
                vec![Message {
                    role: "user".to_string(),
                    content: Some("hello".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                }],
                None,
            )
            .await
            .unwrap();
        assert!(
            matches!(events.last(), Some(StreamEvent::Done { finish_reason }) if finish_reason == "stop")
        );

        let request = String::from_utf8(request.recv().unwrap()).unwrap();
        assert!(request.starts_with("POST /v1/chat/completions "));
        let body = request.split("\r\n\r\n").nth(1).unwrap();
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["model"], "example-model");
        assert_eq!(body["stream"], true);
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("reasoning_effort").is_none());
    }

    #[tokio::test]
    async fn production_http_path_sends_enabled_thinking_effort() {
        let fixture = include_str!("../tests/fixtures/stream_text.txt");
        let (base_url, request) = serve(vec![MockResponse::sse(fixture.to_string())]);
        let thinking = ThinkingOptions::from_config(Some("enabled"), Some("low")).unwrap();
        let client = DeepSeekClient::new_with_thinking(
            "test-key".to_string(),
            base_url,
            "example-model".to_string(),
            thinking,
        )
        .unwrap();
        client.chat_stream(Vec::new(), None).await.unwrap();

        let request = String::from_utf8(request.recv().unwrap()).unwrap();
        let body = request.split("\r\n\r\n").nth(1).unwrap();
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "low");
    }

    #[tokio::test]
    async fn http_errors_are_classified() {
        for (status, expected) in [
            ("401 Unauthorized", "认证失败"),
            ("429 Too Many Requests", "限流"),
            ("400 Bad Request", "模型拒绝"),
            ("503 Service Unavailable", "网络错误"),
        ] {
            let (base_url, _request) = serve(vec![MockResponse::error(status, "error-body")]);
            let client = DeepSeekClient::new(
                "test-key".to_string(),
                base_url,
                "example-model".to_string(),
            )
            .unwrap();
            let error = client.chat_stream(Vec::new(), None).await.unwrap_err();
            assert!(error.to_string().contains(expected), "{status}: {error:#}");
        }
    }

    #[tokio::test]
    async fn dropping_chat_future_cancels_the_open_stream() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (started_sender, started_receiver) = mpsc::channel();
        let (closed_sender, closed_receiver) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
                )
                .unwrap();
            started_sender.send(()).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut buffer = [0_u8; 1024];
            let closed = loop {
                match stream.read(&mut buffer) {
                    Ok(0) => break true,
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        break false;
                    }
                    Err(_) => break true,
                }
            };
            closed_sender.send(closed).unwrap();
        });

        let client = DeepSeekClient::new(
            "test-key".to_string(),
            format!("http://{address}"),
            "example-model".to_string(),
        )
        .unwrap();
        let task = tokio::spawn(async move { client.chat_stream(Vec::new(), None).await });
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match started_receiver.try_recv() {
                Ok(()) => break,
                Err(mpsc::TryRecvError::Empty) if std::time::Instant::now() < deadline => {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("mock stream did not start: {error}"),
            }
        }
        task.abort();
        let _ = task.await;
        assert!(
            closed_receiver
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            "aborting the chat future left the HTTP stream open"
        );
    }
}
