---
verdict: revise
scope: design
artifact: /home/mii/code/draft/docs/plans/mvp-deliberative-execution.md
reviewer: A4 independent design reviewer
date: 2026-07-31
---

# A4 独立设计审查

## 结论

A4 的宿主边界、cookie/CLI bearer 隔离、Host/Origin 校验、WS 子协议、内部 actor
查询、断线不取消 Turn、PWA 缓存边界和 DOM 文本安全方向总体与 PRD、MVP 设计及当前
A1–A3 实现一致，但当前设计不能批准实施。存在两项重大问题：过载恢复游标没有定义
“已交付”的提交点，可能造成权威事件永久跳过；传输入口和轮询 fan-out 没有资源上限，
与“全链 bounded”的硬约束冲突。两项都应在 A4 实现前以小范围协议/传输设计修订解决，
不要求提前实现 B 的持久化或 D 的 score UI。

## 重大问题

### M1 — `Lagged.last_delivered_sequence` 没有可验证的“已交付”语义，恢复可能跳过未写入 socket 的权威事件

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:539-547`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:572-575`
  - `/home/mii/code/draft/docs/design/mvp-design.md:194`
  - `/home/mii/code/draft/docs/design/mvp-design.md:437`
- 实际证据：
  - A4 设计让 adapter 周期拉取、只发送 sequence 大于 connection cursor 的事件，并在
    outbound queue 满时发送带 `last_delivered_sequence` 的 `Lagged`。
  - 设计没有说明 cursor 是在事件“进入 outbound queue”时推进，还是在 writer 完成
    WebSocket send 后推进，也没有定义 poller 与 writer 之间如何确认该提交点。
  - 当前 App Service 的 `EventPage.next_after_sequence` 是“已从 actor 查询并装入 page”
    的最后 sequence，而不是网络交付确认
    （`/home/mii/code/draft/alda-agent/src/app_service.rs:658-677`）。
- 影响：
  - 若 poller 在 enqueue 时推进 cursor，随后队列中的事件未成功写出而连接被标记
    `Lagged`/关闭，客户端按该 cursor 恢复会永久跳过权威事件，直接违反 A4 的
    “无遗漏/重复”和 MVP 的“权威状态事件不静默丢失”验收。
  - 若实现者为避免遗漏而完全不推进，又会在正常轮询中重复装入同一 page，导致队列
    放大和重复发送。当前文字不足以唯一导出正确实现。
- 最小修复方向：
  - 在 A4 §11 冻结两个独立 cursor：`queued_through` 与 `written_through`；只有 WS writer
    成功完成对应 frame 的 send 后，才能单调推进 `written_through`。
  - `Lagged.last_delivered_sequence` 必须定义为 `written_through`，关闭连接时也以它作为
    可恢复下界；不得使用 `EventPage.next_after_sequence` 或“已入队”位置冒充已交付。
  - 明确 frame/page 原子性：一个 `SessionEvents` frame 中的 sequence 连续且恢复 cursor
    只推进到整个 frame 成功写出之后。增加测试：队列已装入 N..M、writer 仅写完 N..K
    后阻塞/失败，恢复从 K 开始，最终既不缺事件也不越过 K。

### M2 — WebSocket fan-out、内部轮询和传输入口没有总体上限，设计不满足“所有内部队列有界”

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:539-543`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:565-579`
  - `/home/mii/code/draft/docs/design/mvp-design.md:154-160`
  - `/home/mii/code/draft/docs/research/client-surface-architecture.md:127-134`
- 实际证据：
  - A4 只规定单个 outbound queue 有界和“有界周期”拉取，没有规定并发 WS connection
    数、每连接订阅数、轮询 in-flight 数、WS frame/message 大小、HTTP JSON/body 大小或
    静态/Artifact 请求并发上限。
  - 当前 actor 确实只有一个有界入口
    （`/home/mii/code/draft/alda-agent/src/app_service.rs:84-89`），A4 又计划让每个连接
    周期性向同一入口提交 `ResolveSessionEvents`。因此即使每条 channel 有界，未受限的
    connection/task/timer 数仍可无限增长，并可持续占满 actor queue，饿死用户命令。
  - 当前 wire DTO 的 `TurnStart.prompt: String` 本身无长度约束
    （`/home/mii/code/draft/alda-agent/src/protocol.rs:50-53`），所以仅依赖 serde/WS
    默认值不能证明入口内存有界。
- 影响：
  - 一个误配置页面、多个标签页或持有凭据的本地进程即可制造无界 task/timer/消息内存，
    或让内部 polling 长期把 command actor 推入 `Overloaded`。这会破坏 A4 自身真实
    Web 纵切片的可用性，也违背 MVP §6.3 的明确硬约束；若等 B 之后再补，会迫使
    App Service 调度和 WS adapter 返工。
- 最小修复方向：
  - 在 A4 §11 写明并测试配置化硬上限：全局 WS connections、每连接 subscriptions
    （若 A4 只支持一个就明确为 1）、每连接 outbound bytes/messages、WS frame/message
    bytes、HTTP bootstrap/command body bytes，以及每连接最多一个 polling request
    in flight。
  - 为内部查询定义独立的 typed actor message/reply（不能伪造外部
    `CommandEnvelope`、不能写幂等表或产生领域事件），并规定公平调度或最小限度的
    polling backoff/jitter：actor `Overloaded` 时不得紧循环重试；外部 command 必须仍
    有进展机会。
  - 上限触发采用稳定、可测试的 HTTP 状态/typed `ProtocolError`/WS close code；补充
    多连接过载测试，证明任务数和内存代理指标有界且 command 不被 polling 永久饿死。

## 重要问题

### I1 — 固定 origin 的端口 `0` 与显式重绑定边界尚未写死

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:503-505`
  - `/home/mii/code/draft/docs/design/mvp-design.md:351-354`
  - `/home/mii/code/draft/alda-agent/src/main.rs:313-329`
- 证据与影响：当前 CLI 接受任意 loopback `SocketAddr`，包括端口 `0`；绑定后会采用
  OS 随机端口并据此生成 origin。A4 文字只称 `37891` 为“开发固定 origin”，没有明确
  生产/手工启动是否拒绝 `0`。若把该行为带入 PWA 路径，会与固定 origin、端口冲突
  fail-closed 的产品约束冲突。
- 建议：A4 明确普通 `serve` 拒绝端口 `0`；仅测试 harness 可通过非公开构造使用临时
  端口。A4 允许显式非默认固定端口时，也应说明它是本次进程的显式 origin，而不是
  自动 fallback；B 再负责持久化选择和重绑定流程。

### I2 — CSP 与 service-worker 缓存规则应从口号收敛为可测试 allowlist

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:509-510`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:561-563`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:576-579`
- 证据与影响：“严格 CSP”和“只缓存静态壳”没有列出指令或 cache key allowlist。
  `textContent` 能约束事件文本，但不能替代对脚本来源、导航、frame/object/base URI
  和动态 URL 属性的限制；以 path 前缀排除 `/v1/` 也不足以证明非 GET、重定向或
  带查询的敏感响应永不进入 Cache API。
- 建议：冻结最低 CSP（至少 `default-src 'self'`、无 `unsafe-inline/eval`、
  `object-src 'none'`、`base-uri 'none'`、`frame-ancestors 'none'`，连接仅同源）；
  SW 采用版本化、精确静态资源清单，仅缓存成功的同源 GET，所有非清单请求直接
  network-only，且 bootstrap/API/Artifact response 继续 `no-store`。测试 Cache API
  keys，而不只 grep `/v1/`。

## 已尝试证伪但未形成否决的问题

- Bootstrap/cookie/CLI bearer：设计已经规定独立生成、header/cookie 分道认证、短期
  server-side expiry、一次性 code、`no-store`，且浏览器 token 不暴露给 JS。HTTP
  loopback 下不使用 `Secure` 是已披露的 A4 开发限制，不把它误判为 B 阻塞项。
- Origin/Host：bootstrap、命令、Artifact 和 WS 的敏感路径都有精确校验意图，服务
  监听也已在当前实现中拒绝非 loopback地址
  （`/home/mii/code/draft/alda-agent/src/main.rs:313-316`）。应保持值的精确比较，不做
  后缀、模糊 hostname 或 CORS 放宽。
- WS typed messages/subprotocol：`alda-agent.v1` 与 tagged client/server envelope 已
  足以形成独立 wire DTO；`CommandReply.client_command_id` 可关联并发命令回复。实现
  时必须确认 handshake 实际回选该 subprotocol，而非只检查请求头。
- Internal polling actor：新增内部只读 `ResolveSessionEvents` 是正确边界；它应复用
  A1 cursor truth table，但不能作为外部 command 写幂等记录或产生事件。M2 要求补足
  资源和公平性契约。
- 外部命令事件：当前 A1–A3 的所有 Session 事件由同一 actor append，`EventResume`
  从该 actor 的单调 sequence 分页
  （`/home/mii/code/draft/alda-agent/src/app_service.rs:633-677`），因此 CLI/HTTP
  命令可由内部查询观察，不需要另建 WS 事实源。
- 断线恢复：snapshot 的 `stream_epoch` 与 `covered_through_sequence` 已存在
  （`/home/mii/code/draft/alda-agent/src/protocol.rs:85-94`），future/epoch cursor
  也已有 typed recovery DTO。断线不取消 Session/Turn 的 A4 规则与 MVP 一致。
- Artifact no-oracle：A4 明确保留 A3 的“Project 不存在”和“hash 对 Project 不可达”
  同形 404 验收，不应在 Web adapter 中退化。

## 审查边界

本报告没有因 B 的磁盘 persistence、实例锁、私有 runtime 文件或重启 replay 尚未实现
而否决 A4，也没有因 D 的曲谱工作面、MIDI/Audition/Feedback/Accept UI 尚未实现而否决。
审查只写本报告，未修改计划、设计或 `alda-agent` 实现。
