# Coding Agent 市场横向调研与 alda-agent 取舍

## 调研结论

市面 Coding Agent 的"独有功能"可归并为 8 个特性轴：Memory、Goal、Subagent/多 Agent、
Plan mode、Sandbox/审批、Context 管理、Skill/Hooks、Checkpoint/版本。没有哪个 Agent 能凭单一
特性通吃，差异在于各自突出哪些轴。

对照 `alda-agent`（单领域、单 Agent、宿主固定工具集）：

- **Memory**：项目状态、持久对话、Skill 已经构成足够的结构化记忆；不需要引入 Cline Memory Bank
  或自动记忆这类"跨会话经验"子系统。当前失败模式不是记忆问题。
- **Goal**：完整候选要求（对话状态持久化）与即将落地的 `form_plan` 持久化就是本领域的 Goal；
  不需要抽象成 DeepSeek Harness 那种事件溯源 goal 服务。
- **Subagent/多 Agent**：当前明确不引入。项目已有三处一致结论（见关联文档），且长篇诊断的根因
  是反馈不对称与缺统一时间表示，multi-agent 解决上下文隔离与并行，不解决反馈问题。
- 其余特性按真实需求条件触发，见下表。

最贴合当前架构、又有市场共识的下一步是**并行只读工具调用**（todo 已有方案）与
**Invocation 快照/可观测性**（deepseek-harness 调研已有建议）。

## 调研基准

- 调研日期：2026-08-18
- 来源：市场认知 + 仓库内 `ref/` 代码（codex、grok-build、deepseek-harness、paseo）+
  官方文档抓取（Aider FAQ、Goose docs、OpenHands docs、Codex README）。
- 局限：本轮 WebSearch 工具不可用，Claude Code / Cursor / Amp / Cline / Devin 等未逐条抓到
  实时文档，描述来自既有知识；方向性结论不受影响，精确核验某家时可再单独抓取。
- 对 `deepseek-harness` 的机制细节以 [deepseek-harness.md](deepseek-harness.md) 为准。

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
| Subagent/多 Agent | Claude Code Task、Amp、Cursor、Goose、dsh workflow | 无；当前不引入，见证据门槛 |
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
- 即将落地的 `form_plan` 持久化（随工作稿、恢复候选、版本保存）本质是"本作品结构性目标"；
- dsh 的 goal 解决多工具、多轮、可随时改向的通用场景；当前领域只有一条主导创作流程，
  ConversationState 已覆盖 goal 的转向/中断/恢复语义。
- 事件溯源 goal 域在 Rust 单 crate 里是纯负担；把 form_plan 与候选要求做实即等价于领域目标。

### Subagent：三个中最不该现在做的，项目已达成共识

三处现有结论：

- [长篇作曲质量与可控修改](../todo/long-form-composition-quality.md)：明确不因长篇任务引入
  Director、Composer、Critic 等多 Agent；
- [Workflow 产物与 Agent 角色派生](../todo/workflow-artifacts-and-agent-roles.md)：Role/fork 需要
  "对照实验表明拆分角色能改善质量、成本或上下文隔离"的证据门槛；
- [deepseek-harness.md](deepseek-harness.md)：不为了"可能有用"增加 Subagent 或 Workflow。

因果判断：长篇诊断的根因是反馈不对称与缺统一时间表示，不是"单个 Agent 上下文不够"。
multi-agent 解决上下文隔离与并行，不解决反馈问题，还会引入编排成本。等 form_plan + Marker +
局部保持闭环稳定、出现"重复发生且单 Agent 完不成"的真实任务后，再按上文的证据门槛重评。

## 其余特性判定

| 特性 | 判定 | 说明 |
|---|---|---|
| 并行只读工具调用 | **优先做** | 工具多数宿主固定且无副作用，同一快照上的 `inspect_*` 可安全并行；[selective-parallel-tool-calls](../todo/selective-parallel-tool-calls.md) 已是正确方向 |
| Invocation 快照/可复现 | 条件触发 | 需要审计或可复现时保存指令 fingerprint、编译后指示、动态上下文、工具 schema；见 [deepseek-harness.md](deepseek-harness.md) 建议，优先级高于 subagent |
| Context 管理/compaction | 暂缓 | 先记录请求大小与趋势，真实会话达到窗口或成本阈值后再压缩；不要预先引入完整 surface 代数 |
| Plan mode 门 | 已有等效物 | 明确创作意图在同一轮完成；只有需要"不确认不进下一步"时才补门 |
| MCP/通用扩展 | 不需要 | 工具集固定且领域化，无接入外部工具生态的需求 |
| Sandbox/审批栈 | 不需要 | 不向模型开放 shell/fs，能力边界宿主固定持有，正是优于"沙箱绕行"的点 |

## 建议优先级

1. 并行只读工具调用（todo 已有方案，直接落地）。
2. Invocation 可观测性（先记录请求大小与来源，为压缩与审计准备）。
3. 按 [long-form-composition-quality.md](../todo/long-form-composition-quality.md) 把 form_plan 落地
   （即本领域的 Goal 与记忆主体）。
4. 暂不引入 memory 子系统、通用 goal、subagent 或 workflow。

## 关联文档

- [DeepSeek Harness 架构与机制](deepseek-harness.md)：subagent/goal/compaction 的机制与取舍。
- [Coding Agent 的工作区与会话关系](agent-workspace-sessions.md)：会话初始化与工作区关联。
- [Coding Agent 的终端信息分层](coding-agent-terminal-information-layout.md)：终端信息结构。
- [Workflow 产物与 Agent 角色派生](../todo/workflow-artifacts-and-agent-roles.md)：Role/fork 证据门槛。
- [长篇作曲质量与可控修改](../todo/long-form-composition-quality.md)：本领域 Goal 与结构化产物方案。
- [分级开放工具并行调用](../todo/selective-parallel-tool-calls.md)：并行只读工具方案。