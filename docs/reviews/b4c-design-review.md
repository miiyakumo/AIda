---
verdict: revise
scope: design
artifact: /home/mii/code/draft/docs/plans/mvp-deliberative-execution.md
report: /home/mii/code/draft/docs/reviews/b4c-design-review.md
reviewer: B4c fresh-context independent design reviewer
date: 2026-08-01
---

# B4c App Service 生产持久化接线独立设计审查

## 结论

B4c 当前设计不可直接进入实施，判定为 `revise`。

本轮发现四项重大问题：B4b1 的 poisoned recovery 会把“校验完整但未确认 fsync”的
control/aggregate 行当作 durable fact；restart reconciliation 的现有 trusted reducer
契约无法装入 control Prepared；启动恢复没有闭合已 Committed control 锚点与 aggregate
transaction index；Fatal 与 graceful shutdown 也没有可终止 listener、WebSocket 和 actor
线程的控制面。前三项可分别造成跨文件半提交、无法实现 restart 接线、或把丢失 aggregate
事实的 control exact reply 发布为权威状态；第四项会让 Fatal 后仍继续监听，或 shutdown
永久等待 sender clone 而不能释放实例锁。

§18 对 metadata provenance、command-only reply 和生产 composition root 的方向是合理的，
但“B4b1 已实现 control redo WAL/双 aggregate redo”这一前提不能只按结构完整性成立。应先把
以下修订写入增量设计并加入确定性门禁，再实施 App Service 接线。

## 重大问题

### M1 — fsync 失败后的完整行被当作 durable commit，破坏 WAL 顺序

**具体位置**

- 设计要求：`docs/plans/mvp-deliberative-execution.md:1453-1460`、
  `docs/plans/mvp-deliberative-execution.md:1521-1531`；§18 却在
  `docs/plans/mvp-deliberative-execution.md:1567-1569` 把现有 control redo/双 aggregate
  redo 当作已完成前提，C2 仅笼统写“每个 failpoint”
  （`docs/plans/mvp-deliberative-execution.md:1664-1666`）。
- Control：`alda-agent/src/control_store.rs:695-705` 在 file sync 失败后返回 Poisoned；
  `alda-agent/src/control_store.rs:912-923` 只要重扫得到 checksum 完整行就直接返回 Ready，
  没有重新 `sync_all`。`alda-agent/src/durable_runtime.rs:378-395` 随后把该行作为 Prepared
  进入 Recovering，并可开始 aggregate redo。
- Project/Session：`alda-agent/src/state_store/mod.rs:742-752`、
  `alda-agent/src/state_store/session.rs:1981-1991` 同样在 sync 失败后 Poison；对应重扫
  `alda-agent/src/state_store/mod.rs:1555-1566`、
  `alda-agent/src/state_store/session.rs:2796-2807` 也仅凭完整行返回 Ready。Runtime 在
  `alda-agent/src/durable_runtime.rs:890-899`、
  `alda-agent/src/durable_runtime.rs:941-950` 看到同 plan transaction 后即视为 aggregate
  已完成，继而允许 control commit。

**实际证据**

现有测试明确冻结了这一行为：

- `state_store::tests::append_file_sync_error_never_returns_success_and_requires_rescan`
  在 `alda-agent/src/state_store/mod.rs:2828-2870` 断言 file-sync error 后完整可见行经重扫
  即成为 transaction/command 事实；
- `control_store::tests::completed_line_failpoints_recover_as_committed_and_retry_exactly`
  在 `alda-agent/src/control_store.rs:1866-1893` 把 `AfterNewlineBeforeSync`、`FileSync` 与
  真正 `AfterSync` 三种情况等同为 committed。

本审查实际运行以上两项测试，均通过；这证明不是假设中的边缘路径，而是当前预期语义。
checksum/JSONL 完整只能证明写入 page cache 的字节自洽，不能证明失败的 fsync 已建立崩溃
耐久性。

**影响**

若 control Prepared 的 sync 失败后重扫为 Ready，进程可先 fsync Project/Session，再在
control commit 前崩溃；重启时 Prepared 可能消失而 aggregate fact 留下。非 create 命令
甚至不会触发 catalog 目录不一致，可能把无 control command/exact reply 的状态发布出去。
反向地，aggregate sync 失败后重扫为 SamePlanCommitted，runtime 可 fsync control
Committed；随后崩溃可能留下 durable Committed 而 aggregate 事务消失。两者都直接违反
§17 的 Prepared→aggregate→Committed 耐久顺序与“零半发布”。

**最小修复方向**

给 poisoned writer 保留“最后一次 append 的已确认耐久阶段/旧 valid boundary”，不能把
普通 scan 的 Clean 等同 durable。对 newline 完整但 sync 结果未知的目标 batch，必须在
任何下游 append/commit 前成功重新 `sync_all`（需要目录耐久时同时 sync 目录）并形成明确
的 `DurabilityConfirmed` 结果；否则保持 Recovering/Fatal，绝不能 probe 后跳过。也可在尚未
触及下游时按旧 durable boundary 截断，但必须由一条统一、可证明的规则决定。

C2 必须新增“sync 返回错误 → 重扫看到完整行 → 随即模拟进程终止/丢弃未耐久尾行 → reopen”
的二阶 crash 向量，分别覆盖 control prepare、Project、Session 与 control commit；只测试
单次 API 不返回 success 不足以验收 WAL 顺序。

### M2 — restart reconciliation 同时违反 aggregate transaction ID 与 command-record 契约

**具体位置**

- §18 要求 restart 使用内部 command identity 并走
  Prepared→Session→Committed（`docs/plans/mvp-deliberative-execution.md:1588-1591`），启动
  时执行该流程（`docs/plans/mvp-deliberative-execution.md:1646-1648`）。
- `PreparedTransactionV1::validate` 要求 Session plan 的 transaction ID 必须等于
  `session_transaction_id(global_tx_id)`，且 plan command record 必须与 control command
  record 相同（`alda-agent/src/control_store.rs:157-175`）；派生格式固定为
  `{global-<32hex>}:session`（`alda-agent/src/control_store.rs:1354-1368`）。
- 现有 restart trusted validator 则要求 transaction ID 逐字节等于
  `restart-v1:{state_instance_id}:{session_id}:{pre_head}`，并明确要求
  `batch.command_record.is_none()`（`alda-agent/src/state_store/session.rs:1136-1183`）。
- control redo plan 目前还拒绝空事件 plan（`alda-agent/src/state_store/session.rs:1517-1520`）；
  低层 append 虽已支持 command-only batch（`alda-agent/src/state_store/session.rs:2233-2245`），
  但这不能解决 restart 的反向约束。

**实际证据**

任何满足 control codec 的 restart Session plan 都必须携带 command record，transaction 为
`global-...:session`；任何满足当前 B3b restart validator 的 batch 都必须没有 command
record，transaction 为 `restart-v1:...`。不存在同时满足两组谓词的值。因此 §18 所描述的
restart Prepared 不是当前类型/API 上可构造的增量。

另外，“不能复用用户 command key”也尚无强制边界：`CommandEnvelope.client_id` 是无约束
字符串（`alda-agent/src/protocol.rs:9-33`），`StoredCommandRecordV1::new` 不拒绝
`internal-restart-v1`（`alda-agent/src/state_store/mod.rs:143-177`）。仅约定一个字符串并不能
建立内部 namespace。

**影响**

B4c 若不修改 trusted reducer 就无法启动含 Running/CancelRequested Session 的生产服务；
若简单放宽任一侧，又可能让普通用户 command 伪装为 restart authorization、破坏旧
`restart-v1` 日志的重放兼容性，或让用户先占用内部 command key 导致启动 fail closed。
这是启动恢复阻断，不是局部实现细节。

**最小修复方向**

在 §18 明确一套唯一的 restart identity mapping：stable restart intent、control global tx、
Session aggregate tx、internal command key 和 authorization 如何相互派生及验证。同步修订
`PreparedTransactionV1` 与 B3b trusted restart validator，使新的 control-coordinated
restart 可携带内部 command record，同时继续重放既有无 command record 的 legacy
`restart-v1` batch；不得用无条件放宽 `command_record`/transaction prefix 代替绑定。

内部 key 必须使用类型化 namespace，或在所有外部入口强制拒绝保留的 client-id/prefix；
还需定义内部 canonical reply 的具体、可验证 schema。门禁至少覆盖 legacy replay、用户尝试
保留 identity、prepare/Session/commit 每个 restart crash 点及重复启动。

### M3 — 启动只 redo pending，没有验证已 Committed control 锚点仍存在于 aggregate

**具体位置**

- §18 从 committed exact replies 重建 Project/occurrence metadata，并要求对应 replay facts
  才发布（`docs/plans/mvp-deliberative-execution.md:1576-1582`）；read view 的发布门槛只写成
  catalog 校验和“全部 Prepared 收敛”（`docs/plans/mvp-deliberative-execution.md:1593-1597`）。
- 当前启动仅枚举 `pending()` 并 redo（`alda-agent/src/durable_runtime.rs:324-330`）；
  `rebuild_published_view` 只做 catalog/目录数量映射再读取 projections
  （`alda-agent/src/durable_runtime.rs:782-829`）。
- Control replay 对 committed anchor 只验证存在性和 checksum 字符串形状
  （`alda-agent/src/control_store.rs:1289-1308`），不打开对应 aggregate 运行
  `probe_transaction(plan.tx, plan.digest)`，也不比较 probe 返回的 sequence/checksum 与
  `AggregateCommitV1`。

**实际证据**

Project/Session checkpoint loader 在日志短于 checkpoint anchor 时会退回 full replay；若
日志恰好在完整 JSONL 行边界被截短，full replay 可以得到一个结构合法的旧前缀。此时目录
仍存在、catalog 映射仍相同，而当前 `ReadyDurableRuntime::open` 不检查 committed
transaction index，无法发现 control 声称 committed 的末笔 aggregate transaction 已消失。
同理，metadata rebuild 若只检查最终 projection 中“有这个 hash/ID”，也可能被同 hash 的
另一 occurrence 或后续事实错误替代，不能证明本 global transaction 的对应事实存在。

**影响**

启动可发布 control exact reply/occurrence metadata，但 B3 缺少其事务；或发布旧 Session
snapshot 却继续对相同 command 返回新状态下已无法推出的旧 exact reply。Approval 双
aggregate 路径尤其可能产生“control Committed + Project 可达、Session terminal 丢失”或
反向组合，违反事实源闭合与下载授权原子性。

**最小修复方向**

把“strict catalog validation”扩展为 committed-transaction audit：在发布 read view 前，
对每个 committed Prepared 的每个 aggregate plan 调用 plan-aware probe，必须得到
`SamePlanCommitted`，且 resulting sequence/checksum 必须逐字段等于 control
`AggregateCommitV1`；Absent、ConflictingPlan 或 anchor mismatch 一律启动 fail closed。
metadata provenance 校验应绑定同一个 global tx 的 plan/events/anchor，而不是只在最终
projection 中按 ID/hash 搜索。

C1/C3 新增 Project、Session、command-only 和 Approval 双 aggregate 的“保留 control，按
完整行删除 aggregate 尾部”负例，分别覆盖有/无 checkpoint；这些用例必须在 bind 前失败。

### M4 — channel-drop 不能保证 Fatal 停监听或 graceful shutdown 释放锁

**具体位置**

- §17 要求 Fatal 停 listener（`docs/plans/mvp-deliberative-execution.md:1512-1517`）；§18
  只写 Fatal “关闭所有 channel”（`docs/plans/mvp-deliberative-execution.md:1631-1633`），
  shutdown 则依赖 drop router/service senders 后 join actor
  （`docs/plans/mvp-deliberative-execution.md:1640-1645`），没有定义反向通知或显式 stop 协议。
- 现有 runner 只有在 command/query sender 全部关闭时退出
  （`alda-agent/src/app_service.rs:389-425`）。`AppService` 可 Clone
  （`alda-agent/src/app_service.rs:113-121`），Axum `HttpState` 也 Clone 并持有它
  （`alda-agent/src/http.rs:177-205`）；每个升级后的 WebSocket 会长期持有完整 `HttpState`
  （`alda-agent/src/http.rs:349-352`、`alda-agent/src/http.rs:378-382`）。
- 当前 `/health` 无条件返回 ok（`alda-agent/src/http.rs:730-732`），production server 的
  graceful shutdown 只监听 Ctrl-C，既没有 actor Fatal→server shutdown 通道，也没有持有/
  join actor handle（`alda-agent/src/main.rs:383-396`、
  `alda-agent/src/main.rs:671-675`）。

**实际证据**

停止 accept/router 并不会自动销毁已经升级且仍空闲的 WebSocket task；它保留
`AppService` sender clone，runner 因而仍可阻塞在 `recv()`。反方向，actor 进入 Fatal 并
drop receiver 只能让请求得到 Closed/503，不能让独立的 Axum listener 停止；`/health` 仍会
返回 200。设计中的“drop senders”和“关闭 channels”都不能推出所声称的线程/listener
生命周期结果。

**影响**

Ctrl-C 可无限等待 actor，`RuntimeCore::Drop` 不发生，实例锁无法释放；Fatal 后进程仍开放
端口并宣称健康，违反 §17 的隔离边界。若 composition root 为避免挂起而 detach blocking
thread，又直接违反 §18“不能在后台遗留持锁writer”。

**最小修复方向**

为 durable composition root 设计显式、独立于 sender 引用计数的双向生命周期控制：

1. server/shutdown controller 可命令 actor 停止接收并拒绝/完成队列；
2. actor Fatal 可触发 Axum graceful-shutdown token，并使 health 反映 unavailable；
3. shutdown token 同时终止/关闭所有 WebSocket 与 polling task；
4. composition root 唯一拥有 blocking thread JoinHandle，并在确认 actor drop runtime/释放锁
   后完成 join，panic/超时有明确错误策略。

C3 必须加入“保持空闲 WebSocket 时 Ctrl-C”和“运行中 failpoint 令 actor Fatal”两个真实
进程测试：要求端口停止接受连接、线程退出、health 不再报 ok，且同 data root 随后可立即
重开。

## 重要但非阻断问题

### I1 — `MAX_PENDING` 实际是 10,000 条终身命令上限

`ControlProjection.prepared` 不会在 commit 后删除（`alda-agent/src/control_store.rs:250-259`），
而新 prepare 用其总长度执行 `MAX_PENDING = 10_000` 限制
（`alda-agent/src/control_store.rs:44-49`、`alda-agent/src/control_store.rs:1021-1026`）。§18
又要求 committed Prepared 永久承载 exact reply metadata 且明确不做 retention/compaction
（`docs/plans/mvp-deliberative-execution.md:1576-1582`、
`docs/plans/mvp-deliberative-execution.md:1657-1658`）。因此第 10,001 个 durable command 后服务
会永久拒绝所有新 mutation；名称“pending”掩盖了真实容量语义。应在 B4c 明确这是可接受的
实例终身硬上限及用户可诊断错误，或设计能保留 global command/exact metadata 的有界
checkpoint/compaction；不能在实现时简单删除 committed Prepared，否则会丢失 metadata 与
跨重启 exact reply。

### I2 — 在 protocol v1 的 closed enum 中追加 durability 值不是自动兼容

`ArtifactDurability` 当前只有 `ProcessLifetimeFixture`
（`alda-agent/src/protocol.rs:343-347`）；§18 在不升级 protocol version 的情况下追加
`DurableLocal`（`docs/plans/mvp-deliberative-execution.md:1650-1653`）。对宽松 JSON 客户端可能
可用，但对 generated/closed-enum v1 客户端会反序列化失败，也是 Rust exhaustive match 的
源兼容破坏。应把兼容承诺改成明确的客户端容忍策略并加入现有 JS/CLI round-trip，或升级
可协商 schema；不能仅以“字段未变”认定兼容。

## 已确认的正向设计点

- command-only 的底层 Session batch 语义已有事实基础：空 events + command record 会推进
  batch checksum/transaction index 而不推进 event sequence，现有
  `event_and_command_only_batches_preserve_head_chain_and_exact_reply` 测试通过。B4c 仍需补齐
  `StoredSessionPlanV1` codec 与 control/runtime 路径，而不是重做 Session batch 格式。
- Project name 与 occurrence provenance 不在 B1/B3 event schema 中，因此以 committed
  control fact 作为补充事实源是可接受的；前提是按 M3 将它绑定到同一 transaction 的
  aggregate commit，而不是只检查最终 projection。
- 单 actor、固定 Project→Session 顺序、Prepared 后隔离 Recovering、download 同 handle
  verify 和 bind-before-recovery 禁止项均应保留。

## 修订后放行条件

1. 在 §18 增加 M1 的“结构完整不等于耐久确认”恢复协议，并以二阶 crash failpoint 验证
   control/Project/Session 的 fsync-error 路径。
2. 冻结可由现有类型表达且向后兼容的 restart identity/authorization/command-record 契约，
   加入外部用户不可占用的内部 namespace。
3. 启动发布前逐笔审计所有 committed aggregate transaction 与 control anchor；完整行截短
   负例必须 bind 前 fail closed。
4. 增加显式 actor↔server shutdown/Fatal 控制面，真实进程测试空闲 WebSocket、listener
   终止、thread join 和锁重开。
5. 对 I1 的终身容量与 I2 的 v1 enum 兼容作出明确设计决定并补验收。

以上四项重大问题关闭前，B4c 不应进入生产 App Service 实施，也不能据此结束 Slice B。
