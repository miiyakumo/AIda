# DeepSeek Harness 提示词编写风格

## 结论

DeepSeek Harness 的提示词不像一篇统一的角色说明，更像一组与运行时能力共同装配的操作契约。它把身份、工具选择、模式、动态策略、Skill、工作区指示和辅助模型任务拆到各自的所有者中，再按顺序和作用域组合。

这种写法的核心不是频繁使用 `Do not`，而是以下四点：

1. 每段只解决一个边界问题，并由最了解该边界的插件维护。
2. 规则同时说明触发条件、错误动作、正确替代动作和可观察后果。
3. 提示词声明的限制尽量由 schema、dispatch、状态机或校验器同步执行。
4. 稳定规则、动态事实和不可信输入使用不同的消息位置与结构边界。

对 `alda-agent` 最值得借鉴的是规则模板和提示词—执行机制同源，而不是照搬 Harness 的插件数量或英文命令式语气。当前 Alda 提示词需要保留领域语法参考，但可以压缩核心协议、减少重复强调、修正冲突，并让每条失败规则紧邻补救动作。

## 调研范围

本文沿用[架构调研](deepseek-harness.md)的版本基准：DeepSeek Harness `0.1.0-rc.5`，commit `47f943859bef60e4160492346772ded9b24f765a`，调研日期为 2026-08-16。

重点检查了以下模型可见文本：

- 标准 persona 与 plan mode：[`agent.cordis.yml`](../../ref/deepseek-harness/apps/cli/config/agent-presets/standard/agent.cordis.yml)
- 文件、搜索、shell 和后台任务工具指导：[`tool-fs`](../../ref/deepseek-harness/packages/fs/tool-fs/src/)、[`tool-fs-search`](../../ref/deepseek-harness/packages/fs/tool-fs-search/src/)、[`tool-bash`](../../ref/deepseek-harness/packages/shell/tool-bash/src/index.ts)、[`tool-jobs`](../../ref/deepseek-harness/packages/jobs/tool-jobs/src/index.ts)
- Code Mode 指导及生成 SDK：[`tools`](../../ref/deepseek-harness/packages/core/tools/src/index.ts)、[`ts-types.ts`](../../ref/deepseek-harness/packages/core/tools/src/ts-types.ts)、[`py-types.ts`](../../ref/deepseek-harness/packages/core/tools/src/py-types.ts)
- Skill、Goal、Subagent 和 Workflow：[`tool-skill`](../../ref/deepseek-harness/packages/skill/tool-skill/src/index.ts)、[`goal`](../../ref/deepseek-harness/packages/goal/)、[`subagent`](../../ref/deepseek-harness/packages/subagent/)、[`workflow`](../../ref/deepseek-harness/packages/workflow/)
- 工作区指示、沙箱策略、压缩与标题生成：[`agent-instructions`](../../ref/deepseek-harness/packages/context/agent-instructions/src/render.ts)、[`sandbox-policy`](../../ref/deepseek-harness/packages/sandbox/sandbox-policy/src/index.ts)、[`summarizer.ts`](../../ref/deepseek-harness/packages/compaction/compaction-basic/src/summarizer.ts)、[`session-title-llm`](../../ref/deepseek-harness/packages/session/session-title-llm/src/index.ts)
- 较长的完整协议样例：[`tool-cordis/src/prompt.ts`](../../ref/deepseek-harness/packages/extensions/tool-cordis/src/prompt.ts)

## 提示词不是一篇文章，而是一组装配件

标准组合大致形成以下模型输入：

| 层 | 典型内容 | 写法 |
|---|---|---|
| identity | `You are an AI agent powered by DeepSeek Harness.` | 一句稳定事实 |
| persona | 模型、角色、工作目录 | 一至两句，不铺陈性格 |
| product context | Web GUI、Harness checkout | 精确定义指代和宿主边界 |
| tool guidance | read、edit、bash、jobs 等跨调用规则 | 每个工具一小段 |
| mode policy | plan mode、Code Mode | 状态、允许行为、退出条件 |
| dynamic context | 沙箱、审批、子 Agent 权限 | 持久 user-role 快照 |
| durable instructions | Skill 目录、AGENTS.md、goal round | 带来源和替换语义的 user-role 消息 |
| tool schema | 单次调用的参数和结果 | 简洁描述、严格结构 |

标准 persona 只有：

> You are a coding agent powered by the {{model}} model. Your working directory is {{cwd}}.

它不在 persona 中重复工具规则、工作方法或抽象人格。需要完整替换的 minimal preset 则显式使用 `complete: true`，避免旧 identity、工具指导和上下文残留。这体现了一个稳定习惯：内容归属通过结构表达，而不是让模型从一篇长文本中猜优先级。

## 语言层面的共同特征

### 1. 使用可判定条件，不使用泛化美德

常见句式是：

- `Use X for Y.`
- `Use X only when Y.`
- `Do not X; Y instead.`
- `If X, then Y.`
- `Before X, do Y.`
- `X does not mean Y.`

例如文件编辑指导不是笼统要求“谨慎修改”，而是明确：

> Read the file first ..., unless you just created or edited it in this session.

后台任务指导也不是“合理管理任务”，而是要求保存每个 job id、不要忙轮询、等待时继续独立工作，并在最终回答前收集仍相关的结果。

这些句子可以直接映射到一个具体决策点。诸如“be helpful”“use best practices”“think carefully”一类无法观察、无法判定的要求很少成为主规则。

### 2. 禁止项通常附带正确替代动作

Harness 很少只写 `Do not ...`：

- 不用 shell `cat`，改用 `read`，因为结果带行号且支持窗口续读。
- 不用 shell `find`，改用 `glob`，并解释无 `/` 的 pattern 会匹配任意深度 basename。
- 不用 shell `grep`，改用 `grep` 工具；需要上下文时再 `read` 命中文件。
- sandbox 拒绝后，不泛化为任务不能做；按结果中的 escalation guidance 重试一次精确操作。
- 子 Agent 权限不足时，不重复被拒操作；在回复中陈述限制，让父 Agent 处理。

因此其负面规则的实际模板更接近：

```text
不要做错误动作，因为它对应了错误的机制认知；改做正确动作，并观察明确结果。
```

### 3. 主动消除容易形成的错误心智模型

长协议尤其重视 `X does not mean Y`：

- `cordis_define` 只定义代码，不代表已运行。
- `starting` 表示进入异步流程，不代表成功。
- 工具仍出现在 plan mode 中，是为了请求缓存稳定，不代表允许修改。
- Python SDK 中的 `TypedDict` 是静态 stub，运行时并不存在。
- Code Mode 中间工具结果不会自动进入对话，只有打印或返回内容可见。

这是比增加感叹词或全大写更有效的强调方式：先指出模型可能作出的错误推断，再给出实际语义。

### 4. 精确指代状态、身份和时间

文案会区分：

- `currentPackageId` 与 `nextPackageId`
- `starting`、`awaiting-approval` 与最终成功
- running、stopping、completed、killed、failed
- 当前 goal revision 与旧 revision
- 本次操作、当前会话、下一 Step 和后续 Turn

它避免用“它”“完成了”“稍后”等模糊词承载关键状态。涉及并发、审批、持久化或版本时，这种名词重复是有意的，不追求文学上的省略。

### 5. 强调词少而有固定用途

全大写的 `ONLY`、`FAILED`、`MAY`、`STATIC STUB` 用于少数协议关键点，例如 Code Mode 的唯一输出通道、工具失败的控制流和并发许可。反引号用于精确名称和代码；破折号常用于在同一句中补充后果或例子。

Harness 不通过“重要”“非常重要”“最关键”等相对形容词建立大量隐式优先级。真正的优先级主要由 system/user role、section order、scope shadowing 和执行器约束承担。

## 不同长度提示词的模板

### 短工具指导

短 section 只放跨调用或跨工具选择规则；单次调用细节留在 tool schema。

典型结构是：

```text
Use <tool> for <job>.
Its critical semantics are <fact>.
If <edge case>, <recovery>.
```

`edit` 的 section 覆盖适用场景、literal replacement、唯一匹配、重复匹配时如何处理，以及何时必须先读文件。参数描述则只解释 `old_string`、`new_string`、`replace_all` 的局部含义。

这种拆分避免每个参数都重复完整工作流，也避免 system prompt 承担 schema 已经精确表达的类型信息。

### 模式提示词

Plan mode 是一个小型状态机，段落顺序稳定：

1. 当前处于什么状态，以及何时退出。
2. 允许的调查动作和禁止的变更动作。
3. 为什么仍能看到变更工具，以及哪条规则覆盖它们。
4. 何时允许向用户提问。
5. 一个“decision-complete”计划必须包含什么。
6. 唯一合法的退出调用、调用位置和失败后的行为。

这类提示词不仅说“请规划”，而是定义状态、输入、动作集合和终止协议。

### 长协议

[`CORDIS_SYSTEM_PROMPT`](../../ref/deepseek-harness/packages/extensions/tool-cordis/src/prompt.ts)最能体现其长文本模板：

1. 先定义机制和非目标。
2. 先判断任务是否适用，防止“工具存在即使用”。
3. 给出推荐工作流和编号工具顺序。
4. 定义身份、版本、审批等概念模型。
5. 单列高频错误，并同时给正确示例。
6. 划分 Host 与 Client 职责。
7. 最后说明异步结果和故障恢复。

这不是百科全书式平铺。章节顺序沿着模型的实际决策顺序：是否使用、如何选择、如何执行、如何理解结果、如何恢复。

### 辅助模型提示词

标题生成和 compaction 不使用 coding-agent persona，而是直接定义单一任务与输出语法。

标题生成要求只输出一行自然语言纯文本，逐一排除 quotes、prefix、explanation、Markdown、XML、控制码和代码；输入消息用 JSON 数组封装，避免用户文本破坏边界。

Compaction 要求固定 Markdown 章节、固定顺序、空章节写 `(none)`、使用短 bullet，并明确保留哪些精确事实。它还定义了已有 checkpoint 的合并语义。这里的冗长是为输出可解析性和恢复完整性服务，不是通用行为规范。

## Markdown、伪 XML 与 JSON 的分工

三种格式承担不同职责：

| 格式 | 使用场景 | 原因 |
|---|---|---|
| Markdown | 长稳定协议、工作流、固定输出模板 | 便于按决策阶段阅读 |
| 伪 XML | `<system-reminder>`、`<goal_round>`、`<skill_content>` 等注入边界 | 从普通对话中标出有生命周期的上下文块 |
| JSON | 标题输入、结构化结果、精确状态快照 | 不可信文本不能伪造结构边界，机器字段无歧义 |

伪 XML 不是为了模拟真正的 XML 数据模型。例如工作区指示会转义正文中的 `</system-reminder>`，说明标签的用途是保护提示框架。Skill catalog 更新还明确写“完整替换所有旧目录”；空目录是 tombstone，而不是让模型自行推断旧条目是否仍有效。

## 提示词与运行时约束同源

这是 Harness 最有价值、也最容易被表面模仿忽略的部分。

### Code Mode

提示词声明：

> `run_code` is the only tool you can call directly — a tool call naming any other tool fails.

生成该文本和执行器拒绝直接调用使用同一个 presentation mode 判断。可见工具集合又同时生成 SDK binding。模型看到的能力与真正可调用的能力来自同一 registry view。

### 文件修改

提示词要求先 `read` 再 `edit`/覆盖已有文件；默认 fs observation policy 会记录读取版本，并在修改时拒绝未观察或过期的目标。失败结果继续给出重新读取等补救提示。

### Tool restriction

作用域限制同时过滤模型 schema 和实际 dispatch，不是只在 prompt 中说“不要调用”。子 Agent 的审批策略也被固定为 `never`，模型文本只负责解释被拒后应该如何交回父 Agent。

### Goal

提示词要求完成前读取当前 goal、使用精确 id/revision，并只在相同阻塞连续达到配置轮数时报告 blocked。运行时执行 compare-and-set，并机械拒绝未达到最小轮数的 blocked；“相同条件是否持续”这种语义判断仍留给模型。

由此形成一个实用分界：

- 类型、枚举、版本、权限、状态转换等可机械判定约束交给代码。
- 适用性、目标是否真正完成、阻塞是否同一原因等语义判断交给提示词。
- 两边重叠处使用同一个配置或 predicate 生成文案和执行策略。

## 为请求缓存与持久化编写提示词

Harness 会考虑文案放在哪里，而不仅是写什么：

- 稳定 identity、persona、工具指导留在 system prompt。
- sandbox mode、approval policy、子 Agent 边界等变化事实作为持久 user-role context。
- Plan mode 保持 tool catalog 不变，只追加模式 section，以保留请求形状稳定性。
- Compaction 辅助请求复用原 system prompt、工具和消息前缀，只在尾部追加摘要指令。
- 工具和 Skill 列表确定排序；未变化时生成字节一致的文本。

这种风格会显式写 replacement、current、exact、same session 等词，因为提示词本身参与可重放状态，而不只是一次性建议。

## 与 alda-agent 的对照

当前 [`protocol.md`](../../alda-agent/prompts/protocol.md) 与 [`progressive-composition/SKILL.md`](../../alda-agent/skills/progressive-composition/SKILL.md)有明显优点：结果类型清晰、领域语法具体、提供了正确与错误示例、草稿与候选边界明确、艺术判断与客观校验有所区分。[`instructions.rs`](../../alda-agent/src/instructions.rs)也已经按 protocol、capability、workflow、advisory、preference、role 固定排序，并为片段保存来源、强度、digest 和整体 fingerprint。

主要差异如下：

| 维度 | DeepSeek Harness | alda-agent 当前实现 |
|---|---|---|
| 内容所有者 | 每个工具、策略或工作流维护自己的段落 | 大部分领域规则集中在 `protocol.md` |
| 核心角色 | 极短，只陈述身份和 cwd | 角色、流程、语法手册和校验规则共同进入编译结果 |
| 优先级 | role、order、scope、replacement | 固定片段顺序，加“最关键/最重要/严禁”等文本强调 |
| 工具规则 | 跨调用指导与 schema 分开 | `submit_result` 协议在正文开头、结尾和角色片段重复出现 |
| 失败恢复 | 通常紧邻失败状态给出下一动作 | 语法规则与常见错误速查分处不同章节 |
| 机制一致性 | prompt、schema、guard 尽量同源 | 项目偏好与 validator 已结构化，但语法手册仍是手工文本 |

### 当前可确认的问题

1. 拍号规则前后冲突且两条都与当前 CLI 不符：属性章节示例使用 `(time-signature! 4 4)`，常见错误章节又要求 `(time-signature 4 4)`。使用当前工作区的 Alda CLI 分别执行最小 `alda parse`，两者都会报 `unresolvable symbol`；这条手册内容已经造成实际的失败—错误修正循环。
2. `submit_result` 的必须调用与结果类型契约重复出现；重复会增加 token，但不增加可执行语义。
3. “乐器命名（最关键）”“时长估算（最重要）”依赖相对形容词表达优先级；当多个章节都声称最高优先时，模型仍需猜冲突顺序。
4. 完整语法手册、工作流和项目偏好全部常驻，稳定但较重。对于只需普通回答或澄清的轮次，大部分 Alda 语法不会参与决策。
5. “收到校验错误后保持原结果类型并修正”等恢复规则是好的，但还可以更靠近实际错误结果或 tool description，使错误、原因与重试动作处在同一上下文。

## 对 alda-agent 的建议

### 近期：只重写文案，不改变架构

1. 将核心协议压缩为“选择结果类型 → 填哪些字段 → 客观检查意味着什么 → 谁能接受候选”四部分，只保留一次 `submit_result` 终止规则。
2. 每条高风险语法规则采用统一句式：正确写法、错误写法、失败表现、修复动作。避免只用粗体和“严禁”强调。
3. 合并重复的时长、乐器、草稿/候选规则；用标题和顺序表达类别，不用多个“最重要”表达优先级。
4. 立即删除或改正当前 CLI 不支持的 `time-signature` 两种写法，并为手册示例增加执行到 score evaluation 阶段的自动 parse 测试；只检查 AST 会接受未解析的 symbol，无法发现这类错误。
5. 保留一个经过自动验证的完整示例；示例的价值是 few-shot 语法锚点，不必堆叠多个相似例子。

### 后续：让提示词与机制共用事实源

1. 从 `submit_result` schema 或结果类型定义生成字段说明，减少协议和工具定义漂移。
2. 从实际 validator 生成目标时长、include/exclude 与失败补救文案；不要在多处手写同一限制。
3. 将稳定的 Alda 语法参考与每轮动态项目偏好分开组装。是否进一步按需加载语法 Skill，应以真实 token 成本和生成质量实验决定，不需要为了模仿 Harness 预先平台化。
4. 若以后增加更多工具，为每个工具就地维护跨调用 guidance；不要继续扩张一篇总协议。
5. 对编译后的实际 system prompt 做 snapshot 测试，同时保留现有 fingerprint，确保顺序、来源和字节内容可追踪。

一个适合 Alda 规则的简化模板是：

```text
Use <result/tool/syntax> when <trigger>.
It means <exact semantics>.
Do not <likely mistake>; <correct replacement> instead.
If validation returns <state>, keep <invariant> and <recovery action>.
```

中文不必逐字翻译成英文命令句；重点是保留可判定触发条件、精确语义、常见误区和恢复动作。

## 不应照搬的部分

- 不应为了拆提示词而引入 Cordis、作用域注册表或事件溯源。`alda-agent` 当前的固定片段编译足以承载少量领域规则。
- 不应机械增加伪 XML。只有当动态注入、替换或不可信正文确实需要明确边界时才有价值。
- 不应把所有禁止项改成英文 `Do not`。Alda 面向中文创作交互，语言自然度仍重要。
- 不应立即把语法手册全部改成按需 Skill。Alda 生成高度依赖语法，常驻参考可能比多一次加载更可靠；应通过请求 token、首轮正确率和自动修正次数验证。
- 不应把语义判断强塞进 validator。作品是否有发展、高潮是否来自材料演变等仍需模型或用户判断。

## 可复用检查清单

编写或评审一段 agent 提示词时，可以依次检查：

1. 这段规则属于身份、工具、模式、动态事实、领域知识还是输出格式？
2. 它是否由最了解该机制的组件维护？
3. 触发条件是否可判定？关键状态和时间范围是否明确？
4. 禁止动作之后是否给出了正确替代动作？
5. 是否纠正了一个真实、高频的错误心智模型？
6. schema 或代码能机械执行的部分是否已经执行，而非只靠模型自律？
7. 文案与执行约束能否从同一配置、枚举或 predicate 生成？
8. 动态事实是否错误地放进稳定 system prompt？
9. 不可信文本是否可能伪造 Markdown、XML 或其他结构边界？
10. 同一规则是否在多处重复，或与另一处文本冲突？
11. 失败后模型能否从当前结果直接知道下一动作？
12. 是否有 snapshot、parser 或行为测试验证模型实际看到的内容？
