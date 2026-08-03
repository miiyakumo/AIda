# Alda Agent 本地服务

本 crate 实现 [MVP 设计](../docs/design/mvp-design.md) 的 Slice A 与 Slice B 基础能力。
真实 `serve` 使用 durable v2 composition root；内存 `ServiceState` 只保留为显式测试
backend。服务仅允许监听回环地址，不能公开暴露。

当前能力包括：

- version 2 typed command、Project 与 Session durable projection、命令幂等和 typed error；
- Project/Session transaction log、control redo WAL、启动恢复、restart reconciliation 与实例锁；
- 专用 actor OS 线程、current-thread Tokio runtime、相互独立的有界 command/query 队列；
- Project、Session、Turn、Question、Approval 与 durable Alda fixture Artifact 流程；
- 同源 PWA、一次性浏览器引导码、HttpOnly cookie、`alda-agent.v2` WebSocket 事件恢复；
- 仅回环 HTTP adapter，以及复用同一 v2 command contract 的 CLI。

尚未实现 Provider、真实 Alda 工具、播放、MIDI、音频、保留策略、GC 或 compaction。

## 启动

data root 必须是显式绝对路径、普通目录且权限为 `0700`。服务在所有恢复、校验和
restart reconciliation 成功后才 bind；同一 data root 同时只允许一个实例。

```bash
mkdir -p /absolute/path/to/alda-data
chmod 700 /absolute/path/to/alda-data
export ALDA_AGENT_SESSION_TOKEN='replace-with-a-local-development-token'
cargo run -- serve --data-root /absolute/path/to/alda-data
```

服务会向可信终端打印一次性浏览器引导码。打开 `http://127.0.0.1:37891/`，在五分钟内
输入该码。浏览器会收到 `HttpOnly; SameSite=Strict` cookie；令牌、引导码和 cookie 值都
不得放入 URL 或浏览器存储。

WebSocket 重连流程固定为：保留客户端已完整处理的 sequence，使用 `alda-agent.v2`
重新连接，读取 `session.snapshot`，再从保留的 cursor 订阅。收到 typed cursor 恢复错误时，
改用新 snapshot 的覆盖范围。断线本身不会取消 Turn。

CLI 示例：

```bash
cargo run -- project create --command-id create-1 --name 'My project'
cargo run -- project snapshot --command-id snapshot-1 --project-id <project-id>
cargo run -- session start --command-id session-1 --project-id <project-id>
cargo run -- session snapshot --command-id snapshot-2 --session-id <session-id>
cargo run -- turn start --command-id turn-1 --session-id <session-id> \
  --prompt 'Write a short etude'
cargo run -- event resume --command-id resume-1 --session-id <session-id> \
  --epoch 1 --after-sequence 0
```

Artifact manifest 提供内容 hash。认证下载只接受 hash path 与 Project identity：

```bash
curl -H "Authorization: Bearer $ALDA_AGENT_SESSION_TOKEN" \
  -H 'Origin: http://127.0.0.1:37891' \
  -H 'X-Alda-Project-Id: <project-id>' \
  http://127.0.0.1:37891/v2/artifacts/<64-lowercase-sha256-hex>
```

## v1 升级说明

production 不提供 v1 path alias、WebSocket subprotocol 协商或 payload 降级。项目在 v1
阶段没有可迁移的持久数据；升级时必须改用 v2 HTTP path、`alda-agent.v2` 和
`protocol_version: 2`，然后重新连接。v2 transport 收到 v1 envelope 会返回 typed
`InvalidProtocolVersion`，不会执行命令。

`ArtifactDurability` wire 同时包含：

- `process_lifetime_fixture`：仅显式内存测试 backend 使用；
- `durable_local`：production durable Artifact 使用。

## 验证

```bash
cargo fmt --all -- --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
node --test web/client-state.test.js
for source in web/*.js; do node --check "$source"; done
```
