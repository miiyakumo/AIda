---
verdict: revise
scope: design
artifact: /home/mii/code/draft/docs/plans/mvp-deliberative-execution.md
reviewer: B1 fresh-context independent design reviewer
date: 2026-07-31
---

# B1 独立设计审查

## 结论

B1 选择“纯领域内核 + 同一 reducer 的在线/dry-run/replay + branch-head CAS”作为 B2/B3
之前的内存语义地基，方向正确；不提前宣称 durability，也符合当前 `alda-agent`
单 actor 状态所有权。

但当前 §13 不能批准实施。存在六项会直接影响未来 Project Event Log、DAG、Artifact
可见性或 CAS 正确性的重大问题：Score 聚合身份被遗漏；Take/Branch 没有权威创建事实；禁止
cross-take parent 与 Take fork 的权威语义冲突；Waiver 与 Accept 没有冻结足以证明
人类授权的持久事实；MVP 明确排除的 Publish 被写进事件白名单；Artifact metadata
注册与 blob 可用性/协议 DTO 边界没有闭合。若先按当前白名单实现，B3 将只能破坏既有
日志 schema 或用投影隐式猜测来补洞。

## 必须修复项

### M1 — B1 丢失 `ScoreId`，Revision 的持久身份链与权威领域模型不一致

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:699-702`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:724-728`
  - `/home/mii/code/draft/docs/design/advanced-implementation-roadmap.md:455-460`
  - `/home/mii/code/draft/docs/design/advanced-music-agent-architecture.md:98-119`
  - `/home/mii/code/draft/docs/design/advanced-music-agent-architecture.md:211-226`
- 实际证据：
  - M6.1 明确要求独立 `Score` ID；高级领域模型把 `CompositionProject` 与 `Score`
    分为不同聚合概念，`ScoreRevision` 明确携带 `score_id`。
  - B1 的 typed ID 清单没有 `ScoreId`，其 `ScoreRevision` 字段改为
    Revision/Project/Take/Branch，却没有任何字段标识被修订的 Score。
  - 当前 Slice A 只有 Project/Session/Fake Artifact，尚无旧 `ScoreId` 兼容负担；这是
    冻结正确持久 schema 的最后低成本时点。
- 影响：
  - B3 事件一旦持久化，Revision DAG 只能被解释为“Project 就是 Score”。后续 IR Lite、
    MIDI、导入、多 Score 或明确 score identity 接入时，需要破坏 `RevisionCreated`
    schema 或迁移所有 Revision/Artifact/Evidence 引用。
  - 仅靠 Project/Take/Branch 不能证明两个 parent 属于同一 Score，DAG reducer 会接受
    领域上不可合并的 parent。
- 最小修复方向：
  - B1 增加 `ScoreId` 值类型；`ProjectInitialized` 明确创建/引用 MVP 的默认 Score，
    `ScoreRevision`、DAG key、parent 校验和 snapshot 都携带该 Score。
  - parent 除同 Project 外必须同 Score；即使 MVP 暂时规定一个 Project 仅一个 Score，
    也应把该限制写成显式不变量，而不是从缺失字段推断。

### M2 — Take/Branch 没有白名单创建事实，branch-head CAS 无法从空日志确定性重建

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:743-757`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:760-784`
  - `/home/mii/code/draft/docs/design/mvp-design.md:92-102`
  - `/home/mii/code/draft/docs/design/mvp-design.md:108-112`
- 实际证据：
  - B1 snapshot 必须包含各 Take/Branch head，`ProposeRevision` 必须指定
    Project/Take/Branch，验收还要求“不同 branch 可并存”。
  - 事件白名单没有 `TakeCreated`、`BranchCreated` 或等价的、字段已冻结的事实。
    `BranchHeadAdvanced` 只表达 head 变化，不能同时无歧义表达 branch 的创建来源、
    owning Take、共同基线与初始 expected-head 语义。
  - `ProjectInitialized` 是否隐式创建默认 Take/Branch、其 ID 和初始 head 均未定义；
    更无法支持验收所述第二条 branch。
- 影响：
  - reducer 无法区分“目标 branch 不存在”“新建空 branch”“从某 Revision fork”；
    `expected_head = None` 可能被错误地当成任意未知 branch 的合法首次提交。
  - B3 replay 若依赖 snapshot 中预置的 branch 或命令侧隐式创建，将使 projection 成为
    事实源；删除 checkpoint 后不能从白名单事件重建相同 CAS 状态。
- 最小修复方向：
  - 冻结显式 `TakeCreated` / `BranchCreated`（或一个等价原子事件）的字段与不变量：
    Project、Score、Take、Branch、可选 fork/base Revision、初始 head。
  - 定义默认 Take/Branch 是否由 `ProjectInitialized` 原子创建；定义 `None` expected
    head 只匹配“已存在且当前为空”的 branch，未知 branch 必须 typed not-found。
  - 增加从空事件 replay 多 branch、重复创建、跨 Project/Take branch 与空 head 首次
    CAS 的表驱动测试。

### M3 — `CrossTakeParent` 拒绝规则与权威 Take fork/common-base 模型矛盾

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:724-730`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:783-784`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:788-800`
  - `/home/mii/code/draft/docs/design/mvp-design.md:108-111`
  - `/home/mii/code/draft/docs/design/advanced-music-agent-architecture.md:631-645`
  - `/home/mii/code/draft/docs/design/advanced-implementation-roadmap.md:643-649`
- 实际证据：
  - B1 要求所有 parent 与新 Revision 属于同一 Take，并把
    `CrossTakeParent` 列为错误和强制负向测试。
  - 权威 MVP 不变量只要求 parent 同 Project；高级模型要求从一个不可变 Revision fork
    Take，`CandidateSet` 保存 `common_base`，路线也明确“从 Revision 创建 branch/take”。
  - 新 Take 的首个 Revision 若以 fork base 为 parent，该 parent 必然仍属于来源 Take；
    按 B1 当前规则会被拒绝。若复制一份相同 Revision 到新 Take，则破坏 Revision
    不可变身份/来源并制造假 DAG 节点。
- 影响：
  - 当前规则使正式的 Take A/B fork 无法表示，或迫使实现使用无 parent 的伪根和日志外
    `common_base`，从而丢失 lineage。
  - 未来为实现 Take 比较而放宽该规则会改变已经持久化的 DAG 合法性定义，并可能让旧
    replay 与新 reducer 得出不同结果。
- 最小修复方向：
  - 将不变量改为 parent 必须同 Project、同 Score；允许新 Take/Branch 的首个 Revision
    引用显式 fork base。
  - 用 Take/Branch 创建事实冻结 `common_base`/fork source，并限制后续普通提交是否只
    能沿本 branch head；merge 的多 parent 与显式冲突决议仍可留到对应里程碑。
  - 删除笼统的 `CrossTakeParent`，换成能区分非法普通跨 branch parent、合法 fork base
    与未来显式 merge 的 typed 错误/规则。

### M4 — `ConstraintWaived` 与 Accept 未冻结人类授权事实，无法持久证明 Gate 合法

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:713-719`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:739-756`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:794-803`
  - `/home/mii/code/draft/docs/design/mvp-design.md:112-117`
  - `/home/mii/code/draft/docs/design/advanced-implementation-roadmap.md:295-307`
- 实际证据：
  - 权威不变量要求 Waiver 持久记录 actor、reason、scope、timestamp、constraint ID 和
    适用 Revision ID，且只能由人类显式 Waive；Accept 也只能由真实用户/明确人类角色
    触发。
  - B1 只称“绑定同一 Constraint/Revision 的有效人类 Waiver”，没有定义
    `ConstraintWaived` 的必需字段、actor 类型/authority 校验、reason/scope；其验收只
    覆盖 Unknown/Fail/错 Revision waiver。
  - `RevisionAccepted` 同样没有冻结决定者字段，也没有验收 Agent/fixture actor 发起
    Accept 必须被拒绝。`created_at` 的通用非空规则不能替代授权身份与理由。
- 影响：
  - reducer 可能只凭 `(constraint_id, revision_id)` 将 Hard Fail/Unknown 视为满足，
    B3 replay 后无法证明 waiver 是谁、为何、覆盖何范围，也无法排除 Agent 自行 Accept。
  - 这些字段属于权威审计事实；后补会要求升级已落盘事件 schema，且旧事件无法无损补造。
- 最小修复方向：
  - 在 B1 冻结 `ConstraintWaived` 的 actor（带 human authority 类型）、reason、scope、
    timestamp、Constraint 和 Revision；所有字段进入 canonical digest/replay。
  - 冻结 `RevisionAccepted` 的 human actor/decision metadata；reducer 明确拒绝
    Agent、Provider、Tool 和 DeterministicFixture actor 的 Waive/Accept。
  - 增加错 actor、空 reason、scope 不覆盖目标 constraint、错 revision、waiver 沿用到
    新 Revision，以及非人类 Accept 的负向测试。

### M5 — B1 把 `RevisionPublished` 纳入 MVP 事实白名单，违反明确范围且缺少发布授权语义

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:729-735`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:754-756`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:800-803`
  - `/home/mii/code/draft/docs/design/mvp-design.md:115-117`
  - `/home/mii/code/draft/docs/design/mvp-design.md:61-69`
- 实际证据：
  - MVP 不变量明确写着“Agent 不能 Accept 或 Publish；MVP 只有人类能 Accept，且不提供
    Publish”，out-of-scope 也排除发布。
  - B1 却把 `Published` 列入 lifecycle、将 `RevisionPublished` 列入 Project event
    白名单，并要求测试 `Accepted→Published`，但没有发布目标、来源/许可、Effect、
    每次审批或 human publisher 等正式发布 Gate 字段。
- 影响：
  - 这会把一个明确不属于 MVP、且安全语义不完整的权威事件固化进 B3 schema；调用方
    可能仅凭 Accepted 状态制造 Published 事实，绕过后续 Studio/发布权限边界。
  - 即使暂时不暴露命令，reducer 接受该事件也使内部 fixture/迁移器可产生产品不承诺的
    状态，后续补齐真实发布 Gate 时旧事件无法证明合法性。
- 最小修复方向：
  - 从 B1 的可构造事件白名单、状态转换和验收中删除 `RevisionPublished` /
    `Accepted→Published`；领域枚举若为未来兼容保留该 variant，也必须在 minimal
    Profile reducer 中 fail closed，且不能生成/持久化该事实。
  - 发布事件 schema 到明确立项时再冻结，并同时包含目标、来源/许可、Effect、审批与
    human authority；不要在 B1 预占一个字段不足的 v1 事件。

### M6 — `ArtifactRegistered` 的可见性与 protocol/domain DTO 隔离未闭合，metadata 可被误当成可用 Artifact

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:738-761`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:774-781`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:804-807`
  - `/home/mii/code/draft/docs/design/mvp-design.md:134-145`
  - `/home/mii/code/draft/docs/design/advanced-implementation-roadmap.md:262-274`
  - `/home/mii/code/draft/docs/design/advanced-implementation-roadmap.md:463-476`
- 实际证据：
  - B1 正确注明 `ArtifactRegistered` 只代表 metadata/reachability、不证明 B2 blob
    durability；但 Candidate prerequisite 只要求“source Artifact metadata 已登记且
    hash 一致”，snapshot 又包含 Artifact reachability。
  - §13 没有定义 B1 Artifact 的 availability/durability 状态，也没有规定
    `ArtifactRegistered` 在 B2 成功提交 blob 前不得进入可下载/可接受的公开投影。
    因而 reducer 可以把不存在 bytes 的 hash 作为 reachable source 并将 Revision
    Promote 为 Candidate。
  - B1 还要求 protocol/CLI 提供 Project domain snapshot 与 revision list/read typed
    查询，却未重申 MVP §6.1 的硬边界：“Protocol 不直接把 Rust 领域结构序列化成永久
    公开协议”。领域对象本身又被设计为依赖 serde；若查询直接返回它们，B1 内部字段会
    意外成为 wire v1。
- 影响：
  - B3 replay 可能稳定重建出“Candidate + reachable Artifact”，而 B2/B4 实际没有可
    verify/get 的 blob；这正是权威设计要避免的“事件已提交但 Artifact 不存在”半状态。
  - 若 protocol 直接序列化 domain snapshot/revision，随后为 B2 availability、迁移或
    内部不变量增加字段就会破坏公开 wire，反过来迫使领域模型迁就协议兼容。
- 最小修复方向：
  - 冻结 Artifact 投影的状态机/可见性：至少区分 metadata proposed/process fixture 与
    verified durable reachable。B1 fixture 可以参与纯 reducer 测试，但 minimal
    Candidate/Accept 和公开 artifact availability 不得把前者当作可用 blob；B2/B4
    必须以已 verify 的 store receipt/commit identity 驱动 durable registration。
  - 明确 `ArtifactRegistered` 的事件字段及其名称是否足以表达 durability；若 B1 事件
    仅是非持久 fixture，避免使用会被 B3 解释为最终 reachability 的同一事实名。
  - 在 `protocol.rs` 定义独立、版本化的 Project/Revision read DTO，由显式 mapper 从
    domain snapshot 生成；禁止 `pub use domain::*`、直接把 domain event/snapshot
    作为 wire reply。增加 mapper fixture 测试证明内部新增字段不自动出现在 wire。

## 非阻断残余

- B1 仅保存进程内幂等 reply、到 B3 才与事务批次一同持久化是清楚且可接受的；B1
  不应暴露可被误认 durable 的写命令。
- canonical projection digest 已要求版本化、稳定字段顺序和 SHA-256；实施时仍应冻结
  明确测试向量、字符串/枚举编码与集合排序，但当前 B1 类型若不含浮点或非规范 map，
  这可作为实施门禁而非设计阻断。
- Evidence append-only 已明确；“更正 Evidence 指向被替代项”的具体字段可在首次需要
  correction 命令前冻结。B1 至少不能通过覆盖同一 Evidence ID 实现更正。
- 当前 `alda-agent` 的 `ServiceState` 是单 actor 所有，幂等缓存与 Slice A 事件投影均
  为进程内；B1 可在其旁新增 ProjectCoordinator，不必先修改磁盘或声称重启恢复。
  B4 集成前仍需避免 App Service 与 ProjectCoordinator 形成两个可写 Project 真相。

## 审查判定

修订 M1–M6 后，B1 可进入第二轮设计复核。重点不是提前实现 B2/B3，而是现在冻结未来
日志必须无损保存的聚合身份、branch/fork 与人类决定事实。
