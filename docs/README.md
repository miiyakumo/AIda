# Alda Music Agent 文档中心

> 本目录描述一个面向音乐创作的垂直 Agent Harness。当前仓库处于“研究与设计完成、实现尚未开始”的阶段；`ref/` 中的 Codex 与 Alda 是上游参考源码，不是本项目实现。

## 状态说明

| 范围 | 状态 | 含义 |
|---|---|---|
| 产品需求 | Draft | 已定义产品目标、P0–P2 范围和验收场景，待决策项尚未冻结 |
| MVP 概要设计 | 已批准进入切片 A | 已冻结正式 MVP 范围、统一 Local Service、协议与验收边界；后续决策按截止点冻结 |
| Alda / Codex 源码调研 | 第一轮完成，快照已核验 | 结论可追溯到仓库内固定快照，外部链接未在线探测 |
| 基础 Harness 设计 | 已完成 | Agent Loop、Provider、工具、Session、压缩与评测已有设计 |
| 基础实施路线 | 已完成 | M0–M5 可按日推进，但仓库尚无 `alda-agent/` 代码 |
| 进阶能力研究 | 已完成并经独立质询 | MewCode 能力、Codex 机制与音乐领域差异已建立映射 |
| 进阶架构设计 | Conditional Go 修订版 | 已完成独立审议；扩展执行仍必须满足文档中的隔离前置条件 |
| 进阶实现 | 未开始 | 文档中的 Rust 类型和协议是目标设计，不代表已编译实现 |

阅读时请区分以下标签：

- **事实**：由本地源码、已校验文档或公开官方资料直接支持。
- **设计决策**：本项目选择的实现方向。
- **探索性假设**：需要原型、消融实验或人类盲听验证，不能写成既成事实。

## 两条学习路线

### 路线 A：先完成最小可用音乐 Agent

适合第一次开发 Agent 的学习者。目标是先完成约 5000 行 Rust 的 `legacy-mvp`：单用户、双 Provider、四个音乐工具、可恢复会话和分层评测。它是进入 V2 的前置基线，不等于正式 `minimal` Profile。

1. [Harness Engineering 零基础教程](tutorials/harness-engineering-tutorial.md)
2. [Alda 与乐理教程](tutorials/music-theory-alda-tutorial.md)
3. [基础 Harness 设计](design/harness-design.md)
4. [M0–M5 实施路线](design/implementation-roadmap.md)

### 路线 B：从垂直 Agent 走向可信音乐创作平台

适合已经理解 Agent Loop、Tool、Session 和 Context 的读者。目标是加入音乐原生的版本、试听、偏好、扩展生态与协作能力。

1. [MewCode、Codex 与音乐 Agent 进阶能力调研](research/advanced-music-agent-capabilities.md)
2. [进阶音乐 Agent 架构](design/advanced-music-agent-architecture.md)
3. [M6–M12 进阶实施路线](design/advanced-implementation-roadmap.md)
4. [进阶能力实战教程](tutorials/advanced-music-agent-tutorial.md)

## 为什么不直接复制 Coding Agent

Coding Agent 通常围绕“文件修改是否通过测试”收敛；音乐创作则同时面对：

- 多个候选都可能有效，没有唯一正确答案；
- 语法正确不代表好听、符合风格或可演奏；
- 同一乐谱在不同音源、设备和片段下听感可能不同；
- 用户反馈必须绑定确切作品版本和试听记录；
- 文本 diff 不能解释和声、声部、动机和段落变化；
- 多 Agent 应产生受控多样性，而不是并行覆盖同一份字符串。

因此，本项目借鉴 Codex 的运行时设计，但将核心对象从“文件与补丁”替换为：

```text
CreativeBrief → ConstraintSet → ScoreRevision → RenderArtifact
        ↓              ↓              ↓                ↓
   创作意图        可验证约束      作品版本图       可追溯试听
                                             ↓
                                      ListeningFeedback
```

## 文档地图

### 需求

- [产品需求文档](requirements/product-requirements.md)

### 研究

- [Harness Engineering 综述](research/harness-engineering.md)
- [Codex Agent Loop](research/codex-agent-loop.md)
- [Codex Model Client](research/codex-model-client.md)
- [Codex Tool System](research/codex-tools.md)
- [Codex Session State](research/codex-session-state.md)
- [Alda Language](research/alda-language.md)
- [Alda Interfaces](research/alda-interfaces.md)
- [Alda Pipeline](research/alda-pipeline.md)
- [Music Theory](research/music-theory.md)
- [进阶能力调研](research/advanced-music-agent-capabilities.md)
- [CLI、Web 与 App 多端架构调研](research/client-surface-architecture.md)
- [进阶文档独立审议记录](reviews/advanced-docs-independent-review.md)

### 设计与路线

- [MVP 概要设计](design/mvp-design.md)
- [基础架构](design/harness-design.md)
- [基础路线](design/implementation-roadmap.md)
- [进阶架构](design/advanced-music-agent-architecture.md)
- [进阶路线](design/advanced-implementation-roadmap.md)

### 教程

- [Harness Engineering](tutorials/harness-engineering-tutorial.md)
- [Music Theory × Alda](tutorials/music-theory-alda-tutorial.md)
- [进阶能力实战](tutorials/advanced-music-agent-tutorial.md)

## 设计原则

1. **作品状态独立于聊天历史**：压缩对话不能改写作品真相。
2. **硬约束与审美偏好分离**：`parse` 成功和用户喜欢是两种证据。
3. **副作用按能力分类**：写文件、播放设备、联网和发布使用不同权限。
4. **工具输出提案，聚合根提交状态**：避免工具和 Agent 共享全局 `&mut Session`。
5. **先单 Agent 基线，再证明多 Agent 的收益**：多 Agent 不是默认正确答案。
6. **所有记忆带来源、范围和置信度**：一次局部意见不能自动升级为全局偏好。
7. **可重建不等于可重复生成**：事件与 Artifact 可恢复，但 LLM 输出不保证逐 token 重现。
8. **人类保留最终艺术决定权**：Agent 可以推荐，不能自行宣布作品“已经好听”。

## 推荐验收顺序

```text
最小闭环
  → 结构化 Brief 与约束
  → 不可变 Revision 与两阶段 Tool/权限
  → Take 与可追溯 A/B 试听
  → 声明式 Hook、Skill
  → MCP 与长期记忆
  → 隔离候选 Agent
  → 盲评、语义合并与产品化
```

不要在 `ScoreRevision`、`ConstraintSet`、`Audition` 三个基础模型尚未建立时先实现 Agent Teams；否则多个 Agent 只会更快地覆盖同一份乐谱文本。
