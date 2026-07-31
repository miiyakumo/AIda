---
verdict: approved
scope: B4a prerequisite
artifact: /home/mii/code/draft/alda-agent
reviewer: B4a independent implementation reviewer R2
date: 2026-07-31
---

# B4a 前置能力独立实现审查 R2

## 结论

B4a可进入B4 control/runtime集成，verdict为`approved`。

R1唯一重大问题已闭合。Project与Session append现在都先把transaction ID、完整command
record、完整events以及Session restart authorization编码进canonical plan digest，并在
任何command-index幂等短路之前执行transaction probe。因此同tx即使复用相同command
payload，只要events或authorization变化也固定返回`IdempotencyConflict`。

`SamePlanCommitted`返回该transaction提交时保存的
`resulting_last_sequence`；probe同时返回对应`resulting_batch_checksum`。它不再把当前
aggregate head冒充原transaction result anchor。Artifact receipt-loss recovery链路未见
回归。

## R1 M1闭合核验

### Project

- `prepare_batch`先将domain events转换为stored DTO，再计算
  `project_plan_digest(ProjectId, transaction_id, command_record, events)`。
- 随后立即执行transaction index三态probe：
  - `Absent`才继续检查command index；
  - `SamePlanCommitted`返回原transaction的result sequence；
  - `ConflictingPlan`返回`IdempotencyConflict`。
- 因此command key与payload相同不能再掩盖同tx异events。
- `append_reopen_replay_and_command_idempotency_are_exact`明确提交
  `transaction-1/command-1/payload-a/initialized`后，以同tx、同command/payload和
  `brief` event重试，断言冲突且writer保持可用。

### Session

- `prepare_session_batch`先构造stored events与stored restart authorization，再计算
  `session_plan_digest(SessionId, transaction_id, command_record, authorization, events)`。
- transaction probe同样先于command index；同tx异events或异authorization不能到达旧
  command reply短路。
- `event_and_command_only_batches_preserve_head_chain_and_exact_reply`明确覆盖同tx、同command
  payload、不同TurnStarted event并断言`IdempotencyConflict`。
- authorization虽然没有独立的“同command只改authorization”单测，但其完整字段
  `pre_head_sequence + ordered turn_ids`直接进入probe前的digest，且
  `apply_session_batch`另有planner/authorization一致性校验；不存在绕过控制流。

## Result anchor核验

- transaction index的value保存：
  - `canonical_plan_digest`
  - `resulting_last_sequence`
  - `resulting_batch_checksum`
- 两种aggregate都在成功应用batch时从该batch实际`last_sequence`和
  `batch_checksum`生成commit，不从当前head事后猜测。
- `SamePlanCommitted` append outcome使用
  `committed.resulting_last_sequence`，不是`state.last_sequence`。
- read-only `probe_transaction`返回完整`TransactionCommit`，B4 recovery可同时核对
  sequence与checksum anchor。
- full replay从每个权威batch重算plan digest/result；checkpoint transaction index会
  从log prefix重放后逐字段比较。即使重算checkpoint外层checksum篡改index，也会回退
  full replay。
- 固定Project/Session probe测试验证same plan、different plan、result sequence、
  checksum格式/固定向量及checkpoint tamper fallback。

跨transaction复用同一command仍按command业务幂等返回旧reply，并报告当前aggregate
head。这条路径的transaction是`Absent`，不声称是`SamePlanCommitted`，因此不会被B4
用作目标transaction的result anchor。

## Artifact prerequisite回归核验

- primitive `ArtifactAuditPlanV1`仍不携带receipt/capability。
- runtime recovery guard通过`Arc::ptr_eq`绑定同一open Store authority；guard与capability
  都不可Clone、不可序列化且crate-private。
- recovery audit验证control tx、Store instance、layout、durability与commit identity，
  并通过同一打开blob handle重算hash/size。
- recovered capability只能被对应stored `ArtifactRegistered` plan消费；错误artifact
  record、错误guard/store/tx/plan和替换blob均有负向测试。
- Project handoff严格先probe：`Absent`才mint；pre-write失败后再次probe Absent可重新
  mint；`SamePlanCommitted`直接返回保存的transaction result且不重复Artifact event。

未发现Artifact prerequisite的新重大回归。

## 非阻断建议

增加一个Session专用固定测试：先提交带command record的合法restart plan，再以同tx、
同command payload、相同events但不同`RestartAuthorizationV1`重试，直接断言
`IdempotencyConflict`和result anchor不变。当前代码路径已保证该结果，此测试可防未来
digest字段或检查顺序回归。

## 机械门禁

在`/home/mii/code/draft/alda-agent`实际运行：

```text
cargo fmt --check                                         PASS
cargo clippy --all-targets --all-features -- -D warnings PASS
cargo test --all-targets --all-features                  PASS
  lib unit tests                                         91 passed
  main unit tests                                         6 passed
  HTTP integration                                        1 passed
  WS integration                                          2 passed
node --check web/app.js                                  PASS
node --check web/client-state.js                         PASS
node --check web/sw.js                                   PASS
node --test web/client-state.test.js                     PASS (5 passed)
git diff --check                                         PASS
```

## B4a判定

- R1 command-before-transaction probe bypass：PASS。
- Project同tx/同command payload/异events：PASS。
- Session同tx/同command payload/异events或authorization：PASS。
- transaction result anchor：PASS。
- Artifact recovery prerequisite：PASS。
- 新重大问题：无。
- B4a prerequisite：**APPROVED**。
