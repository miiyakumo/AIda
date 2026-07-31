---
verdict: revise
scope: design
artifact: /home/mii/code/draft/docs/plans/mvp-deliberative-execution.md
reviewer: A4 second-round independent design reviewer
date: 2026-07-31
---

# A4 第二轮独立设计审查

## 结论

R1 的 `written_through`、资源上限、公平调度、backoff、端口 `0`、精确 CSP 与
service-worker allowlist 修订方向均已进入 A4 计划，但仍有两项实施阻断，因此本轮结论
为 `revise`。两项都属于 R1 重大问题的最后闭合条件，不要求扩大到 B 的持久恢复或 D 的
完整产品 UI。

## 实施阻断

### M1 — 替换/取消订阅没有隔离旧 frame 的迟到 writer ack，仍可能把新 cursor 越过未交付事件

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:538-560`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:620-624`
- 实际证据：
  - 计划允许 `Subscribe` 建立或替换连接上的订阅，也允许 `Unsubscribe`，同时 outbound
    queue 可保留已经入队但尚未写出的旧 `SessionEvents` frame。
  - writer 的内部 ack 只被描述为单调推进 `written_through`；计划没有给订阅分配
    generation/subscription id，也没有规定替换或取消时清除旧 frame，或规定旧 generation
    的 ack 必须被忽略。
  - 第二订阅替换测试只出现在资源验收清单，部分写失败测试只覆盖同一订阅内的
    `queued_through`/`written_through`，没有覆盖旧 frame 与新订阅交错。
- 影响：
  - 客户端可先订阅到较高 sequence、让 frame 入队，然后用较低 cursor 重新订阅同一
    Session。旧 frame 随后写成功并回 ack；若 ack 只按 Session/connection 匹配，就会把
    新订阅的 `written_through` 推到旧 frame 末尾。新 poller 将跳过客户端本次恢复所请求
    的事件，重新引入 R1 M1 的永久遗漏风险。
  - 换成不同 Session 或先 `Unsubscribe` 也会留下迟到 frame/ack 归属不明的问题。当前
    文字不能唯一导出安全实现。
- 最小修复方向：
  - 每次 subscribe/replace 分配单调 subscription generation，frame 与 ack 同时携带
    connection-local generation 和 Session identity；只有与当前订阅完全匹配的 ack
    才能推进 cursor。
  - 明确 replace/unsubscribe 后旧 generation frame 的处理（丢弃或允许写出但永不作用于
    新状态），以及 `Lagged` 也只能引用其所属 generation 的 `written_through`。
  - 增加确定性测试：旧订阅 frame 入队后，以较低 cursor 替换同一 Session，并覆盖不同
    Session 与 unsubscribe；迟到 send/ack 不得推进新订阅 cursor，新订阅最终无遗漏。

### M2 — 两条 actor channel 没有各自的容量和满载契约，“全链 bounded”仍无法验收

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:567-588`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:622-624`
  - `/home/mii/code/draft/alda-agent/src/app_service.rs:78-90`
  - `/home/mii/code/draft/alda-agent/src/app_service.rs:138-154`
- 实际证据：
  - 计划把现有单入口拆成高优先级 command 与低优先级 query 两个“有界 typed
    channel”，却没有给出两者各自的默认容量、配置字段或满载后的稳定返回语义。
  - 当前实现只有一个 `QueueCapacity` 和一个 sender；command 与 Artifact query 共用该
    容量。实施拆分后，旧 `queue_capacity` 究竟同时控制两队列、被拆分，还是只控制
    command，计划没有定义。
  - 计划虽要求测试 queue 上限和 overload backoff，但没有可供测试断言的 query queue
    容量；HTTP Artifact query 满载与 WS poll query 满载也没有区分立即 `503`、typed
    overload 或进入 backoff 的入口契约。
- 影响：
  - 实现者可选择任意容量甚至误用无界 channel，仍能表面满足“两个有界 channel”的文字；
    也无法证明 8:1 调度下的内存代理上限及 overload backoff。R1 M2 要求的总体资源上限
    因而尚未形成可执行、可验收的设计。
- 最小修复方向：
  - 冻结 command/query 两个非零默认容量及独立可缩小测试配置，说明原 CLI
    `queue_capacity` 的兼容映射。
  - 冻结 `try_send` 满载契约：外部 HTTP/WS command、Artifact query、poll query 各自的
    稳定错误/回退行为；只有 poll query 使用指数 backoff，HTTP Artifact 不应在 adapter
    内无界等待或重试。
  - 在验收中分别填满两条队列，验证容量、8:1 公平进展、query overload backoff 与
    command overload 的稳定响应。

## 已确认不构成实施阻断

- Bootstrap 凭据隔离：浏览器 cookie 与 CLI env bearer 被要求独立生成、分道认证且不可
  互换；code/token 的 URL、静态内容和领域状态泄漏也有明确负向验收。
- 固定 origin：普通 `serve` 已明确拒绝端口 `0`，测试 harness 才允许临时端口；显式
  端口冲突 fail closed。
- CSP：指令集固定，`connect-src` 从已验证的精确 listen address 动态生成，并禁止
  wildcard、inline 与 eval。
- Service worker：缓存 key 是版本化精确静态 allowlist；`/sw.js`、query URL、重定向、
  非 GET 与全部 API/Artifact 请求均为 network-only，且有 Cache API key 验收。
- M2 的 8:1 调度规则本身能在持续 command 与持续 query 压力下分别提供有界轮次进展；
  poll 的 250 ms 起始周期和至 2 s 指数 backoff 也已闭合紧循环问题。阻断仅在队列容量
  与满载入口契约尚未冻结。

## 审查边界

本轮直接对照最新 A4 计划、A4 R1 报告、正式 MVP 设计、多端架构调研及当前
`alda-agent` actor/HTTP/CLI/protocol 实现。只写本报告，未修改计划或实现，也未启动
其他 agent。
