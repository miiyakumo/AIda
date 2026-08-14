# 可组合指示系统首期

> 需求来源：[可组合指示、Skill 与 Agent 角色派生](../../todo/composable-instructions-skills-and-agent-roles.md)
>
> 方案日期：2026-08-14
>
> 状态：代码实施完成，自动化验证通过，待真实模型任务验收

## 目标

首期建立单 Agent 的可组合指示闭环：每次模型调用前，把不可覆盖的核心协议、固定内建工作方式、显式启用的
用户/项目 Skill 和当前项目偏好编译成来源可查、顺序稳定、具有内容摘要与整体 fingerprint 的不可变值。
Skill 只影响模型输入，不能改变项目、校验、候选接受或版本写入权限。

## 首期边界

- 当前默认创作策略迁移为固定启用的 `builtin:progressive-composition` workflow Skill；
- 外部首期只支持 advisory Skill，来源为 `~/.alda-agent/skills/` 和 `<project>/skills/`；
- 项目只保存显式启用的限定 Skill ID，不复制 Skill 正文；
- 现有 mode、目标时长、包含和排除乐器是结构化项目偏好，也是 Alda 校验的同一事实来源；
- `/project skills` 负责发现和启停，`/project instructions` 展示当前有效快照；
- 交互 Agent 与一次性 `compose` 都必须消费编译结果，不能再注入任意 `creative_strategy` 字符串。

首期不实现外部 workflow、Skill 参数、references/templates/schema/script、通用工作流引擎、结构化产物、
Role/grant/fork 或 Run/Invocation 历史快照。当前快照用于解释当前配置，不承诺重建历史调用；自然语言规则只按
限定 ID 稳定组合，并明确标记为“未机械验证冲突”。

## 模块与数据流

```text
Project preferences + enabled Skill refs
                    |
SkillCatalog -------+--> InstructionCompiler --> CompiledInstructions
                                              |     fragments + digests
                                              |     resolved preferences
                                              |     diagnostics + fingerprint
                                              v
                                        Agent model request
```

`SkillCatalog` 负责限定 ID、发现、按需正文加载、路径与大小约束；纯编译器不读写项目。`Application` 组合两者并
在每次 Agent 调用前 fail closed。启用项缺失、损坏、越界或超限会阻止模型调用，但项目仍可打开，也可以继续
使用 `/project`、`/alda` 或禁用损坏 Skill。

## 合并规则

片段顺序固定为：核心协议、能力边界、内建 workflow、按限定 ID 排序的 advisory Skill、结构化项目偏好、
默认角色契约。核心协议和能力边界只能累加或收紧；外部 Skill 不参与覆盖。项目偏好直接来自 `Project`，同一份
值同时提供给提示和真实 validator。

相同内容字节和项目偏好必须产生相同 fingerprint；Skill 正文或任何有效偏好变化都必须改变 fingerprint。未启用
Skill 的正文不进入编译结果或模型上下文。

## Skill 格式与安全边界

外部 Skill 使用单层目录及 `SKILL.md`：

```yaml
---
name: lyric-writing
description: 为带歌词作品提供写作与结构建议
kind: advisory
---
```

目录名必须与 `name` 相同。加载器限制目录数、front matter、单文件和启用正文总字节数；规范化路径后拒绝越出
所属 Skill 根目录的符号链接。程序只把启用后的正文作为指示，不执行其中的脚本或工具声明。真正的状态和权限
不变量继续由 `Application` 与 `Project` 强制。

## 验收

- 同名 user/project Skill 以限定 ID 共存，不发生隐式覆盖；
- enable 后正文进入模型输入，disable 后消失，项目重启后引用保持；
- 损坏的启用 Skill 阻止模型请求而不阻止本地恢复操作；
- `/project instructions` 显示来源、摘要、结构化偏好、能力、角色、诊断与 fingerprint；
- Skill 无法绕过 Alda 校验，完整候选仍需用户显式接受才成为版本；
- Rust 格式化、Clippy、全量测试和 Rust 1.85 locked check 通过。
