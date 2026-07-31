---
verdict: revise
scope: final
artifact: /home/mii/code/draft/alda-agent
reviewer: A4 second-round independent final implementation reviewer
date: 2026-07-31
---

# A4 / 切片 A 第二轮最终实现审查

## 结论

`A1–A4` 切片 A **仍不可 RELEASE**。R1 的实现缺口已有实质修复：PWA 现在提供
Project/Session/Turn 结构化操作并展示 ID、状态与 processed cursor；断线和 typed
cursor error 会自动 snapshot/resume；Artifact 下载会读取真实 Blob 并触发浏览器下载；
poll 实现也已改用未来 `sleep_until` deadline。README 正确使用 “A4 candidate awaiting
final review”，没有把候选误称为已 RELEASE 或正式 MVP。

但第二轮对抗检查仍发现两项重大问题：

1. PWA 的 `Lagged` 路径用服务器 `last_delivered_sequence` 覆盖客户端完整处理 cursor，
   可能跳过已写入 socket、但浏览器尚未完整处理的事件；
2. 新增“持续满载”测试没有运行真实 WebSocket poller、也没有填满真实 App Service
   query queue，只在测试内重写了一份简化 deadline 循环，因此不能防止生产路径回归。

两项均无需外部决策，故 verdict 为 `revise` 而非 `blocked`。

本报告只评价 A1–A4 的**切片 A**。即使后续通过，结论也只能是切片 A 可 RELEASE；
切片 B–E 尚未完成，绝不能称正式 MVP 完成。

## 重大问题

### M1 — `Lagged` 恢复覆盖客户端确认 cursor，可能永久遗漏事件

- 位置：
  - `/home/mii/code/draft/alda-agent/web/app.js:136-140`
  - `/home/mii/code/draft/alda-agent/web/app.js:170-172`
  - `/home/mii/code/draft/alda-agent/web/client-state.js:101-104`
  - `/home/mii/code/draft/alda-agent/web/client-state.test.js:68-74`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:552-555`
- 实际证据：
  - `handleServer()` 收到 `Lagged` 后把
    `value.last_delivered_sequence` 传入 `requestRecovery()`。
  - `beginRecovery()` 随即把该服务器值写入 `state.recoveryCursor`，替换
    `state.lastProcessedSequence`；snapshot 后从这个替代值订阅。
  - 测试明确把服务器 delivered cursor `4` 当成 resume cursor，因而固化了错误语义，
    没有覆盖“服务器已写到 4、客户端只完整处理到 2”的关键情形。
  - 冻结计划明确规定 Lagged cursor 只是服务器 best-effort 下界，不能覆盖客户端确认；
    客户端必须以完整处理 frame 后保存的 `last_processed_sequence` 恢复。
- 影响：
  - WebSocket `send` 成功只证明 frame 已交给传输层，不证明浏览器已完成
    `handleServer()`。若服务器报告 `last_delivered_sequence = 4`，而浏览器只完整处理到
    `2`，当前恢复会从 `4` 之后订阅，永久遗漏 `3..4`。
  - 这直接破坏 A4 的断线/过载无遗漏恢复契约，阻止切片 A RELEASE。
- 最小修复方向：
  - `Lagged` 只触发 typed snapshot recovery；resume cursor 始终保留客户端
    `lastProcessedSequence`。服务器 delivered cursor 可显示或用于诊断，不得推进客户端
    cursor。
  - 增加 fixture 测试：客户端 processed=2、Lagged delivered=4、snapshot covered>=4，
    最终 subscribe 必须使用 `after_sequence=2`；随后处理 `3..N` 无缺失/重复。

### M2 — “持续 query 满载”测试未覆盖生产 poll 路径

- 位置：
  - `/home/mii/code/draft/alda-agent/src/http.rs:485-498`
  - `/home/mii/code/draft/alda-agent/src/http.rs:560-562`
  - `/home/mii/code/draft/alda-agent/src/http.rs:1030-1082`
- 实际证据：
  - 生产实现已正确使用 `sleep_until(poll_deadline)`，overload 后将 deadline 设为
    `now + next_poll_delay`；R1 的立即 first tick 缺陷在源码层已修复。
  - 但测试创建的是独立 `Semaphore(1)` 和 `mpsc::channel(1)`；它没有创建
    `AppService` 的 capacity=1 query channel、没有调用
    `resolve_session_events()`，也没有运行 `run_websocket()`。
  - 测试中的 `delay = (delay * 2).min(20ms)` 和 `deadline = now + delay` 是生产逻辑的
    手工副本；生产代码若重新退化为立即 interval、在 overload 时误动 subscription
    cursor，或阻塞 command，测试仍可通过。
  - `observed_cursor` 从头到尾只是等于常量 `cursor` 的另一变量，并未观察生产
    `Subscription.queued_through/written_through`。
- 影响：
  - R1 要求的是“真实持续 query queue 满载”下证明实际重试等待、cursor 不动且 command
    有界进展。当前测试只能证明测试自己的循环正确，不能作为生产资源/公平契约的回归门。
  - 关键并发与过载行为缺少可执行验收，仍阻止 A4 RELEASE。
- 最小修复方向：
  - 以可控 capacity=1 的真实 App Service 填满并持续保持 query queue，运行真实 WS
    subscription/poller；通过测试计数/探针观察实际 query 尝试 deadline。
  - 同一测试中提交真实 command 并断言有界完成，同时检查实际 subscription cursor 在
    overload 期间不变；释放 query 满载后再证明从原 cursor 继续。
  - 可使用 Tokio paused time 或测试专用可缩短 poll 配置，避免依赖脆弱墙钟阈值，但
    必须复用生产调度函数/状态，而非复制算法。

## 已闭合项与回归检查

- PWA 结构化命令：Project create、Session start/snapshot、Turn start/cancel 均映射为
  typed DTO；成功 reply 更新并展示 Project/Session/Turn ID、Turn status 与 cursor。
- 完整 frame cursor：正常 `SessionEvents` 路径先渲染整页，再调用 `applyEventPage`；
  processed cursor 仅对新事件单调推进。断线会保存该客户端 cursor并自动重连。
- typed recovery：future/epoch `ProtocolError` 会自动请求 Session snapshot 并恢复订阅；
  正常断线 snapshot/resume fixture 通过。`Lagged` 的 cursor 归属例外见 M1。
- Artifact：PWA 使用认证 fetch 读取 `response.blob()`、创建 object URL、触发带安全派生
  文件名的下载并 revoke URL；不再丢弃响应体或误报空下载。
- poll deadline：生产代码不再重建立即 first-tick interval；overload 后使用未来
  deadline，指数上限仍为 2 秒。真实持续满载门禁缺失见 M2。
- generation/written cursor：旧 generation ack 不推进替换/取消后的订阅；
  `Lagged.last_delivered_sequence` 来自 `written_through`，未使用 queued cursor。
- A1–A3：cursor truth table、reducer replay、取消、approval digest、Artifact
  occurrence/reachability、corruption-before-304 与 no-oracle 集成回归均通过。
- README：开头明确是 “A4 candidate awaiting final review”，并继续声明开发 Local
  Service、进程内状态和 B–E 未实现能力；候选措辞正确。

## 机械门控

在 `/home/mii/code/draft/alda-agent` 实际运行：

```text
cargo fmt --check                          PASS
cargo clippy --all-targets -- -D warnings PASS
cargo test --all-targets                  PASS
  lib unit tests                           29 passed
  main unit tests                           5 passed
  HTTP integration                          1 passed
  WS integration                            2 passed
node --check web/app.js                    PASS
node --check web/client-state.js           PASS
node --check web/sw.js                     PASS
node --test web/client-state.test.js       PASS (4 passed)
git diff --check                           PASS
```

机械门控全部通过，但 M1 是被现有 Node 测试固化的错误恢复语义，M2 则是测试与生产路径
脱节；因此不能据测试总绿将 verdict 提升为 `approved`。

## RELEASE 判定

- A1：既有 RELEASE 回归通过。
- A2：既有 RELEASE 回归通过。
- A3：既有 RELEASE 与安全加固回归通过。
- A4：`REVISE`，M1/M2 阻止 RELEASE。
- A1–A4 切片 A 整体：**不可 RELEASE**。
- 正式 MVP：**未完成**；B–E 仍是后续范围，本报告不作 MVP 完成声明。
