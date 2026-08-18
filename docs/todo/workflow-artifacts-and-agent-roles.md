# Subagent 委派 A/B 验收

> 实现记录：[按需 Subagent 委派](../iter/on-demand-subagent-delegation/README.md)
>
> 状态：最小委派能力已实现；真实长篇作曲收益待验收

## 当前判断

第二次五分钟创作在 Marker、form_plan 和 checkpoint 落地后仍用了 20 次模型调用、19 次工具往返和 3 次
协议恢复。一次运行同时承担音乐设计、拍数核算、Alda 编码、语法恢复和提交协议，已经足以验证独立委派
是否能降低职责切换，但不足以证明固定角色或通用 Multi-Agent 平台有价值。

当前实现只提供 `delegate(task, context?)`。Composer 自己决定是否调用、调用几次以及委派音乐设计、Alda
实现、问题分析还是只读复核；subagent 可查询文档、检查临时片段，并在项目会话中只读检查 work/current，
最终只把文本结果交回 Composer。Project 写入、完整候选检查、渲染和接受边界没有变化。

## 验收方法

选择至少两个等价的 3–5 分钟叙事作曲任务，对比历史单 Agent 基线或新的未使用委派运行与实际使用委派的
运行。每次保存：

- `model_calls`：包含 Composer 和 subagent 的所有真实模型调用；
- `delegations`：实际委派次数；
- `tool_turns`、`protocol_recoveries` 和 `submissions`；
- Alda 语法失败、跨段溢出、游标回跳和意外重叠等客观错误；
- `form_plan` 中的主题、发展和段落职责是否能在最终源码与听感中定位；
- 完整 WAV 的匿名人工 A/B 试听结论。

委派运行至少不能破坏当前硬边界：直接创作要求仍在同一次用户请求内形成候选，最终源码经过现有检查，
用户仍完整试听并显式接受。静态指标不能代替音乐质量判断。

## 保留或收缩条件

若委派能在总模型调用相近或更低时减少协议恢复和 DSL 错误，或在相近成本下获得稳定更好的匿名试听结果，
保留当前按需能力。若 Composer 很少使用、上下文转述抵消收益，或总调用增加而质量无改善，则先调整提示或
撤回入口，不通过增加角色、调度和产物系统来掩盖负收益。

## 明确不做

- 不固定 Intro/A/A2/Coda 与 B/C/D 等段落家族；
- 不预设 Worker 或 Reviewer 类型；
- 不建立 DelegationPlan、段落覆盖、依赖图或任务分配校验；
- 不允许 subagent 直接修改 Project、再次委派，或调用白名单以外的提交、补丁、渲染、播放等宿主工具；
- 不建设 Workflow DSL、Provider 插件树或通用 Multi-Agent 平台；
- 不在真实 A/B 前宣称 subagent 已改善质量或成本。
