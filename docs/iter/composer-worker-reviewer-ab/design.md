# 原生 Alda 上的最小角色工作流设计

> 后续状态：2026-08-19 已将本文的 roles 路径接入主程序 `composition-ab` 模式；A/B 实验入口已移除，
> Project 仍是工作候选和版本的唯一事实主体。

## 设计结论

Alda 已经提供 sequence、variable、repeat、instrument parts、voices、rests 和 Marker，能够定义、复用和按时间
执行音乐片段。角色工作流不再建设“片段编排器”或第二套音乐 IR。Alda 是唯一的片段与时间线语言；Harness
只是围绕多个模型交付增加一层薄的预算编译、固定模板、安全边界和真实解析验收。

```text
用户要求
   ↓
ComposerPlan（音乐决策与相对比例，不含 Alda 和绝对拍数）
   ↓
Harness 预算编译（纯 Rust、确定性）
   ↓
只读段落预算 ───────────────┐
   ↓                       ↓
主题家族 Worker       对比发展家族 Worker
   └──────────┬────────────┘
              ↓
      固定模板生成普通 Alda
              ↓
      alda parse → MIDI → WAV
              ↓
        只读 Reviewer → 用户完整试听
```

## 为什么仍需要一层 Harness

Alda 官方参考明确说明，[变量可以从较小组件构建乐谱](../../../ref/alda/doc/variables.md)，但
[Alda 刻意不强制各 instrument part 同步](../../../ref/alda/doc/scores-and-parts.md)；每件乐器维护自己的
offset、tempo、duration 和 octave。Alda 2 也
[不提供用户可定义的函数等通用编程设施](../../../ref/alda/doc/writing-music-programmatically.md)，更不存在
跨独立模型调用的交付协议。

因此缺少的不是音乐组合能力，而是多 Worker 边界：谁拥有哪个段落、片段是否越界、名称是否冲突、所有声部
是否抵达约定段尾，以及失败应退回哪个 Worker。Harness 只补这些事实，不解释旋律、和声、织体或对位。

## 最小运行内数据

### ComposerPlan

Composer 提交面向音乐家的计划：

```text
title
tempo
meter
phrase_grid_bars
parts[]
motifs[]
sections[]
  id
  family                 theme | development
  length_weight          正相对权重，不要求求和
  tonal_center
  harmonic_plan
  texture
  material_plan
```

`motifs` 和各音乐计划字段表达创作意图，供 Worker 与 Reviewer 使用；宿主只验证 ID、枚举、引用和基本完整性，
不尝试证明文本中的音乐承诺已经实现。

第一版使用单一全局 tempo 和 meter。只有代表性作品证明需要段内变速且现有验证无法表达时，才扩展 tempo map。

### SectionBudget

Harness 从 ComposerPlan 和用户目标时长派生：

```text
section_id
duration_beats
planned_start_beats
planned_end_beats
```

它是运行内的只读实现预算，不是新的持久 Artifact，也不包含音乐技法类型。

### WorkerSubmission

Worker 只返回所属家族的每个段落及全部约定声部：

```text
family
sections[]
  section_id
  parts[]
    part_id
    alda_sequence_body
```

`alda_sequence_body` 是可直接放进 Alda variable sequence 的原生 Alda 事件。Worker 不能声明变量、part 或
Marker，不能引用其他 Worker 的名称，也不能修改 tempo、声部集合、段落顺序和预算。

Harness 同时把已确定的乐句网格公开为每段 `phrase_beats`、`repeat_count` 和必需 body 形状。它们只是
SectionBudget 的可执行说明，不是第二套音乐 IR。Worker 的重复乐句必须以绝对 `oN` 重置音区，并使用绝对
octave 变化；实验边界拒绝 `<`、`>`，因为 Alda 的相对音区状态会跨 repeat 累积。Harness 不生成乐句内容，
也不会在 Worker 少写时自动补 rest 或 repeat。

### ReviewReport

Reviewer 返回批准状态、引用真实段落的阻断问题、非阻断音乐观察和总结。报告只影响实验臂是否成功，不改变
源码或 Project。

## 预算编译

预算编译是唯一新增的时间计算，不重现 Alda 的事件执行：

1. 根据 meter 将一小节换算为四分音符拍数；
2. 用 `phrase_grid_bars` 得到一个分配网格；
3. 根据目标秒数和 tempo 选择最接近的总网格数；
4. 每段先分配一个网格，再按 `length_weight` 用确定的最大余数法分配剩余网格；
5. 同余数按 Composer 原始段落顺序决胜；
6. 输出精确有理数拍位和实际计划秒数。

如果目标时长不能被音乐网格整除，选择最近的合法网格并明确报告差值。约五分钟任务接受项目既有时长容差；
严格墙钟时长只有在用户明确要求时才允许尾声使用更细网格。

## 原生 Alda 生成

Harness 不解析后重写 Worker 音乐，也不实现片段调用语义。它只用固定模板生成 Alda 原生变量和轨道：

```alda
frag_intro_lead = [
  # Worker 提交的原生 Alda sequence body
]

frag_theme_lead = [
  # Worker 提交的原生 Alda sequence body
]

flute "lead":
  %section_intro
  frag_intro_lead
  %section_theme
  frag_theme_lead
```

每个 part 按同一段落顺序调用自己的变量。Harness 生成所有全局名称，Worker 因而不需要命名空间能力。正式
源码只在一个指定 part 上保留用户可理解的 `%section_*`，避免重复定义；验证源码额外在每个 part 的段首和
段尾插入唯一的临时 Marker。

输出按段落和声部稳定排序并保留计划摘要注释，以便阅读。生成过程不使用 `@marker` 回跳，也不通过提高重复
次数补足长度。

## 验证边界

验证分为三层：

1. 提交边界：家族、段落和声部集合精确匹配；片段不得逃出宿主 sequence、声明全局名称、part 或 Marker。
2. 真实 Alda：先对每个 Worker 家族生成验证谱并执行 `alda parse`，再对合并作品重复一次。
3. 时间事实：比较每个 part 的临时段首/段尾 Marker 与 SectionBudget；任何过短、过长、越界或回跳都失败。

段内的 notes、rests、chords、voices、repeats 和相对进入全部由 Alda 原生执行。Harness 不增加通用 Cue、
Placement、对位关系或材料变形验证。需要晚进入的声部由 Worker 使用原生 rest/voice 表达，Reviewer 根据计划
和最终乐谱判断其音乐作用。

声部游标精确对齐不等于音频恰在同一毫秒结束。最终检查继续分别报告游标时间、可听事件时长和 WAV 时长。

## 失败与返工

- ComposerPlan 协议错误允许一次协议恢复；预算算术不交还模型修正。
- Worker 协议错误遵循统一角色协议；真实 Alda 或时间边界失败后，只将具体错误退回所属 Worker 一次。
- 一个 Worker 失败不会要求另一个 Worker 改写已通过的家族，但角色臂整体失败并保存双方产物和统计。
- Reviewer 首版只批准或阻断，不触发自动艺术返工。先证明流程能稳定形成完整候选，再决定是否增加一次定向
  音乐返工。

## 与现有代码的关系

`composition.rs` 已证明变量模板、临时 Marker 和真实 offset 校验可行；本轮角色实验保留这些验证能力，
但没有把它扩展成音乐运行时。当前实现已经：

- 从 Composer schema 删除绝对 `duration_beats` 和音乐入口 Anchor；
- 增加最小的 meter、phrase grid、length weight 和预算编译；
- 将 Worker 产物收缩为每段每声部的原生 Alda sequence body；
- 保留宿主命名、转义防护、双源码探针和 `alda parse`；
- 放弃早期计划中的通用 Cue/Placement、对位分析器和富音乐 `CompositionSpec` 方向。

接入后 `Application` 负责按项目 Agent 模式路由，Project 的工作稿和版本模型保持不变。角色中间产物仍不
持久化，只有通过最终检查和渲染的完整候选写入统一工作乐谱。

## A/B 与接入门槛

保留首轮失败证据，在新输出目录运行同一任务：

- baseline 继续调用现有单 Agent；
- roles 必须通过 Composer、两个 Worker、合并、Reviewer 和完整渲染；
- 比较调用数、协议恢复、耗时、技术失败、时长和事件事实；
- 只有得到两份完整 WAV 后才进行人工试听 A/B。

一次成功只能证明路径可行。至少再用一个不同曲式或拍号的长篇任务验证没有针对首题过拟合，才能讨论接入
正式流程；若成本或听感没有可测收益，撤回角色入口，保留 Alda 校验改进。

## 非目标

- 不实现 Alda include、模块、参数化函数或第三方音乐 DSL。
- 不自动续写、拼接过渡、改配器或修复“不好听”的片段。
- 不增加对位、和声或风格专用宿主机制。
- 不把内部片段变成 Project 级 Artifact、多个工作稿或用户逐段审批对象。
- 不因已有实验代码存在而承诺把它接入生产。
