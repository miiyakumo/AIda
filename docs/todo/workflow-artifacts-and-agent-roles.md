# Workflow 产物与 Agent 角色派生

> 运行证据：[Composer–Worker–Reviewer 真实 A/B](../iter/composer-worker-reviewer-ab/README.md)
>
> 稳定需求：[长篇作曲角色实验需求](../iter/composer-worker-reviewer-ab/requirements.md)
>
> 最终设计：[原生 Alda 上的最小角色工作流](../iter/composer-worker-reviewer-ab/design.md)
>
> 状态：薄 Harness 与成功配对运行已完成；待人工 A/B、异题复验和接入决策

## 当前结论

长篇单 Agent 同时处理音乐设计、Alda、时长算术和协议恢复，已有真实运行证据支持职责隔离实验。简单拆角色
并不会自动成功：首轮 Composer 绝对拍数失败，后续 Worker 又暴露乐句长度与 repeat 音区状态问题。

当前实验已经把 Composer 收缩到音乐计划和相对比例，由纯 Rust Harness 编译只读预算；两个 Worker 按段落
家族提交原生 Alda sequence；固定模板组装完整乐谱；只读 Reviewer 审查。一次成功配对运行证明该路径可形成
完整 Alda、MIDI 和 WAV，并且没有建立 `CompositionSpec` 之上的 Cue、Placement、技法分析器或第二套音乐
运行时。

这仍不证明 roles 的作品更好，也不能用单次调用数和耗时证明稳定成本优势。正式创作流程继续使用单 Agent。

## 已完成的最小改动

1. Composer schema 已删除绝对 `duration_beats` 和 Anchor，使用全局 meter、乐句网格和段落相对权重。
2. Harness 用确定性最大余数法把目标时长量化为每段只读拍数预算。
3. Worker 只输出每段、每声部的原生 Alda sequence body，不能声明变量、part、Marker 或 tempo。
4. Harness 公开精确乐句拍数和 repeat 次数，固定生成普通 Alda variables 与 instrument tracks。
5. Worker repeat 使用绝对 octave 重置；实验边界拒绝会跨 repeat 累积状态的 `<`、`>`。
6. 每个 Worker 家族和最终合并结果都经过真实 `alda parse`、Marker 时间探针和逃逸检查。
7. Reviewer 只拥有一个结构化提交工具；不改源码、不持久化候选、不接受版本，也不触发艺术返工。
8. baseline 使用 required-candidate 入口，Plan、Answer 或 Draft 不会结束完整创作实验臂。

## 已验证的不变量

- 实验没有修改正式 `Application`、Project、工作乐谱、候选接受或版本流程。
- Project 仍是正式工作乐谱、恢复候选、版本和用户接受状态的唯一事实主体。
- 两个 Worker 按段落家族拆分，每个 Worker 对所属段落的全部声部负责。
- 同一输入的预算分配和模板顺序确定一致。
- 片段过短、过长、缺失声部、命名逃逸、Marker 注入、时间回跳和相对音区 repeat 会在渲染前失败。
- 用户只试听完整候选；内部段落不成为独立工作稿、版本或审批对象。
- 结构、时长、游标和语法由程序验证；音乐表达仍由模型观察与用户试听判断。

## 成功运行事实

`composition-ab-20260818-v7` 的 baseline 与 roles 都成功。roles 计划时长为 297.143 秒，真实 Alda 可听事件
时长为 297.071 秒，WAV 为 299.752 秒；4 个段落、4 个声部的 32 个 Marker 起止检查点最大误差约
`2.68e-9 ms`。两个 Worker 都在首个有效音乐提交通过，没有技术返工。两份 WAV 均非静音并落在 300 秒
±10% 范围内。

该结果只证明路径可行。Marker、事件数、RMS、Reviewer 批准和耗时都不等于音乐质量。

## 剩余工作

- 对 v7 两份完整 WAV 进行人工盲听，记录主题辨识、发展、织体、高潮、结尾与整体偏好。
- 用不同曲式或拍号的另一项 3–5 分钟任务复验，检查预算、Worker 契约和成本是否对首题过拟合。
- 综合两次运行的技术稳定性、调用成本和人工听感，决定接入正式流程或撤回角色入口。
- 若决定接入，再单独设计与 Project 候选事实的最小连接；当前实验产物不能直接成为正式版本。

## 明确不做

- 不建设通用 Multi-Agent、Workflow DSL、Artifact 仓库、Provider 插件树或多角色市场。
- 不建设 Alda 之上的函数、模块、Timeline DSL、声部调度器或音乐 IR。
- 不增加 Cue/Placement 通用机制或对位、和声、风格专用分析器。
- 不按乐器拆 Worker，不增加多个工作乐谱、版本分支或逐段审批。
- 不让 Harness 自动创作、补过渡、改和声、改配器或通过重复材料凑时长。
- 不在人工完整试听与异题复验前宣称角色流程改善了音乐质量。
