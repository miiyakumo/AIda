---
verdict: revise
scope: final
artifact: /home/mii/code/draft/alda-agent
---

# A1 独立最终实现审查

## 结论

A1 的机械门控可实际运行并全部通过，Session snapshot/cursor 恢复闭合、分页、Session 隔离、两层取消幂等、正式 wire 状态与事件、typed protocol errors、CLI 命令面、进程内非持久边界和单写者模型均有源码与测试证据。

但实现仍需修订后才能通过 A1 最终签收。HTTP 命令认证允许请求完全省略 `Origin`，与权威设计要求的精确 Origin 校验不符；现有真实 loopback 测试还把这种请求作为成功路径。该缺口使持有 bearer token 的非浏览器本地请求不必证明其请求来源符合固定 origin，且 README 对 “Origin checks” 的表述强于实际行为。

## 机械门控

在 `/home/mii/code/draft/alda-agent` 实际运行：

```text
cargo fmt --check                          PASS
cargo clippy --all-targets -- -D warnings PASS
cargo test                                PASS
```

测试结果为 library 7 个、binary 2 个、真实 HTTP integration 1 个，共 10 个测试通过，doc tests 0 个。因机械门控通过，本审查继续执行了独立对抗检查。

## 重大问题

### 1. HTTP 命令端点允许缺失 `Origin`，未执行权威设计要求的精确 Origin 校验

**具体位置**

- `docs/design/mvp-design.md:347-359`
- `docs/design/mvp-design.md:457-461`
- `alda-agent/src/http.rs:43-68`
- `alda-agent/src/main.rs:318-340`
- `alda-agent/tests/http_round_trip.rs:17-66`
- `alda-agent/tests/http_round_trip.rs:98-260`
- `alda-agent/README.md:15`

**实际证据**

权威设计的安全基线规定 HTTP 命令和 WebSocket upgrade 都校验精确 Origin、有效 token 和协议版本；安全验收还要求恶意或非 allowlist Origin 被拒绝。

`HttpAuth::authorize` 在 `http.rs:52-57` 仅在请求带有 `Origin` header 时比较 allowlist。header 不存在时直接跳过该检查，随后只验证 Host 和 bearer token。CLI 的 `submit` 在 `main.rs:336-340` 只设置 bearer token 和 JSON body，没有设置 Origin。

真实 HTTP integration test 为成功创建、Session start/snapshot、Turn start/cancel 和 event resume 发出的请求也都只设置 bearer token，没有设置 Origin（`http_round_trip.rs:98-260`），并全部成功。只有显式携带攻击者 Origin 的路径被断言为 403（`:38-46`）；没有“缺失 Origin 必须拒绝”的测试。此次实际 `cargo test` 通过因此直接证明：当前服务接受 `Host` 正确、token 正确但完全没有 `Origin` 的状态变更命令。

README `:15` 声称适配器具备 Host、Origin 和 bearer-token checks，却没有说明 Origin 是可选检查。

**影响**

固定同源 Local Service 的安全模型要求每个 HTTP 命令同时满足精确 Host、Origin 和凭据边界。当前行为将“精确 Origin 校验”降为“若调用者自愿提供 Origin 才校验”，扩大了获得 token 后可调用状态变更端点的请求来源集合，并使 A1 的认证边界测试通过了与设计相反的成功 oracle。该问题位于后续 PWA、审批和播放等高影响命令共用的 transport 边界，不能留作纯文档差异。

**最小修复方向**

使浏览器 HTTP 命令缺失 `Origin` 时 fail closed，并增加合法精确 Origin、缺失 Origin、错误 Origin、错误 Host、缺失/错误 token 的确定性测试。CLI 若继续复用该 HTTP 端点，应显式发送由已验证 loopback server URL 推导出的精确 Origin，或定义并测试一个与浏览器认证边界等强、且符合权威设计的独立 CLI transport/auth 路径；不能通过继续允许所有无 Origin 请求来兼容 CLI。同步收紧 README，使描述与实际边界一致。

## 其余针对性检查证据

- **snapshot/cursor 恢复闭合**：`protocol.rs:67-79,245-262` 定义 snapshot 覆盖序号与机器可读恢复动作；`app_service.rs:303-310,490-497,515-533` 实现同一 Session snapshot 和 epoch/head 错误详情。测试 `app_service.rs:649-754` 验证 snapshot head、终态和 terminal sequence 一致。
- **cursor 真值表与分页**：`app_service.rs:345-405` 实现 kind、stream、epoch、future cursor、严格 `>` 和 256 条分页；测试 `:758-884` 覆盖真值表，`:886-930` 以 301 个事件证明分页无遗漏或重复。
- **跨 Session 隔离**：事件存储位于各自 `SessionState`（`app_service.rs:163-167`），resume 以 cursor 的 Session stream ID 只读取对应 map entry（`:359-390`）；测试 `:758-884` 分别验证两个 Session 的首事件身份。
- **两层取消幂等与终态恰好一次**：同 command ID/digest 在 `app_service.rs:175-202` 返回原 reply；新 command ID 对终态 Turn 在 `:448-459` 返回新关联 ID 的 `TurnAlreadyTerminal`；首次取消只在 `:462-471` 追加一组 cancel-requested/completed。测试 `:649-754` 同时验证两层语义和唯一 sequence 4 终态。
- **正式状态机与 typed errors**：`protocol.rs:82-107,131-148` 使用正式状态和生命周期事件，无 `Fake*` wire 类型；`:230-279` 定义 typed outcome/error code/details。A1 实际只走已批准的 `Running -> CancelRequested -> Cancelled` 子路径。
- **CLI 同协议**：`main.rs:229-316` 将 Session/Turn/event CLI 子命令映射为相同 `ClientCommand`，`:318-390` 调用 `/v1/commands` 并解析 `CommandReply`。本报告所列 Origin 问题是该复用路径当前必须一起修订的 transport 缺口。
- **A1 非持久边界**：`README.md:19-29,58-61` 明确排除持久日志、重启恢复、PWA bootstrap 和 WebSocket，并说明进程重启会丢失全部 A1 状态；未把 epoch 1 描述为持久恢复。
- **单写并发**：`app_service.rs:59-104,133-145` 通过有界 Tokio channel 将所有命令交给一个 runner 串行修改唯一 `ServiceState`；过载测试 `:593-603` 证明满队列返回 typed service error，而非创建第二写者。

除上述 Origin 缺口外，本轮没有发现另一个具有具体位置和实际反证、足以阻止 A1 增量交付的问题。本结论仅覆盖计划 §5 的 A1，不表示持久恢复、WebSocket/PWA 或正式 MVP 已完成。
