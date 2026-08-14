# AIda（暂定名）

Alda Music Agent 是一个面向音乐创作的终端 Agent。用户可以提供意象、叙事或直接的音乐要求，
让模型生成 Alda 乐谱；程序会调用本机 Alda 校验作品，并支持自然语言修改、版本恢复、播放和导出。

首期 CLI 创作闭环已经完成真实模型、双版本和用户试听验收。CLI 交互体验改进已完成代码实施和
自动化验证，等待真实终端验收。详见[首期 CLI 创作闭环](docs/iter/first-cli-loop/README.md)和
[CLI 交互体验改进方案](docs/iter/cli-ergonomics/README.md)。

## 当前能力

- 一次性生成完整曲目或即兴片段；
- 在增强型滚动 REPL 中创建和继续项目，以自然语言创作、澄清和修改；
- 多行编辑、项目级历史、分层命令补全和持续可见的项目/任务状态；
- 检查 Alda 语法、非空内容、时长以及包含/排除乐器约束；
- 最多自动修正三轮，失败时不覆盖当前有效版本；
- 保存线性版本，查看历史、恢复旧版并采用人工编辑；
- 播放当前版本，导出 Alda 与 MIDI；
- 按项目配置模型名称、OpenAI-compatible URL 和密钥；
- 组合内建、用户级和项目级 Skill，并查看当前有效指示 fingerprint；
- 检查 Java、Alda 和 Rust 环境，并按项目探测模型连通性。

## 环境与配置

当前实现面向 Linux，需要 Rust 1.85+、Java 和 Alda 2。先检查或按提示安装运行环境：

```bash
cd alda-agent
bash scripts/install-linux.sh --check
```

安装脚本只处理或检查运行依赖，不安装 `alda-agent` 本身；当前版本从源码通过 Cargo 运行。

模型配置属于项目。先进入项目，再通过项目内命令分别设置模型名称、API Base URL 和密钥：

```console
/project config model your-model-id
/project config url https://your-model-service.example
/project config key
```

密钥由隐藏输入读取，不进入 REPL 历史；若误把密钥写在命令后，程序会拒绝并排除该历史项。三项设置
持久化在项目的私有 `model.json` 中，状态和配置输出不会回显密钥。客户端使用 OpenAI-compatible 的
`/v1/chat/completions` 流式接口。素材、创作要求、当前乐谱和校验错误会发送给所配置的模型服务。

## 使用

```bash
cd alda-agent

# 检查完整运行环境
cargo run -- doctor

# 可选：探测指定项目的模型与本机 Alda
cargo run -- --project ../my-music doctor --probe all

# 在指定目录创建或继续项目
cargo run -- --project ../my-music

# 或在默认目录 ~/.alda-agent/projects/demo 中创建或继续项目
cargo run -- --name demo

# 使用指定项目的模型配置一次性生成 current.alda
cargo run -- --project ../my-music compose --file input.txt --duration 180 --output .
```

进入 REPL 后可输入 `/help` 查看分组命令：`/alda` 负责播放、校验与导出，`/project` 负责版本和
持久设置。自然语言输入直接交给 Agent。

## 开发验证

```bash
cd alda-agent
cargo fmt --all -- --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo +1.85.0 check --locked
```

项目架构、迭代状态和后续事项统一从[项目文档](docs/README.md)进入。
