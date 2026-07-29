# Alda Agent Harness 实施路线文档

> 面向自学场景：读者是 Rust 初学者（但会编程），独自一人，没有导师。
> 每个步骤用自然语言描述"你要做什么，预期看到什么，如果不对怎么办"。
> 本文是实施计划，不是教程——不写乐理内容。用语精确，文件路径准确。
>
> **状态与范围**：本文描述待创建的目标工程；当前仓库没有 `alda-agent/` 实现。本文只覆盖 M0–M5 单 Agent MVP。完成全部验收并冻结基线后，再进入[M6–M12 进阶实施路线](advanced-implementation-roadmap.md)；不要提前拼装 MCP、长期记忆或 Agent Teams。
>
> **评测版本**：本文的 H0–H3 属于 `alda-eval/legacy-v1`，其中 H2=LLM Judge、H3=人工验收。进阶 `alda-eval/v2` 会将它们映射到 V2 H4/H5；报告必须携带 schema version。

## 目录

1. [M0 前检查清单](#m0-前检查清单)
2. [M1：最小 agent loop（5 天）](#m1最小-agent-loop5-天)
3. [M2：REPL 驱动 + 流式 + 第二 provider（5 天）](#m2repl-驱动--流式--第二-provider5-天)
4. [M3：提示词工程 + 持久化（5 天）](#m3提示词工程--持久化5-天)
5. [M4：Evaluation harness（5 天）](#m4evaluation-harness5-天)
6. [M5：上下文压缩 + 工程收尾（4 天）](#m5上下文压缩--工程收尾4-天)
7. [风险点与缓解策略汇总](#风险点与缓解策略汇总)
8. [学时预估表](#学时预估表)
9. [附录：环境依赖检查脚本](#附录环境依赖检查脚本)
10. [完成 M5 之后](#完成-m5-之后)

---

## M0 前检查清单

**目标**：确认 alda 环境可用，搭好 Rust 脚手架。在开始写任何 harness 代码之前，确保所有外部依赖就绪。

**时间**：1-2 天（取决于环境问题排查时间）

### 步骤 0.1：检查 Java / JVM

alda-player 是 JVM 程序（Kotlin/Java），必须有 JRE 才能运行。依据：`alda-interfaces.md` §6.1。

```bash
java -version
```

**预期看到**：Java 8 或更高版本。仓库内 Alda 2.4.3 player 的编译目标和运行要求均为 Java 8+；Java 17/21 也可使用。

**如果没有**：安装 OpenJDK 8 或更高版本（优先选当前仍受支持的 LTS 版本，如 17 或 21）。
- Arch: `sudo pacman -S jdk-openjdk`
- Ubuntu: `sudo apt install openjdk-17-jdk`
- macOS: `brew install openjdk@17`

**验收**：`java -version` 返回 8 或更高版本号，exit code 为 0。

### 步骤 0.2：安装 alda

```bash
# 确认 alda 在 PATH 中
which alda
alda version
```

**预期看到**：`alda 2.x.x` 的版本号。

**如果没有**：按照 https://alda.io/install 的指引安装。

**验收**：`alda version` exit code 为 0。

### 步骤 0.3：运行 alda doctor

```bash
alda doctor
```

**预期看到**：每一步以 `OK` 开头（绿色，无方括号）。如果音频设备不可用，使用 `alda doctor --no-audio` 跳过音频相关检查。

依据：`alda-interfaces.md` §2.8，doctor 执行 22 个检查步骤。对于无桌面环境（如 WSL、Docker、CI），`--no-audio` 跳过第 10 步（Play score）和第 11 步（Export score as MIDI 的验证部分）。

**可能失败的点**：
- 第 6 步 `Locate alda-player`：表示 `alda-player` 二进制不在 PATH 上。`alda doctor` 会自动提示下载安装——同意即可。
- 第 8 步 `Spawn a player process`：如果 `state` 卡在 `starting` 而非 `ready`，通常是 MIDI 合成器 / SoundFont 问题。在无头环境用 `--no-audio`。

**验收**：`alda doctor` 所有已执行步骤输出 `OK`（或 `alda doctor --no-audio` 的所有已执行步骤均输出 `OK`）。

### 步骤 0.4：验证 alda parse 的 JSON 输出

```bash
echo 'piano: c d e f g a b > c' > /tmp/test.alda
alda parse -f /tmp/test.alda -o data
```

**预期看到**：stdout 输出一个 JSON 对象，包含 `"events"` 字段、`"parts"` 等。

```bash
alda parse -f /tmp/test.alda -o events
```

**预期看到**：stdout 输出 JSON 数组，每个元素是一个事件对象。

**验收**：两种输出格式都是合法 JSON（可以通过 `| python3 -m json.tool` 验证）。

### 步骤 0.5：验证 alda play 播放链

```bash
alda play -c 'piano: c d e f g a b > c'
```

**预期看到**：stderr 打印 `Playing...`，exit code 为 0。在无头环境可以跳过此步（play 需要音频输出设备）。

**注意**：这一步确认播放链路（alda -> alda-player -> JVM MIDI 合成器 -> 音频设备）完整可用。如果失败但 `alda doctor --no-audio` 通过，仍然可以继续开发——`play_for_human` 工具最终需要人耳验证，但开发阶段可以通过 `alda parse` 和 MIDI export 验证。

### 步骤 0.6：检查 Rust 工具链

```bash
rustc --version
cargo --version
```

**预期看到**：rustc 版本 >= 1.85.0（Rust 2024 edition 的最低稳定版本）。

**如果没有**：`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`

### 步骤 0.7：创建 Rust 项目

```bash
# 在本仓库根目录运行
mkdir -p alda-agent
cd alda-agent
cargo init
```

这将生成 `Cargo.toml` 和 `src/main.rs`。

### 步骤 0.8：添加 Cargo.toml 依赖

将 `Cargo.toml` 的内容替换为以下内容（精确版本号，依据设计文档附录 A）：

```toml
[package]
name = "alda-agent"
version = "0.1.0"
edition = "2024"
rust-version = "1.85"

[lints.rust]
unsafe_code = "warn"

[lints.clippy]
all = "warn"
pedantic = "warn"

[dependencies]
tokio = { version = "1", features = ["full"] }
reqwest = { version = "0.12", features = ["json", "stream"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
clap = { version = "4", features = ["derive"] }
uuid = { version = "1", features = ["v4"] }
chrono = "0.4"
tokio-util = "0.7"
async-trait = "0.1"
thiserror = "2"
dirs = "5"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
anyhow = "1"
```

运行 `cargo check` 验证依赖能正常解析和下载。

**预期看到**：`cargo check` exit code 为 0，无编译错误。

**如果失败**：检查网络连接（crates.io 可能需要代理）。如果是版本问题，`cargo update` 尝试。

### 步骤 0.9：写最小可编译骨架

创建以下文件，确保项目可以编译通过：

**`src/main.rs`**：
```rust
mod config;
mod error;

use config::AldaConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = AldaConfig::from_env()?;
    tracing::info!("Alda Agent v{}", env!("CARGO_PKG_VERSION"));
    tracing::info!("Provider: {:?}", config.provider);
    tracing::info!("Model: {}", config.model);
    tracing::info!("Alda binary: {:?}", config.alda_binary_path);
    tracing::info!("Sessions dir: {:?}", config.sessions_dir);

    Ok(())
}
```

**`src/config.rs`**：抄设计文档 §8 的 `AldaConfig` + `from_env()` 实现（约 60 行）。

**`src/error.rs`**：抄设计文档 §9 的 `AgentError` 和 `ProviderError` 枚举定义（约 40 行）。

运行 `cargo build`，确认编译通过。

### 步骤 0.10：创建目录结构

根据设计文档 §10.1 的 M1-M2 单 crate 目录结构：

```bash
mkdir -p src/provider
mkdir -p src/tools
mkdir -p src/prompts
mkdir -p tests/fixtures
```

确认最终目录结构如下：

```text
alda-agent/
├── Cargo.toml
├── src/
│   ├── main.rs
│   ├── config.rs
│   ├── error.rs
│   ├── types.rs          # M1 Day1 创建
│   ├── agent.rs           # M1 Day3 创建
│   ├── session.rs         # M1 Day4 创建
│   ├── prompt.rs          # M1 Day4 创建
│   ├── provider/
│   │   └── mod.rs         # M1 Day2 创建
│   │   └── anthropic.rs   # M1 Day2 创建
│   │   └── openai.rs      # M2 Day1 创建
│   ├── tools/
│   │   └── mod.rs         # M1 Day3 创建
│   │   └── write_score.rs # M1 Day3 创建
│   │   └── alda_parse.rs  # M1 Day4 创建
│   │   └── score_analyze.rs # M3 Day2 创建
│   │   └── play_for_human.rs # M2 Day2 创建
│   ├── metrics.rs         # M3 Day1 创建
│   └── prompts/
│       ├── base_instructions.md  # M1 Day4 创建
│       └── alda_cheatsheet.md    # M3 Day1 创建
├── tests/
│   ├── fixtures/
│   └── integration.rs
└── docs/
```

**M0 完成验收**：`cargo build` 和 `cargo run` 均成功。`cargo run` 打印版本、配置信息后退出。

---

## M1：最小 agent loop（5 天）

**目标**：单 provider（Anthropic）+ 两个工具（write_score, alda_parse）的完整闭环。
LLM 能完成"写一段 4 小节 C 大调旋律" -> parse 失败 -> 自动修正 -> 通过的全自动流程。

**设计依据**：设计文档 §11 里程碑 M1。

---

### M1 Day 1：核心类型定义 + Anthropic SSE 解析准备

**时间**：4-6 小时

#### 任务 1.1：实现 `src/types.rs`（约 100 行）

**做什么**：对照设计文档 §2，把 `Message`、`ContentBlock`、`ChatRequest`、`ToolSpec`、`StreamEvent`、`StopReason` 的定义写入 `src/types.rs`。这是整个 harness 的"词汇表"，后续所有模块都引用它。

**关键代码片段**：
- `Message` 枚举（§2.1）：四个变体 `User`、`Assistant`、`Tool`、`System`，每个变体含 `Vec<ContentBlock>`
- `ContentBlock` 枚举（§2.1）：`Text`、`ToolCall`、`ToolResult`
- `ChatRequest` 结构体（§2.2）：`system_prompt`、`messages`、`tools`、`model`、`max_tokens`、`temperature`
- `ToolSpec` 结构体（§2.2）：`name`、`description`、`input_schema: serde_json::Value`
- `StreamEvent` 枚举（§6.2 最终版）：只有 `TextDelta`、`ToolCallDone`、`ThinkingDelta`、`UsageInput`、`UsageOutput`、`Done` 六个变体——**注意**：M1 不公开 `ToolCallStart` 和 `ToolCallDelta`，它们由 provider adapter 内部消化。这是 ADR-8 的设计决策。
- `StopReason` 枚举（§2.3）：`EndTurn`、`MaxTokens`、`ToolUse`、`Error(String)`

**验收**：`cargo check --lib` 通过。`types.rs` 中所有类型都有 `#[derive(Debug, Clone, Serialize, Deserialize)]`。

#### 任务 1.2：搭建 `src/provider/mod.rs`（约 40 行）

**做什么**：对照设计文档 §4.1，定义 `Provider` trait。

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    async fn stream(&self, request: ChatRequest) -> Result<EventStream>;
    async fn complete(&self, request: ChatRequest) -> Result<String>; // 默认实现
    fn context_window(&self) -> usize;
    fn name(&self) -> &str;
}

pub type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>;
```

**验收**：`cargo check --lib` 通过。理解 `Pin<Box<dyn Stream>>` 的语法——这是 Rust 异步 trait 的标准写法。

#### 任务 1.3：开始写 `src/provider/anthropic.rs`（骨架，约 80 行）

**做什么**：对照设计文档 §4.2，创建 `AnthropicProvider` 结构体。

```rust
pub struct AnthropicProvider {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
    model: String,
}
```

实现 `AnthropicProvider::new()`（从环境变量读 `ANTHROPIC_API_KEY`）和 `to_anthropic_request()` 方法（§4.2 的请求构造逻辑）。

**关键点**：
- `to_anthropic_request()` 把内部 `ChatRequest` 转成 Anthropic Messages API 的 JSON。参考 §4.2 的匹配逻辑——`Message::Tool` 映射到 `{"role": "user", "content": [{"type": "tool_result", ...}]}`。
- 先不实现 SSE 解析和 `stream()` 方法——Day 2 专门做。

**验收**：`cargo check --lib` 通过。写一个简单的 `#[test]` 验证 `to_anthropic_request()` 输出的 JSON 格式正确（不需要真调 API）。

**当日结束验收**：
1. `src/types.rs` 包含所有核心类型，`cargo check` 通过
2. `src/provider/mod.rs` 包含 `Provider` trait 定义
3. `src/provider/anthropic.rs` 包含 `AnthropicProvider` 结构体和请求构造逻辑
4. 理解 `ContentBlock` 的四个变体和 `Message` 的四个变体之间的关系

---

### M1 Day 2：Anthropic SSE 解析 + tool call 累积器

**时间**：4-6 小时

#### 任务 2.1：实现 `ToolCallAccumulator`（约 50 行）

**做什么**：对照设计文档 §6.2，在 `src/provider/anthropic.rs` 中实现 `ToolCallAccumulator` 结构体。这是 Anthropic SSE 解析的核心——因为 Anthropic 的 `content_block_stop` 不携带完整 arguments，必须在 adapter 内部累积 `input_json_delta`。

**关键代码片段**：设计文档 §6.2 中部的 `ToolCallAccumulator` 结构体。三个方法：
- `on_start(id, name)` — 记录新 tool_use block
- `on_delta(id, delta)` — 拼接 partial_json
- `finalize_all()` — drain 所有 pending，产生 `Vec<StreamEvent::ToolCallDone>`

**理解要点**：为什么需要这个累积器？看设计文档 §4.2 末尾的"关键差异处理"——Anthropic 在 SSE 流中分步发送 tool call 参数（`content_block_start` → 多次 `content_block_delta(partial_json)` → `content_block_stop`），最后的 `content_block_stop` 没有完整 arguments。所以必须自己拼接。

#### 任务 2.2：实现 `parse_sse_event()`（约 80 行）

**做什么**：对照设计文档 §4.2 中的 `parse_sse_event()` 函数，处理 Anthropic SSE 事件类型。每个 event type 对应一个 `StreamEvent` 变体。

**事件映射表**（依据设计文档 §2.3 映射表）：
- `message_start` → `StreamEvent::UsageInput`
- `content_block_start`(type=tool_use) → 调 `accumulator.on_start()`
- `content_block_delta`(type=text_delta) → `StreamEvent::TextDelta`
- `content_block_delta`(type=input_json_delta) → 调 `accumulator.on_delta()`
- `content_block_delta`(type=thinking_delta) → `StreamEvent::ThinkingDelta`
- `message_delta` → `StreamEvent::UsageOutput` + 缓存 `stop_reason`
- `message_stop` → 调 `accumulator.finalize_all()` 产生 `ToolCallDone` + 缓存的 `stop_reason` 合成 `Done`
- `error` → `StreamEvent::Done(StopReason::Error(...))`

**关键坑**：Anthropic 的 `stop_reason` 在 `message_delta` 事件的 `delta.stop_reason` 字段中，不在 `message_stop` 中！必须在收到 `message_delta` 时缓存到 `AnthropicProvider` 的一个 `Option<String>` 字段上，然后在 `message_stop` 到来时消费它。这是设计文档 §4.2 末尾特别强调的。

#### 任务 2.3：实现 SSE 字节流解析器（约 80 行）

**做什么**：解析 `reqwest::Response` 的字节流为 SSE 事件。SSE 格式是 `event: <type>\ndata: <json>\n\n`。

实现一个异步函数：
```rust
fn parse_sse_stream(
    response: reqwest::Response,
) -> impl Stream<Item = Result<StreamEvent>> {
    // 用 tokio::io::BufReader 逐行读取
    // 累积 event + data 两行
    // 遇到空行（分隔符）时调用 parse_sse_event()
}
```

**验收**：用一个 mock 的 SSE 字节流做单元测试。例如输入 `"event: message_start\ndata: {\"message\":{\"usage\":{\"input_tokens\":10}}}\n\n"` 应该产生 `StreamEvent::UsageInput`。

#### 任务 2.4：实现 `AnthropicProvider::stream()`（约 50 行）

**做什么**：把前面三个组件串起来。`stream()` 方法的逻辑：

1. 调用 `to_anthropic_request()` 构造请求 JSON
2. 用 `self.client.post(...).json(&body).send().await?` 发送 HTTP POST 到 `https://api.anthropic.com/v1/messages`
3. 检查 HTTP 状态码（非 200 → 返回 `ProviderError`）
4. 把 `response` 传给 `parse_sse_stream()`，得到 `impl Stream<Item = Result<StreamEvent>>`
5. `Box::pin` 包装后返回

**HTTP 头**：
- `x-api-key: {api_key}`
- `anthropic-version: 2023-06-01`
- `content-type: application/json`

**验收**：用一个集成风格的测试（需要 `ANTHROPIC_API_KEY` 环境变量）：发送一个简单请求（"回复'hello'"），验证能收到文本响应。如果手边没有 API key，先用 `#[ignore]` 标记测试。

**当日结束验收**：
1. `ToolCallAccumulator` 能正确累积和 finalize
2. SSE 事件解析器能正确处理 `message_start`、`content_block_delta`、`message_delta`、`message_stop` 四种主要事件
3. 有单元测试验证 SSE 解析逻辑（至少一个 mock 测试）
4. `cargo check --lib` 通过

---

### M1 Day 3：工具系统 + Agent 主循环

**时间**：4-6 小时

#### 任务 3.1：实现 `src/tools/mod.rs`（约 80 行）

**做什么**：对照设计文档 §5.1 和 §5.3，实现 `Tool` trait 和 `ToolRegistry`。

**`Tool` trait**：
```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn spec(&self) -> ToolSpec;
    async fn handle(&self, args: &str, session: &mut Session) -> Result<ToolOutput, ToolError>;
}
```

**`ToolOutput`** 结构体（§5.1）：`tool_call_id`、`text`、`data`、`success`。关键方法 `model_visible_text()` 生成 `"Exit code: 0\n<content>"` 格式。

**`ToolError`** 复用 M0 已在 `src/error.rs` 定义的两态枚举，不要在 `src/tools/mod.rs` 重复定义；通过 `use crate::error::ToolError;` 导入。`RespondToModel` 的错误文字会回注给模型让其自纠，`Fatal` 则终止 turn。

**`ToolRegistry`** 结构体（§5.3）：`HashMap<String, Arc<dyn Tool>>`。方法 `register`、`model_visible_specs`、`dispatch`。

#### 任务 3.2：实现 `src/tools/write_score.rs`（约 60 行）

**做什么**：对照设计文档 §5.2 的 `write_score` 工具定义。

`handle()` 方法的逻辑：
1. 解析 JSON args（`path` 和 `content` 字段）
2. 路径安全检查：拒绝含 `..` 的路径（防目录穿越，设计文档附录 C.2）
3. 将 `content` 写入 `path` 指定的文件
4. 更新 `session.current_score = Some(content)`（这是 ADR-7 的设计决策——`Tool::handle` 接收 `&mut Session`）
5. 自动调用 `alda parse`（复用后面写的 `alda_parse` 逻辑，或先简单调用 `alda parse` 子进程）
6. 返回 `ToolOutput`

**如果出现 `io::Error`**（文件写入失败、权限不足）：返回 `ToolError::Fatal`——模型帮不上忙。
**如果 alda parse 报错**：返回 `ToolError::RespondToModel`——把 parse 错误作为 output text 回给模型，让它修正。

#### 任务 3.3：开始实现 `src/agent.rs`（约 150 行）

**做什么**：对照设计文档 §3.1，实现 `AgentLoop` 结构体和 `run_turn()` 方法。

**`AgentLoop` 结构体**（§3.1）：
```rust
pub struct AgentLoop {
    session: Session,
    provider: Box<dyn Provider>,
    tools: ToolRegistry,
    config: AldaConfig,
}
```

**`run_turn()` 方法**（§3.1 的完整实现）：
1. 双层循环结构：外层 `run()` 接收用户输入，内层 `run_turn()` 处理模型调用 → 工具执行 → 模型再思考的循环
2. `MAX_TURN_ITERATIONS = 20` 安全上限
3. 组装 `ChatRequest`（调 `build_system_prompt()` 和 `tools.model_visible_specs()`）
4. 调用 `provider.stream(request)`
5. 处理流事件：`TextDelta` → 打印 + 追加到 `turn_assistant_content`；`ToolCallDone` → 追加到 `pending_tool_calls`
6. 流异常退出检测（EOF without Done → `AgentError::Provider(Fatal(...))`）
7. 执行工具调用（顺序执行，每个 tool call 调 `tools.dispatch()`）
8. 工具失败 → `follow_up = true`（让模型在下一轮自纠）

**今天先不写完整个循环**——Session 和相关类型 Day 4 才实现。今天可以先写骨架和类型签名，能编译即可。

**验收**：`cargo check --lib` 通过。理解双层 while loop 的逻辑。如果暂时有编译错误（Session 未实现），用 `todo!()` 占位。

**当日结束验收**：
1. `Tool` trait 和 `ToolRegistry` 可编译
2. `write_score` handler 实现了路径安全和文件写入逻辑
3. `AgentLoop` 骨架可编译，核心循环逻辑以注释标注
4. 理解工具错误的两态模型（RespondToModel vs Fatal）

---

### M1 Day 4：Session + Prompt + alda_parse 工具 + 端到端联调

**时间**：4-6 小时

#### 任务 4.1：实现 `src/session.rs`（约 200 行）

**做什么**：对照设计文档 §7.1，实现 `Session` 结构体和 JSONL 追加功能。

**核心字段**：`id`、`history: Vec<Message>`、`current_score: Option<String>`、`token_info: TokenUsageInfo`、`log_path: PathBuf`。

**`Session::new(sessions_dir)`**：创建 UUID、生成文件名 `{timestamp}-{uuid}.jsonl`、写首行 `session_meta`。

**`Session::append_line()`**：以追加模式打开 JSONL 文件，写一行 JSON。参考设计文档 §7.1 中部的实现，包括文件尾 `\n` 检查。

**`Session::persist()`**：如果 `current_score` 有值，写 `score_state` 行。

**`Session::active_messages()`**：过滤掉 `Message::System` 变体（system prompt 在 `ChatRequest` 中单独传递）。

**今天不实现 resume**（逆序扫描 + compacted 检查点恢复），那是 M3 的内容。

#### 任务 4.2：实现 `src/prompt.rs` + `src/prompts/base_instructions.md`

**做什么**：对照设计文档 §3.2，实现 `AgentLoop::build_system_prompt()` 和提示词文件。

**`prompts/base_instructions.md`**：抄设计文档 §3.2 中部的模板（约 30 行 Markdown）。定义 agent 的角色、工作流程、核心约束、输出格式。

**`prompt.rs`**：
```rust
impl AgentLoop {
    fn build_system_prompt(&self) -> String {
        let mut prompt = String::new();
        prompt.push_str(include_str!("prompts/base_instructions.md"));
        if let Some(score) = &self.session.current_score {
            prompt.push_str(&format!("\n\n## 当前乐谱\n```alda\n{score}\n```\n"));
        }
        prompt
    }
}
```

注意：M1 阶段暂不注入 `alda_cheatsheet.md`（M3 才做）。

#### 任务 4.3：实现 `src/tools/alda_parse.rs`（约 80 行）

**做什么**：对照设计文档 §5.2 的 `alda_parse` 工具定义和附录 C.1 的 `AldaCommand` 封装。

你需要在 `src/tools/` 下新建一个子进程管理模块（或直接在 `alda_parse.rs` 中实现）。调用 `alda parse -f <path> -o data` 或 `-o events`。

**子进程管理要点**（附录 C）：
- 使用 `tokio::process::Command`，不阻塞异步运行时
- 始终注入环境变量 `ALDA_DISABLE_SPAWNING=yes`（防止 agent 无意启动 player pool）
- 超时 30 秒（parse 通常在毫秒级完成）
- 路径安全检查：拒绝含 `..` 的路径
- stdout 捕获为 `String`，stderr 捕获为 `String`
- 成功时返回 `serde_json::Value`；失败时将 stderr 作为 `ToolError::RespondToModel` 返给模型

**如果 `alda` 二进制不存在**（`Spawn` 错误）：返回 `ToolError::Fatal`（模型帮不上忙）。

#### 任务 4.4：端到端联调

**做什么**：回到 `src/agent.rs`，把 Session + write_score + alda_parse + Anthropic provider 串联起来。

写一个简化的 `main.rs`：
```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 初始化 tracing, config, provider, tools, session
    // 从命令行参数读取用户输入
    // 创建 AgentLoop, 调用 run_turn() 一次
}
```

**验收测试**：用 `ANTHROPIC_API_KEY` 环境变量运行：
```bash
cargo run -- "用alda写一段4小节的C大调旋律, 钢琴音色"
```

**预期行为**：
1. Agent 调用 `write_score` 生成 `score.alda`
2. Agent 自动调用 `alda_parse` 验证
3. 如果 parse 失败（语法错误），Agent 自动修正后重试
4. 最终 parse 通过，Agent 报告成功

**如果模型不调用工具**：检查 `tools.model_visible_specs()` 是否正确返回工具定义，检查系统提示词是否指示了工具使用。
**如果 alda parse 子进程启动失败**：检查 `alda` 是否在 PATH 上，或设置 `ALDA_BINARY` 环境变量指向 alda 绝对路径。
**如果 parse 报错但模型不自纠**：检查 `ToolError::RespondToModel` 的错误信息是否正确传回了模型（确认 `follow_up = true` 生效）。

**当日结束验收**：
1. `Session` 能创建 JSONL 文件，写 `session_meta` 和 `score_state` 行
2. `build_system_prompt()` 正确组装提示词
3. `alda_parse` 工具能成功调用 `alda parse` 子进程并返回 JSON
4. 端到端：Agent 自行完成"写乐谱 → parse 失败 → 修正 → 通过"的循环（至少验证通过一次）

---

### M1 Day 5：修复 + 打磨 + 写入单元测试

**时间**：4-6 小时

#### 任务 5.1：修复 Day 4 端到端测试中发现的问题

你在 Day 4 的端到端测试中几乎一定会遇到问题。常见问题清单：

**问题 A：Anthropic API 返回 401 Unauthorized**
- 检查 `ANTHROPIC_API_KEY` 环境变量是否正确设置
- 检查请求头 `x-api-key` 是否正确

**问题 B：模型不调用工具，直接回复文本**
- 检查 `system_prompt` 是否明确指示了工作流程（"用 write_score 工具写出乐谱"）
- 检查 `tools` 数组是否正确包含在请求 JSON 中
- 检查 Anthropic 的 `tool_choice` 参数（可以设为 `"auto"` 或省略）

**问题 C：SSE 流提前结束，没有收到 Done 事件**
- 这是设计文档 §3.1 中处理的"流异常退出"情况——检查你的 SSE 解析器是否正确处理了连接关闭
- 加日志：在收到每个 SSE 事件时 `tracing::debug!` 打印

**问题 D：alda parse 的输出不是合法 JSON**
- 确认你用的是 `-o data` 或 `-o events`（不是 `-o ast-human`）
- 检查 stdout 是否包含额外输出（如 alda 的 warning）

#### 任务 5.2：添加单元测试

**必须写的测试**（按优先级）：

1. **`types.rs` — Message 序列化/反序列化**：验证 `serde_json::to_value` 和 `from_value` 的往返一致性
2. **`provider/anthropic.rs` — 请求 JSON 格式**：验证 `to_anthropic_request()` 输出符合 Messages API 规范
3. **`provider/anthropic.rs` — ToolCallAccumulator**：模拟三次 `on_delta` + `finalize_all` 产生完整的 `ToolCallDone`
4. **`tools/alda_parse.rs` — 路径安全**：验证 `path` 包含 `..` 时被拒绝
5. **`tools/write_score.rs` — 文件写入和自动 parse**：创建临时目录，写入 alda 代码，验证文件存在

#### 任务 5.3：打磨错误信息

确保所有面向用户的错误信息清晰可操作：
- "alda 二进制未找到，请确认 alda 已安装且在 PATH 中" (而非 "spawn error")
- "API key 未设置，请设置 ANTHROPIC_API_KEY 环境变量" (而非 "auth error")

#### 任务 5.4：准备 M1 最终验收

运行以下验收场景：

1. **正向**：`"写一段 4 小节 C 大调旋律"` → Agent 生成、parse、修正、通过
2. **纠错**：`"写一段旋律，使用不存在的乐器 xyz-instrument"` → parse 失败 → Agent 修正为有效乐器名
3. **简单指令**：`"在现有乐谱上增加小提琴声部"` → Agent 读取当前 score 状态 → 追加

**记录**：保存每个场景的 JSONL 日志，后续 M4 评测会用到。

**M1 完成验收**：
- `cargo build --release` 无 warning
- 所有单元测试通过（`cargo test`）
- 至少 3 个端到端场景验证通过
- JSONL 日志文件格式正确，可用 `cat sessions/*.jsonl | jq .` 查看

---

## M2：REPL 驱动 + 流式 + 第二 provider（5 天）

**目标**：完善交互体验，验证双 provider 抽象的可用性。用户能切换 provider，流式输出流畅，
能进行多轮对话（输入需求 → 看模型迭代 → 播放 → 听反馈 → 修正）。

**设计依据**：设计文档 §11 里程碑 M2。

---

### M2 Day 1：OpenAI Provider 适配器

**时间**：4-6 小时

#### 任务 1.1：实现 `src/provider/openai.rs`（约 200 行）

**做什么**：对照设计文档 §4.3，实现 `OpenAIProvider` 结构体和 Responses API adapter。

**与 Anthropic 的差异**（设计文档 §2.3 映射表）：
- `system` 字段叫 `instructions`（顶层字段）
- 消息格式：`{"type": "message", "role": "...", "content": [{"type": "input_text", "text": "..."}]}`
- 工具格式：`{"type": "function", "name": "...", "description": "...", "parameters": ...}`
- `max_tokens` 叫 `max_output_tokens`
- SSE 事件：`response.output_text.delta` → `TextDelta`；`response.output_item.done` → `ToolCallDone`（携带完整 arguments）

**关键简化**：OpenAI 的 `response.output_item.done` 直接携带完整 `arguments`，不需要 `ToolCallAccumulator`。这是比 Anthropic adapter 简单的地方。

#### 任务 1.2：Provider factory

**做什么**：在 `src/provider/mod.rs` 中添加一个工厂函数：

```rust
pub fn create_provider(config: &AldaConfig) -> Result<Box<dyn Provider>> {
    match config.provider {
        ProviderType::Anthropic => Ok(Box::new(AnthropicProvider::new(config)?)),
        ProviderType::OpenAI => Ok(Box::new(OpenAIProvider::new(config)?)),
    }
}
```

#### 任务 1.3：验证双 provider 抽象

**做什么**：分别用 Anthropic 和 OpenAI key 跑同一个端到端测试，验证两种 provider 的行为一致。

**如果 trait 抽象有漏水**：
- 例如某个 provider 特有的参数需要穿透 trait 传递——这说明 trait 设计不够通用。记录问题，但 M2 不改 trait 签名（ADR-5 的设计意图就是先验证）。
- 一个可行的临时方案：在 `AldaConfig` 中加 `provider_extra: HashMap<String, String>`，adapter 各自读自己需要的。

**验收**：`OPENAI_API_KEY=<key> cargo run -- "写一个简单的旋律"` 和 `ANTHROPIC_API_KEY=<key> cargo run -- "写一个简单的旋律"` 都能得到合理结果。

**当日结束验收**：
1. `OpenAIProvider` 实现完成，`cargo check --lib` 通过
2. 两种 provider 都通过端到端测试
3. provider trait 没有为第二个实现而修改——证明 trait 抽象可用

---

### M2 Day 2：play_for_human 工具

**时间**：4-6 小时

#### 任务 2.1：实现 `src/tools/play_for_human.rs`（约 60 行）

**做什么**：对照设计文档 §5.2 的 `play_for_human` 工具定义。

`handle()` 逻辑：
1. 调用 `alda play -f <path>`（子进程，用 `tokio::process::Command`）
2. 超时 120 秒（可配置 `alda_timeout_secs`）
3. 注入 `ALDA_DISABLE_SPAWNING=yes`
4. 返回 `ToolOutput`：只含元信息 `"已为用户播放, wall time Xs, 退出码 N"`，**不含任何听感评价**

**关键设计约束**（反复强调）：LLM 听不到音频！`play_for_human` 的输出只告诉模型"播放成功/失败"，不告诉模型"好不好听"。用户的反馈是模型获得听感信息的唯一途径。这个边界必须在提示词中和工具描述中反复强调。

**如果 alda play 超时**（乐谱很长）：返回 `ToolError::RespondToModel("播放超时(120s)，乐谱可能太长")`——模型可以修改。
**如果 alda 二进制不存在**：返回 `ToolError::Fatal("alda 未安装")`——模型帮不上忙。

#### 任务 2.2：更新提示词

在 `base_instructions.md` 中加入 `play_for_human` 的使用指引：

```markdown
## 播放工具 (play_for_human)

- 使用 play_for_human 让用户试听你的乐谱
- **重要：你看不到也听不到任何音频**。播放工具只告诉你播放成功/失败
- 播放后必须请用户给你反馈。用户的口头评价是你理解音乐效果的唯一途径
- 不要在用户反馈之前连续多次播放
```

**当日结束验收**：
1. `play_for_human` 工具可以调用 `alda play`
2. 工具输出不含听感评价，只有元信息
3. 提示词中明确说明了 LLM 听不到音频的约束

---

### M2 Day 3：交互式 REPL 模式

**时间**：4-6 小时

#### 任务 3.1：实现 REPL 循环

**做什么**：改造 `src/main.rs`，支持交互式 REPL 模式（默认）和单次命令行模式（`--prompt` 参数）。

使用 `clap` 的 derive 模式定义 CLI：

```rust
#[derive(Parser)]
struct Cli {
    /// 单次提示词模式（非 REPL）
    #[arg(short, long)]
    prompt: Option<String>,

    /// 指定 provider
    #[arg(short, long)]
    provider: Option<String>,

    /// 指定模型
    #[arg(short, long)]
    model: Option<String>,

    /// Resume 指定会话
    #[arg(short, long)]
    resume: Option<String>,
}
```

**REPL 循环**（在 `agent.rs` 的 `run()` 方法中）：
```rust
pub async fn run(&mut self) -> Result<()> {
    loop {
        // 打印提示符 "> "
        print!("> ");
        stdout().flush()?;

        let mut input = String::new();
        match stdin().read_line(&mut input) {
            Ok(0) => break, // Ctrl+D
            Ok(_) => {
                let input = input.trim().to_string();
                if input.is_empty() { continue; }

                // REPL 命令
                if input.starts_with(':') {
                    self.handle_command(&input).await?;
                    continue;
                }

                // 普通消息
                self.session.history.push(Message::User {
                    content: vec![ContentBlock::Text { text: input }],
                });
                self.run_turn().await?;
                self.session.persist()?;
            }
            Err(e) => {
                eprintln!("read error: {e}");
                break;
            }
        }
    }
    Ok(())
}
```

#### 任务 3.2：实现 REPL 命令

**支持的命令**：
- `:quit` / `:q` — 退出
- `:resume <file>` — 从 JSONL 文件恢复会话（M3 实现具体逻辑，M2 先占位）
- `:compact` — 手动触发上下文压缩（M3 实现具体逻辑）
- `:sessions` — 列出会话目录中的 JSONL 文件
- `:help` / `:h` — 打印帮助
- `:model <name>` — 切换模型
- `:provider <name>` — 切换 provider

**`:help` 输出样例**：
```
REPL 命令:
  :quit, :q          退出
  :resume <file>     从会话文件恢复
  :compact           手动压缩上下文
  :sessions          列出所有会话
  :model <name>      切换模型
  :provider <name>   切换 provider (anthropic/openai)
  :help, :h          显示此帮助
```

#### 任务 3.3：添加 CancellationToken 支持

**做什么**：对照设计文档 §3.3，在 `AgentLoop` 中集成 `tokio_util::sync::CancellationToken`。

在 REPL 模式中：
- 注册 `Ctrl+C` handler（`tokio::signal::ctrl_c()`）
- `Ctrl+C` 触发 `cancel_token.cancel()`
- `run_turn()` 在 `tokio::select!` 中同时等待 `cancel_token.cancelled()` 和实际工作

**当 Ctrl+C 被按下**：
- 尝试杀掉 alda 子进程（`child.kill()`）
- 保存当前会话状态（`session.persist()`）
- 不退出程序，回到 REPL 提示符

**验收**：在 REPL 中输入一个复杂的音乐需求，在模型回答过程中按 Ctrl+C，Agent 打印 `[interrupted]` 并回到提示符。

**当日结束验收**：
1. REPL 模式可以交互式输入需求
2. `:help` 列出所有命令
3. `Ctrl+C` 能中断当前 turn 并回到提示符
4. `--prompt "..."` 模式仍然可用（一次性运行后退出）

---

### M2 Day 4：流式输出体验优化

**时间**：4-6 小时

#### 任务 4.1：实时 text delta 输出

**做什么**：确保 `StreamEvent::TextDelta` 的内容实时打印到 stdout，不缓冲。

使用 `print!("{text}")` + `stdout().flush()` 确保每个 delta 立即可见。这是 M1 已经实现的，但验证一下在 REPL 模式下仍然工作。

#### 任务 4.2：可选 thinking 展示

**做什么**：当 `config.show_thinking` 为 true 时，把 `StreamEvent::ThinkingDelta` 的内容以灰色输出到 stderr，前缀 `[think]`。这是设计文档 §3.1 的预期行为。

```rust
StreamEvent::ThinkingDelta { text } => {
    if self.config.show_thinking {
        eprint!("\x1b[90m[think] {text}\x1b[0m");
    }
}
```

#### 任务 4.3：工具输出格式化

**做什么**：当工具执行完成时，以统一的格式输出工具结果摘要：

```
[write_score] Exit code: 0 | /path/to/score.alda (NNN bytes)
[alda_parse] Exit code: 0 | NNNN events, N parts
[play_for_human] Wall time: 2.3s | Exit code: 0
```

用 ANSI 颜色区分：成功用绿色，失败用红色，信息用灰色。

**验收**：运行一次对话，观察终端输出是否清晰可读。工具调用的输出不淹没模型的文本回复。

**当日结束验收**：
1. text delta 实时打印，无明显延迟
2. thinking 展示可选开关
3. 工具执行结果以彩色格式化输出
4. 流式输出的整体感官流畅自然

---

### M2 Day 5：修复 + 多轮对话验证

**时间**：4-6 小时

#### 任务 5.1：多轮对话场景测试

**测试场景 1：风格修改**
```
> 写一首C大调圆舞曲, 钢琴
(Agent 生成乐谱, parse 通过)
> 改成A小调
(Agent 修改调性, 重新 parse)
> 加快速度, 变成快板
(Agent 修改 tempo)
> 播放给我听
(Agent 调用 play_for_human)
> 难听, 低音太重了
(Agent 修改低音部分)
```

**测试场景 2：多声部**
```
> 写一段四声部合唱, SATB
(Agent 生成四个声部)
> 女高音太高了, 降低一个八度
(Agent 修改 soprano 声部)
> 加上竖琴伴奏
(Agent 添加 harp 声部)
```

**测试场景 3：错误恢复**
```
> 写一段用"trumpet"和"flute"的旋律
(Agent 使用合法乐器)
> 把 flute 换成 xylo
(Agent 尝试, 如果 xylo 不是有效 Alda 名则报错并建议 xylophone)
```

#### 任务 5.2：记录会话日志

每个测试场景结束后，检查 `sessions/` 目录下的 JSONL 文件：
- 每行是合法 JSON
- 包含 `session_meta` 首行
- 包含所有 user、assistant、tool 消息
- `score_state` 行反映最新乐谱状态

#### 任务 5.3：打磨 CLI 体验

- `--help` 输出完整且有用
- `--prompt "..."` 模式和 REPL 模式切换流畅
- 环境变量缺失时给出清晰的错误信息
- `RUST_LOG=debug` 时可以查看详细日志

**M2 完成验收**：
- 两种 provider 均可正常工作
- REPL 支持多轮对话，`Ctrl+C` 不退出程序
- play_for_human 工具可以触发播放
- 流式输出流畅，工具输出格式化清晰
- 至少 3 个多轮对话场景通过

---

## M3：提示词工程 + 持久化（5 天）

**目标**：系统提示词注入 alda 语言知识，会话可 pause/resume，score_analyze 工具完整实现。

**设计依据**：设计文档 §11 里程碑 M3。

---

### M3 Day 1：alda 语法速查表 + 乐理度量函数

**时间**：4-6 小时

#### 任务 1.1：编写 `src/prompts/alda_cheatsheet.md`（约 200 行）

**做什么**：从 `docs/research/alda-language.md` 和 `docs/research/music-theory.md` 提炼精简的 alda 语法参考（约 200 行），注入系统提示词。

**内容应包含**（精简版，只够 LLM 参考使用）：
- 基本语法：`乐器名: 音符`、八度记号 `> <`、时值 `.`、附点
- 音符表示：`c d e f g a b`、升降号 `+ -`、休止符 `r`
- 和弦：`c/e/g` 或 `c e g` 同时发音
- 变音记号作用范围：同一小节相同音高
- 声部 (part)：`piano:`、`violin:` 等
- 特性/属性：`(volume 50)`、`(panning 50)`、`(tempo 120)`
- 标记 (marker)：`[verse]`、`[chorus]`
- 全局属性：`(tempo! 120)` vs 声部属性 `(tempo 120)`
- cram 节奏：`{c d e}` 三连音
- 常见陷阱：重复音符需显式写出、变音记号作用范围等

**验证**：把 `alda_cheatsheet.md` 作为附件给模型，问一个 alda 语法问题（"怎么在 alda 里写三连音？"），确认模型能从 cheatsheet 中学到答案。

#### 任务 1.2：实现 `src/metrics.rs` 的部分函数（约 200 行）

**做什么**：对照设计文档，实现乐理度量计算函数。这些函数输入是 `alda parse -o data` 的 JSON 数据，输出是数值。

**M3 Day 1 先实现核心 8 个函数**：
1. `diatonic_ratio(events, key)` — 调内音比例
2. `pitch_range(events)` — 音域（最低到最高 midi-note）
3. `pitch_diversity(events)` — 不同音高的数量
4. `note_density(events)` — 音符密度（音符数/小节）
5. `rest_ratio(events)` — 休止符比例
6. `consonance_ratio(events, key)` — 协和度
7. `melodic_contour(events)` — 旋律轮廓（上行/下行比例）
8. `voice_count(data)` — 声部数量

**输入格式**：每个 event 对象包含 `midi-note`（音高编号 0-127）、`offset`（开始时间 ms）、`duration`（持续 ms）、`volume`（力度）等字段。依据 `alda-interfaces.md` §2.3 的 `data` 输出格式。

**验收**：对 `tests/fixtures/` 中已有的 alda 乐谱运行度量函数，打印结果。

#### 任务 1.3：更新 `build_system_prompt()`

在 `src/prompt.rs` 中加入 `alda_cheatsheet.md` 的注入：

```rust
fn build_system_prompt(&self) -> String {
    let mut prompt = String::new();
    prompt.push_str(include_str!("prompts/base_instructions.md"));
    prompt.push_str("\n\n---\n\n");
    prompt.push_str(include_str!("prompts/alda_cheatsheet.md"));
    if let Some(score) = &self.session.current_score {
        prompt.push_str(&format!("\n\n## 当前乐谱\n```alda\n{score}\n```\n"));
    }
    prompt
}
```

**注意**：`alda_cheatsheet.md` 约 200 行 (~2000 tokens)，这是合理的系统提示词大小。如果将来发现 token 消耗过大，可以考虑动态注入（只注入相关的部分）。

**当日结束验收**：
1. `alda_cheatsheet.md` 涵盖基本 alda 语法
2. 8 个度量函数通过基本测试
3. `cargo check --lib` 通过

---

### M3 Day 2：score_analyze 工具完整实现

**时间**：4-6 小时

#### 任务 2.1：实现 `src/tools/score_analyze.rs`（约 200 行）

**做什么**：对照设计文档 §5.2 的 `score_analyze` 工具定义。

`handle()` 逻辑：
1. 解析 args（`path` 和可选的 `checks` 数组）
2. 调用 `alda parse -f <path> -o data` 获取 score JSON
3. 运行度量函数（默认全部运行，如果 `checks` 指定了则只运行那些）
4. 将度量结果格式化为人类可读的摘要文本
5. 返回 `ToolOutput`

**摘要格式示例**：
```
Score Analysis: tests/fixtures/c_major_melody.alda
====================================================
Events: 32 | Parts: 1 (piano)

--- Pitch ---
Range: C4 (60) - C6 (84)  (2 octaves)
Distinct pitches: 8
Diatonic ratio: 0.97  (very tonal)

--- Rhythm ---
Note density: 4.0 notes/bar
Rest ratio: 0.12
Duration variety: 0.45

--- Harmony ---
Consonance ratio: 0.82
Chord types: major=3, minor=1, dim=0, aug=0

No voice crossing detected.
All instruments within range.
```

**如果 alda parse 失败**：返回 `ToolError::RespondToModel`——把错误信息回给模型。
**如果 score JSON 解析失败**：返回 `ToolError::RespondToModel("无法解析 score 数据: {error}")`。

#### 任务 2.2：实现剩余 9 个度量函数

在 `src/metrics.rs` 中补充实现：
9. `rhythm_diversity(events)` — 节奏多样性
10. `duration_variety(events)` — 时值种类数
11. `chord_type_distribution(events)` — 和弦类型分布
12. `voice_crossing_detection(data)` — 声部交叉检测
13. `instrument_range_compliance(data)` — 乐器音域合规
14. `volume_dynamics(events)` — 力度变化
15. `articulation_variety(events)` — 演奏法多样性
16. `phrase_length_analysis(events, markers)` — 乐句长度分析
17. `cadence_detection(events, key)` — 终止式检测（简化版）

**验收**：对 `tests/fixtures/` 中的多个乐谱运行全部 17 个度量函数，确认没有 panic，输出值在合理范围内。

**当日结束验收**：
1. `score_analyze` 能正确调用 alda parse 并生成分析摘要
2. 全部 17 个度量函数实现完毕
3. 至少用一个真实的 alda 乐谱验证分析输出

---

### M3 Day 3：Session resume 实现

**时间**：4-6 小时

#### 任务 3.1：实现 `Session::resume()` 方法（约 120 行）

**做什么**：对照设计文档 §7.1 的 `resume` 实现。

**算法（两遍扫描）**：
1. **第一遍：逆序扫描** JSONL 文件，找到：
   - `session_meta` → 提取 `id`
   - `compacted`（含 `replacement_history`）→ 记录行号，这是最新检查点
   - `score_state` → 提取 `alda_source`
2. **第二遍：正序重放** 从检查点之后的所有 `response_item` 行，重建 `history`

**关键代码结构**：
```rust
pub fn resume(log_path: &Path) -> Result<Self> {
    let content = std::fs::read_to_string(log_path)?;
    let lines: Vec<&str> = content.lines().collect();

    // 第一遍：逆序找检查点
    let mut id = String::new();
    let mut current_score = None;
    let mut checkpoint_idx = None;

    for (i, line) in lines.iter().enumerate().rev() {
        let item: Value = serde_json::from_str(line)?;
        match item["type"].as_str() {
            Some("session_meta") => {
                id = item["payload"]["id"].as_str().unwrap_or("").to_string();
            }
            Some("compacted") => {
                if item["payload"]["replacement_history"].is_array() {
                    checkpoint_idx = Some(i);
                    break;
                }
            }
            Some("score_state") if current_score.is_none() => {
                current_score = item["payload"]["alda_source"].as_str().map(String::from);
            }
            _ => {}
        }
    }

    // 第二遍：正序重放
    let start = checkpoint_idx.unwrap_or(0);
    let mut history = vec![];

    // 如果有检查点，以 replacement_history 为基线
    if let Some(idx) = checkpoint_idx {
        let item: Value = serde_json::from_str(lines[idx])?;
        if let Some(replacement) = item["payload"]["replacement_history"].as_array() {
            for msg_val in replacement {
                history.push(serde_json::from_value(msg_val.clone())?);
            }
        }
    }

    // 重放检查点之后的 response_item
    for line in &lines[(start + 1)..] {
        let item: Value = serde_json::from_str(line)?;
        if item["type"] == "response_item" {
            history.push(serde_json::from_value(item["payload"].clone())?);
        }
    }

    Ok(Self { id, history, current_score, ... })
}
```

#### 任务 3.2：实现 `:resume` REPL 命令

在 REPL 的 `handle_command()` 方法中实现：
```rust
":resume" => {
    let path = PathBuf::from(args);
    if !path.exists() {
        eprintln!("会话文件不存在: {}", path.display());
    } else {
        let session = Session::resume(&path)?;
        self.session = session;
        println!("已恢复会话: {} ({} 条消息)", self.session.id, self.session.history.len());
    }
}
```

#### 任务 3.3：实现 `:sessions` REPL 命令

列出 `sessions_dir` 中的所有 `.jsonl` 文件，显示文件名、大小、修改时间。

**验收测试**：
1. 进行一次多轮对话，退出程序
2. 重新启动，用 `:sessions` 找到刚才的会话文件
3. `:resume sessions/xxx.jsonl` 恢复会话
4. 输入新的音乐需求，验证 Agent 能接续之前的上下文（知道之前的乐谱内容）

**当日结束验收**：
1. `Session::resume()` 能从 JSONL 文件恢复完整的消息历史
2. `:resume` 和 `:sessions` REPL 命令可用
3. 恢复后 Agent 能记住之前的乐谱内容

---

### M3 Day 4：上下文压缩（compact）

**时间**：4-6 小时

#### 任务 4.1：实现 `Session::compact()` 方法（约 80 行）

**做什么**：对照设计文档 §7.3 的压缩算法。

**三步过程**：
1. **生成摘要**：调用模型的 `complete()` 方法（非流式），传入 compact prompt，让模型写一份"任务交接摘要"
2. **构造替换历史**：保留所有 `Message::User`（含人耳反馈），加上摘要，加上当前乐谱作为 `Message::System`
3. **落盘 compacted 检查点**：写一行 JSONL，包含 `summary` 和 `replacement_history` 全文

**Compact prompt**（§7.3）：
```text
你正在进行上下文压缩。请写一份任务交接摘要给另一个 LLM:
 - 当前的乐谱进展(调性、曲式、乐器)
 - 用户的偏好反馈
 - 还剩什么没完成
 - 乐谱当前是否通过 parse 校验
不需要复述乐谱内容——乐谱会作为 score_state 注入。
```

**`Message::System` 的语义**（§7.3 的注释）：乐谱状态放在 `replacement_history` 中作为 `System` 消息，resume 后 `build_system_prompt()` 会发现并将其设为 `session.current_score`。`active_messages()` 过滤掉 `System` 消息。这样乐谱是"上下文背景"而非"对话内容"。

#### 任务 4.2：实现 pre-turn 自动触发

**做什么**：在 `run_turn()` 开始前检查 `token_info.needs_compaction()`。如果 true，先执行 compact。

触发阈值（§7.2）：当前 input tokens 占用超过 context_window 的 80%。

```rust
if self.session.token_info.needs_compaction() {
    tracing::info!("context window {:.0}% full, triggering compaction",
        (self.session.token_info.last_input as f64 / self.session.token_info.context_window as f64) * 100.0);
    self.session.compact(&*self.provider).await?;
}
```

#### 任务 4.3：实现 `:compact` 手动命令

```rust
":compact" => {
    self.session.compact(&*self.provider).await?;
    println!("压缩完成。当前上下文: {} 条消息", self.session.history.len());
}
```

**验收测试**：
1. 进行 10+ 轮的长对话（反复修改乐谱），观察 token usage
2. 手动触发 `:compact`
3. 检查 JSONL 文件，确认 `compacted` 行存在且 `replacement_history` 包含摘要
4. 退出后用 `:resume` 恢复，确认 Agent 仍保持对乐谱的理解

**当日结束验收**：
1. `compact()` 能调用模型生成摘要并写入检查点
2. `:compact` 命令可用
3. 压缩后 resume 不丢失乐谱信息

---

### M3 Day 5：修复 + 集成测试

**时间**：4-6 小时

#### 任务 5.1：端到端长对话测试

**测试场景**：
```
> 写一段8小节的G大调旋律, 小提琴
> 加上大提琴的低音声部
> parse看看有没有问题
> 分析一下乐谱
(检查 score_analyze 输出是否合理)
> 播放
> 太单调了, 加一些切分节奏
> 改成E小调
> :compact
> :sessions
(退出)
(重新启动, :resume 刚才的会话)
> 在最后加一个终止式 (V-I)
```

验证整个流程中 Agent 的行为一致性。

#### 任务 5.2：提示词微调

根据测试中发现的问题微调提示词：
- Agent 是否过度使用某个工具？
- Agent 是否忽略用户反馈？
- Agent 是否在不需要时也调用 play_for_human？

**不要过度优化**——M3 结束后就应该停止修改提示词，M4 的评测会告诉你真正的改进方向。

#### 任务 5.3：代码清理

- 移除所有 `todo!()` 和 `unimplemented!()`
- 所有 `unwrap()` 改为 `?` 或 `expect()` 带清晰消息
- 使用 `cargo clippy` 检查并修复所有 warning

**M3 完成验收**：
- resume + compact 长对话场景完整通过
- score_analyze 输出合理
- `cargo clippy` 无 warning
- 提示词经过至少 3 次长对话测试验证

---

## M4：Evaluation harness（5 天）

**目标**：可度量地评估 agent 的乐谱生成质量。

**设计依据**：设计文档 §11 里程碑 M4；harness-engineering.md §9 的四层评测。

---

### M4 Day 1：测试集准备 + 客观校验框架

**时间**：4-6 小时

#### 任务 1.1：建立测试集

**做什么**：从项目中的 alda 示例乐谱（如果有 `examples/` 目录）中选择 10 首作为"金标准"，并设计 10-15 个测试场景（prompt）。

**测试场景设计**（三类）：

**转写任务（5 个）**：给一段 alda 代码，要求 Agent 生成"等价的"乐谱。不要求逐音符相同，要求结构相似。
```
例: "请把以下乐谱转换成只使用钢琴的版本: {alda_code}"
```

**补全任务（5 个）**：给前半段，要求 Agent 写后半段。
```
例: "这是8小节旋律的前4小节: {alda_code}。请写完后4小节，保持风格一致。"
```

**风格迁移任务（5 个）**：给一段乐谱，要求改变调性/节奏/情绪。
```
例: "把这段旋律从小调改成大调: {alda_code}"
```

**`tests/fixtures/` 目录结构**：
```text
tests/fixtures/
├── golden/
│   ├── melody_01.alda
│   ├── melody_02.alda
│   ├── ...
│   └── melody_10.alda
├── prompts/
│   ├── transcription_01.txt
│   ├── completion_01.txt
│   └── migration_01.txt
└── expected/
    ├── transcription_01.json  # 结构断言规格
    └── ...
```

#### 任务 1.2：实现 H0 客观校验

**做什么**：对照 harness-engineering.md §9 的 H0 层，实现自动化客观校验。

H0 校验的内容：
- `alda parse` 通过率：生成的所有乐谱能否成功 parse？
- 语法错误率：每千个音符多少个语法错误？
- alda doctor 兼容性：生成的乐谱能否通过 doctor 检查？

**实现 `tests/eval/h0_parse_check.rs`**：
```rust
/// H0: 客观校验 —— alda parse 通过率
fn test_parse_pass_rate(score_path: &Path) -> bool {
    // 调用 alda parse -f <path> -o data
    // 检查 exit code 为 0
}
```

**验收**：写一个脚本跑所有 golden 乐谱的 parse 验证，100% 通过（golden 本身应该是正确的）。

**当日结束验收**：
1. 测试集包含 10 首 golden 乐谱 + 15 个测试 prompt
2. H0 校验框架能运行 `alda parse` 并报告通过/失败

---

### M4 Day 2：H1 结构断言

**时间**：4-6 小时

#### 任务 2.1：定义结构断言规格

对每个测试场景，定义一个 JSON 断言规格文件：

```json
{
  "test_id": "completion_01",
  "prompt": "这是前4小节...请写完后4小节",
  "golden_file": "tests/fixtures/golden/melody_01.alda",
  "assertions": {
    "note_count": { "min": 8, "max": 64 },
    "pitch_range": { "min_octaves": 1, "max_octaves": 4 },
    "voice_count": { "exact": 1 },
    "duration_range_ms": { "min": 60000, "max": 180000 },
    "tempo_bpm": { "min": 60, "max": 200 },
    "diatonic_ratio": { "min": 0.5 },
    "parse_must_pass": true
  }
}
```

#### 任务 2.2：实现结构断言校验器

**实现 `tests/eval/h1_structural.rs`**：

```rust
struct StructuralAssertions {
    note_count: Option<Range<usize>>,
    pitch_range: Option<Range<usize>>,
    voice_count: Option<ExactOrRange>,
    duration_ms: Option<Range<u64>>,
    tempo_bpm: Option<Range<f64>>,
    diatonic_ratio: Option<Range<f64>>,
    parse_must_pass: bool,
}

fn check_h1(output_path: &Path, assertions: &StructuralAssertions) -> Result<Vec<String>> {
    // 1. 调用 alda parse -o data
    // 2. 从 score JSON 提取度量
    // 3. 逐条断言检查
    // 4. 返回失败项的列表
}
```

**验收**：对 golden 乐谱运行 H1 断言，检查断言规格本身是否合理（不过于宽松也不过于严格）。

**当日结束验收**：
1. 每个测试场景有对应断言规格
2. H1 校验器能从 score JSON 提取度量并逐条检查

---

### M4 Day 3：H2 LLM Judge

**时间**：4-6 小时

#### 任务 3.1：设计 Judge prompt

**做什么**：设计一个评估 prompt，让另一个 LLM（Judge）对 Agent 生成的乐谱进行双盲评分。

**双盲设计**：Judge 同时看到 golden（原版）和 generated（Agent 生成），但不知道哪首是哪首。从 5 个维度评分（1-5 分）：

1. **结构相似度**：生成的乐谱结构（声部、段落、小节数）与原文的相似程度
2. **旋律连贯性**：音符序列是否流畅、有逻辑
3. **和声合理性**：和弦选择和进行是否符合调性
4. **节奏一致性**：节奏风格是否与原文一致
5. **整体音乐性**：作为一个完整乐谱的音乐表现力

**实现 `tests/eval/h2_judge.rs`**：
```rust
async fn judge_pair(
    judge_provider: &dyn Provider,
    golden: &str,
    generated: &str,
    criterion: &str,
) -> Result<JudgeScore> {
    // 构造 prompt：同时给出两首乐谱（乱序），请 Judge 评分
    // 返回 1-5 分 + 简短理由
}

struct JudgeScore {
    structural_similarity: f64,
    melodic_coherence: f64,
    harmonic_validity: f64,
    rhythmic_consistency: f64,
    overall_musicality: f64,
    reasoning: String,
}
```

**注意**：LLM Judge 是补充手段，不替代 H0/H1 的客观校验（harness-engineering.md §9 明确：LLM Judge 只覆盖"客观校验和结构断言都覆盖不到"的开放维度）。

#### 任务 3.2：Judge 偏差控制

- 对同一对 (golden, generated) 跑 3 次评分取平均
- 随机化 golden 和 generated 的展示顺序（防止位置偏差）
- 记录 Judge 使用的模型和温度

**验收**：对 golden 对 golden（同一首乐谱，满分预期）跑一次 Judge，验证评分链路可用。

**当日结束验收**：
1. Judge prompt 设计完成
2. Judge 调用链路可用（至少单次评分成功）
3. 评分结果包含 5 个维度分数 + 理由

---

### M4 Day 4：H3 人工验收模板 + 跑测评

**时间**：4-6 小时

#### 任务 4.1：设计人工验收模板

**做什么**：创建一个 checklist 模板，用于抽查 20% 样本进行人工播放验收：

```markdown
# 人工验收记录

验收人: ______  日期: ______

| # | 测试ID | alda parse通过 | 播放成功 | 音乐性(1-5) | 备注 |
|---|--------|---------------|---------|------------|------|
| 1 | transcription_01 | [ ] | [ ] | ___ | |
| 2 | completion_03 | [ ] | [ ] | ___ | |
| ... | ... | ... | ... | ... | ... |

## 整体评价

- 是否有明显的乐理错误？___________
- 生成的乐谱是否可以直接用于演奏？___________
- 最大的改进方向是什么？___________
```

#### 任务 4.2：对 2-3 个配置跑测评

**做什么**：写一个评测脚本（`tests/eval/run_eval.sh` 或 Rust binary），自动化执行以下流程：

1. 对每个测试 prompt，调用 Agent 生成乐谱
2. 运行 H0 检查（alda parse）
3. 运行 H1 检查（结构断言）
4. 运行 H2 评分（LLM Judge）
5. 收集所有结果到 JSON 报告

**测试的配置**（至少 2 个）：
- 配置 A: Claude Sonnet + Anthropic provider
- 配置 B: GPT-4o / GPT-5 + OpenAI provider

#### 任务 4.3：生成评测报告

**报告模板**：
```markdown
# Alda Agent Evaluation Report

## 测试配置
- 测试集: 15 prompts (5 transcription + 5 completion + 5 migration)
- Agent 版本: {git_commit}
- 评测日期: {date}

## H0: 客观校验
| 配置 | parse 通过率 | 语法错误率 |
|------|-------------|-----------|
| A | 93.3% | 0.7% |
| B | 86.7% | 2.1% |

## H1: 结构断言
| 配置 | note_count | pitch_range | voice_count | ... | 综合 |
|------|-----------|-------------|-------------|-----|------|
| A | 87% | 93% | 100% | ... | 88% |
| B | 80% | 87% | 100% | ... | 82% |

## H2: LLM Judge
| 配置 | 结构 | 旋律 | 和声 | 节奏 | 整体 | 综合 |
|------|------|------|------|------|------|------|
| A | 3.8 | 3.5 | 3.6 | 3.7 | 3.5 | 3.62 |
| B | 3.5 | 3.2 | 3.0 | 3.5 | 3.1 | 3.26 |

## H3: 人工抽查
(待人工验收完成后填入)
```

**预期指标**（设计文档 §11）：
- H0 通过率 > 95%
- H1 结构断言通过率 > 80%

**当日结束验收**：
1. 评测脚本可自动化运行
2. 至少对 2 个配置完成完整测评
3. 评测报告含 H0/H1/H2 数据

---

### M4 Day 5：分析测评结果 + 改进

**时间**：4-6 小时

#### 任务 5.1：失败分析

**做什么**：对 H0 和 H1 中失败的 case 逐个分析：
- parse 失败：是 Agent 生成语法错误，还是模型没有正确理解 alda 语法？
- 结构断言失败：是哪个维度？是否可以调整断言阈值？
- LLM Judge 低分：是哪个维度？是否有共性问题？

**分析产出**：每个失败 case 的归因（模型能力边界 vs 提示词问题 vs 工具问题）。

#### 任务 5.2：根据测评结果改进

**低代价改进（优先做）**：
- 提示词微调（如 Agent 反复犯同样错误，在提示词中特别强调）
- 工具描述优化（如 score_analyze 的输出格式让模型更易理解）

**高代价改进（记录但 M5 再做）**：
- 添加新的工具（如 `alda_instruments` 让模型查询可用乐器）
- 模型参数调优
- 增加 few-shot examples 到系统提示词

**不做的改进**：
- 为了通过测评而"作弊"（如在提示词中 hardcode golden 乐谱内容）

#### 任务 5.3：更新评测报告

将改进后的结果补充到评测报告，形成对比（改进前 vs 改进后）。

**M4 完成验收**：
- 完整的 4 层评测数据（H0/H1/H2/H3）
- 失败 case 分析文档
- 至少一轮改进后的对比数据

---

## M5：上下文压缩 + 工程收尾（4 天）

**目标**：长对话场景的完整性，工程细节打磨。

**设计依据**：设计文档 §11 里程碑 M5。

---

### M5 Day 1：压缩效果测试与调优

**时间**：4-6 小时

#### 任务 1.1：长对话压力测试

**做什么**：构造 30 轮以上的长对话，验证压缩机制的实际效果。

**测试场景**：
```
流程: 创作一首完整的奏鸣曲 (sonata form)
- 呈示部 (8轮): 主题1 + 过渡 + 主题2
- 发展部 (8轮): 变形、转调、展开
- 再现部 (8轮): 主题回归
- 尾声 (6轮): 终止
每轮用户给具体反馈 ("主题1太短,扩到16小节", "过渡部分的模进不够流畅")
```

**观测指标**：
- 压缩触发了多少次？（自动触发 + 手动触发）
- 压缩前后 Agent 行为有无明显退化？
- 压缩后乐谱信息是否完整保留？
- token 使用量曲线

#### 任务 1.2：压缩质量评估

**做什么**：设计压缩质量评估方法：
1. 压缩前保存完整对话历史
2. 压缩后让 Agent 回答 5 个关于先前对话内容的"记忆测试"问题
3. 检查 Agent 是否正确记住了关键信息（调性、曲式、用户偏好）

**如果压缩导致信息丢失**：
- 检查 compact prompt 是否明确要求保留哪些信息
- 检查 `Message::System` 中注入的 score_state 是否完整
- 考虑增加保留的 user message 数量

**当日结束验收**：
1. 30+ 轮长对话场景完成
2. 压缩质量评估通过（记忆测试正确率 > 80%）

---

### M5 Day 2：错误恢复 + 边界情况

**时间**：4-6 小时

#### 任务 2.1：网络错误恢复

**做什么**：实现 `stream_with_retry()`（设计文档 §4.4）。测试场景：
- 模拟网络中断（用 `tc` 或 iptables 临时断网）
- 模拟 API 限流（429 Too Many Requests）
- 模拟服务端错误（500/502/503）

**重试策略**（§4.4）：
- 只重试 `RateLimited`、`ServerError`、`NetworkError`
- 指数退避：2^attempt * 100ms
- 最大重试 3 次
- `AuthError` 和 `Fatal` 不重试（直接报错）

#### 任务 2.2：alda 子进程异常处理

**测试场景**：
- alda 二进制不存在 → `ToolError::Fatal`
- alda 子进程超时 → `ToolError::RespondToModel`
- alda 子进程崩溃（SIGKILL）→ `ToolError::Fatal`
- 文件路径含特殊字符 → 路径安全检查拦截
- 输出 JSON 超大数据（>10MB）→ 截断 + 提示

#### 任务 2.3：最大 turn 迭代保护

**做什么**：验证 `MAX_TURN_ITERATIONS = 20` 的保护是否生效。构造一个场景（如反复给出矛盾的反馈），观察 Agent 是否在 20 轮后停止并警告。

#### 任务 2.4：JSONL 文件损坏恢复

**做什么**：模拟 JSONL 文件部分损坏（如进程崩溃导致最后一行不完整）。验证 resume 能否优雅处理：
- 损坏行跳过（打印 warning）
- 正常行正常恢复

**验收**：手动编辑 JSONL 文件删除最后几行，验证 resume 仍然成功（只是丢失最后几轮对话）。

**当日结束验收**：
1. 网络重试机制可用
2. 所有异常场景有合理的错误处理
3. JSONL 损坏恢复可用

---

### M5 Day 3：可观测性与文档

**时间**：4-6 小时

#### 任务 3.1：完善 tracing 日志

**做什么**：在关键路径添加 `tracing` 日志：
- `INFO`: turn 开始/结束、工具调用、compaction 触发
- `DEBUG`: SSE 事件详情、token usage 更新、子进程 stdout/stderr
- `WARN`: 重试、流异常退出、MAX_TURN_ITERATIONS 达到
- `ERROR`: 致命错误

验证 `RUST_LOG=debug cargo run` 能输出有意义的调试信息。

#### 任务 3.2：创建配置示例文件

**做什么**：在 `docs/` 下创建 `.env.example`：

```bash
# Alda Agent 环境变量配置

# 必需: API key (至少设置一个)
ANTHROPIC_API_KEY=sk-ant-...
# OPENAI_API_KEY=sk-...

# 可选: 模型选择（必须与 ALDA_AGENT_PROVIDER 匹配）
ALDA_AGENT_MODEL=claude-sonnet-5-20251001
ALDA_AGENT_PROVIDER=anthropic

# 可选: Alda 路径
ALDA_BINARY=alda

# 可选: 调试
ALDA_AGENT_SHOW_THINKING=0
RUST_LOG=info
```

#### 任务 3.3：更新代码注释

**做什么**：对关键类型和函数补全 doc comment (`///`)：
- `AgentLoop` — 简要说明双层循环结构
- `Session` — 说明 JSONL 格式和 resume 算法
- `Provider` trait — 说明两个 adapter 的职责
- `Tool` trait — 说明两态错误模型

运行 `cargo doc --open` 确认生成的文档可读。

**当日结束验收**：
1. 关键路径有合适的 tracing 日志
2. `.env.example` 包含所有配置项
3. `cargo doc` 生成可读的 API 文档

---

### M5 Day 4：最终验收 + 路线收尾

**时间**：4-6 小时

#### 任务 4.1：全量测试

**做什么**：运行全部测试：
```bash
cargo test --release
cargo clippy -- -D warnings
cargo fmt -- --check
```

修复所有问题。

#### 任务 4.2：最终端到端验收

**完整场景**（覆盖所有功能）：
```
1. 启动 REPL
2. 写一首完整的ABA三段体乐谱
3. 使用 alda_parse 验证
4. 使用 score_analyze 分析
5. 使用 play_for_human 播放
6. 给用户反馈（"B段太短"）
7. Agent 修改 B 段
8. 切换 provider 和 model
9. :compact 压缩
10. 退出
11. :resume 恢复
12. 继续修改
13. :quit 退出
```

#### 任务 4.3：学时统计与路线回顾

**做什么**：对照最初计划，统计实际用时：

| 里程碑 | 计划天数 | 实际天数 | 备注 |
|--------|---------|---------|------|
| M0 | 1-2 | ___ | |
| M1 | 5 | ___ | |
| M2 | 5 | ___ | |
| M3 | 5 | ___ | |
| M4 | 5 | ___ | |
| M5 | 4 | ___ | |
| **总计** | **25-26** | **___** | |

记录每个里程碑遇到的最大困难和解决方法，作为自学笔记。

#### 任务 4.4：（可选）多 crate 重构

**做什么**：如果此时代码量已经让单 crate 难以管理，按照设计文档 §10.2 重构成 workspace。

**不强制**——如果单 crate 仍然够用（<5000 行），不重构。这是设计文档的明确意图："当代码量增长到需要关注模块边界时"才重构。

**M5 完成验收**：
- 全部测试通过，无 clippy warning
- 最终端到端场景完整通过
- 压缩在长对话中有效工作
- 学时统计和路线回顾完成

---

## 风险点与缓解策略汇总

### 全局风险

| # | 风险 | 影响里程碑 | 概率 | 缓解策略 |
|---|------|-----------|------|---------|
| R0 | **alda 环境问题**（JVM/SoundFont/音频设备） | M0-M4 | 中 | M0 专项检查 `alda doctor`；`--no-audio` 模式不影响开发；CI 环境设 `ALDA_DISABLE_SPAWNING=yes` |
| R1 | **API key 不可用** | M1-M5 | 低 | 支持双 provider (Anthropic + OpenAI)；任一可用即可继续 |
| R2 | **Anthropic SSE 解析复杂度超预期** | M1 | 高 | M1 Day 2 有专门的 ToolCallAccumulator 实现日；参考设计文档 §4.2 和 §6.2 的详细规范；如果实在卡住，跳过 tool call 累积先实现文本流 |
| R3 | **模型不按预期调用工具** | M1-M2 | 高 | 提示词工程（base_instructions.md 明确工作流程）；检查工具描述是否清晰；必要时加 `tool_choice` 参数 |
| R4 | **上下文窗口比预期小** | M3-M5 | 中 | M3 的 compact 是核心缓解；`needs_compaction()` 阈值 80% 比 codex 的 90% 更保守 |
| R5 | **alda parse 输出格式变更** | M1-M4 | 低 | alda 2.x 的 parse 输出格式稳定；用固定版本号 |

### 里程碑特定风险

#### M1 风险

| # | 风险 | 缓解 |
|---|------|------|
| M1-1 | Rust async/tokio 不熟悉导致阻塞 | 阅读 tokio tutorial 的 "Hello tokio" 和 "Spawning" 两章（约 30 分钟）；记住：`async fn` 里用 `.await`，别用 `std::thread::sleep` |
| M1-2 | reqwest SSE 流解析复杂 | 先写一个独立的小脚本测试 SSE 解析，验证通过再集成 |
| M1-3 | Provider trait 的 `Pin<Box<dyn Stream>>` 语法不理解 | 这是 Rust 异步的高级语法——照抄设计文档的类型别名即可，不需要完全理解 |

#### M2 风险

| # | 风险 | 缓解 |
|---|------|------|
| M2-1 | OpenAI Responses API 与 Anthropic Messages API 差异大 | 两个 adapter 独立实现，不共享代码；先保证一个正确再写第二个 |
| M2-2 | 流式输出在 REPL 模式下出现缓冲 | `stdout().flush()` 在每个 delta 后调用；使用 `tokio::io::stdout()` 而非 `std::io::stdout()` |
| M2-3 | Ctrl+C 信号处理不优雅 | 使用 `tokio::signal::ctrl_c()` + `CancellationToken`；不要自己处理 SIGINT |

#### M3 风险

| # | 风险 | 缓解 |
|---|------|------|
| M3-1 | resume 算法实现出错 | 写大量单元测试：空文件、只有 session_meta、有 compacted 检查点、无检查点等多种情况 |
| M3-2 | compact 导致关键信息丢失 | M5 Day 1 专门测试压缩质量；紧凑 prompt 中明确列出必须保留的信息 |
| M3-3 | alda_cheatsheet.md 太长消耗过多 token | 约 200 行 ~2000 tokens，在 200K 窗口中可以忽略；如果担心，M5 可以改为按需检索 |

#### M4 风险

| # | 风险 | 缓解 |
|---|------|------|
| M4-1 | LLM Judge 评分不稳定 | 每个 case 跑 3 次取平均；随机化展示顺序；记录 Judge 模型和温度 |
| M4-2 | H1 断言规格难以校准 | 先用 golden 乐谱标定（golden 应该 100% 通过自己的断言）；如果 golden 都不通过，调整阈值 |
| M4-3 | 评测耗时过长 | 优先跑 H0（秒级）；H2 可选（LLM Judge 调用需要几十秒 per case） |

#### M5 风险

| # | 风险 | 缓解 |
|---|------|------|
| M5-1 | 长对话 token 累积超过预期 | compact 阈值设为 80% 已经偏保守；如果仍不足，M5 调整阈值到 70% |
| M5-2 | 不需要的多 crate 重构 | 明确条件：单 crate 代码超过 5000 行或单文件超过 500 行才重构 |

---

## 学时预估表

| 里程碑 | 内容 | 计划天数 | 对应设计文档节 |
|--------|------|---------|---------------|
| M0 | 环境验证 | 2 | §11（M0）, 附录 A, 附录 C |
| M1 | 最小 agent loop | 5 | §2-§7（核心类型到会话）, §11（M1） |
| M2 | REPL + 流式 + OpenAI | 5 | §4（Provider）, §6（流式）, §11（M2） |
| M3 | 提示词 + 持久化 | 5 | §3.2（提示词）, §7（持久化）, §11（M3） |
| M4 | Evaluation harness | 5 | §11（M4）, harness-engineering.md §9 |
| M5 | 压缩 + 工程收尾 | 4 | §7.3（压缩）, §11（M5） |
| **总计** | | **26 天** | |

**总代码量预估**（设计文档 §11）：
- M1: ~1200 行
- M2: ~2500 行（累计）
- M3: ~4000 行（累计）
- M4: ~5000 行（累计，含评测脚本）
- M5: ~5000 行（累计）

## 完成 M5 之后

M0–M5 交付的是“可测量的单 Agent 音乐创作基线”，不是完整的工作室平台。进入进阶阶段前，应同时满足：

- H0–H3 报告可复跑，并记录模型、prompt、工具、评测集和随机参数版本；
- 当前作品能从聊天历史之外恢复，压缩不会改变乐谱真相；
- 写文件和播放只允许受控路径、受控参数，并有超时与取消；
- 已记录单 Agent 的 token、墙钟时间和人工试听结果，以便未来公平比较；
- 团队确认接受[进阶架构](advanced-music-agent-architecture.md)中的 Profile、领域模型和迁移 ADR。

满足后按[进阶实施路线](advanced-implementation-roadmap.md)继续 M6–M12。进阶阶段首先重构状态边界和作品版本，不是先做多 Agent；多 Agent 必须通过同预算单 Agent 对照和盲听验证。

---

## 附录：环境依赖检查脚本

将以下脚本保存为 `scripts/check-env.sh`，在 M0 开始前运行。它会检查 M0 所需的所有外部依赖。依据 `alda-interfaces.md` §6。

```bash
#!/usr/bin/env bash
#
# Alda Agent Harness —— 环境依赖检查脚本
# 用法: bash scripts/check-env.sh
#
# 基于:
#   - docs/research/alda-interfaces.md §6 (运行环境依赖)
#   - docs/design/harness-design.md §11 (M0 环境验证)
#
# 退出码:
#   0 - 所有检查通过
#   1 - 有警告 (非阻塞, 可继续但功能受限)
#   2 - 有错误 (阻塞, 必须先修复)

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

ERRORS=0
WARNINGS=0

check_ok()   { echo -e "${GREEN}[OK]${NC}   $1"; }
check_warn() { echo -e "${YELLOW}[WARN]${NC} $1"; WARNINGS=$((WARNINGS + 1)); }
check_err()  { echo -e "${RED}[ERR]${NC}  $1"; ERRORS=$((ERRORS + 1)); }

# 从 stdin 读取并验证 JSON。返回 2 表示没有可用验证器。
validate_json() {
    if command -v python3 &>/dev/null; then
        python3 -m json.tool >/dev/null 2>&1
    elif command -v jq &>/dev/null; then
        jq empty >/dev/null 2>&1
    else
        return 2
    fi
}

echo "======================================"
echo " Alda Agent Harness 环境依赖检查"
echo "======================================"
echo ""

# ---- 1. Java / JVM ----
echo "--- 检查 Java / JVM ---"
if command -v java &>/dev/null; then
    JAVA_VERSION=$(java -version 2>&1 | head -1)
    check_ok "Java 已安装: $JAVA_VERSION"

    # 同时兼容 Java 8 的 "1.8.x" 与 Java 9+ 的 "17.x" 版本格式。
    JAVA_RAW=$(printf '%s\n' "$JAVA_VERSION" | sed -n 's/.*version "\([^"]*\)".*/\1/p')
    case "$JAVA_RAW" in
        1.*) JAVA_MAJOR=$(printf '%s\n' "$JAVA_RAW" | cut -d. -f2) ;;
        *)   JAVA_MAJOR=${JAVA_RAW%%.*} ;;
    esac
    if ! [[ "$JAVA_MAJOR" =~ ^[0-9]+$ ]]; then
        check_warn "无法识别 Java 主版本: $JAVA_RAW"
    elif [ "$JAVA_MAJOR" -lt 8 ]; then
        check_err "Java 版本 < 8 (当前: $JAVA_MAJOR)。Alda 2.4.3 需要 Java 8+。"
    fi
else
    check_err "Java 未安装。Alda player 是 JVM (Kotlin/Java) 程序，必须有 JRE。"
    echo "       安装: sudo apt install openjdk-17-jdk (Ubuntu)"
    echo "       安装: sudo pacman -S jdk-openjdk (Arch)"
    echo "       安装: brew install openjdk@17 (macOS)"
fi
echo ""

# ---- 2. alda CLI ----
echo "--- 检查 alda CLI ---"
if command -v alda &>/dev/null; then
    ALDA_VERSION=$(alda version 2>&1)
    check_ok "alda 已安装: $ALDA_VERSION"
else
    check_err "alda CLI 未安装或不在 PATH 中。"
    echo "       安装: 访问 https://alda.io/install"
fi
echo ""

# ---- 3. alda doctor (仅当 alda 可用) ----
echo "--- 运行 alda doctor --no-audio ---"
if command -v alda &>/dev/null; then
    # 无头环境使用 --no-audio 跳过音频相关检查
    if DOCTOR_OUTPUT=$(alda doctor --no-audio 2>&1); then
        check_ok "alda doctor 全部检查通过"
    else
        # doctor 在首个失败步骤返回非零；其状态标记是 "ERR"，没有方括号。
        check_err "alda doctor 执行失败"
        printf '%s\n' "$DOCTOR_OUTPUT" | sed 's/^/       /'
        echo ""
        echo "       常见解决方案:"
        echo "       - 如果 alda-player 未找到, 运行 'alda doctor' (不带 --no-audio) 让它自动下载"
        echo "       - 如果 player 状态卡在 'starting', 检查 SoundFont/MIDI 合成器"
        echo "       - Docker/CI 环境: 确保已安装 JRE"
    fi
else
    check_warn "跳过 alda doctor (alda 不可用)"
fi
echo ""

# ---- 4. alda parse JSON 输出验证 ----
echo "--- 验证 alda parse JSON 输出 ---"
if command -v alda &>/dev/null; then
    TMPFILE=$(mktemp)
    echo 'piano: c d e f g a b > c' > "$TMPFILE"
    if PARSE_OUTPUT=$(alda parse -f "$TMPFILE" -o data 2>/dev/null); then
        if printf '%s\n' "$PARSE_OUTPUT" | validate_json; then
            check_ok "alda parse -o data 输出合法 JSON"
        else
            status=$?
            if [ "$status" -eq 2 ]; then
                check_warn "缺少 python3/jq，跳过 data JSON 格式验证"
            else
                check_err "alda parse -o data 的输出不是合法 JSON"
            fi
        fi
    else
        check_err "alda parse -o data 执行失败"
    fi

    if PARSE_EVENTS=$(alda parse -f "$TMPFILE" -o events 2>/dev/null); then
        if printf '%s\n' "$PARSE_EVENTS" | validate_json; then
            check_ok "alda parse -o events 输出合法 JSON"
        else
            status=$?
            if [ "$status" -eq 2 ]; then
                check_warn "缺少 python3/jq，跳过 events JSON 格式验证"
            else
                check_err "alda parse -o events 的输出不是合法 JSON"
            fi
        fi
    else
        check_err "alda parse -o events 执行失败"
    fi

    rm -f "$TMPFILE"
else
    check_warn "跳过 alda parse 验证 (alda 不可用)"
fi
echo ""

# ---- 5. alda play 播放链 (跳过音频测试) ----
echo "--- 检查 alda play 可用性 (无音频) ---"
if command -v alda &>/dev/null; then
    if alda play --help > /dev/null 2>&1; then
        check_ok "alda play 命令可用"
        echo "         注意: 未实际测试音频播放。如需验证完整播放链，请手动运行:"
        echo "         alda play -c 'piano: c d e f g a b > c'"
    else
        check_err "alda play 命令不可用"
    fi
else
    check_warn "跳过 alda play 检查 (alda 不可用)"
fi
echo ""

# ---- 6. Rust 工具链 ----
echo "--- 检查 Rust 工具链 ---"
if command -v rustc &>/dev/null; then
    RUSTC_VERSION=$(rustc --version)
    check_ok "rustc 已安装: $RUSTC_VERSION"
    RUSTC_SEMVER=$(printf '%s\n' "$RUSTC_VERSION" | awk '{print $2}')
    RUSTC_MAJOR=${RUSTC_SEMVER%%.*}
    RUSTC_REST=${RUSTC_SEMVER#*.}
    RUSTC_MINOR=${RUSTC_REST%%.*}
    if [ "$RUSTC_MAJOR" -lt 1 ] || { [ "$RUSTC_MAJOR" -eq 1 ] && [ "$RUSTC_MINOR" -lt 85 ]; }; then
        check_err "rustc 版本过低。Rust 2024 edition 需要 1.85.0+。"
    fi
else
    check_err "rustc 未安装。"
    echo "       安装: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
fi

if command -v cargo &>/dev/null; then
    CARGO_VERSION=$(cargo --version)
    check_ok "cargo 已安装: $CARGO_VERSION"
else
    check_err "cargo 未安装。请通过 rustup 安装。"
fi
echo ""

# ---- 7. API Key ----
echo "--- 检查 API Key ---"
if [ -n "${ANTHROPIC_API_KEY:-}" ]; then
    check_ok "ANTHROPIC_API_KEY 已设置"
else
    check_warn "ANTHROPIC_API_KEY 未设置。Anthropic provider 将不可用。"
    echo "         设置: export ANTHROPIC_API_KEY=sk-ant-..."
fi

if [ -n "${OPENAI_API_KEY:-}" ]; then
    check_ok "OPENAI_API_KEY 已设置"
else
    check_warn "OPENAI_API_KEY 未设置。OpenAI provider 将不可用。"
    echo "         设置: export OPENAI_API_KEY=sk-..."
fi

if [ -z "${ANTHROPIC_API_KEY:-}" ] && [ -z "${OPENAI_API_KEY:-}" ]; then
    check_err "两个 API key 均未设置。至少需要一个 provider 的 API key。"
fi
echo ""

# ---- 8. 其他工具 (非阻塞) ----
echo "--- 检查可选工具 ---"
if command -v python3 &>/dev/null; then
    check_ok "python3 可用 (用于 JSON 格式验证)"
else
    check_warn "python3 未安装；若 jq 可用，JSON 验证会自动改用 jq。"
fi

if command -v jq &>/dev/null; then
    check_ok "jq 可用 (用于查看 JSONL 日志)"
else
    check_warn "jq 未安装。不影响核心功能，建议安装用于查看会话日志。"
fi
echo ""

# ---- 总结 ----
echo "======================================"
if [ $ERRORS -gt 0 ]; then
    echo -e "${RED}环境检查失败: $ERRORS 个错误, $WARNINGS 个警告${NC}"
    echo ""
    echo "请先修复以上错误再继续 M0。"
    echo "参考文档: docs/research/alda-interfaces.md §6"
    exit 2
elif [ $WARNINGS -gt 0 ]; then
    echo -e "${YELLOW}环境检查通过 (有 $WARNINGS 个警告)${NC}"
    echo ""
    echo "可以开始 M0，但警告项可能导致部分功能受限。"
    exit 1
else
    echo -e "${GREEN}所有环境检查通过!${NC}"
    echo ""
    echo "可以开始 M0: 环境验证。"
    echo "下一步: 按照实施路线 M0 步骤 0.7 创建 Rust 项目。"
    exit 0
fi
```

---

*本文档基于以下源文件编写：*
- `docs/design/harness-design.md` — 设计文档（尤其 §11 里程碑, §10 目录结构, 附录 A 依赖项, 附录 B ADR, 附录 C 子进程管理）
- `docs/research/alda-interfaces.md` — Alda 接口调研（尤其 §6 运行环境依赖, §2 CLI 子命令）
- `docs/research/harness-engineering.md` — Agent 工程综述（尤其 §2 Agent Loop, §3 Tool, §7 压缩, §9 Evaluation, §11 极简设计）
