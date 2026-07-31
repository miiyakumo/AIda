---
verdict: revise
scope: final
artifact: /home/mii/code/draft/alda-agent
reviewer: B3b fresh-context independent implementation reviewer R2
date: 2026-07-31
---

# B3b 独立实现审查 R2

## 结论

B3b 当前仍不可 RELEASE，verdict 为 `revise`。

R1的两个重大安全问题均已闭合：

- terminal eligibility现在是hidden projection state；Succeeded/Failed分别只能消费匹配
  Approve/Deny resolution产生的eligibility，`AbortedByRestart`只能消费经过完整planner
  batch校验的`RestartAuthorizationV1`。
- Approval request保存canonical subject inputs，并使用protocol层与App Service共享的
  digest函数，从authoritative stored prompt重新计算后逐字节比较。错误digest、
  provider、field set、prompt以及request+resolve同时伪造均有checksum重算负向测试。

但本轮发现一个独立重大回归：获批§16把`BudgetExceeded`定义为正式terminal并要求其
restart切点验收，当前reducer却无条件拒绝它；测试也把拒绝当成预期。因此B3b无法保存或
恢复合法budget终态，未满足白名单与验收范围。

## 重大问题

### M1 — `BudgetExceeded`被codec接受但被reducer永久拒绝

- 严重度：重大。
- 位置：
  - `/home/mii/code/draft/alda-agent/src/state_store/session.rs:601-616`
  - `/home/mii/code/draft/alda-agent/src/state_store/session.rs:825-850`
  - `/home/mii/code/draft/alda-agent/src/state_store/session.rs:3298-3312`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1290-1297`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1358-1360`
- 实际证据：
  - stored codec把`"budget_exceeded"`解析为`TurnStatus::BudgetExceeded`，表明它属于
    versioned stored vocabulary。
  - reducer的terminal分支却固定执行`TurnStatus::BudgetExceeded => false`，不存在任何
    状态、fact或authorization能使该terminal合法。
  - `restart_planner_converges_running_cancelled_waiting_and_terminal_turns`构造Running后
    `TurnCompleted(BudgetExceeded)`，只断言`ProjectionRejected`；没有合法budget
    terminal的replay、checkpoint、wire sequence或restart验收。
  - §16明确规定`BudgetExceeded`是正式terminal，并要求在该切点验证restart reconciliation。
- 影响：
  - 一旦B4/runtime需要提交预算耗尽结果，B3b只能拒绝整个事务；无法重建A1/A2完整
    `TurnStatus`生命周期。
  - 若实现方绕过B3b直接生成wire terminal，则持久log、snapshot和resume会与在线状态
    分裂；若不生成，则用户可见Turn会错误停留在Running。
- 最小修复方向：
  - 增加versioned authoritative budget-exhaustion fact/authorization，或在获批设计中
    冻结等价、不可由普通调用者伪造的eligibility来源。
  - reducer只允许`BudgetExceeded`消费该eligibility，并清除它；普通
    `TurnCompleted(BudgetExceeded)`继续fail closed。
  - 增加合法budget路径的full replay、checkpoint+tail、wire event和restart no-op测试，
    以及checksum重算但缺失/错误budget authorization的负向测试。

## R1 M1闭合核验：terminal eligibility与restart authorization

- `TurnProjection`新增非wire的`terminal_eligibility`：
  `ApprovalApproved`、`ApprovalDenied`、`RestartAuthorized`。
- `ApprovalResolved`只有在pending digest匹配时才恢复Turn为Running，并按decision设置
  单次eligibility；Succeeded/Failed必须匹配该eligibility且无pending object。
- 新Question/Approval request在eligibility尚未消费时被拒绝；cancel会清除eligibility；
  terminal成功后也清除，避免陈旧授权跨后续transition复用。
- `RestartAuthorizationV1`验证pre-head、非空turn IDs、当前state instance/session/head
  派生的transaction ID，并与`plan_restart_reconciliation`重新生成的完整events及
  authorization逐字段相等。
- authorization在event应用前只赋予列出的Running/no-pending Turns；应用后
  `AbortedByRestart`出现顺序必须与authorized turn list完全相等。普通transaction前缀
  冒充、附加/遗漏abort或伪造authorization均被拒绝。
- full replay逐batch执行同一校验。checkpoint load不是直接信任serialized hidden
  eligibility：它从log offset 0调用`apply_session_batch`重放到anchor，再比较events、
  projection digest和完整projection，因此不能通过篡改cache授权terminal。
- wire映射只在通过reducer的stored event上生成sequence；eligibility和restart
  authorization不泄漏wire，也不能由wire字段重新注入。
- 负向测试覆盖direct Succeeded/Failed/BudgetExceeded/AbortedByRestart、
  Approve→Failed、Deny→Succeeded及重算checksum的forged restart transaction。

除上述新M1的BudgetExceeded缺口外，R1 terminal/restart绕过问题已闭合。

## R1 M2闭合核验：canonical subject绑定

- `ApprovalSubjectInputsV1`保存provider origin与完整egress field names；构造时按bytes
  排序去重并限制非空、最多64项，stored replay还要求输入本身已是canonical顺序。
- digest实现唯一收口在`protocol::approval_subject_digest_v1`；App Service与Session
  reducer都调用该函数，不再存在test-only或重复算法漂移。
- digest canonical tuple绑定schema、provider、排序去重fields、owner Turn ID及
  authoritative prompt SHA-256。
- reducer从owner Turn的stored `canonical_prompt`重算digest，必须与请求中的
  `ApprovalSubjectDigest`逐字节相同，之后才创建pending Approval。
- `event_to_wire`隐藏subject inputs，保留现有A4 wire schema；wire只暴露已验证的digest。
- 负向测试覆盖错误requested digest、错误provider、额外field、由错误prompt计算的
  digest，以及request与resolve同时使用相同伪造digest。所有fixture均重算外层batch
  checksum并确认`ProjectionRejected`。

R1 M2已闭合。

## 新回归检查

除BudgetExceeded外，未发现新的重大回归：

- stored prompt仍为authoritative且不泄漏wire。
- stream identity、cursor sequence和command-only batch head语义未被hidden eligibility
  改动破坏。
- restart batch的partial/sync后恢复及checkpoint reopen测试证明transaction全有或全无。
- descriptor-relativeSession枚举、global owner重复检测、poison/repair及资源上限保持
  fail closed。
- B3b仍未接生产App Service，未越过B4集成边界。

## 机械门禁

在`/home/mii/code/draft/alda-agent`实际运行：

```text
cargo fmt --check                                         PASS
cargo clippy --all-targets --all-features -- -D warnings PASS
cargo test --all-targets --all-features                  PASS
  lib unit tests                                         83 passed
  main unit tests                                         6 passed
  HTTP integration                                        1 passed
  WS integration                                          2 passed
node --check web/app.js                                  PASS
node --check web/client-state.js                         PASS
node --check web/sw.js                                   PASS
node --test web/client-state.test.js                     PASS (5 passed)
git diff --check                                         PASS
```

机械门禁通过不能替代缺失的合法BudgetExceeded持久化路径。

## RELEASE判定

- R1 M1 direct terminal/restart bypass：PASS。
- R1 M2 subject digest binding：PASS。
- 新重大问题：BudgetExceeded正式terminal不可表示。
- B3b：**不可 RELEASE**。
- 本报告是max round 2最终复核；剩余重大问题需由主流程裁决。
