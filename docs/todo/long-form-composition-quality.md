# 长篇作曲质量与可控修改

> 证据：[长篇叙事作曲 Agent 困境分析](../iter/long-form-composition-agent-diagnosis/README.md)
>
> 状态：第二次代表性长曲已完成；结构机制有效，但调用成本上升、音乐质量未证明改善，进入任务专用 subagent A/B 原型阶段

## 结论

长篇质量问题不能靠继续增加提示词、模型调用额度或静态评分解决。当前最小可落地方案是：

1. 长篇候选必须携带结构化段落计划；
2. 用 Alda 原生 Marker 返回每个段落的真实时间位置和声部覆盖；
3. 修改已有长篇时声明目标段落，宿主机械验证其他段落的音乐事件没有改变；
4. 预检候选用内容哈希引用提交，局部修改优先发送基于 checkpoint 的受限 patch；
5. 完整试听作为主流程；出现具体问题时，可按 Marker 播放带前后文的局部窗口并进行 A/B；
6. 语法、时长和保持范围由程序判定，音乐表达仍由试听判定。
7. 用固定的主 Agent、两个段落家族 Worker、确定性 Harness 和只读 Reviewer 做任务专用 A/B，验证职责隔离。

现有实现仍复用单 Agent、`inspect_alda_source`、候选检查点、工作乐谱和 MIDI/WAV 链路，不引入分支乐谱、
通用工作流引擎或自动审美评分。下一阶段只对长篇作曲验证固定角色、两个段落家族 Worker 的最小原型；
不把它扩展为通用 Multi-Agent 平台。

## 原始缺口与当前进展

第一次五分钟真实创作用了 14 次模型调用、13 次工具往返和 1 次协议恢复。结构机制落地后的同题第二次运行
用了 20 次模型调用、19 次工具往返和 3 次协议恢复；虽然 7 个 Marker、form_plan、checkpoint 引用和
299.65 秒完整 WAV 均成功，但调用成本没有下降，音乐材料仍主要由短句高倍率展开。

现在 `ParseOutput`、`ScoreInfo`、`inspect_score` 和 `inspect_alda_source` 已按实际时间返回 Marker、段落事件、
声部覆盖与段落事件哈希。`form_plan` 边界对齐、持久化、接受时复检、`edit_scope` 保持校验、候选 hash 引用、
`inspect_alda_patch` 和按 Marker 局部播放也已实现。完整播放与完整 WAV 仍为默认路径，局部播放仅用于定位问题。

第二次运行还暴露了新的确定性缺口：模型误算变量拍数，D 段长笛越过下一 Marker，随后 `@section_a2` 将
声部游标向后跳并形成重叠；现有 Marker 边界校验没有拒绝这种情况。候选引用虽最终成功，但一致内容以冗余
字段提交时连续恢复两次。当前要同时补齐游标安全和协议容错，并通过任务专用角色 A/B 验证作曲设计与 DSL
实现的职责隔离是否有效。

## 目标流程

```text
用户要求
  → 结构化段落计划
  → 核心材料短片段检查
  → 带 Marker 的完整结构稿
  → 段落时间与覆盖检查
  → 配器和细节完善
  → 预检并产生候选 checkpoint
  → 引用 checkpoint 正式提交并自动渲染
  → 完整试听与整体反馈
  → 必要时定位并试听局部上下文
  → 局部修改并验证未目标段落保持不变
  → 用户显式接受
```

直接要求创作完整作品时，Agent 仍在同一次请求内完成候选，不先向用户提交一个必须确认的计划。段落计划
作为 `inspect_alda_source` 和最终 `submit_result` 的结构化参数参与自动循环，不额外制造一次对话往返。

## 1. 结构化段落计划

当项目目标时长下限不少于 120 秒时，`candidate` 必须提供 `form_plan`：

```json
{
  "target_duration_secs": 300,
  "sections": [
    {
      "id": "intro",
      "target_start_secs": 0,
      "target_end_secs": 35,
      "function": "建立环境与核心动机",
      "material_action": "introduce",
      "energy": "low"
    }
  ]
}
```

完整计划包含 4–10 个段落。字段语义如下：

| 字段 | 语义 |
|---|---|
| `id` | 稳定的 ASCII 段落 ID，对应 Alda `%section_<id>` |
| `target_start_secs` / `target_end_secs` | 计划时间区间 |
| `function` | 该段在全曲中的听觉或叙事职责 |
| `material_action` | `introduce`、`develop`、`contrast`、`reprise` 或 `close` |
| `energy` | `low`、`medium`、`high` 或 `peak`，用于说明能量曲线，不作自动评分 |

宿主只验证可机械判断的部分：ID 唯一、区间从 0 开始且连续无重叠、末尾符合项目总时长边界、段落数量和
字段有效。它不判断 `function` 是否真的实现，也不把填写计划视为质量通过。

`form_plan` 与对应源码一起保存在 `WorkingScore`、`PendingRevision` 和接受后的 `VersionMeta` 中。它不是
独立的通用 Artifact，也不建立另一套项目状态；工作乐谱或版本仍是唯一事实主体。

第二次运行证明这些字段不足以成为稳定的作曲契约：计划曾从 420 秒压缩到 300 秒，又跟随 406 秒、291 秒的
实际结果回填为 297.14 秒。后续 `CompositionSpec` / `SectionContract` 还需表达精确动机、和声区域、织体、
主导声部、前后过渡和必须采用的主题变形；否则 form_plan 只能验证结构登记，不能约束音乐实现。

## 2. Marker 段落证据

每个计划段落必须在源码中定义一次 `%section_<id>`。`ParseOutput` 增加 Alda 已经提供的 `markers` 字段，
`ScoreInfo` 在现有事件时间线上计算：

```text
SectionTimeline
├── id
├── planned_start_ms / planned_end_ms
├── actual_start_ms / actual_end_ms
├── boundary_error_ms
├── event_count
└── parts[]
    ├── part
    ├── event_count
    ├── sounding_ms
    └── coverage_ratio
```

段落起点取对应 Marker 的真实 offset；终点取下一个段落 Marker，最后一段取全曲结束时间。声部覆盖率按
事件区间与段落区间的交集计算。事件起点落在哪个段落，就归属哪个段落；跨越边界的延音不会被重复计数。

以下情况是候选硬失败：

- 缺少计划 Marker、出现额外的 `section_` Marker、顺序错误或重复；
- 计划区间不连续，或末尾不符合项目总时长约束；
- Marker 实际位置偏离计划边界超过 `max(2 秒, 该段目标时长的 10%)`。

声部覆盖率、事件密度、最大静音和源码中的高次数 `*N` 只作为诊断。它们能暴露“尾部只剩一个声部”或
“主要依靠扩大循环”等现象，但不能成为所有声部必须铺满、禁止休止或限制合法重复的硬规则。

下一轮确定性检查还必须报告并拒绝：

- 声部事件越过下一段 Marker 且不属于声明过渡；
- `@section_<id>` 将同一声部游标跳回已经播放的时间；
- 同一 Alda 声部由回跳或错误组装造成意外时间重叠。

`inspect_alda_source(scope=candidate)` 返回紧凑段落表，例如：

```text
intro     0.0–34.2s   目标 0–35s     6 声部
develop  34.2–121.0s 目标 35–120s   8 声部
climax  121.0–231.5s 目标 120–230s 10 声部
coda    231.5–290.0s 目标 230–300s  5 声部
```

模型因此直接修正具体区段，不再根据源码长度、循环次数或声部结尾反推整曲结构。

## 3. 局部修改契约

已有带 `form_plan` 的工作乐谱或版本时，新的候选修改必须提供：

```json
{
  "edit_scope": {
    "mode": "local",
    "target_sections": ["climax"],
    "intent": "增强高潮的主题变形与铜管推进"
  }
}
```

`local` 模式下，所有未列入 `target_sections` 的段落自动成为保持区，不让模型逐项声明后漏掉某段。宿主
分别解析基线和新候选，将每个保持区内的事件规范化后计算 SHA-256：

- 使用稳定的声部别名与 stock instrument，不使用 Alda 内部指针；
- offset 改为相对当前段落起点，允许前一目标段改变长度后整体平移；
- 保留 audible duration、音高、力度、声像和其他实际事件字段；
- 事件按相对 offset、声部和内容稳定排序；
- 事件起点位于段落内才归入该段，边界规则与段落诊断一致。

非目标段落的 `id`、`function`、`material_action`、`energy` 和目标时长也必须保持；如果前面的目标段变长或
缩短，只允许其 `target_start_secs` / `target_end_secs` 等量平移。

任一保持区哈希变化都是硬失败，并明确返回发生变化的段落。这样可以允许高潮变长导致尾声整体后移，
同时拒绝尾声内容被无意重写。

`global` 模式不执行保持校验，但只在首次创作或用户明确要求整体重写、改变曲式/风格时允许；否则工具返回
错误，要求选择目标段落或向用户澄清。最终结果必须显示本轮是“局部修改”还是“全局重写”。

第一次实现不使用 Alda 源码文本 diff 判断保持范围。变量、重复和声部排列可以重构，只要保持段落解析后的
音乐事件相同；这比比较源码行更接近用户实际听到的内容。

## 4. 候选引用与增量修改

候选预检成功后返回可引用的内容哈希：

```text
CandidateCheckpoint
├── source_hash
├── alda_code
├── form_plan
├── checks
└── baseline_hash        # 修改既有作品时存在
```

`submit_result(kind=candidate)` 必须且只能提供以下一种来源：

- `alda_code`：没有 checkpoint 的初次完整提交；
- `candidate_ref.source_hash`：引用本轮最新且检查通过的 checkpoint。

引用提交时，宿主根据 hash 读取已检查源码并执行最终渲染，不让模型再次序列化同一份 `alda_code`。hash
不是授权凭证，只能引用本次运行或项目中明确保存的候选；检查失败、hash 不匹配或基线已变化时拒绝提交。
若请求同时携带与 checkpoint 内容一致的冗余 `alda_code` 或 `form_plan`，宿主应安全归一化或在工具 schema
层阻止该组合，而不是消耗新的模型回合；只有内容冲突时才硬失败。

局部修改增加 `inspect_alda_patch`，参数为：

```json
{
  "base": { "kind": "work", "source_hash": "..." },
  "replacements": [
    { "old": "[group_a]*45", "new": "[group_a]*40 [theme_a2 theme_a]*5" }
  ],
  "edit_scope": {
    "mode": "local",
    "target_sections": ["climax"],
    "intent": "增强高潮的主题变形"
  }
}
```

每条 `old` 必须非空且在基线中恰好出现一次；替换之间不得重叠，基线 hash 必须匹配。宿主只在内存临时源码
上应用 1–8 条替换，然后执行与全量候选完全相同的语法、段落、保持范围和项目约束检查。成功后产生新的
checkpoint，失败不修改工作稿。若修改无法由少量唯一替换表达，仍可提交完整源码，但不得绕过 `edit_scope`
和保持段哈希。

补丁只能基于当前最新事实主体；若项目已有更新的恢复候选，不允许继续从旧 work/current 派生，改用恢复候选
上下文提交完整源码，避免补丁源码与保持校验引用不同基线。

这不是通用文件编辑器或 Artifact 运行时：工具只操作 Alda 候选，只接受已知基线，只产生可检查的候选
checkpoint。源码上限继续保留；只有代表性任务实际触及时，才根据测量调整，不把预想容量问题当成扩容依据。

## 5. Agent 生成策略

渐进式创作 Skill 对长篇任务增加以下确定顺序：

1. 建立 4–10 段的 `form_plan`，先分配叙事职责、材料动作和时间预算；
2. 用 `scope=fragment` 检查 1–2 个核心材料，不要求完整时长；
3. 先完成带所有 Marker 的全曲结构稿，再补充配器细节；
4. 用 `scope=candidate` 读取真实段落时间，只修正失败区段；
5. 时长不足时扩展承担明确职责的段落，不默认增加同一短循环次数或只降低 tempo；
6. 客观检查通过后引用最新 checkpoint 提交候选，由现有链路自动生成 MIDI/WAV。

工具不会要求模型逐段提交多个工作稿。中间源码仍只存在于本次模型上下文和最新候选检查点，成功时只保存
一个工作候选，保持当前项目模型简单。

第二次真实运行表明，单 Agent 在上述步骤中仍同时承担艺术设计、拍数核算、Alda 编码与协议恢复。下一阶段
增加任务专用 A/B 路径：主 Agent 持有 CompositionSpec；主题家族 Worker 实现 Intro/A/A2/Coda；对比发展
Worker 实现 B/C/D；Harness 确定性组装；独立 Reviewer 只读复核。详细边界见
[Workflow 产物与 Agent 角色派生](workflow-artifacts-and-agent-roles.md)。

## 6. 整体试听与可选局部诊断

候选仍自动渲染完整 WAV，用户首先按自己的方式整体试听并反馈。系统不要求逐段播放、逐段评分或逐段确认，
也不把局部试听设为接受候选的门禁。

`play_score` 和 `/alda play work|current section ID` 增加可选时间窗口。用户可以使用段落 ID，也可以直接说
“三分钟附近开始单调”或“高潮来得太突然”；宿主根据实际秒数和 Marker 定位相关段落。局部窗口默认包含
目标区间前后各 5–15 秒，不机械切断过渡、延音和跨段织体。最后一段只需给出起点。

`render_score` 保持完整 WAV 渲染，不持久化每段独立音频；首次实现只解决低成本的定位与试听，不增加局部
音频资产生命周期。

局部播放只在定位问题、验证修改或 A/B 时启用。后续修改把反馈映射为一个或多个
`edit_scope.target_sections`，由保持校验保护其他段落；修改完成后仍以完整作品的整体听感作为接受依据。

以下 Rubric 仅作为可选试听提示，不要求逐项填写，也不合成为自动分数：

- 核心主题是否可辨认并发生发展；
- 各段是否具有可听辨的职责与过渡；
- 叙事峰值是否出现在计划段；
- 重复是否伴随变形，而非机械延长；
- 结尾是否完整。

静态检查只报告事实；用户未试听并明确接受前，候选仍不是有效版本。

## 代码落点

| 文件 | 修改 |
|---|---|
| `src/alda.rs` | 读取 `markers` 和完整事件字段；计算 `SectionTimeline` 与段落事件哈希 |
| `src/agent.rs` | 增加 `form_plan`、`edit_scope`、checkpoint 引用和 `inspect_alda_patch`；返回段落诊断 |
| `src/project.rs` | 将计划随工作稿、恢复候选和版本原子持久化 |
| `src/application.rs` | 构造基线与全局重写授权；保存计划；展示修改范围 |
| `src/alda.rs`、`src/application.rs` | 支持按 Marker 的 `--from` / `--to` 上下文窗口播放 |
| `prompts/protocol.md`、渐进式 Skill | 固定长篇的结构先行、局部修正和试听边界 |
| `src/command.rs`、`src/control.rs` | 支持按需播放 Marker 局部窗口 |

## 实施顺序

### 第一阶段：段落事实

- [x] 读取 Alda `markers`；
- [x] 输出 Marker 实际时间；
- [x] 输出各段边界、事件与声部覆盖；
- [x] 随 `form_plan` 为缺失、乱序和明显偏离计划的 Marker 建立测试。

Marker 段落事实已经能替代段落位置和声部覆盖手算；计划边界校验随第二阶段实现。

### 第二阶段：长篇计划（已实现）

- 扩展工具 schema 和项目持久化；
- 长篇候选强制携带有效 `form_plan`；
- 预检返回 source hash，正式提交引用 checkpoint；
- 更新渐进式 Skill，让结构稿先于配器细化。

### 第三阶段：局部保持（已实现）

- 计算规范化段落事件哈希；
- 增加基于 source hash 和唯一文本替换的候选 patch；
- 在预检和正式提交时执行同一套 `edit_scope` 校验；
- 覆盖目标段变化、保持段误改、整体平移和重启恢复测试。

### 第四阶段：试听闭环（工程能力已实现，待人工验收）

- 完整渲染和整体试听保持默认路径；
- 按 Marker 或时间位置播放带前后文的可选局部窗口；
- 展示问题对应段落和修改范围；
- 用代表性长曲执行真实模型与人工试听验收。

### 第五阶段：任务专用角色 A/B（待实现）

- 固定主 Agent、两个段落家族 Worker、Harness assembler 和只读 Reviewer；
- 用 CompositionSpec、SectionContract、SectionArtifact、ReviewReport 传递明确产物；
- 对照现有单 Agent 基线测量调用成本、协议恢复、游标错误和人工完整试听结果；
- 若无可测收益，撤回原型，不扩展通用 Artifact、Role 或 Multi-Agent 平台。

## 完成判定

- 三个 3–5 分钟代表性任务的所有计划段落都能由真实 Marker 时间定位；
- 初次完整候选在 4–6 次模型调用内形成，协议恢复为 0；
- 直接完整创作意图不再错误返回 plan 并要求用户输入“继续”；
- 预检通过后的正式提交只引用 checkpoint，不再重传完整源码；
- 不存在未声明的跨段溢出、声部游标回跳或意外同声部重叠；
- 时长修正记录显示修改的是具体职责段落，不再主要依赖提高同一循环次数；
- 连续两次局部修改中，所有非目标段落事件哈希保持一致；
- 适合局部表达的修改通过 patch 完成，基线冲突或非唯一替换不会改变工作稿；
- 用户可用自然语言时间位置或段落定位问题，并按需播放带前后文的局部窗口；
- 不进行逐段强制确认，最终接受仍依据完整作品试听；
- 人工试听能辨认主题发展、段落职责、叙事峰值和完整结尾；
- 主题变形与和声路线能从 CompositionSpec 定位到最终事件；
- 静态检查结果与试听结论分别记录，没有把非静音、事件数或时长达标表述为艺术质量通过。

## 明确不做

- 不让 LLM、事件数、RMS 或重复次数自动给作品打质量分；
- 不要求所有声部等长或每段持续发声；
- 不直接建设通用 Director/Composer/Critic 平台；
- 不按乐器拆 Worker，不让 Agent 自由合并或持久化完整源码；
- 不把内部按段生产变成用户逐段试听或审批；
- 不建立通用 Artifact 仓库、工作流 DSL、并列候选或版本分支；
- 不保存每次中间模型源码，只保留现有最新恢复候选和成功工作稿。
