---
verdict: revise
scope: final
artifact: /home/mii/code/draft/alda-agent
reviewer: A4 slice-A independent final implementation reviewer
date: 2026-07-31
---

# A4 / 切片 A 最终实现审查

## 结论

`A1–A4` 切片 A **目前不可 RELEASE**。A1–A3 的既有协议、状态投影、Artifact
边界与认证回归均通过，A4 的 bootstrap、cookie/bearer 隔离、固定 origin、CSP、
WebSocket 子协议、generation ack 隔离、双队列及 8:1 调度也大体落地；但仍有两项重大
验收缺口：

1. Web/PWA 没有实现 A4 冻结的最小交互与客户端恢复算法，只提供原始 JSON 输入和手工
   cursor 订阅，无法由页面完成切片 A 宿主纵切片；
2. WebSocket query overload 的所谓指数 backoff 会因 `tokio::time::interval` 的立即
   first tick 而退化为紧循环，违反 A4 明确的资源与公平性契约。

因此结论是 `revise`，不是 `blocked`：无需用户裁决或外部条件，按下述最小方向修复并
补充真实测试即可再次复核。

本结论只评价 A1–A4 的**切片 A**。即使修复后切片 A 可 RELEASE，也绝不等于正式 MVP
完成；切片 B–E 的持久化/恢复、真实 Agent 与 Alda 工具、Revision、试听、Feedback、
比较与人类 Accept 仍未实现。

## 重大问题

### M1 — PWA 没有实现冻结的 A4 最小交互和 snapshot/resume 客户端闭环

- 位置：
  - `/home/mii/code/draft/alda-agent/web/index.html:17-24`
  - `/home/mii/code/draft/alda-agent/web/app.js:20-39`
  - `/home/mii/code/draft/alda-agent/web/app.js:77-90`
  - `/home/mii/code/draft/alda-agent/web/app.js:124-151`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:608-618`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:647-649`
- 实际证据：
  - 页面只有一个 `Command JSON` textarea；没有 create Project、start Session、
    start/cancel Turn 的结构化控件或映射。`sendCommand()` 直接把用户文本作为 HTTP
    body 发送，页面并未共同驱动这些正式命令。
  - `handleServer()` 只处理 question、approval 与 approval reply 中的 Artifact；
    不保存或展示 Project/Session/Turn ID，不维护
    `last_processed_sequence`，也不把完整处理后的 page cursor 写回客户端恢复状态。
  - 名为 “Snapshot/resume subscription” 的按钮只发送 `Subscribe`；代码没有发送
    `session.snapshot`，没有用 snapshot 的 epoch/covered sequence 建立订阅，也没有在
    `ProtocolError.recovery = FetchSessionSnapshot` 或 `Lagged` 后执行恢复。
  - `reconnect()` 仅重新打开 socket；断线后仍依赖用户手工填写 session、epoch 和
    sequence。README 描述的 snapshot/resume 流程是人工说明，不是当前 PWA 行为。
  - `pwa_sources_use_safe_dom_and_exact_cache_allowlist` 只做源码字符串/allowlist 检查；
    真实 WS E2E 使用 Rust 测试客户端手工维护 cursor，不能证明浏览器命令映射或恢复。
- 影响：
  - A4 的目标是“同源 Web/PWA 能 bootstrap、调用统一 command contract、通过
    WebSocket 接收/恢复事件、处理 question/approval、取消 Turn 并下载 Artifact”。
    当前页面不能从 Project 创建走到 Turn 取消/批准，也不能在断线、Lagged 或 cursor
    error 后按权威 snapshot 恢复；用户若填写旧/高 cursor 只能看到错误，页面不会闭环。
  - 这不是切片 D 的曲谱/MIDI/UI 美化缺失，而是切片 A 自己冻结的宿主纵切片未完成，
    因而直接阻止 A1–A4 RELEASE。
- 最小修复方向：
  - 为 Project create、Session start/snapshot、Turn start/cancel 提供结构化操作，
    将成功 reply 中的 ID 和状态写入只读 DOM 展示。
  - 在完整处理 `SessionEvents` frame 后维护客户端 `last_processed_sequence`；连接恢复
    时先请求 snapshot，再按保留 cursor 或 snapshot covered cursor 订阅；遇到
    `Lagged`、future/epoch recovery 时自动重取 snapshot，且 UI 明示恢复状态。
  - 增加可执行的浏览器逻辑测试（至少 DOM/协议 fixture 驱动），证明命令 DTO 映射、
    cursor 更新、断线重连和 typed recovery，而非只 grep 安全字符串。

### M2 — poll overload backoff 重建立即触发的 interval，持续满载时会紧循环

- 位置：
  - `/home/mii/code/draft/alda-agent/src/http.rs:400-402`
  - `/home/mii/code/draft/alda-agent/src/http.rs:473-480`
  - `/home/mii/code/draft/alda-agent/src/http.rs:543-547`
  - `/home/mii/code/draft/alda-agent/src/http.rs:560-562`
  - `/home/mii/code/draft/alda-agent/src/http.rs:1008-1016`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:599-603`
- 实际证据：
  - 每次 poll semaphore 或 query queue overload 时，代码计算更大 delay 后执行
    `poll = tokio::time::interval(poll_delay)`。
  - Tokio `interval` 的第一个 tick 立即完成；所以下一次 `select!` 可立即再次查询。
    达到 2 秒上限后，每次失败仍重新创建一个 first tick 立即就绪的新 interval，可持续
    无等待循环。
  - 现有 `poll_overload_backoff_is_bounded_and_not_a_tight_loop` 测试只调用纯函数
    `next_poll_delay` 并断言数值为 500ms/2s；它没有运行 `run_websocket`、占满 query
    queue 并测量实际 query 尝试间隔，因此测试名称所声称的“不紧循环”没有被验证。
- 影响：
  - 一个已认证订阅在 query queue 持续满载时可高频抢 poll semaphore、反复
    `try_send`，消耗 CPU，并与其他连接竞争；这违反冻结的
    `250 ms → 2 s` 有上限 backoff 和“禁止紧循环”契约。
  - 双队列和 runner 的 8:1 公平只能约束 actor 已接收的消息，不能消除 adapter 外部的
    busy loop，故资源/公平门禁没有真正闭合。
- 最小修复方向：
  - 使用 `sleep_until`/显式 next-deadline，或
    `interval_at(Instant::now() + poll_delay, poll_delay)`，确保 overload 后首次重试
    也至少等待当前 backoff；不要在每次失败后创建立即 tick 的 interval。
  - 增加真实时间受控测试：保持 capacity=1 query queue 满载，运行订阅连接并统计查询
    尝试时间/次数，证明 250ms 起步、指数增长至 2s，且 cursor 不变、command 仍有界进展。

## 重要问题

### I1 — PWA 的 Artifact “Download” 丢弃响应体，没有向用户交付可用文件

- 位置：
  - `/home/mii/code/draft/alda-agent/web/app.js:92-108`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:617`
- 证据与影响：点击按钮后 `fetch` Artifact，只检查 `response.ok`，既不读取 body，也不
  创建 Blob/download link 或导航到下载响应；随后却显示 “Downloaded N bytes”。
  服务端 hash-only 下载边界本身正确，但 PWA 用户没有取得 Artifact，且状态文案误报。
- 建议：使用受认证 fetch 读取 Blob，创建短生命周期 object URL 和服务端派生/固定安全
  文件名下载，完成后 revoke；失败时不得显示已下载。

### I2 — README 把“completed A1–A4”写成既成事实，与当前实现和计划状态不一致

- 位置：
  - `/home/mii/code/draft/alda-agent/README.md:3`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:662-663`
- 证据与影响：README 称 “completed A1–A4 development protocol slice”，但计划中的
  A4 实施门控和独立复核仍为 `pending`，且本报告发现 M1/M2。README 后文正确声明不是
  MVP，但切片完成状态仍属 stale docs。
- 建议：修复并复核前改为“正在实现/候选”；通过终审后再同步门控表和 RELEASE 状态。

## 对抗检查结果

- Bootstrap：高熵随机 code、5 分钟 expiry、成功原子消费、错误/重放统一拒绝、全局
  失败限速、成功/错误 `no-store` 均已实现。bootstrap code 只复制到启动 stderr；
  静态资源、URL、领域对象未发现 code/token 泄漏。
- Cookie vs bearer：浏览器 cookie 与 CLI bearer 独立随机值、不同入口校验；WS 只收
  cookie，HTTP command/Artifact 可收各自合法方式，交叉错用测试通过。cookie 为
  `HttpOnly; SameSite=Strict; Path=/`，30 分钟 server-side expiry；开发 HTTP 无
  `Secure` 的限制已披露。
- Origin/Host/port 0：敏感 HTTP、bootstrap、Artifact 与 WS 均精确校验 Host/Origin；
  `serve` 拒绝非 loopback 与端口 0，显式端口 bind 失败不 fallback。CSP 使用精确
  `ws://host:port`，无 wildcard/inline/eval。
- Service worker：只缓存四个精确、同源、无 query 的成功 GET；`/sw.js`、重定向、
  `/v1/`、Artifact 和非 allowlist 均 network-only。未发现 API cookie response 缓存。
- WS auth/subprotocol/typed message：upgrade 前校验 cookie、Host、Origin 与
  `alda-agent.v1`，并实际回选子协议；typed command/subscribe/unsubscribe/ping 与
  server event/error DTO 存在，外部 HTTP 命令产生的事件由真实 WS E2E 观察到。
- generation / queued / written / Lagged：frame/ack 带 generation 与 Session，
  stale ack 不推进替换/取消后的订阅；完整 send 后才 ack，Lagged 使用
  `written_through`。该部分单元测试存在；问题在 M2 的调度等待而非 cursor 归属。
- command/query：独立默认 64/32 capacity、非零校验、`try_send` 满载契约和 runner
  8:1 调度已实现；capacity=1 与双方进展测试通过。连接 16、poll 8、HTTP concurrency
  32、body/frame 64KiB、outbound 16×64KiB 等主要上限存在。实际 backoff 例外见 M2。
- A3 no-oracle：HTTP 测试覆盖已存在 Project 但 hash 不可达、未知 Project及错误 token，
  前两者同为 404；认证发生在 actor lookup 前。
- A1–A3：cursor truth table、reducer replay、取消顺序、approval digest、Artifact
  occurrence/reachability、preparation fault 和 corruption-before-304 回归均通过；
  未发现新重大退化。

## 机械验证

在 `/home/mii/code/draft/alda-agent` 实际运行：

```text
cargo fmt --check                          PASS
cargo clippy --all-targets -- -D warnings PASS
cargo test --all-targets                  PASS
  lib unit tests                          28 passed
  main unit tests                          5 passed
  HTTP integration                         1 passed
  WS integration                           2 passed
node --check web/app.js                   PASS
node --check web/sw.js                    PASS
git diff --check                          PASS
```

机械门控通过不覆盖 M1 的产品交互缺失，也没有检测 M2 的真实定时行为；因此不能据此将
最终 verdict 提升为 `approved`。

## RELEASE 判定

- A1：既有 RELEASE 回归通过。
- A2：既有 RELEASE 回归通过。
- A3：既有 RELEASE 回归与 no-oracle 加固通过。
- A4：`REVISE`，M1/M2 阻止 RELEASE。
- A1–A4 切片 A 整体：**不可 RELEASE**。
- 正式 MVP：**未完成，且本报告不对其作完成声明**；B–E 仍是明确后续范围。
