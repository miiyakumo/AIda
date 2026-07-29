# Codex 模型客户端层调研

本文档基于对 [codex-rs](https://github.com/openai/codex) 代码库的深入审查,分析其模型客户端层的架构设计。

**阅读本调研的代码库**: `ref/codex/codex-rs/`, commit `61a44880a85d2fd0d8770908dea5733495e571c8` (2026-07-26).

---

## 1. 发给模型的请求如何组装

### 1.1 整体流程

`ModelClient::stream()` 方法是请求组装的总入口 (`core/src/client.rs:1794`)。调用链如下:

```
ModelClientSession::stream()
  -> ModelClient::build_responses_request()    // 组装 ResponsesApiRequest
  -> stream_responses_api() / stream_responses_websocket()  // 选择传输方式
     -> ResponsesClient::stream_request()      // 序列化为 JSON, POST 到 /responses
        -> spawn_response_stream()             // 解析 SSE 流
           -> map_response_stream()            // 映射为上层 ResponseEvent
```

### 1.2 系统提示词 (system prompt / instructions)

系统提示词在 Codex 中被建模为 `BaseInstructions` 结构:

**`protocol/src/models.rs:1253-1268`**:
```rust
pub const BASE_INSTRUCTIONS_DEFAULT: &str =
    include_str!("prompts/base_instructions/default.md");

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(rename = "base_instructions", rename_all = "snake_case")]
pub struct BaseInstructions {
    pub text: String,
}

impl Default for BaseInstructions {
    fn default() -> Self {
        Self {
            text: BASE_INSTRUCTIONS_DEFAULT.to_string(),
        }
    }
}
```

默认系统提示词内容文件: **`protocol/src/prompts/base_instructions/default.md`** (276 行)。

该文件包含完整的 agent 行为规范,包括:身份与角色定义、AGENTS.md 规范、响应性要求、planning 规则、任务执行与验证、shell 命令使用指南、工具指南等。开头为:

```
You are a coding agent running in the Codex CLI, a terminal-based coding assistant.
Codex CLI is an open source project led by OpenAI. You are expected to be precise, safe, and helpful.
```

**工作方式**: 请求组装时 (`core/src/client.rs:838-925`),`build_responses_request()` 函数将 `prompt.base_instructions.text` 填入 Responses API 的 `instructions` 字段:

```rust
// core/src/client.rs:877
instructions: prompt.base_instructions.text.clone(),  // 正常路径
```

在 `use_responses_lite` 模式下,`instructions` 为空字符串,系统提示词转而塞入 input 数组的第一个元素:

```rust
// core/src/client.rs:862-871
if !prompt.base_instructions.text.is_empty() {
    prefix.push(ResponseItem::Message {
        id: None,
        role: "developer",
        content: vec![ContentItem::InputText {
            text: prompt.base_instructions.text.clone(),
        }],
        ...
    });
}
```

此外,`prompts` crate 还包含多组动态 prompt:

**`prompts/src/lib.rs`** -- 导出的 prompt 常量:
- `SUMMARIZATION_PROMPT` / `SUMMARY_PREFIX` -- 压缩摘要 (`compact.rs`)
- `BACKEND_PROMPT` -- Realtime 后端 prompt,从 `prompts/templates/realtime/backend_prompt.md` (66 行) 加载
- `START_INSTRUCTIONS` / `END_INSTRUCTIONS` -- Realtime 会话开始/结束指令
- `REVIEW_PROMPT` / `APPLY_PATCH_TOOL_INSTRUCTIONS` -- code review 与补丁工具指令
- `budget_limit_prompt()` / `continuation_prompt()` / `objective_updated_prompt()` -- 动态生成的 goal 相关 prompt

### 1.3 工具定义序列化

工具在 `Prompt` 结构中以 `Vec<ToolSpec>` 存在 (`core/src/client_common.rs:24`)。

**`tools/src/tool_spec.rs:19-53`** -- `ToolSpec` 枚举定义了 5 种工具类型:
```rust
pub enum ToolSpec {
    Function(ResponsesApiTool),       // {"type": "function", ...}
    Namespace(ResponsesApiNamespace),  // {"type": "namespace", ...}
    ToolSearch { ... },               // {"type": "tool_search", ...}
    WebSearch { ... },                // {"type": "web_search", ...}
    Freeform(FreeformTool),           // {"type": "custom", ...}
}
```

**`ResponsesApiTool`** (`tools/src/responses_api.rs:26-38`) 包含 `name`, `description`, `strict`, `defer_loading`, `parameters` (JsonSchema), `output_schema`。

序列化为 JSON 的两个入口:
- `create_tools_json_for_responses_api()` (`tools/src/tool_spec.rs:79-90`) -- 返回 `Vec<serde_json::Value>`
- `create_tools_raw_json_for_responses_api()` (`tools/src/tool_spec.rs:93-97`) -- 返回 `Arc<RawValue>` 用于高效复用

请求组装时的选择 (`core/src/client.rs:855-879`):
- 正常路径: `instructions` = system prompt 文本, `tools` = `Arc<RawValue>` (共享引用)
- `use_responses_lite` 路径: `instructions` = 空字符串, 工具以 `AdditionalTools` 的 `ResponseItem` 形式前置到 input 数组

### 1.4 历史消息格式

对话历史定义为 `Vec<ResponseItem>` (`Prompt.input` 字段),序列化后填入 Responses API 的 `input` 字段。

**`protocol/src/models.rs:799-1028`** -- `ResponseItem` 是一种富枚举,包含以下变体:

| 变体 | 说明 |
|------|------|
| `Message { role, content, phase, ... }` | 对话消息 (role: "user"/"assistant"/"developer") |
| `AgentMessage { author, recipient, content }` | 子 agent 间通信消息 |
| `Reasoning { summary, content, encrypted_content }` | 推理链 (reasoning summary/tokens) |
| `FunctionCall { name, arguments, call_id }` | 标准 function call |
| `FunctionCallOutput { call_id, output }` | function call 执行结果 |
| `CustomToolCall { call_id, name, input }` | 自定义工具调用 |
| `CustomToolCallOutput { call_id, output }` | 自定义工具调用结果 |
| `LocalShellCall { call_id, status, action }` | 本地 shell 命令 |
| `ToolSearchCall { ... }` | 工具搜索调用 |
| `ToolSearchOutput { ... }` | 工具搜索结果 |
| `WebSearchCall { ... }` | Web 搜索调用 |
| `ImageGenerationCall { ... }` | 图片生成调用 |
| `Compaction { encrypted_content }` | 上下文压缩摘要 |
| `CompactionTrigger` | 压缩触发信号 |
| `ContextCompaction` | 上下文压缩 |
| `AdditionalTools { role, tools }` | 工具定义的前置声明 |
| `Other` | 未识别的 serde(other) 兜底 |

每条 `Message` 的 `content` 字段是 `Vec<ContentItem>`,支持 `InputText`, `InputImage` (含 `detail`), `OutputText` 等多种内容类型。

`Prompt::get_formatted_input_for_request()` (`core/src/client_common.rs:52-61`) 在发送前做最终处理:在 `use_responses_lite` 模式下调用 `strip_image_details()` 剥离图片 detail 属性。

### 1.5 请求最终结构

`ResponsesApiRequest` (`codex-api/src/common.rs:252-275`):

```rust
pub struct ResponsesApiRequest {
    pub model: String,
    pub instructions: String,           // <-- 系统提示词
    pub input: Vec<ResponseItem>,       // <-- 对话历史
    pub tools: Option<ResponsesApiTools>, // <-- 工具定义 (Arc<RawValue>)
    pub tool_choice: String,            // "auto"
    pub parallel_tool_calls: bool,
    pub reasoning: Option<Reasoning>,
    pub store: bool,
    pub stream: bool,                   // 恒为 true
    pub stream_options: Option<StreamOptions>,
    pub include: Vec<String>,           // ["reasoning.encrypted_content"]
    pub service_tier: Option<String>,
    pub prompt_cache_key: Option<String>,
    pub text: Option<TextControls>,     // verbosity + output_schema
    pub client_metadata: Option<HashMap<String, String>>,
}
```

---

## 2. OpenAI Responses API vs Chat Completions

### 2.1 确凿代码证据:使用 Responses API

**证据 1 -- WireApi 枚举** (`model-provider-info/src/lib.rs:55-70`):

```rust
pub enum WireApi {
    /// The Responses API exposed by OpenAI at `/v1/responses`.
    #[default]
    Responses,
}
```

`"chat"` 被显式拒绝并返回错误信息:
```rust
"chat" => Err(serde::de::Error::custom(CHAT_WIRE_API_REMOVED_ERROR)),
```

其中 `CHAT_WIRE_API_REMOVED_ERROR` (第 50 行) 明确声明:
> "`wire_api = \"chat\"` is no longer supported. How to fix: set `wire_api = \"responses\"` in your provider config."

**证据 2 -- 请求端点** (`core/src/client.rs:160`):
```rust
const RESPONSES_ENDPOINT: &str = "/responses";
```

**证据 3 -- HTTP 客户端** (`codex-api/src/endpoint/responses.rs:100-101`):
```rust
fn path() -> &'static str {
    "responses"
}
```

POST 到该路径时,Accept header 设为 `text/event-stream` (`codex-api/src/endpoint/responses.rs:148-151`)。

**证据 4 -- 请求体** 使用 `ResponsesApiRequest` 而不是 `ChatCompletionRequest`。该结构体包含 `instructions` (非 `messages`) 和 `input` (非 `messages`) 字段,与 OpenAI Responses API 格式完全对应。

**证据 5 -- 流事件类型** 全部以 `response.` 为前缀 (如 `response.created`, `response.output_text.delta`, `response.completed`),而非 Chat Completions 的 `choices.*` 前缀。

### 2.2 同时使用了 WebSocket 传输

Codex 还通过 WebSocket 使用 Responses API (`core/src/client.rs:1522`)。WebSocket 握手时添加 beta header:

```rust
// core/src/client.rs:156-157
const RESPONSES_WEBSOCKETS_V2_BETA_HEADER_VALUE: &str = "responses_websockets=2026-02-06";

// core/src/client.rs:1101-1104
headers.insert(
    OPENAI_BETA_HEADER,
    HeaderValue::from_static(RESPONSES_WEBSOCKETS_V2_BETA_HEADER_VALUE),
);
```

WebSocket 消息格式 (`codex-api/src/common.rs:353-360`):
```rust
pub enum ResponsesWsRequest<'a> {
    #[serde(rename = "response.create")]
    ResponseCreate(ResponseCreateWsRequest<'a>),
}
```

---

## 3. SSE 流式事件解析

### 3.1 解析架构

流入口: `spawn_response_stream()` (`codex-api/src/sse/responses.rs:34-100`)
- 先解析 HTTP response headers (rate limits, server model, x-codex-turn-state 等)
- 然后调用 `process_sse_with_treatment()` 逐行解析 SSE 事件

核心解析: `process_responses_event()` (`codex-api/src/sse/responses.rs:327-473`)

### 3.2 所有可能的流事件类型

解析函数匹配以下事件类型 (`kind` 字段):

| SSE 事件类型 (`type` 字段) | 映射到 `ResponseEvent` 变体 | 说明 |
|---|---|---|
| `response.created` | `ResponseEvent::Created` | 响应已创建 |
| `response.output_item.added` | `ResponseEvent::OutputItemAdded(item)` | 新输出项添加 (如开始生成消息) |
| `response.output_item.done` | `ResponseEvent::OutputItemDone(item)` | 输出项完成 (含完整 `ResponseItem`) |
| `response.output_text.delta` | `ResponseEvent::OutputTextDelta(delta)` | 文本增量 |
| `response.custom_tool_call_input.delta` | `ResponseEvent::ToolCallInputDelta { item_id, call_id, delta }` | 自定义工具调用参数增量 |
| `response.function_call_arguments.delta` | `ResponseEvent::ToolCallInputDelta { ... }` | 函数调用参数增量 (测试中可见,第 935 行) |
| `response.reasoning_summary_text.delta` | `ResponseEvent::ReasoningSummaryDelta { delta, summary_index }` | 推理摘要文本增量 |
| `response.reasoning_summary_text.done` | `ResponseEvent::ReasoningSummaryDone { item_id, text, summary_index }` | 推理摘要文本完成 |
| `response.reasoning_text.delta` | `ResponseEvent::ReasoningContentDelta { delta, content_index }` | 推理内容增量 |
| `response.reasoning_summary_part.added` | `ResponseEvent::ReasoningSummaryPartAdded { summary_index }` | 推理摘要新部分开始 |
| `response.completed` | `ResponseEvent::Completed { response_id, token_usage, end_turn }` | 响应完成 (终端事件) |
| `response.failed` | **Error** (`ResponsesEventError::Api(...)`) | 响应失败,根据 error code 细分 |
| `response.incomplete` | **Error** (`ApiError::Stream(...)`) | 响应不完整 |
| `response.metadata` | 三种子事件 (见下) | 服务端元数据 |
| 未知类型 | `trace!` 日志,静默跳过 | 向前兼容 |

**`response.failed` 的错误细分** (`codex-api/src/sse/responses.rs:390-416`):

| Error Code | 映射到的 `ApiError` | 是否可重试 |
|---|---|---|
| `context_length_exceeded` | `ApiError::ContextWindowExceeded` | 否 |
| `insufficient_quota` | `ApiError::QuotaExceeded` | 否 |
| `usage_not_included` | `ApiError::UsageNotIncluded` | 否 |
| `cyber_policy` | `ApiError::CyberPolicy { message }` | 否 |
| `invalid_prompt` / `bio_policy` | `ApiError::InvalidRequest { message }` | 否 |
| `server_is_overloaded` / `slow_down` | `ApiError::ServerOverloaded` | 可重试 |
| `rate_limit_exceeded` | `ApiError::Retryable { message, delay }` | 可重试 |
| 其他 | `ApiError::Retryable { message, delay }` | 可重试 |

**`response.metadata` 的子事件** (`codex-api/src/sse/responses.rs:213-234`):
- `ModelVerifications` -- 服务端推荐的额外账户验证 (`openai_verification_recommendation`)
- `TurnModerationMetadata` -- 审核元数据 (`openai_chatgpt_moderation_metadata`)
- `turn_state` -- `x-codex-turn-state` 粘性路由令牌

**HTTP header 转义的事件** (在 SSE 处理前从 response headers 提取,`spawn_response_stream` 中):
- `ResponseEvent::ServerModel(model)` -- 实际服务的模型 (可能与请求不同)
- `ResponseEvent::RateLimits(snapshot)` -- 速率限制快照
- `ResponseEvent::ModelsEtag(etag)` -- 模型目录 ETag
- `ResponseEvent::ServerReasoningIncluded(bool)` -- 服务端是否已计入推理 token
- `ResponseEvent::SafetyBuffering(..)` -- 安全缓冲通知 (在 SSE 事件中同等提取)

**顶层 `ResponseStream`** (`core/src/client_common.rs:104-117`) 封装了一个 `mpsc::Receiver`,将底层的 `codex_api::ResponseStream` 映射为上层消费者可用的流。

---

## 4. 重试与限流处理

### 4.1 重试配置

定义在 `model-provider-info/src/lib.rs:26-33`:

```rust
const DEFAULT_STREAM_IDLE_TIMEOUT_MS: u64 = 300_000;   // 5 分钟
const DEFAULT_STREAM_MAX_RETRIES: u64 = 5;
const DEFAULT_REQUEST_MAX_RETRIES: u64 = 4;
const MAX_STREAM_MAX_RETRIES: u64 = 100;                // 硬上限
const MAX_REQUEST_MAX_RETRIES: u64 = 100;
```

每次请求的 `RetryConfig` 构造 (`model-provider-info/src/lib.rs:265-271`):
```rust
let retry = ApiRetryConfig {
    max_attempts: self.request_max_retries(),  // 默认 4
    base_delay: Duration::from_millis(200),
    retry_429: false,    // 不自动重试 429
    retry_5xx: true,     // 自动重试 5xx
    retry_transport: true, // 自动重试传输层错误
};
```

- `request_max_retries` 控制非流式请求 (compact, memories 等) 的重试次数
- `stream_max_retries` 控制流式请求的重连次数
- `stream_idle_timeout` 控制 SSE 流中两个事件之间的最大空闲时间
- `websocket_connect_timeout_ms` 控制 WebSocket 握手超时 (默认 15 秒, `model-provider-info/src/lib.rs:29`)

### 4.2 HTTP 与 WebSocket 双传输 + 回退机制

`ModelClientSession::stream()` (`core/src/client.rs:1794-1844`):

1. 优先尝试 WebSocket 传输 (`stream_responses_websocket()`)
2. WebSocket 成功后直接返回流
3. WebSocket 返回 `FallbackToHttp` 时调用 `try_switch_fallback_transport()` 永久切换到 HTTP:

```rust
// core/src/client.rs:1853-1863
pub(crate) fn try_switch_fallback_transport(...) -> bool {
    let activated = self.client.force_http_fallback(session_telemetry, model_info);
    self.websocket_session = WebsocketSession::default();
    activated
}
```

`force_http_fallback()` (`core/src/client.rs:522-541`) 设置 `disable_websockets = true` (session 级别,`AtomicBool`),后续所有 turn 都走 HTTP。

### 4.3 Unauthorized (401) 重试

HTTP 和 WebSocket 路径都有独立的 401 重试循环:

- `stream_responses_api()` (`core/src/client.rs:1395-1504`): 遇到 401 时调用 `handle_unauthorized()`,尝试刷新 ChatGPT token,成功后 `continue` 重试
- `stream_responses_websocket()` (`core/src/client.rs:1522-1603`): 同样逻辑

`PendingUnauthorizedRetry` (`core/src/client.rs:2100-2115`) 跟踪重试状态,每次只允许一次 401 恢复重试。

### 4.4 速率限制解析

**HTTP Response Header 解析** (`codex-api/src/rate_limits.rs:22-51`):

解析 `x-codex-*-used-percent`, `x-codex-*-window-minutes`, `x-codex-*-reset-at` 等自定义 header,支持多 limit family (通过 `header_name_to_limit_id()` 自动发现所有 `x-{limit_id}-primary-used-percent` 模式的 header)。

**SSE 事件流中的速率限制** (`codex-api/src/rate_limits.rs:134-167`):

解析 `codex.rate_limits` 事件 (JSON payload)。

**错误消息中的 retry-after** (`codex-api/src/sse/responses.rs:599-623`):

从 `rate_limit_exceeded` 错误的 message 中正则提取 `"try again in Xs"` 或 `"try again in Xms"`。

---

## 5. Provider 抽象

### 5.1 `ModelProvider` Trait

定义在 `model-provider/src/provider.rs:101-216`:

```rust
pub trait ModelProvider: fmt::Debug + Send + Sync {
    fn info(&self) -> &ModelProviderInfo;
    fn capabilities(&self) -> ProviderCapabilities { ... }
    fn approval_review_preferred_model(&self) -> &'static str { ... }
    fn memory_extraction_preferred_model(&self) -> &'static str { ... }
    fn memory_consolidation_preferred_model(&self) -> &'static str { ... }
    fn supports_attestation(&self) -> bool { false }
    fn auth_manager(&self) -> Option<Arc<AuthManager>>;
    fn auth(&self) -> ModelProviderFuture<'_, Option<CodexAuth>>;
    fn account_state(&self) -> ProviderAccountResult;
    fn map_api_error(&self, error: ApiError) -> CodexErr { ... }
    fn api_provider(&self) -> ModelProviderFuture<'_, Result<Provider>> { ... }
    fn runtime_base_url(&self) -> ModelProviderFuture<'_, Result<Option<String>>> { ... }
    fn api_auth(&self) -> ModelProviderFuture<'_, Result<SharedAuthProvider>> { ... }
    fn api_auth_for_scope(&self, scope: ProviderAuthScope) -> ... { ... }
    fn models_manager(&self, codex_home: PathBuf, ...) -> SharedModelsManager;
    fn models_manager_without_cache(&self, ...) -> SharedModelsManager;
}
```

`ProviderCapabilities` (`model-provider/src/provider.rs:34-38`):
```rust
pub struct ProviderCapabilities {
    pub namespace_tools: bool,
    pub image_generation: bool,
    pub web_search: bool,
}
```

### 5.2 工厂函数

`create_model_provider()` (`model-provider/src/provider.rs:232-241`):

```rust
pub fn create_model_provider(
    provider_info: ModelProviderInfo,
    auth_manager: Option<Arc<AuthManager>>,
) -> SharedModelProvider {
    if provider_info.is_amazon_bedrock() {
        Arc::new(AmazonBedrockModelProvider::new(provider_info, auth_manager))
    } else {
        Arc::new(ConfiguredModelProvider::new(provider_info, auth_manager))
    }
}
```

两个实现:
- **`ConfiguredModelProvider`** -- 标准实现,通过 `ModelProviderInfo` 配置驱动,适用于 OpenAI、自定义 OSS provider 等
- **`AmazonBedrockModelProvider`** -- AWS Bedrock 特化实现

### 5.3 `ModelProviderInfo` -- 配置驱动的 Provider 元数据

定义在 `model-provider-info/src/lib.rs:89-144`:

关键字段:
- `name`, `base_url`, `wire_api` (仅 `Responses`)
- `env_key` / `experimental_bearer_token` / `auth` -- 多种认证方式
- `aws` -- AWS SigV4 认证 (Bedrock 专用)
- `request_max_retries`, `stream_max_retries`, `stream_idle_timeout_ms`, `websocket_connect_timeout_ms` -- 重试/超时
- `requires_openai_auth` -- 是否走 OpenAI 登录流程
- `supports_websockets` -- 是否支持 WebSocket 传输
- `supports_standalone_web_search` -- 是否支持独立 web search

`to_api_provider()` (`model-provider-info/src/lib.rs:244-281`) 将配置转换为 `codex-api::Provider`:
- 默认 base URL: ChatGPT 登录使用 `https://chatgpt.com/backend-api/codex`,API Key 使用 `https://api.openai.com/v1`
- 合并 `http_headers` 和 `env_http_headers`
- 构建 `RetryConfig`

### 5.4 Ollama / LM Studio -- 辅助客户端而非 Provider 实现

**关键发现**: Ollama 和 LM Studio 都 **不是** `ModelProvider` trait 的实现。它们是独立的辅助客户端,用于模型管理 (拉取、探测、版本检查)。

**Ollama** (`ollama/src/client.rs:25-29`):
```rust
pub struct OllamaClient {
    client: reqwest::Client,
    host_root: String,
    uses_openai_compat: bool,
}
```

功能:
- `probe_server()` -- 探测 Ollama 是否运行 (`GET /api/tags` 或 `GET /v1/models`)
- `fetch_models()` -- 获取已安装模型列表
- `fetch_version()` -- 获取 Ollama 版本 (要求 >= 0.13.4 以支持 Responses API)
- `pull_model_stream()` -- 拉取模型 (POST `/api/pull`, NDJSON 流)
- `pull_with_reporter()` -- 带进度报告的拉取

**LM Studio** (`lmstudio/src/client.rs:10-13`):
```rust
pub struct LMStudioClient {
    client: RouteAwareClientPool,
    base_url: String,
}
```

功能:
- `check_server()` -- 探测 LM Studio 是否运行 (`GET /models`)
- `fetch_models()` -- 获取模型列表
- `load_model()` -- 通过发送一个 `max_output_tokens: 1` 的空请求到 `/responses` 来预热模型
- `download_model()` -- 通过 `lms get` CLI 命令下载模型

### 5.5 Adapter 模式的本质

Codex 的 provider 架构不是传统的 "每个 provider 实现一个 trait" 模式,而是 **统一 Responses API 协议**:

```
                    ModelProvider trait
                    /                  \
        ConfiguredModelProvider    AmazonBedrockModelProvider
                    |                       |
            ModelProviderInfo          AWS SigV4 signing
            (配置驱动)                   (region-aware)

            POST /v1/responses (OpenAI Responses API 格式)
                    |
        ┌──────────┼──────────┐
        |          |          |
    api.openai.com  localhost:11434  localhost:1234
    (OpenAI)       (Ollama)         (LM Studio)
```

第三方 provider (Ollama, LM Studio) 通过 `ModelProviderInfo` 配置 `base_url` 指向本地地址,使用 **完全相同的 Responses API 线协议**。Ollama/LM Studio 各自实现了对 Responses API 的服务端兼容,因此 Codex 客户端无需区分 provider--它们都是 `ConfiguredModelProvider`。

在 `built_in_model_providers()` (`model-provider-info/src/lib.rs:438-464`) 中可以看到:

```rust
(OLLAMA_OSS_PROVIDER_ID, create_oss_provider(DEFAULT_OLLAMA_PORT, WireApi::Responses)),
(LMSTUDIO_OSS_PROVIDER_ID, create_oss_provider(DEFAULT_LMSTUDIO_PORT, WireApi::Responses)),
```

两者都是通过 `create_oss_provider()` 创建,使用 `WireApi::Responses`,没有特化的 provider 实现。

---

## 6. OpenAI Responses API vs Anthropic Messages API 概念对照表

### 6.1 基础概念

| 概念 | OpenAI Responses API | Anthropic Messages API |
|------|---------------------|----------------------|
| **端点** | `POST /v1/responses` | `POST /v1/messages` |
| **基础 URL** | `https://api.openai.com/v1` | `https://api.anthropic.com/v1` |
| **HTTP Method** | POST | POST |
| **Stream Accept** | `text/event-stream` | `text/event-stream` (SSE) |
| **请求 ID header** | `x-request-id` | `request_id` (在 SSE 事件中) |
| **Codex 中的使用** | 唯一支持的协议 | 不支持 |

### 6.2 System Prompt / Instructions 注入方式

| 维度 | OpenAI Responses API | Anthropic Messages API |
|------|---------------------|----------------------|
| **参数名** | `instructions` (字符串,顶层字段) | `system` (字符串或 `TextBlockParam[]` 数组,顶层字段) |
| **位置** | 请求体顶层 | 请求体顶层,与 `messages` 并列 |
| **类型** | `String` | `string \| TextBlockParam[]` |
| **缓存支持** | 通过 `prompt_cache_key` (Codex 自定义) | 每个 `TextBlockParam` 可带 `cache_control` |
| **示例** | `"instructions": "You are a coding agent..."` | `"system": "You are a helpful assistant."` |
| **多段 system** | 不支持(单段字符串) | 支持(数组形式) |
| **Codex 映射** | `BaseInstructions.text` -> `instructions` | N/A |

### 6.3 Tool Use / Function Calling 对比

| 维度 | OpenAI Responses API | Anthropic Messages API |
|------|---------------------|----------------------|
| **参数名** | `tools` (数组) | `tools` (数组) |
| **工具类型标签** | `"type": "function"` | `"type": "function"` (自定义工具) / `"type": "web_search_20250305"` 等 |
| **Schema 字段** | `parameters` (JSON Schema) | `input_schema` (JSON Schema) |
| **输出 Schema** | `output_schema` (额外字段) | 不支持 |
| **strict 模式** | `"strict": true` 可选 | `"strict": true` 可选 |
| **延迟加载** | `"defer_loading": true` | `"defer_loading": true` |
| **命名空间** | 支持 `"type": "namespace"` (Codex 扩展) | 不支持 |
| **工具选择** | `tool_choice: "auto" \| "required" \| "none"` | `tool_choice: { type: "auto" } \| { type: "any" } \| { type: "tool", name: "..." }` |
| **并行调用** | `parallel_tool_calls: bool` | `disable_parallel_tool_use: bool` (语义相反) |
| **Codex 中的工具类型** | `function`, `namespace`, `tool_search`, `web_search`, `custom` | N/A |

### 6.4 流式事件类型对照

| OpenAI Responses API (Codex 解析的) | Anthropic Messages API | 说明 |
|---|---|---|
| `response.created` | `message_start` | 响应/消息开始 |
| `response.output_item.added` | `content_block_start` | 内容块开始 |
| `response.output_text.delta` | `content_block_delta` (delta.type=`text_delta`) | 文本增量 |
| `response.output_item.done` | `content_block_stop` + (最终消息) | 内容块完成 |
| `response.custom_tool_call_input.delta` | `content_block_delta` (delta.type=`input_json_delta`) | 工具参数增量 |
| `response.function_call_arguments.delta` | `content_block_delta` (delta.type=`input_json_delta`) | 函数调用参数增量 |
| `response.reasoning_text.delta` | `content_block_delta` (delta.type=`thinking_delta`) | 推理/思考内容增量 |
| `response.reasoning_summary_text.delta` | (无直接对应) | 推理摘要增量 (Codex 特有) |
| N/A | `content_block_delta` (delta.type=`signature_delta`) | 思考签名 (Anthropic 特有) |
| `response.completed` | `message_delta` + `message_stop` | 完成 (OpenAI 一次事件完成) |
| `response.metadata` | (无直接对应) | 元数据 |
| `response.failed` | `error` | 错误 |
| 无 | `ping` | 心跳 |
| HTTP header 中传递 | SSE data 中传递 | usage 信息位置不同 |

**关键差异**:
- Anthropic 用 `message_delta` + `message_stop` 两个事件完成,OpenAI 仅用 `response.completed` 一个事件
- Anthropic `content_block_delta` 通过 `delta.type` 区分 text/input_json/thinking/signature,OpenAI 用不同的事件类型名 (`response.output_text.delta` vs `response.reasoning_text.delta`)
- OpenAI 的 usage/stop_reason 在 `response.completed` 中一次性给出,Anthropic 的 usage 在 `message_delta` 和 `message_stop` 中累积更新
- Anthropic 有 `ping` 心跳,OpenAI 无
- Anthropic 有 `signature_delta` (用于验证 thinking 完整性),OpenAI 无对应物

### 6.5 消息/历史格式对比

| 维度 | OpenAI Responses API | Anthropic Messages API |
|------|---------------------|----------------------|
| **参数名** | `input` (数组) | `messages` (数组) |
| **角色** | `"user"`, `"assistant"`, `"developer"`, `"system"` (在 input 中) | `"user"`, `"assistant"` (无 developer, system 不在 messages 中) |
| **消息结构** | `{ "type": "message", "role": "...", "content": [...] }` | `{ "role": "...", "content": [...] }` |
| **工具调用** | `{ "type": "function_call", "call_id": "...", "name": "...", "arguments": "..." }` | `{ "type": "tool_use", "id": "...", "name": "...", "input": {...} }` |
| **工具结果** | `{ "type": "function_call_output", "call_id": "...", "output": "..." }` | `{ "type": "tool_result", "tool_use_id": "...", "content": "..." }` |
| **content 类型** | `input_text`, `input_image`, `output_text` 等 | `text`, `image`, `tool_use`, `tool_result` 等 |
| **图片支持** | Base64 data URL, 带 `detail` 属性 | Base64, 支持 `cache_control` |
| **多轮历史** | 所有轮次混在 `input` 的一维数组中 | 通过交替 user/assistant `messages` 区分轮次 |

### 6.6 其他差异

| 维度 | OpenAI Responses API | Anthropic Messages API |
|------|---------------------|----------------------|
| **max_tokens** | `max_output_tokens` | `max_tokens` |
| **temperature** | `temperature` (请求体顶层) | `temperature` |
| **store** | `store: bool` (服务端存储) | N/A |
| **service_tier** | `service_tier` (如 "default", "flex") | N/A |
| **推理配置** | `reasoning: { effort, summary, context }` | `thinking: { type: "adaptive" \| "enabled", budget_tokens, display }` |
| **文本格式控制** | `text: { verbosity, format: { type, schema } }` | 不支持 |
| **prompt 缓存** | `prompt_cache_key` (Codex 通过此字段启用缓存) | `cache_control: { type: "ephemeral" }` (标记在 content block 上) |
| **WebSocket** | 支持 `responses_websockets=2026-02-06` | 不支持 |
| **API 版本** | 无版本 header (通过 beta header 控制) | `anthropic-version: 2023-06-01` |

---

## 7. 文件索引

### Core 层

| 文件 (绝对路径) | 主要内容 | 行数 |
|---|---|---|
| `ref/codex/codex-rs/core/src/client.rs` | `ModelClient` / `ModelClientSession`, 请求组装, 双传输, 重试逻辑 | ~2440 |
| `ref/codex/codex-rs/core/src/client_common.rs` | `Prompt` 结构, `ResponseStream` 封装 | ~128 |

### API 层

| 文件 | 主要内容 | 行数 |
|---|---|---|
| `ref/codex/codex-rs/codex-api/src/common.rs` | `ResponsesApiRequest`, `ResponseEvent`, `ResponseCreateWsRequest` 等核心类型 | ~394 |
| `ref/codex/codex-rs/codex-api/src/sse/responses.rs` | SSE 解析, `process_responses_event()`, 所有流事件类型 | ~1662 |
| `ref/codex/codex-rs/codex-api/src/sse/mod.rs` | SSE 模块入口 | ~6 |
| `ref/codex/codex-rs/codex-api/src/endpoint/responses.rs` | HTTP Responses API 客户端 | ~165 |
| `ref/codex/codex-rs/codex-api/src/endpoint/responses_websocket.rs` | WebSocket Responses API 客户端 | ~150+ |
| `ref/codex/codex-rs/codex-api/src/endpoint/session.rs` | 底层 HTTP 传输 + 重试策略应用 | ~157 |
| `ref/codex/codex-rs/codex-api/src/error.rs` | `ApiError` 枚举定义 | ~42 |
| `ref/codex/codex-rs/codex-api/src/rate_limits.rs` | 速率限制 header 和事件解析 | ~381 |
| `ref/codex/codex-rs/codex-api/src/auth.rs` | `AuthProvider` trait | ~93 |

### Provider 层

| 文件 | 主要内容 | 行数 |
|---|---|---|
| `ref/codex/codex-rs/model-provider/src/provider.rs` | `ModelProvider` trait 定义, `create_model_provider()`, `ConfiguredModelProvider` | ~840 |
| `ref/codex/codex-rs/model-provider-info/src/lib.rs` | `ModelProviderInfo`, `WireApi`, 默认 provider 注册, 重试配置常量 | ~555 |
| `ref/codex/codex-rs/model-provider/src/auth.rs` | Provider 认证解析 | - |

### Tools 层

| 文件 | 主要内容 | 行数 |
|---|---|---|
| `ref/codex/codex-rs/tools/src/tool_spec.rs` | `ToolSpec` 枚举, `create_tools_json_for_responses_api()` | ~142 |
| `ref/codex/codex-rs/tools/src/responses_api.rs` | `ResponsesApiTool`, `LoadableToolSpec` | ~141 |

### Prompts 层

| 文件 | 主要内容 | 行数 |
|---|---|---|
| `ref/codex/codex-rs/protocol/src/prompts/base_instructions/default.md` | 默认系统提示词 (BASE_INSTRUCTIONS_DEFAULT) | 276 |
| `ref/codex/codex-rs/prompts/src/realtime.rs` | Realtime 会话 prompt 引用 | ~3 |
| `ref/codex/codex-rs/prompts/templates/realtime/backend_prompt.md` | Realtime 后端 agent prompt | 66 |
| `ref/codex/codex-rs/prompts/templates/realtime/realtime_start.md` | Realtime 开始指令 | 10 |
| `ref/codex/codex-rs/prompts/templates/realtime/realtime_end.md` | Realtime 结束指令 | 3 |

### 第三方 Provider 适配

| 文件 | 主要内容 | 行数 |
|---|---|---|
| `ref/codex/codex-rs/ollama/src/lib.rs` | `ensure_oss_ready()`, `ensure_responses_supported()` | ~98 |
| `ref/codex/codex-rs/ollama/src/client.rs` | `OllamaClient` -- 模型管理 (pull, probe, version) | ~459 |
| `ref/codex/codex-rs/lmstudio/src/lib.rs` | `ensure_oss_ready()` | ~47 |
| `ref/codex/codex-rs/lmstudio/src/client.rs` | `LMStudioClient` -- 模型管理 (load, download, probe) | ~425 |

### Protocol 层

| 文件 | 主要内容 | 行数 |
|---|---|---|
| `ref/codex/codex-rs/protocol/src/models.rs:799-1028` | `ResponseItem` 枚举定义 | ~230 (of ~1200+) |
| `ref/codex/codex-rs/protocol/src/models.rs:1253-1268` | `BaseInstructions` 结构定义 | ~16 |
| `ref/codex/codex-rs/protocol/src/openai_models.rs:389` | `ModelInfo.base_instructions` 字段 | - |
