# Alda Music Agent MVP 设计

> 状态：概要设计已批准，可进入切片 A；后续决策按 §15 截止点冻结
> 日期：2026-07-30
> 目的：定义正式产品 MVP 的范围、架构边界和可执行验收标准
> 不包含：逐日排期、完整 UI 视觉稿、最终 wire schema 和 Rust 代码

## 1. 结论

本项目的正式 MVP 是一个**本地优先、单用户、Web/PWA 为主界面、CLI 为辅助入口**的音乐创作 Agent。它让用户从自然语言需求出发，得到经过校验的 Alda 乐谱，在主要界面预览并试听，对确切版本和片段反馈，生成新版本，最终由用户接受一个可恢复、可导出的结果。

MVP 的核心不是聊天，也不是一次性生成乐谱文本，而是这条可追溯闭环：

```text
需求与约束
→ Agent 生成或修改
→ parse 与结构 Gate
→ 不可变 Revision
→ MIDI Artifact
→ 预览与试听
→ 绑定实际播放范围的反馈
→ 新 Revision / Take 比较
→ 人类接受
```

架构采用宿主无关的 Runtime 和统一 App Service。Web/PWA、CLI 和未来原生 App 只通过同一套命令、事件和审批语义访问作品状态，不各自实现业务逻辑。

## 2. 名称和范围裁决

现有文档同时使用了 `legacy-mvp`、`minimal` 和“MVP”。本文作如下统一：

| 名称 | 含义 | 是否为正式产品 MVP |
|---|---|---|
| `legacy-mvp` | M0–M5 的单 Agent 工程基线：Provider、四工具、REPL、JSONL Session、压缩和 legacy 评测 | 否 |
| `minimal` | 具备 Brief、Constraint、Revision、Artifact、权限、Take、Audition、Feedback 和统一多端协议的产品 Profile | 是 |
| `advanced` | `minimal` 加模式、MUSIC.md、Skill、声明式 Hook、Plugin、只读 MCP 和证据化 Memory | 否，后续版本 |
| `studio` | 外设、DAW、写入 MCP、命令扩展和经验证的 SubAgent/Teams | 否，探索范围 |

正式 MVP 对应 `minimal` Profile，不等于先完成一个 CLI Demo。M0–M5 仍可作为低风险学习和验证路径，但其 CLI composition root、共享 `&mut Session` 和 `current_score: String` 不能成为产品架构。产品实现应从第一天建立统一 App Service；旧基线若已实现，则通过 Adapter 和迁移器接入。

## 3. MVP 边界

### 3.1 In Scope

- 单用户、单项目单写者；允许多个本地客户端查看和提交，过期提交被拒绝；
- 响应式 Web/PWA 主界面；同一设备上的本地 Runtime 提供服务；
- 薄 CLI，用于创建/恢复项目、提交请求、查看状态、导出和运行评测；
- 至少一个真实可用的模型 Provider；Anthropic 与 OpenAI Adapter 均完成工程兼容验证；
- 结构化 Brief、Hard/Soft Constraint 和待确认问题；
- 生成整首或修改可靠范围，失败不覆盖已有作品；
- 不可变 Revision、简单 Take、历史、比较、回退和人类 Accept；
- Alda 源码、parse 结果、分析结果和 MIDI 的 Artifact 化；
- `score_patch`、`score_parse`、`score_analyze`、`audition` 四条基础工具路径；
- Web/PWA 中的曲谱工作面接入点、版本/片段定位和播放联动；具体记谱形式及直接编辑深度由专项设计冻结；
- 整首与可靠片段试听、停止、实际播放范围和 Feedback；
- 版本化双向协议、流式事件、审批、取消、幂等命令和断线恢复；
- Model Egress、项目内写入和 Audible Output 的最小权限策略；
- 本地事件存储、Artifact Store、投影恢复、损坏尾记录处理；
- `alda-eval/v2` 的 MVP 子集：H0、H1、关键 H2、基础 H3、人工闭环和工程指标。

### 3.2 Out of Scope

- 云多租户、账号体系、跨设备同步和多人实时协作；
- 原生 iOS/Android/桌面 App；MVP 的 App 形态由响应式 PWA 覆盖；
- 完整五线谱编辑器、出版级排版或 MusicXML/Alda 双向编译；
- 音频录制、离线 WAV 渲染和 Audio Critic；Alda 原生 MIDI 不描述为音频 Render；
- 专业实时伴奏、演出级低延迟和后台持续播放；
- 任意 Bash、外部网络工具、DAW/外设写入、发布和 Marketplace；
- Skill、Hook、Plugin、MCP、长期 Memory；
- SubAgent、Agent Teams、自动语义合并；
- 用自动分数证明作品“好听”或用相似度给出法律结论。

## 4. MVP 用户闭环

1. 用户在 Web/PWA 创建 Project，输入目标、风格、编制、时长等要求。
2. 系统保留原始描述，形成 `BriefRevision` 和 `ConstraintSet`；影响 Hard Constraint 的未知项先询问。
3. 用户确认后启动 Turn。Runtime 调用 Provider，模型只能使用当前 Profile 注册的工具。
4. 工具在隔离 staging 中生成 Alda 源码，执行 parse 和结构检查。失败结果回给模型修正，但不提交 canonical。
5. 通过最低 Gate 后，Coordinator 以当前基线创建不可变 `ScoreRevision`，保存源码、parse、分析和 MIDI Artifact。
6. Web/PWA 展示版本、证据和曲谱工作面。用户选择整首或可靠片段并主动播放；CLI 可在能力允许时请求本机播放。
7. 系统记录实际开始、停止/完成和 played range。用户反馈绑定该次 `Audition`，不能自动覆盖未播放范围。
8. Agent 基于原 Revision 提出 Patch，重新走权限、校验和 CAS，创建新 Revision；旧版本保持不变。
9. 用户可建立第二个 Take，比较符号差异和现场 A/B，选择继续修改、回退或 Accept。
10. 用户重载页面、客户端断线或进程重启后，Project、Revision、Artifact、Audition、Feedback 和 Accept 状态可恢复；进行中的外部调用不伪装为已恢复。

## 5. 最小领域模型

### 5.1 对象

| 对象 | 责任 |
|---|---|
| `Project` | 长期作品身份、当前 Brief、Take/Branch 集合和已接受 Revision |
| `BriefRevision` | 原始意图、结构化目标、编制和未决问题的版本 |
| `ConstraintSet` | 带来源、严重度、范围和验证方法的约束集合 |
| `Session` / `Turn` | Agent 工作上下文与一次执行；不是作品事实源 |
| `ScoreRevision` | 不可变的作品版本，引用父版本、Brief、Artifact 和 Evidence |
| `Take` | 从共同基线产生、供比较的候选分支 |
| `Artifact` | 以内容 hash 标识的 Alda、parse 数据、分析数据或 MIDI |
| `Evidence` | 语法、结构、启发式或人类证据，绑定 Revision hash 和范围 |
| `Audition` | 某客户端对某 Revision/MIDI/范围的一次实际试听 |
| `Feedback` | 绑定 Audition 和 played range 的用户原话及可纠正结构化解释 |
| `LifecycleProjection` | Candidate、Accepted 等可变生命周期投影，不写回 Revision |

MVP 不实现完整 `Score IR`。它只保留足以定位的 `IR Lite`：稳定 Part alias、显式 Marker、已确认 Section/Beat 映射和 source map。映射不可靠时，只允许 `WholeScore`、可靠 Part 或 Marker 范围，不虚构小节地址。

### 5.2 不变量

1. Revision 创建后不可覆盖；内容变化必须创建新 Revision。
2. Revision 的每个 parent 必须存在、属于同一 Project，新增边不能形成环；根 Revision 除外。
3. 作品事实来自领域事件和 Artifact，不来自聊天摘要或模型记忆。
4. 每次 Revision 写入都指定 `target_take_id`、`branch_id` 和 `expected_head_revision_id`；CAS 针对该 branch head，不使用含糊的全项目 current revision，也不做 last-write-wins。
5. Artifact hash、Evidence subject hash 和 Revision source 必须一致。
6. Hard Constraint 只有 `Pass` 或人类显式 Waive 才允许 Accept；`Unknown` 不是 `Pass`。Waiver 必须记录 actor、reason、scope、timestamp、constraint ID 和适用 Revision ID，不能自动沿用到新 Revision。
7. Evidence append-only；更正通过新 Evidence 指向被替代项，不能覆盖原证据。
8. parse、启发式分析、模型判断和人类意见互不替代。
9. Feedback 范围不得超过实际 played range；读谱意见不能标为听审证据。
10. 工具只能提出 Artifact、Evidence 和 Domain Event；只有 Project Coordinator 能提交。
11. Agent 不能 Accept 或 Publish；MVP 只有人类能 Accept，且不提供 Publish。
12. 对话压缩只能改变模型上下文，不能改变 Brief、Revision、Artifact、Audition 或 Feedback。

## 6. 系统架构

```text
Web/PWA ── WebSocket/HTTP ─┐
CLI ── local IPC/loopback ─┼── Local Service Process
                           │      ├─ App Service
                           │      ├─ Session / Turn Runtime
                           │      ├─ Project Coordinator
                           │      ├─ Provider + Tool Runtime
                           │      └─ Event / Artifact Store
测试/排他嵌入模式 ──────────┘
```

### 6.1 边界

- **Domain**：纯领域对象、状态迁移和不变量；不依赖 Tokio、HTTP、CLI 或 Provider。
- **Runtime**：Agent Loop、上下文构建、取消和工具编排；只产生结构化事件，不打印终端或操作 UI。
- **Project Coordinator**：Project 单写者，串行处理领域命令、验证基线并提交事件。实现可用 Actor/消息循环，不使用全局 `Arc<Mutex<Session>>`。
- **App Service**：统一处理客户端命令、查询、订阅、幂等、审批和投影恢复。
- **Protocol**：稳定的 wire DTO；不直接把 Rust 领域结构序列化成永久公开协议。
- **Transport**：Web/PWA 使用 WebSocket 和 HTTP；独立 CLI 进程优先使用当前 OS 用户可访问的 local IPC，也可使用同一受认证 loopback API。进程内 channel 只用于测试或取得排他锁的嵌入模式。
- **Client**：只持有显示状态和未提交草稿，不拥有权威作品状态。

本地 Alda 子进程和模型密钥只存在于受信 Runtime。浏览器不能直接得到服务端凭据或任意文件访问权。

### 6.2 进程所有权

- 一台本地实例只有一个 Local Service Process 持有 Runtime、App Service、Project 写锁、事件日志和 Artifact Store；Web 与 CLI 不各启一个事实源。
- 服务进程启动时取得用户级实例锁并发布受限的 IPC/loopback endpoint。发现已有健康实例时，CLI 连接该实例，不创建第二套 Runtime。
- 嵌入模式只用于测试、无服务的离线 CLI 或恢复工具。它必须先取得同一排他锁并确认不存在健康服务；锁冲突时 fail closed。
- 服务异常退出后，新实例先验证锁持有者、恢复日志并处理未完成资源，不能仅因发现陈旧 endpoint 就并行启动。
- 一个 Web 客户端和一个 CLI 同时提交时，命令进入同一 Coordinator，并遵循相同幂等、branch-head CAS、权限和事件顺序。

### 6.3 并发模型

- 每个 Project 只有一个 Coordinator 写者；查询读取投影。
- 同一 Project 可有多个连接，但命令通过 `client_command_id` 幂等，写操作通过目标 branch head CAS。
- parse/analyze 可在隔离 staging 中有界并发；canonical commit、播放设备和同一 Project 写入串行。
- 所有内部队列有界；过载返回可识别、可重试错误，不静默丢弃权威事件。
- 不持有锁跨 `await`。阻塞或 CPU 密集工作进入受控 worker，Alda 使用异步子进程管理。

## 7. Application Protocol

协议是双向、版本化的 Command/Event/ServerRequest 契约。具体采用 JSON-RPC 还是 tagged message 在实现前冻结；两者不得改变下列语义。

### 7.1 最小命令面

```text
initialize / capability.declare
project.create / project.open / project.snapshot
session.start / session.resume / session.subscribe
turn.start / turn.steer / turn.cancel
question.respond / approval.respond
revision.history / revision.compare / revision.accept
take.create / take.select
audition.start / audition.progress / audition.stop / feedback.record
artifact.manifest / event.resume(stream_kind, stream_id, epoch, after_sequence)
```

改变权威状态的命令均携带 `client_command_id` 和规范化 payload digest；Revision 生成命令另带 `target_take_id`、`branch_id` 和 `expected_head_revision_id`，ActionPlan 必须绑定相同目标。`take.select` 默认只改变当前 Connection/Session 的显示焦点，不移动 Project branch head；若未来需要共享选择，必须另定义显式领域命令。协议、事件日志和评测报告分别带 schema version。

幂等结果持久化在对应事务批次中：同一 `client_command_id` 与同一 digest 重试时返回原结果；同一 ID 携带不同 digest 时返回 `IdempotencyConflict`，不能把后一个请求当作重试。

### 7.2 事件等级

- **权威事实**：Revision/Artifact/Evidence 创建、审批决定、Audition 状态、Feedback、Accept、Turn 完成，以及 `PendingQuestion` / `PendingApproval`。必须写入对应事实日志；投影只是可重建查询结果，不能充当恢复来源。
- **可恢复流事件**：模型正文、工具状态，以及待决问题/审批的投递通知。保留到 Session 恢复窗口结束；通知过期或丢失不删除其持久待决状态。
- **短暂事件**：token 动画、重复进度、播放光标。可合并或丢弃，但必须发出 lag 状态，不能冒充完整流。

### 7.3 断线与重启

- Connection、Session、Turn 和 Project 生命周期分离；断开 WebSocket 不删除或取消它们。
- MVP 中，客户端断线后 Turn 默认继续，直到完成、显式取消、预算耗尽或 Runtime 停止。
- 重连先取得权威 snapshot/version，再按流调用 `event.resume(stream_kind, stream_id, epoch, after_sequence)` 补读。Project Event 与 Session Rollout 各自单调排序，cursor 不跨流比较；epoch/schema 不匹配或发现缺口时重取对应 snapshot。
- Runtime 重启后恢复已提交状态。重启时仍在运行的 Provider、Alda 或播放任务统一记录为 `AbortedByRestart`，不会自动重放副作用。
- 待决审批和问题保存为可恢复状态并在重连后重新投递；授权有 scope 和有效期，过期后重新请求。

资源所有权固定如下：

| 资源 | Owner | 断线行为 | 结束与清理 |
|---|---|---|---|
| Provider 请求、parse/analyze/Alda 子进程 | Turn | 可继续 | Turn 取消、总预算、timeout 或 Runtime 退出沿取消树清理 |
| Artifact staging | ActionPlan/Turn | 可继续 | commit 后转为可达 Artifact；失败、取消或超时后回收 |
| 浏览器/客户端播放 | Audition + client lease | 心跳丢失后停止，不随 Turn 无限继续 | stop、完成或 lease 到期，持久化实际 played range |
| 服务端本机播放 | Audition lease | 发起连接断开后只持续到 lease 上限 | stop、完成、取消或 lease 到期，释放设备/进程 |
| 审批与问题 | Session | 持久等待并可重投 | 回复、有效期届满或显式取消；Runtime 重启后保留并重投，若所属 Turn 已终止则带 `OwnerTurnAborted` 状态等待用户处理 |
| 订阅与播放光标 | Connection | 立即结束 | 丢弃短暂状态；重连读取 snapshot/sequence |

“断线后仍合法运行”的任务必须能追到 owner ID、CancellationToken、总预算/lease 和 Runtime 资源注册表；缺少任一项的进程或播放即为 orphan，必须被清理。仅观察到进程仍存在，不等于资源泄漏。

## 8. Runtime、Provider 与工具

### 8.1 Agent Runtime

Runtime 使用单 Agent 双层循环：外层管理 Turn，内层执行“模型 → 工具 → 结果回注”，直到正常结束、需要用户输入、取消、失败或达到预算。每个 Turn 至少限制模型迭代、token、墙钟时间、Artifact 大小和试听次数。

上下文按以下顺序构建：稳定指令 → 当前 Brief/Hard Constraint → 当前 Revision checkpoint → 近期真实用户消息 → 当前工具 schema。乐谱正文按需从 Artifact 读取；compact 只生成上下文 handoff，不生成作品事实。

### 8.2 Provider

- 内部统一 `Message`、`ToolCall`、流式 delta、usage 和 stop reason；Provider 差异留在 Adapter。
- MVP 至少一个 Provider 通过真实端到端测试；另一个通过协议 fixture 和受控 smoke test，避免抽象只对一家成立。
- 模型请求是 `ModelEgress`。首次发送前展示 Provider、endpoint 和字段范围；密钥不进入日志、Project 或浏览器。
- 只重试明确可重试且尚未产生不可安全复用输出的请求。流已部分返回后中断，Turn 标记为中断，由用户选择重试，避免重复工具调用。
- Provider 输出永远视为不可信输入，必须经过 schema、权限和领域 Gate。

### 8.3 Tool Contract

MVP 直接使用两阶段 Tool V2：

```text
resolve(args, snapshot)
→ ActionPlan(target_take_id / branch_id / expected_head_revision_id /
             effect / target / resource / data egress)
→ PermissionDecision
→ sealed AuthorizedActionPlan
→ execute(StagingCapabilities)
→ Artifact + Evidence + ProposedEvent
→ verify + branch-head CAS commit
```

`execute` 不接受裸参数，工具拿不到任意文件、网络或设备句柄。动态路径、设备或参数变化会使授权失效。不可事务化的外部副作用不在 staging `execute` 中直接发生：工具只准备计划和 Artifact，先提交 durable intent，再由 outbox dispatcher 执行。若必须接入 legacy Tool，只允许临时 Adapter，且该 Adapter 也必须证明审批前无副作用；MVP 发布时注册表中不得残留 Adapter。

### 8.4 基础工具

| 工具路径 | MVP 行为 | Effect |
|---|---|---|
| `score_patch` | ReplaceWholeScore 或可靠范围的最小语义 Patch，生成新 Revision | WorkspaceWrite |
| `score_parse` | 调固定 Alda 版本 parse，返回 Syntax Evidence | Observe / Subprocess |
| `score_analyze` | 生成可解释的结构与启发式 Evidence | Observe |
| `audition` | 生成/选择 MIDI Artifact 和播放计划；durable intent 提交后才委托客户端或本机播放器试听 | AudibleOutput |

首期语义 Patch 至少覆盖整首替换、转调、tempo、动态、`ChangeInstrumentation`、可靠 Part/Marker 范围替换，以及一个受限、可验证的节奏变换：`RotateRhythm { scope, steps }`。其输入必须是单一稳定 Part 内、单声部、拍网格可解析，且不含 chord、Voice、Cram 或跨 scope tie 的线性 note/rest cell。`steps` 对 cell 数取模，正数向后、负数向前，零步不创建内容变化；变换循环移动 cell 的时值序列，保持音高顺序、note/rest cell 数和总拍长，再从 scope 起点重算 onset。边界音符必须完整落在 scope 内；不满足条件时返回结构化 `UnsupportedRhythmShape` 或 `NeedsClarification`，不能猜测复调语义。范围外规范化 IR 事件切片 hash 必须不变。

`ChangeInstrumentation` 只作用于稳定 Part，必须保持该 Part 的音高、顺序、时值和范围外内容。任何 Patch 后重跑 H0 和适用 Hard Gate；每种 Patch 都有正向性质检查和非目标范围回归，局部证据复用不属于 MVP。

## 9. 持久化与 Artifact

### 9.1 本地存储

MVP 使用两个相互独立的追加日志：

- Project Event Log：Brief、Constraint、Revision、Evidence、Audition、Feedback 和 Accept；
- Session Rollout：对话、模型流、工具过程、`PendingQuestion` / `PendingApproval`、Turn 终态和 compact checkpoint。

两者均为版本化 JSONL 或等价 append-only 存储，各自带稳定 stream ID、epoch/schema 和流内单调 sequence；Project 与 Session sequence 不跨流比较。事实日志按下一节的事务批次、checksum 和 durability 规则写入，损坏尾记录只影响未完成提交。投影可删除重建，不是事实源。

MVP 不要求 SQLite；当项目查询、索引或并发规模证明 JSONL 不足时再迁移。迁移必须保留 schema、原始 hash 和 dry-run，不原地覆盖唯一副本。

### 9.2 崩溃一致提交

一次改变 Project 的命令使用同一提交协议：

1. Coordinator 校验 `client_command_id`、payload digest、目标 Take/Branch 和权限，在目标文件系统的 staging 区执行 ActionPlan。
2. 每个 Artifact 以流式 SHA-256 计算内容地址，写入临时文件；完成后重新校验 hash/size，`fsync` 文件，原子 rename 到 content-addressed 最终位置，再 `fsync` 父目录。先完成的 Artifact 此时仍不可见，崩溃后只是可清理的 orphan blob。
3. Coordinator 验证 Artifact、Evidence、Revision DAG 与 Hard Gate，并在唯一写者内再次比较 `expected_head_revision_id` 和目标 branch head。
4. CAS 成功后，把 Domain Events、Artifact metadata、旧/新 branch head、单调 sequence、命令 ID/digest 及稳定响应写为**一个带 schema 与 checksum 的事务批次记录**。批次完整写入并 `fsync` 事实日志后，才是唯一可见提交点；批量 fsync 可以 group commit，但不得在覆盖该批次的 fsync 前向客户端确认成功。
5. 提交后更新/保存投影并发送事件。投影更新失败不回滚事实日志；重启时从已提交批次重建。

恢复时忽略 checksum/JSON 不完整的尾批次。若崩溃发生在 Artifact 持久化后、事件批次前，只产生不可达 blob；若发生在事件 fsync 后、响应前，客户端以相同 command ID/digest 重试并得到批次中保存的原结果。同一 ID 不同 digest 返回 `IdempotencyConflict`。任何读取、snapshot 或事件推送都不得在第 4 步前暴露 staged Revision。

### 9.3 不可事务化副作用

播放等不可回滚副作用使用 durable intent/outbox，不复用“先 execute、后提交”的 staging 顺序：

1. Coordinator 先以事实事务持久化 `AuditionRequested` / `AuditionAuthorized`、幂等 `dispatch_id`、Revision/MIDI hash、range、目标客户端/设备和 lease；
2. 事务提交后，outbox dispatcher 才向客户端或本机播放器发送播放请求；重复 dispatch 必须由接收端以 `dispatch_id` 去重；
3. 接收端实际开始后回传确认，Coordinator 以新事务记录 `AuditionStarted` 和实际起点；后续 progress 只更新 played range，stop/completed 形成终态；
4. dispatch 后未收到确认、客户端丢失或 Runtime 重启时，记录 `StartUnknown` 或 `AbortedByRestart`，不得伪装成已经播放，也不得自动重放；
5. 重启恢复未完成 outbox 时，只允许查询接收端的同一 `dispatch_id` 状态或终止为 Unknown/Aborted；用户显式重试产生新的 Audition 和 dispatch ID。

因此 staging 中的 `audition` 只准备 MIDI/计划，不直接驱动扬声器。事实日志至少能回答“已授权但未派发、已派发但未确认、已开始、已停止/完成”四类状态。

### 9.4 Artifact Store

Alda 源码、parse 数据、分析数据和 MIDI 按内容 hash 保存。元数据至少包含：mime、size、producer、source Revision、Alda/工具/模型版本、创建时间和来源。

Accepted Revision 及其必要 Artifact 为 strong reference，MVP 不自动 GC。导出只允许已存在且 hash 验证通过的 Artifact；MIDI 是符号/演奏数据，不称为可重复音频。

## 10. 试听与曲谱工作面

### 10.1 试听

- Web/PWA 声明 `midi_playback`、`playback_progress_reporting` 等 capability；不具备能力时服务端提供下载或受控本机播放路径。
- 浏览器播放绑定 MIDI hash、播放器版本、可得的 SoundFont/设备信息和目标范围。现场声音可能因设备不同，不承诺音频 hash 一致。
- 用户主动点击播放可视为本次明确意图；Agent 主动触发或 CLI 本机播放在首次 Audible Output 前请求批准。
- Audition 至少持久化 requested range、started、stopped/completed、played range、客户端和失败原因；高频光标不持久化。
- `audition_stop` 必须释放播放资源；客户端失联时由租约/超时结束 Audition，不能留下永久 Playing 状态。
- Audible Output 必须遵循 §9.3：`AuditionRequested/Authorized` 持久化早于播放派发，`AuditionStarted` 只在接收端确认实际开始后记录；`StartUnknown` 不能形成 played range 或听审证据。
- Agent 没有听觉。没有 `ListeningHumanEvidence` 或明确的 `AudioModelEvidence` 时，Agent 只能陈述符号事实和预期听感，必须标明依据，不能声称“我听到/听过”。AudioModel 不在 MVP 范围，因此 MVP 中只有真实 Audition 后的人类反馈能形成听审证据。

### 10.2 版本差异试听

MVP 的 before/after 比较必须绑定同一 `MusicalAddress`。系统记录两端 Revision/MIDI hash、requested range、播放顺序、播放器/SoundFont/设备和 tempo/gain 等播放参数；除被比较的音乐内容外，参数默认一致，无法一致时明确展示差异。

差异结果同时提供：

- 符号差异：音高、时值、乐器、动态、tempo 和结构变化；
- 可听差异摘要：根据符号变化说明**预计**可感知的差异，并给出依据，不伪装成系统已听见；
- 同范围 before/after 播放入口及各自 Audition/played range。

客户端不能播放时，降级为同范围符号差异、两份 MIDI/manifest 下载和 `NotAuditioned` 状态；不得生成虚假的试听完成或偏好证据。

### 10.3 曲谱工作面边界

“区别于纯代码编辑器的曲谱预览/编辑工作面”已作为 P0 产品需求保留，但本文不提前选择五线谱、Piano Roll、混合视图或直接编辑深度。

MVP 架构只冻结以下接口要求：

- 视图能明确绑定 `RevisionId` 和 Artifact hash；
- 用户选择能映射为可靠 `MusicalAddress`，无法可靠映射时降级而非猜测；
- 播放光标、选区、Feedback 和版本比较共享同一地址语义；
- UI 直接编辑若进入 MVP，也必须转为 `MusicPatch` 并创建新 Revision，不能绕过 Coordinator。

具体视觉形态、渲染库和首期编辑能力由独立设计与用户验证决定，不阻塞后端领域和协议地基。

## 11. 权限与安全

MVP 默认策略：

| Effect | 默认 |
|---|---|
| 读取 Project Artifact | 允许，限定当前 Project |
| 写入当前 Project | 允许，但只能走 staging、验证和 Coordinator 提交（WorkspaceWrite） |
| Model Egress | 首次披露并取得项目级同意 |
| Audible Output | 用户主动播放允许；Agent 主动播放首次询问 |
| 外部文件写、任意网络、设备写、发布、破坏性操作 | 禁止 |

安全基线：

- Local Service 安装/首次初始化时选择一个高位 loopback 端口并写入用户私有配置，之后保持同一精确 origin（scheme/host/port）托管 PWA、API 和 WebSocket。端口被其他进程占用时 fail closed 并提供显式修复/重绑定流程，不能静默换 origin；重绑定会使旧 PWA 安装失效并要求重新安装。不启用通配 CORS，不监听非 loopback 地址，并校验 Host 与 `Origin` allowlist；
- 首次浏览器连接只得到 bootstrap 页面。服务在可信本地终端/宿主界面显示一次性 bootstrap code，用户在页面内提交；成功后 code 立即作废，换取短期 session token。token 使用 `HttpOnly`、`SameSite=Strict` cookie 或等价内存凭据，不进入 URL、浏览历史、应用日志、Project、Rollout 或 Artifact；
- HTTP 命令和 WebSocket upgrade 都校验精确 Origin、有效 session token 和协议版本。bootstrap code 限时、限次、失败限速且不可重放；session token 过期或服务重启后重新 bootstrap；
- CLI 优先通过 OS 用户权限约束的 Unix socket/named pipe 连接；若使用 loopback，则从用户私有 runtime 文件读取短期凭据。CLI 不复用浏览器 URL token，也不把凭据写入项目；
- 禁止无认证暴露到局域网或公网；本安全方案只覆盖本地单用户 MVP，不替代云端认证；
- 项目路径通过 root capability 和 no-follow/提交前身份复验约束，不能只检查字符串中是否有 `..`；
- Alda 子进程固定可执行文件和参数模板，设置 timeout、输出上限、进程组 kill 和取消清理；
- 权限在副作用前判断，审批展示动作、范围、设备、预计时长和外发字段；
- 控制面、密钥和日志不属于普通 WorkspaceWrite；
- 日志默认只记录 ID、hash、状态、大小和时延，遥测默认关闭。

## 12. 实现切片

以下按依赖切片，不替代 M0–M8 的逐日路线，也不承诺工期。

### 切片 A：协议纵切片

用 Fake Provider 打通 Web/PWA 和 CLI：创建项目、启动/取消 Turn、结构化事件、Artifact 下载、断线恢复和审批往返。此切片优先验证宿主边界，不要求真实作曲质量。

### 切片 B：作品状态地基

实现 Brief、Constraint、Revision、Artifact、Project Coordinator、事件投影和 CAS；完成崩溃恢复与不变量测试。

### 切片 C：真实 Agent 与安全工具

接入 Provider、上下文构建和 Tool V2；完成生成、parse、自修复、分析、取消和权限闭环。若存在 legacy 代码，在此切片完成迁移并删除 Adapter。

### 切片 D：主要产品闭环

接入曲谱工作面契约、MIDI、Audition、Feedback、Take、比较和人类 Accept；完成 Web/PWA 主路径与 CLI 一致性。

### Acceptance Manifest

进入切片 E 前冻结机器可读、版本化的 Acceptance Manifest。每个验收 case 至少包含：

- case ID、需求/风险映射和测试类型（unit、property、integration、fault injection、E2E 或 human study）；
- 输入 fixture、随机种子、Provider/模型/Prompt/工具/Alda 版本；
- OS、浏览器、音频/MIDI 设备、网络和资源上限等环境；
- 初始 Project/Take/branch head，以及预期事件序列、Artifact hash/性质、投影、错误码或 CLI 退出码；
- 哪些步骤允许重试、最大次数，以及 pass/fail/允许失败率；
- TTFT、首次可试听时间、Turn 总时长、token、货币成本和 Artifact 大小门槛；
- 证据输出位置、日志脱敏规则和人工验收记录模板。

无法预先固定的生成内容用性质、范围和 Gate 判断，不硬编码具体旋律 hash；确定性 fixture、迁移和 replay 则固定预期 hash。阈值必须在发布候选测试开始前冻结，测试后不得为通过而静默调宽。

### 切片 E：硬化与发布验收

按 Acceptance Manifest 完成断线/重启、损坏日志、路径逃逸、过载、子进程残留、Provider 中断和跨客户端冲突测试；冻结协议和 `alda-eval/v2` MVP 报告。

每个切片都必须有可运行纵向证据；不能用 mock 通过最终闭环，也不能因前端尚未完成而让 CLI 成为作品事实源。

## 13. MVP 验收标准

以下全部通过才可称为正式 MVP：

### 13.1 用户结果

- [ ] 非 Alda 用户能在 Web/PWA 完成“需求 → 生成 → 预览 → 试听 → 反馈 → 修改 → 比较 → Accept”。
- [ ] Web/PWA 存在区别于纯源码编辑器的可视曲谱工作面，并完成 Revision/Artifact 绑定、可靠地址选择、播放光标、Feedback 和版本比较联动；验收不预设五线谱/Piano Roll，也不要求自由拖拽编辑。
- [ ] UI 始终能回答当前 Project、Revision、Take、Constraint 状态和正在试听的 Artifact。
- [ ] 用户可恢复历史项目并导出已接受 Revision 的 Alda 与 MIDI。
- [ ] CLI 读取同一 Project 时，Revision、Evidence、Audition 和 Accept 状态与 Web 一致。
- [ ] Web 与独立 CLI 同时连接唯一 Local Service 并提交命令，不会启动第二套 Runtime 或事实存储。

### 13.2 正确性

- [ ] 故意制造一次 parse 错误后，Agent 能修正并只提交通过最低 Gate 的候选。
- [ ] 修改产生新 Revision；旧 Artifact hash 不变，失败候选不污染 canonical。
- [ ] 同一 Take/Branch 上两个客户端使用相同 expected head 并发写时，恰有一个提交成功，另一个得到包含 expected/actual head 的冲突；不同 Take 上的提交互不产生伪冲突。
- [ ] `take.select` 只改变发起 Connection/Session 的显示焦点，不改变其他客户端焦点或任一 branch head。
- [ ] 不存在、跨 Project 或成环的 parent 被拒绝；合法多代 Revision DAG 可从事件重建。
- [ ] Hard Constraint 为 Fail/Unknown 且无 Waiver 时，Accept 被拒绝。
- [ ] 合法 Waiver 含 actor/reason/scope/timestamp/constraint/revision；缺字段、错 Revision 或试图自动沿用到后继 Revision 时被拒绝。
- [ ] Evidence 更正追加新记录并保留 supersedes 链，原 Evidence/hash 不被覆盖。
- [ ] Feedback 绑定确切 Audition，用户中途停止后 scope 不超过 played range。
- [ ] `ChangeInstrumentation` 保持目标 Part 的音高/顺序/时值；`RotateRhythm` 按已定义方向和取模规则移动时值，并保持音高顺序、note/rest cell 数、重算后的 onset、总拍长及范围外规范化 IR 事件切片 hash。chord、Voice、Cram、跨边界 tie 和非网格输入 fixture 必须返回结构化拒绝，不产生 Revision。
- [ ] compact 前后 Project 投影 hash 一致。

### 13.3 恢复与协议

- [ ] WebSocket 中断后能按 Project/Session 分别使用 `(stream_kind, stream_id, epoch, after_sequence)` 恢复；cursor 不跨流复用，epoch/schema 错配或缺口会触发对应 snapshot，不依赖旧 token delta。
- [ ] 待决问题/审批的投递通知过期或丢失后，其持久 Session 状态仍可在重连/重启后重投；删除投影再重建不会丢失待决项。
- [ ] 重复发送同一 `client_command_id` 与 digest 返回原结果且不创建两个 Revision/Audition；同一 ID 不同 digest 返回 `IdempotencyConflict`。
- [ ] Runtime 在 staging 写入、Artifact fsync/rename、branch-head CAS、事件批次写入/fsync、投影更新和响应前各点被故障终止后，不出现可见半提交；事件前 orphan blob 可识别清理，事件后投影可重建。
- [ ] 播放派发前已持久化 durable intent；在 intent fsync、dispatch、接收端开始、Started 确认和终态记录各点故障注入后，状态只能是 Requested/Authorized、StartUnknown、Started 或 Aborted，不能出现“已播放但无 Audition 身份”，重启也不自动重放。
- [ ] 重启后进行中任务标为 Aborted，已提交 Project 状态完整，且不会自动重放扬声器或模型请求。
- [ ] 队列过载产生明确 Lag/Retry 信号，权威状态事件不静默丢失。
- [ ] 断开全部客户端后，Turn-owned Provider/Alda 在预算内继续属于合法运行；client-owned playback 在 lease 后停止；测试通过 owner/lease/取消注册区分合法任务与 orphan，并确认 orphan 为零。

### 13.4 安全与资源

- [ ] 未授权的 Model Egress 和 Agent 主动播放在 execute 前被阻断。
- [ ] 恶意或非 allowlist Origin、错误 Host、无 token、过期 token、bootstrap 重放和同一 code 二次兑换均被拒绝；合法 bootstrap 只成功一次。
- [ ] session token 不出现在 URL、浏览历史、应用日志、Project、Rollout 或 Artifact；WebSocket 与 HTTP 使用同一认证策略。
- [ ] 安装 PWA 后重启 Local Service，已安装入口仍使用同一 origin，并能重新 bootstrap、打开原 Project；端口冲突时服务拒绝静默换端口，显式重绑定会提示重新安装 PWA。
- [ ] 路径穿越、符号链接逃逸、任意参数和外部目标写入被阻断。
- [ ] Turn 取消、超时、播放 lease 到期和 Runtime 退出后无 orphan Alda/播放子进程。
- [ ] 浏览器、日志、Project 和 Rollout 中均不存在 Provider 密钥。
- [ ] 普通工具无法修改权限、配置或其他控制面文件。

### 13.5 质量证据

- [ ] 固定 fixture 上 H0 Artifact/parse 完整性全部通过。
- [ ] 所有声明为 Hard 的 H1 约束有 Pass/Fail/Unknown 证据，不缺省为 Pass。
- [ ] 转调和 tempo 修改至少有对应 metamorphic test，非目标范围有回归检查。
- [ ] 在没有 Audition/Human 或 AudioModel 证据的固定对抗提示中，Agent 明确说明自己未听音频，不产生“我听到/听过”的无来源主张；符号推断标为预期听感。
- [ ] before/after 使用同一范围和已记录播放参数，提供符号差异、带依据的预期可听差异摘要和两端 Audition；无播放能力时降级为 MIDI/manifest + `NotAuditioned`，不产生试听证据。
- [ ] 人工闭环记录允许“无偏好/不确定”，不合成单一“音乐质量总分”。
- [ ] Acceptance Manifest 覆盖全部发布验收，并记录 Provider、模型、Prompt、工具、Alda、输入、环境、平台、预期事件/hash/退出码、重试/失败阈值、成本/性能门槛和证据位置。

## 14. 主要风险

| 风险 | 影响 | MVP 应对 |
|---|---|---|
| Web/PWA 与本地 Alda 的连接方式选错 | 后期重写认证、文件和播放链 | 先做协议纵切片；Runtime 只监听 loopback；不把本地路径写入 wire schema |
| 曲谱工作面范围失控 | 演变为不完整的专业记谱软件 | 单独立项；本文只冻结 Revision/地址/播放接口 |
| Alda 地址不稳定 | 反馈或 Patch 指向错误位置 | alias/marker 优先；未知映射显式降级 |
| MIDI 在不同设备声音不同 | 用户误以为可重复音频 | 保存播放 manifest；UI 明示现场声音边界 |
| Provider 流中断或重复工具调用 | 重复写入、状态不一致 | command/tool 幂等、staging、部分流不自动重试 |
| JSONL 随项目增长变慢 | 恢复和查询延迟增加 | 投影/checkpoint；以测量结果决定是否迁移 SQLite |
| 多标签页并发 | 覆盖用户作品 | Coordinator 单写者 + branch-head CAS |
| 本地 Web 被跨站页面调用 | 未授权读取、命令或播放 | 同 origin 托管、精确 Origin/Host 校验、一次性 bootstrap、短期 token |
| legacy 设计继续渗透 | 绕过权限与 Revision | 发布门检查无 `&mut Session` Tool 和 Legacy Adapter |
| 产品范围过大 | MVP 无法收敛 | Profile 注册隔离；Advanced/Studio schema 不进入 minimal |

## 15. 实现前必须冻结的决策

| 决策 | 本文默认建议 | 截止点 |
|---|---|---|
| 首发部署 | 本地 Runtime + 同设备 Web/PWA | 协议纵切片前 |
| 协议 envelope | 简单 tagged message；若复用成熟库再选 JSON-RPC | wire schema 编码前 |
| 曲谱工作面 | 专项设计；不得默认等同代码编辑器 | 切片 D 前 |
| 浏览器 MIDI 引擎与 SoundFont | 先做跨平台 spike，再冻结 manifest 字段 | Audition 实现前 |
| 第二 Provider 的发布要求 | 一个真实 E2E，另一个 fixtures + smoke；若 PRD 坚持双生产可用则提高门槛 | Provider 接入前 |
| 断线后的 Turn 上限 | 本地 Runtime 存活期间继续，受 Turn 总预算限制 | 协议测试前 |
| JSONL group commit | 默认逐批 fsync；若启用 group commit，冻结最大等待时间，成功响应仍必须晚于覆盖批次的 fsync | Event Store 实现前 |
| 首发平台 | 建议先冻结一个桌面 OS + 两个主流浏览器 | 产品验收计划前 |

如果首发改为云托管、跨设备移动 App 或多人协作，应先新增租户隔离、认证、对象存储、后台作业、数据驻留和协作冲突设计；不能把本地单用户 Coordinator 直接部署到公网后仍称为同一 MVP。

## 16. 与现有文档的关系

- 产品目标和 P0/P1/P2 以[产品需求文档](../requirements/product-requirements.md)为准；本文把 P0 落为架构和验收边界。
- M0–M5 的具体学习步骤仍见[基础实施路线](implementation-roadmap.md)，但其 CLI/Session/Tool V1 只视为 legacy 基线。
- Revision、Tool V2、权限和 Audition 的详细协议沿用[进阶架构](advanced-music-agent-architecture.md)及[进阶路线](advanced-implementation-roadmap.md)。
- 多端边界和断线设计依据[CLI、Web 与 App 多端架构调研](../research/client-surface-architecture.md)。
- 若本文与旧基础设计冲突，正式 MVP 范围内以本文和产品需求为准；Advanced/Studio 细节仍以进阶文档为准。
