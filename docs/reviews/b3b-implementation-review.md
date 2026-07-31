---
verdict: revise
scope: final
artifact: /home/mii/code/draft/alda-agent
reviewer: B3b fresh-context independent implementation reviewer
date: 2026-07-31
---

# B3b 独立实现审查 R1

## 结论

B3b 当前不可 RELEASE，verdict 为 `revise`。

实现已经完成获批设计的大部分高风险机制：primitive-only stored codec；authoritative
prompt且不泄漏wire；Session ID即stream ID；空batch唯一空区间；exact raw reply；
poison/recover/repair；checkpoint跨同event head的batch anchor；restart planner；
descriptor-relative枚举、目录hash与全局owner重复检测；完整初始化/append/checkpoint/
repair failpoint矩阵。B3b也没有接入生产App Service，范围控制正确。全仓机械门禁全部
通过。

但Session reducer仍有两项授权/生命周期重大缺口：

1. 任意`Running` Turn可直接被stored `TurnCompleted(Succeeded|Failed)`终止，不要求A2的
   Approval resolved路径；
2. `ApprovalRequested`只校验digest格式，不从authoritative prompt与approval subject输入
   重算digest，因此checksum重算后的任意合法hex digest可成为持久审批对象。

这两项都会让外层checksum正确、字段格式合法但不代表A2真实执行路径的事实通过trusted
replay。

## 重大问题

### M1 — reducer允许无Approval事实的直接Succeeded/Failed terminal

- 严重度：重大。
- 位置：
  - `/home/mii/code/draft/alda-agent/src/state_store/session.rs:683-705`
  - `/home/mii/code/draft/alda-agent/src/state_store/session.rs:715-729`
  - `/home/mii/code/draft/alda-agent/src/state_store/session.rs:839-869`
  - `/home/mii/code/draft/alda-agent/src/state_store/session.rs:3570-3646`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1286-1288`
- 实际证据：
  - `TurnStarted`把Turn置为`Running`。
  - `TurnCompleted`对所有非Cancelled terminal使用同一规则：
    `current == Running && no pending`。因此紧随`TurnStarted`写入
    `TurnCompleted(Succeeded)`或`TurnCompleted(Failed)`会被接受。
  - reducer不记录“该Turn刚刚由哪个Question/Approval结果恢复为Running”，也不检查
    `Succeeded`前存在`ApprovalResolved(Approve)`、`Failed`前存在
    `ApprovalResolved(Deny)`。
  - `BudgetExceeded`和`AbortedByRestart`与普通成功/失败也共用这条分支，无法表达各自
    不同的合法来源。
  - tamper测试只覆盖“仍有pending时terminal失败”，没有覆盖“无任何
    Question/Approval事实直接成功/失败”。
- 影响：
  - 一个重算batch checksum的stored log可让Turn在没有人类Approval决定的情况下显示
    Succeeded，或伪造deny产生的Failed终态，破坏A2审计链。
  - full replay与checkpoint会稳定接受并传播该状态；这不是投影显示误差，而是权威
    terminal fact验证缺失。
- 最小修复方向：
  - 在projection中维护不泄漏wire的明确transition evidence，例如最后resolved
    Approval decision/sequence或runtime completion eligibility。
  - 分别冻结并验证：
    - `Succeeded`只接在同Turn的`ApprovalResolved(Approve)`之后；
    - `Failed`只接在`ApprovalResolved(Deny)`或另一个明确版本化failure fact之后；
    - `BudgetExceeded`只来自对应budget fact/eligible runtime状态；
    - `AbortedByRestart`只由restart planner允许的pre-state产生。
  - 增加checksum重算的direct succeeded/failed、错误decision→terminal及普通在线事件
    冒充restart abort的负向测试。

### M2 — Approval subject digest没有与stored prompt/payload重新绑定

- 严重度：重大。
- 位置：
  - `/home/mii/code/draft/alda-agent/src/state_store/session.rs:392-414`
  - `/home/mii/code/draft/alda-agent/src/state_store/session.rs:438-488`
  - `/home/mii/code/draft/alda-agent/src/state_store/session.rs:805-837`
  - `/home/mii/code/draft/alda-agent/src/state_store/session.rs:2738-2803`
  - `/home/mii/code/draft/alda-agent/src/app_service.rs:1751-1774`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1271-1276`
- 实际证据：
  - codec的`StoredApprovalSubjectDigestV1::into_live`只要求algorithm为`sha256`、
    schema为1且value为64位小写hex。
  - reducer处理`ApprovalRequested`时只往返验证payload字段与digest格式，随后原样保存
    supplied digest；它没有读取该Turn的authoritative `canonical_prompt`并重算。
  - 当前A2 digest实际绑定provider origin、排序去重后的egress field names、owner Turn
    ID和prompt digest。Session stored event只保存展示用`ApprovalPayload`与最终digest，
    没有冻结完整canonical subject inputs；`scope: String`不能无歧义恢复原field list。
  - `stored_prompt_is_authoritative_but_never_leaks_to_wire`测试在重启后调用App Service
    test helper自行生成digest，再把结果传入event；它只证明调用者能算出相同值，没有证明
    reducer会拒绝错误但格式合法的requested digest。
  - tamper测试只修改`ApprovalResolved` digest，使其与已请求值不匹配；没有伪造
    `ApprovalRequested`本身的digest。
- 影响：
  - 攻击/损坏fixture可同时写入任意ApprovalRequested digest及匹配的ApprovalResolved，
    重算外层checksum后完整通过replay；持久审计无法证明用户批准的是原prompt对应的
    ModelEgress subject。
  - authoritative prompt虽已落盘，却没有实际参与trusted capability验证，未闭合R1 M2
    的安全目的。
- 最小修复方向：
  - stored Approval request必须保存完整、版本化canonical subject inputs，或定义能从
    stored prompt+payload唯一推导的subject schema；共享非test digest函数在reducer内
    重算并逐字节比较。
  - 不允许仅凭调用者提供的合法hex建立pending Approval；增加wrong requested digest、
    wrong prompt、wrong provider/field set及request+resolve同时伪造的checksum重算测试。

## 已验证通过

- `StoredSessionEventV1`全部使用primitive/stored DTO，`into_live`逐项验证ID、text、
  choice唯一性、effect、digest格式、decision和terminal enum，不直接Deserialize live
  event/capability。
- stored `TurnStarted`保存1..=8000-byte canonical prompt；projection保留prompt，
  `event_to_wire`只输出Turn ID，没有新增wire字段。
- reducer强制首事件唯一且Session身份匹配、ID唯一、pending owner/session一致、
  choice属于request、resolve digest匹配request、cancel owner-abort顺序及terminal后
  禁止继续。
- `stream_id = SessionId`、epoch固定1；cursor检查kind/Session/epoch/future并从权威
  event head分页，command-only batch不产生wire event。
- command-only batch使用
  `event_count=0, first=head+1, last=head`，只推进checksum/offset/transaction；
  checkpoint以offset+sequence+batch checksum复核prefix，保存完整command index和
  transaction IDs，再replay tail。
- stable reply复用B3a canonical JSON/raw length/base64 codec；sync后response前recover
  返回原始bytes且不重复events。
- Session writer lease与Project registry分离；poison/recover/repair全程保持原Session
  identity和lease，完整newline坏行fail closed，无newline尾部需compare-and-truncate。
- restart planner对Running、CancelRequested、合法/非法WaitingForInput和terminal状态
  生成稳定`restart-v1:<instance>:<session>:<pre-head>`计划；单batch故障测试覆盖partial
  与sync后恢复。
- `list_sessions`descriptor-relative枚举，拒绝非canonicalentry，验证private directory/
  regular rollout，复算directory hash，full replay后检测跨Session重复Turn/Question/
  Approval ID。
- typed 128-bit CSPRNG ID helper带prefix、碰撞检查和32次上限；生产catalog接线仍正确
  留给B4。
- Session目录/rollout、append、checkpoint、repair的获批逻辑failpoint矩阵均有测试。
- 搜索生产代码未发现`StateStore` Session writer、`list_sessions`、restart planner或
  stored rollout接入`AppService`；只有`#[cfg(test)]`的online reducer/digest对照helper，
  未提前实现B4。

## 非阻断残余

- ID随机分配耗尽当前返回`IdempotencyConflict`，而获批设计写的是typed internal error；
  语义不精确，但调用面仍为B4范围，且不会误分配重复ID。B4接线前应换成专用错误。
- `list_sessions`最多100,000个Session，但catalog总内存还受每个合法rollout历史规模影响；
  当前逐行和checkpoint读取有界且startup本就要求全量owner index，因此不单独阻断B3b。
- checkpoint是派生缓存，超过上限时拒绝写入并在load时回退full replay；长期command
  index增长会降低checkpoint可用性，但不丢权威事实。

## 机械门禁

在 `/home/mii/code/draft/alda-agent` 实际运行：

```text
cargo fmt --check                                         PASS
cargo clippy --all-targets --all-features -- -D warnings PASS
cargo test --all-targets --all-features                  PASS
  lib unit tests                                         81 passed
  main unit tests                                         6 passed
  HTTP integration                                        1 passed
  WS integration                                          2 passed
node --test web/client-state.test.js                     PASS (5 passed)
node --check web/app.js                                  PASS
node --check web/client-state.js                         PASS
node --check web/sw.js                                   PASS
git diff --check                                         PASS
```

机械门禁通过不覆盖M1的direct terminal事实或M2的forged ApprovalRequested digest，因此
verdict仍为`revise`。

## RELEASE判定

- Slice A / B1 / B2 / B3a回归：PASS。
- B3b存储、恢复与durability机制：主体正确。
- B3b trusted Session reducer：M1/M2未闭合。
- B3b：**不可 RELEASE**。
- B4与正式MVP：本报告不作完成声明。
