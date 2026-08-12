# AIda（暂定名）

Alda Music Agent 是一个面向音乐创作的终端 Agent。用户可以提供意象、叙事或直接的音乐要求，
让模型生成 Alda 乐谱；程序会调用本机 Alda 校验作品，并支持自然语言修改、版本恢复、播放和导出。

首期 CLI 创作闭环已经完成真实模型、双版本和用户试听验收。验收同时确认当前交互过于简陋、
不符合人体工程学；改善交互体验是下一阶段的核心需求。详见
[首期 CLI 创作闭环](docs/iter/first-cli-loop/README.md)和[CLI 交互体验改进](docs/todo/cli-ergonomics.md)。

## 当前能力

- 一次性生成完整曲目或即兴片段；
- 在 REPL 中创建和继续项目，以自然语言创作、澄清和修改；
- 检查 Alda 语法、非空内容、时长以及包含/排除乐器约束；
- 最多自动修正三轮，失败时不覆盖当前有效版本；
- 保存线性版本，查看历史、恢复旧版并采用人工编辑；
- 播放当前版本，导出 Alda 与 MIDI；
- 检查模型配置、Java、Alda 和 Rust 环境。

## 环境与配置

当前实现面向 Linux，需要 Rust 1.85+、Java 和 Alda 2。先检查或按提示安装运行环境：

```bash
cd alda-agent
bash scripts/install-linux.sh --check
```

安装脚本只处理或检查运行依赖，不安装 `alda-agent` 本身；当前版本从源码通过 Cargo 运行。

将仓库根目录的 `.env.example` 复制为 `.env`，填写模型服务配置：

```text
ALDA_AGENT_API_KEY=...
ALDA_AGENT_BASE_URL=https://your-model-service.example
ALDA_AGENT_MODEL=your-model-id
ALDA_AGENT_THINKING=disabled
```

客户端使用 OpenAI-compatible 的 `/v1/chat/completions` 流式接口。素材、创作要求、当前乐谱和
校验错误会发送给所配置的模型服务。

## 使用

```bash
cd alda-agent

# 检查完整运行环境
cargo run -- doctor

# 在指定目录创建或继续项目
cargo run -- repl --project ../my-music

# 或在默认目录 ~/.alda-agent/projects/demo 中创建或继续项目
cargo run -- repl --name demo --duration 180

# 从文本文件一次性生成 current.alda
cargo run -- create --file input.txt --duration 180 --output .
```

进入 REPL 后可输入 `/help` 查看命令，包括 `/play`、`/stop`、`/export`、`/history`、
`/restore`、`/reload`、`/continue` 和 `/strategy`。

## 开发验证

```bash
cd alda-agent
cargo fmt --all -- --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo run -- alda-smoke
```

项目架构、迭代状态和后续事项统一从[项目文档](docs/README.md)进入。
