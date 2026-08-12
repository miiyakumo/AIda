# Coding Agent 的工作区与会话关系

> 调研日期：2026-08-13
>
> 范围：本地参考源码中的 Codex CLI、Claude Code 和 Grok Build

## 调研问题

确认成熟 coding agent 如何初始化交互、工作目录与会话是什么关系，以及哪些做法适合 alda-agent 的
Project 模型。本调研只支撑会话边界决策，不比较完整 TUI 架构。

## 共同模式

三者都没有在启动时要求用户先完成一段独立的“项目素材录入”：

1. 启动时先确定 cwd 或 workspace；
2. 默认建立空的新会话并显示普通输入界面；
3. 第一条用户 prompt 启动实际 Agent 工作；
4. 会话单独持久化，需要时按 ID、标题或“当前目录最近会话”恢复。

因此工作区是会话上下文，不等于会话本身。同一目录可承载多条彼此独立的任务会话。

## 项目证据

### Codex CLI

- TUI 的启动选择显式区分 `StartFresh`、`Resume` 和 `Fork`；没有恢复目标时进入全新会话。
- 恢复选择器可以按 cwd 过滤，说明会话关联工作目录，但不由工作目录唯一标识。
- 启动参数可携带 `initial_prompt`，它在会话建立后提交，不是工作区初始化字段。

相关实现：

- `ref/codex/codex-rs/tui/src/resume_picker.rs`
- `ref/codex/codex-rs/tui/src/lib.rs`
- `ref/codex/codex-rs/tui/src/app.rs`

### Claude Code

- 无参数启动默认为新的交互会话，可直接用位置参数传入首条 prompt。
- `--continue` 恢复当前目录最近的 conversation；`--resume` 按 session ID 恢复或打开选择器。
- `--session-id` 与 `--name` 都表明会话有独立身份，不由 cwd 唯一决定。

相关实现：

- `ref/cloud-code/claude-code-source/src/main.tsx`
- `ref/cloud-code/claude-code-source/src/screens/ResumeConversation.tsx`

### Grok Build

- 每次启动 TUI 默认创建新 session，`/new` 可在进程内开始另一条对话。
- `/resume` 列出当前 workspace 的近期会话；CLI 的 `--resume` 与 `--continue` 分别恢复指定会话和当前目录
  最近会话。
- 会话按 cwd 分组存储，每条会话有独立 ID、消息、工具结果和恢复数据。

相关实现和说明：

- `ref/grok-build/crates/codegen/xai-grok-pager/docs/user-guide/17-sessions.md`
- `ref/grok-build/crates/codegen/xai-grok-pager/src/app/session_startup.rs`

## 对 alda-agent 的取舍

alda-agent 的 Project 不等同于 coding agent 的普通 cwd。一个代码目录可以包含许多无关任务，而一个
Project 已经代表一首作品的设置、有效版本和连续修改生命周期。

首版因此采用更窄的关系：

```text
Project 1 ── owns ── 1 Conversation
```

- 创建 Project 后直接进入普通对话输入，不再启动素材向导；
- 第一条自然语言消息成为对话首条消息和首次创作请求，不另存重复字段；
- 打开 Project 自动恢复其唯一对话；
- Project 是聚合根，对话使用供应商无关的消息类型并与项目原子持久化；
- 不引入 session ID、`/new`、`/resume` 或会话选择器。

这没有照搬三个参考项目的一对多模型，因为当前没有在同一作品下并行多个独立创作方向的需求。如果以后
真实出现多方案探索、分支创作或多人协作，再把 Conversation 提升为带 ID 的独立实体；当前不预留命令和
存储框架。
