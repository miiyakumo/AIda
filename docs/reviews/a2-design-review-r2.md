---
verdict: revise
scope: design
artifact: /home/mii/code/draft/docs/plans/mvp-deliberative-execution.md
---

# A2 第二轮独立设计审查

## 结论

首轮三项重大问题已有实质修订：

1. question 已改为不授予权限的作品长度选择，Model Egress approval 是后续独立决定；
2. requested/resolved event、snapshot 和 reducer 已覆盖正常回答与审批决定所需的
   answer/decision、responder、payload 和 subject digest；
3. approval 已绑定规范化 Fake Action Plan 的 SHA-256 subject digest，并要求响应时
   校验、在 requested/resolved/snapshot 中保持一致。

但 A2 仍需修订后实施。取消路径承诺把待决 question/approval 变为
`OwnerTurnAborted`，却没有定义任何能表达这项权威状态变化的事件。按当前事件集合，
在线投影可以被直接修改，清空投影后 replay 却只能恢复到 `Pending`，与 A2 自己的
逐字段 replay 验收冲突。这是 A2 的事件模型缺口，不是因为 B 尚未实现磁盘持久化而
否决。

## 重大问题

### 1. 取消导致的 OwnerTurnAborted 没有事实事件，reducer 无法重放

**位置**

- `docs/plans/mvp-deliberative-execution.md:250-252`
- `docs/plans/mvp-deliberative-execution.md:277-282`
- `docs/plans/mvp-deliberative-execution.md:291-303`
- `docs/plans/mvp-deliberative-execution.md:311-325`
- 对照 `docs/design/mvp-design.md:184-206`
- 对照当前 A1 的取消事实顺序：
  `alda-agent/src/app_service.rs:462-471`

**实际证据**

A2 明确规定：

- question/approval 都有 `OwnerTurnAborted` 状态；
- 在 question 或 approval 待决阶段取消时，所有所属 Pending 对象都追加该状态；
- reducer 必须从 Session events 重建投影，并与在线 snapshot 逐字段一致；
- requested event 是创建投影的完整事实，resolved event 是决定投影的完整事实。

然而列出的新增事件只有：

- `QuestionRequested`
- `QuestionResolved`（必带 choice ID 和 responder）
- `ApprovalRequested`
- `ApprovalResolved`（必带 decision 和 responder）
- 通用 `TurnCompleted`

其中没有 question/approval aborted/owner-terminated 事件。`QuestionResolved` 和
`ApprovalResolved` 也不能合法复用来表示取消，因为取消没有 choice、decision 或
responder。当前 A1 的 `TurnCancelRequested -> TurnCompleted(Cancelled)` 只记录 Turn
事实；若 reducer 依赖看见 `TurnCompleted` 后扫描并隐式终止其 Pending 子对象，A2
设计并未定义该 reducer 规则、事件应用顺序或 owner 级联不变量，而且
`OwnerTurnAborted` 还适用于权威设计中的其他 Turn 终止原因，不等价于一次审批/
问题 resolved。

因此以下两份状态会分叉：

```text
在线命令路径: PendingQuestion -> OwnerTurnAborted
事件 replay:  QuestionRequested -> PendingQuestion
```

approval 阶段同理。

**影响**

- A2 的取消验收与 replay 验收不能同时成立；
- B 把同一事件写盘后仍无法恢复取消后的待决对象状态，必须补造 A2 声称已冻结的事实
  schema；
- snapshot/cursor 的客户端可见状态可能与重建后的状态不同，后续 respond 的
  `RequestOwnerTurnAborted` 语义也会在恢复后退化为可响应的 Pending。

**最小修复方向**

冻结唯一、可重放的 owner-abort 表达。最直接的是增加携带对象 ID、owner Turn ID 和
终止原因的 `QuestionOwnerTurnAborted` / `ApprovalOwnerTurnAborted`（或一个有明确
对象类型的统一事件），由事件 reducer 同时驱动在线投影和 replay；规定取消时各事件
与 `TurnCancelRequested`、`TurnCompleted(Cancelled)` 的确定顺序，并将
`resolved_sequence` 的含义扩为 terminal sequence 或另设 terminal sequence。另一种
可行方案是正式规定 `TurnCompleted` 对仍 Pending 子对象的确定性级联 reducer，但
必须覆盖所有 terminal status、事件顺序和逐字段 replay 测试，不能只在命令处理器里
直接改投影。

## 首轮问题闭合核对

### question 与 approval：已闭合

`docs/plans/mvp-deliberative-execution.md:267-275` 将 question 固定为作品长度的
有界选择，并明确它不授予权限；未知 choice 在状态变化前拒绝。随后才创建独立
Model Egress approval。两类对象的含义、输入和终态不再混用。

### 正常 answer/decision 的 event、snapshot、reducer：已闭合

`docs/plans/mvp-deliberative-execution.md:239-249,291-303,311-313` 已冻结完整
requested 事实、正常 resolved 事实和逐字段 replay 目标。事件 envelope 的 sequence
可提供 resolved sequence，requested 对象提供 session/owner 关联。除上述取消缺口
外，happy path 与 deny path 已有足够事实重建 question/approval 投影。

### approval subject digest 演进到 C：重大问题已闭合

`docs/plans/mvp-deliberative-execution.md:271-275,287-295` 要求先构造包含 endpoint、
字段集合、owner Turn 和 prompt digest 的规范化 Fake Action Plan，以 SHA-256 得到
subject digest；响应必须回传并匹配，requested/resolved/snapshot 保持同一值。这已
提供 C 所需的“显示 payload 与授权身份分离”协议接缝，而没有越界实现 Permission
Broker 或 sealed plan。

## 重要问题（不单独阻止实施）

### digest 的规范化契约仍应版本化

计划写明 SHA-256 和参与字段，但没有冻结规范化编码、字段排序、集合排序、字符串/
endpoint 规范或 digest 的 domain/version tag。不同客户端或 C 的真实 ActionPlan
若各自序列化，可能对同一语义得到不同 digest，或在 schema 扩展后无法区分算法版本。

建议 A2 将 digest 定义为服务端生成的 opaque wire value，同时给 Fake plan 冻结
canonical bytes 规则与测试向量，并预留 subject schema/algorithm version。此项可以
在不增加 C 授权器的前提下完成。

## 取消、幂等、deny 与状态唯一性核对

- 当前单 actor 与 A1 的串行命令处理为 cancel/respond 竞态提供唯一排序；
- 同 command ID + digest 返回原 reply，新 command ID 对 resolved 对象返回 typed
  AlreadyResolved，且不追加事件，幂等与业务重复已分开；
- deny 明确产生 `Denied` approval 和 `TurnCompleted(Failed)`，不会经过 Approved，
  也没有 Fake 副作用；
- cancel 后 respond 的 typed error 方向正确，但在补齐上述 owner-abort 事实事件前，
  该状态不能跨 replay 保持唯一。

## 审查范围说明

本轮只审查设计，没有修改计划或 `alda-agent` 实现，也没有因 B 的磁盘 persistence、
C 的 Permission Broker/sealed plan 尚未实现而降级结论。当前 `alda-agent` 仍是 A1
基线；其已预留 `WaitingForInput`、`Succeeded`、`Failed` 等 Turn wire 状态，并以单
actor、单调 Session sequence 和命令幂等表提供 A2 可复用的实现边界。
