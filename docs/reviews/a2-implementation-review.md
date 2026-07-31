---
verdict: approved
scope: final
artifact: /home/mii/code/draft/alda-agent
---

# A2 独立最终实现审查

## 结论

A2 实现获准通过。本轮没有发现需要 `revise` 的重大问题。

实现满足 A2 的切片边界：question 是作品长度的有界创作输入，不承担 consent；
Model Egress approval 是后续独立对象；requested/resolved/owner-abort 都形成完整事件事实；
在线处理与 replay 共用 reducer；取消顺序固定；响应绑定版本化 SHA-256 subject digest；
HTTP 与 CLI 仍受 loopback/Origin 边界约束。README 也准确限定为单进程 Fake fixture，
没有把 epoch、replay、批准决定夸大为持久恢复或真实副作用授权。

## 对抗核对

### Question 与 consent 分离

- `alda-agent/src/app_service.rs:364-388` 创建“请选择作品长度”的 `bars_8` /
  `bars_16` 有界问题。
- `alda-agent/src/app_service.rs:679-718` 先记录创作答案，再独立创建
  `EffectClass::ModelEgress` approval。
- `alda-agent/src/protocol.rs:57-67` 将 `QuestionRespond` 与 `ApprovalRespond` 定义为
  两个不同命令，question 没有 approve/deny 权限语义。

这与 PRD FR-03、FR-41/42 及 A2 两轮设计审查的修复要求一致。

### Approval payload、canonical digest 与响应绑定

- `alda-agent/src/protocol.rs:137-151,170-182` 的 snapshot 对象保存完整展示 payload、
  `algorithm/schema_version/value` digest、decision 和 responder。
- `alda-agent/src/app_service.rs:994-1017` 固定 domain tag、schema version、有序 tuple、
  UTF-8 JSON bytes、字段字节序排序去重、prompt SHA-256 和小写 hex。
- `alda-agent/src/app_service.rs:2196-2209` 固定 canonical v1 测试向量
  `52d128...eb33`。
- `alda-agent/src/app_service.rs:780-800` 在 Pending approval 状态改变前比较客户端回传
  digest，并把同一 digest 写入 `ApprovalResolved`。

显示内容和授权对象身份没有混用；实现也没有越界声称已提供 C 阶段的 sealed plan、
Permission Broker 或真实 Provider 调用。

### 完整事实与同一 reducer

- `alda-agent/src/protocol.rs:250-276` 的 requested/resolved/owner-abort 事件分别携带
  完整 question/approval、choice/decision、responder、subject digest、owner Turn 和
  terminal status。
- `alda-agent/src/app_service.rs:826-830` 的在线 append 先调用 `reduce`，没有由命令
  处理器旁路写派生投影。
- `alda-agent/src/app_service.rs:847-947` 的单一 reducer 投影 Turn、question、approval
  的正常解决和 owner abort。
- `alda-agent/src/app_service.rs:1608-1747` 清空派生投影并从事件 replay，逐字段比较
  turns/questions/approvals，覆盖 answer、decision、responder、payload 与 digest。

### 取消、顺序与终态

- `alda-agent/src/app_service.rs:547-603` 严格追加
  `TurnCancelRequested` → 按 `created_sequence` 排序的 pending object abort →
  `TurnCompleted(Cancelled)`。
- `alda-agent/src/app_service.rs:1781-1965` 分别验证 question 阶段和 approval 阶段取消
  的事件顺序、snapshot 终态及取消后 typed respond 错误。
- 已终止 Turn 的新 cancel command 在 `alda-agent/src/app_service.rs:533-544` 返回
  `TurnAlreadyTerminal` 且不追加事件；同 command ID 的 transport retry 仍由统一
  idempotency 表返回原 reply。

### Happy、deny、invalid、幂等、ownership、snapshot/cursor

- happy path、完整事件序列、snapshot/cursor 与 replay：
  `alda-agent/src/app_service.rs:1608-1747`。
- deny 只得到 `Denied` + `Failed`：
  `alda-agent/src/app_service.rs:1751-1776`。
- invalid choice 与 digest mismatch 不追加事件、不改变状态：
  `alda-agent/src/app_service.rs:1969-2046`。
- transport retry、业务重复 respond 恰好一次、跨 Session ownership：
  `alda-agent/src/app_service.rs:2051-2193`。
- 分页无重复/遗漏及 cursor truth table：
  `alda-agent/src/app_service.rs:1277-1450`。

### HTTP、CLI 与 Origin 边界

- `alda-agent/src/http.rs:43-69` 对 Host、Origin、Bearer token 做精确匹配，且 guard
  位于 JSON handler 之前。
- `alda-agent/src/main.rs:282-299` 拒绝非 loopback listen；CLI endpoint 校验和精确
  Origin 派生由 `src/main.rs` 的三个单元测试覆盖。
- `alda-agent/tests/http_round_trip.rs:20-87,272-366` 用真实 loopback HTTP 验证
  Host/Origin/token 拒绝，并走通 question/approval respond。
- `alda-agent/src/main.rs:181-220,263-277` 暴露 question/approval CLI 命令并映射到
  同一 wire contract。

### 范围与诚实性

`alda-agent/README.md:25-37,77-83` 明确排除磁盘 persistence、restart recovery、
WebSocket、Provider、sealed action plan 和真实 side effect；批准只推进内存 Fake
状态机。因此固定 epoch `1` 和进程内 replay 没有被描述成 durable recovery。

## 重要问题（不阻止 A2）

### 1. 取消路径尚无显式 replay 等价断言

**位置与证据**

- `alda-agent/src/app_service.rs:1741-1747` 的 replay 等价测试仅位于 happy path。
- `alda-agent/src/app_service.rs:1781-1965` 的两种取消测试检查 snapshot 和事件顺序，
  但没有把这些取消事件重新喂给空投影并逐字段比较。

**影响**

当前实现确实由同一 reducer 产生在线结果，人工检查也确认 abort 分支可重放，因此不构成
现存功能错误；但未来修改 abort reducer 时，测试可能只证明在线 snapshot 与事件外形，
不能直接捕获取消事件 replay 后仍为 Pending 或 terminal sequence 分叉的回归。

**修复方向**

把共用 replay helper 用于 question-cancel 和 approval-cancel 两条事件流，并逐字段比较
turns/questions/approvals，尤其是 `OwnerTurnAborted` 与 `terminal_sequence`。

### 2. HTTP A2 round trip 对业务事实的断言偏弱

**位置与证据**

`alda-agent/tests/http_round_trip.rs:272-366` 只断言 question/approval 命令返回对应
success variant；没有在最终 snapshot/event page 中断言 responder、decision、digest
和 Turn 终态。

**影响**

核心业务字段已由 App Service 单元测试覆盖，所以当前不阻止交付；但 HTTP DTO 映射若
以后遗漏或改错某个字段，现有 round-trip 测试的定位能力有限。

**修复方向**

在 approval 后再取一次 HTTP snapshot 和增量事件，断言 question answer/responder、
approval decision/responder/digest 以及 `Succeeded`，并避免只依赖硬编码
`question-2`。

## 机械验证

在 `/home/mii/code/draft/alda-agent` 实际执行：

```text
cargo fmt --check                          PASS
cargo clippy --all-targets -- -D warnings PASS
cargo test                                PASS
```

测试结果为：lib 13 通过、CLI/main 3 通过、真实 HTTP integration 1 通过、doc tests
0 项；共 17 项通过，0 失败。

## 审查范围

本轮直接阅读了原始 PRD、MVP 设计、A2 执行设计、两轮 A2 设计审查、当前
`alda-agent` 的全部源码、测试、README、Cargo 配置与工作区 diff。除本报告外未修改
被审查产物，也未把 B–E 尚未实现的 persistence、Permission Broker、Provider 或真实
副作用错误地算作 A2 缺陷。
