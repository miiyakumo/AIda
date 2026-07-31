---
verdict: revise
scope: B4a prerequisite
artifact: /home/mii/code/draft/alda-agent
reviewer: B4a independent implementation reviewer
date: 2026-07-31
---

# B4a 前置能力独立实现审查

## 结论

B4a当前不可进入B4 control/runtime集成，verdict为`revise`。

Artifact receipt-loss redo链路已经闭合§17/R1-R2的M2边界：primitive audit plan不携带
receipt能力；guard绑定同一Store handle的私有authority；blob通过同一打开描述符重验；
capability不可Clone且只能经Project stored-event handoff消费；`Absent`允许重新mint，
`SamePlanCommitted`停止mint/append。错误guard/store、control tx、plan、替换blob和错误
Project event均有负向测试。

Project与Session transaction index/probe本身也正确保存plan digest及结果锚点，并由完整
日志重放校验checkpoint cache。然而append入口在transaction probe之前按command key
提前返回，允许同一transaction ID携带不同plan却被报告为幂等成功。这违反获批M4的
“重复append同plan成功、不同plan固定冲突”，会使B4 recovery把未执行的redo plan误认为
已完成。

## 重大问题

### M1 — command幂等短路绕过transaction plan冲突

- 严重度：重大。
- 位置：
  - `/home/mii/code/draft/alda-agent/src/state_store/mod.rs:943-970`
  - `/home/mii/code/draft/alda-agent/src/state_store/session.rs:2056-2097`
  - `/home/mii/code/draft/alda-agent/src/state_store/mod.rs:1158-1189`
  - `/home/mii/code/draft/alda-agent/src/state_store/session.rs:2326-2345`
- 实际控制流：
  - Project `prepare_batch`先查`state.commands`。只要client command key已存在且
    `payload_digest`相同，就在计算`project_plan_digest`、查询transaction index之前返回
    `AppendOutcome { appended: false }`。
  - Session执行相同顺序；因此还可绕过对变化后的`restart_authorization`和events的
    transaction冲突检查。
  - 两种canonical digest实际上覆盖aggregate ID、transaction ID、完整command record、
    完整stored events，Session还覆盖restart authorization；问题不在digest完整性，而在
    append没有到达该检查。
- 可复现反例：
  1. 先提交`tx=T, command=C, payload_digest=P, events=E1`。
  2. 再append`tx=T, command=C, payload_digest=P, events=E2`，其中`E2 != E1`；Session
     也可只改变restart authorization。
  3. transaction index对第二个plan本应返回`ConflictingPlan`，当前append却在command
     index处直接返回旧reply和`appended=false`。
- 影响：
  - B4 Prepared保存的plan若因codec、恢复选择或损坏而与已提交plan不同，aggregate
    append不会固定冲突；control层可能把旧command reply误当作当前redo plan的成功。
  - 返回的`last_sequence`还是当前aggregate head，而非该transaction保存的
    `resulting_last_sequence`，进一步破坏结果锚点语义。
- 最小修复方向：
  - 对非空transaction ID先构造canonical plan digest并检查transaction index；同tx必须
    严格执行`Absent | SamePlanCommitted | ConflictingPlan`。
  - 仅在transaction为`Absent`时再应用跨transaction的client command幂等规则；或显式
    定义同时满足两套索引时的优先级，但不得允许同tx异plan成功。
  - 为Project和Session各增加“同tx、同command payload、异events”的append负向测试；
    Session另覆盖仅restart authorization变化。断言`IdempotencyConflict`且head、reply、
    transaction result anchor均不变。

## Transaction index/probe核验

- Project digest覆盖schema tag、Project ID、transaction ID、完整command record及stored
  events；Session还覆盖restart authorization。未发现digest字段遗漏。
- full replay从batch重新计算digest，并记录实际`last_sequence`和`batch_checksum`。
- checkpoint虽可重算外层checksum，但加载时会从offset 0重放到anchor，并比较
  sequence、batch checksum、command index和完整transaction index；篡改transaction
  cache会回退full replay。
- `probe_transaction`能精确区分`Absent`、同digest commit及异digest conflict；结果带
  committed sequence/checksum。
- 无command record的重复append路径符合contract；带command record的路径被M1阻断。

## Artifact恢复能力核验

- `ArtifactAuditPlanV1`只保存hash、size、layout、store instance、durability、commit
  identity和control tx；反序列化不会生成receipt/capability。
- `ArtifactRecoveryGuard`和`RecoveredArtifactCapability`均为crate-private、不可Clone、
  不可序列化。guard使用`Arc::ptr_eq`绑定创建它的同一Store handle，而不是仅比较可伪造
  的instance字符串。
- audit逐项验证control tx、plan结构、Store instance/layout/durability/commit identity，
  再由`get`在同一fd上完成hash/size验证，不存在verify后按路径重开窗口。
- Project handoff先probe：`Absent`才reaudit/mint，`SamePlanCommitted`直接返回transaction
  result，`ConflictingPlan`失败；capability消费时还逐字段比较audit plan与stored
  `ArtifactRegistered` record。
- receipt丢失、pre-write失败后重新open并再次`Absent`可重新mint；提交后不会再次mint。
  错误Store/guard/control tx、tampered plan、替换或损坏blob、错误event均fail closed。

## 机械门禁

在`/home/mii/code/draft/alda-agent`实际运行：

```text
cargo fmt --all -- --check                              PASS
cargo clippy --all-targets --all-features -- -D warnings PASS
cargo test --all-targets --all-features                 PASS
  lib unit tests                                         91 passed
  main unit tests                                         6 passed
  HTTP integration                                        1 passed
  WS integration                                          2 passed
node --test web/*.test.js                                PASS (5 passed)
git diff --check -- . excluding .agents/.codex           PASS
```

仓库根目录本身不是Cargo workspace，因此Cargo门禁在`alda-agent`执行。工作树中已有大量
用户改动；本审查未修改实现，也未触碰`.agents/.codex`。

## B4a判定

- §17/R1-R2 M2 Artifact recovery capability：PASS。
- §17/R1-R2 M4 transaction digest/index/probe/checkpoint：PASS。
- transaction-aware idempotent append：FAIL（M1）。
- B4a prerequisite：**REVISE**。

