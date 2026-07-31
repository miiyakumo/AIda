---
verdict: revise
scope: design
artifact: /home/mii/code/draft/docs/plans/mvp-deliberative-execution.md
report: /home/mii/code/draft/docs/reviews/b4c-design-review-r2.md
reviewer: B4c round-2 fresh-context independent design reviewer
date: 2026-08-01
---

# B4c App Service 生产持久化接线独立设计审查 R2

## 需求覆盖结论

修订后的 §18 已实质关闭 R1 的 restart identity、Committed anchor audit 和
Fatal/shutdown 控制面三项阻断，并对 R1 的两个重要问题作出了明确决定：生产 wire schema
升级为 v2，`DurableLocal` 不再冒充 v1 closed-enum 兼容；control 容量也被明确为统计全部
Prepared 的 10,000 条实例终身硬上限。

但是结论仍为 `revise`。R1 的未知 fsync 耐久只闭合了同一进程内 poisoned writer 的
rescan 路径，没有闭合 poisoned 后进程退出、普通 startup open 再次看到完整脏页的路径。
此外，新增的“全部 Prepared 共用 10,000 上限”会与启动期必须写 control Prepared 的多
Session restart reconciliation 争用同一容量，存在合法状态无法完成启动恢复的直接冲突。
这两项都会破坏 §17 的重启可恢复事实源目标，属于重大问题。

## 重大问题

### M1 — durability confirmation 没有跨越进程边界，R1 M1 仍未完全关闭

**具体位置**

- 修订设计只要求 poisoned Control/Project/Session writer 在 rescan 为 Clean 后、返回 Ready
  前对同一 handle 再次 `sync_all`（`docs/plans/mvp-deliberative-execution.md:1604-1609`）；二阶
  门禁又显式包含“recovery sync 再失败/进程终止”
  （`docs/plans/mvp-deliberative-execution.md:1616-1619`）。设计没有规定下一进程的普通 open
  对 Clean 非空日志重新确认耐久。
- 当前 in-process recovery 确实已增加 sync：`alda-agent/src/control_store.rs:910-924`、
  `alda-agent/src/state_store/mod.rs:1551-1569`、
  `alda-agent/src/state_store/session.rs:2787-2810`。
- 但普通 startup open 仍只 scan→seek→Ready：control 在
  `alda-agent/src/control_store.rs:426-453`，Project 在
  `alda-agent/src/state_store/mod.rs:1491-1516`，Session 在
  `alda-agent/src/state_store/session.rs:2723-2750`；三个路径都没有 `sync_all`。
- `ReadyDurableRuntime::open` 得到普通 Ready control 后立即枚举 pending 并 redo aggregate
  （`alda-agent/src/durable_runtime.rs:289-326`）；aggregate probe 同样使用普通 open 得到的
  transaction index（`alda-agent/src/durable_runtime.rs:713-769`）。

**实际证据**

append failpoint 在写完 newline 后、真实 `sync_all` 前即可返回 Poisoned：control 的
`AfterNewlineBeforeSync`/`FileSync` 位于 `alda-agent/src/control_store.rs:678-705`，Project 的
对应路径位于 `alda-agent/src/state_store/mod.rs:725-752`。Linux 上进程退出不会清空 page
cache，因此下一进程可以由普通 open 读到 checksum 完整、但从未成功确认 fsync 的行；是否
在未来掉电后留存仍未知。

本轮实际运行的
`control_store::tests::completed_line_failpoints_recover_as_committed_and_retry_exactly` 和
`state_store::tests::append_file_sync_error_never_returns_success_and_requires_rescan` 均通过，
但它们都在同一进程调用 poisoned `recover()`；当前测试没有“drop poisoned writer，随后走
普通 open，并让 startup confirmation 再失败”的路径。源码中的普通 open 无 sync 也直接
证明该路径未被设计规则覆盖。

具体反例是：control Prepared 的首次 sync 返回错误后进程退出；新进程从 page cache scan
出完整 Prepared，随即 fsync Project/Session；若在 control commit 前掉电，aggregate 可留存
而从未被确认的 Prepared 可丢失。反向地，aggregate sync 失败后退出，新进程普通 open/probe
把它当 `SamePlanCommitted` 并写 durable control Committed，也可能留下缺 aggregate 的
Committed。后者虽会在再下一次启动被 anchor audit 拒绝，但当前进程已经写出了错误的 durable
control 顺序，不能靠事后 fail closed 代替 WAL 前置耐久证明。

**影响**

这仍允许 §17 禁止的无 control aggregate 或无 aggregate Committed 组合，破坏 exact reply、
Project reachability 与 Session terminal 的原子恢复。R1 M1 的根因是“结构完整不等于耐久
确认”；仅把确认放在保有 Poisoned typestate 的进程内，无法覆盖 typestate 随进程退出而丢失
的情况。

**最小修复方向**

把规则扩展为：任何普通 startup open 在把既有 Clean 日志中的最后有效 boundary 当作
authoritative、允许 control redo 或 aggregate probe 前，也必须对该已打开文件成功
`sync_all`；或者持久化一个可验证的 last-confirmed boundary，并只信任该 boundary。Control
必须在任何 aggregate redo 前确认，Project/Session 必须在 probe/生成 control anchor 前
确认。新增 ordinary-open failpoint：写完整行但不执行成功 sync，drop poisoned 状态，重新
open 时注入 confirmation sync 失败，断言未产生下游 append/Committed/read view；Control、
Project、Session 和有/无 checkpoint 都应覆盖。

### M2 — 10,000 条全 Prepared 上限可耗尽启动 reconciliation 的内部命令容量

**具体位置**

- restart reconciliation 被要求为每个需要处理的 Session 生成独立、带内部 command record
  的 control Prepared→Session→Committed（`docs/plans/mvp-deliberative-execution.md:1589-1600`、
  `docs/plans/mvp-deliberative-execution.md:1680-1682`）。
- 同一设计又冻结“每个 data root 最多 10,000 条 durable mutation”，并明确统计**全部**
  Prepared（`docs/plans/mvp-deliberative-execution.md:1696-1699`），没有给启动维护事务保留或
  单列容量。
- 现有 control projection 永久保留所有 Prepared，`MAX_PENDING = 10_000`
  （`alda-agent/src/control_store.rs:44-49`、`alda-agent/src/control_store.rs:250-259`），第
  10,001 个 Prepared 在 replay/append validation 中固定被拒绝
  （`alda-agent/src/control_store.rs:1023-1028`）。
- restart planner 按单个 Session 产生 plan（`alda-agent/src/state_store/session.rs:1214-1283`）；
  当前合法 B3b reducer/Stored plan 并不要求一个普通 command batch 最终必须是 Waiting 或
  terminal，因此现有 B4b1 data root 可以包含需要 reconciliation 的 Running/
  CancelRequested Session。

**实际证据**

构造一个 control projection 已有 9,999 个 Prepared、同时两个已 catalog Session 分别需要
restart reconciliation 的合法状态即可触发冲突。启动若顺序提交内部事务，第一个把计数推进
到 10,000，第二个在 Prepared 前被永久拒绝；若先整体 preflight，则启动只能在写入前失败。
两种实现都无法同时满足“全部 Session 在 bind 前 reconciliation 完成”和“全部 Prepared
绝不超过 10,000”。在恰好已有 10,000 个 Prepared 且仍有一个 restart plan 时冲突更直接。

外部命令达到容量时可以返回 typed `ServiceUnavailable`，但 startup reconciliation 没有
protocol caller，不能实现设计所说的 typed reply；要求用户换新 data root 也不能让旧 root
的 Pending/Running 事实完成恢复。此问题不是 ID namespace 或 command-record codec 冲突，
而是它们开始消耗 control 记录后暴露出的容量互斥。

**影响**

一个结构与 checksum 均合法、未丢数据的 root 可能永久无法 bind，或者若实现选择跳过剩余
reconciliation，则会发布设计明确禁止的 Running/CancelRequested 启动态。多 Session 场景
还可能先提交部分内部 reconciliation 后才发现容量不足，使重试虽幂等但仍无法收敛。

**最小修复方向**

冻结不互相冲突的双层预算。例如将 10,000 定义为外部 durable command 预算，同时给内部
restart transaction 设置独立、由可达 Session/外部 mutation 数推导的有界保留容量；control
物理上限覆盖两者并分别计数。若必须坚持“全部 Prepared 总数恰为 10,000”，则必须在任何
外部写入前动态保留完成所有潜在 restart obligations 所需的槽位，并明确旧 B4b1 root 已无
保留容量时的可恢复迁移/只读策略。C0/C3 门禁加入上限前一条、恰好上限、多个 Session 需要
reconciliation 的 drop/reopen 向量，并断言不会部分收敛后卡死。

## R1 四项阻断闭合评估

1. **未知 fsync 耐久：未完全闭合。** 同进程 poisoned recovery 的二次 sync 方向正确，
   但普通 startup open 未纳入 durability confirmation，见 M1。
2. **restart identity/command record：设计层已闭合。** stable intent、payload digest、global
   tx、aggregate tx、保留 client namespace、canonical SessionSnapshot reply 和 legacy 分支
   均有唯一映射；现有 `PreparedTransactionV1` 的 aggregate tx/同 command record 约束
   （`alda-agent/src/control_store.rs:149-176`）可通过新增 control-coordinated trusted 分支
   实现，而无需放宽 legacy replay。剩余阻断是 M2 的容量，不是 identity 不可表达。
3. **Committed anchor audit：已闭合。** §18 要求逐 Prepared、逐 aggregate 的 plan-aware
   probe 和 sequence/checksum 全字段相等；当前 C0 代码也已在 pending redo 后、read view
   前执行该审计（`alda-agent/src/durable_runtime.rs:320-327`、
   `alda-agent/src/durable_runtime.rs:803-836`）。设计还正确要求 metadata 绑定同一 global tx
   的 exact reply 与 plan event，而非只按最终 ID/hash 搜索。
4. **Fatal/shutdown 控制面：已闭合。** `Running | Stopping | Fatal` watch 独立于 sender
   引用计数，server、health、WebSocket、actor 和唯一 JoinHandle 的终止顺序均明确
   （`docs/plans/mvp-deliberative-execution.md:1671-1679`），并有空闲 WebSocket/Fatal 的真实
   进程 watchdog 门禁。该方案可在现有 Clone `AppService`/sender 架构上实现。

## 其他重要发现

### I1 — v2 schema 决策已关闭 enum 风险，但 transport 版本标识仍需冻结

§18 的 v2 决策足以关闭 R1 对 `ArtifactDurability` closed enum 的质疑：当前 enum 只有
`ProcessLifetimeFixture`（`alda-agent/src/protocol.rs:343-347`），在 `PROTOCOL_VERSION = 2`
下新增 `DurableLocal`、让 durable server typed-reject v1，并对 CLI/JS 做 round-trip 是一致且
可实现的。

不过当前 HTTP 路径仍是 `/v1/commands`（`alda-agent/src/main.rs:642-645`），WebSocket 路径/
subprotocol 是 `/v1/ws` 与 `alda-agent.v1`（`alda-agent/src/http.rs:49`、
`alda-agent/web/app.js:215`）。§18 没有明确这些 transport 标识是稳定且与 envelope schema
version 解耦，还是同步升级为 `/v2/*`、`alda-agent.v2`。这不必阻断设计，因为两种方案都可
实现，但实施前应冻结唯一选择，并让 HTTP/WS/CLI/PWA 测试断言同一契约；否则可能出现 v2
payload 继续在 v1 subprotocol 下运行、而“v1 返回 typed InvalidProtocolVersion”的承诺在
WS handshake 层含义不清晰。

## 验证充分性评估

本轮执行了以下只读/机械检查，均通过：

- 完整读取 `deliberative-execution/SKILL.md` 与 `reference/agent-rules.md`，直接核对 §17–18、
  R1 报告、README/项目状态和指定 B4b1/C0 源码；
- `cargo test --manifest-path alda-agent/Cargo.toml
  control_store::tests::completed_line_failpoints_recover_as_committed_and_retry_exactly -- --exact`；
- `cargo test --manifest-path alda-agent/Cargo.toml
  state_store::tests::append_file_sync_error_never_returns_success_and_requires_rescan -- --exact`；
- `cargo test --manifest-path alda-agent/Cargo.toml
  state_store::session::tests::event_and_command_only_batches_preserve_head_chain_and_exact_reply
  -- --exact`；
- `cargo test --manifest-path alda-agent/Cargo.toml
  durable_runtime::tests::combined_redo_publishes_both_aggregates_and_exact_reply_after_reopen
  -- --exact`。

四项测试各 `1 passed, 0 failed`。它们证明现有 in-process recovery、command-only batch 和基础
redo 可运行，但不覆盖 M1 的 ordinary startup open，也不覆盖 M2 的容量/reconciliation 交叉
边界，因此不足以推翻上述反例。仓库没有已登记的 `.agents/invariants/` 检查项。

## verdict 理由

`blocked` 不合适，因为两项问题都不需要用户决策或外部条件，可以通过局部冻结耐久 open
规则和容量预算修订解决。`approved` 也不成立：M1 仍能破坏 WAL 耐久顺序，M2 使强制启动
reconciliation 与硬容量上限在可构造状态下无法同时满足。故第二轮结论为 `revise`；按两轮
设计审查上限，应由主 Agent 基于本报告证据裁决或缩小范围，不应把本报告解释为自动开启第三轮。

## 不变式检查

未发现 `.agents/invariants/` 中已登记的不变式；无可执行 L3 invariant 项。§17 的关键设计
不变式中，Committed anchor、显式生命周期与 wire v2 决策保持，Prepared→aggregate→Committed
耐久顺序和“启动前全部 reconciliation 收敛”分别被 M1、M2 破坏。
