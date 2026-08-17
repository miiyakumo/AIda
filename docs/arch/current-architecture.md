# 当前架构

> 代码基线：启动环境门禁、模型文本流、乐谱诊断与候选音频门禁（2026-08-17）
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
    ├── alda.rs：解析、校验、播放、停止、MIDI 导出与单操作取消
    └── audio.rs：FluidSynth 渲染、SoundFont 发现和 WAV 信息分析
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
外部文件。`alda_export` 支持 `current`、`work`、`vN` 和 `alda`、`midi`、`wav`、`all` 格式，默认生成全部
产物。控制面刻意不支持模型密钥设置，密钥仍只能通过交互终端隐藏输入或已有的私有 `model.json`
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

`Project` 是聚合根，直接持有规范化、强类型的 `ProjectPreferences`、至多一个工作乐谱、至多一个待修正
候选、当前有效版本、线性版本元数据和一条供应商无关的 `Conversation`。持久 Conversation 只保存用户
消息和成功提交的语义助手消息，以及 `ready`、`awaiting_input`、`revision_available`、`request_pending`
状态；供应商 tool call、tool result、失败候选源码和模型临时推演只存在于当前模型往返。加载旧项目时会
压缩遗留的供应商协议轨迹。项目领域不依赖模型传输消息；Agent 边界负责转换。
mode 使用可序列化枚举；提示编译和 Alda 校验都从同一份 Project 偏好派生。持久化检查记录使用强类型
`CheckStatus`，JSON 中继续保持既有的中文状态值。

工作乐谱分为草稿和完整候选。草稿只要求语法、内容和乐器约束，可在未达到整曲时长时试听；完整候选还
必须满足项目时长。两者都写入 `work.alda`，不会改变有效版本。项目只保留一个工作乐谱，新结果覆盖旧
结果；`/project accept` 接受完整候选并创建版本，`/project discard` 放弃工作乐谱。

本轮最终仍失败但包含源码时，只把最新一份源码写入 `revision.alda`，`project.json` 保存对应种类、摘要、
检查结果与源码 SHA-256。重启后哈希不一致或文件缺失视为项目损坏；更晚的失败候选覆盖更早候选，成功
生成工作稿时清除 revision。失败候选之后即使发生模型服务错误或协议恢复熔断，也先保存 revision 再把
终止错误返回给调用方。这样后续模型仍可继续修正源码，而持久对话不需要保存大段 Alda 或失败工具轨迹。

用户消息在模型配置前置检查通过后持久化，避免缺失配置时把配置值或无效请求污染创作会话；进入模型请求
后的失败和取消仍保留请求状态。`ProjectView` 展示
项目事实、版本、设置和能力；`ConversationView` 展示消息、待处理状态和下一步建议。两者每次从
Project 与进程内能力状态派生，不是新的事实来源。

## 能力与降级

模型客户端按需创建：自然语言操作才读取项目内 `model.json`。模型名称、OpenAI-compatible API Base URL
和密钥必须全部设置；模型服务失败不会阻止已经启动的 `/project` 或 `/alda` 操作。

Java、Alda、FluidSynth 与 General MIDI SoundFont 是启动 REPL、`control` 或 `compose` 的统一运行时
前置条件。Shell 在选择或创建项目、读取 compose stdin、进入 UI 之前一次检查全部四项；任一项不可发现
就拒绝启动，并提示先运行 `scripts/install-linux.sh` 后用 `alda-agent doctor` 验证。`projects` 和
`doctor` 本身不受门禁限制，缺少依赖时仍可列项目和诊断环境。Rust 只用于从源码构建，不是已编译程序的
运行时门禁。

Linux 安装脚本在应用启动前安装或检查四项运行时依赖，可用 `ALDA_AGENT_SOUNDFONT` 指向非标准
SoundFont。`doctor` 还报告源码构建所需的 Rust 工具链；Alda probe 会真实执行 Alda → MIDI → WAV，
并拒绝零帧或静音 WAV。

模型配置完整性、最近模型服务状态和对话请求状态彼此独立。限流、认证、网络或模型拒绝不会把完整配置
标成不可用；界面按错误类型分别提示稍后重试、更新密钥或检查 API Base URL。用户消息在请求前以
`request_pending` 状态持久化；失败或取消后重新提交相同内容会复用原消息，不会重复污染模型上下文。

模型名称和 API Base URL 由普通 `/project config` 命令设置；命令缺少值时 TTY 会立即读取一次配置输入，
不会把下一行当作 Agent 消息。密钥使用 `/project config key` 后的隐藏
输入，避免进入 `.repl-history`；携带明文参数的 key 命令会被拒绝且不写入历史，启动时也会清理旧历史
中的同类行。配置视图只显示密钥是否存在。`model.json` 在 Unix 上以 `0600` 权限原子写入。
程序不读取 `.env` 或模型环境变量。`compose` 与 `doctor --probe model` 通过 Shell 的 `--project` 或
`--name` 选择同一个项目配置，未指定时使用当前目录。

Alda 操作使用每次前台操作独立的 `CancellationToken`。Ctrl+C 在模型阶段丢弃 HTTP future；在 Alda
阶段设置 token 并等待子进程组终止后才返回提示符。Ctrl+C 编辑输入只清空缓冲，Ctrl+D 或 `/quit`
退出。Alda 2.3.3 的 MIDI 导出可能在冷启动时先拉起后台 player/JVM，因此默认命令超时为 120 秒；超时或
取消时仍终止完整子进程组。

## 可组合指示与 Skill

每次模型调用前，`Application` 从同一个 `ProjectPreferences` 读取 mode、目标时长和乐器约束，并通过
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

Agent 只有交互式 `respond_with_reporter` 和一次性 `create` 两个真实生成入口，两者共享同一个内部
`run_generation` 循环。`Application::execute` 驱动交互入口；`application::prepare_compose` 与
`application::compose_once` 负责一次性 compose 所需的 Project、模型配置、Skill、指示和 Alda 编排，
`main.rs` 只处理 CLI 输入输出、文件读取写入与退出语义。

Agent 在每次结果提交前报告开始事件，并报告候选校验、完整检查结果和自动修正。模型传输层不写
stdout/stderr；它把 SSE 中的 `content` 与 `submit_result.message` 解码为增量 `ModelText` 语义事件。
终端在“模型”块中即时追加文本，JSONL 控制面逐增量输出带请求 ID 的 `model_text` 事件；其他工具参数
（包括 Alda 源码）不回流到 UI。宿主阶段、检查、结果和错误继续由各 UI 适配器统一渲染。

模型请求在提供工具时显式发送 `parallel_tool_calls: false`，每次响应只允许调用一个工具。除 `submit_result`
外，宿主提供 `lookup_alda_docs`、`inspect_score`、
`render_score` 和 `play_score`：分别读取固定官方章节、检查已有乐谱、生成 MIDI/WAV 并返回真实音频指标、
以及真实发起播放。宿主工具往返和协议恢复不计作候选提交。SSE 按 tool-call index 分别聚合；若供应商仍
返回多个并行调用，宿主拒绝且不执行全部调用，把每个对应错误写回上下文后自动继续。模型只返回普通文本
或空响应而未调用工具时，宿主同样把协议错误写回并自动继续；原始文本不作为已完成结果持久化，但会按
增量显示。终端将工具往返、协议恢复和候选提交分别显示，不再展示固定提交额度。

生成循环没有固定三次限制，也不再因连续无改善、相同失败或回到既有失败签名而提前停止。失败候选始终
把检查反馈交还模型继续修正，直到成功、用户取消，或命中默认 15 分钟、24 次模型调用、8 次协议恢复之一。
运行时依赖已在应用启动前门禁，因此缺少 Alda、FluidSynth 或 SoundFont 不会进入模型生成循环。

模型最后通过 `submit_result` 明确返回普通回答、澄清、创作计划、草稿或完整候选。计划必须结构化携带核心
材料、曲式、配器和发展方式，宿主将其拼成用户可见正文；带提问意图的回答归类为澄清。文本结果只更新对话；
草稿和候选由宿主真实解析并通过各自检查后更新工作乐谱。草稿不自动渲染；完整候选只有在静态检查全部
通过后才自动导出临时 MIDI、渲染 WAV 并检查整首非静音，随后一起保存为 `work.alda`、
`exports/work.mid`、`exports/work.wav`。渲染失败、零帧、零时长或整首静音都是硬失败，不会覆盖已有工作
稿或音频。后续自然语言优先基于工作乐谱继续发展，不会按对话轮次创建版本。

Alda 静态检查除语法、内容、时长和乐器约束外，还扫描 `%marker` 与 `@marker`：重复定义、未定义引用和
先引用后定义均为硬失败。解析事件的 `part`、`offset`、`audible-duration` 必须引用已存在声部并为有限非负
值。每个声部的首尾、事件数、实际发声时长、最大空档、覆盖率，以及决定全曲结尾的声部、尾差和全局事件
空档只作为诊断；晚进入、长休止和声部不等长本身不失败。全局事件区间以 150 ms 容差合并，避免把正常
量化间隙误判成长静音。

WAV 以 100 ms 窗口分析 peak 与 RMS，报告开头、结尾、最长内部静音、静音占比和代表区间。局部静音同样
只作为诊断，不使用固定的“超过若干秒即失败”阈值；当前硬门禁只拒绝整首无有效音频。

用户消息中的精确或区间总时长（如“目标 3 分钟”“2–3 分钟”）在模型调用前写入项目偏好；旧数字格式
保持兼容，区间保存为 `min_secs/max_secs` 并按硬边界校验。“数分钟”等含糊描述不被擅自折算，“第 3 分钟”
“开头 30 秒”等段落位置不会被误存为总时长。用户明确要求编写完整曲目时，固定工作流要求直接提交完整
候选；宿主还会拒绝回答、计划或短草稿，并在同一轮自动要求模型改交 `candidate`。每轮结果同时报告工作
乐谱是否真的改变；校验通过、渲染成功和播放成功是三个
独立事实，失败候选不会覆盖已有工作稿，播放后替换 work 时播放对象会明确标为修改前工作乐谱。

“编曲/作曲/写曲/写一首/开始创作”等完成意图同样受完整候选策略约束，但“计划/方案/思路/建议”等讨论
请求不触发。完整候选要求作为对话状态持久化，跨澄清回答、模型请求失败和进程重启保留，直到候选完成或
对话以非待定结果结束。用户回答一次澄清后，宿主禁止模型再次进入澄清，避免连续询问或把原始创作意图
降级成拒绝/替代方案。题材、体裁和时长已经明确时，未指定配器等可选项不构成阻塞。

`submit_result` 参数解析失败不再终止用户请求。宿主把不完整或无效参数作为工具错误写回模型并自动重试，
该往返不计作候选提交，终端以“自动恢复工具参数”独立显示；仍受协议恢复和模型调用安全上限约束。

系统提示由核心协议和 `prompts/alda-reference.md` 共同编译。完整官方快照保存在
`vendor/alda-docs/2.4.3/`，含来源提交与 EPL-2.0 许可证；当前运行时兼容目标仍为 Alda 2.3.3，因此精简
参考中的示例由实际 2.3.3 解析测试验证，官方来源版本与运行时兼容版本不会混称。

Agent 产生的完整候选不会自动调用 `Project::save_version`。`/project accept` 会按当前项目约束重新校验，
通过后才创建版本并更新 `current.alda`；失败、取消、草稿和未接受候选都不会改变有效版本。接受候选时，
新 immutable version、当前版本元数据和清除工作状态以一次 `project.json` 写入作为提交点。显式
`/project adopt PATH` 仍可采用外部文件。版本切换不删除后续历史，新版本号始终递增。

## 持久化布局

```text
project-root/
├── project.json
├── model.json                 # 项目模型配置，Unix 0600
├── work.alda                  # working metadata 引用的规范工作源码，可选
├── revision.alda              # pending_revision 引用的最新失败源码，可选
├── current.alda               # 当前有效版本投影，可选
├── .repl-history
├── skills/<name>/SKILL.md     # 项目级 advisory Skill，可选
├── versions/0001.alda ...     # 不可变版本源码
└── exports/
    ├── work.mid|wav            # 已验证完整候选的工作产物，可选
    └── version-0001.alda|mid|wav ...
```

WAV 使用 FluidSynth 离线渲染。产物报告包含 Alda 解析时长、声部数、事件数、乐器，以及 WAV 实际时长、
采样率、声道数、帧数、peak、RMS 和静音判断；因此“有可播放事件”和“生成了非静音音频”不再混为一谈。

`project.json` 与其引用的 immutable version 文件共同构成已接受版本的规范事实，`current.alda` 是便利投影。
working metadata 与其引用的 `work.alda` 共同构成规范工作状态；被引用的 `work.alda` 缺失视为项目损坏，
自动生成的完整候选同时保存工作 MIDI/WAV。pending revision metadata 与带哈希校验的 `revision.alda`
共同构成待修正状态。只有对应 metadata 已清除后的残留投影才可清理。项目加载时会从当前 version 修复
缺失或陈旧的 `current.alda`，并删除严格匹配协议文件名但未被元数据引用的中断残留。项目尚未发布，
元数据更新不提供迁移层。

## 验证基线

- `cargo test`：自动化测试通过；
- `cargo clippy --all-targets --all-features -- -D warnings`：通过；
- `cargo +1.85.0 check --locked`：通过；锁文件固定 Rust 1.85 可用的依赖链。

真实 Alda → MIDI → WAV 已在 Linux 环境验收：4.45 秒、44.1 kHz、双声道、peak 0.0140、RMS 0.0021，
非静音。真实模型的长对话音乐质量和终端人体工程学仍需人工验收。
