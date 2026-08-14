# 可组合指示、Skill 与 Agent 角色派生

> 来源：渐进式创作闭环的真实使用与后续 Multi-Agent 架构讨论
>
> 状态：组合指示与 Advisory Skill 首期已落地；产物、运行快照、Role/fork 仍待真实任务验证
>
> 已完成范围与最终语义：[可组合指示系统首期](../iter/composable-instructions/README.md)

> 说明：本文用于记录当前问题、约束和候选方向。其中的模型、边界和演进顺序都是初步建议，不代表最终结论；
> 若后续代码验证、真实作品测试或对照实验不支持当前设想，应直接修订或放弃相关方案。

## 背景

本需求提出时，Agent 指示由三部分直接拼接：内建生成协议、默认创作策略和项目级 `creative_strategy` 字符串。这足以
支持当前单 Agent 创作，但不能清楚回答以下问题：

- 某条指示来自内建协议、产品默认、用户偏好还是本轮请求；
- 哪些规则不可覆盖，哪些默认值允许用户修改；
- 多个用户 Skill 同时启用时如何合并并发现冲突；
- 一套工作方式如何既由当前单 Agent 执行，又能在未来派生给 Director、Composer、Critic 等不同角色；
- 子 Agent 应继承哪些上下文、产物和能力，如何避免复制完整对话和无关指示；
- Skill 修改后如何重建历史运行输入并解释行为差异。

后续设计不应先把系统改成 Multi-Agent。应先把工作方式表达为可组合、可追踪、可验证的指示与产物协议；
单 Agent 和多角色只是同一工作流的不同执行方式。

## 核心原则

1. **Skill 描述工作，Role 描述执行者。** Skill 定义步骤、产物和门槛，不绑定某个 Agent；Role 定义职责、
   申请的产物范围与工具，以及停止条件。
2. **核心不变量不可覆盖。** 用户偏好和 Skill 可以改变创作方法与默认值，但不能关闭 Alda 校验、让候选
   自动成为版本或扩大角色能力。
3. **Project 是持久事实来源。** 指示编译器、工作流和角色运行时不能各自维护另一套项目状态。
4. **能力只能显式授予并在 fork 时收窄。** 子角色不能因为继承父 Agent 上下文而自动得到版本写入、接受
   候选或其他无关权限。
5. **按需加载。** Skill 元数据用于发现和触发，正文只在启用后加载，详细参考资料只在具体步骤需要时读取。
6. **冲突必须可见。** 不能依赖 Prompt 拼接顺序暗中解决两个 Skill 或偏好之间的实质冲突。
7. **先单 Agent 验证，再拆角色。** 只有相同工作方式由单 Agent 稳定执行后，才评估 fork 是否改善质量、
   成本或上下文隔离。
8. **先限制组合范围。** 首期暂定只允许一个定义步骤拓扑的 workflow Skill，其他 Skill 只贡献指示、偏好或
   附加门槛。多个 workflow Skill 的通用合并语义在出现真实需求前不实现。

## 概念边界

| 概念 | 作用 | 示例 | 是否可覆盖 |
|---|---|---|---|
| 核心协议 | 保证工具、状态、版本和安全不变量 | `submit_result` 类型、候选接受边界 | 否 |
| 运行授权 | 由宿主限制本次执行可读写的对象和可调用工具 | Critic 只能创建评审报告 | 只能收窄 |
| 产品默认 | 提供未配置时的通用行为 | 默认使用渐进创作 | 可被偏好覆盖 |
| Workflow Skill | 定义可复用的主步骤、产物和门槛 | 分段创作 | 首期同时只启用一个 |
| Advisory Skill | 贡献方法指示、结构化偏好或附加门槛 | 歌曲写作、短谱配器 | 可组合、启停 |
| 用户偏好 | 表达用户或项目的稳定选择 | 目标时长、密度、避免某件乐器 | 用户可修改 |
| Role | 定义某类执行者的职责和能力 | Director、Composer、Critic | 由执行配置选择 |
| 本轮任务 | 表达当前一次请求和反馈 | “把副歌发展得更有力量” | 仅本轮有效 |

原实现的产品默认已与 `creative_strategy: String` 分开注入；真正混合在该字符串中的是项目偏好和项目工作方法。
后续应拆分这两类信息，不再把它作为所有自定义需求的唯一容器。

## 指示分层与合并

每次模型调用前由指示编译器产生一个不可变的有效指示快照。建议顺序为：

```text
Core Protocol
  → Capability View
  → Product Defaults
  → Active Skills
  → User Preferences
  → Project Preferences
  → Role Contract
  → Current Task Brief
```

不应用一条全局优先级处理所有类型的冲突。初步建议按语义分开合并：

```text
结构化偏好：本轮明确值 > 项目偏好 > 用户级偏好 > Skill 默认 > 产品默认
安全与状态不变量：只能由 Application 强制，不参与文本覆盖
运行能力：Application Policy ∩ Parent Grant ∩ Role Allowlist
工作流门槛：结构化合并；无法合并时停止编译并报告冲突
```

优先级不能被实现为无条件的文本覆盖：

- 普通偏好可以覆盖默认值，例如目标时长从 180 秒改为 240 秒；
- 安全、状态和版本约束只能累加或收紧；
- 角色能力只能从父运行能力集合中取子集；
- 同层 Skill 对同一字段给出不同强制值时，应报告冲突并要求选择；
- 自然语言规则暂时无法机械合并时，应保留来源标签和稳定顺序，但不把文本顺序宣称为已解决的覆盖关系；
- 只能报告“未发现结构化冲突”，不能因此断言自然语言规则之间没有冲突。

建议的核心值对象：

```text
InstructionFragment
├── id
├── qualified_id
├── kind                   # protocol / default / skill / preference / role / task
├── origin                 # builtin / user / project
├── scope                  # global / project / run / stage
├── strength               # invariant / constraint / preference / guidance
├── content
└── digest

CompiledInstructions
├── fragments
├── resolved_preferences
├── effective_capability_view
├── conflicts
└── fingerprint
```

能力视图只用于向模型和用户解释当前边界；权威的能力集属于运行时执行上下文，不由指示编译器或 Skill 产生。

不要在首期为所有自然语言指示建立复杂表达式语言。先结构化可机械处理的偏好和能力视图，其余规则作为带来源的
文本片段参与编译。

## Skill 模型

Skill 是可发现、按需加载的指示包，不是 Agent，也不直接拥有项目状态。建议支持三个来源：

```text
程序内建：builtin skills
用户级：~/.alda-agent/skills/<name>/SKILL.md
项目级：<project>/skills/<name>/SKILL.md
```

最小目录结构：

```text
skill-name/
├── SKILL.md
├── workflow.yaml          # 可选；仅 workflow Skill 使用的结构化步骤声明
├── references/            # 可选，按需读取的领域资料
├── schemas/               # 可选，结构化产物定义
└── templates/             # 可选，计划或评审模板
```

首期不允许 Skill 执行任意脚本。待出现稳定的确定性任务后，再单独设计脚本能力和审批边界。
`SKILL.md` 的自然语言正文也不应成为可执行工作流的唯一事实来源；稳定步骤 ID、产物输入输出和机械门槛需要结构化声明。
第一个 workflow Skill 可先作为程序内建定义；只有用户或项目级 workflow 出现真实需求时，再固定
`workflow.yaml` 格式。

`SKILL.md` 至少包含：

```yaml
---
name: sectional-composition
description: 用统一小节时间轴逐段完成多声部作品
kind: workflow
---
```

正文描述：

- 适用条件与触发语义；
- 输入产物；
- 步骤和顺序；
- 每步输出产物；
- 需要调用的内建 validator ID 及其参数；
- 需要真实试听或用户确认的检查点；
- 必要参考资料及其读取条件。

Skill 不应写死“由 Composer Agent 执行”。相同 Skill 在当前可以全部绑定给默认 Agent，将来可把不同步骤
绑定给不同角色。

机械门槛必须引用 `Application` 拥有的内建 validator，例如 `section_bar_count` 或
`required_voice_coverage`。Skill 可以选择并参数化 validator，但不能通过自然语言声称某项检查已经机械通过。

Skill loader 至少需要约束：

- 使用带来源的限定 Skill ID，避免内建、用户和项目 Skill 同名时暗中覆盖；
- 规范化根目录和符号链接，参考资料、schema 和模板不得越出当前 Skill 根目录；
- 限制扫描深度、目录数量、单文件字节数、总加载字节数和模型上下文占用；
- 对无效元数据、缺失引用和超限内容给出可见错误，不部分启用一个已损坏的 Skill。

## 偏好与启用配置

项目元数据应只保存 Skill 引用、参数和结构化偏好，不复制 Skill 全文。例如：

```json
{
  "instruction_profile": {
    "workflow_skill": "builtin:sectional-composition",
    "enabled_advisory_skills": [
      "project:lyric-song-workflow"
    ],
    "skill_parameters": {
      "builtin:sectional-composition": {
        "draft_bars": 8,
        "require_listening_checkpoint": true
      }
    },
    "preferences": {
      "target_duration_secs": 240,
      "arrangement_density": "moderate"
    }
  }
}
```

保存规则：

- 可跨作品复用的方法写入 Skill；
- 用户长期偏好写入用户级配置；
- 单项目稳定偏好写入项目配置；
- 本轮要求和试听反馈保留在对话或运行任务中；
- 计划、短谱、段落契约和诊断属于项目产物；
- 一次性修改意见不得自动回写 Skill。

用户级长期偏好属于独立的用户配置，不得因为本次编译后生效就复制到 `project.json`。项目元数据只保存显式的
项目覆盖。

## 结构化工作产物

为支持可靠交接，工作流应逐步引入明确产物，而不是让角色只交换自然语言消息：

```text
brief
form_plan
section_contract
harmony_timeline
short_score
orchestrated_section
diagnostic_report
review_report
candidate
```

首期不要求一次实现全部类型。应从真实缺陷直接需要的 `form_plan`、`section_contract` 和
`diagnostic_report` 开始，先解决小节数、声部覆盖和段落边界无法验证的问题。

产物初步建议使用不可变 revision：修改产物时创建新 revision，不原地改写已被下游引用的内容。每个 revision 应具备：

- 稳定 ID 与类型；
- revision ID；
- 创建它的运行和阶段；
- 所依据的上游产物 ID；
- 内容引用、内容摘要与摘要哈希；
- 验证结果；
- 用户确认状态。

`Project` 保存当前生效的 `active_artifact_refs`，不可变内容可以单独存在 `artifacts/` 下。`runs/` 只记录某次运行读写了
哪些 revision，不决定当前项目事实。产物的可读、可创建和可接受权限由运行时 grant 检查，不作为可被 Role 提示词
改变的产物属性。

## Role 与 fork

Role 是能力受限的执行配置：

```text
RoleProfile
├── id
├── purpose
├── readable_artifact_types
├── creatable_artifact_types
├── requested_tools
├── output_schema
├── stop_conditions
└── instruction_overlay
```

RoleProfile 只是运行配置和能力申请，不是授权主体。每次运行由宿主生成不可变的 `ExecutionGrant`：

```text
ExecutionGrant
├── actor_id
├── readable_artifact_refs
├── creatable_artifact_types
├── allowed_tools
├── allowed_project_operations
└── digest
```

有效 grant 是 `Application Policy ∩ Parent Grant ∩ Role Allowlist` 的交集。Agent Runtime 只向模型注册交集后的工具，
`Application` 仍需在真实写操作入口复核 grant；不能仅靠系统提示中的“明确禁止”执行权限边界。

建议的未来角色边界：

| Role | 可读取 | 可写入 | 明确禁止 |
|---|---|---|---|
| Director | 用户意图、偏好、反馈、已有计划 | `brief`、`form_plan`、`section_contract` | 写完整 Alda、接受版本 |
| Composer | 计划、段落契约、当前短谱和工作乐谱 | 短谱、草稿、候选 | 修改用户偏好、接受版本 |
| Critic | 计划、候选、确定性诊断和检查结果 | `review_report` | 修改 Alda、写工作乐谱、接受版本 |
| Coordinator | 工作流状态和所有阶段结果摘要 | 下一步状态、用户检查点请求 | 绕过校验、代替用户接受候选 |

不应按单个乐器拆分 Agent。音乐声部强耦合，分别生成吉他、贝斯或小提琴会放大时间轴和和声错位。

Fork 不继承父 Agent 的完整对话和全部权限，而是从运行快照构造最小上下文：

```text
ForkSpec
├── parent_run_id
├── role_id
├── stage_id
├── task_brief
├── selected_artifact_refs
├── selected_skill_snapshots
├── instruction_snapshot_id
└── execution_grant_id
```

不得默认传递：

- 父 Agent 的隐藏推理；
- 与本阶段无关的完整原始对话；
- 未被选择的 Skill 和参考资料；
- 父 Agent 拥有但子角色不需要的工具；
- 接受候选或直接写有效版本的权限。

## 执行映射

工作流步骤与 Role 之间使用独立映射：

```text
create_brief              → Director
create_form_plan          → Director
compose_short_score       → Composer
orchestrate_section       → Composer
review_candidate          → Critic
request_listening         → Human checkpoint
accept_candidate          → Application, requires explicit Human authorization
```

当前单 Agent 配置可把所有模型步骤绑定到 `default-agent`。未来 Multi-Agent 配置只改变执行映射，不修改
Skill、产物结构或项目不变量。

`accept_candidate` 不应成为任何模型 Role 可调用的工具。用户是授权者，`Application` 是执行者；两者不应合并成一个模糊的
工作流执行角色。若未来必须支持异步批准，再单独设计一次性批准凭证和失效语义。

## 运行快照与可解释性

只保存 Skill 名称或内容摘要哈希不足以重建历史输入，因为 Skill 文件和角色配置会变化。指示快照应保存当时真实渲染内容或指向
不可变内容对象：

```text
InstructionSnapshot
├── core_protocol_version
├── active_skill_snapshot_refs
├── resolved_preferences
├── role_profile_ref
├── execution_grant_digest
├── rendered_instruction_ref
├── rendered_instruction_digest
└── created_at
```

当前 Agent 的一次运行可以包含多次模型调用和校验反馈，因此还需要分开 `Run` 与 `Invocation`：

```text
Run
├── run_id
├── task
├── input_artifact_refs
├── invocation_ids
└── outcome

Invocation
├── invocation_id
├── instruction_snapshot_id
├── input_message_digest
├── model_and_sampling_config
├── tool_schema_digests
├── validator_version
└── response_and_result_refs
```

初步建议将大块不可变内容按摘要存放，运行目录仅保存引用和本次状态：

```text
project-root/
├── objects/<digest>             # Skill、渲染指示、产物等不可变内容
├── artifacts/                  # 项目产物 revision 元数据
└── runs/<run-id>/
    ├── run.json
    ├── invocations/
    └── outcome.json
```

运行快照用于审计和输入重建，不成为项目长期配置的第二事实来源。模型本身可能具有非确定性，因此不承诺仅凭快照生成逐字节相同的
输出。记录中不得持久化模型密钥等秘密。

用户应能查看本次实际生效的配置，例如后续提供 `/project instructions`：

```text
Core protocol: v3
Active skills: builtin:sectional-composition, project:lyric-song-workflow
Project preferences: target duration 240s, moderate density
Role: composer
Structured conflicts: none detected
Natural-language conflicts: not mechanically verified
Fingerprint: 7f3a...
```

## 与当前实现的演进关系

当前边界继续保留：

- `Project` 管理工作乐谱、有效版本和持久化不变量；
- `Application` 执行用户动作和领域用例；
- `Agent` 调用模型并编排 Alda 校验与有限修正；
- 终端和 JSONL control 只是适配器；
- 用户显式接受仍是 Agent 候选成为版本的唯一入口。

后续新增关注点应保持为：

```text
Instruction Compiler   # 本次应遵守什么
Workflow               # 依次产生什么产物、满足什么门槛
Role                    # 谁执行、能读写什么
Execution Grant         # 宿主实际授予什么能力
Agent Runtime           # 如何调用模型和工具
Project                 # 什么是持久事实和有效版本
```

不要让 Instruction Compiler 直接修改项目，不要让 Role 拥有独立持久状态，也不要让 Skill 绕过
`Application` 调用领域写操作。指示中的能力描述不能替代 Agent Runtime 的工具过滤和 `Application` 的命令级复核。

## 最小演进顺序

1. 把当前核心协议、产品默认、项目策略和本轮任务迁入指示编译器，先保持现有行为，并能查看实际渲染内容；
2. 定义 `InstructionFragment`、`CompiledInstructions` 和按语义分类的合并规则，拆分 `kind`、`origin`、`scope` 和 `strength`；
3. 把 `creative_strategy` 拆成结构化项目偏好和 Advisory Skill，并把默认创作策略迁移为内建 Skill；
4. 支持有边界的 Skill 发现、显式启用、参数校验和 `/project instructions`；首期仅允许一个内建 workflow Skill 与多个 Advisory Skill；
5. 引入 Project 拥有活动引用的最小不可变产物：`form_plan`、`section_contract`、`diagnostic_report`，门槛只引用内建 validator；
6. 保存 Run/Invocation 审计记录和实际指示内容引用，验证失败、取消、自动修正和重启后的一致性；
7. 继续由单 Agent 执行 workflow，用多个代表性真实作品验证工作方式、产物和冲突规则；
8. 再实现只读 Critic fork 原型，通过运行时 `ExecutionGrant` 裁剪工具和产物范围，与单 Agent 基线对照；
9. 只有多任务实验产生稳定收益后，再增加更多角色、用户级 workflow Skill 或多 workflow 合并能力。

不应在第一步引入通用工作流引擎、后台队列、分布式执行、多候选分支或完整 Multi-Agent 框架。

## 完成判定

- 用户能查看每次运行实际生效的核心协议、Skill、偏好、Role、能力和冲突；
- 内建不变量不能被用户 Skill、项目偏好或角色覆盖；
- 用户可在个人或项目作用域定义并启用 Advisory Skill，未启用 Skill 不进入完整上下文；
- 同时只启用一个 workflow Skill，多个 workflow 请求会显式冲突，不依赖文本顺序暗中合并；
- 相同 workflow Skill 能在单 Agent 和多角色执行映射之间复用，无需复制或改写工作流声明；
- Project 持有当前活动产物引用，runs 和不可变内容对象不会成为第二套当前项目状态；
- fork 只接收声明的产物、指示快照和 `ExecutionGrant`，运行时实际工具及项目写操作不超出 grant；
- 接受候选不暴露为模型工具，必须由用户显式授权后由 `Application` 执行；
- 历史运行保存足以审计和重建每次模型调用输入的实际内容引用，而不只保存无法恢复内容的摘要哈希；
- 使用多个代表性音乐任务、重复运行和盲听评价对比单 Agent 与 Composer + 只读 Critic，同时记录质量、失败率、成本和延迟；
- 未产生明确对照收益前，项目继续使用当前轻量单 Agent 编排。

## 待验证问题

以下问题仍未形成最终结论：

- 项目和用户是否真的需要自定义 workflow Skill，还是内建 workflow 加 Advisory Skill 已足够；
- `form_plan`、`section_contract` 和 `diagnostic_report` 的最小结构哪些字段能被真实校验器消费；
- 不可变产物与内容对象是否带来足够审计价值，还是对当前单用户 CLI 过度复杂；
- 只读 Critic 能否在盲听评价中稳定提升作品质量，以及收益是否足以抵消额外成本和延迟；
- 多个 workflow Skill 的组合是否会成为真实需求。在证据出现前，暂不设计通用合并算法。
