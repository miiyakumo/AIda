# Coding Agent 的终端信息分层

> 调研日期：2026-08-13
>
> 范围：本地参考源码中的 Codex CLI、Claude Code 和 Grok Build

## 调研问题

alda-agent 当前在每次提示符前连续打印项目摘要和下一步状态，会话输出、运行阶段、播放状态、项目上下文
和输入提示都落在同一滚动平面。真实长会话中，用户难以区分“已发生的对话”“正在发生的操作”“当前
项目事实”和“下一次输入入口”。本调研确认成熟 coding agent 如何划分这些信息，并给出适合当前
reedline 架构的最小演进路径。

## 共同模式

三个项目虽然渲染技术不同，但都把界面概念分成四层：

1. **会话历史**：用户消息、模型回复和已经完成的工具结果进入可滚动、可回看的主区域；
2. **活动状态**：spinner、当前工具和等待原因靠近输入区显示，操作结束后转成简短结果，避免反复污染历史；
3. **输入区**：多行 composer 固定在底部，有明确边界、前缀或背景，与上方输出分离；
4. **环境上下文**：模型、模式、目录、权限、token 等压缩为一行状态或按需详情，不在每轮重复完整打印。

核心不是使用全屏 TUI，而是让不同生命周期的信息拥有不同视觉位置：历史是永久的，活动状态是临时的，
项目上下文是稳定但可变化的，输入区始终是下一步操作入口。

## 项目证据

### Codex CLI

Codex 使用 `ChatWidget` 管理已提交的 transcript cell 和可变的流式 active cell；两者都属于会话主区域。
独立的 `BottomPane` 持有 `ChatComposer`、临时弹层、任务状态和 footer。运行状态显示在 composer 上方；流式
模型文本出现时会隐藏重复的状态行，避免同时出现两种“正在工作”提示。项目、模型和上下文等放在可配置
状态行，不成为对话历史。

相关实现：

- `ref/codex/codex-rs/tui/src/chatwidget.rs`
- `ref/codex/codex-rs/tui/src/bottom_pane/mod.rs`
- `ref/codex/codex-rs/tui/src/bottom_pane/chat_composer.rs`
- `ref/codex/codex-rs/tui/src/bottom_pane/footer.rs`

### Claude Code

Claude Code 的全屏布局把 `Messages` 放入可滚动区域，把 `PromptInput` 放在 `bottom` 固定区域；权限等临时
交互也固定在底部，不随流式消息上下跳动。spinner 位于消息区尾部、输入区上方。`PromptInputFooter` 再承载
状态行、快捷提示和通知，并根据终端宽高降级：窄屏改为纵向排列，短屏优先隐藏可选状态行。

相关实现：

- `ref/cloud-code/claude-code-source/src/screens/REPL.tsx`
- `ref/cloud-code/claude-code-source/src/components/FullscreenLayout.tsx`
- `ref/cloud-code/claude-code-source/src/components/PromptInput/PromptInput.tsx`
- `ref/cloud-code/claude-code-source/src/components/PromptInput/PromptInputFooter.tsx`

### Grok Build

Grok Build 的分区最明确：`AgentViewLayout` 分配 scrollback、单行 turn status、多行 prompt 和 shortcuts bar；
输入区自身有边框或左侧强调线，底部信息行显示模型和模式。会话按结构化 block 渲染，用户 prompt 可作为
滚动中的 sticky section header。项目还支持在 scrollback 与 prompt 之间显式切换焦点，说明历史浏览与输入
编辑是两个独立交互面。

相关实现：

- `ref/grok-build/crates/codegen/xai-grok-pager/src/app/agent_view/render.rs`
- `ref/grok-build/crates/codegen/xai-grok-pager/src/views/turn_status.rs`
- `ref/grok-build/crates/codegen/xai-grok-pager/src/views/prompt_widget/mod.rs`
- `ref/grok-build/crates/codegen/xai-grok-pager/src/scrollback/`

## 对 alda-agent 的取舍

当前项目只有单线程音乐创作闭环，不需要立即引入 ratatui、alternate screen、焦点路由或结构化 scrollback。
这些机制会把简单 REPL 变成完整 TUI 状态机。保留终端原生滚动记录和 reedline 输入，可以先实现主要收益。

建议按以下顺序演进：

### 第一阶段：在现有 REPL 内建立视觉边界

- 项目摘要只在启动、配置/版本/播放状态发生变化后显示，不在空输入和每轮提示符前重复；
- 把摘要压缩为单行上下文，例如 `alda-agent · v6 · full · 播放 v2`；完整配置仍由 `/project` 查看；
- 输入前增加稳定的空行或弱分隔线，使用明确的 `›` 前缀；多行续行保持同一输入块；
- 活动 spinner 只占临时行；持久历史只保留用户消息、阶段结果、校验结果和最终成功/失败；
- 播放状态与当前版本分开表达，明确显示“当前 v6，正在播放 v2”，但不在每个历史块重复。

这一步不需要全屏重绘。它解决当前最明显的混杂问题，并保留终端原生复制、搜索和滚动行为。

### 第二阶段：按真实需求决定是否使用固定底部 composer

只有当用户明确需要“滚动历史时输入区始终可见”“运行中继续编辑/排队下一条消息”或“可交互状态栏”时，
再引入 ratatui/alternate screen。届时界面应保持四区，而不是继续往提示符周围堆文字：

```text
┌ 会话历史（可滚动） ───────────────────────┐
│ 用户要求、模型回复、工具与保存结果         │
└───────────────────────────────────────────┘
  ◇ Alda 校验 · 第 1/3 轮              状态
┌ 输入 ─────────────────────────────────────┐
│ › 多行修改要求                             │
└───────────────────────────────────────────┘
  alda-agent · v6 · full · 播放 v2     /help
```

状态行应保持低信息密度；配置缺失、认证失败等需要行动的问题临时提升到输入区上方，处理后消失。`/project`
继续是完整项目事实的唯一展开视图。

## 验收重点

- 连续五轮创作后，用户能一眼找到每条用户要求和对应版本结果；
- 空输入不会重复打印项目摘要；
- 当前版本、实际播放版本和活动阶段不会被误认成同一状态；
- 错误和警告有明确归属，但恢复后不继续占据每一轮提示符；
- 终端宽度不足时先隐藏帮助和次要上下文，不截断输入与主要状态。
