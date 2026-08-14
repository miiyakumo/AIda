# 当前架构

> 代码基线：可组合指示系统首期实现（2026-08-14）
>
> 本文描述当前源码实际行为。

## 总体结构

项目是 Rust 2024 单 crate、单进程 CLI。`Application` 是 UI 无关的应用入口；终端只是一个适配器：

```text
Shell CLI（main.rs）
├── 默认进入项目交互
├── projects / compose / doctor
├── Machine control adapter（control.rs，JSONL）
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

`control` 是同一个应用入口的机器适配器。它从 stdin 逐行读取 JSON 请求，把 Agent 阶段事件、动作结果、
错误和动作后的双视图快照逐行写到 stdout；单个请求失败不会终止会话。控制面不直接修改项目文件，也不
提供跳过 Alda 校验、任意 shell 或模型密钥读写能力。

## Shell 与项目内命令

Shell 入口只有：

```text
alda-agent [--name NAME | --project PATH]
alda-agent projects
alda-agent compose [OPTIONS]
alda-agent doctor [--probe model|alda|all] # 无 probe 时只做本地环境检查
alda-agent [--name NAME | --project PATH] control
```

项目内命令按职责分组：自然语言输入进入 Agent；`/alda` 只执行本地工具动作；`/project` 查看和修改
持久项目；`/help` 提供分层帮助；`/quit` 退出。旧的扁平命令已删除。

## JSONL 控制面

每个请求包含调用方生成的字符串 `id` 和一个带 `type` 的 `action`。例如：

```json
{"id":"1","action":{"type":"agent","prompt":"发展当前草稿"}}
{"id":"2","action":{"type":"alda_check","target":"work"}}
{"id":"3","action":{"type":"project_accept"}}
```

每个长操作事件和最终响应都带相同 `id`。成功响应包含结构化 `result`、`project` 与 `conversation`；错误
响应包含 `error.kind`、`error.message` 和错误后的双视图快照。无法解析为 JSON 时 `id` 为 `null`；JSON
有效且含字符串 `id` 时，即使动作无效也会保留该 `id`。可用动作是：

| 分组 | `type` |
|---|---|
| Agent | `agent` |
| Alda | `alda_play`、`alda_stop`、`alda_check`、`alda_export` |
| Project | `project_overview`、`project_instructions`、`project_skills`、`project_skill_enable`、`project_skill_disable`、`project_versions`、`project_switch`、`project_adopt`、`project_accept`、`project_discard` |
| Config | `config_show`、`config_mode`、`config_duration`、`config_include`、`config_exclude`、`config_model`、`config_url` |

`alda_play` 和 `alda_check` 的 `target` 接受 `current`、`work` 或 `vN`；`alda_check` 也可改用 `file` 检查
外部文件。控制面刻意不支持模型密钥设置，密钥仍只能通过交互终端隐藏输入或已有的私有 `model.json`
提供，避免进入自动化命令与日志。

控制面用于真实模型调用、Alda 操作、状态流转、重启恢复和错误恢复等自动化验收。音乐听感、修改是否符合
主观意图和终端实际手感仍不能由结构化协议证明，需要人工判断；接受完整候选虽可被显式调用，但自动化
调用方必须先获得用户对该接受动作的授权。

TTY 使用 reedline 0.49，并显式启用 bracketed paste，使多行粘贴先完整进入编辑缓冲、等待 Enter 后再作为
一条请求提交；同时支持多行输入、项目级 500 条历史和 Tab 补全。Alt+Enter、
Shift+Enter 插入换行，普通 Enter 提交。非 TTY 使用逐行纯文本适配器，不输出控制序列或动画。
indicatif 0.18 只负责 TTY 活动指示；所有阶段和结果仍是稳定的语义事件与文本。
reedline 每轮输入前会查询终端光标位置；如果终端没有响应并触发该查询超时，TTY 适配器会在当前会话
内切换为基础逐行输入，而不是终止进程。此降级只失去多行编辑、历史导航和补全，不影响项目状态、
working score 或后续命令执行；其他 reedline I/O 错误仍作为真实故障返回。

TTY 的活动输入块分为 `项目 ·`、`状态 ·` 和 `›` 三层。项目行来自 `ProjectView`，状态行来自
`ConversationView::next_step`，输入和多行续行分别使用 `›`、`·` 标记。reedline transient prompt 在提交
时移除项目与状态行，只把带 `›` 的用户输入留在原生 scrollback；因此稳定上下文始终可见于当前输入块，
但不会在空输入和连续轮次中污染会话历史。活动 spinner 仍由 reporter 临时绘制，完成结果才进入历史。

## 项目聚合与双视图

`Project` 是聚合根，持有项目设置、至多一个工作乐谱、当前有效版本、线性版本元数据和一条供应商无关的
`Conversation`。Conversation 保存用户、模型和 Agent 内部工具消息，以及 `ready`、
`awaiting_input`、`revision_available` 状态。项目领域不依赖模型传输消息；Agent 边界负责转换。

工作乐谱分为草稿和完整候选。草稿只要求语法、内容和乐器约束，可在未达到整曲时长时试听；完整候选还
必须满足项目时长。两者都写入 `work.alda`，不会改变有效版本。项目只保留一个工作乐谱，新结果覆盖旧
结果；`/project accept` 接受完整候选并创建版本，`/project discard` 放弃工作乐谱。

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

## 可组合指示与 Skill

每次模型调用前，`Application` 从同一个 `Project` 读取 mode、目标时长和乐器约束，并通过
`SkillCatalog` 发现内建、用户级和项目级 Skill。指示编译器按固定顺序生成不可变的
`CompiledInstructions`：核心协议、应用能力边界、固定内建 workflow、按限定 ID 排序的 advisory Skill、
项目偏好和默认角色。每个片段保留来源、作用域、强度和 SHA-256 摘要，整体渲染结果具有 fingerprint。

默认创作方法是固定启用的 `builtin:progressive-composition`。外部首期只允许 advisory Skill，位于
`~/.alda-agent/skills/<name>/SKILL.md` 或 `<project>/skills/<name>/SKILL.md`；项目元数据只保存显式启用的
`user:name`/`project:name` 引用，不复制正文。发现阶段读取 frontmatter，编译生效项时才读取正文；加载器
限制扫描层级、数量和字节数，并在规范化路径后拒绝符号链接越界。

`/project skills` 查看发现结果，`/project skills enable|disable QUALIFIED_ID` 修改项目引用，
`/project instructions` 展示当前生效 Skill、偏好、角色、能力、冲突说明和 fingerprint。它描述当前配置，
不是历史 Invocation 快照。启用 Skill 缺失、损坏或超限时模型调用失败关闭，但本地项目、Alda 和禁用该
Skill 的恢复操作仍可用。

Skill 内容只影响模型输入。Alda 校验、工作乐谱写入、候选接受和有效版本创建仍由 `Application` 与
`Project` 强制；提示中的能力说明不授予任何运行权限。

## 生成与输出边界

Agent 在每轮报告轮次开始、模型文本增量、Alda 校验开始、完整检查结果和自动修正。模型传输层不写
stdout/stderr；SSE 文本经 callback 实时交给应用 reporter。终端统一渲染模型、Agent、Alda、项目结果
和错误。

模型每轮通过 `submit_result` 明确返回普通回答、澄清、创作计划、草稿或完整候选。文本结果只更新对话；
草稿和候选通过各自检查后更新工作乐谱。后续自然语言优先基于工作乐谱继续发展，不会按对话轮次创建版本。

Agent 产生的完整候选不会自动调用 `Project::save_version`。`/project accept` 会按当前项目约束重新校验，
通过后才创建版本并更新 `current.alda`；失败、取消、草稿和未接受候选都不会改变有效版本。显式
`/project adopt PATH` 仍可采用外部文件。版本切换不删除后续历史，新版本号始终递增。

## 持久化布局

```text
project-root/
├── project.json
├── model.json                 # 项目模型配置，Unix 0600
├── work.alda                  # 当前草稿或完整候选，可选
├── current.alda               # 当前有效版本，可选
├── .repl-history
├── skills/<name>/SKILL.md     # 项目级 advisory Skill，可选
├── versions/0001.alda ...
└── exports/version-0001.alda|mid ...
```

项目尚未发布，元数据更新不提供迁移层；旧项目若存在无元数据的遗留 `work.alda`，后续写入工作乐谱时会
直接覆盖。

## 验证基线

- `cargo test`：自动化测试通过；
- `cargo clippy --all-targets --all-features -- -D warnings`：通过；
- `cargo +1.85.0 check --locked`：通过；锁文件固定 Rust 1.85 可用的依赖链。

真实模型、真实 Alda 播放和完整人体工程学流程仍需在 Linux 终端进行最终验收。
