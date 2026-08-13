# 当前架构

> 代码基线：CLI 交互体验改进实现（2026-08-13）
>
> 本文描述当前源码实际行为。

## 总体结构

项目是 Rust 2024 单 crate、单进程 CLI。`Application` 是 UI 无关的应用入口；终端只是一个适配器：

```text
Shell CLI（main.rs）
├── 默认进入项目交互
├── projects / compose / doctor
└── Terminal adapter（repl.rs）
    ├── command.rs：分层动作解析、帮助与补全目录
    ├── application.rs：动作执行、能力装载、双视图快照
    ├── project.rs + conversation.rs：项目聚合与持久对话
    ├── agent.rs + deepseek.rs：模型生成、事件报告与协议转换
    └── alda.rs：校验、播放、停止、导出与单操作取消
```

终端不直接修改 Project，也不编排 Agent 生成循环。它把输入解析为 `UserAction`，调用
`Application::execute`，再读取 `ProjectView` 和 `ConversationView` 渲染提示符与结果。未来界面可以
直接复用这一入口。

## Shell 与项目内命令

Shell 入口只有：

```text
alda-agent [--name NAME | --project PATH]
alda-agent projects
alda-agent compose [OPTIONS]
alda-agent doctor [--probe model|alda|all] # 无 probe 时只做本地环境检查
```

项目内命令按职责分组：自然语言输入进入 Agent；`/alda` 只执行本地工具动作；`/project` 查看和修改
持久项目；`/help` 提供分层帮助；`/quit` 退出。旧的扁平命令已删除。

TTY 使用 reedline 0.49，并显式启用 bracketed paste，使多行粘贴先完整进入编辑缓冲、等待 Enter 后再作为
一条请求提交；同时支持多行输入、项目级 500 条历史和 Tab 补全。Alt+Enter、
Shift+Enter 插入换行，普通 Enter 提交。非 TTY 使用逐行纯文本适配器，不输出控制序列或动画。
indicatif 0.18 只负责 TTY 活动指示；所有阶段和结果仍是稳定的语义事件与文本。

TTY 的活动输入块分为 `项目 ·`、`状态 ·` 和 `›` 三层。项目行来自 `ProjectView`，状态行来自
`ConversationView::next_step`，输入和多行续行分别使用 `›`、`·` 标记。reedline transient prompt 在提交
时移除项目与状态行，只把带 `›` 的用户输入留在原生 scrollback；因此稳定上下文始终可见于当前输入块，
但不会在空输入和连续轮次中污染会话历史。活动 spinner 仍由 reporter 临时绘制，完成结果才进入历史。

## 项目聚合与双视图

`Project` 是聚合根，持有项目设置、当前有效版本、线性版本元数据和一条供应商无关的
`Conversation`。Conversation 保存用户、模型和 Agent 内部工具消息，以及 `ready`、
`awaiting_input`、`revision_available` 状态。项目领域不依赖模型传输消息；Agent 边界负责转换。

用户第一条消息在模型或 Alda 前置条件检查之前持久化，失败和取消不会丢失输入。`ProjectView` 展示
项目事实、版本、设置和能力；`ConversationView` 展示消息、待处理状态和下一步建议。两者每次从
Project 与进程内能力状态派生，不是新的事实来源。

## 能力与降级

项目、Alda 和模型能力独立：打开项目不创建模型客户端；自然语言操作才读取项目内 `model.json` 并
创建客户端。模型名称、OpenAI-compatible API Base URL 和密钥必须全部设置；模型失败不会阻止
`/project` 或可用的 `/alda` 操作。Alda 缺失时仍可导出版本的 Alda 源码；请求 MIDI 或 all 会明确
报告 MIDI 未完成。

模型配置完整性、最近模型服务状态和对话请求状态彼此独立。限流、认证、网络或模型拒绝不会把完整配置
标成不可用；界面按错误类型分别提示稍后重试、更新密钥或检查 API Base URL。用户消息在请求前以
`request_pending` 状态持久化；失败或取消后重新提交相同内容会复用原消息，不会重复污染模型上下文。

模型名称和 API Base URL 由普通 `/project config` 命令设置。密钥使用 `/project config key` 后的隐藏
输入，避免进入 `.repl-history`；携带明文参数的 key 命令会被拒绝且不写入历史，启动时也会清理旧历史
中的同类行。配置视图只显示密钥是否存在。`model.json` 在 Unix 上以 `0600` 权限原子写入。
程序不读取 `.env` 或模型环境变量。`compose` 与 `doctor --probe model` 通过 Shell 的 `--project` 或
`--name` 选择同一个项目配置，未指定时使用当前目录。

Alda 操作使用每次前台操作独立的 `CancellationToken`。Ctrl+C 在模型阶段丢弃 HTTP future；在 Alda
阶段设置 token 并等待子进程组终止后才返回提示符。Ctrl+C 编辑输入只清空缓冲，Ctrl+D 或 `/quit`
退出。

## 生成与输出边界

Agent 在每轮报告轮次开始、模型文本增量、Alda 校验开始、完整检查结果和自动修正。模型传输层不写
stdout/stderr；SSE 文本经 callback 实时交给应用 reporter。终端统一渲染模型、Agent、Alda、项目结果
和错误。

只有必要检查均未失败的候选才能调用 `Project::save_version`。失败候选、被取消候选和外部文件校验失败
都不会覆盖 `current.alda`。版本切换不删除后续历史，新版本号始终递增。

## 持久化布局

```text
project-root/
├── project.json
├── model.json                 # 项目模型配置，Unix 0600
├── current.alda
├── .repl-history
├── versions/0001.alda ...
└── exports/version-0001.alda|mid ...
```

项目尚未发布，本次元数据为破坏性更新，不迁移旧的 `source_material`、`requirements`、
`interpretation` 或供应商消息数组格式。

## 验证基线

- `cargo test`：自动化测试通过；
- `cargo clippy --all-targets --all-features -- -D warnings`：通过；
- `cargo +1.85.0 check --locked`：通过；锁文件固定 Rust 1.85 可用的依赖链。

真实模型、真实 Alda 播放和完整人体工程学流程仍需在 Linux 终端进行最终验收。
