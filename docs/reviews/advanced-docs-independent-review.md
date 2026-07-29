# Alda 进阶文档独立审议记录

> 审议日期：2026-07-29  
> 审议方式：由未参与三路研究和两份主体文档撰写的独立 SubAgent 反向质询。  
> 初审结论：`Conditional Go`；M6 领域原型可继续，协议阻塞项关闭前 M9/M10/Studio 为 `No-Go`。

## 审议原则

- 以本地 Codex/Alda 固定快照、官方 Codex 手册和文档内部协议为证据；
- 不因多个 Agent 结论一致就判定正确；
- 优先寻找审批绕过、状态矛盾、无法落地的后端和不可验证的验收项；
- MewCode 只按用户提供的宣传材料审查，不推断其源码完成度。

## 阻塞项与修订

| 级别 | 初审发现 | 修订结果 |
|---|---|---|
| Critical | Tool `execute` 可能先产生副作用，之后才审批 | 改为 `resolve → ActionPlan → Permission → sealed AuthorizedPlan → execute(StagingCapabilities) → verify/CAS`；动态 target/args 变化使授权失效 |
| Critical | Hook/Plugin/stdio MCP 只有 hash/Effect，没有真实执行隔离 | Advanced 只允许运行声明式白名单 Hook；stdio MCP 必须进受限进程/容器；命令扩展、写入 MCP 与 DAW 延至 Studio Extension Host |
| High | Alda 不原生生成 WAV，却要求 Audio Render/Audible Diff | 分为原生 `MidiRenderArtifact` 与需另选离线 synth/录音器的 `AudioRenderArtifact`；import/export 明确为 Agent Adapter |
| High | 不可变 Revision 内含可变 status | 删除实体 status，另建 `RevisionLifecycleProjection`，readiness 与 lifecycle 分离 |
| High | legacy H0–H3 与 V2 H0–H7 同名换义 | 增加 `alda-eval/legacy-v1`、`alda-eval/v2` 和 H2/H3 → H4/H5 映射 |
| High | 无 marker/meter 时仍假定 Section/Beat 可定位 | 增加 `NeedsSectionMapping`、`UnknownMeter`；未映射时降级 WholeScore/可靠 Part/MarkerRange |
| High | 只接受“完整播放后的 HumanEvidence” | 拆分 Listening/ScoreReview/PerformanceTest；部分或用户停止的试听记录 played range，反馈不得越界 |
| High | Critic/Analyst 被迫返回 MusicPatch | 改为角色化 `AgentResult` tagged enum；只有 Candidate/Integrator 可携带 Patch |
| High | Profile 与里程碑冲突 | `legacy-mvp` 仅作兼容标签；minimal=M6–M8，advanced=M9–M10，studio=M11+ |
| High | GC 与“完整 Replay”冲突 | 定义 strong/weak ref、replay horizon、retention 和 tombstone；窗口外不承诺 blob 存在 |
| High | Memory forget 可能只是 UI 隐藏 | Rollout 只存 ID/hash；正文独立加密，forget 删除索引并销毁密钥，备份按期清除 |
| High | 威胁模型遗漏 LLM Provider/Audio Critic | 增加 `ModelEgress`、字段最小化、Provider 政策披露、本地选项和 Audio 独立 opt-in |
| High | 路径规范化未覆盖 TOCTOU/控制面自修改 | 增加 root capability/no-follow/openat 或 inode 重验；控制面仅允许专用宿主命令与更高审批 |

## 其他质询与修订

- 多 Agent 评测拆成等经济成本与等墙钟实验，展示质量—成本—延迟 Pareto；
- CreativeBudget 增加货币、输入/输出、Render CPU、Artifact bytes 和子 Agent reservation；
- MCP 增加协议版本、initialize/capability negotiation、JSON-RPC 错误、断线与幂等规则；
- MUSIC.md 目录层叠只用于多文件，单文件局部规则使用 MusicalAddress；
- 教程 Skill manifest 补齐输入输出、许可、文化范围与 eval cases；
- Future Team 补充 TeamId、任务 DAG/mailbox、预算、权限继承、恢复与停机边界；
- 工期明确为低置信度基线，安全、人类研究、跨平台音频与文档维护单列。

## 经质询后仍成立的设计

- Take/Revision 比 Git Worktree 更适合作为音乐用户心智模型；
- 作品与 Artifact 必须独立于聊天 compaction；
- Parse、启发式、Audio Critic 与人耳是不可互相替代的证据；
- Human 保留 Accept/Publish，生成者不能自证审美与授权；
- 多 Agent 默认关闭，必须与单 Agent 做公平对照；
- 不承诺专业实时伴奏，不把相似度当法律结论；
- 取消树、资源锁、CAS、失败候选隔离的方向合理；
- 当前无 `alda-agent/` 代码，设计片段不能写成编译通过的实现。

## 复核门

以下四项必须在真实代码中再次验证，文档修订不能替代实现证据：

1. 未授权 `execute` 在类型和运行时都不可达，内建工具无旁路直接 I/O；
2. 非托管扩展的文件、环境、网络和设备能力由 OS/容器实际约束；
3. 选定离线音频后端后，Render manifest 足以解释同一/不同 hash；
4. forget、GC 与 replay horizon 在备份恢复和故障注入下符合承诺。

