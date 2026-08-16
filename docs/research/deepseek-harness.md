# DeepSeek Harness 架构与机制

## 调研结论

DeepSeek Harness 不是一个围绕固定 agent loop 增加少量工具的应用，而是一套以 Cordis 插件树为运行时、以仅追加事件日志为会话真源的通用 Coding Agent 平台。模型适配器、提示词、工具、Skill、agent loop、压缩、子 Agent、工作流、沙箱和 Web UI 都通过插件及作用域组合。

它最有价值的设计不是某个单独工具，而是三条贯穿全系统的约束：

1. **模型可见即有持久记录**：消息历史、实际 system prompt、工具 schema 和模型调用配置都能从会话日志重建。
2. **能力、策略和消费入口分离**：可替换能力通常拆成 Service Definition、Provider 和 Consumer，策略通过事件流水线介入。
3. **动态信息不随意污染稳定前缀**：静态 system prompt、动态 user-role 上下文、Skill 目录与正文、历史压缩各有不同生命周期。

这些约束适合多模型、多工具、多宿主和可扩展插件平台。`alda-agent` 当前是单领域、单 Agent、只有一个模型工具的小型 Rust 应用，不应整体移植 Cordis 微内核、事件溯源、子 Agent 或工作流系统。更适合按实际需求局部借鉴调用快照、动态上下文分层和单调权限约束。

## 调研基准

- 官方仓库：[deepseek-ai/deepseek-harness](https://github.com/deepseek-ai/deepseek-harness)
- 本地源码：[ref/deepseek-harness](../../ref/deepseek-harness/)
- 分支：`master`
- commit：`47f943859bef60e4160492346772ded9b24f765a`
- commit 日期：2026-08-13
- 包版本：`0.1.0-rc.5`
- 调研日期：2026-08-16

仓库 README 将项目标为 developer preview，并明确会发生破坏兼容性的变更。因此本文描述的是上述 commit 的机制，不应被当作稳定公共 API 约定。

调研以源码、运行时配置和包内 README 为主要证据；项目自身的架构文档用于定位，再由实现交叉验证。仓库规模约为 226 个 `packages/**/package.json`，基础 bundle 当前包含 78 个配置行，说明其复杂度来自平台化能力组合，而不是单一 agent loop。

## 产品定位与入口

DeepSeek Harness 面向通用 Coding Agent 场景，提供多种宿主入口：

- `npx @deepseek-ai/dsh web` 启动 Web 应用。
- `dsh --profile headless "task"` 执行一次性无服务器任务。
- Python SDK 通过 stdio JSON-RPC 启动并驱动内置运行时。
- 还可接入 ACP、TypeScript SDK 和不同子 Agent provider。

直接 DeepSeek 适配器只是其中一个 provider。平台也提供基于 `pi-ai` 的多提供方适配器，二者可以同时挂载。

## 总体架构

```text
Profile / bundle / patch
          │
          ▼
    Cordis plugin tree
          │
          ├── services: LLM / Session / Tools / Prompt / Sandbox / ...
          ├── scoped plugins: per-agent overrides and restrictions
          └── typed events: observe / waterfall / parallel / serial
                         │
                         ▼
                   ReactLoopAgent
                         │
          ┌──────────────┼──────────────┐
          ▼              ▼              ▼
     Session log      ToolRuntime      LLM adapter
     source of truth  policy/dispatch  one request
          │
          ├── Web / transcript / replay / persistence
          ├── compaction surface replacement
          └── fork / subagent lineage
```

### Cordis 插件运行时

[Cordis 入门](../../ref/deepseek-harness/docs/cordis-primer.zh.md)把核心抽象归纳为插件、Context、Service、依赖注入、类型化事件和可逆副作用：

- 插件通过 `inject` 声明需要的服务，Fiber 等服务可用后再激活，不依赖配置行顺序控制启动。
- 服务位于稳定的 `ctx.<key>` 上，调用方依赖服务接口而不是具体实现。
- `emit`、`waterfall`、`parallel`、`serial` 表达不同的观察或控制语义。`waterfall` 是可短路、可包装返回值的中间件链。
- `ctx.effect()` 和 `ctx.on()` 返回 disposer；Fiber 卸载时按逆注册顺序执行清理，并等待异步 disposer。
- Fiber 根据依赖实现的世代在 `PENDING`、`LOADING`、`ACTIVE`、`FAILED`、`UNLOADING`、`DISPOSED` 间变化。依赖消失或更换会触发卸载和重新加载。

Loader 用稳定配置行 id 管理插件树，支持 create、update、remove 和嵌套 group。HMR 不是直接修改活跃树：用户 patch 变化后重新组合候选配置；读取、解析、导入或激活失败时保留最后一个可用树，并广播失败事件。

这种设计使注册行为天然可撤销，也让同一服务在不同 agent scope 中有不同实现或限制。代价是行为可能分散在大量插件、事件监听器和配置层中，理解一次请求需要同时检查组合树与作用域。

### Profile、Bundle 与配置覆盖

运行时从空配置树开始，按以下顺序应用 patch：

1. profile 的 `dsh.profile.bundles` 所列 bundle，按声明顺序。
2. profile 自己的 `cordis.patch.yml`。
3. `$DSH_HOME/cordis.patch.yml`。
4. 命令行中按顺序出现的 `--patch` overlay。

后层按配置行 id 覆盖前层。一个 patch 会替换目标行的整个 `config`，不是深度合并，因此覆盖者必须重述所有仍需保留的字段。

官方组合主要分三层：

| Bundle | 职责 |
|---|---|
| `dsh-base` | LLM、Session、Agent、Tools、Prompt、持久化、Skill、压缩、沙箱、审批、子 Agent provider 等共享能力 |
| `dsh-web-app` | Web 宿主、前端、工作区、浏览器插件、Web persona 和运行上下文 |
| `dsh-headless` | 一次性 headless surface 与相应的 agent 配置 |

基础组合可见于 [`packages/bundle/base/cordis.patch.yml`](../../ref/deepseek-harness/packages/bundle/base/cordis.patch.yml)，Profile 组合规则由 [`apps/cli/src/profile-boot.ts`](../../ref/deepseek-harness/apps/cli/src/profile-boot.ts) 实现。

## Agent loop

默认实现是 [`ReactLoopAgent`](../../ref/deepseek-harness/packages/core/agent-loop/src/agent.ts)。其两个边界是：

- **Turn**：从领取一次唤醒输入开始，直到没有待执行工作。
- **Step**：一次模型请求，以及该请求产生的工具调用。

Inbox 提供三种输入语义：

| 操作 | 进入位置 | 是否主动唤醒 |
|---|---|---|
| `followup` | 下一 Turn | 是 |
| `steer` | 下一 Step | 是 |
| `inject` | 下一 Step | 否，等待其他输入唤醒 |

一个典型 Step 的持久与实时流程为：

```text
turn/start
  claim inbox
  assemble prompt + runtime contexts + tool schemas
  agent/pre-step
  step/start
  user/message*
  deriveMessages(session log)
  agent/request
  request/header (+ request/context when changed)
  llm/stream
  assistant/chunk* -> assistant/message
  tool/call* -> pre-execute -> execute -> post-execute -> tool/result*
  step/end
turn/end
```

`agent/pre-step` 可以拒绝或重写将进入模型的输入。即使首批输入被拒绝，也会留下一个无 Step 的 Turn，从而保留发生过这次尝试的事实。工具欠下下一次请求或新的 next-step 输入到达时，当前 Turn 继续执行下一 Step。

Agent loop 只协调流程；Skill、compaction、goal、subagent 等能力通过服务和事件接入，而不是写死在循环里。

## Session：仅追加事件日志与可重建请求

[`Session`](../../ref/deepseek-harness/packages/core/session/src/index.ts) 是仅追加的类型化事件日志，也是交互历史的唯一真源。LLM history 不单独保存，而是通过 `deriveMessages()` 从日志当前 surface 派生。

核心事件包括：

- `turn/start`、`turn/end`
- `step/start`、`step/end`
- `user/message`
- `assistant/chunk`、`assistant/message`
- `tool/call`、`tool/result`
- `request/header`、`request/context`

`assistant/chunk` 保存流式回放保真度，`assistant/message` 保存下一次请求需要的完整 assistant 消息。Web UI、transcript、恢复、fork、遥测和持久化都消费同一事件流。

### Surface 与 replacement

只有 `user/message`、`assistant/message` 和 `tool/result` 产生模型消息。它们构成有序 surface，并可携带 `surfaceOp`：普通消息追加到 surface，压缩或裁剪产生的新节点则替换一个旧范围。旧事件仍保留在日志中，但不再出现在派生的模型历史里。

这样同时保留了不可变审计记录和可缩减的模型上下文。replacement 范围按 surface 位置解释，不能简单视为连续事件序号，因为先前替换产生的新高序号节点可以位于更早位置。

### Request header

每次请求实际使用的信封通过完整 `request/header` 快照持久化：

- provider、model、reasoning effort、采样参数等调用配置。
- 适配器补齐的默认值。
- 已渲染的完整 system prompt。
- 已组装的工具 schema。

首次或恢复时写完整快照；内容变化时再写新的完整快照。`request/context` 单独记录路由和 context window 等路由元数据。由此可以从日志回答“当时模型真正看到了什么、用了哪个模型和工具定义”，而不是用当前配置猜测历史调用。

## System prompt 与动态上下文

[`SystemPrompt`](../../ref/deepseek-harness/packages/core/system-prompt/src/index.ts) 将模型输入分为三个来源：

1. `PromptSection`：按 `order` 排序并拼接为真正的 system prompt。
2. `PromptContext`：动态运行事实，作为持久 user-role 快照进入历史。
3. Tool provider：提供本次请求可见的工具 schema。

Section 的惯例顺序是 identity `-100`、部署 persona `0`、工具指导 `100–199`。`complete: true` 可以用一个 section 替换整条 system prompt。文本支持严格的 `{{variable}}` provider；缺少变量会失败，而不是静默保留占位符。

注册同时存在 global 与 agent scope。相同名字的 scoped section、context 或 variable 覆盖 global 版本；工具 provider 则共同贡献，再应用作用域限制。每个 Step 都重新组装，所以配置或上下文变化无需重建 Agent。

动态 context 只在完整快照变化，或先前快照被 compaction 隐藏时，追加新的持久 user message。其目的不仅是记录事实，也是在频繁变化的运行状态与稳定 system prompt 前缀之间建立缓存边界。

## 工具体系

[`ToolRuntime`](../../ref/deepseek-harness/packages/core/tools/src/index.ts) 同时负责注册、可见性、策略与执行。一个 Tool Definition 声明：

- 模型可见的 name、description、parameters。
- 实际执行函数。
- 结果规范化和 UI presentation。
- 是否允许并发，以及可选的超时等元数据。

执行链为：

```text
tools/pre-execute -> monotonic guards -> tools/execute -> tools/post-execute
```

`pre-execute` 可以 allow、deny 或 ask。ask 只有在审批服务返回一次性允许后才执行，没有审批通道时按拒绝处理。Guard 位于可扩展 pre-execute 之后且是单调的：任意 guard 可以拒绝，但后续逻辑不能恢复已被拒绝的权限。

Agent scope 的 tool restriction 同时影响 schema 可见性和实际 dispatch，防止“模型看不见但仍可调用”或“模型看得见却必然被隐藏策略拒绝”的错位。

多个工具调用按模型顺序预处理与提交结果，但执行体可并发：声明 concurrency-safe 的调用进入有界并行池；exclusive 调用形成前后 barrier。运行时会在启动前重新判定模式，避免策略变化破坏顺序。

工具还支持 `native`、`code`、`both` 三种呈现模式。Code Mode 只向模型直接暴露 `run_code`，其他工具生成 SDK，由模型编写的程序间接调用，以减少大量原生 schema 对请求的占用。

## Skill：目录与正文分离

Skill registry 合并 runtime、project、user 和 provider catalog。模型不会默认收到所有 Skill 正文：

1. 初始请求得到一条持久 user-role Skill catalog，只包含可调用 Skill 的 name 与截断后的 description。
2. 模型判断匹配后调用 `skill` 工具加载完整正文。
3. 工具结果本身进入历史，不再额外重复注入一份正文。
4. 用户输入 `/skill-name` 时，pre-step listener 确定性注入该 Skill 正文，并提示模型不要再次调用工具加载。
5. Skill 集合或描述变化时，追加一份完整 replacement catalog；空 catalog 是明确的 tombstone。

Catalog 的 durable source 保存结构化 entries 与 digest，消费者无需反向解析展示文本。若 compaction 隐藏了当前 catalog，下一次完整观察会重新发布当前快照。正文所引用的资源仍按需读取，不在加载 Skill 时递归注入。

这一设计让常驻 token 成本大致随 Skill 名称和描述增长，而不是随所有正文总长度增长；代价是多一次工具往返、目录 replacement 的历史成本，以及需要处理 Skill 生命周期和 compaction 交互。

关键实现位于 [`packages/skill/tool-skill/src/index.ts`](../../ref/deepseek-harness/packages/skill/tool-skill/src/index.ts) 与 [`packages/skill/skill/src/index.ts`](../../ref/deepseek-harness/packages/skill/skill/src/index.ts)。

## Compaction：事务式 surface 替换

基础压缩后端由 [`BasicCompactionEngine`](../../ref/deepseek-harness/packages/compaction/compaction-basic/src/index.ts) 实现。默认在路由 context window 的 80% 处触发，并保留约 16% 的近期 surface；这些值可按精确 provider/model 覆盖。

处理顺序为：

1. 使用 token meter 测量最新已记录请求信封和 surface。
2. 可选地先裁剪超大文本工具结果；重新测量后若已回到安全范围，则不调用摘要模型。
3. 从最旧的完整 surface 单元选择范围，调整边界以保证 tool call/result 配对平衡，并保留近期尾部。
4. 追加 `compaction/start`，形成持久锁。
5. 调用摘要模型并验证结果确实缩小源内容。
6. 追加 `compaction/summary`，紧接着追加带 replace 操作的 `user/message` checkpoint。
7. 追加 `compaction/end`；失败也记录 error。未匹配的 start 会阻止并发压缩。

摘要请求会逐字回放被压缩请求的 system prompt、工具 schema 和历史前缀，再追加固定压缩指令，以尽量复用提供方已有 KV cache。只有摘要文本进入会话 surface；辅助调用的 reasoning 和 tool calls 被丢弃，但 provider、model、maxTokens、usage 和完整 raw output 会记录在 `compaction/summary` 中。

Context overflow 也可触发恢复。只有 surface replacement generation 真正前进后才重试原请求，避免无进展循环。系统不能修复 system/tool envelope 自身过大、不可分单元过大等问题。

这个机制的核心价值在于：压缩不是覆盖或删除历史，而是一笔可审计、可检测并发变化、保持工具配对的投影事务。代价是 token 计量、surface 代数、持久锁、辅助模型调用和异常恢复的显著复杂度。

## Subagent 与 Workflow

Subagent 是独立 capability seam，不属于默认 agent loop。Provider 可以是：

- 进程内 spawn 或 fork。
- ACP。
- Codex。
- Claude Code。
- DSH SDK 等外部实现。

请求可以指定 persona、tool filter、depth limit 和 structured output，运行模式可以是 one-shot 或可 followup 的 continuable child。父子控制权限基于持久 lineage，而不是可伪造的消息来源字段。控制面提供 followup、interrupt 和 list。

Workflow 则在 worker thread 中执行受限脚本，通过 `agent()` 调用一个或多个子 Agent，并提供 parallel、pipeline 等组合器。工作流生命周期和成员运行会写入父 Session；取消和 dispose 有界，并等待子任务停稳。

两者的分工是：Subagent 抽象一次可替换的 Agent 委派；Workflow 抽象多个委派的确定性编排。相关约定见 [subagent](../../ref/deepseek-harness/docs/subsystems/subagent.zh.md) 与 [workflow](../../ref/deepseek-harness/docs/subsystems/workflow.zh.md)。

## 沙箱与权限

Sandbox seam 只承诺文件系统效果限制，不承诺网络隔离或进程可见性隔离。模式为：

- `read-only`
- `workspace-write`
- `danger-full-access`

策略按每次能力调用解析，工作区边界来自调用 Session 的不可变 cwd。受限模式必须返回真正施加约束的 argv，或以 `SANDBOX_UNAVAILABLE` fail closed；不能在后端不可用时静默裸执行。

本地实现按平台选择 Linux bwrap/Landlock、macOS Seatbelt 或 Windows ACL restricted token，并报告 `full` 或 `partial` enforcement。Base bundle 默认使用 `workspace-write + ask`；`danger-full-access` 对应 approval policy `never`。

沙箱、工具可见性、pre-execute ask 和 monotonic guard 是不同层次：沙箱约束进程文件效果，审批决定一次调用是否放行，guard 施加不可恢复的策略拒绝。

## DeepSeek 模型适配器

直接适配器注册 `deepseek-official` 路由，使用原生 `fetch` 与 SSE，而不是由通用 SDK 隐藏协议细节。默认公开：

| 项目 | 默认值 |
|---|---|
| 模型 | `deepseek-v4-flash`、`deepseek-v4-pro` |
| context window | 1,000,000 token |
| maxTokens | 256,000 |
| reasoning | `off`、`high`、`max`，默认 `high` |
| stream idle timeout | 300,000 ms |

模型目录只是建议；未列出的 model id 仍可原样发送。连接、模型目录、请求默认值和 credentials 在每次操作前重新解析，合法的新设置无需重启；进行中的流保持启动时快照。配置只保存 API key 引用，字面密钥由 credentials seam 或环境解析。

适配器一次 `stream()` 只发一个网络请求，重试由独立 `dsh-llm-retry` 插件处理。带 tool call 的 assistant 历史会按 DeepSeek 协议要求回传 `reasoning_content`，普通轮次则丢弃 reasoning 以减少后续输入。

错误统一映射为 `AUTH`、`QUOTA`、`RATE_LIMIT`、`CONTEXT_WINDOW_EXCEEDED`、`INVALID_REQUEST`、`SERVER`、`TRANSPORT`、`TIMEOUT` 等稳定 code。这个分类也为 compaction overflow 恢复和重试策略提供机器可读输入。

实现与完整协议说明见 [`packages/llm/llm-deepseek`](../../ref/deepseek-harness/packages/llm/llm-deepseek/)。

## 与 `alda-agent` 的对照

以下判断以当前源码为准，重点检查了 [`agent.rs`](../../alda-agent/src/agent.rs)、[`application.rs`](../../alda-agent/src/application.rs)、[`project.rs`](../../alda-agent/src/project.rs)、[`conversation.rs`](../../alda-agent/src/conversation.rs)、[`instructions.rs`](../../alda-agent/src/instructions.rs) 和 [`skills.rs`](../../alda-agent/src/skills.rs)。

| 维度 | DeepSeek Harness | `alda-agent` 当前实现 |
|---|---|---|
| 定位 | 通用、多宿主 Coding Agent 平台 | Alda 音乐创作领域应用 |
| 运行时 | Cordis 插件树，能力和策略可按 scope 替换 | Rust 模块直接组装，单 Agent |
| Agent loop | 多 Turn/Step、通用工具循环、可 steering/inject | 单请求内最多 3 轮 `submit_result` + Alda 校验修正 |
| 持久化 | append-only typed event log | `project.json` 中的可变 Conversation 快照和项目状态 |
| 请求重建 | 持久化完整 request header、system prompt、tools、route | 不保存每次请求的完整信封 |
| System prompt | ordered sections + variables + scoped override | 每次调用编译一份 `CompiledInstructions` |
| 动态上下文 | durable user-role snapshot | 当前/工作 Alda 和项目设置每次重建为第二条 system message |
| Skill | catalog 常驻，正文按需工具加载 | builtin workflow 和已显式启用 Advisory Skill 正文整体编入 system prompt |
| 工具 | 注册表、审批、guard、并发调度、Code Mode | 模型只见 `submit_result`，宿主校验并决定是否保存工作乐谱 |
| Compaction | 带持久事务与 surface replacement | 无历史压缩 |
| 权限/沙箱 | 通用 shell/fs 工具需要多层策略 | 不向模型开放通用 shell/fs，主要能力由宿主固定实现 |
| Subagent/workflow | 多 provider 和脚本编排 | 无；当前领域流程由单 Agent 完成 |

### 当前持久化边界

`alda-agent` 在执行用户请求前将 user message 和 `RequestPending` 写入 `project.json`；成功取得模型结果后，去除 system message，将 provider transcript 中的 user、assistant 和 tool 消息写回 Conversation。这样可以恢复用户对话和自动修正中的工具往返。

但以下调用事实没有持久化：

- 本次 `CompiledInstructions.rendered()` 的完整内容。
- 已计算但只在内存和 `/project instructions` 输出中使用的 fingerprint。
- 第二条 system message 中的项目设置和当时的 current/working Alda 快照。
- 模型、thinking、生成上限等完整请求配置。
- `submit_result` 的确切工具 schema。

因此，外部 Skill 正文、项目设置、工作乐谱或程序版本变化后，不能仅凭现有 Conversation 精确重建某次历史请求。

### Skill 差异

`alda-agent` 已经避免无条件载入所有发现到的 Advisory Skill：只有项目显式启用的 Skill 才由 `load_active()` 读取正文并加入 instructions。当前启用数量有限，且工作流 Skill 是每次创作都需要的领域规则，所以 DeepSeek Harness 的“目录 + `skill` 工具”机制暂时未必能节省足以抵消复杂度的 token。

真正需要渐进加载的信号应是：可选 Skill 数量明显增长、正文常驻成本可测、模型只在少数请求中使用其中一部分。达到该条件后，再引入目录和按需加载会更合理。

## 对 `alda-agent` 的建议

### 值得优先借鉴

1. **在需要审计或可复现时增加 Invocation 快照。**

   最小有用集合是 instruction fingerprint、完整 compiled instructions、动态项目上下文、模型调用配置和工具 schema。API key 只记录来源或是否存在，不能保存字面值。仅存 fingerprint 不够，因为 user/project Skill 文件可能随后改变。

2. **明确区分稳定指示和动态项目事实。**

   当前两条 system message 已有物理分层，但都属于 system prefix。若项目状态频繁变化并实际影响缓存或历史解释，可把动态项目上下文定义为有来源、有 digest 的 user-role snapshot，只在变化时持久化。没有测得缓存或重建问题前，不必立即改协议。

3. **保留“可见性与执行权限一致”的约束。**

   如果以后加入通用文件、shell 或插件工具，同一策略应同时过滤模型 schema 与实际 dispatch；权限 guard 只能收紧，不能被后续 listener 恢复。这比照搬整个 ToolRuntime 更重要。

4. **为长上下文先做可观测性，再设计压缩。**

   先记录请求大小、各上下文来源和增长趋势。只有真实会话达到窗口或成本阈值后，再考虑工具结果裁剪、摘要 checkpoint 或 replacement；不要预先引入完整 surface 代数。

### 适合条件触发后采用

- Advisory Skill 常驻 token 成本成为实际问题时，引入 catalog + 按需正文加载。
- 出现多个稳定的模型或执行后端时，再采用 Definition / Provider / Consumer 三角色 seam。
- Conversation 需要跨版本精确回放、审计或复杂压缩时，再评估 append-only invocation/event log。
- 多 Agent 音乐创作经过对照实验确认稳定提高结果质量时，再增加 subagent；当前没有这项证据。

### 当前不建议采用

- 不移植 Cordis 插件微内核或 HMR 配置树。`alda-agent` 的能力数量和部署形态不足以抵消其认知与生命周期成本。
- 不照搬通用工具注册、Code Mode、沙箱和审批栈。当前宿主固定持有 Alda 校验和文件写入，能力边界更窄也更清楚。
- 不引入 DeepSeek Harness 的完整 compaction 事务。当前应先证明对话长度确实成为问题。
- 不为了“可能有用”增加 Subagent 或 Workflow；音乐创作流程已有领域化的单 Agent 校验闭环。

## 风险与局限

- 项目仍处 developer preview，且调研 commit 很新；配置、包边界和默认模型都可能快速变化。
- 本文分析的是仓库内实现，不代表 DeepSeek 服务端的未公开机制。
- `defaultContextWindow = 1,000,000` 和默认模型列表是适配器配置事实，不等价于对所有部署、gateway 或未来模型的服务承诺。
- 沙箱只覆盖声明的文件系统效果；不能把它描述成完整容器隔离。
- 226 个 package manifest 反映仓库拆包规模，不等于一次 profile 会激活全部包。
- 对 `alda-agent` 的建议是架构取舍，不是已批准的实现计划；是否落地应由真实审计、缓存、token 或扩展需求触发。

## 主要源码索引

- 总体架构：[`docs/architecture.zh.md`](../../ref/deepseek-harness/docs/architecture.zh.md)
- Cordis：[`docs/cordis-primer.zh.md`](../../ref/deepseek-harness/docs/cordis-primer.zh.md)、[`vendor/cordis/src/fiber.ts`](../../ref/deepseek-harness/vendor/cordis/src/fiber.ts)
- Profile boot：[`apps/cli/src/profile-boot.ts`](../../ref/deepseek-harness/apps/cli/src/profile-boot.ts)
- Agent loop：[`packages/core/agent-loop/src/agent.ts`](../../ref/deepseek-harness/packages/core/agent-loop/src/agent.ts)
- Session：[`packages/core/session/src/types.ts`](../../ref/deepseek-harness/packages/core/session/src/types.ts)、[`packages/core/session/src/index.ts`](../../ref/deepseek-harness/packages/core/session/src/index.ts)
- System prompt：[`packages/core/system-prompt/src/index.ts`](../../ref/deepseek-harness/packages/core/system-prompt/src/index.ts)
- Tools：[`packages/core/tools/src/index.ts`](../../ref/deepseek-harness/packages/core/tools/src/index.ts)、[`packages/core/agent-loop/src/tool-calls.ts`](../../ref/deepseek-harness/packages/core/agent-loop/src/tool-calls.ts)
- Skill：[`packages/skill/skill/src/index.ts`](../../ref/deepseek-harness/packages/skill/skill/src/index.ts)、[`packages/skill/tool-skill/src/index.ts`](../../ref/deepseek-harness/packages/skill/tool-skill/src/index.ts)
- Compaction：[`packages/compaction/compaction-basic/src/index.ts`](../../ref/deepseek-harness/packages/compaction/compaction-basic/src/index.ts)、[`packages/compaction/compaction/src/types.ts`](../../ref/deepseek-harness/packages/compaction/compaction/src/types.ts)
- Subagent：[`packages/subagent`](../../ref/deepseek-harness/packages/subagent/)
- Workflow：[`packages/workflow`](../../ref/deepseek-harness/packages/workflow/)
- Sandbox：[`packages/sandbox`](../../ref/deepseek-harness/packages/sandbox/)
- DeepSeek adapter：[`packages/llm/llm-deepseek`](../../ref/deepseek-harness/packages/llm/llm-deepseek/)
