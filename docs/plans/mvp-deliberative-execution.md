# Alda Music Agent MVP 审议式实施记录

> 状态：执行中
> 原始目标：阅读 `docs/` 理解背景，编写计划并以审议式流程逐步实施，直至完成正式 MVP。
> 权威范围：`docs/requirements/product-requirements.md` 的 P0、`docs/design/mvp-design.md` 的 `minimal` Profile 与切片 A–E。

## 1. 任务解释

### 明确要求

- 阅读现有文档并以实际仓库状态校正文档中的历史描述。
- 制定覆盖正式 MVP 的计划，而不是把 `legacy-mvp` 或 CLI Demo 当作完成。
- 按“调查、设计、独立审查、增量执行、机械门控、独立复核”闭环实施。
- 最终逐条证明 `mvp-design.md` §13 的验收条件。

### 合理推断

- 当前代码是切片 A 的第一小段，应在其上继续，而不是重写为 M0–M5 的 legacy 架构。
- 大任务按可独立验证的子切片推进；子切片通过不代表 MVP 完成。
- 尚未到截止点的产品决策采用 `mvp-design.md` §15 的默认建议，并在进入对应实现前冻结。

### 待决事项

- 切片 D 前冻结曲谱工作面的具体技术方案和直接编辑深度。
- Audition 前通过 spike 冻结浏览器 MIDI 引擎、SoundFont 与 manifest 字段。
- Provider 接入前冻结第二 Provider 的发布门槛。
- 产品验收前冻结首发 OS、浏览器、性能和成本阈值。

这些事项当前不阻塞协议、状态、权限和 Agent 地基；到截止点若缺少足够证据，不擅自扩大承诺。

### 明确排除

- MVP 不包含云多租户、跨设备同步、多人协作、完整记谱编辑器、音频录制、DAW/外设写入、发布、Skill/Hook/Plugin/MCP/Memory 或 SubAgent。
- 不用 mock 冒充最终 Agent、Alda、试听或恢复闭环。
- 不以自动评分宣称作品“好听”。

## 2. 有界自治

### DANGER ZONES

- 协议兼容性、事件 schema、持久化和崩溃一致性。
- loopback 服务认证、Origin/Host、bootstrap token、路径和凭据处理。
- 子进程、播放设备、模型外发、取消树和资源清理。
- Revision/Artifact/Evidence/Audition/Feedback 的不可变性和引用一致性。
- 工作区已有 `.codex/` 删除及 `.agents/` 未跟踪内容属于用户变更。

### NEVER DO

- 不修改或恢复用户已有的 `.codex/` 删除与 `.agents/` 内容。
- 不监听非 loopback 地址，不把密钥/token 写入 URL、日志、Project、Rollout 或 Artifact。
- 不执行发布、联网模型调用、真实扬声器播放、Git commit/push 或破坏性迁移。
- 不允许工具直接提交 canonical，不允许 Agent Accept/Publish。
- 不用 `Arc<Mutex<Session>>` 或共享 `&mut Session` 绕过单写 Coordinator。

### IRON LAWS

- 正式 MVP 以 `minimal` Profile 为准；A–E 全部完成且 §13 逐条有证据后才能声明完成。
- 作品事实独立于聊天；Revision/Artifact/Evidence append-only，修改失败不污染 canonical。
- 所有状态变更命令幂等；Revision 写入使用明确 branch-head CAS。
- 所有副作用先解析 Effect、完成权限判断，再执行；不可事务化副作用使用 durable intent/outbox。
- HTTP/WS 只允许精确 loopback origin、Host 和有效认证。
- 每个执行子切片必须先过 `fmt`、`clippy -D warnings`、相关测试和已有不变式，再进入独立复核。

## 3. 当前事实与差距

调查日期：2026-07-31。

实际仓库已有 `alda-agent/`，因此 `docs/README.md` 和部分路线中“实现尚未开始”的描述已经过时。现有实现具备：

- versioned typed command：initialize、project create、snapshot；
- 有界 Tokio channel 和单个内存 App Service writer；
- 按 client/command ID 的进程内幂等与 payload 冲突拒绝；
- 带 Host、Origin、bearer token 检查的 loopback HTTP；
- 调用同一 HTTP 契约的薄 CLI；
- 单元测试和真实 loopback HTTP 测试。

现有实现仍缺少：

- 切片 A：Fake Turn、取消、事件订阅/恢复、审批往返、Artifact 下载、PWA；
- 切片 B：正式领域状态、持久化、恢复、CAS 和不变量；
- 切片 C：Provider、Agent Loop、Tool V2、Alda、权限与资源清理；
- 切片 D：曲谱工作面、MIDI、Audition、Feedback、Take、比较和 Accept；
- 切片 E：Acceptance Manifest、故障注入、跨客户端冲突、安全与质量发布门。

基线证据：

```text
cargo fmt --check                         PASS
cargo clippy --all-targets -- -D warnings PASS
cargo test                                PASS (6 tests)
```

## 4. 总体实施顺序

1. **切片 A — 协议纵切片**
   - A1：Fake Turn 启动/取消、结构化 Session 事件、cursor 恢复。
   - A2：PendingQuestion/PendingApproval 协议、Session 投影、重投与审批往返；
     A2 仅进程内，B 再提供重启持久恢复。
   - A3：Artifact manifest/download 与安全流式传输。
   - A4：最小同源 Web/PWA 与 CLI 共同驱动以上协议；断线恢复 E2E。
2. **切片 B — 作品状态地基**
   - 领域 ID、Brief、Constraint、Revision DAG、Artifact/Evidence。
   - Project Event Log、Session Rollout、事务批次、投影重建和损坏尾恢复。
   - Coordinator、branch-head CAS、幂等结果持久化与故障注入。
3. **切片 C — 真实 Agent 与安全工具**
   - Provider 抽象和至少一个真实 E2E、另一个 fixture/smoke。
   - Agent 双层循环、context/compact、预算和取消树。
   - Tool V2、Effect/Permission/Approval、staging/outbox。
   - Alda patch/parse/analyze、自修复和资源清理。
4. **切片 D — 主要产品闭环**
   - 冻结并实现曲谱工作面契约。
   - MIDI Artifact、Audition lease/played range、Feedback。
   - Take、比较、可靠 MusicPatch 和人类 Accept。
5. **切片 E — 硬化**
   - 冻结机器可读 Acceptance Manifest。
   - 完成恢复、安全、并发、路径、过载、Provider/子进程故障测试。
   - 完成 `alda-eval/v2` MVP 子集、工程指标和人工闭环证据。

## 5. 当前执行设计：A1

### 问题

现有协议只能同步创建/读取 Project，无法验证 Session/Turn 生命周期、结构化事件或断线后的 cursor 恢复。后续 Provider、审批和 WebSocket 都依赖这些语义。

### 方案

- 新增 `SessionId`、`TurnId` 和显式 `StreamCursor { stream_kind, stream_id, epoch, after_sequence }`。
- 新增命令：
  - `session.start(project_id)`
  - `session.snapshot(session_id)`
  - `turn.start(session_id, prompt)`
  - `turn.cancel(session_id, turn_id)`
  - `event.resume(cursor)`
- `SessionSnapshot` 至少返回 `session_id`、`project_id`、stream epoch、`covered_through_sequence` 和每个 Turn 的身份与正式状态。
- Fake Turn 不伪装真实 Provider：它只走正式状态机
  `Running -> CancelRequested -> Cancelled` 的受限子路径。wire 状态预留
  `WaitingForInput`、`Succeeded`、`Failed`、`BudgetExceeded` 和
  `AbortedByRestart`，但 A1 不伪造这些路径。
- 生命周期事件固定为 `SessionStarted`、`TurnStarted`、
  `TurnCancelRequested`、`TurnCompleted { status }`；Fake executor 不使用临时
  `Fake*` wire 类型。
- Session 事件从 sequence 1 开始、严格单调，固定 epoch 1；resume 返回
  `sequence > after_sequence` 的事件、当前 epoch/head 和下一 cursor。A1 事件不截断，
  因而不声称覆盖 retention gap。
- cancel 必须校验 Session/Turn 归属，并区分：
  1. 同一 command ID + digest 的重试逐字返回已存 reply，不追加事件；
  2. 新 command ID 对已终止 Turn 的业务重复取消返回带新 command ID 的
     `TurnAlreadyTerminal { turn_id, terminal_status, terminal_sequence }`，不追加事件。
- 继续使用 App Service 单写 actor；协议错误为 typed reply，队列错误保持 transport/service error。
- A1 仅为进程内恢复语义；重启持久恢复属于 B，不在 README 中夸大。

### Cursor 真值表

`event.resume` 在 A1 只接受 `SessionRollout` stream；返回最多 256 条事件，并返回
`next_after_sequence`，客户端可继续拉取。snapshot 的
`covered_through_sequence` 与读取该 snapshot 时的 head 一致。

| 输入 | 结果 | 是否追加事件 | 恢复动作 |
|---|---|---:|---|
| `after_sequence = 0` | 从 sequence 1 起返回现有页；空流则空页 | 0 | `Continue` |
| `0 < after_sequence < head` | 返回严格大于 cursor 的现有页 | 0 | `Continue` |
| `after_sequence = head` | 成功空页 | 0 | `Continue` |
| `after_sequence > head` | `InvalidCursor`，附 epoch/head | 0 | `FetchSessionSnapshot(session_id)` |
| epoch 不匹配 | `CursorEpochMismatch`，附 expected/actual | 0 | `FetchSessionSnapshot(session_id)` |
| 非 `SessionRollout` kind | `UnsupportedStreamKind` | 0 | `UseSupportedStreamKind` |
| stream ID 不存在 | `SessionNotFound` | 0 | `None` |

A1 没有事件截断，故不存在 retention gap。B 引入持久、有界保留或 compaction 时再增加
`CursorGap`，其恢复动作为 `FetchSessionSnapshot`，不能把 future cursor 冒充 gap。

### 异常路径

- 不存在的 Project/Session/Turn：typed not-found。
- Turn 属于另一 Session：typed ownership mismatch。
- 空 prompt 或超长 prompt：invalid request。
- cursor 错误按上表返回机器可读 `RecoveryAction`，客户端无需解析错误文案。
- 已终止 Turn 用新 command ID 再取消：返回关联新 ID 的稳定终态，不追加事件。

### A1 验收标准

- 单元测试证明 start → structured events → resume → cancel → terminal event 的序列与单调性。
- Session snapshot 返回可闭合恢复的 epoch、covered sequence 和 Turn 状态。
- cursor 真值表的每一行都有确定性测试；分页不遗漏或重复事件。
- 两个 Session 的 cursor 不能交叉读取。
- 同命令同 payload 重试无重复 Session、Turn 或事件；同 ID 不同 payload 仍冲突。
- 新 command ID 重复取消返回 `TurnAlreadyTerminal`、回显新 ID，终态事件仍恰好一次。
- HTTP round trip 覆盖 start/resume/cancel 和认证边界。
- CLI 能通过同一命令契约启动 Session/Turn、取消并读取事件。
- README 明确 A1 是开发纵切片及其非持久限制。
- `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test` 全部通过。

## 6. 门控与审查记录

| 阶段 | 状态 | 证据 |
|---|---|---|
| 初始基线 | PASS | 2026-07-31：fmt、clippy、6 tests |
| A1 设计审查 R1 | REVISE | `docs/reviews/a1-design-review.md`：snapshot/cursor 不闭合；重复取消语义混淆 |
| A1 设计审查 R2 | PASS | `docs/reviews/a1-design-review-r2.md` |
| A1 实施门控 R1 | PASS | fmt、clippy、10 tests；CLI help；分页/真值表 |
| A1 独立复核 R1 | REVISE | `docs/reviews/a1-implementation-review.md`：缺失 Origin 被放行 |
| A1 修复门控 | PASS | fmt、clippy、11 tests；精确 Origin/Host/token 矩阵 |
| A1 独立复核 R2 | PASS | `docs/reviews/a1-implementation-review-r2.md` |

### A1 RELEASE 与后续 L3 回归

A1 于 2026-07-31 通过 RELEASE。后续每个子切片的 L3 基线必须在
`alda-agent/` 运行以下命令，并保留 A1 的 Session/Turn/cursor/认证测试：

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

未将该不变式写入 `.agents/invariants/`，因为当前 `.agents/` 是用户已有的未跟踪内容，
任务边界明确禁止修改；本记录是本任务内的权威 L3 登记。

## 7. 当前执行设计：A2

### 问题

A1 已有 Session/Turn/cursor，但客户端无法处理服务端发起的问题或审批。权威协议要求
`question.respond`、`approval.respond`，且投递通知丢失不能删除待决状态。后续 Tool V2
还要求审批 payload 展示实际动作、Effect、目标、范围和预计影响。

### 边界

- A2 冻结生产兼容的 wire DTO、状态机、命令和 Session snapshot 投影。
- A2 的“重投”指同一服务进程内：客户端可随时读取 snapshot，并用 cursor 补读通知。
- A2 不声称进程重启恢复；B 将待决对象和决定写入 Session Rollout 并做 replay 测试。
- A2 不执行真实 Model Egress、写文件或播放；批准只推进 Fake 状态机，不产生副作用。
- Permission policy、sealed `AuthorizedActionPlan` 和审批 cache 属于 C，不在 A2 伪实现。

### 方案

#### 正式对象

- `QuestionId`、`ApprovalId` 使用独立语义 ID。
- `PendingQuestion` 包含：
  - `question_id`、`session_id`、`owner_turn_id`；
  - `prompt`、非空结构化 `choices { choice_id, label }`、`status`；
  - 创建 sequence、可选 terminal sequence。
  - resolved 后保留规范化 `answer { choice_id }` 与 `responder_client_id`。
- `PendingApproval` 包含：
  - `approval_id`、`session_id`、`owner_turn_id`；
  - `ApprovalPayload { action, effect, target, scope, estimated_impact }`；
  - `status`、创建 sequence、可选 terminal sequence。
  - 不可变 `approval_subject_digest`，标识被批准的精确规范化计划；
  - resolved 后保留 `decision` 与 `responder_client_id`。
- Question 状态：`Pending | Answered | OwnerTurnAborted`。
- Approval 状态：`Pending | Approved | Denied | Expired | OwnerTurnAborted`。
- `terminal_sequence` 表示 Answered/Approved/Denied/Expired/OwnerTurnAborted 任一终态
  的事实 sequence，不把 owner abort 冒充正常 resolved。
- snapshot 返回全部未决对象及其当前状态；已解决对象保留在 A2 内存投影中，B 再定义保留期。

#### Fake 流程

`turn.start` 后 Fake executor 进入以下正式状态机子路径：

```text
Running
→ PendingQuestion + WaitingForInput
→ question.respond
→ PendingApproval + WaitingForInput
→ approval.respond(approve) → Succeeded
                        deny → Failed
```

固定 question 是纯创作约束：“请选择作品长度”，choices 为 `bars_8` 与 `bars_16`；
它不授予任何权限，只把有界答案记为后续 Brief 输入。`question.respond` 只接受列出的
`choice_id`，未知 choice 在状态改变前返回 `InvalidQuestionChoice`，A2 不接受自由文本。

随后独立创建 `EffectClass::ModelEgress` approval，payload 明确动作、目标 Provider、
发送字段范围和预计影响。fixture 先构造包含 provider endpoint、字段集合、owner Turn
和 prompt digest 的规范化 Fake Action Plan，以 SHA-256 生成
`approval_subject_digest { algorithm, schema_version, value }`；
requested/resolved/snapshot 始终携带同一 digest。
这只是协议 fixture，不实际调用 Provider。A2 不根据 prompt 文本选择隐藏脚本。

digest 由服务端生成，客户端视为 opaque。A2 canonical bytes 固定为
`serde_json::to_vec` 对以下有序 tuple 的 UTF-8 编码：

```text
(
  "alda-agent.approval-subject",
  1,
  normalized_provider_origin,
  sorted_unique_egress_field_names,
  owner_turn_id,
  lowercase_prompt_sha256
)
```

endpoint 只保留规范化 origin，字段名按 UTF-8 字节升序去重；wire 固定
`algorithm = "sha256"`、`schema_version = 1`、`value` 为小写 hex。实现必须冻结一个
固定输入/输出测试向量。C 若扩展 subject schema 必须增加版本，不能改变 v1 bytes。

取消可在任一待决阶段发生：

- 事件顺序固定为 `TurnCancelRequested` → 按创建 sequence 升序追加所有
  `QuestionOwnerTurnAborted` / `ApprovalOwnerTurnAborted` →
  `TurnCompleted(Cancelled)`；
- 上述事件通过同一个 reducer 将 Turn 置为 `Cancelled`，并把所有仍 Pending 且属于
  该 Turn 的对象置为 `OwnerTurnAborted`；命令处理器不得直接写派生投影；
- 后续 respond 返回 typed `RequestOwnerTurnAborted`，不推进 Turn 或产生副作用。

#### 命令与事件

- 新增 `question.respond(session_id, question_id, choice_id)`。
- 新增
  `approval.respond(session_id, approval_id, approval_subject_digest, decision)`；
  digest 不匹配时返回 `ApprovalSubjectMismatch` 且不改变状态。A2 decision 仅
  `Approve | Deny`，有效期/expiry 由 C 的 Permission Broker 实现。
- 新增事件：
  - `QuestionRequested` 携带完整不可变 question/choices；
  - `QuestionResolved` 携带 question ID、choice ID、responder client ID；
  - `ApprovalRequested` 携带完整展示 payload 与 subject digest；
  - `ApprovalResolved` 携带 approval ID、subject digest、decision、responder client ID；
  - `QuestionOwnerTurnAborted` / `ApprovalOwnerTurnAborted` 携带对象 ID、
    owner Turn ID 和 `owner_terminal_status`；
  - 继续使用通用 `TurnCompleted`，成功/拒绝分别为 `Succeeded`/`Failed`。
- 同 command ID + digest 重试返回原 reply，不重复解决或追加事件。
- 新 command ID 对已解决对象返回 typed `QuestionAlreadyResolved` /
  `ApprovalAlreadyResolved`，回显新 command ID，不追加事件。
- Session/object 不匹配返回 ownership mismatch；不存在返回 not-found。
- 空 answer、超长 answer、空 payload 字段在状态改变前返回 `InvalidRequest`。
- requested event 是创建投影的完整事实，resolved event 是决定投影的完整事实；B 只
  增加 durability，不补造 A2 丢失的字段。
- owner-abort event 是取消导致的对象终止事实，不能由命令处理器旁路 reducer 修改，
  也不能只依靠 `TurnCompleted` 隐式扫描推断。

### 恢复与事件规则

- `SessionSnapshot.covered_through_sequence` 与 questions/approvals/turns 来自同一 actor
  原子读取点。
- 通知事件即使已被客户端读过，也不删除 snapshot 中的 Pending 对象。
- snapshot 后按 `covered_through_sequence` resume 不重复旧通知，只取得并发新增事件。
- A2 提供纯 reducer，从 Session 事件重建 question/approval 投影；测试会先清空派生
  投影，再从内存事件 replay，所得 snapshot 必须逐字段一致（含 answer/decision、
  responder、payload 和 subject digest）。
- A2 沿用 A1 epoch/cursor；不引入 retention gap 或磁盘 persistence。

### A2 验收标准

- happy path 的完整事件序列、状态转换和 snapshot 一致。
- 未知 question choice 与 approval subject digest mismatch 均不改变状态或追加事件。
- deny path 以 `Failed` 终止，且没有 Approved 状态或任何副作用。
- question 阶段取消、approval 阶段取消均把未决对象标为 `OwnerTurnAborted`。
- 同命令重试与新命令重复 respond 分离，resolved 事件恰好一次。
- 跨 Session 的 question/approval respond 被 ownership mismatch 拒绝。
- snapshot + cursor 证明通知可重投且 snapshot/增量无缺口或重复。
- 从事件 replay 后的 question/approval 投影与在线 snapshot 逐字段相同。
- HTTP round trip 与 CLI 覆盖 question/approval respond，并继续满足精确
  Host/Origin/token 边界。
- README 明确 A2 仍为 Fake、进程内且不会执行批准的副作用。
- L3 固定重跑全部 A1 测试；fmt、clippy、全部测试通过。

## 8. A2 门控与审查记录

| 阶段 | 状态 | 证据 |
|---|---|---|
| A2 设计审查 R1 | REVISE | `docs/reviews/a2-design-review.md`：语义混淆、replay 字段缺失、approval 未绑定 plan |
| A2 设计审查 R2 | REVISE | `docs/reviews/a2-design-review-r2.md`：owner abort 缺事实事件 |
| A2 主流程裁决 | APPROVED | 补齐显式 abort 事件、reducer 顺序及版本化 canonical digest；两轮上限后按证据裁决 |
| A2 实施门控 | PASS | fmt、clippy、17 tests、CLI help、diff check；A1 全回归 |
| A2 独立复核 | PASS | `docs/reviews/a2-implementation-review.md` |

### A2 RELEASE 与后续加固

A2 于 2026-07-31 通过 RELEASE。后续 L3 除 A1 基线外必须保留 A2 的 canonical digest、
reducer/replay、两阶段取消、幂等、ownership、HTTP/CLI 和认证测试。

独立终审记录两项非阻断加固，进入 A3 实施时一并补测：

- question/approval 两条取消事件流分别从空投影 replay，并与在线 snapshot 逐字段相同；
- HTTP approval 后读取最终 snapshot/event，断言 answer/decision/responder/digest 与
  Turn `Succeeded`。

## 9. 当前执行设计：A3

### 问题与边界

切片 A 还未验证 Artifact manifest 和二进制 HTTP 下载宿主边界。A3 使用 Fake
Provider 的确定性 Alda 源码 fixture 打通这一路径，但不提前冒充 B：

- A3 只提供进程内、只读、content-addressed fixture store。
- B 才实现磁盘 staging、流式 hash、size/hash 复验、fsync、原子 rename、metadata
  event、Project replay、orphan 清理和 Revision/Artifact 引用。
- A3 不生成 MIDI/音频，不执行 Alda，不声称 fixture 是通过 parse Gate 的 Revision。
- A3 没有导出到任意用户路径；HTTP response 只下载现有 blob。

### Artifact 对象

- `ArtifactHash` 是验证过的 `sha256:<64 lowercase hex>` typed value；wire 不接受路径。
- `ArtifactOccurrenceId` 标识一次成功产生/引用；`ArtifactManifest` 至少包含：
  - occurrence ID 与内容 hash；
  - hash、kind (`AldaSource`)、MIME (`text/x-alda; charset=utf-8`) 和 size；
  - producer (`FakeProviderFixtureV1`)；
  - owning `project_id`、source `session_id` / `turn_id`；
  - tool/provider fixture version、created sequence 和 provenance label；
  - `durability = ProcessLifetimeFixture`，防止客户端误认持久 Artifact。
- 明确拆分：
  - `BlobRecord: hash -> immutable bytes/size/mime`，只在 blob 层按内容去重；
  - 每次不同成功 Turn 创建新的不可变 occurrence/manifest，唯一键为 occurrence ID；
  - Project reachability 单独登记 `(project_id, hash)`；
  - 同 Project 不同 Turn 的固定 bytes 共享 blob，但各自 occurrence 保留真实来源；
  - 跨 Project 相同 bytes 共享 blob，occurrence 与 reachability 仍隔离。
- A3 fixture bytes 固定、大小受小上限约束，并冻结 SHA-256 测试向量。

### 创建与查询

- 仅 `approval.respond(Approve)` 的 Fake 成功路径生成 fixture；Deny/Cancel/invalid/digest
  mismatch 不生成 Artifact。
- `ApprovalDecided` 成功结果增加本次 occurrence manifest；同 command ID 重试返回同一结果和
  blob，不重复创建。
- 新增只读 `artifact.manifest(project_id, artifact_occurrence_id)` 命令；occurrence
  必须属于 Project，manifest 再提供内容 hash。
- hash 存在但未对该 Project 登记时返回 `ArtifactNotFound`，避免跨 Project
  existence oracle；不存在与不可达使用同一错误。
- manifest 查询不改变事实，不需要 `client_command_id` 业务幂等之外的额外写入。

### Actor 原子边界

- HTTP 与 wire 命令都不直接持有或锁住 Artifact Store。
- 现有 bounded App Service channel 扩展为内部消息枚举：
  `WireCommand` 与非 wire 的
  `ResolveArtifactDownload { project_id, hash, if_none_match }`。
- 同一 `AppServiceRunner` actor 处理内部下载查询，并返回不可变
  `VerifiedDownload { manifest summary, Arc<[u8]> } | NotModified | NotFound | Corrupt`；
  HTTP adapter 只负责认证、解析 header 和映射 response。
- Fake fixture 先在不可见局部构造并复验固定 hash/size/MIME；测试注入的 mismatch 在
  追加 Approval/Turn 事实或修改 Store 前返回 `ArtifactPreparationFailed`，approval
  保持 Pending，Turn 保持 WaitingForInput，且无 blob/reachability/occurrence/reply。
- 验证成功后，单 actor 在一个不含 `await` 的同步 transition 中插入/复用 blob、
  登记 reachability 与 occurrence、追加 `ApprovalResolved` / `TurnCompleted`，并生成
  含 manifest 的稳定 reply。actor 不会在 transition 中处理查询，因此外部只能观察
  “全部之前”或“全部之后”。A3 进程崩溃会丢失全部内存；B 再提供磁盘原子提交。

### HTTP 下载

- 路径固定为 `GET /v1/artifacts/{sha256_hex}`；不得接收文件名、绝对路径、`..`、
  URL 或任意 filesystem target。
- 与命令端点使用同一精确 Host、Origin、session token 策略，并额外要求
  `X-Alda-Project-Id`；只有 manifest 对该 Project 可达才返回。
- 响应设置固定 allowlist `Content-Type`、`Content-Length`、
  `ETag: "sha256:<hex>"`、`X-Content-Type-Options: nosniff` 和
  `Content-Disposition: attachment; filename="score-<short-hash>.alda"`。
  文件名只由服务端 hash 派生。
- 支持 `If-None-Match` 精确匹配并返回 `304`；A3 不实现 Range。
- 所有 Artifact 成功、304 和错误响应设置 `Cache-Control: private, no-store` 与
  `Vary: Origin, Authorization, X-Alda-Project-Id`；304 不带 body。
- 无效 hash 格式返回 `400`；不存在和跨 Project 均返回相同 `404`；认证失败在查找前
  返回，不泄露存在性。
- 下载时重新计算内存 bytes 的 SHA-256 与 size；不一致返回 `500`，不发送损坏 bytes。
- 即使 ETag 命中，也必须先完成认证、hash 解析、Project reachability、hash/size
  corruption 复验；只有全部通过后才返回 304。

### A3 验收标准

- Approve 生成 manifest/blob；Deny、两阶段 Cancel、invalid choice 和 digest mismatch
  不生成 Artifact。
- 固定 fixture hash/size 测试向量通过；重复 approve transport retry 不重复创建。
- 同 Project 两个不同 Turn产生两个真实 occurrence、共享一个 blob；跨 Project 相同
  bytes 共享 blob但 occurrence/reachability 隔离。
- manifest 命令按 occurrence 同 Project 成功，跨 Project/不存在均返回相同 not-found。
- fixture hash/size 故障注入证明 approval、Turn、blob、reachability、occurrence 和
  幂等 reply 均未半提交。
- HTTP bytes 只能通过 actor 内部只读查询取得，源码中不存在共享可变 Store 旁路。
- HTTP 合法下载 bytes/hash/headers 正确；ETag 命中返回 304。
- HTTP 覆盖无效 hash、跨 Project、错误/缺失 Project header、Host、Origin、token。
- 下载 API 不接受任何路径或客户端文件名；源码检索与测试证明目标仅由 hash 决定。
- 补齐 A2 终审登记的取消 replay 和 HTTP 最终事实增强测试。
- CLI 支持 `artifact manifest`；下载可由输出 manifest 中的受认证 HTTP URL/示例完成，
  CLI A3 不写本地文件。
- README 明确 fixture 的进程生命周期、未 parse、非 Revision、非持久 Store。
- L3 重跑 A1/A2 全部测试；fmt、clippy、全部测试和 `git diff --check` 通过。

## 10. A3 门控与审查记录

| 阶段 | 状态 | 证据 |
|---|---|---|
| A3 设计审查 R1 | REVISE | `docs/reviews/a3-design-review.md`：blob/provenance 冲突、HTTP actor 边界缺失、原子顺序未冻 |
| A3 设计审查 R2 | PASS | `docs/reviews/a3-design-review-r2.md` |
| A3 实施门控 | PASS | fmt、clippy、21 tests、diff check、CLI help；A1/A2 全回归 |
| A3 独立复核 | PASS | `docs/reviews/a3-implementation-review.md` |

### A3 RELEASE

A3 于 2026-07-31 通过 RELEASE。后续 L3 必须保留 blob/occurrence/reachability、
preparation fault、internal actor download、HTTP auth/cache/ETag/corruption/path 和
manifest CLI 测试。终审建议增加“Project 存在但 hash 不可达”的 404 no-oracle 测试，
进入 A4 加固项。

## 11. 当前执行设计：A4

### 目标与边界

A4 完成切片 A 的真实宿主纵切片：同源 Web/PWA 能 bootstrap、调用统一 command
contract、通过 WebSocket 接收/恢复 Session 事件、处理 question/approval、取消 Turn
并下载 A3 Artifact。

- Web 只是 client；Project/Session/Turn/Artifact 真相仍只在 App Service actor。
- A4 不实现 B 的磁盘状态、实例锁/用户私有 runtime 文件或重启 replay。
- 默认端口 `37891` 是开发固定 origin；显式端口冲突直接启动失败，不静默换端口。
- 普通 `serve` 拒绝端口 `0`；仅 Rust 测试 harness 可直接绑定临时端口。显式非默认
  端口是本次进程明确选择的 origin，不是端口冲突后的自动 fallback。
- A4 Web UI 是可运行的最小产品壳，不冒充切片 D 的曲谱工作面、MIDI 或完整创作闭环。

### Bootstrap 与认证

- `GET /`、`/app.js`、`/app.css`、`/manifest.webmanifest`、`/sw.js` 由同一 loopback service 提供，
  不启用 CORS；HTML 无内联脚本。固定 CSP：
  `default-src 'self'; script-src 'self'; style-src 'self';
  connect-src 'self' ws://<exact-listen-host:port>;
  img-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none';
  form-action 'self'`，其中 WS origin 由已验证的精确 listen address 生成，禁止
  wildcard、`unsafe-inline` / `unsafe-eval`。
- 服务启动生成高熵、一次性、5 分钟 bootstrap code，只输出到可信终端 stderr；
  code 不进入 URL、HTML、日志后的请求记录、Project、Session 或 Artifact。
- `POST /v1/bootstrap` 必须同时满足精确 Host/Origin，body 只含 code：
  - 成功一次即原子作废，返回短期随机 session token cookie；
  - cookie 为 `HttpOnly; SameSite=Strict; Path=/`，不写 URL/localStorage/JS；
  - loopback HTTP 开发环境不能可靠使用 `Secure`，README 明确此限制；未来 HTTPS
    origin 必须加 `Secure`。
  - code 过期、错误、重放统一 `401`，失败按进程内 IP/全局计数限速；
  - 成功/错误 response 均 `Cache-Control: no-store`。
- 浏览器 HTTP/WS 使用 cookie；CLI 继续使用独立 env bearer token。两种凭据不可互换
  生成，认证器只接受其对应 header/cookie，且都继续要求精确 Host/Origin。
- session token 固定短期 expiry，服务重启失效；A4 不实现 refresh。

### WebSocket 协议

- `GET /v1/ws` upgrade 前校验 Host、Origin、cookie/bearer、协议版本子协议
  `alda-agent.v1`；失败在 upgrade 前拒绝。
- WS client message 使用 typed tagged envelope：
  - `Command(CommandEnvelope)`：调用现有 bounded actor，返回
    `CommandReply(CommandReply)`；
  - `Subscribe { session_id, epoch, after_sequence }`：建立/替换该 connection 的
    Session subscription；
  - `Unsubscribe { session_id }`；
  - `Ping` / server `Pong`。
- server message：
  - `SessionEvents(EventPage)`；
  - `Lagged { session_id, last_delivered_sequence, recovery: FetchSessionSnapshot }`；
  - `ProtocolError`。
- WS adapter 不持有 Session state。App Service 增加内部只读
  `ResolveSessionEvents` 查询，复用 A1 cursor truth table；adapter 以有界周期拉取，
  只发送 sequence 大于 connection cursor 的事件。因此 CLI/HTTP 产生的事件也能推送。
- 每个订阅分别维护 `queued_through` 与 `written_through`：
  - 每次 subscribe/replace 分配 connection-local 单调 `subscription_generation`；
    subscription、frame、writer ack、Lagged 均同时携带 generation 与 Session ID；
  - 只有 generation 和 Session 都等于当前订阅的 ack 才能推进 cursor；
  - replace/unsubscribe 后旧 frame 可被 writer 丢弃；即使已经写出，其迟到 ack 也必须
    忽略，且不得修改新订阅或未订阅状态；
  - poller 只用 `queued_through` 防止重复入队；
  - 一个 `SessionEvents` frame 内 sequence 必须连续；
  - writer 只有在整个 frame 的 WS `send` 成功返回后，才通过内部 ack 单调推进
    `written_through`；部分/失败 frame 不推进；
  - `Lagged.last_delivered_sequence` 严格等于 `written_through`，不得使用
    `EventPage.next_after_sequence` 或 queued cursor，并只描述其所属 generation；
  - 客户端处理完整 frame 后保存自己的 `last_processed_sequence`，断线恢复以该客户端
    cursor 为准；Lagged cursor 只是服务器 best-effort 下界，不能覆盖客户端确认。
- outbound queue 有界；满时不静默丢权威事件：停止该订阅的增量发送并投递一个
  `Lagged`（若连 Lagged 无法排队则关闭连接），客户端必须 snapshot + resume。
- 每个 connection 的订阅/cursor 只影响该连接；断线不取消 Session/Turn。
- 重连流程固定：`session.snapshot` → 使用 snapshot epoch 与
  `covered_through_sequence` 订阅；如要补断线窗口，则使用客户端最后确认 cursor
  subscribe，cursor error 时按 recovery action 重取 snapshot。

### 资源与公平性上限

A4 默认常量/配置必须有非零校验，并在测试中可缩小：

- 全局同时 WS connections：16；超限 handshake 返回 `503`。
- 每 connection 只允许 1 个 Session subscription；新订阅显式替换旧订阅。
- inbound WS frame/message：64 KiB；超限 close `1009`。
- outbound：16 messages 且序列化总 bytes 不超过 1 MiB；超限走 Lagged/close。
- 单个 `SessionEvents` frame：最多 64 KiB 且最多 A1 page 256 events；单事件若超过
  frame 上限，发送 typed `EventTooLarge` 并关闭，不截断权威事件。
- HTTP bootstrap body：1 KiB；command JSON body：64 KiB；超限 `413`。
- HTTP command/bootstrap/Artifact 并发各 32；超限/排队上限返回 `503`。
- 每 connection 最多 1 个 poll in flight；全局 poll in flight 8。
- poll 基础周期 250 ms；actor/query overloaded 时指数 backoff 到 2 s，禁止紧循环。

App Service 使用同一 state owner，但把入口拆成两个有界 typed channel：

- 高优先级 `WireCommand`，默认 capacity 64；
- 低优先级内部 query（Session events、Artifact download），默认 capacity 32。

两个 capacity 使用独立非零 validated newtype，测试可缩到 1。现有 CLI
`--queue-capacity` 明确只映射 command capacity；新增
`--query-queue-capacity`（默认 32），不得将一个值复制成两个隐含容量。

满载契约：

- HTTP command：command queue `try_send` 满返回现有 `503` / typed `Overloaded`；
- WS Command：返回 server `ProtocolError::Overloaded`，不关闭连接、不自动重试；
- HTTP Artifact query：query queue 满立即 `503`，adapter 不等待/重试；
- WS poll query：query queue 满不产生 cursor 变化，按 250 ms→2 s 指数 backoff；
- 其他内部 query 同样必须选择显式立即错误或有上限 backoff，禁止 `send().await` 在
  handler 中形成隐藏无界等待。

runner 使用确定性加权公平调度：每轮最多处理 8 个已就绪 commands，然后若 query
存在则处理 1 个；没有 command 时立即处理 query。内部 query 不构造
`CommandEnvelope`、不写幂等表或领域事件。这样 polling 不能长期饿死用户命令，命令
洪水也不能永远饿死恢复。测试暴露仅计数指标验证连接/task/poll/queue上限。

### Web/PWA 最小交互

- 页面支持：
  - 输入 bootstrap code 并建立 cookie session；
  - create Project、start Session、start/cancel Turn；
  - 展示 Project/Session/Turn IDs、结构化事件和当前 cursor；
  - 渲染 question choices，并发送 `question.respond`；
  - 展示完整 approval action/effect/target/scope/impact 与 subject digest，明确按钮
    Approve/Deny；
  - 展示 occurrence manifest，并以 Project header + cookie 下载 Artifact；
  - 断线状态、手动 reconnect、snapshot/resume 状态。
- UI 不把 question answer 当 consent；Approval 按钮不隐藏 payload。
- 所有 DOM 文本使用 `textContent`，不把 prompt/payload/event JSON 注入 `innerHTML`。
- service worker 使用版本化精确 allowlist `[/, /app.js, /app.css, /manifest.webmanifest]`，
  只缓存这些 URL 的成功同源 GET；`/sw.js` 自身、带 query 的 URL、重定向、非 GET 与
  所有非 allowlist 请求全部 network-only，不使用 path-prefix fallback，不缓存
  bootstrap/API/cookie/event/Artifact；
  离线时显示不可操作状态，不伪装已提交。

### A4 验收标准

- bootstrap：合法仅成功一次；错误、过期、重放、限速、缺/错 Origin/Host 均拒绝；
  cookie 属性、no-store、code/token 不进 URL/静态内容/领域状态。
- HTTP command 与 Artifact 使用浏览器 cookie 成功；CLI bearer 继续成功；两种凭据
  错用、缺失和过期均拒绝。
- WS handshake 覆盖 Host/Origin/token/subprotocol；无认证连接无法观察 stream。
- 真实 WS E2E：Web connection subscribe，CLI/HTTP command 产生 question/approval/
  terminal events，WS 按序收到；断线不取消 Turn，重连按 cursor 无遗漏/重复。
- future/epoch cursor error 产生 typed recovery；outbound 过载产生 Lagged 或关闭，
  不静默越过权威事件。
- 部分写失败测试：N..M 已入队，仅前一完整 frame 写成功后 writer 失败；恢复从
  `written_through`/客户端 processed cursor 继续，最终无缺失且不跳到 queued cursor。
- generation 测试：旧 frame 入队后以较低 cursor 替换同 Session、替换为不同 Session、
  以及 unsubscribe；迟到 send/ack 均不得推进新订阅，重新 poll 最终无遗漏。
- 资源测试覆盖 17th WS、第二 subscription 替换、超大 WS/HTTP body、outbound
  message/bytes、poll semaphore、command/query 加权调度和 overload backoff；命令在
  多连接 polling 下仍有界进展。
- 分别填满 capacity=1 的 command/query queue：断言 HTTP/WS command、Artifact query、
  poll query 的上述稳定满载契约，以及 8:1 调度下双方均有进展。
- PWA 静态资源、manifest、service worker、CSP/MIME/cache 策略正确；service worker
  Cache API key 测试只含精确静态 allowlist，绝不含 `/v1/` 或带 query URL。
- 最小 UI 的命令映射与协议 DTO fixture 测试通过，源码无 `innerHTML`、token
  localStorage/URL 或前端业务状态写入。
- A3 no-oracle 加固：Project 存在但目标 hash 不可达与不存在 Project 同为相同 404。
- README 给出浏览器启动/bootstrap/恢复流程，明确开发 HTTP cookie限制和所有未实现项。
- L3 重跑 A1–A3 全部测试；fmt、clippy、tests、diff check 通过。

## 12. A4 门控与审查记录

| 阶段 | 状态 | 证据 |
|---|---|---|
| A4 设计审查 R1 | REVISE | `docs/reviews/a4-design-review.md`：written cursor 未定义、fan-out/入口无总体上限 |
| A4 设计审查 R2 | REVISE | `docs/reviews/a4-design-review-r2.md`：订阅缺 generation、双队列容量/满载契约未冻结 |
| A4 主流程裁决 | APPROVED | 补齐 generation 隔离、64/32 独立容量及各入口 overload 语义；两轮上限后按证据裁决 |
| A4 实施门控 | PASS | 2026-07-31：`cargo fmt --all -- --check`、`cargo clippy --all-targets --all-features -- -D warnings`、37 个 Rust 测试、5 个 Node 状态机测试、JS syntax、安全 DOM 扫描与 `git diff --check` 全通过 |
| A4 独立复核 R1 | REVISE | `docs/reviews/a4-implementation-review.md`：PWA 仍暴露原始 JSON/手填 cursor，poll overload 可能紧循环 |
| A4 独立复核 R2 | REVISE | `docs/reviews/a4-implementation-review-r2.md`：Lagged 错用 server delivered cursor，overload 测试未经过生产路径 |
| A4 主流程最终裁决 | PASS | 两轮复核上限后完成显式二次主审：真实生产 overload 路径已有回归；恢复状态机区分 `preserve_client_cursor` 与 `reset_to_snapshot`，Lagged/断线不覆盖客户端 processed cursor，future/epoch recovery 按 snapshot coverage 重置；对应 Node 与真实 WS 测试通过 |

### A4 与 Slice A RELEASE

A4 于 2026-07-31 通过 RELEASE，Slice A（A1–A4）至此完成。两轮独立最终复核均曾
给出 REVISE；修复后因达到审查轮次上限，由主流程按完整差异、失败路径回归及全部机械
门禁作二次裁决。残余风险是 PWA 浏览器自动化仍以状态机/源码契约测试为主，后续 Slice
D 的真实交互验收必须补充浏览器级 E2E。Slice B 从持久化聚合、Revision/Artifact
领域模型与单实例恢复开始，且必须持续重跑 A1–A4 的 L3 回归。

## 13. Slice B 分解与 B1 设计

### Slice B 子切片

1. **B1 — 领域内核与确定性投影**
   - typed IDs、CreativeBrief/ConstraintSet、不可变 ScoreRevision、Evidence；
   - Project DomainEvent 白名单、Revision DAG、Take/Branch head、生命周期投影；
   - 纯内存 ProjectCoordinator 的 branch-head CAS 和事务候选验证。
2. **B2 — 磁盘 Artifact Store**
   - metadata/blob 分离，streaming SHA-256、临时文件、文件与父目录 fsync、原子 rename；
   - put/get/verify/pin、Project reachability，并把 A3 fixture durability 升级为持久 Artifact；
   - 中断/损坏/orphan 故障注入。
3. **B3 — Project Event Log 与 Session Rollout**
   - 各自稳定 stream ID、epoch、schema、单调 sequence；
   - checksum 事务批次、持久幂等结果、损坏尾识别、禁止越过中间损坏；
   - full replay 与 checkpoint + tail replay 等价；待决 question/approval 重启重投。
4. **B4 — App Service/Coordinator 持久化集成**
   - 单实例私有 data root 与 Project 单写者；
   - Artifact → CAS → event fsync → projection → response 的崩溃一致路径；
   - HTTP/WS/CLI 共用持久状态，重启恢复、orphan 识别及全故障点测试。

B1 不提前宣称磁盘耐久或重启恢复；B2 不把 blob 存在冒充事件可达；B3 不执行
Provider/Alda；B4 仍使用 Fake Turn，但把 Slice A 的正式事实迁入持久边界。V1 迁移器
不适用于本仓库当前没有 V1 Session fixture 的事实来源；本项目以 A1–A4 wire/state
作为兼容输入，并保留 schema migration hook，不能制造虚假的“旧数据已迁移”证据。

### B1 领域边界

代码先保持单 crate，新增 `src/domain/` 与 `src/state/`；`domain` 只依赖 serde/hash
值类型，不依赖 Tokio、HTTP、Provider 或文件系统。只有 state/coordinator 能从命令
构造事件并改变投影。

#### ID 与值对象

- 独立 newtype：`ScoreId`、`BriefRevisionId`、`ConstraintId`、`TakeId`、
  `BranchId`、`RevisionId`、`EvidenceId`。现有 `ProjectId`、`ArtifactHash` 是
  protocol DTO；在 DTO → domain value 边界显式验证转换，不能把领域类型直接暴露成
  wire schema，也不能收紧旧 DTO 而破坏 A1–A4。
- wire/string 构造必须拒绝空值、控制字符、路径分隔符、`.`/`..`、过长值；内部生成器
  不依赖当前容器路径。
- `SchemaVersion` 非零；Project event schema 初始为 1。
- 所有集合进入 digest/projection hash 前使用显式稳定顺序，禁止依赖 HashMap 迭代顺序。

#### Brief 与 Constraint

- `CreativeBrief` 是不可变版本，包含 ID、Project、原始用户描述、结构化目标、编制、
  未决问题；后继 brief 通过事件引用前一版本。
- `Constraint` 包含 ID、BriefRevision、`Hard | Soft | Advisory`、可读描述、可选
  machine rule key 与版本化 `MusicalScope`。B1 最小 scope 仅为 `WholeScore`、
  `StablePart(stable_id)`、`MarkerRange(from, to)`；stable ID/marker 走同一值验证。
  coverage 代数固定为：相同 scope 覆盖自身，WholeScore 覆盖任意 scope，其余只在
  完全相等时覆盖；未知、空值或不可比较关系 fail closed。D 冻结更完整
  `MusicalAddress` 时只能版本化扩展，不能重解释 B1 事件。
- `ConstraintOutcome = Pass | Fail | Unknown | NotApplicable`。Hard 只有 Pass 或绑定
  同一 Constraint/Revision 的有效人类 Waiver 才满足；Unknown 永不升级为 Pass。
- B1 实现构造与投影，不声称已有真实音乐 Gate；fixture Evidence 的 producer 明确为
  `DeterministicFixture`。

#### Revision、Evidence 与生命周期

- MVP 一个 Project 显式拥有一个默认 `ScoreId`；这是 reducer 验证的 profile 不变量。
  DAG、Artifact/Evidence subject 和 snapshot 均携带 Score identity。
- `ScoreRevision` 字段：Revision/Project/Score/Take/Branch IDs、至少零个 parent、
  绑定的 BriefRevision、source Artifact hash、可选 IR Artifact、创建来源。
- Project 初始化原子创建默认 Score、Take 和 Branch，三者 ID 与空 head 进入
  `ProjectInitialized`；额外 Take/Branch 必须由显式事件创建。
- 根 Revision 必须无 parent；非根至少一个 parent。parent 必须存在且属于同一
  Project/Score；不能 self-parent、重复 parent 或成环。
- `TakeCreated` 冻结 Project/Score、Take ID、`common_base` 和原子创建的默认空
  Branch。`BranchCreated` 冻结 owning Take、可选 fork base 与初始空 head。新
  Take/Branch 的首次 Revision 可引用创建事实中的 fork base；普通后续 Revision
  必须以本 branch 当前 head 为唯一 parent。B1 不实现 merge，多 parent fail closed。
- `expected_head = None` 只匹配已存在且 head 为空的 Branch，未知 Branch 必须报错。
- Revision 创建后不可修改，不含 lifecycle status。`Draft/Candidate/Accepted/
  Rejected/Aborted` 由事件投影；正式 MVP 不提供 Publish，B1 没有 Published
  状态、事件或命令。
- Candidate 要求 source Artifact 为 `VerifiedDurable`、hash 一致且有 H0 Pass
  Evidence。`ArtifactRegistered` 是可重放的最终领域事实，但其构造器要求 opaque
  `VerifiedArtifactReceipt`；receipt 包含 hash/size/store commit identity，只能由
  Artifact Store 成功 verify/commit 后返回。B1 没有生产 Store/command，因此生产
  surface 不能产生该事件；domain test store 可产生同结构 receipt，并把事件放入完整
  测试事件流，从空状态 replay，不使用日志外初始状态。
  Accepted 只能从 Candidate 进入，所有 Hard Constraint 必须 Pass 或有当前 Revision
  有效 Waiver。
- `EvidenceEnvelope` 至少绑定 Evidence ID、Revision、subject hash、Constraint/H0
  scope、outcome、producer/method、Artifact refs、created_at。Evidence append-only；
  subject hash 不等于 Revision source 时拒绝。
- B1 timestamp 由命令显式注入并校验非空，保证 replay/hash 不读取系统时钟。
- `HumanActor` 只能由认证客户端身份映射构造；Agent、Provider、Tool 和 fixture
  actor 不能构造人类决定。Waiver 必须保存 human actor、非空 reason、能覆盖
  Constraint scope 的 waiver scope、timestamp、Constraint ID 与适用 Revision ID，
  且不能沿用到后继 Revision。Accept 必须保存 human actor、timestamp 与 decision
  note/source command；reducer 对非人类 actor 和字段缺失 fail closed。

#### Project 事件与投影

B1 白名单事件：

- `ProjectInitialized`
- `TakeCreated`
- `BranchCreated`
- `BriefRevisionCreated`
- `ConstraintDeclared`
- `FixtureArtifactDeclared`（仅 metadata；不满足 Candidate，不公开、不持久化）
- `ArtifactRegistered`（VerifiedArtifactReceipt 消费后产生的 durable reachability
  事实；B1 仅由测试 store 构造，B2/B4 接入真实 store，B3 可持久重放）
- `RevisionCreated`
- `EvidenceRecorded`
- `ConstraintWaived`
- `RevisionPromotedToCandidate`
- `RevisionAccepted | RevisionRejected | RevisionAborted`
- `BranchHeadAdvanced`

每个事件有 `schema_version`，事务候选有严格递增 Project-local sequence。Reducer 从空
状态逐事件验证同一不变量；在线路径必须调用同一 reducer，不能另写一套 mutation。
`ProjectSnapshot` 至少包含 Score identity、active Brief、Constraint 投影、Revision
DAG、各 Take/Branch head、accepted Revision、lifecycle、fixture Artifact availability
和最后 sequence。
canonical projection digest 使用版本化 canonical JSON 字段顺序与 SHA-256。

B1 明确区分 `FixtureOnly` 与 `VerifiedDurable` Artifact availability。Fixture 可用于
纯 reducer/DAG 测试，但不能被公开下载或满足 Candidate/Accept。测试 store receipt
与未来 B2 receipt 遵循相同消费语义：一个 receipt 只能登记对应 hash/size/store
commit identity；删掉 `ArtifactRegistered` 后 promotion 必须 fail closed。B2 实现真实
fsync/rename/verify receipt，B4 才把其接入生产 Coordinator；B1 不通过 command/wire
暴露 fixture 或 receipt。

Protocol 查询 DTO 在 `protocol.rs` 独立版本化定义，由 mapper 从 domain projection
复制白名单字段；禁止 `pub use domain::*`、直接序列化 domain event/snapshot/revision，
内部新增字段不会自动进入 wire。

### B1 Coordinator/CAS

`ProposeRevision` 输入固定包含：

- command ID 与 canonical payload digest；
- Project/Take/Branch；
- `expected_head_revision_id: Option<RevisionId>`；
- 完整不可变 Revision 候选、Artifact metadata 与 Evidence。

单写者按以下顺序构造一个内存原子事务候选：

1. 校验 command id/digest；相同 pair 返回已保存结果，不追加事件，不同 digest 返回
   `IdempotencyConflict`。
2. 校验 Score/Take/Branch 创建事实、Artifact metadata、Brief、parents、Evidence 与
   lifecycle prerequisites。
3. 再比较目标 branch 当前 head 与 expected head；不一致返回
   `CommitConflict { expected, actual }`，不追加任何事件。
4. 一次性对候选 events 用 reducer dry-run；全部成功后替换投影并缓存稳定结果。

B1 的缓存明确仅进程内；B3 才把 command/digest/stable reply 放入同一 durable
transaction batch。显式 fork 创建的不同 Branch 可基于同一 parent 创建候选；同一 Branch 的两个命令
只有一个 expected-head CAS 能成功。`take.select` 不移动 branch head。

### B1 错误与可观测契约

新增 typed domain errors：`InvalidDomainId`、`InvalidDomainValue`、`UnknownParent`、
`CrossProjectReference`、`CrossScoreReference`、`UnknownTake`、`UnknownBranch`、
`InvalidForkParent`、`DuplicateParent`、`RevisionCycle`、`UnsupportedMerge`、
`ArtifactHashMismatch`、`EvidenceSubjectMismatch`、`HardConstraintUnsatisfied`、
`InvalidLifecycleTransition`、`CommitConflict`、`ProjectionCorrupt`。用户输入错误不
panic；内部不变量破坏也返回错误并保留旧投影。

### B1 验收与门禁

- 构造器覆盖所有 ID/Brief/Constraint/Revision/Evidence 正反例和四态 Hard 语义。
- scope 覆盖表驱动测试包含相同 scope、WholeScore 父范围、不相交及未知/不可比较
  fail closed；scope 字段进入事件与 canonical digest。
- DAG 表驱动/属性式生成覆盖合法多代与合法跨 Take fork，以及 self/duplicate/missing/
  cross-project/cross-score/非法普通跨 branch parent、成环与 merge 拒绝。
- 从空事件 replay 默认及额外 Take/Branch；覆盖重复创建、跨 Project/Take branch、
  未知 branch 和已存在空 head 的首次 CAS。
- 同 branch 双提交固定只有一个成功；不同 branch 可并存；冲突、验证失败和 reducer
  失败均为零事件/零投影污染。
- lifecycle 覆盖 Draft→Candidate→Accepted、Rejected/Aborted 及全部非法反向转换；
  Unknown/Fail/错 Revision waiver、waiver 沿用、非人类 actor、空 reason、scope 不
  覆盖和非人类 Accept 均拒绝；B1 没有 Publish surface。
- lifecycle 的完整测试事件流包含 test store receipt 生成的 `ArtifactRegistered`；
  从空状态 replay digest 相同，删除该前置事实后 Candidate fail closed，FixtureOnly
  永远不能替代它。
- online projector 与从事件清空后 replay 逐字段和 canonical digest 相同；事件顺序
  或字段变化能改变 digest。
- command 幂等同 payload 返回原结果；不同 payload 冲突。
- protocol/CLI 至少提供 Project domain snapshot 与 revision list/read 的 typed 查询；
  mapper fixture 证明 domain 新字段不自动泄漏；B1 不新增会误称 durable 的 create
  命令，也不向 wire 暴露 FixtureOnly Artifact。
- L3：Slice A 的 37 Rust + 5 Node 回归、fmt、clippy、diff check 全通过。

### B1 审查记录

| 阶段 | 状态 | 证据 |
|---|---|---|
| B1 设计审查 R1 | REVISE | `docs/reviews/b1-design-review.md`：Score、Take/Branch/fork、人类决定、Publish 范围及 Artifact/wire 隔离存在重大缺口 |
| B1 设计审查 R2 | REVISE | `docs/reviews/b1-design-review-r2.md`：Constraint scope 不可判定，VerifiedDurable 前置无法从事件 replay |
| B1 主流程裁决 | APPROVED | 两轮上限后补齐版本化最小 scope coverage 与 receipt-gated、可重放 `ArtifactRegistered`；逐项闭合 R2 两个 reducer 矛盾后批准实施 |
| B1 实施门控 | PASS | 2026-07-31：fmt、clippy `-D warnings`、47 个 Rust 测试、5 个 Node 测试、JS syntax、安全 DOM 扫描与 diff check 全通过 |
| B1 独立最终复核 R1 | REVISE | `docs/reviews/b1-implementation-review.md`：live serde/capability 绕过、Draft reject 偏差、typed read surface 缺失 |
| B1 独立最终复核 R2 | REVISE | `docs/reviews/b1-implementation-review-r2.md`：M2/M3 已闭合，stored fact/replay 仍公开 |
| B1 主流程最终裁决 | PASS | 两轮上限后移除授权/Artifact fact 的 Deserialize，事件、apply/replay/from_events/events 均收窄为 trusted crate/test 边界；B3 必须另建 checksum/schema 验证 facade。最终全门禁通过 |

### B1 RELEASE

B1 于 2026-07-31 通过 RELEASE。交付包括纯 domain/state、不可变 Revision/DAG、
Take/Branch fork、receipt-gated Artifact 可用性、Human Gate、统一 reducer、内存
Coordinator/CAS/幂等、固定 projection digest，以及独立 v1 Project/Revision read DTO、
mapper 和 CLI 查询。现有 Project create 只原子初始化内存 B1 默认 Score/Take/Branch；
它不声称 durable。B2 开始实现真实磁盘 Artifact Store；B3 才定义可反序列化的 trusted
stored batch，严禁重新给 `ProjectEvent` 或授权事实直接派生公开 Deserialize。

## 14. 当前执行设计：B2 磁盘 Artifact Store

### 目标与边界

B2 把 A3 的进程内 fixture blob 与 B1 的抽象 `VerifiedArtifactReceipt` 落为真实本地
content-addressed Store。它交付 blob durability、verify/get/pin 与故障注入，但不把
blob 存在自动变成 Project reachability；只有 B4 Coordinator 消费 receipt 并在 B3
事实批次提交 `ArtifactRegistered` 后才可公开下载。B2 不实现自动 GC，不删除用户
Artifact，不接 Provider/Alda，不改变 Slice A 当前下载来源。

### 模块与线程边界

- 新增 `src/artifact_store.rs`，不依赖 HTTP/CLI/Provider；只依赖 domain hash/receipt。
- Store 核心使用同步文件 API，以便精确控制 file/dir `sync_all`；B4 从 async actor
  调用时必须进入独立有界 blocking worker，禁止在 Tokio App Service loop 直接 I/O。
- `ArtifactStore::open(root)` 只接受由 composition root 提供的绝对私有 data root。
  B2 单元测试使用 `tempfile::TempDir`；CLI/serve data-root 与实例锁留到 B4。
- root 不从 Project 名、MIME、用户文件名或 wire hash 拼任意路径；所有 blob path 只从
  已验证 SHA-256 lowercase hex 派生。

### 布局与权限

```text
<root>/
  artifacts-v1/
    blobs/sha256/ab/<64hex>
    staging/<random-128bit>.tmp
    pins/<64hex>.pin
```

- `artifacts-v1` 是 schema/layout 版本；未知 layout fail closed，不原地猜测迁移。
- B2 首发仅支持 Linux。absolute root 必须预先存在、词法规范且只含普通非空组件；
  从可信 `/` directory fd 开始逐组件 `openat(O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC)`，
  明确拒绝 `.`、`..`、symlink/magic-link 与非 directory；每一步都相对上一已持有 fd，
  不用一次普通 `open(root)`，因此中间父组件也不能被跟随。测试覆盖中间组件 symlink
  及逐级 open 间的 rename/replace race。最终 root fd 持有后，
  layout/staging/blobs/sha256/pins/shard 全部用 `mkdirat/openat` 相对可信父 fd 创建/
  打开，逐组件要求 directory/regular type 与 owner/mode，不再“先检查 path 再普通
  open”。blob/pin 用 `openat(...O_NOFOLLOW)`，安装用 `linkat` 在 staging/shard 两个
  已持有 fd 间 no-replace，列举与 unlink 同样 descriptor-relative。root 路径在 open
  后被 rename/替换不改变 capability；任何 component race 要么继续绑定原 inode，要么
  fail closed，不访问替换目标。
- Unix 新建目录 mode 0700、文件 0600（受 umask 只能更窄）；现有 root 必须为当前用户
  owned、非 group/world writable 的真实目录。非 Linux 返回 `UnsupportedSafety`，
  不提供安全或 durability 降级模式；跨平台适配另行设计。
- Store 永不跟随 caller 提供的相对路径、`..`、separator 或 URL；hash parser先于路径。

### put 与 receipt

`put(reader, expected: Option<{hash,size}>) -> VerifiedArtifactReceipt` 固定顺序：

1. 在持有的 staging dir fd 下用 OS 随机 128-bit 名和
   `openat(O_CREAT|O_EXCL|O_NOFOLLOW)` 建 0600 临时文件；
2. 有界 streaming copy，同时 SHA-256 与 checked `u64` size；默认单 Artifact 上限
   64 MiB，超过立即错误并清理 temp；0-byte 允许；
3. flush + `sync_all(temp)`，重新从 temp 流式读取并复算 hash/size，防止 writer/磁盘
   路径不一致；
4. expected hash/size 任一不符：返回 typed mismatch，清理 temp，不产生 receipt；
5. 计算 hash-only final path，创建/同步 shard 目录；
6. 若 final 已存在：必须从 shard fd 以 no-follow regular-file 校验其 hash/size；相同则去重并删除
   temp，不覆盖；不同/非 regular/symlink 返回 `ExistingBlobCorrupt`，保留现有对象；
7. 若不存在：`linkat(staging_fd,temp,shard_fd,hash)` 原子 no-replace；并发 loser
   必须从 shard fd 打开并验证 winner，绝不覆盖。成功/去重后 unlinkat temp；
8. `sync_all(final)`（若本次创建），并同步 shard fd；staging unlink 后同步 staging
   fd。只有 manifest、目录链与 final 全部完成 Linux durability policy 后创建 receipt。

Store 初始化必须先 durable commit `store-manifest-v1.json`：包含 canonical schema/
layout version、随机 128-bit instance ID、`linux_file_and_directory_synced` capability。
创建流程为 staging fd 下 create-new temp → 写/复读校验 → file sync → linkat 到 layout
fd 的固定 manifest 名 → layout fd sync → staging unlink+sync；并发 opener 校验 winner。
instance ID 跨 reopen 稳定，manifest 非 regular、symlink、未知字段/version、checksum
错误均 fail closed。首次创建每一级目录后同步新 directory inode，再同步其父 fd 中的
directory entry；从既有 durable root 到 layout/staging/blobs/sha256/pins 全链完成前
`open` 不成功。lazy shard 同样 mkdirat → sync shard fd → sync sha256 parent fd。

receipt 类型改由 `artifact_store` 模块拥有：`CommittedArtifactReceipt` 字段私有、无
Deserialize/Clone，只能由成功 put 返回；state 提供 consuming handoff 接受该 opaque
类型，外部只可读 accessor。domain 不再尝试给 sibling Store 授权构造器。receipt 固定
包含 canonical domain hash、size、layout version、store instance ID、commit identity
与 durability capability；commit identity 是上述 canonical fields 的 SHA-256，不含
绝对 root。

B1 `ArtifactRecord`/`ArtifactRegistered` 在 B2 升为 v1 audit fact，无损保存 hash、
size、layout version、store instance ID、durability capability 与 commit identity；
FixtureOnly 的 audit 为 None，VerifiedDurable 必须全部为 Some 且 canonical commit
identity 重算一致。Project reachability 仍只由 B4/B3 transaction 建立。B3 stored fact
保存这些审计字段本身，不保存 opaque Rust capability。

任何失败都 best-effort 清理本次 temp；清理失败返回主错误并附 typed cleanup warning，
不会把失败 temp 视为 blob。B2 open 扫描 staging，只报告/枚举 orphan，不自动删除；
B4 恢复流程按 age/实例锁治理。

### get / verify / pin

- `verify(hash)`：从持有的 shard fd no-follow 打开 final regular file，stream hash/size，返回
  `VerifiedBlob { hash,size }`；不存在与损坏明确区分。
- `get(hash)` 必须先完成同一次 open handle 上的 verify，再把该 handle rewind 后返回
  bounded reader/文件快照；禁止 verify path 后重新 open 造成 TOCTOU。B4 HTTP streaming
  通过 blocking adapter 使用该 handle，不把全 blob 读进内存。
- `pin(hash)` 仅在 verify 成功后以 create-new temp + sync + no-replace rename 写
  hash-only marker；实际采用 staging/pins fd 间 linkat，并同步 marker/pins/staging
  fd；幂等 pin 验证 marker 内容。B2 不 unpin/delete。
- `list_orphans()` 只列合法 staging regular files与未被任何事件引用的 blob需要 B3
  reachability 才能判定，故 B2 仅提供 `list_staging_orphans` 和 `list_blob_hashes`；
  不自行声称 blob 是 orphan。

### 错误与故障注入

typed errors至少：`InvalidRoot`、`UnsafeSymlink`、`UnsupportedSafety`、`InvalidHash`、`TooLarge`、
`ExpectedHashMismatch`、`ExpectedSizeMismatch`、`Io { operation }`、
`ExistingBlobCorrupt`、`BlobNotFound`、`BlobCorrupt`、`UnsupportedDurability`。
错误不暴露 secret/root 全路径到 wire；内部日志可用受限诊断路径。

测试专用 failpoints：

- after temp create/write/sync；
- 初始化每一级 mkdir/dir sync、manifest write/install/layout sync；
- after verify、before final commit；
- concurrent winner installed；
- after final install、before directory sync；
- pin temp/write/install；
- cleanup failure。

failpoint 不能编入产品可调用 wire，且每点验证：没有错误 receipt、既有 blob不变、成功
final 可 verify、失败 staging 可枚举。

### B2 验收与门禁

- fixed bytes 的 hash/size/commit vector；put→verify→get 字节一致。
- 同内容顺序/并发 put 去重为一个 final blob，各成功 receipt 指向同 hash；不同内容
  不冲突。
- expected hash/size mismatch、64 MiB+1、read error、temp sync、final install、dir
  sync 故障均无错误 receipt；故障点后的可见文件满足上述不变量。
- 预置 symlink root/shard/final、directory-as-blob、损坏 existing blob、路径穿越形态
  全 fail closed；可控 race 在 check/open/install/verify/pin/list 间替换父路径，操作
  必须继续绑定原 fd 或 fail closed。Store API 不接受任意 path。
- verify/get 使用同 handle 测试；校验后替换 path 不改变已返回 handle 内容。
- fixture metadata→同 hash VerifiedDurable 升级仍只能通过 receipt；外部 crate/serde
  不能构造 receipt 或在线 `ArtifactRegistered`。
- pin 幂等且只接受 verified blob；无自动 unpin/GC。
- L3 重跑 B1 与 Slice A；fmt、clippy、all targets/features、Node、diff check。

### B2 审查记录

| 阶段 | 状态 | 证据 |
|---|---|---|
| B2 设计审查 R1 | REVISE | `docs/reviews/b2-design-review.md`：路径 TOCTOU、完整目录/manifest durability、receipt 类型所有权与 audit fact 字段存在阻断 |
| B2 设计审查 R2 | REVISE | `docs/reviews/b2-design-review-r2.md`：M2–M4 闭合，初始 absolute root 中间组件仍可跟随 symlink |
| B2 主流程裁决 | APPROVED | 两轮上限后补齐从 `/` fd 逐组件 openat/no-follow 的 root 锚定与 race 测试，逐项闭合剩余 M1 后批准实施 |
| B2 实施门控 | PASS | 2026-07-31：fmt、clippy all-targets/features、57 个 Rust 测试、5 个 Node 测试、JS syntax 与 diff check 全通过 |
| B2 独立最终复核 R1 | REVISE | `docs/reviews/b2-implementation-review.md`：pin special-file blocking/unbounded read 与故障矩阵不足 |
| B2 独立最终复核 R2 | APPROVED | `docs/reviews/b2-implementation-review-r2.md`：pin 同 handle 有界验证及完整逻辑 failpoint 矩阵闭合 |

### B2 RELEASE

B2 于 2026-07-31 通过 RELEASE：Linux descriptor-relative Store、durable manifest/
instance identity、64 MiB streaming put、no-replace 并发去重、verify/get same handle、
pin/list、opaque receipt 与完整 audit handoff均完成。逻辑 failpoint 不等于真实断电；
B4 必须补进程终止恢复。`CleanupFailed` 当前只保留组合错误类别，不同时携带 primary 与
warning 细节，作为非阻断诊断债务记录。B2 blob 尚未接 App Service 或 Project
reachability，Slice B/MVP 未完成。

## 15. B3 分解与当前执行设计：B3a Project Transaction Log

### B3 分解

- **B3a**：Project 事务批次 framing、trusted stored-event 转换、append/fsync、尾损坏
  检测、幂等 stable reply 与 Project projection replay/checkpoint。
- **B3b**：独立 Session Rollout，把 A1/A2 的 Session/Turn/question/approval/terminal
  事实、命令幂等结果和 cursor epoch 持久化，并验证重启重投。

两条流不共享 sequence、stream ID 或 epoch；generic 代码只复用 byte framing/fsync，
不把 Project 与 Session 事件塞进一个万能 enum。B3a/B3b 都不接生产 App Service 写入；
B4 在单实例锁和 Coordinator 下集成。

### B3a 文件与 descriptor 边界

```text
<data-root>/state-v1/
  state-manifest-v1.json
  projects/<project-id-hash>/events-v1.jsonl
  projects/<project-id-hash>/checkpoint-v1.json
```

- Linux-only，与 B2 相同：data root 从 `/` fd 逐组件 no-follow 打开；受管目录
  mkdirat/openat、current uid、0700、逐级 fsync。文件 0600、no-follow regular。
- Project 目录名是 `sha256(ProjectId canonical bytes)`，不使用原始 ID；manifest/batch
  内保存并验证真实 Project ID，hash collision/mismatch fail closed。
- state manifest durable 初始化协议与 B2 等价，包含 schema/layout、随机 state store
  instance ID、durability capability 与 checksum；跨 reopen 稳定。
- `StateStore` 由 B4 不可复制的全局实例锁 lease 构造，并维护 mutex-protected
  `HashSet<ProjectKey>` writer registry。`open_project_writer` 在同一临界区原子
  check+insert，返回不可 Clone 的 `ProjectWriterLease`；writer Drop释放 key。相同
  Project 第二 writer（含线程竞态）拒绝，不同 Project可并行，repair与append持有同一
  lease。B3a 测试使用 sealed test instance lease。reader 可并存，但 writer fd 生命周期
  内不重新按路径 open。无 lease 的公开 API只能 scan/read。

### 事务行格式与 checksum

每行恰好一个 `StoredProjectBatchV1` JSON object 加 `\n`：

- `schema_version = 1`
- `stream_id`（稳定随机 ID）
- `epoch = 1`
- `transaction_id`
- `first_sequence` / `last_sequence`
- `command_record: Option<{ client_id, client_command_id, payload_digest,
  stable_reply_protocol_version, stable_reply_raw_len, stable_reply_base64 }>`
- `events: Vec<StoredProjectEventV1>`（非空；只允许 B1 whitelist）
- `previous_batch_checksum: Option<sha256>`，首批 None
- `batch_checksum`

checksum 固定为 canonical tuple
`("alda-project-batch-v1", schema, stream, epoch, tx, first, last, command, events,
previous_checksum)` 的 compact JSON bytes 后 SHA-256。stored collections 用 Vec + 明确
顺序，禁止 map/float/平台时间格式进入 canonical bytes。固定 byte/hash vector进入测试。
每行上限 1 MiB、每批最多 256 events；stable reply 最大 64 KiB。

stable reply 在 commit 前必须是 versioned `CommandReply` DTO 的 canonical compact
UTF-8 JSON bytes：parse 后协议/client command匹配，再用唯一 serializer重编码必须逐
字节相等。raw bytes上限 64 KiB，长度在 base64前计数；stored 用 RFC 4648 standard
base64无换行/固定 padding无损封装，记录 raw_len。scan解码、验证 raw_len、UTF-8、
DTO/canonical re-encode，但缓存和重试返回原始 decoded bytes，不 parse-reserialize。
固定 reply bytes/base64/batch bytes/checksum/recovered bytes五项向量逐字节相等。

sequence 必须与上一合法批次连续，`first = previous.last + 1`，
`last = first + events.len - 1`；事务/command ID 在 stream 内唯一。checksum chain 防止
重排、删中间行或拼接另一流。Project stream epoch B3a 固定 1；未来 compaction 改 epoch
必须新 stream/snapshot 协议，不原地重编号。

### Trusted stored-event 边界

- B1 `ProjectEvent`、HumanDecision、Waiver、Artifact audit fact 继续不公开 Deserialize。
- `state_store/project_codec.rs` 定义独立 `StoredProjectEventV1` 与 stored value DTO，
  `#[serde(deny_unknown_fields)]`，只用于已通过 line size、JSON、schema、stream、
  checksum chain 和 sequence 验证后的 trusted conversion。
- conversion 重新调用 ID/hash/scope/audit/decision构造器与 B1 reducer；不能把
  `authenticated_human: bool`、`VerifiedDurable` tag 或 stored event直接视为在线
  capability。trusted replay constructor只重建“过去已提交的审计事实”，不可通过
  live Coordinator mutation API调用。
- Store receipt仍是唯一 live `ArtifactRegistered` producer；stored audit字段重算
  commit identity。Human stored actor只重建历史决定；live Waive/Accept仍需要 B4
  authenticated principal。
- unknown event/schema、字段缺失/多余、无效转换、reducer拒绝均为
  `IncompatibleOrCorrupt`，不跳过。

### append 与恢复

append 在持有 writer/token 时：

1. 从已恢复 writer state验证 command/digest/idempotency与 Project reducer dry-run；
2. 构造完整 batch canonical bytes/checksum，验证大小/sequence；
3. 对 events file 单次 `write_all(line + "\n")`；禁止多 writer交错；
4. `flush` + `sync_all(file)`；只有 sync成功后 batch成为 committed并返回 stable reply；
5. 更新内存 projection/command index。checkpoint失败不回滚 committed log。

从首次可能改变 events fd 的 write开始，任何 write/flush/sync错误或 test failpoint都把
writer转为 typestate `PoisonedProjectWriter`；该值仍独占原不可复制
`ProjectWriterLease`，禁止 append、直接 repair或返回成功。`recover(self)` 消费
poisoned值、关闭旧 fd，在**不释放 registry key**时重新 descriptor-relative open/scan，
并返回三者之一：`ReadyProjectWriter`（完整合法 batch已出现并恢复stable reply）、
`RepairRequiredWriter`（持同一lease及valid/tail compare token）或
`CorruptProjectWriter`（持lease、只允许诊断/Drop）。`repair(self)` 只能消费
RepairRequired，compare-and-truncate成功后返回Ready；失败返回仍poison/corrupt状态。
只有这些 typestate Drop才释放 registry key，恢复窗口内第二 writer始终被拒绝。禁止
先Drop再open造成lease竞态，也禁止用旧 sequence/checksum续写不确定尾部。

同 command ID/digest 在已提交 command index中返回 exact stable reply bytes，不 append；
同 ID不同 digest返回 IdempotencyConflict。崩溃在 log sync 后、response 前，重试返回
批次内原 reply。

scan 从 offset 0 有界逐行：

- 完整 newline line必须 JSON/schema/checksum chain/sequence/reducer全部合法，否则整个
  open fail closed，绝不跳到后续行。
- EOF 最后一段没有 newline：报告 `RecoverableIncompleteTail { valid_bytes,
  damaged_bytes }`，只重放完整 prefix；自动 open不截断、不声称 clean。
- 最后一行已有 newline但 JSON/checksum错误视为 committed-area corruption，不按尾损坏
  忽略。
- `repair_incomplete_tail(expected_valid_bytes, lock_token)` 仅在重新 scan确认同一长度/
  digest且持有排他 token后 ftruncate→sync file→sync Project dir；返回 repair record。
  不提供“跳过坏中间行”。

### checkpoint

- checkpoint 是派生缓存，包含 projection schema、stream/epoch、covered sequence、
  covered batch checksum、canonical projection digest、ProjectSnapshot whitelist 与
  截至 covered batch 的完整 command index（client+command ID、payload digest、
  protocol version、raw_len、base64 exact reply）及 checksum。command index按
  `(client_id, command_id)`稳定排序并逐项重新验证 canonical reply。
- 写 temp→file sync→复读→link/rename no-replace versioned generation→Project dir sync；
  `checkpoint-v1.json` 可作为原子 pointer/最新 generation，但任何失败只丢缓存。
- load checkpoint必须验证 Project ID、stream/epoch、covered checksum确实在 log prefix、
  projection digest及 reducer schema；否则丢弃 checkpoint并 full replay。
- full replay 与 checkpoint + tail replay逐字段/digest/command index及reply bytes相同；删除 checkpoint
  可完整恢复。checkpoint绝不包含 opaque live receipt/human capability。

### B3a 错误与故障注入

typed errors：`UnsafeRoot`、`WriterLockRequired`、`StreamMismatch`、`SequenceMismatch`、
`BatchTooLarge`、`ReplyTooLarge`、`ChecksumMismatch`、`ChecksumChainMismatch`、
`MiddleCorruption`、`RecoverableIncompleteTail`、`IncompatibleSchema`,
`IdempotencyConflict`、`ProjectionRejected`、`Io { operation }`。

test backend/logical failpoints至少：

- manifest/Project dir/log create与各级 sync；
- batch before write、partial write N bytes、after newline before file sync、file sync error；
- after sync before in-memory update/response；
- checkpoint temp/write/sync/install/dir sync；
- repair rescan race、truncate、file/dir sync。

### B3a 验收

- 两批固定 canonical bytes/checksum chain vector；full replay重建 B1 snapshot与 exact
  stable reply。
- write前/partial/no-newline/sync前故障不返回 success；reopen只恢复 committed prefix并
  报明确 tail。sync后/response前故障重试同 command返回原 reply且不重复 Revision。
- 任一可能修改fd的 append错误后，不reopen直接第二次append固定返回 `WriterPoisoned`；
  sync报错但完整合法line可见时，重扫恢复为已提交或明确待修尾，不猜测。
- 中间 JSON/checksum/sequence/stream/project篡改、删行、重排、跨流拼接全部 fail closed；
  final newline corrupt不伪装 recoverable tail。
- repair必须 token + compare-and-truncate，race后拒绝；修复后 append sequence连续。
- checkpoint full vs tail replay相同；损坏/旧 schema/错误 covered checksum自动弃用但
  权威 log仍可恢复。
- stored serde不能进入 live mutation；字段无效的 Human/Artifact fact即使 checksum重算
  仍在 conversion/reducer/audit校验失败；字段完全合法且重算全链的同 UID离线改写不在
  checksum威胁模型。0700/current-uid data root是信任边界，checksum只承诺 accidental
  corruption/torn-write检测；如未来要抗同 UID篡改，必须另设计密钥/MAC生命周期。
- 同 instance lease同Project第二 writer、线程竞态、Drop后重开、不同Project并行和
  repair/append互斥；1 MiB/256 events/64 KiB raw reply、读错误与所有 failpoint。
- poisoned→recover/repair全过程第二 open拒绝；lease identity不变，Ready后sequence/
  checksum来自重扫结果；Corrupt/RepairRequired不能append，Drop后才允许重开。
- checkpoint前 command同 digest重投返回byte-exact reply，不同digest冲突，均不append。
- L3 B2/B1/Slice A、fmt/clippy/all features/Node/diff。

### B3a 审查记录

| 阶段 | 状态 | 证据 |
|---|---|---|
| B3a 设计审查 R1 | REVISE | `docs/reviews/b3a-design-review.md`：reply bytes、checkpoint command index、writer poison、checksum威胁模型与per-Project lease存在阻断 |
| B3a 设计审查 R2 | REVISE | `docs/reviews/b3a-design-review-r2.md`：poisoned writer恢复与不可复制lease缺少原子ownership transfer |
| B3a 主流程裁决 | APPROVED | 两轮上限后冻结持lease的 poisoned/recover/repair typestate，恢复全程不释放Project registry key，闭合最后竞态后批准实施 |
| B3a 实施门控 | PASS | primitive-only stored codec、exact reply、Project lease、poison/recover/repair、checkpoint 与全故障点实现；fmt、strict Clippy、65 lib + 6 main + 3 integration、5 Node 与 diff 全通过 |
| B3a 独立最终复核 R1 | REVISE | `docs/reviews/b3a-implementation-review.md`：stored constructor、recovery identity、受管对象权限/上限与故障矩阵四项阻断 |
| B3a 独立最终复核 R2 | REVISE | `docs/reviews/b3a-implementation-review-r2.md`：R1 M1–M3 闭合、M4 仅余 events create 缺 file-sync→parent-dir-sync |
| B3a 主流程最终裁决 | PASS | 两轮上限后补 `EventsFileSync` failpoint，并把 events open/create 收敛为 same-handle validate→file sync→Project dir sync；完整初始化矩阵及全门禁通过，批准 RELEASE |

B3a 于 2026-07-31 通过 RELEASE。它只发布 Project transaction log 与可信恢复边界；
生产 App Service 写入、Session Rollout、进程级实例锁和数据根接线仍分别属于 B3b/B4，
不得据此宣称整个 Slice B 或 MVP 已完成。

## 16. B3b 执行设计：Session Rollout

### 问题与边界

B3b 把 A1/A2 的 Session、Turn、Question、Approval 与 terminal facts 保存为独立
Session Rollout，使重启后 snapshot、cursor resume、待决对象及命令幂等回复可由事实
重建。它不把 Project event 混入 Session stream，不执行 Provider/Alda/副作用，也不
接生产 App Service；B4 才以单实例 Coordinator 把命令事务接入两个持久聚合。

wire DTO 不是可信存储模型。B3b 新增 `StoredSessionEventV1` primitive-only 白名单，
逐字段重新调用 Session/Turn/Question/Approval ID、choice、subject digest、payload 和
状态构造/验证；不能直接 Deserialize live protocol/domain capability。A1/A2 现有 wire
schema保持兼容。

### 文件、stream 与 writer ownership

- `state-v1/sessions/<sha256(SessionId)>/rollout-v1.jsonl` 与
  `session-checkpoint-v1.json`；目录和文件沿用 B3a fd-relative、current-owner、
  private mode、regular/nonblocking、有界读取和 file→parent-dir durability policy。
- MVP 保持 A4 wire 兼容：Session Rollout 的 `stream_id` **就是 canonical
  `SessionId`**，`epoch = 1`、sequence从1连续递增；不引入 snapshot 无法表达的随机
  stream ID。start→snapshot→resume→restart 固定向量必须证明该 identity 不变。目录 hash、
  stored Session ID、writer lease key三者必须一致；同 StateStore 内同 Session 仅一个
  writer，poison/recover/repair全过程持有同一不可复制 lease。
- Project 与 Session registry/文件分离；同名 ID 不能互相占用、拼接或重放。
- batch最多256 events、单行1 MiB、raw stable reply 64 KiB；每批含 schema、
  Session/stream/epoch、transaction ID、first/last sequence、previous checksum、
  optional command record、events与checksum。

### Session 事实与 reducer

stored event白名单逐一覆盖 A1/A2：

- `SessionStarted`、`TurnStarted`、`TurnCancelRequested`、`TurnCompleted`；
- `QuestionRequested`、`QuestionResolved`、`QuestionOwnerTurnAborted`；
- `ApprovalRequested`、`ApprovalResolved`、`ApprovalOwnerTurnAborted`。

纯 `SessionRolloutProjection` 从空状态 replay，至少保存 Session/Project ownership、
ordered Turns、完整 Questions/Approvals、head sequence与stream epoch。验证规则：

- stored `TurnStartedV1` 除 wire-visible `turn_id` 外，必须保存 canonical prompt
  （UTF-8、1..=8000 bytes）作为继续执行上下文；映射到现有 wire event时不泄漏prompt。
  projection保留该context，使 Question Pending重启后生成的approval subject digest与
  无重启路径逐字节相同。checkpoint只能缓存它，不能补造它。

- 首事件必须且仅能是匹配目录身份的 `SessionStarted`；之后禁止第二次开始。
- Turn ID唯一；cancel/complete必须引用已存在且未终止 Turn，terminal只发生一次。
- requested对象 ID唯一，owner Turn必须存在且未终止，created sequence由包络 sequence
  决定，stored payload不能伪造 sequence。
- resolved/owner-aborted必须引用同 Session 的 Pending对象并匹配 owner/digest；
  Question choice必须属于requested choices；Approval decision只允许 approved/denied。
- Turn `Succeeded/Failed/Cancelled` 前必须满足 A2 路径不变量；取消事件顺序固定为
  cancel requested→按创建 sequence 的owner-abort→completed cancelled。终态后禁止
  新待决对象或新的执行事实。
- `BudgetExceeded`与`AbortedByRestart`也是正式terminal。startup reconciliation由
  B3b提供纯计划器、B4提交单一持久batch：`Running`且无Pending对象追加
  `TurnCompleted(AbortedByRestart)`；`CancelRequested`先按created sequence追加所有
  owner-abort，再`TurnCompleted(Cancelled)`；`WaitingForInput`只有在恰好存在属于该
  Turn的Pending Question或Approval时保留并重投，否则判projection corrupt。Pending
  输入不因重启终止。reconciliation transaction ID为
  `restart-v1:<state-instance-id>:<session-id>:<pre-head>`，相同pre-head重复执行幂等；
  reconciliation写到一半再次崩溃时由batch原子性得到全有或全无。
- replay输出映射为现有 `SessionSnapshot`，与 A2 reducer逐字段一致；B3b 不增加 wire
  可见的隐藏状态。

### 事务、幂等与 cursor

- 一个用户命令产生的全部 Session facts与 canonical `CommandReply` 放入同一batch；
  batch `fsync`前不返回成功，sync后/response前重试返回原始reply bytes且不重复事实。
- command index key为 `(client_id, client_command_id)`；相同 payload digest byte-exact
  返回，同key不同digest冲突。已定位到可信既有Session、但没有新事实的稳定业务回复允许
  command-only batch：`event_count = 0`、`first_sequence = current_head + 1`、
  `last_sequence = current_head`（唯一空区间表示），不推进event head，但推进batch
  checksum/offset/transaction chain；连续空批、空批夹事件和checkpoint anchor均按
  batch checksum而非仅event head验证。
- `SessionStart`与无法定位可信Session的解析/not-found/ownership错误没有per-Session
  归档位置，明确不在B3b durable idempotency承诺内。B4必须先以control catalog事务分配
  Session并记录create command→Session ID，随后首个Session batch原子记录
  `SessionStarted`及相同stable reply；B4未完成前B3b不接生产create。已定位Session后的
  ownership mismatch写请求所声明的Session stream，不改写实际owner stream。
- cursor不是命令事务：`EventResume`从恢复后的权威 head读取，不写 command index。
  epoch不匹配、future cursor、错误stream kind/ID继续返回 A4 已冻结 typed recovery。
- checkpoint保存完整 projection whitelist、完整 command index、transaction IDs、
  covered offset/sequence/checksum及projection digest；从log prefix验证anchor后 replay
  tail。缓存损坏回退full log。
- retention/compaction不在B3b实现；epoch保持1且所有事件保留。因此不存在 retention
  gap，不能通过改epoch掩盖损坏。

### ID、目录发现与全局 ownership

- B4生产创建不再依赖进程内递增计数器：Session/Turn/Question/Approval ID使用带类型
  前缀的128-bit CSPRNG小写hex，并在相应 durable catalog/Session projection中碰撞检查；
  A1–A4现有开发内存fixture仍可保留可预测ID，不改变wire类型。
- B3b提供descriptor-relative `list_sessions()`：只枚举64字符小写hex目录，逐个以
  `O_DIRECTORY|NOFOLLOW|NONBLOCK`打开并验证owner/private，读取首批/检查点恢复canonical
  Session ID，重新计算目录hash并比对。异常目录、重复Session ID、跨Session重复
  Turn/Question/Approval ID全部使启动fail closed，不静默跳过。
- B4启动必须先完成全量Session catalog/owner index重建，再接受命令；owner index仅由
  replay事实生成，不从文件名或旧内存counter猜测。新ID生成后先查该索引与目标目录，
  冲突则重试有界次数，耗尽返回typed internal error。

### crash、repair 与故障注入

B3b复用 B3a 的 append poisoned typestate、完整newline坏行 fail closed、无newline尾部
显式repair、compare-and-truncate及checkpoint cache规则。故障矩阵逐项覆盖：

- Session目录/rollout创建及file/dir sync；
- batch before/partial/newline/file-sync/sync后response前；
- repair rescan race、truncate、file/dir sync；
- checkpoint create/write/file-sync/install/install后/dir-sync。

任一可能触碰rollout fd的错误都不得返回可继续append的Ready；恢复前第二writer仍拒绝。
初始化或checkpoint失败不得破坏已提交Session prefix。

### B3b 验收

- 固定 happy、deny、question-cancel、approval-cancel 四条 A2 event vectors；full replay
  的 snapshot、pending/terminal对象、sequence与在线 reducer逐字段相同。
- 重启在 Question Pending、Approval Pending、terminal 三个切点恢复；相同命令重投返回
  exact reply且不重复requested/resolved/completed。
- Question Pending重启后回答，生成的approval digest与不中断向量完全相同；prompt仅在
  trusted stored projection存在，不新增wire字段。
- `Running`、`CancelRequested`、合法/非法`WaitingForInput`、`BudgetExceeded`切点及
  reconciliation中途崩溃矩阵，验证pending保留、runtime work转
  `AbortedByRestart`、取消顺序与稳定transaction ID。
- choice/digest/owner/terminal顺序、重复ID、跨Session、schema/checksum/sequence篡改即使
  重算外层JSON也 fail closed。
- cursor从0、snapshot coverage、head、future、wrong epoch/stream/Session truth table
  与 A4一致；重启不改变stream ID/epoch/head。
- command-only拒绝batch与有事件成功batch均验证checksum chain、idempotency和崩溃点。
- 连续command-only、command-only夹事件、checkpoint跨相同event head的多个batch，
  full/tail replay必须得到相同batch anchor与command index；无可信Session错误明确不
  伪称durable。
- 多Session重启枚举重建owner index后继续随机分配ID；目录hash错配、重复owner ID、
  weak/special目录及有界随机碰撞重试fail closed。
- full replay与checkpoint+tail相同；坏checkpoint只丢缓存；所有资源上限和failpoints。
- L3：B3a/B2/B1/Slice A、fmt、strict Clippy、all-target/all-feature Rust、Node、JS
  syntax与diff全部通过。

### B3b 审查记录

| 阶段 | 状态 | 证据 |
|---|---|---|
| B3b 设计审查 R1 | REVISE | `docs/reviews/b3b-design-review.md`：stream identity、prompt事实、restart reconciliation、空batch/路由及全局ID/owner恢复五项阻断 |
| B3b 设计审查 R2 | APPROVED | `docs/reviews/b3b-design-review-r2.md`：五项均闭合；实施时未知Session目录必须fail closed，B4另证control catalog→首Session batch恢复 |
| B3b 实施门控 | PASS | 2026-07-31：primitive codec、strict reducer、event/command-only batch、checkpoint、restart、catalog与18项专属测试；fmt/check/strict Clippy、83 lib + 9 other Rust、5 Node、JS与diff全通过 |
| B3b 独立最终复核 R1 | REVISE | `docs/reviews/b3b-implementation-review.md`：terminal未绑定Approval事实，Approval digest未从authoritative context重算 |
| B3b 独立最终复核 R2 | REVISE | `docs/reviews/b3b-implementation-review-r2.md`：R1问题闭合，唯一遗留为正式BudgetExceeded没有可信存储事实入口 |
| B3b 主流程最终裁决 | PASS | 两轮上限后新增独立`TurnBudgetExceeded`可信stored fact；普通`TurnCompleted(BudgetExceeded)`仍拒绝，事实映射为连续wire terminal；完整门禁通过 |

B3b 于 2026-07-31 通过 RELEASE。B3（Project Transaction Log + Session Rollout）至此
完成存储基础，但仍未接生产 App Service；进程实例锁、control catalog、双聚合命令
协调、启动reconciliation及HTTP/WS/CLI重启恢复属于B4。

## 17. B4 执行设计：生产持久化集成

### 目标与非目标

B4 将现有 Fake Turn 纵切片接入 B2 Artifact Store、B3a Project log 与 B3b Session
Rollout，使 `serve`、HTTP、WebSocket 与 CLI 共享一个重启可恢复的本地事实源。B4 不引入
真实 Provider/Alda/播放，也不实现 C 的 Permission Broker 或 outbox；但所有 A1–A4
正式事实、B1 Project初始化/Artifact reachability 与命令回复必须走持久事务。

测试仍可显式构造内存 App Service；生产 `serve` 不得回退到内存事实源。开发状态与
README必须准确区分 Fake执行与持久性。

### 私有 data root 与单实例锁

- `serve` 接受显式 `--data-root <absolute-private-path>`；若未提供，启动失败并给出typed
  配置错误，避免隐式写用户目录。测试用0700 TempDir。
- 启动从 `/` fd逐组件打开root并验证current UID/0700；descriptor-relative创建
  `instance-lock-v1`（0600 regular/current-owner/private），以非阻塞排他 advisory lock
  持有到进程退出。锁文件内容仅为诊断性的pid/start nonce，获取锁后
  truncate→write→file sync→root dir sync；内容不能替代内核锁能力。
- 同一root第二实例立即返回`InstanceAlreadyRunning`，不得打开writers、bind端口或修改
  文件。锁能力不可Clone；它同时构造 B2/B3 store，替代B3测试lease。锁丢失或fd关闭后
  整个 durable App Service不可继续写。
- 先取得锁，再打开/验证 Artifact Store、StateStore/control log；全部恢复完成后才
  bind loopback与打印bootstrap code。
- ownership实现冻结为顶层 `DurableRuntime` 唯一拥有不可Clone lock fd，并私有拥有
  B2、B3、control与actor；它创建一个外部不可构造的共享`LockHealth`，各store只持
  `Weak<LockHealth>`并在每次生产写前upgrade/check。不得把线性lock值分别move给store，
  也不公开脱离runtime的生产writer构造。shutdown先停止入口/actor，drop所有writer/
  store，再标记health false并最后释放lock；任一锁健康检查失败进入Fatal。

### Control transaction log

跨 Project/Session 文件无法靠两个rename原子提交。B4新增
`state-v1/control/control-v1.jsonl`，沿用fd-relative private bounded checksum batch与
poison/repair规则，作为恢复协调WAL及全局命令/catalog事实：

- `ProjectAllocated { project_id }`、`SessionAllocated { session_id, project_id }`；
- `CommandPreparedV1 { global_tx_id, client/key/digest, exact_reply,
  stored_project_plan?, stored_session_plan?, artifact_audit_plans[] }`；
- `CommandCommittedV1 { global_tx_id, project_last?, session_last? }`；
- restart reconciliation prepare/commit。

Prepared内嵌完整versioned primitive-only `StoredProjectPlanV1` /
`StoredSessionPlanV1`：expected Project/Session identity、pre-head sequence/checksum、
transaction ID、完整stored event DTO bytes、command record关联与plan digest。control
codec逐字段调用B3专用trusted-plan converter，converter重新构造/验证events并确认
canonical bytes/digest；digest不是redo payload的替代品。固定Prepared bytes必须能在
空进程内存下重建逐字节相同B3 append request。

Prepared不序列化opaque `CommittedArtifactReceipt`，而保存primitive
`ArtifactAuditPlanV1 { hash,size,layout,store_instance,durability,commit_identity,
control_tx }`。B2新增窄的recovery-only consuming API：仅在live `DurableRuntime`
guard下，以same-handle重新verify blob hash/size，复算store manifest与commit identity，
并核对control tx plan后产生不可Clone `RecoveredArtifactCapability`；它只能被对应
StoredProjectPlan converter消费。错误store instance、替换blob、audit篡改或重复消费
全部fail closed，不能新增通用receipt Deserialize/Clone。

协议顺序：

1. 在单 App Service writer内验证命令、owner、expected heads与权限，生成稳定
   `global_tx_id`、聚合plans及canonical reply；涉及Artifact时先完成B2 put，所得blob此时
   仍可能是orphan。
2. create命令把Allocation、Prepared command record、exact reply与完整aggregate plan
   放在**同一个control batch/JSONL line/一次fsync**；不存在独立可见的Allocation先行
   状态。非create命令同样append+fsync一个完整Prepared。Prepared前崩溃没有权威命令，可安全重试；已产生
   blob由orphan扫描发现。
3. 按固定 `Project → Session` 顺序调用B3 append；每个以global tx派生transaction ID，
   已存在同tx且digest相同视为完成，不同则fail closed。
4. append+fsync `CommandCommittedV1`，然后更新内存catalog/projections并返回Prepared中
   exact reply。Committed不能先于所有目标aggregate。
5. 启动恢复逐Prepared检查：无Committed则验证各aggregate是否已追加，补齐缺失append，
   再写Committed；任一冲突/损坏使启动fail closed。重复恢复幂等。

B3a/B3b transaction index在B4前扩展为
`transaction_id -> { canonical_plan_digest, resulting_last_sequence,
resulting_batch_checksum }`，进入full replay与checkpoint。提供只读
`probe_transaction(tx_id, plan_digest) -> Absent | SamePlanCommitted |
ConflictingPlan`；重复append同plan可直接返回已提交结果，不同plan固定冲突。control
recovery不能靠head猜测或把任意重复tx当成功。

所有改变权威状态的生产命令（ProjectCreate、SessionStart、TurnStart/Cancel、
QuestionRespond、ApprovalRespond）都经过control WAL。纯query与EventResume不写WAL。
在路由前即可确定的InvalidRequest可不承诺durable reply；一旦Prepared存在，相同
client command key/digest跨重启必须返回同一bytes，不同digest冲突。control command
index解决Project/Session尚未存在时的create重试。

### Catalog、projection 与启动恢复

- control replay重建Project/Session catalog及全局command index；再调用B3
  `list_sessions()`和Project目录枚举，双向比对：catalog缺目录、未知目录、hash错配、
  重复owner均fail closed。B4为B3a补对称`list_projects()`。
- 对每个Project/Session打开writer并重建projection；完成所有Prepared事务后再次读取
  heads并发布不可变read view。writer由一个durable actor独占，不跨HTTP任务共享
  `&mut`。
- 运行B3b restart planner：Pending Question/Approval保留；Running/
  CancelRequested按§16生成control Prepared→Session batch→Committed。完成重协调前不
  接流量；重复启动不重复terminal。
- B2 `list_orphans()`以Project replay后的ArtifactRegistered hash集合判定：未引用blob
  仅报告，不在B4自动删除；staging按B2策略列出。HTTP下载只允许Project投影可达且B2
  same-handle verify成功。
- control checkpoint保存完整catalog、global command index、pending Prepared集合与
  covered checksum；损坏回退full replay。B4不做compaction，epoch保持1。

### App Service 适配

- 将现有 `ServiceState` 的命令“验证/规划”与“提交/投影”分离。内存backend继续用于
  A1–A4测试；`DurableBackend`实现同一内部trait，但提交必须经过上述WAL。
- durable创建使用CSPRNG typed IDs；内存fixture可继续使用可预测ID。回复与现有wire
  schema相同，客户端不感知backend。
- ProjectCreate同时提交B1 `ProjectInitialized`；SessionStart提交
  `SessionStarted`并写catalog；Turn/question/approval/cancel写B3b facts。
- Fake approval Approve路径：B2 put固定Alda fixture→验证receipt→B3a
  `ArtifactRegistered`（并在本切片创建最小合法Revision/Evidence/Candidate事实所需时
  必须完整满足B1 gates，不能只伪造reachability）→B3b ApprovalResolved/
  TurnCompleted，均由一个control Prepared协调。若A3 fixture尚不满足B1 Revision要求，
  B4只登记durable Artifact occurrence并保持A3 wire manifest，Revision生成留C；不得
  冒充Candidate。
- snapshot/list/read从恢复后的B1/B3b projection生成；WS继续使用written-ack cursor，
  durable actor提交后才广播。重启后新订阅从B3b权威head恢复，不依赖旧broadcast channel。
- actor command/query容量、8:1调度、HTTP/WS认证与资源限制保持A4不变；磁盘I/O用
  `spawn_blocking`或专用单writer线程，不能阻塞Tokio worker，也不能并行绕过writer。
- durable actor是显式 `Ready | Recovering(global_tx_id) | Fatal` 状态机。Prepared
  fsync后任一aggregate/control/projection错误立即离开Ready：停止处理后续命令、query与
  broadcast，所有入口返回typed `ServiceRecovering`；在仍持writer leases时只允许redo
  同一Prepared直至Committed并重建published view，或转Fatal并关闭listener。不能继续在
  半提交head上规划，也不能回退内存backend。所有query只读最近Committed read view，
  Recovering期间不返回旧view。

### 故障矩阵与验收

failpoints覆盖：lock create/write/file/dir sync；control init/prepare sync；每个aggregate
append前/后；control commit sync；in-memory projection前；response前；startup recovery
每一步；control checkpoint。每点进程终止后重开必须满足：

- 无Prepared：无权威状态，blob最多为可识别orphan；
- Prepared未完成：启动补齐且exact reply唯一；
- 两aggregate只完成一个：对外开放前补齐另一个；
- Committed：所有aggregate/reply/catalog一致，不重复events；
- 任何checksum/plan/head/receipt冲突：fail closed，不猜测或回滚已提交事实。
- 对每个不终止进程的I/O错误并发发起query/command，验证actor先进入Recovering、入口
  只返回unavailable且无广播；原地redo成功后一次性发布新view，失败则Fatal并停监听。

验收向量：

- 两次独立service进程（或真实drop/reopen composition root）完成
  ProjectCreate→SessionStart→TurnStart→QuestionRespond→Approval Approve/deny/cancel，
  每个切点重启后snapshot/event resume/reply bytes一致。
- create在control prepare/aggregate/commit/response各点崩溃后同command重投仍返回同
  Project/Session ID，不重复目录或事件。
- approval Artifact在blob commit后、Project append后、Session append后崩溃矩阵；
  恢复后Project reachability、Session terminal和download authorization全有或启动前
  补齐，永不出现对外半提交。
- 同root第二serve拒绝；不同root可并行；锁释放后可重开。启动失败不bind端口。
- catalog/目录双向不一致、control中间坏行、Prepared plan篡改、B3 stream冲突、
  artifact receipt篡改全部fail closed。
- restart reconciliation覆盖Pending/Running/CancelRequested并可重复崩溃；orphan报告
  准确且不自动删用户数据。
- HTTP、WS、CLI真实往返在drop/reopen后读到同一事实；A1–A4 overload/cursor/auth回归。
- fmt、check、strict Clippy、all targets/features、全Rust/Node/JS与diff门禁。

### B4 审查记录

| 阶段 | 状态 | 证据 |
|---|---|---|
| B4 设计审查 R1 | REVISE | `docs/reviews/b4-design-review.md`：redo plan、Artifact recovery capability、create原子绑定、B3 transaction probe、live隔离与lock ownership六项阻断 |
| B4 设计审查 R2 | APPROVED | `docs/reviews/b4-design-review-r2.md`：六项均闭合，批准按prerequisite→control/runtime→App Service集成增量实施 |
| B4a 前置能力门控 | PASS | Project/Session plan-aware transaction probe + checkpoint，B2 receipt-loss recovery audit capability；91 lib + 9 other Rust、5 Node及静态门禁通过 |
| B4a 独立复核 R1 | REVISE | `docs/reviews/b4a-implementation-review.md`：command幂等短路先于transaction probe，可隐藏同tx异plan |
| B4a 独立复核 R2 | APPROVED | `docs/reviews/b4a-implementation-review-r2.md`：Project/Session均先probe完整plan，Artifact recovery无回归 |
| B4c 增量设计审查 R1 | REVISE | `docs/reviews/b4c-design-review.md`：未知fsync耐久、restart identity冲突、Committed anchor未审计、Fatal/shutdown控制面缺失 |
| B4c 增量设计审查 R2 | REVISE | `docs/reviews/b4c-design-review-r2.md`：ordinary open仍可把未确认Clean boundary用于redo/probe，单一10,000 Prepared上限可耗尽restart reconciliation容量 |
| B4c 主Agent设计裁决 | AMENDED | `docs/reviews/b4c-design-adjudication.md`：源码证明ordinary open已在scan前同handle sync，故驳回R2 M1前提；接受容量冲突，并冻结双向transaction closure、Prepared后错误归置、双预算与v2 transport；这不是第三轮APPROVED |
| B4c C0实现验证 | pending | ordinary-open耐久确认、Fatal归置、双向索引闭合、restart预算及跨进程二阶crash门禁 |
| B4 实施门控 | pending | L1/L2/L3 |
| B4 独立最终复核 | pending | fresh-context reviewer |

## 18. B4c 增量执行设计：App Service 生产接线

### 现状校正与目标

B4a 已冻结 transaction probe 和 Artifact recovery capability；随后源码中的 B4b1 已实现
实例锁、control redo WAL、`Ready | Recovering | Fatal` runtime typestate、双 aggregate
redo/commit 与 committed read view，但它仍是 crate-private foundation。生产 `serve`
仍直接构造内存 `ServiceState`，没有 `--data-root`，也没有把 HTTP/WS/CLI 命令规划成
`PreparedTransactionV1`。B4c 的唯一目标是完成这条生产接线并结束 Slice B；不引入
Provider、Alda、Permission Broker、播放或 Slice C/D 能力。

本增量保持 §17 已批准设计，并冻结当前实现暴露出的三个补充事实来源：

1. B1 `ProjectInitialized` 不保存展示名称；A3 Project Artifact record 不保存 occurrence
   provenance。B4c 不篡改已冻结的 B1/B3 event schema，而是从 **已 committed 的 control
   Prepared exact replies** 重建 `ProjectSnapshot { name, version }` 与
   `ArtifactManifest` occurrence index。只有同时存在 committed control 事实、对应
   Project/Session replay事实且 Artifact hash 在 Project projection 中为
   `VerifiedDurable` 时才发布 read view。重复ID、Project/Session/Turn provenance不匹配、
   reply无法canonical decode或孤立metadata均使启动 fail closed。
2. 已定位可信Session后的 `TurnAlreadyTerminal`、`QuestionAlreadyResolved`、
   `ApprovalAlreadyResolved` 与 ownership mismatch 是无新事件的稳定业务回复。B4c允许
   `StoredSessionPlanV1` 在 `events.is_empty()` 时作为 command-only plan，但必须携带
   command record、引用已存在Session、保持相同pre-head且 append后不推进event head；
   create、restart reconciliation或无command record的空plan继续拒绝。
3. restart reconciliation 不能直接把 legacy `restart-v1:*` transaction 塞入 control
   plan。B4c 为每次 reconciliation 先冻结稳定 intent
   `restart-v1:<state-instance>:<session-id>:<pre-head>`，再计算
   `payload_digest = sha256(canonical_json("alda-restart-control-v1", intent, events))`、
   `global_tx_id = "global-" + payload_digest前32个hex`、Session aggregate transaction
   `<global_tx_id>:session`。内部 command identity 固定为保留的 typed namespace
   `client_id = __alda_internal_restart_v1`、`client_command_id = <intent>`，exact reply固定为
   reconciliation后canonical `CommandResult::SessionSnapshot`。所有HTTP/WS/CLI外部入口在
   idempotency查询前拒绝 `__alda_internal_` client前缀，内部构造器不接受普通
   `CommandEnvelope`。B3b trusted validator新增control-coordinated分支，逐字段重算上述
   mapping并要求相同command record；原有无command record、`restart-v1:*` transaction
   只作为legacy replay分支保留，不能由B4c新写。

### B4c0 — B4b1耐久、错误归置与启动闭合修订

- **每次open确认不变式**：JSON/checksum完整只证明line结构，不证明一次返回错误的`fsync`
  已耐久。普通Control、Project、Session open必须在任何scan/checkpoint replay前，对该次open
  得到的同一log handle成功`sync_all`；现有`open_or_create_control_log`、
  `open_or_create_events`与`open_or_create_rollout`已执行此门。在单实例锁与writer lease排除
  并发写入后，这次pre-scan sync确认随后scan可见的全部boundary，checkpoint只缩短replay而
  不能绕过它。普通control open确认失败使启动在任何aggregate I/O前失败；普通aggregate open
  确认失败发生在probe/read前并使启动失败，运行期Prepared后则进入Fatal。poisoned rescan
  不能复用先前open的确认，扫描得到Clean后仍必须对重新打开的同一handle再次成功`sync_all`
  才能返回Ready。append不创建目录项，Clean确认无需重复directory sync；incomplete tail仍走
  compare-and-truncate→file sync→directory sync repair。AfterSync错误也重复确认，不能依赖
  不可观察的失败阶段推断耐久。
- **Prepared后错误归置不变式**：control prepare开始写入后，若rescan本身失败而无法证明
  Prepared是否存在，必须保守Fatal；若rescan看见完整Prepared但Clean confirmation失败也必须
  Fatal。只有rescan+repair明确证明旧boundary上没有Prepared并仍持Ready writer时，命令才可按
  Prepared前拒绝处理。一旦Prepared已权威存在，后续codec/reducer或writer恢复失败，以及任何
  writer/lease/Artifact capability丢失，全部走typed Fatal并触发composition-root shutdown；
  不得返回仍可接受普通`recover()`的Recovering，也不得因空`Option`、poison或join路径
  `panic!`/`expect`。Recovering只允许用于
  明确分类为瞬态、runtime仍持有完成同一Prepared所需的全部writer/lease/capability、且能够
  从同一权威plan安全幂等重试的失败；它只暴露`retry_same_prepared`或转Fatal，不能普通reopen
  store、换writer、处理别的请求或发布旧view。新进程只能重新走上述ordinary-open确认门，
  不能把上一进程丢失的typestate当作证明。
- **双向transaction closure不变式**：启动最终发布前，不仅从control逐笔验证aggregate
  anchor，还必须枚举每个Project/Session replay得到的完整transaction index。control侧每个
  Committed Prepared的每个plan必须恰有一个同aggregate entry，且transaction ID、
  `canonical_plan_digest`、resulting sequence和batch checksum全字段相等；反向地，每个
  `global-<32 lowercase hex>:{project|session}` entry必须唯一归属于同global ID、同aggregate
  identity/类型的Committed control plan与anchor。expected缺actual、actual多global、重复归属、
  suffix/aggregate错配或任一字段不等均在bind前fail closed。
- aggregate index只白名单**逐字段通过既有trusted restart replay验证**的Session legacy
  `restart-v1:<state-instance>:<session-id>:<pre-head>`：它必须无command record、authorization/
  events/identity与该pre-head重算结果完全相等，且不能由B4c新写。格式相似但验证失败的
  `restart-v1:*`、Project中的legacy marker、以及任何非上述legacy且非control-owned global的
  production transaction一律未知并fail closed；测试/fixture transaction不得进入production
  root形成隐式白名单。
- 启动先把control Prepared按Project/Session aggregate分组并预建expected maps，再以control
  顺序处理各组；每个aggregate writer在一次startup中只open、确认和replay一次，在该writer上
  完成pending redo、restart planning/append、transaction枚举与最终projection快照。每个control
  transaction只在其所有目标aggregate结果收集齐后按control顺序补Committed；全部处理完成再
  执行最终双向集合比较。复杂度固定为
  `O(control Prepared + aggregate log/index)`，不得对每个Prepared重新open/replay同一writer，
  从而避免在物理20,000条Prepared上限附近退化为`O(N²)`。
- C0二阶崩溃门禁分别覆盖control prepare、Project、Session、control commit：第一次sync返回
  error、同进程rescan看见完整line、drop poisoned typestate、下一真实进程ordinary open再次
  注入confirmation sync失败或终止并丢弃未确认tail；必须断言无下游append/Committed/read
  view且无panic。Control/Project/Session均覆盖空/非空log及有/无checkpoint。另以完整line边界
  增删aggregate transaction，验证control→aggregate缺失、aggregate→control额外、错误legacy、
  digest/sequence/checksum错配都在bind前拒绝。

### B4c1 — Durable planning/read boundary

- 扩展 `DurableReadView`：B1 Project projections、B3b Session snapshots与全局owner index，
  以及从committed exact reply重建的wire Project/occurrence metadata。read view只在完整
  catalog校验、全部Prepared收敛和双向transaction closure通过后，从启动期间已确认且只
  replay一次的aggregate状态一次发布。
- `ReadyDurableRuntime` 提供crate-private窄接口：command lookup、CSPRNG typed ID分配、
  Project/Session current head与checksum、Session canonical prompt、cursor page、Artifact
  same-handle verified read、durable fixture put/audit plan。它不暴露Store/writer/lock fd。
- planner使用 `sha256(canonical_json(protocol_version, command))` 作为payload digest；
  相同client/key/digest直接返回control中exact reply，不重新分配ID或写Artifact；不同
  digest返回 `IdempotencyConflict`。
- 所有规划先在当前committed projection上完成完整验证，再构造
  `StoredCommandRecordV1`、Project/Session append request和control Prepared。任何失败
  在Prepared前不污染投影；Artifact put成功但Prepared失败最多留下可报告orphan。

### B4c2 — Durable App Service backend

- 保留现有内存 `ServiceState` 作为显式测试backend；新增独立 `DurableServiceState`，不在
  durable路径先调用/修改内存backend。两个backend共享wire mapping/validation helper，
  但权威mutation分别来自内存reducer与B3 trusted reducer。
- 六类生产mutation全部走control WAL：
  `ProjectCreate`、`SessionStart`、`TurnStart/Cancel`、`QuestionRespond`、
  `ApprovalRespond`。query、Initialize与EventResume不写WAL。
- durable ID固定为 `project|score|take|branch|session|turn|question|approval|occurrence-`
  加128-bit CSPRNG lowercase hex；在control catalog、Project/Session projection与
  occurrence index中碰撞检查，32次耗尽返回 `ServiceUnavailable`，禁止退回递增ID。
- `ProjectCreate` 的同一Prepared原子保存allocation、B1 `ProjectInitialized`、exact
  `ProjectCreated` reply；`SessionStart`同理保存allocation、`SessionStarted`与reply。
- `TurnStart`一个Session batch保存`TurnStarted(canonical_prompt)`与完整
  `QuestionRequested`；Question response保存`QuestionResolved`和完整Approval subject
  inputs/request；cancel按A2顺序保存cancel requested、owner abort、terminal。
- Approval deny只写Session facts。approve先把固定Alda fixture写B2，生成audit plan与
  B1 `ArtifactRegistered`，再用一个global transaction协调Project+Session；reply中的
  occurrence manifest标为 `DurableLocal`，其sequence绑定ApprovalResolved的第一条
  Session sequence。B4不创建Revision/Candidate。
- artifact manifest query从committed occurrence index读取；download还必须核对Project
  projection reachability并由B2 same-handle重新hash后才能返回。不存在Project与不可达
  hash保持相同404 oracle边界。
- `Recovering`期间actor不处理下一命令、query或broadcast；它只redo同一Prepared。
  仅当B4c0的完整writer/capability与已分类瞬态条件成立时，redo成功后才一次替换published
  view并返回原exact reply；rescan/confirmation失败或任一能力丢失直接Fatal、关闭所有channel，
  不经普通reopen自愈、不panic，也不回退内存backend。

### B4c3 — composition root、线程与重启

- `serve` 必须显式接收 `--data-root`；先同步打开/恢复 `ReadyDurableRuntime`，成功后才
  bind loopback和打印bootstrap。缺参数、非absolute/非0700、第二实例或恢复错误均在
  bind前失败。
- durable actor运行在专用blocking线程及current-thread Tokio runtime上；所有同步B2/B3/
  control I/O只阻塞该专用线程，不占用Axum worker。HTTP/WS handler只经已有bounded
  command/query channel通信，不持有runtime、Store或锁。
- composition root建立独立于sender引用计数的`watch`生命周期控制面，状态固定为
  `Running | Stopping | Fatal`。Ctrl-C/宿主shutdown先发送Stopping；actor在select中停止
  接收并明确拒绝/清空队列后退出。actor发生Fatal先发送Fatal再drop runtime；server的
  graceful-shutdown future监听同一状态，health在非Running时返回unavailable；每个WebSocket
  poll/write loop也监听状态并主动结束，不能靠router drop间接关闭。
- composition root唯一拥有actor OS thread `JoinHandle`；Axum停止后必须同步join，不允许
  detach。actor退出触发`RuntimeCore::Drop`，按writers/stores→health false→instance lock
  释放。panic作为serve错误传播；生产进程不以超时detach，真实进程测试用外部5秒watchdog
  将超时判为门禁失败。join完成后同data root必须可立即重开。
- startup在bind前完成所有pending Prepared redo和B3b restart reconciliation；Pending
  Question/Approval保留，Running→AbortedByRestart，CancelRequested→ordered abort+
  Cancelled。生成任何内部Prepared前先对所有Session预计算restart obligations并校验内部预算，
  防止处理部分Session后才发现容量不足；完成后执行严格catalog和双向transaction closure，
  再从同一轮已确认aggregate replay发布read view。

### B4c 错误、兼容与资源边界

- B4 production payload和transport同步冻结为version 2，并向`ArtifactDurability`追加
  `DurableLocal`；内存fixture仍返回`ProcessLifetimeFixture`，既有命令/result字段不变。
  production HTTP API固定为`/v2/bootstrap`、`/v2/commands`、
  `/v2/artifacts/{sha256_hex}`，WebSocket固定为`/v2/ws`且只协商`alda-agent.v2`。不提供
  `/v1/*` alias、v1 WS subprotocol、payload降级或v1/v2协商：v1 transport不路由/不upgrade，
  在v2 transport收到v1 envelope才返回typed `InvalidProtocolVersion`升级说明。CLI与PWA在
  同一变更中切换path、subprotocol和payload常量；production bundle/round-trip断言不含
  `alda-agent.v1`或`/v1/`，并覆盖两个durability值、v1 HTTP payload拒绝和v1 WS handshake
  拒绝。README记录无持久v1数据可迁移、客户端必须升级并重连，不能以closed enum或旧path
  名称声称兼容。
- Prepared前的typed业务拒绝保持可用；Prepared后的错误严格按B4c0归置为安全重试同一plan的
  Recovering或typed Fatal。HTTP在仍存活的Recovering窗口映射`ServiceUnavailable`/503；Fatal
  立即驱动listener/WS关闭，错误均不泄漏磁盘路径、checksum或control内部细节。
- data root、队列、WS、event page、Artifact 64MiB及日志/Session资源上限沿用A4/B2/B3；
  B4c不做GC、retention或compaction，orphan只报告不删除。
- B4把10,000精确定义为**外部durable command实例终身预算**，另为trusted internal restart
  transaction保留独立的最多10,000预算，control物理Prepared上限为20,000。外部和内部按已
  验证command namespace/identity分别统计全部Prepared（pending与Committed都计数），replay与
  checkpoint分别验证`external <= 10,000`、`internal_restart <= 10,000`、`total <= 20,000`；
  伪造/未知internal identity直接fail closed，不能借内部槽绕过外部预算。外部prepare只检查并
  消耗外部槽，即使内部已满也保有其第10,000个外部名额；restart planner反之只消耗内部槽。
- 内部预算的有界证明冻结为：每个restart obligation必须来自某一已replay验证的外部状态变更，
  单个外部batch最多产生一个需要terminal reconciliation的Session状态；相同pre-head反复崩溃
  派生同一intent/global tx，不能新增Prepared；内部terminal事件不会产生Running或
  CancelRequested，因此完成后若没有新的外部mutation就不能再产生obligation。故内部终身数量
  不超过外部终身数量，独立10,000保留足够且不会自激增长。达到外部上限在Prepared前返回typed
  `ServiceUnavailable`和“需要新data root/未来compaction”；内部上限不应在合法root上先于该
  证明触发，若replay计数/因果不变量被破坏则启动Fatal而非跳过Session。不得删除Committed
  Prepared/exact reply/metadata；Slice E发布门重新裁决compaction与产品容量。

### B4c 增量门禁

1. **C0 durability/startup**：poisoned与ordinary open（含checkpoint）的Clean durability
   confirmation；Prepared后rescan/confirmation/writer-loss必为Fatal且无panic；restart legacy/
   coordinated mapping；按aggregate单次open/replay的双向transaction closure；截短/额外
   aggregate及跨进程二阶crash矩阵。此实现验证仍为下一门，通过后才做C1。
2. **C1 planning/read**：fixed command digest、committed-reply metadata rebuild、
   command-only plan、owner/cursor/prompt/read-view篡改负例；fmt/check/strict Clippy及全回归。
3. **C2 backend**：内存/持久语义向量覆盖happy/deny/question-cancel/approval-cancel、
   same command exact reply、different digest conflict、Artifact双aggregate与download；每个
   failpoint后验证Ready/Recovering/Fatal及零半发布。
4. **C3 production E2E**：真实 `serve --data-root` 或等价production composition root
   两次drop/reopen，HTTP+WS+CLI在每个切点snapshot/cursor/reply一致；无data-root、弱权限、
   第二实例均在bind前失败；保持空闲WebSocket时Stopping、actor Fatal两条路径都必须在5秒
   watchdog内停止listener/WS、join线程并释放锁。HTTP/WS/CLI/PWA只走v2 transport；v1 path、
   subprotocol与payload负例必须按冻结契约拒绝。
5. **容量边界**：外部9,999/10,000/10,001、内部9,999/10,000、物理19,999/20,000/20,001
   分别验证prepare与replay；覆盖“外部已10,000但多个Session待reconcile”“内部保留已被使用但
   外部尚有最后名额”、多Session一次preflight及每个restart crash点，断言不会部分收敛后卡死、
   外部不能消耗内部槽、重复启动不增加internal计数。
6. 最终L3：`cargo fmt --all -- --check`、`cargo check --all-targets --all-features`、
   `cargo clippy --all-targets --all-features -- -D warnings`、
   `cargo test --all-targets --all-features`、Node tests、JS syntax、安全DOM扫描、
   `git diff --check`全部通过。
7. 门禁通过后另起fresh-context subagent，按用户要求对 Slice A+B 的整体架构、持久事实
   边界、并发/资源生命周期及C阶段可演进性做独立复核。任何重大问题先REVISE和修复；
   只有复核approved后才登记B4/Slice B RELEASE并进入Slice C。
