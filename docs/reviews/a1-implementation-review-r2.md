---
verdict: approved
scope: final
artifact: /home/mii/code/draft/alda-agent
---

# A1 第二轮独立最终实现审查

## 结论

批准 A1 增量交付。

第一轮唯一重大问题已修复：命令 HTTP 端点现在对缺失 `Origin` fail closed；CLI 只接受显式 loopback IP 的 HTTP endpoint，在完成 scheme、host、loopback 和 userinfo 校验并规范化命令路径后，从该 URL 推导精确 Origin 并随请求发送。真实 loopback 测试分别证明合法认证请求成功、缺失/错误 Origin 被拒绝、错误 Host 被拒绝、缺失/错误 token 被拒绝。

本轮复读了计划、权威设计、第一轮报告、README、当前全部 Rust 源码和测试，并检查了当前 diff。未发现修复引入新的重大回归，也未发现仍足以阻止 A1 签收的问题。本结论仍只覆盖计划中的 A1 开发纵切片，不表示持久恢复、WebSocket/PWA 或正式产品 MVP 已完成。

## 第一轮问题关闭证据

### 服务端缺失 Origin fail closed

- `alda-agent/src/http.rs:43-69` 依次读取并精确比较 Host、Origin 和 bearer token。
- `Origin` 缺失、非 UTF-8 或不等于 `expected_origin` 均在进入 command handler 前返回 `403 Forbidden`；不存在第一轮的可选检查分支。
- `Host` 缺失/非法返回 `400 Bad Request`，错误 Host 返回 `403 Forbidden`；authorization 缺失或错误返回 `401 Unauthorized`。

### CLI 安全推导并发送精确 Origin

- `alda-agent/src/main.rs:358-378` 的 `command_endpoint` 要求 `http`、显式可解析 IP、`IpAddr::is_loopback()`，拒绝 hostname、远端 IP、HTTPS 和 URL userinfo，并在验证后固定为 `/v1/commands`、清除 query/fragment。
- `alda-agent/src/main.rs:331-340` 从上述规范化 `reqwest::Url` 的 `origin()` 取得 ASCII serialization，并通过 `Origin` header 发送；redirect policy 为 `none`，不会把 token/Origin 跟随到重定向目标。
- `alda-agent/src/main.rs:430-453` 的单元测试覆盖远端/含糊 host 和 userinfo 拒绝，以及带原始 path/query/fragment 的已验证 endpoint 最终推导出精确 `http://127.0.0.1:37891` Origin。

### HTTP 认证测试矩阵

`alda-agent/tests/http_round_trip.rs:17-142` 提供真实 loopback HTTP 证据：

- 合法 Host + 精确 Origin + 正确 token：命令成功；
- 缺失 Origin：`403`；
- 外来 Origin：`403`；
- 错误 Host：`403`；
- 缺失 token：`401`；
- 错误 token：`401`。

其后的 Project、Session、Turn cancel 和 event resume 成功路径均显式携带精确 Origin，因此测试不再以缺失 Origin 作为成功 oracle。

## 机械门控

在 `/home/mii/code/draft/alda-agent` 实际运行：

```text
cargo fmt --check                          PASS
cargo clippy --all-targets -- -D warnings PASS
cargo test                                PASS
```

测试结果为 library 7 个、binary 3 个、真实 HTTP integration 1 个，共 11 个测试通过，doc tests 0 个。

## 非阻断观察

真实 HTTP 测试没有构造“完全缺失 Host”的原始 HTTP/1 请求，也没有把 CLI binary 接到测试服务上直接观察其发出的 Origin；前者由 `http.rs:44-49` 的明确 fail-closed 分支支撑，后者由 endpoint 推导单元测试和 `submit` 的直接 header 设置支撑。增加 raw HTTP 缺失 Host 测试及 CLI 进程级 round trip 可进一步提升回归检测强度，但当前证据已足以关闭第一轮重大问题，不构成 A1 阻断项。

