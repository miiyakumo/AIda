# Repository Guidelines

## 项目结构与模块组织

当前实现位于 Rust crate `alda-agent/`。生产代码放在 `alda-agent/src/`：`main.rs` 是 CLI 与服务入口，`lib.rs` 导出库模块，领域、协议、HTTP、持久化和运行时逻辑按职责拆分。跨进程集成测试位于 `alda-agent/tests/`，单元测试与被测模块同文件维护。无依赖 PWA 及其 Node 测试位于 `alda-agent/web/`。需求、研究、设计、计划和评审分别归档到 `docs/` 对应目录。`ref/` 是被忽略的上游参考材料，不属于项目实现；不要提交 `target/` 等生成物。

## 构建、测试与本地开发

以下命令均在 `alda-agent/` 中执行：

```bash
cargo build                         # 编译 crate
cargo run -- serve                  # 启动本地回环服务
cargo fmt --check                   # 检查 Rust 格式
cargo clippy --all-targets -- -D warnings
cargo test                          # 运行单元及集成测试
node --test web/client-state.test.js
```

启动服务前设置仅用于本地开发的 `ALDA_AGENT_SESSION_TOKEN`。项目使用 Rust 1.85 与 edition 2024。

## 编码风格、命名与语言

接受 `rustfmt` 输出（四空格缩进），并保持 `clippy::all`、`clippy::pedantic` 零警告。模块、文件、函数和变量使用 `snake_case`，结构体、枚举和 trait 使用 `UpperCamelCase`，常量使用 `SCREAMING_SNAKE_CASE`。优先使用小模块、明确的领域类型和 `Result` 错误传播，避免对可恢复错误执行 panic。Web 文件沿用两空格缩进、ES modules 与 camelCase。

项目代码注释（包括 `///` 和 `//!`）以及仓库文档必须使用中文；代码标识符、协议字段、命令和不可翻译的专有名词可保留原文。注释应解释约束与设计原因，不要复述代码表面行为。

## 设计参考

设计关键模块、运行时边界或基础设施方案前，可按问题定向阅读 `ref/codex/` 与 `ref/grok-build/`，吸收其模块划分、错误处理、安全边界和测试策略。参考代码不能直接视为本项目需求；应结合音乐 Agent 的领域约束进行取舍，并在设计文档中记录借鉴来源、采用理由及差异。

## 测试规范

Rust 使用内置测试框架，异步场景使用 `#[tokio::test]`。测试名描述可观察行为，例如 `typed_cursor_recovery_resets_to_snapshot_coverage`。公共传输流程放入 `tests/*_round_trip.rs`，局部不变量放入模块内 `tests`。修改客户端状态或恢复逻辑时同步更新 Node 测试。项目暂无覆盖率门槛，但每项行为变更都应带回归测试。

## 提交与拉取请求

近期提交采用简短、祈使式、带类型前缀的主题。使用 `<type>: <中文摘要>`，例如 `feat: 增加会话恢复测试`，并保持单次提交聚焦。拉取请求需说明行为与设计影响、列出验证命令、关联问题或设计文档；可见 PWA 变更需附截图。协议、持久化或安全假设必须明确标注。

## 安全与配置

不得提交令牌、引导码或本地产物。保留仅回环地址可用的 Host/Origin 校验；不要公开暴露开发服务，也不要在示例中弱化认证。
