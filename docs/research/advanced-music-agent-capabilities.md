# MewCode、Codex 与 Alda 音乐 Agent 进阶能力调研

> 调研日期：2026-07-29  
> 本地 Codex 快照：`61a44880a85d2fd0d8770908dea5733495e571c8`  
> 本地 Alda 快照：2.4.3，`33e17e5674fd98da89462f21b2e0e5f2d9f16944`

## 1. 调研目标与证据边界

本报告回答四个问题：

1. MewCode 宣传材料中的 Coding Agent 能力，哪些可以直接迁移？
2. 哪些能力只能保留设计意图，不能照搬实现？
3. Codex 还有哪些 MewCode 材料没有明确体现的优秀设计？
4. 作曲与编程的差异会催生哪些音乐原生能力？

证据分为三级：

- **高置信事实**：仓库内源码、已校验调研文档或 OpenAI 官方 Codex 文档。
- **宣传材料自述**：用户提供的 MewCode 文案与截图；未审查其源码，不判断完成度。
- **设计提案**：本报告提出的音乐领域方案，必须在路线中通过测试或用户实验验证。

当前仓库只有 `docs/` 与 `ref/`，没有 `alda-agent/` 实现。下文的“已覆盖”只表示已有调研或设计，不表示功能已完成。

## 2. 执行摘要

基础 Alda Harness 已经深入覆盖模型通信、Agent Loop、工具、System Prompt、JSONL Session、压缩和 H0–H3 评测，但它主动采用“单用户、单 Session、无 Sandbox、无扩展”假设。进阶目标会使这些假设部分失效：

- `write_score`、导出、播放、MCP 和 DAW 都有副作用，权限不能继续整体砍掉；
- 长期记忆不是 resume，必须处理范围、来源、置信度和删除；
- 多 Agent 不能共享当前 `&mut Session`，必须使用隔离版本与消息传递；
- Git Worktree 不是音乐家的心智模型，应升级为 Take/Variation 版本图；
- `parse` 通过只是事实证据，人耳反馈才是审美证据；
- 作品、渲染、试听、反馈和来源必须分别建模。

推荐将系统从 5 层升级为 7 层：交互意图、创作编排、音乐领域、能力扩展、状态知识、信任资源、质量运维。

## 3. MewCode 课程的音乐化映射

截图编号是第零章到第十七章，即“第零章导读 + 17 个正文章节”。本项目采用这一口径，避免把 18 个编号页面误写成 17 个。

| MewCode 主题 | Alda 音乐版 | M0–M5 基线状态 | 进阶验收 |
|---|---|---|---|
| 导读 | 为什么不是让模型吐一段 Alda | 已有基础教程 | 能解释模型、Harness、Alda 与人类听审边界 |
| 初识 Agent | Music Composition Agent 生命周期 | 部分覆盖 | 用户需求能编译为 `CreativeBrief` |
| 模型通信 | 双 Provider、SSE、结构化工具事件 | 设计较完整 | 录制 fixture 覆盖 delta、usage、异常 EOF、取消 |
| 工具系统 | 乐谱、分析、试听、互操作工具 | 已设计 4 个 | 扩为语义编辑、导入导出、比较和询问用户 |
| 自主循环 | 修错循环 + 发散/收敛循环 | 只有通用 ReAct | 同预算生成差异候选，再选择和局部修订 |
| System Prompt | 角色、Brief、事实/审美边界 | 较完整 | Prompt 回归防止“假装听见”和规则绝对化 |
| 权限 | 工作区、播放、设备、网络、发布权限 | 只有固定白名单与局部约束，无通用权限框架 | 路径穿越、覆盖、未知 MCP、外设写入受控 |
| MCP | Music MCP Gateway | 只有 Codex 调研 | 本地只读 MCP 通过命名、schema、超时与审批测试 |
| 上下文 | 对话 + Brief + Score + Evidence | Session/压缩较完整 | 30 轮后硬约束、动机、未决问题不丢 |
| 记忆 | 偏好、项目、情节、程序与来源记忆 | 未实现 | 可查看、纠正、删除；一次意见不误升全局 |
| Slash | `/take`、`/diff`、`/audition` 等宿主命令 | 只有少量固定命令 | 帮助、补全、参数校验、脚本模式 |
| Skill | 对位、配器、爵士和声等技能包 | 仅概念 | 安装、发现、冲突、版本、卸载、回归测试 |
| Hook | 创作生命周期 Hook | 有 Codex 插入点调研 | 顺序稳定、超时隔离、循环检测、信任确认 |
| SubAgent | 作曲、编曲、乐评等隔离角色 | 早期文档只列可选串行构想，未实现 | 独立 Session、有限上下文、可取消、带来源 |
| Worktree | Take/Variation Workspace | 无 | 隔离候选、A/B、promote、段落/声部语义 diff |
| Agent Teams | Music Production Team | 无 | 任务依赖、预算、盲评、重复劳动与冲突检测 |
| 回顾 | 架构、评测、可靠性、失败库 | H0–H3 有优势 | 同配置可复跑并区分事实与主观结论 |
| 求职 | ADR、故障复盘、演示和证据化面试 | 无 | 每个亮点都有代码、测试、指标或 ADR 证据 |

## 4. 不应机械照搬的 Coding Agent 设计

### 4.1 六个文件工具不是音乐工具体系

`ReadFile/WriteFile/EditFile/Bash/Glob/Grep` 的设计意图是让 Agent 操作代码仓库。音乐 Agent 应暴露更高语义工具：

- 读取作品结构与指定片段；
- 修改小节、段落、声部或动机；
- 转调、重新和声、调整配器与动态；
- 生成并比较 Take；
- 按 marker 或时间范围试听；
- 导入 MusicXML/MIDI，导出 MIDI/音频；
- 请求人类听感反馈。

通用 Bash 可以作为专家模式 escape hatch，但不应默认暴露。

### 4.2 Worktree 只是隔离手段

音乐创作者关心“版本 A 的旋律”和“版本 B 的伴奏”，不关心 checkout 路径。Git Worktree 可作为底层实现，但上层应是不可变 `ScoreRevision`、`CandidateSet`、`MusicPatch` 和 A/B 试听。

### 4.3 多 Agent 不是质量保证

并行 Agent 会增加 token、延迟、相关错误、协调失败和审美平均化。必须比较：

- 同总预算单 Agent；
- 多候选单 Agent；
- 多 Agent 候选；
- 是否使用盲评 Critic。

只有盲听、接受率或修改轮数显示稳定收益时，才默认开启。

### 4.4 代码测试与音乐质量不是同一种终止条件

编程可以用编译和测试建立强终止信号；音乐需要两条循环：

```text
硬约束循环：parse → 结构/音域/时长检查 → repair
软偏好循环：候选 → 分析 → 渲染 → 盲听 → select/refine
```

## 5. Codex 值得额外借鉴的设计

以下能力在用户提供的 MewCode 材料中没有明确展开，或 Codex 的设计粒度更细。

### 5.1 沙箱与审批正交

Codex 将“技术上能做什么”的 Sandbox 与“何时必须询问”的 Approval 分开。网络默认关闭，越界操作需要审批。音乐版也应分开：工作区写入可能自动允许，但播放外设、控制 DAW、联网和发布需要不同策略。

官方依据：[Agent approvals & security](https://learn.chatgpt.com/docs/agent-approvals-security)。本地源码还将审批动作结构化，并在沙箱拒绝后重新审批，而不是无条件提权。

### 5.2 工具渐进披露

Codex 的工具曝光包含 `Direct`、`Deferred`、`DirectModelOnly` 和 `Hidden`。工具多时先给模型核心工具，再通过搜索加载 MCP、导出、版权或 DAW 工具，避免 schema 挤占上下文。当前只有 4 个工具时不必实现搜索，但接口应预留曝光元数据。

### 5.3 Skill 与 Plugin 分工

Skill 是按需加载的任务工作流；Plugin 是可安装、可分发的组合单元，可以打包 Skills、MCP、Hooks、UI 和资产。音乐版可先做本地 Skill，再在生态成熟后引入 Plugin。

官方依据：[Build skills](https://learn.chatgpt.com/docs/build-skills)、[Build plugins](https://learn.chatgpt.com/docs/build-plugins)。

### 5.4 Hook 不止工具前后

Codex Hook 覆盖 Session、Tool、Permission、Compact、Subagent 和 Stop，并通过内容哈希确认非托管 Hook 的信任。音乐版需要 `PreAudition`、`PreExport`、`PrePublish` 等领域 Hook，但真正的安全检查必须在副作用发生前。

官方依据：[Hooks](https://learn.chatgpt.com/docs/hooks)。

### 5.5 SubAgent 是独立线程，不是函数调用

Codex SubAgent 可 fork 有限上下文、被 steering、等待、中断和复用，并将摘要而非噪声返回主线程。音乐版候选 Agent 也应独立持有 Take，不能共享一个全局可变 Session。

官方依据：[Subagents](https://learn.chatgpt.com/docs/agent-configuration/subagents)。

### 5.6 指导文件按目录层叠

Codex 合并全局、仓库根和近层 `AGENTS.md`。音乐工程可采用根 `MUSIC.md`，并允许乐章或段落目录中的近层规则覆盖远层规则；`/status` 必须展示最终来源。

官方依据：[Custom instructions with AGENTS.md](https://learn.chatgpt.com/docs/agent-configuration/agents-md)。

### 5.7 Rollout 与语义 Trace 分层

JSONL 是权威事件记录；语义 trace 可以由确定性 reducer 从原始事件归约得到。音乐版应记录事实事件，再生成“Brief → Take → Parse → Render → Audition → Feedback”的时间线，而不是把自然语言总结当唯一历史。

### 5.8 记忆是后台提取与合并管线

Codex 本地记忆区分是否使用与是否生成，支持闲置过滤、秘密脱敏、外部上下文过滤和全局合并。音乐偏好更加上下文相关，因此每条记忆还要有 scope、evidence、confidence 和 decay。

官方依据：[Memories](https://learn.chatgpt.com/docs/customization/memories)。

### 5.9 模式是不同工作面

Plan、Review、Fork、Side chat 不应都被塞进一个 Prompt。音乐版至少分为：

- Plan：只澄清 Brief、约束和创作方案；
- Compose：生成或修改 Take；
- Review：只读分析，不直接写 canonical；
- Audition：播放、比较和采集反馈；
- Batch：无人值守生成/评测，不弹交互审批。

### 5.10 可观测性、重放和定时任务

Codex 将 OTel、原始 Prompt 记录和常规遥测分开 opt-in；Record & Replay 也区分产品工作流录制与工程事件重放。音乐版可增加定时回归评测、批量渲染和作品健康检查，但无人值守任务必须使用最小权限。

官方依据：[Record & Replay](https://learn.chatgpt.com/docs/extend/record-and-replay)、[Scheduled tasks](https://learn.chatgpt.com/docs/automations)。

## 6. 作曲不同于编程的领域事实

### 6.1 Alda Score 是执行快照，不是完整作品模型

Alda `Score` 适合表示 Parts、事件、Marker 和播放状态，但不表达创作意图、段落身份、动机谱系、候选关系、听感或来源。`Score JSON` 应作为 `ParsedScoreSnapshot` 证据，而不是顶层业务聚合。

### 6.2 Alda Part ID 不是跨版本稳定身份

`part001` 等 ID 由创建顺序产生。插入乐器会改变编号，因此跨版本比较需要稳定的领域 `PartId` 和 Alda alias 映射。

### 6.3 小节线没有执行语义

Alda barline 本身不改变播放状态。进阶编辑必须显式维护 `MeterMap`、`BeatGrid`、`SectionMap`，不能把源码 `|` 当成经过验证的小节边界。

### 6.4 人耳反馈必须绑定确切试听

“副歌太挤”必须记录：哪个 Revision、哪个 Render、哪个片段、什么播放顺序与设备。否则长期记忆会把局部意见错误泛化。

### 6.5 音乐规则具有风格条件

协和度、平行五度、重复度和节奏多样性不是跨风格统一真理。规则必须携带适用风格、范围、严重性与解释。`Unknown` 不能按 `Pass` 处理。

### 6.6 Alda 原生 MIDI 导出不等于音频 Render

Alda 2.4.3 原生导出 MIDI，不直接产出可复用 WAV。现场 `play` 依赖 SoundFont、合成器与设备。进阶系统若要 Audio Artifact、响度归一化或可重复 Audible Diff，必须另选离线 synth/录音适配器并记录完整 Render manifest；MIDI/MusicXML import 和音频 export 也应明确标为 Agent 适配器，而非 Alda 原生能力。

## 7. 音乐原生创新能力

| 创新 | 核心观点 | 最小验证 |
|---|---|---|
| CreativeBrief Compiler | 将自然语言拆为意图、硬约束、软偏好和未知项 | 约束抽取的用户纠正率 |
| Canonical Score IR Lite | 稳定 Part/Section/Beat 身份与 source span | 插入声部后跨版本映射仍正确 |
| Musical Semantic Patch | 用段落、声部、动机表达修改，不只做文本 diff | 非目标区域 hash 保持 |
| Audible Diff | 对 patch 自动生成 before/after、loop 和变化摘要 | 用户能定位听感变化 |
| Take/Variation DAG | 候选不可变、可 fork、promote 和追溯 | A/B 互不覆盖且 parent 完整 |
| Hard Constraint Proof | 硬约束输出机器可读证据 | 未通过不能进入 Acceptable |
| Creative Beam Search | 显式分配发散、修订与试听预算 | 候选有差异且过最低质量门 |
| Render-bound Feedback | 反馈绑定 Revision、Render、Range | 反馈链完整率 |
| Blind A/B Protocol | 隐藏作者/模型，随机和平衡播放顺序 | 顺序偏差下降 |
| Motif Lineage Graph | 记录动机的移调、逆行、增减值与来源 | 变形可追溯 |
| PerformerProfile | 音域之外建模手型、换气、速度与演奏水平 | 专家标注集可演奏性准确率 |
| Evidence-based Memory | 偏好有 scope、证据、置信度和衰减 | 单次局部意见不升全局 |
| Pareto Candidate Selection | 不把张力、简洁、可演奏性压成单一总分 | 用户从 Pareto 候选的选择率 |
| Music Evaluation Card | 分开事实、启发式、模型判断和人耳意见 | 每条结论可追溯 |
| Temporal Resource Locks | 音频设备、DAW transport、Player 是独占资源 | 并发试听不冲突 |
| Provenance Ledger | 记录素材、模型、Skill、工具和人工修改 | 发布前来源链完整 |
| Structure-aware Compaction | 保留 Brief、Constraint、Motif、Take 与反馈引用 | 30 轮后结构事实保真 |
| Live Annotation Loop | 播放时把“这里”转换为精确时间/乐段地址 | 时间定位误差 |

## 8. 争议问题与本报告裁决

### 8.1 是否首期实现完整 Canonical IR

**裁决：不实现完整双向 IR，先做 IR Lite。**

首期只在显式 alias、marker、用户确认的 MeterMap/SectionMap 或带来源元数据上保证稳定地址，并保存 source hash 和 Alda 临时 ID 映射。缺少依据时必须返回 `NeedsIdentityMapping`、`NeedsSectionMapping` 或 `UnknownMeter`，降级到 WholeScore/MarkerRange。自动语义 merge 与完整 MusicXML round-trip 延后。

### 8.2 是否立即采用完整 DDD

**裁决：持久化边界使用领域类型，进程内小对象保持简单。**

`CompositionProject`、`CreativeBrief`、`ScoreRevision`、`Audition` 和 `PreferenceMemory` 需要独立身份；音高、拍位、范围等使用值对象。不要为每个临时分析结果建立 Repository。

### 8.3 是否立即做完整 OS Sandbox

**裁决：legacy MVP 使用能力白名单和受控路径；执行非托管命令、stdio MCP、DAW 或网络扩展前必须建立受限 Extension Host / OS Sandbox。**

接口从第一天使用 `resolve → ActionPlan → Permission → AuthorizedPlan → execute(StagingCapabilities)`，确保审批发生在副作用之前。内容 hash、allowlist 和远端 Effect 声明不能替代真实文件、环境、网络与设备隔离。

### 8.4 是否让音频模型代替人耳

**裁决：不能。**

符号核心是确定性基线；可选 Audio Critic 只能提供带模型版本和置信度的辅助证据。用户审美和最终接受权不可委托。

### 8.5 是否默认启用多 Agent

**裁决：否。**

先建立同预算单 Agent 和多候选基线。首个多 Agent 实验只做两个隔离候选 + 一个盲评 Critic，最大并发 3，Integrator 才能提交 canonical。

### 8.6 是否自动语义合并

**裁决：首期只检测冲突并由人类/Integrator 选择。**

只有 `ReplaceSection`、`Transpose`、`ChangeInstrumentation` 等最小 Patch 集合稳定后，才尝试三方语义 merge。

### 8.7 是否承诺实时伴奏

**裁决：不承诺。**

当前 Alda 更适合交互试听、片段重放和播放中打时间标签。专业实时伴奏需要独立时钟、低延迟 MIDI 和缓冲架构，应作为研究项目。

## 9. 推荐能力依赖图

```text
CreativeBrief + ConstraintSet
          ↓
ScoreRevision + Artifact Store + IR Lite
          ↓
Permission + Hook + Semantic Tool Contract
          ↓
Take Branch + Audible Diff + Audition
          ↓
Skill + Memory + MCP
          ↓
Isolated SubAgent + Blind Critic + Integrator
          ↓
Agent Teams + Semantic Merge + Batch Automation
```

权限必须先于 MCP 和外部设备；版本与 Artifact 必须先于记忆和多 Agent；评测从第一阶段开始积累，不能最后补。

## 10. 风险与反例

| 风险 | 反例 | 缓解 |
|---|---|---|
| 指标刷分 | 为提高协和度而消除必要张力 | 风格条件化、多指标、保留人耳 Gate |
| 偏好误泛化 | “这次不要鼓”变成全局讨厌鼓 | scope、evidence、confidence、用户确认 |
| 多 Agent 模式坍缩 | 三个候选只是轻微换音 | DiversityIntent 与距离下限 |
| Critic 锚定 | 看到作者解释后附和 | 隐藏作者与理由，只给 Brief 和 Artifact |
| 文本合并成功但音乐冲突 | 两边都改了同一声部副歌 | MusicAddress 与语义冲突检测 |
| 试听串版本 | 用户评论的不是当前源码 | Audition 绑定 hash、Render、范围 |
| MCP 越权 | 只读声明实际触发 DAW 写操作 | 本地 EffectClass，不信任远端 annotation |
| 版权护栏误报 | 常见和弦被判高度相似 | 只做风险提示，保存来源，人工发布审查 |
| 实时功能夸大 | API 延迟无法跟随演奏 | 区分时间标注、快速重放、实时伴奏 |
| 文档状态误导 | 设计代码被称为已实现源码 | 总入口状态表与逐里程碑证据 |

## 11. 参考资料

### 本地材料

- `docs/design/harness-design.md`
- `docs/design/implementation-roadmap.md`
- `docs/research/codex-tools.md`
- `docs/research/codex-agent-loop.md`
- `docs/research/codex-session-state.md`
- `docs/research/alda-interfaces.md`
- `docs/research/alda-pipeline.md`
- `docs/research/music-theory.md`
- `ref/codex/codex-rs/`
- `ref/alda/`

### Codex 官方材料

- [Subagents](https://learn.chatgpt.com/docs/agent-configuration/subagents)
- [Agent approvals & security](https://learn.chatgpt.com/docs/agent-approvals-security)
- [Hooks](https://learn.chatgpt.com/docs/hooks)
- [Memories](https://learn.chatgpt.com/docs/customization/memories)
- [Worktrees](https://learn.chatgpt.com/docs/environments/git-worktrees)
- [Custom instructions with AGENTS.md](https://learn.chatgpt.com/docs/agent-configuration/agents-md)
- [Scheduled tasks](https://learn.chatgpt.com/docs/automations)

## 12. 置信度总结

| 结论 | 置信度 |
|---|---|
| 当前文档覆盖度和缺口 | 高 |
| Codex 机制描述 | 高 |
| CreativeBrief、Revision、Audition 是必要领域边界 | 高 |
| IR Lite 能支撑首期语义编辑 | 中高 |
| 盲式 A/B 能降低部分评价偏差 | 中 |
| 多 Agent 能提升最终音乐质量 | 中低，需消融实验 |
| 音频模型能代替用户审美 | 低，不成立 |
| 相似度工具能判断法律安全 | 低，只能提示风险 |
