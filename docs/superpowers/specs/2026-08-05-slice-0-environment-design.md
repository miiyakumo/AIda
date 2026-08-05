# 切片 0：环境与骨架 设计文档

> 日期：2026-08-05
> 状态：已确认
> 需求基线：`docs/plans/initial-implementation-plan.md` 第 4 节

## 1. 目标

用户可以检查本机是否具备运行 Alda Music Agent 所需的运行时环境，并通过一键脚本安装缺失依赖。

## 2. Crate 结构与文件清单

```
alda-agent/
├── Cargo.toml
├── scripts/
│   └── install-linux.sh          # 一键安装脚本
├── src/
│   ├── main.rs                   # CLI 入口 + clap 子命令分发
│   ├── doctor.rs                 # doctor 检查逻辑
│   └── lib.rs                    # crate 根，供测试引用
└── tests/
    └── fixtures/
        └── minimal.alda          # 最小合法 Alda 文件，验证 alda parse
```

- `main.rs`：clap 解析、`doctor` 子命令路由
- `doctor.rs`：一个 `pub fn run() -> anyhow::Result<()>`，顺序执行各项检查
- `lib.rs`：`pub mod doctor;`，后续切片按需追加 `pub mod deepseek;` 等
- 不建 `error.rs`、`config.rs`、`output.rs` 等辅助模块——切片 0 不需要

## 3. CLI 框架

使用 `clap` derive 模式。入口结构：

```rust
#[derive(Parser)]
#[command(name = "alda-agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Doctor,
}
```

切片 0 只有一个子命令。后续切片中的 REPL 命令（`/play`、`/export` 等）在切片 4 中作为 REPL 内部分发，不在此处暴露为 clap 子命令。

## 4. doctor 检查项

面向终端用户的运行时检查。不包含 Rust 工具链——那是开发者从源码构建才需要的，放在安装脚本的可选段落中。

### 4.1 检查项列表

| # | 检查项 | 方法 | 状态 |
|---|---|---|---|
| 1 | Java 运行环境 | `java -version` | ✅ / ❌ |
| 2 | Alda 可执行文件 | PATH 查找 `alda` | ✅ / ❌ |
| 3 | Alda 版本 | `alda version --no-update-check` | ✅ / ❌ |
| 4 | Alda 解析能力 | `alda parse --file tests/fixtures/minimal.alda` | ✅ / ❌ |
| 5 | 播放链 | Alda 播放后端可用性检测 | ✅ / ⚠️ / ❌ |

### 4.2 执行逻辑

顺序执行。如果前置项失败导致后续项无意义（例如 Alda 未安装 → 版本和解析无意义），后续项标记为"跳过（依赖未满足）"而非"失败"，避免一连串误导性错误。

示例输出：

```
$ cargo run -- doctor

  Java 运行环境   ✅  openjdk 21.0.4
  Alda            ❌  未找到 alda
                      → 运行 scripts/install-linux.sh 或访问 https://alda.io/install
  Alda 版本       ⏭  跳过（Alda 未安装）
  Alda 解析       ⏭  跳过
  播放链          ⏭  跳过

环境状态：1/2 通过，3 项跳过，1 项失败
```

### 4.3 播放链检测策略

Alda 播放后端可能为 `alda-player`（内置）或外部 MIDI 合成器。首期不深入探测 MIDI 设备细节，仅检测：
- Alda 播放命令是否可用（通过 `alda doctor` 或等效检查）
- 若有明确的可检测后端，输出设备名；否则标注 ⚠️ 并建议用户使用 `alda play` 实际验证

## 5. 安装脚本

### 5.1 职责

检查并协助安装运行时依赖。不静默提权、不修改 shell 配置文件。

### 5.2 结构

```
scripts/install-linux.sh
  ├── 检测发行版（apt / dnf / pacman / 其他）
  ├── 检查 Java → 给出安装命令，确认后执行
  ├── 安装 Alda → 使用官方安装方式
  ├── 检查播放链环境
  ├── 【可选段落】Rust 工具链（从源码构建时需要）
  ├── 运行基本健康检查（alda version, alda parse fixture）
  └── 总结：打印仍需手动处理的项
```

### 5.3 行为约定

- `sudo` 操作明确提示并等待确认
- 不写 `.bashrc`、`.profile`、`/etc/environment`
- 幂等：已安装的跳过
- 结尾打印 `cargo run -- doctor` 提示用户验证

## 6. 依赖

```toml
[dependencies]
clap = { version = "4", features = ["derive"] }
anyhow = "1"

[dev-dependencies]
# 暂无额外依赖
```

不需要 `tokio`、`reqwest`、`serde`——切片 0 不涉及网络和序列化。

## 7. 测试

| 测试 | 类型 | 说明 |
|---|---|---|
| clap 命令行解析 | 单元测试 | 验证 `doctor` 子命令正确路由 |
| fixture 文件存在 | 单元测试 | `tests/fixtures/minimal.alda` 可读且非空 |
| fixture 语法合法 | 集成测试 `#[ignore]` | `alda parse` 无报错，仅本机有 Alda 时运行 |

不测试的事项：网络请求、音频输出、真实 Alda 命令（离线测试中跳过）。

## 8. 门禁

```bash
cargo fmt --all -- --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

## 9. 完成条件

- 全新 Linux 环境能按照脚本输出完成依赖准备
- 缺少依赖时 `doctor` 指出缺少什么以及如何修复
- `cargo run -- doctor` 能报告 Alda 运行时状态
- 默认 Rust 测试不需要网络或真实音频设备
