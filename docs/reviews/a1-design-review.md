---
verdict: revise
scope: design
artifact: /home/mii/code/draft/docs/plans/mvp-deliberative-execution.md
---

# A1 独立设计审查

## 结论

A1 是切片 A 的正确下一增量：现有实现只有同步 Project 命令、进程内单写者和 HTTP 往返，尚无 Session/Turn 或可恢复事件；先用确定性 Fake Turn 固定生命周期、幂等和 cursor 语义，能在不伪装 Provider 的情况下为后续审批、Web/PWA 和真实 Runtime 建立协议地基。

但当前 A1 设计应先修订再实施。存在两个重大协议问题：恢复失败后的指定动作在 A1 命令面中不可执行；重复取消的返回语义把命令幂等与业务幂等混为一谈。二者都会固化 wire DTO 和客户端行为，若留到 A2/A4 再纠正，会迫使 CLI、HTTP 测试和协议类型返工。

## 重大问题

### 1. cursor 失效时要求“重取 snapshot”，但 A1 没有可取得的 Session snapshot

**具体位置**

- `docs/plans/mvp-deliberative-execution.md:126-136`
- `docs/plans/mvp-deliberative-execution.md:140-144`
- `docs/design/mvp-design.md:190-196`
- `docs/design/mvp-design.md:429-433`
- `alda-agent/src/protocol.rs:28-32`

**实际证据**

A1 只新增 `session.start`、`turn.start`、`turn.cancel` 和 `event.resume`（计划第 126–131 行），却规定 stream kind、stream ID、epoch 或 sequence gap 错误时“指示客户端重取 snapshot”（第 143 行）。权威设计的恢复算法明确要求先取得 snapshot/version，再按 cursor 补读，并在 epoch/schema 错配或缺口时重取“对应 snapshot”（设计第 190–196 行）；最终验收也要求 Project/Session 分别恢复（第 429–433 行）。现有协议只有 `ProjectSnapshot`，没有 Session snapshot 或等价查询（源码第 28–32 行）。

因此，对 Session stream 返回的恢复指示在 A1 自身协议中没有可执行目标。客户端既无法获得新的 Session epoch/基准 sequence，也无法重建正在运行或已终止 Turn 的权威状态。仅重新 `session.start` 会创建另一个 Session，并非恢复。

**影响**

这不是“尚未实现完整 MVP”的范围问题，而是 A1 宣称验证的 cursor 恢复语义自身不闭合。若先编码 `CursorError { refetch_snapshot: true }` 而没有明确 snapshot DTO、基准序号和 Session 生命周期投影，A4 的 Web/PWA 重连和 B 的持久恢复必须改动协议返回、CLI 命令与测试；更严重的是客户端可能错误地把新 Session 当成旧 Session 的恢复。

**最小修复方向**

在 A1 设计中补入最小只读 `session.snapshot(session_id)`（或明确恢复等价物），并冻结其至少返回 `session_id`、`project_id`、stream `epoch`、snapshot 覆盖到的 sequence，以及 Turn 身份/状态。定义 cursor 错误携带机器可读的恢复动作和目标 stream，而不只是文字提示。A1 仍可保持进程内；B 再把同一契约持久化，不需要提前实现磁盘恢复。

### 2. “终态后重复 cancel 返回原幂等 reply”无法同时满足新的命令关联 ID

**具体位置**

- `docs/plans/mvp-deliberative-execution.md:132-135`
- `docs/plans/mvp-deliberative-execution.md:140-150`
- `docs/design/mvp-design.md:180-183`
- `alda-agent/src/protocol.rs:53-59`
- `alda-agent/src/app_service.rs:155-181`

**实际证据**

计划第 134 和 144 行规定终态后重复 cancel “返回原幂等 reply”。权威设计第 180–183 行定义的是命令级幂等：同一 `client_command_id` 与同一 digest 才返回原结果；同一 ID 不同 digest 必须冲突。现有 reply 必须回显当前 `client_command_id`（源码第 53–59 行），现有实现也仅在同一 `(client_id, client_command_id)` 命中时克隆原 reply（App Service 第 155–181 行）。

对同一已终止 Turn 使用一个新的 `client_command_id` 再发 cancel，不是同一命令重试。若真的返回首次 cancel 的“原 reply”，reply 会携带旧 command ID，破坏请求—响应关联；若把所有重复 cancel 都按同一幂等项处理，又与当前以命令 ID 为键的契约冲突。反之，若重复 cancel 被当作 not-found/invalid-state，计划要求的稳定终态也没有定义。

**影响**

HTTP 和 CLI 可依赖回显 ID 关联并发请求。模糊语义会导致实现者在“克隆旧 reply”“生成带新 ID 的相同业务结果”“返回 AlreadyTerminal 错误”之间任意选择，测试也无法给出唯一 oracle。A2 的审批取消和 C 的真实取消树会继承该歧义。

**最小修复方向**

明确分开两层：

1. 同一 `client_command_id` + digest 的传输重试，逐字返回已存 reply，不追加事件；
2. 新 `client_command_id` 对已终止 Turn 的业务重复取消，生成回显新 ID 的成功结果（例如 `TurnAlreadyTerminal { turn_id, terminal_status, terminal_sequence }`），同样不追加终态事件。

同时在 A1 验收中分别测试这两种情况，并验证终态事件恰好一次。

## 重要问题

### 3. cursor 边界与错误分类尚不足以形成确定性测试 oracle

**具体位置**

- `docs/plans/mvp-deliberative-execution.md:133`
- `docs/plans/mvp-deliberative-execution.md:143`
- `docs/plans/mvp-deliberative-execution.md:148-150`
- `docs/design/mvp-design.md:184-195`

计划定义 `sequence > after_sequence`，但没有冻结初始 sequence、空流返回、`after_sequence == head`、`after_sequence > head`、批量上限/续页游标，以及“gap”与“future cursor”的区别。A1 进程内保留全部事件时不会自然产生 retention gap，因此把所有越界都称为 gap 容易固化错误的恢复逻辑。

建议在实施前给出简短 cursor 真值表：合法空结果、合法增量、future sequence、epoch/schema mismatch、stream mismatch；如果 A1 不做截断，则明确 retention gap 留给 B/A4 的持久或有界存储测试。返回值应包含当前 epoch、head sequence 和是否需要 snapshot，避免客户端解析错误文案。

### 4. Fake Turn 的终态集合应明确是生产状态机的受限子集

**具体位置**

- `docs/plans/mvp-deliberative-execution.md:132`
- `docs/plans/mvp-deliberative-execution.md:148`
- `docs/design/mvp-design.md:192-195`
- `docs/design/mvp-design.md:213-217`

计划只说 Fake Turn “保持 running，直到显式取消”，验收只要求 terminal event，没有冻结状态/事件名称及允许的迁移。权威设计后续至少需要正常结束、等待用户、取消、失败、预算耗尽和 `AbortedByRestart`。如果 A1 使用临时的布尔 `running` 或专用 `FakeCancelled` wire 事件，C/B 会替换协议。

建议 A1 直接冻结最小生产兼容状态机和通用终态 envelope，Fake executor 只走 `Started -> CancelRequested -> Cancelled` 子路径；未实现的终态可先保留枚举或明确为向后兼容扩展。无需在 A1 实现 Provider、超时或重启恢复。

## 范围与顺序判断

- A1 没有因未包含 A2–E 而被否决；总体计划已明确保留这些切片。
- 选择 Fake Turn 而非立即接 Provider符合原始需求的可信边界，也与现有 App Service 单写结构相容。
- 把 WebSocket 订阅留到 A4、把磁盘持久恢复留到 B 是可接受的，只要 A1 先闭合“snapshot + cursor”语义，并明确当前 HTTP/CLI 是拉取式协议证据而非断线 E2E 完成。
- A1 不应提前引入 Project Revision/CAS、权限或 Artifact；这些不影响本次 Session/Turn 协议增量的可验证性。

## 建议的修订后门槛

修订 A1 后，设计门槛应至少能唯一回答：

1. Session snapshot 如何取得，覆盖到哪个 sequence；
2. 每类 cursor 输入返回事件、空结果还是机器可读恢复错误；
3. 同命令重试与新命令重复取消分别返回什么，reply 回显哪个 ID；
4. Fake Turn 使用哪些正式生命周期事件，哪些状态转换合法；
5. 每种路径是否追加事件，以及追加几次。

这些问题冻结后，A1 可进入实施。
