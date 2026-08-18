# Coding Agent 提示词的组织方式与控制原理

## 结论

成熟 Coding Agent 的提示词并不是一篇写得足够全面的“总说明书”，而是一套由宿主参与执行的控制协议。
本轮直接检查 `ref/` 当前源码后，最值得借鉴的规律是：

1. **稳定身份要短，能力规则跟随能力注册。** DeepSeek Harness 的基础身份只有一句，文件、Shell、Goal、
   Subagent、Workflow 和 Plan Mode 各自贡献自己的提示段。工具不存在时，对应规则也不存在。
2. **静态规则与动态状态必须分开。** 当前权限、工作目录、模式、目标轮次和失败状态应由宿主在当下轮次注入，
   不能让模型从旧对话里猜。
3. **失败后的升级不是普通建议。** “不要重复”“必要时委派”只能提供方向；何时提醒、何时冻结假设、何时要求
   独立复核，应由可观察事件和计数器触发。
4. **不同可靠性要求使用不同约束层。** 风格偏好适合写提示词；输出形状适合工具 schema；权限、提交门禁、
   调用上限和关键状态转换应由宿主执行。
5. **上下文是运行时资源。** 长篇自述、递归摘要和无关工具结果会改变模型后续决策。提示词设计同时也是注意力
   分配和上下文预算设计。

这正好解释了当前 Composer 失败：它并非不知道“查文档、不要手算、必要时委派”，而是这些规则没有进入
“同类失败第几次、当前主假设是什么、测试是否等价、剩余多少预算”这一运行时决策闭环。

## 调研范围

- 日期：2026-08-18。
- 只研究当前工作区源码和仓库内文档，没有查询 Git 历史、提交、分支或 blame。
- 重点：`ref/deepseek-harness`。
- 对照：`ref/cloud-code/claude-code-source`、`ref/codex`、`ref/grok-build`、`ref/paseo`。
- 目的不是评判模型能力，而是分析宿主怎样组织模型可见指令、运行时状态和失败升级。

## 横向概览

| 项目 | 主要风格 | 规则主要放在哪里 | 失败闭环 | 主要取舍 |
|---|---|---|---|---|
| DeepSeek Harness | 插件化、契约式、状态驱动 | 有序 system section、工具 schema、动态 context、事件插件 | 事件提醒、Goal 续轮、受限 Subagent；通用循环提醒仍是软约束 | 组合精确，可观测性强，框架复杂度高 |
| Claude Code | 长篇行为手册、启发式明确 | 主提示词、Agent 专项提示、compact 提示 | 明写失败纪律和委派判据，较依赖模型遵循 | 可读、规则集中，但主提示较长 |
| Codex | 核心指令逐步变短，模式和环境外置 | 基础指令、developer/world-state、模式模板 | 权限和模式由宿主固定，计划/执行分态 | 状态边界清楚，需要宿主提供完整拼装 |
| Grok Build | 主提示短，Goal 子系统角色化 | 普通模板 + Planner/Implementer/Verifier/Strategist 模板 | 不收敛后由宿主切换独立角色 | 闭环最强，但主要服务长任务 Goal |
| Paseo | 编排层和可安装 Skill | 外部 Agent 原提示 + committee/advisor/handoff skills | 卡住或需要第二意见时显式选择工作流 | 不拥有统一主 Agent，依赖上游能力 |

## DeepSeek Harness：它实际怎样“写提示词”

### 1. 没有一篇万能总提示词

基础身份在
[`packages/core/system-prompt/src/index.ts`](../../ref/deepseek-harness/packages/core/system-prompt/src/index.ts)
中注册，部署 Persona 则由预设覆盖。标准预设的 Persona 只有：模型身份与当前工作目录，见
[`agent.cordis.yml`](../../ref/deepseek-harness/apps/cli/config/agent-presets/standard/agent.cordis.yml)。

具体能力自己注册提示段，例如：

- 文件 `read`、`write`、`edit` 使用相邻的 100、101、102 顺序；
- Shell 使用 105，后台任务使用 106；
- Goal 使用 114，Workflow 使用 115，Ralph 使用 116；
- Subagent 的 `report` 只在可继续的子 Agent 中注册。

相关实现分别位于
[`tool-fs`](../../ref/deepseek-harness/packages/fs/tool-fs/src/read.ts)、
[`tool-bash`](../../ref/deepseek-harness/packages/shell/tool-bash/src/index.ts)、
[`tool-goal`](../../ref/deepseek-harness/packages/goal/tool-goal/src/index.ts)、
[`tool-workflow`](../../ref/deepseek-harness/packages/workflow/tool-workflow/src/index.ts) 和
[`tool-ralph`](../../ref/deepseek-harness/packages/workflow/tool-ralph/src/index.ts)。

这种写法的重点不是把一个长文件拆成多个文件，而是让三件事共用同一生命周期：

```text
能力插件装载
    ├── 注册工具 schema
    ├── 注册该工具的行为规则
    └── 注册必要的运行时 context

能力插件卸载
    └── 三者一起消失
```

因此不会出现“提示词要求使用某工具，但当前 Agent 根本没有这个工具”，也不容易出现工具行为升级而总提示词
忘记同步的问题。

### 2. Prompt 被当成有类型的装配结果

`SystemPrompt` 不直接拼接任意字符串，而是分别管理：

- `sections`：稳定的 system 指令；
- `contexts`：当前轮次的动态状态；
- `tools`：模型可见工具 schema；
- `variables`：严格插值变量。

装配时按作用域决定覆盖关系，按 `order` 稳定排序工具规则；工具也按显式顺序或稳定字典序排列。未知工具名、
重复注册、非法顺序、未知变量或未定义变量都会直接失败，不静默降级。动态 context 渲染成完整快照，并带有：

> Current runtime context. This snapshot supersedes earlier runtime-context snapshots.

底层原因有三个：

- **确定性**：相同组合得到相同顺序，便于缓存、快照测试和复现。
- **新状态覆盖旧叙述**：权限、cwd、审批策略改变后，模型不必自行判断哪段历史仍有效。
- **故障前移**：拼装错误在请求模型前暴露，而不是变成难以归因的模型行为漂移。

### 3. 写作风格像运行规约，不像教程

DeepSeek Harness 的局部提示通常有以下语言特征：

- 先规定当前组件的职责边界，再描述允许动作；
- 明确权威来源，例如当前 workspace、工具结果、durable state；
- 明确优先级和覆盖关系，例如当前快照替代旧快照；
- 把终止条件和输出义务写清楚；
- 避免在每个局部段重复整个 Agent 的通用人格。

Goal 自动续轮提示是典型例子，见
[`goal-round-driver/src/prompt.ts`](../../ref/deepseek-harness/packages/goal/goal-round-driver/src/prompt.ts)：它要求继续同一目标，
把当前工作区、工具结果和持久状态视为权威，先检查实际状态，再收集足够的整体证据后完成。这段话并不重新
讲解所有工具，只修正“跨轮继续工作”最容易漂移的几个决策点。

### 4. Subagent 的边界由宿主固定

子 Agent 会继承父级组合，但在创建边界上被重新注入委派上下文、工具过滤和审批策略。审批被固定为
`never`，子 Agent 不能通过自行请求扩大权限；相关机制见
[`child-agent.ts`](../../ref/deepseek-harness/packages/subagent/subagent/src/child-agent.ts)。

`report` 义务同时出现在系统提示和工具 schema 中，见
[`tool-subagent-report`](../../ref/deepseek-harness/packages/subagent/tool-subagent-report/src/index.ts)。这是有意的重复：
schema 负责接口合法性，system 指令负责让模型理解该动作是完成协议，而不是普通可选工具。这里的重复是跨层
冗余，不是同一层反复说教。

### 5. Compaction 压缩状态，不复制思考过程

Compaction 提示要求保留用户请求、关键文件、已做修改、测试和错误、当前工作及单一下一步；已有摘要需要与
新事实合并，删除过时结论，而非完整套娃。实现见
[`summarizer.ts`](../../ref/deepseek-harness/packages/compaction/compaction-basic/src/summarizer.ts)。

它的底层假设是：续作需要的是可验证状态和未完成边界，不是模型此前怎样犹豫、怎样排列过临时计划。保留过多
过程性叙述会产生锚定效应，让后续轮次继续回应已经过时的假设。

### 6. 它也证明“动态提示”不是硬门禁

`repeat-tool-reminder` 会对同一 Agent 连续使用完全相同的工具和规范化参数计数，默认阈值是 3、5、8。
第一次提醒要求重读结果和换方法，后续提醒列出工具、次数和参数，并要求不要再次原样调用。实现见
[`repeat-tool-reminder/src/index.ts`](../../ref/deepseek-harness/packages/guard/repeat-tool-reminder/src/index.ts)，会话行为可见
[`session.jsonl`](../../ref/deepseek-harness/examples/acp-agent/tests/snapshots/repeat-tool-reminder/session.jsonl)。

但插件明确只是 advisory：它不否决、不改写工具调用。示例中模型在较弱提醒后仍可能继续重复，说明：

- 动态提醒比把同一句话永久塞进总提示更有注意力优势；
- 对必须成立的不变量，提醒仍不能替代宿主拒绝、状态转换或预算门禁。

这是 DeepSeek Harness 最值得吸收、也最不能照搬表面的地方。真正有效的是“事件触发 + 当下状态”，不是
“把警告写得更凶”。

## 其他项目的提示词逻辑

### Claude Code：长手册，但判据具体

Claude Code 的主提示较长，按 Doing Tasks、Action Safety、Using Tools、Tone、Output Efficiency 等主题组织，
见 [`constants/prompts.ts`](../../ref/cloud-code/claude-code-source/src/constants/prompts.ts)。它与 DeepSeek Harness
最大的差异是：许多工作纪律集中由人编写在主提示里，而不是随插件装配。

它写得有效的部分不在于篇幅，而在于判据具体：

- 工具失败后先阅读错误、检查假设、做聚焦修复，不盲目原样重试；
- 中间工具输出以后不再需要、会污染主上下文时，适合 fork；
- 开放式研究或较大实现任务适合委派；
- 主 Agent 不重复做已经委派的工作；
- `Never delegate understanding`：主 Agent 先理解边界，再给子 Agent 具体任务。

Agent 专项提示还区分 fork 与 fresh agent：fork 已继承上下文，任务说明应简短；fresh agent 没有上下文，必须
显式提供背景、已排除假设、范围和输出格式。见
[`AgentTool/prompt.ts`](../../ref/cloud-code/claude-code-source/src/tools/AgentTool/prompt.ts)。

这比“适合独立完成时可以委派”更容易执行，因为模型能从当前任务观察到触发条件。但它仍主要依靠模型自己
识别和遵循，确定性低于宿主计数后切换状态。

### Codex：把模式从人格中剥离

Codex 旧默认提示约 275 行，覆盖执行、计划、工具、验证和最终回答；较新的 GPT-5.2 Codex 基础提示约 80 行，
见 [`gpt-5.2-codex_prompt.md`](../../ref/codex/codex-rs/core/gpt-5.2-codex_prompt.md)。AGENTS 指令、权限、
环境和协作模式由独立消息或模板注入。

Plan Mode 是最典型的状态协议：模式只能被对应的宿主状态改变，用户用命令语气要求实现也只会被解释为制定
实现计划；允许的探索动作、禁止的变更动作和最终输出格式都由模式模板规定。见
[`plan.md`](../../ref/codex/codex-rs/collaboration-mode-templates/templates/plan.md)。

底层原理是把“现在处于什么状态”从自然语言推断改成显式状态机。这样模型不需要同时权衡用户措辞、旧消息
和工具描述来猜当前能否修改文件。

### Grok Build：失败升级由角色状态机承担

Grok Build 普通 Agent 主提示很短，并按实际能力渲染工具名；Subagent 使用单独且更窄的模板。真正强的
闭环在 Goal 子系统：

- Planner 把目标写成有限、可验证、结果导向的验收条件；
- Implementer 被要求 tool-call first，叙述不能冒充动作；
- Verifier 是独立、对抗式角色，用固定 JSON 和 Markdown 契约逐项判断；
- 多轮不收敛时才启动 Strategist，诊断结构根因，并只提出一个结构性调整；
- Verifier 有 anti-ratchet，后续轮不能不断提高既定验收标准。

相关提示位于
[`goal_planner_prompt.md`](../../ref/grok-build/crates/codegen/xai-grok-shell/src/session/templates/goal_planner_prompt.md)、
[`goal_verifier_prompt.md`](../../ref/grok-build/crates/codegen/xai-grok-shell/src/session/templates/goal_verifier_prompt.md)、
[`goal_strategist_prompt.md`](../../ref/grok-build/crates/codegen/xai-grok-shell/src/session/templates/goal_strategist_prompt.md) 和
[`goal_task_discipline.md`](../../ref/grok-build/crates/codegen/xai-grok-shell/src/session/templates/goal_task_discipline.md)。

这是几个项目中最接近“失败后的决策闭环”的实现。关键不是多 Agent 本身，而是宿主根据不收敛事实决定何时
切换到独立验证或策略诊断，原 Implementer 无权无限延长自己的试错路径。

### Paseo：把升级策略包装成显式 Skill

Paseo 不拥有统一 Coding Agent 主提示，它启动并监督现有 CLI，再通过 Skill 教它们编排。`committee` 明确用于
stuck、looping、tunnel vision 或困难规划；`advisor` 用于 second opinion；`handoff` 用于完整交接。见
[`skills.md`](../../ref/paseo/public-docs/skills.md)。

它说明触发语义本身可以成为可安装能力。相比一个泛化的 `delegate` 描述，“卡住时做只读根因分析”和
“需要第二意见但不交出任务”分别有明确名称、边界和输出责任，更容易被模型正确调用。

## 为什么这些写法有效

### 注意力不是均匀的

模型不会以传统程序的方式逐条执行 300 行规则。长期静态出现、与当前错误无关、在多个章节重复的文字容易
退化为背景噪声。刚由工具失败触发、紧邻下一次决策、包含当前错误类别和次数的短提示更容易影响行为。

因此，“不要手算拍数”永久存在并不等于模型在第 17 次工具调用后仍会遵守；在第二次时长推测后注入“本轮
时长只能引用 inspect 结果，下一步必须检查原片段”更接近真实控制点。

### 模型擅长语义判断，不擅长稳定计数和自我升级

“这个任务是否可能由另一个 Agent 完成”是语义判断；“同一错误已经连续修了两次，必须升级”为事件计数。
前者可以交给模型，后者应由宿主记录。让 Composer 自己决定何时承认卡住，会受到沉没成本和自我确认偏差
影响，通常倾向于再试一次。

### 独立验证的价值来自信息边界

Reviewer 的价值不是多一份算力，而是它没有参与生成当前假设，因此较少替原方案辩护。若主 Agent 把失败的
`*6` 改成 `*2` 后再委派，关键状态已经丢失，独立性也无法挽救错误实验。因此委派协议必须保留原始失败输入、
关键参数和期望输出，并禁止未复现时声称排除根因。

### 提示词、schema 和宿主门禁各有职责

| 约束 | 最合适的层 | 原因 |
|---|---|---|
| “回复简洁”“优先最小修改” | 提示词 | 需要语义权衡，允许例外 |
| `candidate` 必须有 Alda 源码 | 工具 schema + 校验 | 结构可机械判断 |
| 一次只能调用一个工具 | 宿主门禁 | 必须稳定成立 |
| 当前是第几次同类失败 | 宿主状态 + 动态 context | 模型不应从长历史自行计数 |
| 测试必须保留关键重复次数 | Reviewer 输出契约 + 宿主元数据 | 一部分是语义等价，一部分可结构化对比 |
| 第三次同类失败必须独立诊断 | 宿主状态转换 | 不能继续依赖失败主体自觉升级 |

## 对 alda-agent 调研前 Composer 的判断

调研开始时 [`protocol.md`](../../alda-agent/prompts/protocol.md) 约 332 行，已经写明：

- 一次响应只调用一个工具；
- 语法事实应查官方文档；
- 时长、Marker 和声部覆盖应真实解析，不要手算；
- 可以按需调用 `delegate`；
- 校验失败后只修硬错误、避免重复相同源码。

[`progressive-composition/SKILL.md`](../../alda-agent/skills/progressive-composition/SKILL.md) 也重复了其中多项纪律。
因此继续向总提示词追加“务必”“严格禁止”收益有限，反而会让 Alda 手册、创作方法、提交协议和失败恢复互相
争夺注意力。

这轮失败揭示的是以下缺失状态：

- 当前硬失败的稳定类别与连续次数；
- 当前唯一主假设及支持/反对证据；
- 最小复现相对原失败候选改变了哪些关键参数；
- 最近是否查过与错误直接对应的官方章节；
- 是否连续重传大候选而没有缩小问题；
- 剩余工具/模型预算处于正常、收尾还是禁止扩展阶段；
- 是否已达到独立诊断的强制升级门槛。

## 建议的 Composer 提示与宿主分工

### 1. 缩短核心提示，按能力拆分

核心协议只保留：角色、结果类型、单工具协议、事实权威、提交门禁和最终责任。其余内容按能力绑定：

- `lookup_alda_docs`：遇到语法事实错误时的查询纪律；
- `inspect_alda_source`：不得手算、fragment/candidate 边界、等价最小复现要求；
- `inspect_alda_patch`：基线、局部替换和哈希要求；
- `delegate`：触发条件、上下文完整性、只诊断输出契约；
- 长篇创作 Skill：音乐发展与 form_plan，不重复通用提交协议；
- Alda 语法手册：改为按需参考，不永久占据核心注意力。

不必复制 DeepSeek Harness 的通用插件框架；alda-agent 工具固定，可以用简单的静态分段拼装达到同样效果。

### 2. 增加按错误类别驱动的失败状态

宿主每次硬失败后生成一个短快照，例如：

```text
<diagnostic_state>
error_class: midi_range
consecutive_failures: 2
current_hypothesis: octave state accumulates across repetition
must_preserve: exact failing phrase, repeat_count=6, starting octave, instrument instance
last_test_changed: repeat_count 6 -> 2
remaining_calls: 9
required_next_action: equivalent minimal reproduction or diagnostic delegation
</diagnostic_state>
```

新快照明确替代旧快照。不要要求模型从 20 轮工具反馈中自行汇总这些事实。

### 3. 用分级升级替代泛化的“必要时委派”

建议的最小状态机：

1. **首次事实性语法失败**：要求查询对应官方章节；若工具结果已经给出明确规则，可直接做最小修复。
2. **同类失败第二次**：冻结主假设；下一步只能使用原失败条件做等价最小复现，或明确证明改变的条件不影响
   根因。禁止同时引入第二个无关假设。
3. **同类失败第三次**：进入 `diagnose_only`，要求调用独立 Reviewer；Composer 不能继续重写完整作品。
4. **Reviewer 给出最小修复后**：Composer 整合并对原失败候选做一次完整验证。
5. **预算进入最后 20%**：停止扩展曲式和大范围重命名，只允许诊断、最小修复、最终验证或如实结束。

第三步可以先做“强提醒 + 只暴露 delegate/inspect/submit 等有限动作”，不一定立即实现复杂多 Agent 调度。

### 4. Reviewer 必须是窄协议

推荐的诊断任务不是“帮我看看哪里有问题”，而应固定为：

```text
只诊断，不重写完整作品。
输入：原始错误、完全相同的失败片段、起始状态、重复次数、相关官方规则、Composer 当前假设。
输出：
1. 是否在不改变关键条件时复现；
2. 单一主假设；
3. 支持和反对证据；
4. 最小修复；
5. 修复为何保持音乐意图；
6. 与原失败候选的等价性检查。
未复现时不得宣称已排除根因。
```

这比预设固定 Worker 或把整首曲子交给另一个 Agent 更符合当前最小架构，也直接针对 `*6` 被改成 `*2` 的
实验失真。

### 5. 限制工具前叙述进入后续上下文

Grok Build 的 `tool-call first` 纪律可直接借鉴到宿主协议：工具前只允许一句动作说明，禁止长篇“让我分析”、
“关键洞察”“我决定”。宿主保存给用户看的进度消息时，可以不把它们全部回灌为下一轮诊断上下文；模型
续轮主要需要：用户目标、当前候选/检查点、最新工具结果、失败状态和未完成动作。

### 6. 让校验失败也产生可修复基线

当前只有通过全部硬检查的 candidate 才成为检查点，导致失败后必须反复重传整份源码。可以保存独立的
`diagnostic_candidate`：

- 明确标记为无效，绝不覆盖 work/current；
- 允许后续 patch 和 inspect 引用；
- 每次失败保存源码哈希、错误类别和工具解析结果；
- 通过后再晋升为普通 candidate 检查点。

这不是放松正确性门禁，而是把失败候选从“不可引用的大段聊天文本”变成“可局部修复的受控状态”。

## 不建议的改法

- 单纯把 24 次调用上限提高到 40 次；这只延长没有升级策略的循环。
- 在总提示词继续追加更多 Alda 语法事实；事实应按错误查文档，并由真实解析验证。
- 每次失败都委派；首次明确错误直接修复更简单，连续不收敛才需要独立诊断。
- 让 Reviewer 自由重写整曲；这会丢失故障定位目标并扩大整合成本。
- 只检测“完全相同的工具参数”；Composer 每次略改大候选仍可能处于同一语义循环，应按错误类别、假设和
  关键状态统计。
- 把动态提醒当硬保证；必须成立的协议继续由宿主机械执行。

## 建议实施顺序

1. 先记录结构化失败状态：错误类别、连续次数、候选哈希、关键参数变化、剩余预算。
2. 注入简短且可替代的 `diagnostic_state`，并减少工具前自述回灌。
3. 为第二次同类失败增加等价最小复现契约。
4. 为第三次同类失败增加 `diagnose_only` Reviewer 升级。
5. 增加无效但可引用的 diagnostic candidate/patch 基线。
6. 最后再拆分和压缩 332 行主协议；用原失败会话做 A/B，比较调用数、文档查询时机、等价测试率、委派率和
   最终有效提交率。

其中 1–4 是“失败后的决策闭环”，比重新润色整份 Persona 更优先。

## 落地状态（2026-08-18）

上述建议已经按最小宿主改造落地：

- 核心协议缩至 72 行，渐进式创作 Skill 缩至 29 行；系统指示不再自动注入 Alda 精简参考，DSL 事实改由
  `lookup_alda_docs` 按需读取，工具局部纪律随各自 schema 提供。
- 宿主按硬失败类别维护可替代的 `diagnostic_state`，记录连续次数、首次与最新源码哈希、关键状态签名、
  当前假设、最近测试变化、文档查询、Reviewer 与剩余预算。
- 首次语法事实失败强制查文档；同类第二次失败强制等价性声明或独立诊断；第三次进入 `diagnose_only`，
  只能调用 Reviewer。Reviewer 同时收到首次与最新失败源码，不能用简化测试覆盖原条件。
- candidate 检查无论成败都形成内存检查点；失败检查点不可提交，但可作为 `diagnostic` 基线被
  `inspect_alda_patch` 修复。初次创作没有 Project 上下文时也支持这条路径。
- 剩余模型调用进入最后 20% 时注入 `wrap_up`；工具前模型文本回灌时只保留第一条非空行、最多 160 个
  Unicode 字符。

因此本轮优化不是单纯改写 Persona，而是把提示词、工具 schema、动态状态和宿主门禁分别放到最适合的层。
这些机制已经有自动化测试；对真实五分钟任务的调用成本、等价复现率和最终音乐质量仍需新的运行样本验证。
