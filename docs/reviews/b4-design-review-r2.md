---
verdict: approved
scope: design
artifact: /home/mii/code/draft/docs/plans/mvp-deliberative-execution.md
reviewer: B4 fresh-context independent design reviewer R2
date: 2026-07-31
---

# B4 独立设计审查 R2

## 结论

B4修订设计可批准实施，verdict 为`approved`。

R1的M1–M6均已闭合。control WAL现在是可redo而非仅可验证的日志；Artifact恢复没有开放
opaque receipt反序列化；create allocation、exact reply和Prepared plan具有同一control
batch原子边界；B3 transaction index提供plan-aware三态probe；运行中半提交会把actor
隔离到Recovering/Fatal；顶层`DurableRuntime`唯一拥有实例锁及全部durable组件，store
不能脱离其health guard取得生产写能力。

本轮未发现新的重大矛盾。B4仍依赖在实施前完成明确列出的B3 transaction-index/probe与
B2 recovery-only API扩展；这些是设计内的前置切片，不是隐藏假设。B3a/B3b当前独立实现
复核尚有待主流程裁决的问题，因此B4最终RELEASE的L3必须以裁决后的B3基线为准。

## R1 M1–M6闭合核验

### M1 — 已闭合：Prepared保存完整primitive redo plan

- `CommandPreparedV1`内嵌versioned、primitive-only
  `StoredProjectPlanV1`/`StoredSessionPlanV1`。
- plan包含aggregate identity、pre-head sequence/checksum、派生transaction ID、完整
  stored event DTO bytes、command record关联及plan digest；digest只用于验证，不替代
  payload。
- control codec逐字段验证并调用B3专用trusted-plan converter，重新构造domain events、
  canonical bytes及digest。
- 验收要求固定Prepared bytes在空进程内存下重建逐字节相同append request，排除了重跑
  live命令、重新分配ID或从digest猜测payload。

### M2 — 已闭合：Artifact恢复能力不破坏opaque receipt边界

- Prepared只保存primitive `ArtifactAuditPlanV1`，明确不序列化
  `CommittedArtifactReceipt`。
- B2 recovery-only API只能在live `DurableRuntime` guard下调用；它以same handle重验
  blob hash/size，复核Store manifest/instance、durability、commit identity及control tx。
- 返回的`RecoveredArtifactCapability`不可Clone，只能由对应StoredProjectPlan converter
  消费；不能进入普通live mutation或wire入口。
- 错误Store instance、替换blob、audit plan篡改与同一能力重复消费均fail closed，既能
  redo Prepared-only崩溃，又不新增通用receipt Deserialize/Clone。

### M3 — 已闭合：create allocation与Prepared exact reply原子绑定

- create的Allocation、Prepared command record、exact reply和完整aggregate plan必须在
  同一个control batch、同一JSONL line、同一次fsync提交。
- 不存在Allocation先行可见或Prepared引用未分配catalog identity的中间control状态。
- control global command index提供create前无Project/Session stream时的command
  idempotency；Prepared后的重试必须返回相同ID及reply bytes。
- 验收覆盖prepare、aggregate、commit及response切点，要求不重复目录和事件。

### M4 — 已闭合：B3 transaction probe可区分完成、缺失和冲突

- B3a/B3b transaction index扩展为
  `transaction_id -> { canonical_plan_digest, resulting_last_sequence,
  resulting_batch_checksum }`，并进入full replay与checkpoint。
- `probe_transaction(tx_id, plan_digest)`明确返回`Absent`、
  `SamePlanCommitted`或`ConflictingPlan`；重复append同plan可返回已提交结果，不同plan
  固定冲突。
- control recovery按probe决定补append或跳过，禁止以aggregate head、last checksum或
  “任意重复tx”猜测成功。
- 该索引也为aggregate fsync后、control commit前的正常恢复切点提供确定证据。

### M5 — 已闭合：live半提交进入隔离恢复状态

- durable actor冻结为`Ready | Recovering(global_tx_id) | Fatal`显式状态机。
- Prepared fsync后任一aggregate/control/projection错误立即离开Ready；停止后续命令、
  query和broadcast，入口统一返回typed `ServiceRecovering`，不会提供旧read view。
- Recovering期间保留原writer leases，只能redo同一Prepared直至Committed并重建完整
  published view；失败转Fatal并关闭listener。
- 不允许在半提交head上规划后续命令，也不允许fallback内存backend。
- 故障矩阵要求每个非进程终止I/O错误期间并发query/command均unavailable且无广播，
  recovery成功后只一次发布新view。

### M6 — 已闭合：顶层runtime唯一拥有lock和durable graph

- `DurableRuntime`唯一拥有不可Clone lock fd，并私有拥有B2、B3、control和actor；不再
  尝试把一个线性lease move给多个Store。
- runtime创建外部不可构造的共享`LockHealth`，stores只持`Weak`并在每次生产写前
  upgrade/check；不公开脱离runtime的生产writer构造。
- 锁获取先于任何Store writer/bind；同root第二实例失败时不得修改文件或开放端口。
- shutdown顺序固定为停止入口/actor、drop writers/stores、使health失效、最后释放lock；
  任一health检查失败进入Fatal。

## 新矛盾检查

未发现阻断实施的新问题：

- control Prepared/Committed是协调WAL，不声称跨文件物理原子；对外原子性由恢复前不bind
  和运行时Recovering隔离实现。
- Project→Session固定append顺序与plan-aware probe结合，既允许redo，也不会把冲突日志
  当成已完成。
- Prepared前完成的B2 blob明确只是orphan；只有Project replay reachability与same-handle
  verify同时满足才授权下载。
- control catalog与Project/Session目录做双向核验，unknown/missing/hash mismatch和
  global owner重复均fail closed。
- read view、WS broadcast和written cursor只在Committed后发布，不会泄漏单aggregate
  半提交状态。
- B4没有提前引入真实Provider/Alda、Permission Broker、outbox或compaction，范围保持
  在Fake Turn持久集成。

## 非阻断实施注意

### I1 — recovery capability的“重复消费”应按对象而非永久plan解释

同一个不可Clone`RecoveredArtifactCapability`只能消费一次；但若它被消费后Project
append尚未fsync成功，Recovering必须能根据`probe_transaction == Absent`重新执行
same-handle验证并为同一Prepared plan取得新的单次能力。若probe为
`SamePlanCommitted`则不得再次mint/append。实现测试应覆盖这两个分支，避免把“重复消费
fail closed”误写成Prepared plan永久不可重试。

### I2 — lock health必须由composition root封装真实fd生命周期

`Weak<LockHealth>`只是一道API生命周期门，不能替代内核advisory lock本身。实现应确保
lock fd没有公开clone/close路径，health owner与fd同属一个不可拆分的runtime字段，并让
每次生产写同时依赖health与runtime私有构造边界。测试需包含模拟health失效后B2、B3和
control写入均拒绝。

## 实施门控

- 先完成并独立验证B3 transaction index/probe和B2 recovery-only capability切片，再接
  control actor。
- 固定Prepared canonical bytes→Project/Session append bytes及artifact audit重验向量。
- 执行create、双aggregate、artifact、restart及live Recovering的完整崩溃/failpoint矩阵。
- 使用两次真实composition-root drop/reopen验证HTTP、WS、CLI snapshot/cursor/reply
  byte一致性。
- 生产`serve`缺少`--data-root`必须失败，且不得fallback内存backend。
- L3必须包含裁决后的B3a/B3b、B2/B1/Slice A以及全仓机械门禁。

## 审查判定

- R1 M1：PASS。
- R1 M2：PASS。
- R1 M3：PASS。
- R1 M4：PASS。
- R1 M5：PASS。
- R1 M6：PASS。
- 新重大问题：无。
- B4设计：**APPROVED FOR IMPLEMENTATION**。
