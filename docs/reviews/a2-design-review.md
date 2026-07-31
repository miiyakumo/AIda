---
verdict: revise
scope: design
artifact: /home/mii/code/draft/docs/plans/mvp-deliberative-execution.md
---

# A2 独立设计审查

## 结论

A2 需要修订后再实施。当前 A1 基线健康，A2 也正确限定为进程内协议
fixture，没有提前执行 Provider、文件或播放副作用；但 A2 把用于补齐创作信息的
question 与权限 consent 混为一体，并且没有冻结足以重建“回答/审批决定”事实的
wire/event 字段。这两处若按现稿实现，会让客户端行为含混，并在 B/C 引入 replay
与真实授权时改动已经宣称“生产兼容”的协议。

本结论不因 A2 尚未实现 B–E 而否决；仅审查 A2 自身和其明确承诺的演进边界。

## 重大问题

### 1. 固定 question 实际上是第二个 Model Egress approval

**位置**

- `docs/plans/mvp-deliberative-execution.md:251-266`
- `docs/plans/mvp-deliberative-execution.md:277-288`
- 对照 `docs/requirements/product-requirements.md:142-145`
- 对照 `docs/requirements/product-requirements.md:209-216`
- 对照 `docs/design/mvp-design.md:219-225`
- 对照 `docs/design/advanced-music-agent-architecture.md:457-482`

**实际证据**

A2 规定固定问题“用于确认将创作描述发送给配置的模型 Provider”，随后又创建
`EffectClass::ModelEgress` 的固定审批；流程对任意非空 `answer` 都直接进入后一个
审批，只有 approval 才有 `Approve | Deny` 决定。于是：

```text
question: “是否发送给 Provider？” → answer: “不发送”
                                      → 仍创建 ModelEgress approval
```

PRD 的 question 用于影响 Hard Constraint 的未知项（FR-03），approval 用于副作用
前权限决定（FR-41/42）；MVP 设计也把模型外发明确归类为 `ModelEgress` 权限。
A2 的 fixture 因而不是在测试两个不同协议概念，而是在用两种 wire 对象表达同一次
同意。

**影响**

- 客户端无法稳定区分“补齐任务输入”和“授权副作用”，可能把普通回答错误地当作
  consent，或向用户连续展示两次相同授权。
- question 的否定回答没有闭合转换；任意文本（包括“不发送”）都会推进审批。
- 后续 C 若以这一状态机接入真实 Provider，必须重新定义 question 的含义或增加
  分支，破坏 A2 所称的生产兼容协议和测试。

**最小修复方向**

把两个 fixture 分成真正不同的语义：question 询问一个不会授予权限的创作未知项
（最好是影响 Brief/Hard Constraint 的有界选择），其回答只补齐输入；随后独立发起
Model Egress approval。为有 choices 的 question 明确响应规则（例如 choice ID 与
是否允许自由文本），并测试无效 choice 不改变状态。不要让 question 承载 consent。

### 2. 待决对象和 resolved 事件没有保存决定内容，无法形成可重放事实

**位置**

- `docs/plans/mvp-deliberative-execution.md:236-249`
- `docs/plans/mvp-deliberative-execution.md:275-296`
- 对照 `docs/design/mvp-design.md:173-188`
- 对照 `docs/design/mvp-design.md:190-207`
- 对照 `docs/design/mvp-design.md:257-266`

**实际证据**

A2 为 `PendingQuestion` 只列出 prompt、choices、status、创建/解决 sequence；为
`PendingApproval` 只列出展示 payload、status、创建/解决 sequence。两者都没有：

- question 的实际 answer；
- approval 的实际 decision；
- 作答/决定的 actor/client；
- resolved event 必须携带哪些完整字段。

事件只命名为 `QuestionRequested/Resolved`、`ApprovalRequested/Resolved`，没有冻结
payload。可是权威设计明确把审批决定和 PendingQuestion/PendingApproval 视为必须写入
事实日志的权威事实，并要求 B 从 Session Rollout replay。删除投影后，仅凭
`status = Answered/Approved/Denied` 和 sequence 无法恢复用户到底回答了什么，也无法
审计谁作出决定；若 requested event 也只携带 ID，甚至无法重建原 prompt、choices 或
approval payload。

**影响**

- snapshot 虽能显示“已解决”，却不能恢复回答内容或审批决定来源，不能支持真实
  Agent 在 question 后继续，也不能满足审批审计。
- B 无法按现有 DTO/event 定义完成删除投影后的 replay；届时必然扩展或替换 A2
  已冻结的 wire/event schema。
- 同 command ID 的 reply 幂等只能证明响应不重复，不能证明 Session 事实完整可恢复。

**最小修复方向**

在 A2 设计中先冻结最小事实 schema：requested event 含完整不可变请求数据；
resolved event 含 `question_id/approval_id`、规范化 answer/decision、responder
identity 和 sequence（必要时含对应 request digest）。snapshot 的 resolved 对象应
投影这些字段。明确 B 只改变 durability，不补造 A2 已遗失的事实字段，并增加
“从 A2 内存事件清空投影后 replay 得到同一 snapshot”的确定性测试模型；A2 可仍不写盘。

### 3. approval wire 没有绑定被批准的精确计划，无法安全演进到 C

**位置**

- `docs/plans/mvp-deliberative-execution.md:228-245`
- `docs/plans/mvp-deliberative-execution.md:264-266`
- 对照 `docs/design/mvp-design.md:227-242`
- 对照 `docs/design/advanced-music-agent-architecture.md:374-403`
- 对照 `docs/design/advanced-music-agent-architecture.md:486-498`

**实际证据**

A2 宣称冻结“生产兼容的 wire DTO”，但 `PendingApproval` 仅以五个展示字段
`action/effect/target/scope/estimated_impact` 标识审批对象。正式 Tool V2 则要求
授权绑定 plan hash/args、动态 target、Effect、策略版本、scope 和 expiry，并在参数
或资源身份变化时使计划失效。A2 明确不实现 `AuthorizedActionPlan` 是正确的边界，
但其 wire 对象也没有不可变 `action_plan_digest`（或等价的 approval subject
reference）来表示“人类批准的是哪一个精确计划”。

展示字符串相同不代表计划相同；例如发送字段或 endpoint 改变后，五个字符串可以仍
显示同样摘要。`approval_id` 只能标识审批记录，设计没有规定它必须绑定并校验哪个
plan digest。

**影响**

- C 无法证明批准决定只适用于用户看到的精确 action plan；审批后参数替换会成为
  TOCTOU/授权错配风险。
- 要满足两阶段 Tool V2，C 必须给 A2 wire/event/snapshot 增加关键关联字段，造成
  协议返工。

**最小修复方向**

A2 不必实现 Permission Broker 或 sealed plan，但应在协议中预留并实际使用不可变的
`approval_subject_digest`（fixture 可由规范化 Fake plan 计算），并规定 requested、
resolved event 与 snapshot 都携带同一 digest。展示 payload 与 subject digest
分工：前者供人阅读，后者供 C 校验精确绑定。策略版本、expiry 可由 C 增加，但不要
把展示摘要当授权身份。

## 已尝试证伪但未发现阻塞问题

- **副作用前审批**：A2 明确不调用 Provider、不写文件、不播放；approve 只推进 Fake
  状态机。因此现稿本身没有发生“审批前执行副作用”。需要修的是 question/approval
  语义，而不是把 Fake fixture误判成真实外发。
- **取消闭合**：单写 actor 前提下，任一待决阶段取消后把仍 Pending 的所属对象置为
  `OwnerTurnAborted`，后续 respond 返回 typed error，方向闭合。实现时仍应测试
  cancel/respond 的两种队列顺序。
- **业务幂等**：同 ID+digest 返回原 reply、新 ID 对 resolved 对象返回
  AlreadyResolved 且不追加事件，设计与 A1 已验证的区分一致。
- **未提前伪实现 C**：把 policy、sealed plan、approval cache 留给 C 是合理切片边界；
  问题 3 只要求冻结跨切片关联字段，不要求 A2 实现 C 的授权器。

## 验证证据

完整阅读了指定需求、MVP 设计、进阶架构的 Tool/权限/审批部分、实施计划全文与
`alda-agent` 全部手写源码和测试。当前 A1 机械基线：

```text
cargo fmt --check                          PASS
cargo clippy --all-targets -- -D warnings PASS
cargo test                                PASS (11 tests)
```

这些通过只证明当前 A1 未回归；A2 尚未实施，不能用它们抵消上述设计问题。
