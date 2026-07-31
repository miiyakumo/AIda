---
verdict: approved
scope: design
artifact: /home/mii/code/draft/docs/plans/mvp-deliberative-execution.md
---

# A1 第二轮独立设计审查

## 结论

修订后的 A1 可以进入实施。第一轮提出的两个重大问题均已在 A1 自身范围内闭合；本轮对照产品需求、权威设计、多端架构调研、当前源码与测试尝试证伪后，没有发现新的、具有具体证据且会阻止实施的重大问题。

本结论只批准 `docs/plans/mvp-deliberative-execution.md` §5 的 A1 设计，不表示 A1 已实现，也不批准后续 A2–E 或正式 MVP。

## 第一轮重大问题闭合检查

### 1. Session cursor 失效后的恢复动作已经可执行

第一轮问题成立时，A1 要求 cursor 错误后重取 snapshot，却没有 Session snapshot 命令或最小投影。

修订后：

- 计划 `:127-133` 新增 `session.snapshot(session_id)`，并冻结 snapshot 至少包含 Session/Project 身份、stream epoch、`covered_through_sequence` 和 Turn 正式状态；
- `:153-155` 明确 snapshot 覆盖序号等于读取时的 head；
- `:157-168` 分别定义合法空页、增量、future cursor、epoch mismatch、stream kind mismatch 和不存在 stream 的确定结果；A1 不截断事件，因此明确不伪造 retention gap；
- `:175` 要求 cursor 错误返回机器可读 `RecoveryAction`。

这与权威设计 `docs/design/mvp-design.md:190-195` 的“snapshot 后按独立 stream cursor 补读”一致。客户端现在可以在错误后取得同一 Session 的新 epoch/head/Turn 投影，再从 `covered_through_sequence` 继续，而不需要创建替代 Session。第一轮问题已闭合。

### 2. 命令重试与业务重复取消已经分离

第一轮问题成立时，“返回原幂等 reply”无法同时覆盖新 command ID 的业务重复取消和 reply 关联。

修订后：

- 计划 `:144-147` 规定同一 command ID 与 digest 的传输重试逐字返回已存 reply；新 command ID 对终态 Turn 返回回显新 ID 的 `TurnAlreadyTerminal`，且不追加事件；
- `:176` 与 `:185-186` 把新 ID、稳定终态和终态事件恰好一次写入异常路径及验收条件；
- 该区分与权威设计 `docs/design/mvp-design.md:180-182` 的命令级幂等规则一致，也可直接沿用当前 `alda-agent/src/app_service.rs:155-181` 已存在的 `(client_id, client_command_id)` 幂等缓存和 `alda-agent/src/protocol.rs:53-59` 的 reply command ID 回显结构。

两种请求现在具有唯一且可测试的 oracle，不再要求新请求复用旧 reply。第一轮问题已闭合。

## 对抗性检查结果

本轮重点尝试推翻以下设计主张，均未得到足以阻止实施的反证：

- **状态机可演进性**：计划 `:134-140` 使用正式 `Turn` 状态与通用生命周期事件，Fake 只执行 `Running -> CancelRequested -> Cancelled` 子路径，没有引入后续必须替换的 `Fake*` wire 类型。
- **cursor 确定性**：计划 `:141-168` 冻结起始序号、严格单调、epoch、分页上限、空页、future cursor 和不支持 stream 的行为；这足以形成 A1 的机械测试 oracle，同时把真正的 retention gap 留给引入截断/持久化的后续切片。
- **现有实现可承载性**：当前 App Service 是有界队列后的单写者，命令结果和协议错误已有 typed reply，且现有 HTTP 与 CLI 都消费同一 `CommandEnvelope`；新增内存 Session/Turn/事件投影不要求绕过现有架构。当前 `cargo test` 实际通过 6 个测试。
- **范围声明真实性**：计划 `:149` 明确 A1 只有进程内恢复语义，重启恢复属于 B；验收没有把 HTTP 拉取式 `event.resume` 冒充 A4 的 WebSocket 断线 E2E。

## 重大问题

无。

## 批准边界

实施仍须逐项满足计划 `:178-189` 的 A1 验收标准，尤其是完整 cursor 真值表、跨 Session 隔离、两类取消幂等、HTTP/CLI 同契约和非持久限制说明。任何尚未实现的测试或 DTO 都不是本次设计批准所证明的事实，应由 A1 实施门控和独立实现复核验证。
