---
verdict: revise
scope: final
artifact: /home/mii/code/draft/alda-agent
reviewer: B1 fresh-context independent implementation reviewer
date: 2026-07-31
---

# B1 独立最终实现审查

## 结论

B1 当前不可 RELEASE，结论为 `revise`。

内存 reducer/coordinator 已实现 Score identity、默认与额外 Take/Branch、合法
cross-Take fork、branch-head CAS、事务 dry-run/零污染、四态 Hard Gate、H0、scope、
无 Publish 和固定 projection digest；Slice A 回归也通过。B1 不含磁盘 Store/Event
Log 和生产写集成本身不构成缺陷。

但仍有三项重大问题：

1. receipt 与 human capability 都能通过公开 serde event + `apply_events` 绕过；
2. `Draft → Rejected` 被错误拒绝，与获批所引用的生命周期状态机不一致；
3. B1 明确验收的独立 typed Project/Revision read DTO、mapper 与 CLI 查询完全缺失。

前两项会让未来 B3/B4 持久事件接受未经 Store 或认证边界授权的事实；第三项是 B1
当前范围内明确承诺的读面缺失，不应误归为“生产写集成留待 B4”。

## 重大问题

### M1 — `VerifiedArtifactReceipt` 与 `HumanActor` 构造限制可被公开 serde event mutation 绕过

- 位置：
  - `/home/mii/code/draft/alda-agent/src/domain/mod.rs:238-245`
  - `/home/mii/code/draft/alda-agent/src/domain/mod.rs:274-283`
  - `/home/mii/code/draft/alda-agent/src/domain/mod.rs:325-336`
  - `/home/mii/code/draft/alda-agent/src/domain/mod.rs:371-400`
  - `/home/mii/code/draft/alda-agent/src/domain/mod.rs:403-455`
  - `/home/mii/code/draft/alda-agent/src/state/mod.rs:207-221`
  - `/home/mii/code/draft/alda-agent/src/state/mod.rs:247-280`
  - `/home/mii/code/draft/alda-agent/src/state/mod.rs:503-539`
  - `/home/mii/code/draft/alda-agent/src/lib.rs:9-13`
- 实际证据：
  - `VerifiedArtifactReceipt` 的 test constructor 和 `into_record` 确实受
    `pub(crate)`/`cfg(test)` 限制；`HumanActor` 的 constructor 也只在 crate tests
    存在。
  - 但 `ArtifactRecord`、`HumanDecision`、`ConstraintWaiver` 和整个 `ProjectEvent`
    都公开实现 `Deserialize`。private 字段不会阻止 serde 构造：外部调用方可从自选
    JSON 得到 `ArtifactRecord { availability: VerifiedDurable,
    store_commit_identity: Some(...) }`，也可把 `authenticated_human` 设为 `true`。
  - `state` 与 `domain` 都由 crate root 公开；`ProjectCoordinator::apply_events` 是
    public，并接受任意 `Vec<ProjectEvent>`。因此外部 Rust 调用方无需 receipt 即可提交
    `ArtifactRegistered`，无需认证 adapter 即可提交 Waiver/Accept。
  - reducer 只检查反序列化出的布尔位和字符串，不验证不可伪造 capability/receipt
    来源。现有 lifecycle 测试只走合法 constructors，没有覆盖 serde bypass。
- 影响：
  - §13 的核心约束“生产 surface 不能产生 ArtifactRegistered”“HumanActor 只能由认证
    身份映射构造”在实际 crate API 上不成立。
  - B3 若直接反序列化相同事件类型用于 replay，攻击者/损坏日志可伪造 durable
    reachability 或 human decision；B4 若复用公开 `apply_events`，任何同进程组件都能
    绕开 Store commit 和认证命令边界。Candidate/Accept 可能据此合法化。
- 最小修复方向：
  - 分离“已验证的持久事件重放表示”和“可提交领域命令/capability”。生产 coordinator
    mutation 不得公开接受任意 `ProjectEvent`；`apply_events` 应限于 reducer/replay
    内部或 tests，正常入口只接受经过构造器验证的 command/receipt/human capability。
  - 不要把 `authenticated_human: bool` 或 `VerifiedDurable` enum tag 当 capability。
    可序列化事件应保存审计事实，但事件创建必须由不可伪造的 sealed input 完成；replay
    应在可信 Event Store 校验批次/schema 后使用专门入口，而非成为在线提交入口。
  - 增加外部/集成级负向测试，证明 serde 构造的 ArtifactRegistered、Waiver 和 Accept
    无法通过任何生产 coordinator API 提交。

### M2 — reducer 拒绝权威状态机允许的 `Draft → Rejected`

- 位置：
  - `/home/mii/code/draft/alda-agent/src/state/mod.rs:281-289`
  - `/home/mii/code/draft/docs/design/advanced-implementation-roadmap.md:276-293`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:748-750`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:849-851`
- 实际证据：
  - 权威生命周期图明确从 `Draft` 直接存在 `reject → Rejected`；`Draft` 只是已创建，
    并不要求先通过 H0 成为 Candidate 才能记录拒绝。
  - 实现的 `RevisionRejected` 固定调用
    `require_lifecycle(..., Candidate)`，所以 Draft rejection 必然返回
    `InvalidLifecycleTransition`。
  - B1 现有测试只覆盖 Candidate 后 Accept，没有按验收要求覆盖 Rejected/Aborted 的
    全部合法与非法转换，因此该偏差未被门禁发现。
- 影响：
  - H0 失败、用户在 Gate 前放弃或明确拒绝 Draft 时，系统无法记录 Rejected，只能错误
    使用 Aborted 或保留 Draft。这会在 B3 固化错误 lifecycle 事实，并混淆“用户拒绝”
    与“取消/执行中止”。
- 最小修复方向：
  - 按获批状态机允许 `RevisionRejected` 从 Draft（以及设计若保留的 Candidate）进入
    Rejected；继续拒绝 Accepted/Rejected/Aborted 的反向转换。
  - 添加完整生命周期表驱动测试，区分 reject 与 abort 的合法来源，并验证失败零污染。

### M3 — 获批 B1 范围内的 typed Project/Revision read DTO、mapper 和 CLI 查询未实现

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:802-804`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:858-860`
  - `/home/mii/code/draft/alda-agent/src/protocol.rs:34-83`
  - `/home/mii/code/draft/alda-agent/src/main.rs:46-138`
  - `/home/mii/code/draft/alda-agent/src/lib.rs:9-13`
- 实际证据：
  - 获批设计明确要求 `protocol.rs` 中独立版本化查询 DTO、显式 domain→wire mapper，
    并要求 protocol/CLI 至少提供 Project domain snapshot 与 revision list/read。
  - `ClientCommand` 仍只有 Slice A 的 ProjectSnapshot/Session/Turn/Artifact/Event
    命令；既有 `protocol::ProjectSnapshot` 只包含 name/version，没有 Score、Revision
    或 lifecycle。
  - CLI Project 子命令仍只有 Slice A 的 create/snapshot；不存在 revision list/read。
    全仓检索没有 B1 read DTO、mapper 或 mapper fixture test。
  - domain/state 类型与模块被直接 `pub mod` 暴露，但这不是独立 wire DTO，也不满足
    protocol/domain 隔离；客户端无法通过正式协议读取 B1 projection。
- 影响：
  - 这是 B1 验收中明确的生产读面，不是 B2 磁盘或 B4 写集成。缺失后无法验证客户端
    看到的是稳定白名单字段，也无法证明 domain 新字段不会自动泄漏成 wire。
  - 若后续直接把当前 serde `state::ProjectSnapshot` 补进 reply，会违反已经批准的
    protocol/domain 隔离并固化内部模型。
- 最小修复方向：
  - 增加独立版本化 Project-domain snapshot 和 Revision summary/detail DTO，以及显式
    mapper；仅复制获批白名单字段，不公开 domain event、fixture artifact 或内部
    capability 字段。
  - 将只读查询接入现有 bounded query path 和 CLI revision list/read；不必接入任何
    durable create/write 命令。
  - 添加 mapper fixture，证明 domain-only 新字段不会自动进入 JSON wire。

## 已验证通过

- reducer 在线提交和 `replay(events)` 使用同一 `ProjectSnapshot::apply`；合法完整事件
  流 replay 与在线 snapshot/digest 相同，删除 ArtifactRegistered 后 promotion
  fail closed。
- Coordinator 在 clone projection 上 dry-run 整个候选批次，成功后才替换 snapshot
  并追加 events；已覆盖 CAS、验证失败和 reducer 失败零污染。
- 默认 Take/Branch、额外 Take/Branch 与合法 cross-Take fork 能从空事件重建；
  ordinary revision 必须沿 branch head/fork base，merge fail closed。
- Candidate 要求 VerifiedDurable + WholeScore H0 Pass；FixtureOnly 不满足 Gate。
  Hard Constraint 只接受同 Revision Pass 或有效 Waiver，Unknown/Fail 不当作 Pass；
  scope coverage 为 WholeScore 或完全相等。
- `RevisionLifecycle` 与 `ProjectEvent` 均没有 Published variant；protocol/CLI 也没有
  Publish surface。
- domain 未依赖 Tokio、HTTP、Provider 或文件系统；现有 protocol 没有直接
  `pub use domain::*`。M3 是承诺的独立 read DTO 缺失，不是已经发生 domain DTO
  自动序列化。

## 非阻断残余

- B1 明确只做内存领域核；没有磁盘 Artifact Store、Project Event Log、checkpoint、
  restart recovery、持久幂等或 App Service 生产写集成，均属于 B2–B4，不阻止 B1。
- `ProjectCoordinator::from_events` 不恢复内存 commands/idempotency cache，符合 B1
  “B3 才持久化 stable reply”的边界。
- 当前 DAG 测试数量小于设计列出的完整属性/表驱动矩阵，且 duplicate parent 会先返回
  `UnsupportedMerge` 而非 `DuplicateParent`。这些降低门禁强度，但在 merge 明确
  fail-closed、核心 fork/CAS 路径已有确定性覆盖的前提下，本轮不单独升级为阻断。
- `npm test --prefix web` 失败是因为 `web/package.json` 没有 test script；实际指定的
  Node 门禁 `node --test web/client-state.test.js` 为 5/5 PASS。建议增加 npm script
  以统一开发入口，但这不是 B1 domain correctness 阻断。

## 机械门禁

在 `/home/mii/code/draft/alda-agent` 实际运行：

```text
cargo fmt --check                          PASS
cargo clippy --all-targets -- -D warnings PASS
cargo test --all-targets                  PASS
  lib unit tests                          36 passed
  main unit tests                          5 passed
  HTTP integration                         1 passed
  WS integration                           2 passed
node --test web/client-state.test.js      PASS (5 passed)
node --check web/app.js                   PASS
node --check web/client-state.js          PASS
node --check web/sw.js                    PASS
git diff --check                          PASS
```

机械门禁全绿不能覆盖 M1 的 crate API/serde capability bypass、M2 的生命周期偏差和 M3
的 read surface 缺失，因此最终 verdict 仍为 `revise`。

## RELEASE 判定

- Slice A 回归：PASS。
- B1 内存领域核：主体已实现，但 M1–M3 未闭合。
- B1：**不可 RELEASE**。
- Slice B / 正式 MVP：本报告不作完成声明。
