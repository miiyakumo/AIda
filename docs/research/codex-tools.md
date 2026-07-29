# codex 工具系统调研

> 调研对象:`ref/codex/codex-rs/`(下文所有路径省略该前缀)。校验基准: commit `61a44880a85d2fd0d8770908dea5733495e571c8`。
> 目的:为 alda mini agent harness 的工具子系统设计提供参照。所有引用均为亲自查证的 文件路径:行号。

## 0. 总览:两层架构

codex 的工具系统分为两层:

1. **`tools/` crate(包名 `codex-tools`)** —— 与宿主无关的通用抽象:
   - `ToolSpec` / `ResponsesApiTool` / `FreeformTool`(模型可见的工具声明,`tools/src/tool_spec.rs:19`、`tools/src/responses_api.rs:26`)
   - `JsonSchema`(手写的 JSON Schema 子集,`tools/src/json_schema.rs:41`)
   - `ToolExecutor` trait(spec 与执行绑定,`tools/src/tool_executor.rs:49`)
   - `ToolPayload` / `ToolOutput` / `FunctionCallError`(输入、输出、错误的统一形态)
2. **`core/src/tools/`** —— 宿主运行时:
   - `registry.rs`(注册表 + 分发 + hooks 插入)、`router.rs`(模型响应 → 工具调用)、`spec_plan.rs`(按配置组装工具集)、`orchestrator.rs` + `approvals.rs` + `sandboxing.rs`(审批与沙箱)、`parallel.rs`(并行与取消)
   - `handlers/` 目录基本"一个工具一个文件",并配对一个 `*_spec.rs` 声明文件(见 `core/src/tools/handlers/mod.rs:1-36` 的模块清单):`shell.rs`/`shell_spec.rs`、`plan.rs`/`plan_spec.rs`、`apply_patch.rs`/`apply_patch_spec.rs`、`unified_exec/`、`mcp.rs`、`view_image.rs`/`view_image_spec.rs` 等。

一次工具调用的完整链路:

```
模型流式输出 ResponseItem
  → ToolRouter::build_tool_call            (router.rs:128, 解析出 ToolCall)
  → ToolCallRuntime::handle_tool_call      (parallel.rs:75, 并行门 + 取消)
  → ToolRouter::dispatch_*                 (router.rs:203, 构造 ToolInvocation)
  → ToolRegistry::dispatch_any_with_terminal_outcome (registry.rs:427)
       ├─ PreToolUse hooks(可拦截/改写输入)
       ├─ handler.handle(invocation)       (各 handler; exec 类内部再走 ToolOrchestrator: 审批→沙箱→重试)
       └─ PostToolUse hooks(可拒绝结果/追加反馈)
  → ToolOutput::to_response_item → FunctionCallOutput 回注模型下一轮输入
```

---

## 1. JSON Schema 如何定义与生成

### 1.1 ToolSpec:五种模型可见形态

`tools/src/tool_spec.rs:19` 定义 `ToolSpec`,serde 直接序列化成 OpenAI Responses API 的 Tool JSON(`#[serde(tag = "type")]`):

- `Function(ResponsesApiTool)` —— 普通函数工具(绝大多数);
- `Namespace(ResponsesApiNamespace)` —— 一组函数打包进命名空间(MCP、multi-agent v2 用);
- `ToolSearch { .. }` —— 延迟加载工具的检索入口;
- `WebSearch { .. }` —— 平台托管工具(无本地 handler);
- `Freeform(FreeformTool)` —— 自由文本工具,由 **Lark 语法**约束(apply_patch 用,`tools/src/responses_api.rs:12`)。

`ResponsesApiTool`(`tools/src/responses_api.rs:26`)字段:`name`、`description`、`strict: bool`、`defer_loading: Option<bool>`、`parameters: JsonSchema`、`output_schema: Option<Value>`(`#[serde(skip)]`,只用于 code-mode 提示,不发给 API)。发送前经 `create_tools_json_for_responses_api`(`tools/src/tool_spec.rs:79`)逐个 `serde_json::to_value`。

### 1.2 手写 JsonSchema builder(内建工具的做法)

`tools/src/json_schema.rs:41` 的 `JsonSchema` 是一个受限 JSON Schema 子集(对齐 OpenAI Structured Outputs 支持的类型,见 `json_schema.rs:12-18` 注释),提供 `string() / number() / boolean() / string_enum() / array() / object()` 等构造器(`json_schema.rs:110-165`)。

内建工具的 schema **不是**用宏或 derive 生成,而是在 `*_spec.rs` 里手写 `BTreeMap<String, JsonSchema>`。典型例子:

- `exec_command`:`core/src/tools/handlers/shell_spec.rs:21-111`。properties 含 `cmd`(必填)、`workdir`、`tty`、`yield_time_ms`、`max_output_tokens`,并按配置条件插入 `shell`、`login`、`environment_id` 与审批参数(`sandbox_permissions`/`justification`/`prefix_rule`,`shell_spec.rs:298-344`)。还带 `output_schema`(`shell_spec.rs:264-296`,声明 `chunk_id/wall_time_seconds/exit_code/session_id/original_token_count/output`)。
- `update_plan`:`core/src/tools/handlers/plan_spec.rs:7-58`,`plan` 数组项为 `{step, status}`,`status` 是 `string_enum(["pending","in_progress","completed"])`。
- `shell_command`:`shell_spec.rs:157-225`,`command`(必填)+`workdir`+`timeout_ms`,description 里直接写使用规范("Always set the `workdir` param … Do not use `cd`")。

要点:**schema 描述文本本身就是 prompt engineering 的一部分**(默认值、取值范围、平台差异都写在 description 里,如 `shell_spec.rs:26-30` 针对 Windows 的 yield_time_ms 说明);`strict` 一律为 `false`。

### 1.3 外部 schema(MCP / dynamic tools)的净化管线

外部工具带来的任意 JSON Schema 要先过 `parse_tool_input_schema`(`tools/src/json_schema.rs:189-193`)三步:

1. `sanitize_json_schema`(`json_schema.rs:466`):补 `type`、`const`→单值 `enum`、bool schema→string、为 object/array 补默认 `properties`/`items` 等,把任意 schema 压进自家子集;
2. `prune_unreachable_definitions`(`json_schema.rs:593`):删掉 `$defs/definitions` 中不可达项,省 token;
3. `compact_large_tool_schema`(`json_schema.rs:229`):超过 5000 字节预算(`json_schema.rs:222`,约 1k token 的代理指标)就按 4 个递进有损 pass 压缩——去 description、丢 definitions、折叠深度>3 的复杂对象、剪掉 anyOf/oneOf/allOf(`json_schema.rs:240-245`)。

MCP 工具再经 `mcp_tool_to_responses_api_tool`(`tools/src/responses_api.rs:107`)转成 `ResponsesApiTool`。

### 1.4 Freeform:apply_patch 的 Lark 语法

apply_patch 不用 JSON 参数,而是把 patch 文本语法作为 Lark grammar 内嵌:`core/src/tools/handlers/apply_patch_spec.rs:5` `include_str!("apply_patch.lark")`,语法定义 `*** Begin Patch / *** Add File: / *** Update File: / *** End Patch` 等(`core/src/tools/handlers/apply_patch.lark:1-16`)。description 明确提示 "This is a FREEFORM tool, so do not wrap the patch in JSON"(`apply_patch_spec.rs:20`)。

### 1.5 specs 如何进入模型请求

每个 turn 由 `build_tool_router`(`core/src/tools/spec_plan.rs:158`)产出 `(model_visible_specs, registry)`;`build_prompt`(`core/src/session/turn.rs:1143-1159`)把 `router.model_visible_specs()` 填进 `Prompt.tools`(turn.rs:1151),连同 `parallel_tool_calls` 开关一起发给 Responses API。同名 Namespace 会被 `merge_into_namespaces` 合并排序(`spec_plan.rs:539-583`)。

---

## 2. 注册:spec 与 handler 绑定在同一对象

### 2.1 两级 trait

- `ToolExecutor<Invocation>`(`tools/src/tool_executor.rs:49-69`):`tool_name()`、`spec()`、`exposure()`、`search_info()`、`supports_parallel_tool_calls()`(默认 `false`)、`handle(invocation) -> ToolExecutorFuture`。**spec 与执行逻辑在同一 struct 上**,注册即声明,杜绝 schema 与解析代码漂移。
- `CoreToolRuntime: ToolExecutor<ToolInvocation>`(`core/src/tools/registry.rs:52-149`):core 专属扩展点——`matches_kind`(payload 类型校验)、`waits_for_runtime_cancellation`、`telemetry_tags`、`pre_tool_use_payload` / `post_tool_use_payload`(hook 契约)、`with_updated_hook_input`(hook 改写入参)、`create_diff_consumer`(流式参数 diff → UI 事件,apply_patch 用,`core/src/tools/handlers/apply_patch.rs:78-101`)。

最简 handler 只需 `impl ToolExecutor` + 空的 `impl CoreToolRuntime for PlanHandler {}`(`core/src/tools/handlers/plan.rs:99`)。

### 2.2 曝光级别 ToolExposure

`tools/src/tool_executor.rs:15-36`:`Direct`(进初始工具列表)、`Deferred`(注册但不进列表,靠 `tool_search` 检索后加载)、`DirectModelOnly`(仅直连,不进 code-mode)、`Hidden`(仅可分发不可见——如 unified_exec 开启时,旧 `shell_command` 以 `add_dispatch_only` 保留兼容,`core/src/tools/spec_plan.rs:696-698`)。`override_tool_exposure`(registry.rs:242)用包装器改写曝光。

### 2.3 按配置条件化组装(spec_plan.rs)

`build_tool_specs_and_registry`(`spec_plan.rs:176-211`)依次:`add_tool_sources`(608)→ direct-only namespace 覆写 → 追加 tool_search executor(962)→ 前插 code-mode executor → `build_model_visible_specs_and_registry`(243)。注册全部由 feature/config/模型能力驱动,例如:

- shell 形态三选一(`spec_plan.rs:683-706`):模型要求 `UnifiedExec` → 注册 `exec_command`+`write_stdin`;否则注册 `shell_command`;`Disabled` 则没有 shell;
- `update_plan` 仅当 `config.update_plan_enabled`(`spec_plan.rs:736-738`);
- `apply_patch` 仅当 `model_info.apply_patch_tool_type` 存在(`spec_plan.rs:798-802`);
- MCP resource 三件套仅当有 MCP server(`spec_plan.rs:722-728`)。

`ToolRegistry` 本体就是 `HashMap<ToolName, Arc<dyn CoreToolRuntime>>`(`registry.rs:326-328`),`from_tools` 对重名 `error_or_panic`(registry.rs:341)。`ToolName` 是 `{name, namespace: Option<String>}`(`protocol/src/tool_name.rs:9`)。

---

## 3. 路由:模型 tool call → handler

### 3.1 解析:ResponseItem → ToolCall

`ToolRouter::build_tool_call`(`core/src/tools/router.rs:128-176`)对三种响应项分别产出 `ToolCall { tool_name, call_id, payload }`(router.rs:32):

| ResponseItem | ToolPayload(`tools/src/tool_payload.rs:7-11`) | 说明 |
|---|---|---|
| `FunctionCall { name, namespace, arguments }` | `Function { arguments: String }` | arguments 是**原始 JSON 字符串**,交给 handler 自己 serde 解析 |
| `ToolSearchCall`(execution=="client") | `ToolSearch { arguments }` | 延迟工具检索 |
| `CustomToolCall { input }` | `Custom { input: String }` | freeform 工具(apply_patch)的裸文本 |

### 3.2 分发与并行门

流式回调 `handle_output_item_done`(`core/src/stream_events_utils.rs:295`)拿到 ToolCall 后交 `ToolCallRuntime::handle_tool_call`(stream_events_utils.rs:316-322;`core/src/tools/parallel.rs:75`)。关键机制:

- **并行门**:一个 `RwLock<()>`——`supports_parallel_tool_calls()==true` 的工具拿读锁并发跑,否则拿写锁独占(`parallel.rs:133-137`)。默认不支持并行;`exec_command` 支持(`core/src/tools/handlers/unified_exec/exec_command.rs:96`),MCP 工具按 `read_only_hint` 或 server 声明(`core/src/tools/handlers/mcp.rs:76-87`);
- **取消**:每个调用一个 `CancellationToken` 子 token,`tokio::select!` 竞争;取消时按 `waits_for_runtime_cancellation` 决定等 teardown 还是直接 abort,并回一条 `AbortedToolOutput`("aborted by user…",`parallel.rs:160-199`、237-259);
- **未知工具**:查不到注册项 → `FunctionCallError::RespondToModel("unsupported call: …")`(registry.rs:464-483),让模型自纠。

### 3.3 registry.dispatch:hooks 的插入点

`dispatch_any_with_terminal_outcome`(registry.rs:427)在 handler 前后织入:

1. **PreToolUse hooks**(registry.rs:517-561):可 `Blocked(message)`(→ RespondToModel,工具不执行)或 `Continue { updated_input }`(经 `with_updated_hook_input` 改写 arguments 再执行);
2. handler 执行(`handle_any_tool`,registry.rs:724);
3. **PostToolUse hooks**(registry.rs:613-644):可 `should_block`(丢弃结果,`PostToolUse hook blocked the tool result` 回给模型,registry.rs:676-684)或注入 `feedback_message`(用 `PostToolUseFeedbackOutput` 替换模型可见输出,原始输出仍供日志/code-mode,registry.rs:685-693)。

---

## 4. handler 的输入、输出与错误形态

### 4.1 输入:ToolInvocation

`core/src/tools/context.rs:59-70`:`session: Arc<Session>`、`turn: Arc<TurnContext>`、`step_context`、`cancellation_token`、`tracker`(turn 级文件 diff 追踪)、`call_id`、`tool_name`、`source`(Direct / CodeMode)、`payload`。参数解析统一走 `parse_arguments::<T>`(serde,`core/src/tools/handlers/mod.rs:83`),失败即 `RespondToModel("failed to parse function arguments: …")`(如 `plan.rs:101-105`)。

### 4.2 输出:ToolOutput trait + 若干具体类型

`tools/src/tool_output.rs:16-53` 定义 `ToolOutput`:核心是 `to_response_item(call_id, payload) -> ResponseInputItem`(转成回注模型的 `FunctionCallOutput` / `CustomToolCallOutput` 等),外加 `log_preview()`(遥测预览,2KiB/64 行截断,context.rs:499-537)、`success_for_logging()`、`post_tool_use_*`(hook 契约)、`code_mode_result()`。具体实现:

- `FunctionToolOutput`(context.rs:191):文本/多段内容 + `success: Option<bool>`;
- `ExecCommandToolOutput`(context.rs:315-469):unified_exec 专用,响应文本按 `Chunk ID / Wall time / Process exited with code N / Process running with session ID N / Original token count / Output:` 分节,并做 **token 预算截断**(超限时加 `Warning: truncated output (original token count: N)`);
- `McpToolOutput`(context.rs:73-148):包 `CallToolResult`,前缀 `Wall time: … seconds\nOutput:`,按对话历史同款截断策略 ×1.2 缓冲;
- `ApplyPatchToolOutput`(context.rs:242)、`JsonToolOutput`(tool_output.rs:93)、`AbortedToolOutput`(context.rs:281)。

shell_command 的模型可见输出由 `format_exec_output_for_model`(`core/src/tools/mod.rs:78-103`)统一格式化:`Exit code: N` + `Wall time: X seconds` +(截断时)`Total output lines: N` + `Output:` + 截断正文;超时则前缀 `command timed out after N milliseconds`(mod.rs:116-126)。

### 4.3 错误:两态 FunctionCallError

`tools/src/function_call_error.rs:5-10` 只有两个变体,这是整个错误设计的核心:

- `RespondToModel(String)` —— **一切"模型可自救"的失败**:参数解析错、补丁校验错(`apply_patch verification failed: …`,apply_patch.rs:367)、审批被拒、hook 拦截、未知工具……统统变成 `FunctionCallOutput { success: Some(false) }` 回给模型继续下一轮(`parallel.rs:87` + 210-235);
- `Fatal(String)` —— 宿主级故障,升级为 `CodexErr::Fatal` 终止 turn(`parallel.rs:86`)。

### 4.4 exec 类工具的内层管线

`shell_command` handler(`core/src/tools/handlers/shell/shell_command.rs:42`)解析参数后调 `run_exec_like`(`core/src/tools/handlers/shell.rs:63`):先 `intercept_apply_patch`(shell.rs:142-156,识别 `["apply_patch","*** Begin Patch…"]` 形态的命令转交 patch 管线),再构造 `ShellRequest` 交给 `ToolOrchestrator::run`(shell.rs:211-228)。`exec_command`(unified exec)则由 `UnifiedExecProcessManager` 管理 PTY 会话(exec_command.rs:129),命令未结束时返回 `session_id`,后续用 `write_stdin` 工具续写/轮询(参数 `ExecCommandArgs` 见 `core/src/tools/handlers/unified_exec.rs:28-48`)。

---

## 5. 审批(approval)在链路中的插入点

审批不在 registry 层,而在 **exec 类 handler 内部的 `ToolOrchestrator`**(`core/src/tools/orchestrator.rs:1-8` 模块注释:"approval → select sandbox → attempt → retry with an escalated sandbox strategy on denial")。

### 5.1 三态需求 + 决策序列

`ExecApprovalRequirement`(`core/src/tools/sandboxing.rs:156-175`):`Skip { bypass_sandbox }` / `NeedsApproval { reason }` / `Forbidden { reason }`。由 exec policy + `AskForApproval` 策略与沙箱策略推导(`default_exec_approval_requirement`,sandboxing.rs:198-234:Never 不问;OnRequest 在受限沙箱下问;UnlessTrusted 总问)。

`ToolOrchestrator::run`(orchestrator.rs:136)流程:

1. **审批阶段**(orchestrator.rs:165-225):`Forbidden` → `ToolError::Rejected`;`NeedsApproval` → `resolve_tool_apporval`(注意源码拼写如此,`core/src/tools/approvals.rs:191`),内部先跑 **permission-request hooks**(可 Allow/Deny,approvals.rs:203-230),否则路由到 Guardian(自动审查器)或 User(弹审批 UI,`tool.start_approval_async`);
2. **首次尝试**:按策略选沙箱执行;
3. **沙箱拒绝后的升级重试**(orchestrator.rs:301-499):仅当 `escalate_on_failure` 且策略允许时,以 `retry_reason`("command failed; retry without sandbox?",orchestrator.rs:532)**再次走审批**(orchestrator.rs:395-421),批准后脱沙箱重跑。

### 5.2 审批缓存与"批准整个会话"

`with_cached_approval`(sandboxing.rs:71-117):以序列化的 key(命令前缀、文件路径等)缓存 `ReviewDecision::ApprovedForSession`;apply_patch 一次可写多文件,所以是 **key 列表**全部命中才跳过弹窗。模型侧也能主动请求升级:`exec_command`/`shell_command` schema 里的 `sandbox_permissions: "require_escalated"` + `justification` + `prefix_rule`(shell_spec.rs:298-344),非 OnRequest 策略下这类请求被直接驳回(shell.rs:125-138)。

### 5.3 审批载荷是结构化的

`ApprovalAction`(approvals.rs:25-52)只有三种:`Shell`、`ExecCommand`、`ApplyPatch { cwd, files, patch }`——UI/Guardian 拿到的是**结构化动作**(命令 argv、cwd、要改的文件列表),不是自由文本。MCP 工具另有一套审批(`core/src/mcp_tool_call.rs:1275` `maybe_request_mcp_tool_approval`,同样带 session 级缓存,mcp_tool_call.rs:1937-1942)。

---

## 6. MCP 工具接入

- **服务器管理**:`core/src/mcp.rs:54` `McpManager` 把 config.toml、插件、扩展贡献的 server 合成 `McpConfig`(带冲突解析,mcp.rs:243-251);
- **工具枚举与命名**:`codex-mcp/src/tools.rs:25` `ToolInfo` 同时保留**原始名**(`server_name` + `tool.name`,协议调用用)与**模型可见名**(`callable_namespace`/`callable_name`,经 `normalize_tools_for_model_with_prefix` 清洗、去重、超长哈希,≤64 字节,tools.rs:113、226);`canonical_tool_name()` = namespaced 组合(tools.rs:58);
- **注册**:`core/src/mcp_tool_exposure.rs:20` `build_mcp_tool_runtimes` 把每个 ToolInfo 包成 `McpHandler`(`core/src/tools/handlers/mcp.rs:32`)——**每个 MCP 工具就是一个普通的 CoreToolRuntime**,与内建工具同一注册表;有 tool_search 时曝光为 `Deferred`(mcp_tool_exposure.rs:35-39);
- **spec**:`create_tool_spec`(`core/src/tools/handlers/mcp.rs:233-257`)统一产出 `ToolSpec::Namespace`;schema 走 §1.3 的净化管线;
- **执行**:handler 转发 `handle_mcp_tool_call`(`core/src/mcp_tool_call.rs:110` 起,内部含审批、参数文件重写、遥测),输出 `McpToolOutput`;hook 名统一 `mcp__<server>__<tool>` 前缀(`core/src/tools/handlers/mcp.rs:29-30、43-46`);
- **反向**:`mcp-server` crate 把 codex 自身暴露为 MCP server,提供 `codex`(`mcp-server/src/codex_tool_config.rs:118`,参数即 `CodexToolCallParam`:prompt/model/cwd/approval-policy/sandbox…,codex_tool_config.rs:25)与 `codex-reply`(codex_tool_config.rs:237)两个工具;底层 MCP 客户端在 `rmcp-client` crate。

---

## 7. 对 alda harness 的映射建议

### 7.1 值得照抄的骨架(并大幅裁剪)

| codex 机制 | alda harness 取舍 |
|---|---|
| `ToolExecutor`:spec 与 handle 同一 struct | **照抄**。`trait Tool { fn name(); fn spec() -> ToolSpec; async fn handle(inv) -> Result<Output, ToolError>; }` |
| 手写 `JsonSchema` builder + `*_spec.rs` 文件 | **照抄**(比 schemars derive 更可控,description 即 prompt)。但内部保存 provider 无关的 `{name, description, input_schema}`,由 Anthropic adapter 转 `input_schema`、OpenAI adapter 转 `parameters` —— 这是 codex 没有的双 provider 需求 |
| `FunctionCallError::{RespondToModel, Fatal}` 两态 | **照抄**。alda parse 报错、参数错都走 RespondToModel 让模型自纠;alda CLI 不存在才是 Fatal |
| `ToolRegistry = HashMap<name, Arc<dyn Tool>>` + 重名 panic + 未知工具回 "unsupported call" | **照抄** |
| 输出统一 `Exit code / Wall time / Output:` 头 + token 截断 | **照抄**(score JSON 可能很大,必须有截断策略) |
| `supports_parallel` 读写锁并行门、CancellationToken | M1 可省(顺序执行即可),留接口 |
| ToolExposure 四级、tool_search、code-mode、Namespace 合并 | **全部砍掉**(工具 <10 个) |
| 沙箱 + 升级重试 | **砍掉**——不给 LLM 通用 shell,只给白名单化的领域工具,codex 里最重的一层复杂度就消失了 |
| Pre/PostToolUse hooks | 可选;若做,插在 registry 分发处(registry.rs:517/613 的位置)而非 handler 内 |

### 7.2 建议工具集(全部为 `ToolSpec::Function`)

**符号反馈闭环(LLM 可读)**:

1. `write_score { path, content }` —— 类比 apply_patch 但简化为整文件覆写(alda 谱通常单文件,不需要 diff 语法;M2 若做局部修改可借鉴 Lark freeform)。学 codex 把校验放进 handler:写入后**自动跑一次 `alda parse`**,把错误直接附在输出里,省一轮往返;路径限定 workspace 内(校验失败 → RespondToModel)。
2. `alda_parse { path, output?: "data"|"events"|"ast" }` —— 执行 `alda parse -o data`,成功返回 score JSON(每个音符含 midi-note/offset/duration),失败把 stderr 原文 RespondToModel。只读,可并行。
3. `score_analyze { path, checks?: [...] }` —— 基于 parse JSON / MIDI 导出计算派生乐理度量(音域、时值网格对齐、声部交叉、音符密度、调内音比例等),返回**摘要**而非原始 JSON(对应 codex 的截断哲学:预算内给模型最有用的信号)。
4. (M2)`midi_export { path, out }` —— `alda export` + 程序化 MIDI 分析。

**人耳通道(LLM 不可读音频)**:

5. `play_for_human { path, from?, to? }` —— 执行 `alda play`。输出**只含元信息**:"已为用户播放,wall time X s,退出码 0"。绝不返回任何"听感"字段——LLM 的唯一听觉替代物是上面的符号工具;人的评价以自然语言用户消息回注。这一点必须写进工具 description,防模型幻觉出音频反馈。
6. (可选)`ask_human { questions }` —— 参考 codex 的 `RequestUserInputHandler`(`core/src/tools/handlers/request_user_input.rs`),把"请用户听完后从这几方面反馈"结构化。

**流程辅助(可选)**:`update_plan` —— 直接抄 `plan_spec.rs`/`plan.rs`(它是最小 handler 样板:解析参数 → 发事件 → 固定文本 "Plan updated" 回模型)。

### 7.3 审批映射

借 `ExecApprovalRequirement` 三态但退化为常量策略:`alda_parse`/`score_analyze` → Skip;`write_score` → Skip(限 workspace)或首次 NeedsApproval;`play_for_human` → NeedsApproval(打扰性副作用,类比 codex 的 escalated exec),并按 codex 的 `with_cached_approval` 模式支持"本次会话总是允许播放"。审批载荷学 `ApprovalAction`:给用户看结构化的 `{file, 时长估计}` 而非原始 JSON。

### 7.4 一个最小 handler 的形状(对照 plan.rs)

```rust
// 对照 core/src/tools/handlers/plan.rs:48-99 的结构
impl Tool for AldaParseTool {
    fn name(&self) -> &str { "alda_parse" }
    fn spec(&self) -> ToolSpec { create_alda_parse_spec() }   // 手写 JsonSchema,见 §1.2
    async fn handle(&self, inv: Invocation) -> Result<ToolOutput, ToolError> {
        let args: AldaParseArgs = parse_arguments(&inv.arguments)?;  // serde,失败 RespondToModel
        let out = run_alda(["parse", "-f", &args.path, "-o", "data"]).await?;
        if !out.status.success() {
            return Err(ToolError::RespondToModel(format!("alda parse failed:\n{}", out.stderr)));
        }
        Ok(ToolOutput::text(truncate_with_notice(out.stdout, budget)))
    }
}
```
