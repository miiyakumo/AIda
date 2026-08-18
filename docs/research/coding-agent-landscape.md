# Coding Agent 市场横向调研与 alda-agent 取舍

## 调研结论

市面 Coding Agent 的"独有功能"可归并为 8 个特性轴：Memory、Goal、Subagent/多 Agent、
Plan mode、Sandbox/审批、Context 管理、Skill/Hooks、Checkpoint/版本。没有哪个 Agent 能凭单一
特性通吃，差异在于各自突出哪些轴。

对照 `alda-agent`（单领域 Composer、宿主固定工具集、受限按需委派）：

- **Memory**：项目状态、持久对话、Skill 已经构成足够的结构化记忆；不需要引入 Cline Memory Bank
  或自动记忆这类"跨会话经验"子系统。当前失败模式不是记忆问题。
- **Goal**：完整候选要求（对话状态持久化）与已落地的 `form_plan` 持久化就是本领域的 Goal；
  不需要抽象成 DeepSeek Harness 那种事件溯源 goal 服务。
- **Subagent/多 Agent**：第二次同题运行暴露了作曲设计、拍数核算、DSL 排错和协议恢复的职责耦合。当前已
  落地单个 `delegate(task, context?)` 原语，允许 Composer 按需调用具备最小只读工具集的 subagent；价值仍需长篇 A/B
  验证，不建设角色、调度或通用 workflow 平台。
- 其余特性按真实需求条件触发，见下表。

最贴合当前架构、又有市场共识的下一步是**并行只读工具调用**（todo 已有方案）与
**Invocation 快照/可观测性**（deepseek-harness 调研已有建议）。

## 调研基准

- 调研日期：2026-08-18
- 来源：市场认知 + 仓库内 `ref/` 代码（codex、grok-build、deepseek-harness、paseo）+
  官方文档抓取（Aider FAQ、Goose docs、OpenHands docs、Codex README）。
- 局限：本轮 WebSearch 工具不可用，Claude Code / Cursor / Amp / Cline / Devin 等未逐条抓到
  实时文档，描述来自既有知识；方向性结论不受影响，精确核验某家时可再单独抓取。
- 对提示词拼装、运行时状态和失败升级的机制细节以
  [Coding Agent 提示词的组织方式与控制原理](coding-agent-prompt-design.md) 为准。

## 市场概览

| Agent | 形态 | 标志性/独有功能 |
|---|---|---|
| Claude Code | 终端 / IDE | 子代理（Task）、Plan 模式、hooks 事件自动化、Skill 渐进披露、CLAUDE.md + 自动记忆、细粒度权限 |
| OpenAI Codex（CLI/Web） | 终端 / 云端 | 沙箱 + 审批模式、云端 sessions、并行 agent、MCP、AGENTS.md |
| Aider | 终端 | git 原生提交、repository map 紧凑代码库地图、多编辑格式、provider 极广 |
| Amp（Sourcegraph） | 终端 | deep context（并行扇出 agent 填满上下文）、spec→plan 流程、审批 |
| Goose（Block） | 终端 / 桌面 / API | MCP 扩展、Recipes（YAML 工作流）、子代理、提示注入检测、沙箱模式 |
| Cline / Roo Code | IDE 扩展 | git checkpoint 回滚、plan/act 模式、Memory Bank、角色模式 + 子任务 |
| OpenHands | Web / 云 | Docker 沙箱、delegates + microagents、任务状态跟踪、GitHub 集成 |
| Cursor / Windsurf | IDE | 代码库索引、后台并行 agent、rules、checkpoint、@-提及上下文 |
| Gemini CLI / Jules | 终端 / 异步云 | 子代理 + skills；Jules 异步 VM 任务 |
| Devin | 云 IDE | 长期自治规划的 goal/plan、浏览器/终端/Slack |
| Grok Build | 终端 | 前后台任务、MCP、沙箱、checkpoint、skill（`ref/grok-build`） |
| DeepSeek Harness | 平台 | 微内核插件树、goal（事件溯源）、subagent + workflow 编排、compaction、审批栈（`ref/deepseek-harness`） |
| Paseo | 语音控制层 | 语音驱动其他 agent，本质是 MCP 编排 + routing（`ref/paseo`） |

## 特性轴拆解

| 轴 | 市场代表 | 对应 alda-agent 现状 |
|---|---|---|
| Memory（跨会话经验） | Cline Memory Bank、Claude Code 自动记忆 | 项目状态 + 持久对话 + Skill；缺自动经验记忆，不需要 |
| Goal（可跨轮/重启的目标） | dsh goal、Devin plan | 候选要求持久化 + form_plan；缺通用抽象，不需要 |
| Subagent/多 Agent | Claude Code Task、Amp、Cursor、Goose、dsh workflow | 已有隔离的按需委派和最小只读工具集；无固定角色、递归委派或 workflow 编排 |
| Plan mode（执行前批准门） | Claude Code、Amp、Cursor | `submit_result(plan)` 已有计划产物；当前一轮完成，无需打断 |
| Sandbox/审批 | Codex、Goose、OpenHands、dsh | 宿主固定持有校验与文件写入，无需下游沙箱 |
| Context 管理（compaction） | Claude Code /compact、dsh、OpenHands | 无；对话已按需裁剪，先做可观测性再压缩 |
| Skill/Hooks（方法注入） | Claude Code、dsh、Cursor | 已对齐，是全项目最贴近市场的地方 |
| Checkpoint/版本 | Cline/Roo、Cursor、Grok Build | 已超配：版本不可变 + 候选检查点 + 哈希校验 |

## 三问判定

### Memory：已覆盖大部分，不引入通用记忆子系统

已有能力即成熟 memory：

- `Project` 聚合根持久化偏好、工作乐谱、版本、待修正候选，比多数 agent 的 memory 更结构化；
- 持久 Conversation 跨重启恢复；
- Skill（builtin + advisory）是"方法记忆"；
- 模型配置与 fingerprint 有快照。

不引入的理由：

- 长篇诊断的失败模式是缺 Marker 时间证据、总时长压倒结构、整体重写多于局部修改——这是
  反馈与表示问题，不是记忆问题；
- 一次创作的上下文由 `work.alda` + form_plan + 持久对话承载，不漂移；
- Memory Bank 式"固定 markdown + 每会话读写"会新增事实来源，违反"Project 是唯一事实来源"
  的不变量。

唯一可能值得记的跨会话经验是"项目既定方向/审美约束"，但先让 form_plan 落地，看真实缺口再说。

### Goal：已有领域化版本，不抽象成通用 goal 服务

- "完整候选要求"作为对话状态持久化，跨澄清、模型失败与重启保留——这就是一个领域 goal；
- 已落地的 `form_plan` 持久化（随工作稿、恢复候选、版本保存）本质是"本作品结构性目标"；
- dsh 的 goal 解决多工具、多轮、可随时改向的通用场景；当前领域只有一条主导创作流程，
  ConversationState 已覆盖 goal 的转向/中断/恢复语义。
- 事件溯源 goal 域在 Rust 单 crate 里是纯负担；把 form_plan 与候选要求做实即等价于领域目标。

### Subagent：最小能力已落地，生产价值待验证

第一阶段的因果判断仍成立一半：multi-agent 不能替代 Marker、form_plan、静态校验或人工试听，也不能单独
解决反馈不对称。Marker/form_plan 落地后的第二次运行则补充了新的证据：同一 Agent 同时设计动机、和声、
曲式和织体，又手算拍数、修 Alda 语法、维护 Marker 和提交字段，最终用了 20 次模型调用、19 次工具往返、
3 次协议恢复，并留下跨段溢出和游标回跳。

这足以验证职责隔离是否有效，但不足以采用角色或工作流平台。当前最小实现只是 Composer 可按需调用
`delegate(task, context?)`；subagent 不继承主对话或源码，只能查询文档、检查片段和只读检查项目乐谱，最终
组装、完整检查和提交仍由 Composer 与现有宿主完成。若调用成本、DSL 正确性或完整试听没有可测改善，调整提示或撤回入口。具体边界见
[Subagent 委派 A/B 验收](../todo/workflow-artifacts-and-agent-roles.md)。

## 其余特性判定

| 特性 | 判定 | 说明 |
|---|---|---|
| 并行只读工具调用 | **优先做** | 工具多数宿主固定且无副作用，同一快照上的 `inspect_*` 可安全并行；[selective-parallel-tool-calls](../todo/selective-parallel-tool-calls.md) 已是正确方向 |
| Invocation 快照/可复现 | 条件触发 | 需要审计或可复现时保存指令 fingerprint、编译后指示、动态上下文、工具 schema；见 [Coding Agent 提示词的组织方式与控制原理](coding-agent-prompt-design.md) 建议，优先级高于 subagent |
| Context 管理/compaction | 暂缓 | 先记录请求大小与趋势，真实会话达到窗口或成本阈值后再压缩；不要预先引入完整 surface 代数 |
| Plan mode 门 | 已有等效物 | 明确创作意图在同一轮完成；只有需要"不确认不进下一步"时才补门 |
| MCP/通用扩展 | 不需要 | 工具集固定且领域化，无接入外部工具生态的需求 |
| Sandbox/审批栈 | 不需要 | 不向模型开放 shell/fs，能力边界宿主固定持有，正是优于"沙箱绕行"的点 |

## 建议优先级

1. 补齐声部游标回跳、跨段溢出和意外重叠的确定性检查。
2. 用不委派运行作为基线，执行长篇作曲按需委派 A/B。
3. 并行只读工具调用与 Invocation 可观测性按现有 todo 推进。
4. 暂不引入 memory 子系统、通用 goal、通用 subagent/workflow 平台。

## 关联文档

- [Coding Agent 提示词的组织方式与控制原理](coding-agent-prompt-design.md)：DeepSeek Harness 为主的提示词拼装、运行时状态与失败闭环调研。
- [Coding Agent 的工作区与会话关系](agent-workspace-sessions.md)：会话初始化与工作区关联。
- [Coding Agent 的终端信息分层](coding-agent-terminal-information-layout.md)：终端信息结构。
- [Subagent 委派 A/B 验收](../todo/workflow-artifacts-and-agent-roles.md)：按需委派的验收与撤回条件。
- [长篇作曲质量与可控修改](../todo/long-form-composition-quality.md)：本领域 Goal 与结构化产物方案。
- [分级开放工具并行调用](../todo/selective-parallel-tool-calls.md)：并行只读工具方案。
