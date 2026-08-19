# AIda

AIda 是一个面向音乐创作的终端 Agent。用户可以提供意象、叙事或直接的音乐要求，让模型生成 Alda 乐谱；
程序调用本机 Alda 校验作品，并支持自然语言修改、版本恢复、整曲或按段落播放，以及 Alda、MIDI、WAV
导出。

核心 CLI 创作闭环与交互改造已经过多轮真实模型、用户试听和终端渲染验证。项目仍在开发中，当前从源码
运行；架构说明、迭代状态和后续需求统一从[项目文档](docs/README.md)进入。

## 当前能力

- 一次性生成完整曲目或即兴片段；
- 在增强型滚动 REPL 中创建和继续项目，以自然语言创作、澄清和修改；
- 默认使用 single-agent，并可按项目切换到 Composer–Worker–Reviewer `composition-ab` 模式；
- 多行编辑、项目级历史、分层命令补全，以及持续可见的项目与任务状态；
- 检查 Alda 语法、非空内容、总时长、Marker 引用和包含/排除乐器约束；
- 自动修正直至校验通过或用户取消，失败时不覆盖当前有效版本，最新失败源码保留为继续修正的检查点；
- 保存线性版本，查看历史、恢复旧版并采用人工编辑的外部乐谱；
- 播放当前版本或工作乐谱（支持按段落定位），导出 Alda、MIDI 与 WAV；
- 按项目配置模型名称、OpenAI-compatible URL 和密钥；
- 组合内建、用户级与项目级 Skill，并查看当前生效指示的 fingerprint；
- 检查 Java、Alda、FluidSynth 与 GM SoundFont 环境并报告 Rust 工具链，按项目探测模型与 Alda 连通性。

## 环境与配置

运行需要 Java、Alda 2、FluidSynth 和一个 General MIDI SoundFont；从源码构建需要 Rust 1.85+。
当前面向 Linux 与 macOS。先检查或按提示安装运行环境：

```bash
cd alda-agent
bash scripts/install-linux.sh --check   # macOS 使用 scripts/install-macos.sh
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

# 按项目探测模型与本机 Alda
cargo run -- --project ../my-music doctor --probe all

# 在指定目录创建或继续项目
cargo run -- --project ../my-music

# 或在默认目录 ~/.alda-agent/projects/demo 中创建或继续项目
cargo run -- --name demo

# 使用指定项目的模型配置一次性创作完整曲目
cargo run -- --project ../my-music compose --file input.txt --duration 180 --output .

# 或生成即兴片段
cargo run -- --project ../my-music compose --file input.txt --mode improv --output .
```

进入 REPL 后可输入 `/help` 查看分组命令：`/alda` 负责播放、校验与导出，`/project` 负责版本和持久
设置。自然语言输入直接交给 Agent。新项目默认使用 single-agent；角色模式用于完整曲目，并需要目标时长：

```console
/project config duration 300
/agent composition-ab
# 输入完整曲目要求
/agent single
```

## 开发验证

```bash
cd alda-agent
cargo fmt --all -- --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo +1.85.0 check --locked
```

## 许可

本项目（自有代码与文档）以 [GPL-3.0-or-later](LICENSE) 授权。仓库内 `ref/` 为第三方参考项目、
`alda-agent/vendor/` 为 Alda 官方文档，各自保留原始许可证（Apache-2.0 / MIT / EPL-2.0 等）。
