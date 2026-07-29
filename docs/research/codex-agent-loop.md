# Codex Agent 主循环与 Turn 生命周期调研

## 调研范围

- 仓库: `codex-rs`
- 关键 crate: `exec`, `core`, `protocol`, `codex-api`
- 目录: `ref/codex/codex-rs/`
- 校验基准: commit `61a44880a85d2fd0d8770908dea5733495e571c8`

---

## 1. 架构概览

```
 CLI (exec)
   │
   ▼
 InProcessAppServerClient (进程内 JSON-RPC)
   │
   ▼
 AppServer ─────► Session::spawn ─────► CodexThread
                     │                      │
                     ▼                      │
              submission_loop               │
                     │                      │
                     ▼                      ▼
              user_input_or_turn ───► spawn_task (RegularTask)
                                           │
                                           ▼
                                       run_turn
                                           │
                                    ┌──────┴──────┐
                                    │  sampling    │
                                    │  loop        │
                                    │  (per turn)  │
                                    └──────┬──────┘
                                           │
                              ┌────────────┼────────────┐
                              ▼            ▼            ▼
                      build_prompt   stream()    handle_events
                              │         │              │
                              ▼         ▼              ▼
                         Prompt    SSE/JSON     ResponseEvent
                                   stream        │
                                        ┌────────┴────────┐
                                        ▼                 ▼
                                   ToolCall           Message
                                        │                 │
                                        ▼                 ▼
                               execute & append     emit to client
                               to history           (TurnComplete)
                                        │
                                        ▼
                                   needs_follow_up
                                   = true → loop
                                   = false → done
```

两队列模式: **Submission Queue (SQ)** + **Event Queue (EQ)** 是 Codex 协议的核心抽象。

- `protocol/src/protocol.rs:176` `struct Submission` — 客户端发往 agent 的请求
- `protocol/src/protocol.rs:522` `enum Op` — Submission 的负载类型
- `protocol/src/protocol.rs:1261` `struct Event` — agent 发回客户端的事件
- `protocol/src/protocol.rs:1279` `enum EventMsg` — Event 的负载类型

---

## 2. 用户输入如何变成模型请求 (CLI → exec → core)

### 2.1 CLI 解析 (`exec/src/main.rs:28-40`)

```rust
// exec/src/main.rs:28-40
fn main() -> anyhow::Result<()> {
    arg0_dispatch_or_else(|arg0_paths: Arg0DispatchPaths| async move {
        let top_cli = TopCli::parse();
        let mut inner = top_cli.inner;
        inner.config_overrides.prepend_root_overrides(top_cli.config_overrides);
        run_main(inner, arg0_paths).await?;
        Ok(())
    })
}
```

### 2.2 run_main → 加载配置 → 构建首轮输入 (`exec/src/lib.rs:240-789`)

`run_main` 位于 `exec/src/lib.rs:240`, 核心流程:

1. **解析 CLI 参数** (L250-265): 读取 `command`, `prompt`, `images`, `output_schema` 等
2. **加载配置** (L451-466): `ConfigBuilder` 加载 codex_home, cloud_config, etc
3. **确定 InitialOperation** (L730-788):
   ```rust
   // exec/src/lib.rs:730-788
   let (initial_operation, prompt_summary) = match (command.as_ref(), prompt, images) {
       (Some(ExecCommand::Resume(args)), root_prompt, imgs) => {
           // Resume: 构建 UserInput，加入 images
           let mut items: Vec<UserInput> = imgs.into_iter()...collect();
           items.push(UserInput::Text { text: prompt_text, .. });
           (InitialOperation::UserTurn { items, output_schema }, prompt_text)
       }
       (None, root_prompt, imgs) => {
           // 新建: 类似 Resume
           ...
       }
   };
   ```
4. **启动 InProcessAppServerClient** (L801): 进程内 client
5. **发送 `thread/start`** (L850-862): 获取 thread_id
6. **发送 `turn/start`** (L898): 将 `Vec<UserInput>` 作为 `TurnStartParams.input`
   ```rust
   // exec/src/lib.rs:898-927
   let response: TurnStartResponse = send_request_with_response(
       &client,
       ClientRequest::TurnStart {
           params: TurnStartParams {
               thread_id: primary_thread_id,
               input: items.into_iter().map(Into::into).collect(),
               ..defaults
           },
       },
       "turn/start",
   ).await?;
   ```

### 2.3 AppServer → Session 创建

AppServer 收到 `turn/start` 后调用 `Session::spawn` (`core/src/session/mod.rs:497`). Session 创建过程:

```rust
// core/src/session/mod.rs:521-773
async fn spawn_internal(args: SessionSpawnArgs) -> CodexResult<(Arc<Self>, SessionIo)> {
    // 1. 创建 SQ/EQ channel (L562-563)
    let (tx_sub, rx_sub) = async_channel::bounded(SUBMISSION_CHANNEL_CAPACITY);
    let (tx_event, rx_event) = async_channel::unbounded();

    // 2. 构建 Session (会话对象)
    let session = Arc::new(Session { .. });

    // 3. 启动 submission_loop (L770-773)
    let session_loop_handle = tokio::spawn(async move {
        submission_loop(session_for_loop, config, rx_sub).await;
    });

    // 4. 返回 SessionIo (L774-778)
    let io = SessionIo { tx_sub, rx_event, agent_status, session_loop_termination };
    Ok((session, io))
}
```

### 2.4 submission_loop → user_input_or_turn (`core/src/session/handlers.rs:692-838, 81-88`)

```rust
// handlers.rs:692-838 (simplified)
pub(super) async fn submission_loop(sess, config, rx_sub) {
    while let Ok(sub) = rx_sub.recv().await {
        match sub.op {
            Op::UserInput { .. } => {
                user_input_or_turn(&sess, sub.id, sub.op, sub.client_user_message_id).await;
            }
            Op::Interrupt => { interrupt(&sess).await; }
            Op::Shutdown => { /* break loop */ }
            // ..其他 op
        }
    }
}
```

`user_input_or_turn_inner` (`handlers.rs:176-262`) 调用 `sess.steer_input()` 将用户输入排入当前 turn 的 pending input, 然后调用 `spawn_task()`.

### 2.5 spawn_task → run_turn → build_prompt (`core/src/session/mod.rs:314`)

`spawn_task` (`tasks/mod.rs:314`) 最终调用 `turn.rs:151` 的 `run_turn`:

```rust
// turn.rs:151-158
pub(crate) async fn run_turn(sess, turn_context, turn_extension_data, input, ...) {
    // 1. pre-sampling compact (检查是否需要 compact)
    // 2. capture_step_context - 捕获当前 tool/environment 状态
    // 3. 构建 skills/plugins 注入
    // 4. 进入主循环 (L252: loop { ... })
}
```

主循环中 (`turn.rs:252`), 每次 sampling 前调用 `build_prompt` (`turn.rs:1143-1158`):

```rust
// turn.rs:1143-1158
pub(crate) fn build_prompt(input, router, turn_context, base_instructions) -> Prompt {
    Prompt {
        input,                                    // 完整 conversation history
        tools: router.model_visible_specs(),      // 可用工具 (MCP tools + built-in)
        parallel_tool_calls: model_info.supports_parallel_tool_calls,
        base_instructions,
        output_schema: turn_context.final_output_json_schema,
        output_schema_strict: !is_guardian_reviewer_source(...),
    }
}
```

`Prompt` 类型 (`client_common.rs:17-48`) 包含一次性模型请求的全部要素: 对话历史、可用工具列表、指令、output schema.

---

## 3. 模型流式响应如何解析为事件 (SSE/JSON → ResponseEvent → EventMsg)

### 3.1 stream() 调用 (`turn.rs:2035-2048`)

```rust
// turn.rs:2035-2048
let mut stream = client_session
    .stream(
        prompt,
        &turn_context.model_info,
        &turn_context.session_telemetry,
        turn_context.reasoning_effort,
        turn_context.reasoning_summary,
        turn_context.config.service_tier,
        responses_metadata,
        &inference_trace,
    )
    .await??;
```

`ModelClientSession::stream` 返回 `ResponseStream` (`client_common.rs:104-117`), 它是一个 `futures::Stream<Item = Result<ResponseEvent>>`.

### 3.2 ResponseEvent 枚举 (`codex-api/src/common.rs:76-123`)

这是底层 provider (OpenAI/Anthropic/etc) 数据归一化后的统一流事件:

```rust
// codex-api/src/common.rs:76-123
pub enum ResponseEvent {
    Created,                                    // 流已建立
    SafetyBuffering(SafetyBuffering),           // 安全审查缓冲
    OutputItemDone(ResponseItem),               // 一个输出项完成
    OutputItemAdded(ResponseItem),              // 一个输出项开始
    ServerModel(String),                        // 实际使用的模型 (可能被 reroute)
    ModelVerifications(Vec<ModelVerification>), // 账号验证建议
    TurnModerationMetadata(TurnModerationMetadataEvent), // 审核元数据
    ServerReasoningIncluded(bool),              // 服务端已计 reasoning tokens
    Completed {                                 // 模型本轮输出结束
        response_id: String,
        token_usage: Option<TokenUsage>,
        end_turn: Option<bool>,                 // 模型是否主动结束 turn
    },
    OutputTextDelta(String),                    // 逐 token 的文本增量
    ToolCallInputDelta { item_id, call_id, delta },  // tool call 参数的增量
    ReasoningSummaryDelta { delta, summary_index },
    ReasoningSummaryDone { item_id, text, summary_index },
    ReasoningContentDelta { delta, content_index },
    ReasoningSummaryPartAdded { summary_index },
    RateLimits(RateLimitSnapshot),
    ModelsEtag(String),
}
```

### 3.3 try_run_sampling_request 事件处理循环 (`turn.rs:2068-2500+`)

`turn.rs:2005` 的 `try_run_sampling_request` 的核心是一个 `loop` (`turn.rs:2068`), 逐项处理 stream 中的 `ResponseEvent`:

```rust
// turn.rs:2068-2500 (简化)
loop {
    let event = stream.next().await?;
    match event {
        ResponseEvent::Created => {}
        ResponseEvent::OutputItemDone(item) => {
            // 核心: 处理完成的输出项
            let output_result = handle_output_item_done(&mut ctx, item, ...).await?;
            if let Some(tool_future) = output_result.tool_future {
                in_flight.push_back(tool_future);  // 工具调用后台执行
            }
            needs_follow_up |= output_result.needs_follow_up;
            // 记录最终 agent message
        }
        ResponseEvent::OutputItemAdded(item) => {
            // 项开始: 如果是 AgentMessage, 准备流式输出
            let turn_item = handle_non_tool_response_item(sess, ..., &item, plan_mode).await;
            active_item = Some(turn_item);
            active_item_is_streaming_to_client = stream_item_to_client;
        }
        ResponseEvent::OutputTextDelta(delta) => {
            // 流式文本: 分发给客户端
            emit_streamed_assistant_text_delta(sess, turn_context, ..., delta).await;
        }
        ResponseEvent::ToolCallInputDelta { call_id, delta } => {
            // 流式 tool call 参数: 增量展示
        }
        ResponseEvent::ReasoningSummaryDelta { delta, summary_index } => {
            // 流式推理摘要
        }
        ResponseEvent::Completed { response_id, token_usage, end_turn } => {
            // 模型输出完成
            sess.record_token_usage_info(&turn_context, token_usage).await;
            if let Some(false) = end_turn { needs_follow_up = true; }
            break Ok(SamplingRequestResult { needs_follow_up, last_agent_message });
        }
    }
}
```

**关键返回**: `SamplingRequestResult { needs_follow_up, last_agent_message }` (定义于 `turn.rs:1414-1417`)

---

## 4. Tool Call 如何被识别、执行、结果回传

### 4.1 识别 Tool Call (`stream_events_utils.rs:287-325` + `tools/router.rs:128-176`)

```
ResponseEvent::OutputItemDone(item)
    │
    ▼
handle_output_item_done()      // stream_events_utils.rs:287
    │
    ▼
ToolRouter::build_tool_call()  // tools/router.rs:128
```

`build_tool_call` (`router.rs:128-176`) 匹配 `ResponseItem` 变体:

```rust
// router.rs:128-176
pub fn build_tool_call(item: ResponseItem) -> Result<Option<ToolCall>, FunctionCallError> {
    match item {
        ResponseItem::FunctionCall { name, namespace, arguments, call_id, .. } => {
            Ok(Some(ToolCall {
                tool_name: ToolName::new(namespace, name),
                call_id,
                payload: ToolPayload::Function { arguments },
            }))
        }
        ResponseItem::CustomToolCall { name, namespace, input, call_id, .. } => {
            Ok(Some(ToolCall {
                tool_name: ToolName::new(namespace, name),
                call_id,
                payload: ToolPayload::Custom { input },
            }))
        }
        ResponseItem::ToolSearchCall { call_id, execution, arguments, .. } if execution == "client" => {
            Ok(Some(ToolCall {
                tool_name: ToolName::plain("tool_search"),
                call_id,
                payload: ToolPayload::ToolSearch { arguments },
            }))
        }
        _ => Ok(None), // 非 tool call 项 (如 Message, Reasoning)
    }
}
```

非 tool call 的项由 `handle_non_tool_response_item` (`stream_events_utils.rs:391-438`) 解析为 `TurnItem` 并发送给客户端.

### 4.2 执行 Tool Call

```rust
// stream_events_utils.rs:318-325
let cancellation_token = ctx.cancellation_token.child_token();
let tool_future: InFlightFuture<'static> = Box::pin(
    ctx.tool_runtime.clone().handle_tool_call(call, cancellation_token),
);
output.needs_follow_up = true;
output.tool_future = Some(tool_future);
```

`ToolCallRuntime::handle_tool_call` (`tools/parallel.rs:75`) 调度到具体的 tool handler.

### 4.3 结果回传到下一次模型请求

Tool 执行结果以 `ResponseInputItem::FunctionCallOutput` 的形式记录到 conversation history:

```rust
// stream_events_utils.rs:501-506 (response_input_to_response_item)
ResponseInputItem::FunctionCallOutput { call_id, output } => {
    Some(ResponseItem::FunctionCallOutput {
        id: None, call_id: call_id.clone(), output: output.clone(), ..
    })
}
```

这些 output 项目作为 assistant turn 历史的一部分, 在 next sampling request 时通过 `sess.clone_history().await.for_prompt(&modalities)` 包含在 `Prompt.input` 中发送给模型.

### 4.4 Follow-up 循环

```rust
// turn.rs:322-476 (run_turn 主循环)
match sampling_request_result {
    Ok((output, input)) => {
        let SamplingRequestResult { needs_follow_up, last_agent_message } = output;
        if model_needs_follow_up {
            // 模型调用了 tool → 继续循环
            sess.input_queue.accept_mailbox_delivery_for_current_turn(...).await;
        }
        // 检查 token 限制 → 可能触发 auto-compact
        // 检查 pending input (steer) → 注入下一轮
        if !needs_follow_up {
            // 模型发出 assistant message → turn 结束
            last_agent_message = sampling_request_last_agent_message;
            // 运行 stop hooks → 可能 block 导致继续
            break;
        }
        continue; // loop again
    }
    Err(err) if TurnAborted => return Err(err),
    Err(err) => { /* fatal error → break */ }
}
```

---

## 5. Turn 何时结束 (停止条件)

Turn 结束的条件位于 `run_turn`(`turn.rs:151`) 的主循环中 (`turn.rs:252-511`):

### 5.1 正常结束

**条件 A: `needs_follow_up == false`** (模型输出 assistant message 且没有 tool call)

```rust
// turn.rs:425-474
if !needs_follow_up {
    last_agent_message = sampling_request_last_agent_message;
    let stop_outcome = run_turn_stop_hooks(...).await;
    if stop_outcome.should_block {
        // stop hook 要求继续 → 注入 hook prompt, 继续循环
        continue;
    }
    if stop_outcome.should_stop {
        break;
    }
    // 运行 legacy after agent hook
    break; // ← 正常退出
}
```

**条件 B: `ResponseEvent::Completed { end_turn: Some(false) }`** (`turn.rs:2369-2372`) — 模型明确指示未结束, 强制 `needs_follow_up = true`

### 5.2 异常结束

**Turn Aborted** (用户中断):
- `CodexErr::TurnAborted` 异常传播, `run_turn` 返回 `Err` (`turn.rs:478-479`)
- `spawn_task` 的 `on_task_finished` 会处理

**Fatal Error** (`turn.rs:499-509`):
- 非可恢复错误: `sess.emit_turn_error_lifecycle(...)` + 发送 `ErrorEvent`
- 让用户继续对话 (`break` 而不是 `return Err`)

**Token Limit Reached** (`turn.rs:349, 395`):
- `token_limit_reached` 触发 `should_roll_over` → `run_auto_compact` → 继续循环
- 如果 compaction 失败 → 返回 error

### 5.3 Turn 完成后的处理

```rust
// tasks/mod.rs:401-450 (spawn_task)
let handle = tokio::spawn(async move {
    let task_result = task_for_run.run(...).await;
    sess.on_task_finished(ctx_for_finish, task_result).await;
});
```

`on_task_finished` 发出 `EventMsg::TurnComplete` (`protocol.rs:1986`):

```rust
// protocol.rs:1986-2009
pub struct TurnCompleteEvent {
    pub turn_id: String,
    pub last_agent_message: Option<String>,
    pub error: Option<ErrorEvent>,  // 如果失败
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub duration_ms: Option<i64>,
    pub time_to_first_token_ms: Option<i64>,
}
```

exec 端 (`exec/src/lib.rs:1010-1019`) 接收 `TurnCompleted` 通知:
```rust
// exec/src/lib.rs:1010-1019
ServerNotification::TurnCompleted(payload) if {
    matches!(payload.turn.status, TurnStatus::Failed | TurnStatus::Interrupted)
} => {
    error_seen = true;
}
```

---

## 6. 错误与用户中断处理

### 6.1 用户中断 (Ctrl+C)

exec 端注册 SIGINT handler (`exec/src/lib.rs:885-891`):

```rust
// exec/src/lib.rs:885-891
let (interrupt_tx, mut interrupt_rx) = mpsc::unbounded_channel::<()>();
tokio::spawn(async move {
    if tokio::signal::ctrl_c().await.is_ok() {
        let _ = interrupt_tx.send(());
    }
});
```

主事件循环 (`exec/src/lib.rs:967-1058`) 使用 `tokio::select!` 响应中断:

```rust
// exec/src/lib.rs:967-993
loop {
    let server_event = tokio::select! {
        maybe_interrupt = interrupt_rx.recv(), if interrupt_channel_open => {
            // 发送 TurnInterrupt 请求
            client.request_typed::<TurnInterruptResponse>(
                ClientRequest::TurnInterrupt { params: TurnInterruptParams { thread_id, turn_id } }
            ).await;
            continue;
        }
        maybe_event = client.next_event() => maybe_event,
    };
    // ...
}
```

Session 端的处理链路:

1. `Op::Interrupt` → `handlers.rs:704` → `interrupt(&sess).await`
2. `interrupt` → `sess.interrupt_task()` → `abort_all_tasks(TurnAbortReason::Interrupted)`
3. `abort_all_tasks` (`tasks/mod.rs:509`) 取消 `CancellationToken` → `run_turn` 中的 stream 被取消
4. `try_run_sampling_request` 捕获 `Cancelled` → `CodexErr::TurnAborted` (`turn.rs:2091-2092`)

### 6.2 Stream 错误重试

```rust
// turn.rs:1198-1267 (run_sampling_request)
loop {
    let err = match try_run_sampling_request(...).await {
        Ok(output) => return Ok((output, original_input.unwrap_or(prompt.input))),
        Err(err) => match err.details() {
            CodexErrorDetails::ContextWindowExceeded => return Err(err),  // 不重试
            CodexErrorDetails::UsageLimitReached(e) => return Err(err),   // 不重试
            _ => err,  // fall through to retry logic
        },
    };
    if !err.is_retryable() { return Err(err); }
    handle_retryable_response_stream_error(
        &mut retries, max_retries, err, client_session, &sess, &turn_context,
        ResponsesStreamRequest::Sampling,
    ).await?;
    turn_context.turn_timing_state.record_sampling_retry();
}
```

### 6.3 Image Invalid Error

```rust
// turn.rs:481-497
Err(codex_error) if matches!(codex_error.details(), CodexErrorDetails::InvalidImageRequest()) => {
    sess.emit_turn_error_lifecycle(turn_context.as_ref(), CodexErrorInfo::BadRequest).await;
    sess.send_event(&turn_context, EventMsg::Error(ErrorEvent {
        message: "Invalid image in your last message. Please remove it and try again.",
        codex_error_info: Some(CodexErrorInfo::BadRequest),
    })).await;
    break;
}
```

### 6.4 Submission Channel 关闭

```rust
// handlers.rs:840-849
if !shutdown_received {
    shutdown_session_runtime(&sess).await;
    emit_thread_stop_lifecycle(sess.as_ref()).await;
    if let Some(live_thread) = sess.live_thread()
        && let Err(err) = live_thread.shutdown().await
    { /* warn */ }
}
```

---

## 7. 时序图 (Mermaid SequenceDiagram)

```mermaid
sequenceDiagram
    actor User
    participant CLI as exec main
    participant AppServer as AppServer
    participant Sess as Session
    participant Loop as submission_loop
    participant Task as RegularTask(spawn)
    participant Turn as run_turn
    participant Model as LLM Provider
    participant Tool as Tool Router
    participant Client as Event Consumer (exec)

    User->>CLI: codex exec "write a test"
    CLI->>CLI: parse CLI args
    CLI->>CLI: load config
    CLI->>AppServer: thread/start
    AppServer->>Sess: Session::spawn
    Sess->>Sess: create SQ/EQ channels
    Sess-->>CLI: SessionConfigured { thread_id, .. }

    CLI->>AppServer: turn/start { items: [UserInput], .. }
    AppServer->>Sess: enters submission_loop via SQ channel
    Loop->>Loop: recv Submission { op: UserInput }
    Loop->>Loop: user_input_or_turn_inner

    Note over Loop,Sess: steer_input → enqueue to pending_input
    Loop->>Sess: spawn_task(RegularTask)

    Sess->>Task: tokio::spawn
    Sess-->>Client: Event { msg: TurnStarted }

    Task->>Turn: run_turn(sess, turn_context, input, ...)

    rect rgb(240, 248, 255)
        Note over Turn: pre-sampling compact
    end

    Turn->>Turn: capture_step_context
    Turn->>Turn: build_prompt(input, tools, instructions)
    Turn->>Model: client_session.stream(prompt)

    loop Sampling Request (per model call, may repeat)
        Model-->>Turn: SSE stream → ResponseEvent
        Turn->>Turn: match ResponseEvent

        alt OutputItemDone (message)
            Turn->>Turn: handle_non_tool_response_item → TurnItem
            Turn-->>Client: EventMsg::AgentMessageContentDelta (streaming)
            Turn->>Turn: last_agent_message = text
            Note over Turn: needs_follow_up stays false

        else OutputItemDone (function_call)
            Turn->>Tool: handle_output_item_done → ToolCall
            Tool->>Tool: ToolRouter::build_tool_call
            Tool->>Tool: ToolCallRuntime::handle_tool_call
            Tool-->>Turn: FunctionCallOutput
            Turn->>Turn: record to history
            Note over Turn: needs_follow_up = true

        else Completed
            Turn->>Turn: record_token_usage_info
            Model-->>Turn: end_turn: Some(true/false)
            Note over Turn: break inner loop
        else OutputTextDelta
            Turn-->>Client: stream text to UI

        else Error
            Turn->>Turn: check retryable → retry or return error
        end
    end

    alt needs_follow_up == true
        Turn->>Turn: accept mailbox delivery
        Turn->>Turn: check token limit → maybe compact
        Turn->>Turn: continue outer loop (next sampling request)
    else needs_follow_up == false
        Turn->>Turn: run stop hooks
        Turn->>Turn: break outer loop (turn done)
    end

    Turn-->>Task: return Ok(last_agent_message)
    Task->>Sess: on_task_finished
    Sess-->>Client: Event { msg: TurnComplete { last_agent_message, .. } }

    Client->>Client: event_processor.process_server_notification
    rect rgb(255, 245, 240)
        Note over Client: if last_message_file: write output
    end
    alt SIGINT (user Ctrl+C)
        Client->>AppServer: turn/interrupt
        AppServer->>Sess: Op::Interrupt
        Sess->>Sess: abort_all_tasks(Interrupted)
        Sess-->>Turn: CancellationToken cancelled
        Turn-->>Task: Err(CodexErr::TurnAborted)
        Task->>Sess: on_task_finished
        Sess-->>Client: TurnComplete { error: Some(...) }
    end

    opt error_seen in event loop
        Client->>Client: std::process::exit(1)
    end

    Client->>AppServer: thread/unsubscribe (shutdown)
    AppServer->>Sess: Op::Shutdown
    Loop->>Loop: break submission loop
    Loop->>Sess: teardown: shutdown_session_runtime + emit_thread_stop_lifecycle
```

---

## 8. 我们的 mini harness 应借鉴什么、砍掉什么

### 8.1 Alda Agent 场景

用户给乐谱需求 → LLM 生成 alda 代码 → alda parse 校验 → 如果失败, 错误信息回传 LLM 修正 → 循环 → 成功则输出最终 .alda 文件

### 8.2 应借鉴

| 借鉴点 | Codex 对应位置 | Alda harness 做法 |
|--------|---------------|-------------------|
| **两通道抽象 (SQ/EQ)** | `protocol.rs:176` Submission, `protocol.rs:1261` Event | 保留: 一个 request channel + 一个 event channel. 不需要完整的 Op/EventMsg 枚举, 但模式很有用. `AgentRequest`/`AgentEvent` 两个简单枚举即可 |
| **Prompt struct** | `client_common.rs:17-48` | 借鉴: Prompt { input(items), tools(specs), instructions }. 比手工拼字符串干净得多 |
| **run_turn 的主循环结构** | `turn.rs:252` loop { sampling → tool → continue | break } | 直接借鉴: while need_follow_up { response = llm.stream(prompt); match response { ToolCall => execute & append; Message => break } } |
| **Tool call 解析 + 执行分离** | `tools/router.rs:128` build_tool_call, `stream_events_utils.rs:287` handle_output_item_done | 借鉴: LLM 返回的 JSON 先解析为 ToolCall 枚举 → dispatch → 结果格式化为 conversation item |
| **CancellationToken 驱动的中断** | `turn.rs:2087-2092` | 必须借鉴: Ctrl+C 时 cancel token → stream 中断 → 优雅退出 |
| **Stream 重试** | `turn.rs:1252-1266` | 可选借鉴: 可重试错误 vs 致命错误的区分. 对 alda 场景可能不需要 |
| **ResponseEvent 概念** | `codex-api/src/common.rs:76-123` | 借鉴思想: LLM stream 的每个 chunk 归一化为统一事件枚举 (TextDelta, ToolCallDone, Completed). 不要在不同 provider 之间处理 SSE 差异 |
| **steer/inject 模式** | `session/mod.rs:3975` steer_input, `session/inject.rs` inject_if_running | 借鉴: 允许在 turn 运行期间注入新信息 (如 alda parse 错误 → 注入 developer message → 继续 turn) |

### 8.3 应砍掉

| 砍掉点 | 理由 |
|--------|------|
| **SQ/EQ 的 512 容量 channel + async_channel** | Alda harness 是单线程/单任务场景, 用 `tokio::sync::mpsc` 或甚至简单的 `VecDeque` + `Mutex` 即可 |
| **Op 枚举的 35+ 变体** | 只需要 `UserInput`, `Interrupt`, `Shutdown` (或更少) |
| **EventMsg 枚举的 60+ 变体** | 只需要 `TurnStarted`, `AgentMessage`, `ToolCall`, `TurnCompleted`, `Error` |
| **AppServer/InProcessClient JSON-RPC 层** | Alda harness 不需要独立的 server-client 架构. agent 循环直接在主进程中运行 |
| **Thread/Session 的持久化 (rollout/thread_store)** | 不需要持久化. 如果以后需要, 可以简单 dump JSON |
| **Multi-agent / SubAgent** | 不在 scope 内 |
| **MCP / dynamic tools / connectors / plugins** | 不需要. Tool 只有 `run_alda_parse` 一个 (或者未来加 `run_alda_play` 等) |
| **Realtime conversation (audio/video)** | 砍掉 |
| **Guardian / sandbox / approval 系统** | 砍掉. alda parse 的输出是纯文本, 没有安全问题 |
| **Hook 系统 (PreToolUse/PostToolUse/...)** | 不需要 |
| **Auto-compact / token budget** | Alda 场景对话极短 (通常 2-5 轮), 不需要 |
| **Context window management** | 暂时不需要. 对话长度可控 |
| **Model provider 抽象层 (connector/inference_trace/rate_limits/...)** | 只需要单一的 API client (如 Anthropic Messages API 或 OpenAI Chat Completions). 用 reqwest + SSE parser 即可 |
| **Extension API / Plugin system** | 不需要 |
| **SessionConfiguration/Config 的复杂层级** | 只需要 model: String, max_tokens: u32, alda_binary_path: PathBuf 几个配置项 |
| **W3C trace context / OpenTelemetry** | 暂时不需要 |

### 8.4 建议的 mini harness 核心结构

```
AldaAgent {
    config: AldaConfig,
    history: Vec<Message>,       // conversation history
    alda_path: PathBuf,          // alda binary
}

loop {                          // 主循环 (类似 submission_loop)
    prompt = read_user_input()  // 或来自 CLI args
    history.push(user_msg)

    while follow_up {           // 类似 run_turn 的内层循环
        request = build_request(&history)
        stream = llm.stream(request)

        for event in stream {
            match event {
                TextDelta(t) → print(t),
                ToolCall { name: "alda_parse", args } => {
                    result = alda parse(args.code)
                    history.push(tool_result(result))
                    if result.ok { follow_up = false }
                    else { follow_up = true /* 让 LLM 修正 */ }
                },
                Completed => follow_up = false,
            }
        }
    }

    // turn 完成, 输出最终结果
}
```

**总行数预估**: 所有 agent 逻辑 < 500 行 Rust. 比 Codex 的 10w+ 行精简 200 倍.

### 8.5 一个关键洞察

Codex 大量复杂性来自:

1. **多 client 并发**: CLI + VS Code + Web 同时操作同一 session → 需要 AppServer + channel 架构
2. **多 provider 抽象**: OpenAI, Anthropic, 自建等 → responses_retry, provider info
3. **安全 sandbox**: 执行任意 shell 命令 → 需要 guardian, approvals
4. **长对话管理**: compact, token budget, context window
5. **Hook/Plugin 系统**: 第三方扩展

Alda harness 是 **单用户, 单 session, 单 provider, 无 sandbox, 短对话, 无扩展** 场景. 砍掉上述全部之后, 剩下的核心就是一个 50 行的 `while loop` + tool dispatch. 这正是我们从 Codex 中学到的最有价值的东西.
