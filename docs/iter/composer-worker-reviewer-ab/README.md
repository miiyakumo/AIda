# Composer–Worker–Reviewer 真实 A/B

> 状态：薄 Harness 已实现并完成一次成功配对运行；两份完整 WAV 已可人工盲听，尚未得出音乐质量胜负，
> 也尚未达到接入正式流程的门槛。

## 文档索引

[稳定需求](requirements.md)
description：定义角色职责、Alda 原生边界、时间精度、验收条件与明确非目标。

[最终设计](design.md)
description：以 Alda 为唯一片段语言，设计薄 Harness、预算编译、Worker 提交、验证和 A/B 接入门槛。

[固定 A/B 任务](ab-task.txt)
description：首轮与后续同题实验使用的五分钟东方风格叙事作曲原始任务。

## 实验问题与边界

同一个约五分钟的东方 Project 风格 Boss 战任务按固定顺序运行两个实验臂：

- `baseline`：现有单 Agent 直接完成整首 Alda、校验与渲染；实验入口要求它最终提交 candidate，Plan、Answer
  或 Draft 不会被误当成完成。
- `roles`：Composer 只声明音乐设计和段落相对权重；Harness 确定性编译只读预算；`theme` 与
  `development` 两个 Worker 各自实现所属段落的全部声部；Harness 用固定模板组装普通 Alda；只读
  Reviewer 最后审查。

实验入口是独立的 `composition-ab` 二进制，不修改正式 CLI、`Application`、`Project`、工作乐谱或版本事实。
Alda 仍是唯一的音乐片段和时间线语言；Harness 不续写、补拍、改和声、改配器或建设第二套音乐 IR。

## 当前实现

Composer schema 已移除绝对 `duration_beats` 和 Anchor，只保留 tempo、meter、乐句网格、段落权重、动机、
和声、织体与材料计划。预算编译器按乐句网格和最大余数法分配每段拍数，对相同输入产生稳定结果。

Worker 收到每段的 `phrase_beats`、`repeat_count` 和必需 body 形状，直接提交受限的原生 Alda sequence。
重复乐句必须用绝对 `oN` 重置音区；`<`、`>` 被实验边界禁止，避免相对音区在 repeat 间累积。每个 Worker
家族和最终合并谱都经过真实 `alda parse` 与临时 Marker 时间探针。技术验证失败最多反馈一次；Harness 不会
代替 Worker 修补音乐。

Reviewer 会收到 Composer 计划、预算、最终 Alda 和真实解析摘要，但模型侧只暴露一个 `submit_review` 工具。
它能批准或提出带段落证据的阻断问题，不能写文件、改源码、接受版本或触发艺术返工。

## 失败演进证据

失败目录全部保留，没有用后续成功覆盖：

| 运行 | baseline | roles | 暴露的问题 |
|---|---|---|---|
| 首轮 | 成功 | Composer 前失败 | Composer 被要求手算绝对拍数，恢复后仍只得到 37.14 秒时间线。 |
| `v2` | 成功 | 失败 | theme Worker 的结构化结果缺失 `parts`，唯一一次协议恢复后仍无效。 |
| `v3` | 成功 | 失败 | theme Worker 没有调用提交工具，唯一一次协议恢复后仍未形成结果。 |
| `v4` | 失败 | 失败 | baseline 把 Plan 当结束；Worker 未按精确乐句网格填满预算。 |
| `v5` | 成功 | 失败 | theme 首次通过；development 的 `>` 在 17 次 repeat 间累积，最终 MIDI 音高越界到 130。 |
| `v6` | 未完成 | 未完成 | 外部进程中断，只留下 Composer 与预算；没有顶层报告，不计作模型成败。 |
| `v7` | 成功 | 成功 | required-candidate、显式乐句 repeat 和绝对音区约束后首次形成完整配对产物。 |

这些修正仍是通用的实验边界：它们公开已有预算、约束 Alda 状态语义和要求完整候选，没有让 Harness 创作或
针对固定旋律硬编码。

## v7 成功配对

运行命令：

```text
cargo run --manifest-path alda-agent/Cargo.toml \
  --bin composition-ab -- \
  --file docs/iter/composer-worker-reviewer-ab/ab-task.txt \
  --duration 300 \
  --output alda-agent/target/composition-ab-20260818-v7 \
  --config-root .
```

模型为 `deepseek-v4-flash`，顺序固定为 baseline 后 roles。顶层事实保存在
`alda-agent/target/composition-ab-20260818-v7/report.json`。

| 指标 | baseline | roles |
|---|---:|---:|
| 成功 | 是 | 是 |
| 总耗时 | 299.11 秒 | 81.52 秒 |
| 模型调用 | 13 | 6（Composer 2、Worker 3、Reviewer 1） |
| 工具往返 | 12 | 不适用；角色只提交结构化结果 |
| 协议恢复 | 1 | 2（Composer 1、development 1） |
| Worker 技术返工 | 不适用 | 0 |
| 计划时长 | 模型自行计划 | 297.143 秒 |
| Alda 可听事件时长 | 301.077 秒 | 297.071 秒 |
| WAV 时长 | 303.279 秒 | 299.752 秒 |
| 声部 / 可播放事件 | 9 / 2631 | 4 / 3191 |
| WAV RMS | 0.007188 | 0.010233 |

两个 Worker 都在首个通过结构协议的音乐提交上通过真实 Alda 和精确时间线，没有使用唯一一次技术返工额度。
roles 的 4 段 × 4 声部共有 32 个段首/段尾检查点，最大 Marker 误差约为
`2.68e-9 ms`。两份正式 Alda 均再次独立执行 `alda parse` 成功；两份 MIDI 与 WAV 非空，WAV 为 44.1 kHz
双声道且非静音，时长均在 300 秒 ±10% 容差内。

Reviewer 批准了 roles 产物，同时保留两个非阻断观察：第二段的主题链接不够明显，末段计划中的皮卡第三度
不够清楚。Reviewer 的批准、Marker 对齐、事件数与 RMS 都只说明流程和可检查事实，不证明音乐质量。

## 最终工程验收

2026-08-18 对当前工作树和 v7 保存产物完成逐条复核：

- Composer schema 与 `composer.json` 均不含 `duration_beats` 或 Anchor；预算编译的网格取整、最小分配和稳定
  最大余数顺序有确定性单元测试。
- 组装器的确定性、短片段、长片段、缺失声部、Marker 注入、命名引用、宿主 sequence 逃逸、tempo 篡改和
  时间回跳边界均有测试；真实 Alda 测试会核对临时 Marker，而不是自行模拟事件时长。
- v7 的两个 Worker 技术返工均为 0；最终角色臂有 32 个段首/段尾检查点，且报告保存了各角色调用和协议恢复。
- 两份正式 Alda 再次独立执行 `alda parse` 成功；六个 Alda/MIDI/WAV 文件均非空并计算了 SHA-256。
  外部音频探针确认 WAV 分别为 303.279 秒和 299.752 秒、44.1 kHz、双声道；宿主音频分析确认二者非静音。
- `cargo test --manifest-path alda-agent/Cargo.toml` 全量通过，包含 175 个库测试、9 个实验入口测试和全部集成
  测试；`cargo clippy --manifest-path alda-agent/Cargo.toml --all-targets -- -D warnings` 通过。

上述验收完成的是技术试验，不包含尚未发生的人工听感比较，也不把 Reviewer 观察升级为用户艺术结论。

## 当前结论

本轮已经证明这条最小角色路径能够端到端形成完整候选，并且消除了首轮 Composer 绝对拍数失败、Worker
乐句长度误解和 repeat 音区累积三类技术阻塞。单次运行中 roles 的模型调用和墙钟时间也低于 baseline，
但运行顺序固定、样本只有一个，不能据此推断稳定成本优势，更不能推断听感更好。

两份 WAV 现已具备完整人工 A/B 条件：

- `alda-agent/target/composition-ab-20260818-v7/baseline/score.wav`
- `alda-agent/target/composition-ab-20260818-v7/roles/score.wav`

下一步由用户完整试听并记录偏好；在讨论接入正式流程前，还必须用不同曲式或拍号的另一项长篇任务验证没有
针对首题过拟合。若完整试听与后续运行没有可测收益，应撤回角色入口，只保留独立有价值的 Alda 校验改进。
