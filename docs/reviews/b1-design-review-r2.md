---
verdict: revise
scope: design
artifact: /home/mii/code/draft/docs/plans/mvp-deliberative-execution.md
reviewer: B1 second-round fresh-context independent design reviewer
date: 2026-07-31
---

# B1 第二轮独立设计审查

## 结论

B1 的第二版已经实质闭合 R1 的 M1、M2、M3、M5，并补上 M4/M6 的大部分边界：
`ScoreId`、默认及额外 Take/Branch 创建事实、合法 cross-Take fork、minimal Profile
排除 Publish、`FixtureOnly`/`VerifiedDurable` 分层和独立 wire DTO mapper 都已写入。

但本轮仍不能批准实施，verdict 为 `revise`。两处 reducer 级矛盾会使验收无法按当前文字
实现：

1. R1 M4 要求的 waiver scope 判定没有真正闭合：`Constraint` 仍未保存 scope，却要求
   reducer 判断 waiver scope 是否覆盖它；
2. Candidate 要求 `VerifiedDurable`，B1 又没有可 replay 的 durable registration
   事实，只允许日志外 test capability 注入；这与“同一 reducer 从空事件 replay”及
   Draft→Candidate→Accepted 门禁互相冲突。

这两项不要求提前实现 B2 磁盘 I/O，也不要求扩大 B1 wire 写面；只需冻结一个自洽、可由
测试事件完整重放的领域契约。

## R1 M1–M6 闭合核对

| R1 项 | 判定 | 第二版证据 |
|---|---|---|
| M1 Score identity | 已闭合 | §13:709-712 增加 `ScoreId`；§13:731-738 规定一个 Project 的默认 Score，Revision、DAG、Artifact/Evidence 与 snapshot 均带 Score，parent 校验同 Project/Score。 |
| M2 Take/Branch 创建事实 | 已闭合 | §13:735-743 冻结 `ProjectInitialized` 的默认 Score/Take/Branch、`TakeCreated`、`BranchCreated`、fork base、空 head 与 unknown branch 语义；§13:766-768 纳入事件白名单；§13:835-836 有从空日志 replay 验收。 |
| M3 合法 cross-Take fork | 已闭合 | §13:739-742 允许新 Take/Branch 首次 Revision 引用显式 fork base，普通后续提交才限定当前 branch head；§13:823-826 用 `InvalidForkParent`/`UnsupportedMerge` 取代笼统 CrossTake 拒绝；§13:833-834 覆盖正反例。 |
| M4 人类 Waiver/Accept | **部分闭合，仍阻断** | §13:755-760 已补 human actor、reason、waiver scope、timestamp、constraint/revision、Accept metadata 与非人类 fail-closed；但 Constraint 本身没有 scope，详见 M1。 |
| M5 Publish 越界 | 已闭合 | §13:744-746 明确没有 Published 状态、事件或命令；白名单无 `RevisionPublished`；§13:839-841 明确验收无 Publish surface。 |
| M6 Artifact/wire 隔离 | **部分闭合，仍阻断** | §13:787-796 已区分 FixtureOnly/VerifiedDurable、限定 B2 receipt/B4 registration 并冻结独立 mapper；但 VerifiedDurable 的 B1 lifecycle 测试前置不可从事件重放，详见 M2。 |

## 必须修复项

### M1 — `Constraint` 无 scope，waiver scope coverage 无法确定性验证

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:722-725`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:756-760`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:839-841`
  - `/home/mii/code/draft/docs/design/mvp-design.md:94`
  - `/home/mii/code/draft/docs/design/mvp-design.md:113`
  - `/home/mii/code/draft/docs/design/advanced-music-agent-architecture.md:156-163`
- 实际证据：
  - B1 把 `Constraint` 字段冻结为 ID、BriefRevision、severity、description 与可选
    machine rule key，没有 `scope`。
  - 同一节随后要求 Waiver 保存 scope，且其 scope 必须“能覆盖 Constraint scope”；
    验收还要求 scope 不覆盖时拒绝。当前 reducer 没有被覆盖目标可供比较。
  - 权威 MVP 把范围列为 `ConstraintSet` 的基本属性；advanced architecture 的
    `Constraint` 也明确包含 `scope: MusicalAddress`，并同时包含 source、predicate 与
    verification。
- 影响：
  - 实现只能忽略 coverage、从 description/machine key 猜测，或引入日志外数据。
    任一种都会使同一 `ConstraintDeclared` 事件在 replay 时无法确定性证明 waiver 合法。
  - 这直接破坏 Hard Constraint Gate 与人类 Waiver 审计，是 R1 M4 的核心要求，而非
    表达细节。
- 最小修复方向：
  - 将 Constraint 的规范 scope 纳入 `ConstraintDeclared` 持久字段和 canonical digest；
    冻结 B1 所需的最小 scope 类型及确定性 covers 规则。
  - 若完整 `MusicalAddress` 要到 D 才冻结，B1 可采用明确版本化的最小 scope（例如
    WholeScore/opaque stable scope ID），但不得从自然语言推断覆盖关系。
  - 增加同范围、父范围覆盖、不相交范围、未知/不可比较范围的表驱动测试；未知关系应
    fail closed。

### M2 — Candidate 的 durable 前置只能日志外注入，无法由同一事件流 replay

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:747-751`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:764-781`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:787-792`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:839-843`
- 实际证据：
  - Candidate 必须看到 source Artifact 为 `VerifiedDurable`；`FixtureOnly` 明确不能满足
    Candidate/Accept。
  - B1 白名单只有 `FixtureArtifactDeclared`，且它既不是 durable 事实，也禁止 B3
    持久化。真正的 durable registration 要等 B2 receipt 与 B4 构造。
  - B1 仍要求测试 Draft→Candidate→Accepted，并允许用“不进入 protocol/event
    persistence 的 domain test harness capability”注入等价前置条件。
  - 同时 §13 要求 reducer 从空状态逐事件验证同一不变量，并要求 online projection 与
    清空后 event replay 逐字段/digest 相同。清空后重放白名单事件时，日志外 capability
    已不存在；`RevisionPromotedToCandidate` 要么被拒绝，要么 reducer 必须绕过生产
    `VerifiedDurable` Gate。
- 影响：
  - 实施者无法同时满足 lifecycle 测试、production fail-closed 与 replay 等价。
    若把测试 capability 隐藏在初始 state，投影不再由事件唯一决定；若让 fixture 满足
    Candidate，又违反已冻结的 M6 Artifact 可见性边界。
  - 该矛盾会在 B3 固化事件 schema 前迫使修改 reducer 或产生不可重放 Candidate。
- 最小修复方向：
  - 冻结测试专用、可进入测试事件流并可从空状态 replay 的 verified receipt/registration
    事实；其类型或 reducer profile 必须保证 production/B3 无法构造或持久化。
  - 或把 Candidate/Accept 的事件构造与 replay 验收明确推迟到 B4，只在 B1 测纯 Gate
    predicate，且从 B1 白名单/“完整 lifecycle replay”声明中同步删除无法达到的部分。
  - 无论选择哪条，验收必须证明：相同完整输入事件流从空状态可重放；删掉 verified
    prerequisite 后 promotion fail closed；FixtureOnly 永远不能替代它。

## 新增对抗检查

- 当前 `alda-agent/src/protocol.rs:8-24` 的 Slice A ID 是透明 wire string newtype，
  `ProjectSnapshot` 也是既有公开 DTO；B1 要求 DTO→domain 显式验证且不收紧旧 DTO，
  与现有兼容边界一致。
- 当前 `AppService` 仍由单 actor 持有 command/query sender，B1 新增内存
  ProjectCoordinator 不需要先做 B2/B3；但 B4 前必须继续保持 Project 写入只有一个
  coordinator，不让 Slice A `ServiceState` 与新 projection 各自成为事实源。
- `ScoreRevision` 明确带 Project/Score/Take/Branch 与 fork 规则，虽比 advanced
  architecture 示例字段更具体，但没有破坏 MVP 的 branch-head CAS 语义。
- B1 明确不暴露 durable create 命令、不公开 FixtureOnly，也不直接序列化 domain
  类型；没有发现新的 wire 兼容或范围扩张阻断。
- Publish 被从 minimal Profile 完整移除；advanced 文档中的 Published 仍可作为未来
  Profile 设计，不应反向进入 B1。

## 非阻断项

- `HumanActor` 已规定只能由认证客户端映射，但当前 Slice A 的 `client_id` 是
  `CommandEnvelope` 中的调用方字符串，不应直接作为 authority。实施时应从受信 adapter
  capability/认证上下文构造 human principal，并把 caller-supplied client ID 仅用于
  幂等与审计关联。B1 若不开放 Waive/Accept wire 命令，此项可在首次接入命令前门禁。
- `TakeCreated.common_base` 与 `BranchCreated.fork base` 最好明确 required/optional
  组合及“一个 Score 是否只允许一个无 parent root”。当前 fork/CAS 主路径已可实现，
  该边界可在构造器测试中 fail closed，不必扩大 B1 为 merge 设计。
- `Evidence` 已明确 append-only，但 correction/supersedes 仍可按计划在首次提供更正
  命令前冻结；B1 不得用重复 ID 覆盖原记录。
- canonical projection digest 已要求版本化稳定 JSON 与 SHA-256；实施门禁应包含固定
  byte vector、枚举编码和集合排序。当前没有浮点 canonicalization 必须在设计阶段解决。
- §13:848 的 “Slice A 37 Rust + 5 Node” 是当时登记的 L3 数量；测试数量变化时应以
  门控命令全绿为准，不应把固定计数当作删测或增测的替代指标。

## 审查判定

修订上述 M1/M2 后，B1 的事件身份、DAG/fork、CAS、human decision、Artifact 分层和
wire 隔离即可形成自洽的实施契约。当前没有外部阻塞，故结论为 `revise`。
