# 按需 Subagent 委派

> 状态：实现完成；真实长篇作曲 A/B 收益待验收

## 需求决策

长篇作曲已经证明单个 Agent 同时承担音乐设计、Alda 编码、语法恢复和提交协议时会发生明显职责切换。首版
subagent 的目标只是验证“按需委派一个独立任务是否有帮助”，不预设固定 Worker、段落分组、Reviewer、
DelegationPlan 或 Workflow 引擎。

Composer 自己决定是否委派、委派什么和调用几次；宿主只提供一次调用、返回结果的最小能力。subagent 结果
是当前生成上下文中的参考信息，Composer 负责判断、整合和最终提交，现有 Alda 校验与 Project 边界不变。

## 当前流程

```text
用户请求
  → Composer
  → delegate(task, context?)
  → 隔离的 subagent 模型循环
      ↔ lookup_alda_docs / inspect_alda_source(fragment)
      ↔ inspect_score(work|current，仅项目会话)
  → 文本结果作为 tool result 返回 Composer
  → Composer 继续调用现有检查或 submit_result
  → Application / Project 执行原有校验与持久化
```

`task` 是一个边界清晰、可直接执行的音乐设计、Alda 实现、问题分析或只读复核任务；`context` 只携带完成
任务所需的规格、约束、源码或待复核内容。宿主不会自动复制主对话、Project prompt、工作乐谱源码或其他
临时上下文。

subagent 使用与 Composer 相同的模型配置，并拥有最小只读工具集：始终可查 Alda 文档、检查
`scope=fragment` 的临时源码；项目上下文存在时可用 `inspect_score` 读取 work/current 的结构、检查结果和
源码哈希。`inspect_score` 不返回源码，片段检查不产生候选检查点。subagent 不能提交、再次委派、检查项目
补丁、渲染、播放或修改 Project。返回内容及是否因模型长度上限截断一并交给 Composer，最终产物仍必须经过
现有完整候选、`submit_result`、Alda 和音频链路验证。

## 调用与统计

一次 `delegate` 可以包含多次 subagent 模型请求和只读工具往返。两类模型请求共同受
`RunPolicy.max_model_calls` 限制；`model_calls` 记录所有真实请求，`delegations` 记录实际启动的委派次数，
`tool_turns` 同时记录外层 delegate 和内部实际执行的只读工具。若额度不足以启动 subagent 并保留一次 Composer
续写，宿主拒绝委派；内部最后一次额度仍请求工具时不执行该工具，把错误交给 Composer 收尾。终端摘要与
JSONL 控制面均公开这些数据，供后续 A/B 使用。

## 明确不做

- 不固定主题、发展或 Reviewer 等运行时角色；
- 不固定 Worker 数量，也不建立段落覆盖和依赖校验；
- 不建立 Workflow DSL、Provider 插件树或通用 Multi-Agent 平台；
- 不给 subagent 提交、递归委派、补丁、渲染、播放、Project 写权限或持久会话；
- 不自动采纳、拼接或持久化 subagent 返回内容；
- 不在真实 A/B 前宣称委派已经改善音乐质量或成本。

## 验证证据

自动测试覆盖：

- `delegate` 在没有 Project 工具上下文时仍可用；
- Composer 调用、subagent 调用和 Composer 续写形成完整三次请求闭环；
- subagent 请求只含专用 system prompt、显式 `task/context`，不含主对话；
- subagent 只携带最小只读工具，schema 与运行时均拒绝越权工具；
- 项目上下文存在时三个只读工具可连续调用并把结果带入下一次请求；
- `inspect_alda_source` 只允许 fragment，不能生成候选检查点；
- 返回结果进入 Composer 的后续模型上下文；
- 调用额度不足时不启动 subagent，并保留一次 Composer 续写；
- 总模型调用数、委派数、工具往返和提交数分别正确统计。

真实长篇作曲是否因此受益，继续由
[Subagent 委派 A/B 验收](../../todo/workflow-artifacts-and-agent-roles.md)跟踪。
