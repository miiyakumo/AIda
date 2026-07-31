---
decision: amended
scope: design-adjudication
artifact: /home/mii/code/draft/docs/plans/mvp-deliberative-execution.md
inputs:
  - /home/mii/code/draft/docs/reviews/b4c-design-review.md
  - /home/mii/code/draft/docs/reviews/b4c-design-review-r2.md
date: 2026-08-01
next_gate: B4c C0 implementation validation
---

# B4c 两轮设计审查后的主 Agent 裁决

## 裁决

R2维持`REVISE`，不启动第三轮设计审查，也不登记虚假的`APPROVED`。主 Agent逐层读取ordinary
open helper后驳回R2 M1的事实前提：Control、Project、Session早已在scan前对同一次open的
handle执行`sync_all`，单实例锁/lease又排除了sync与scan间的并发writer，因此跨进程确认门并未
缺失。主 Agent接受R2的restart容量冲突，并基于同一事实源闭合原则，把现有单向Committed audit
扩展为双向transaction closure，同时冻结Prepared后错误归置和v2 transport选择。修订后的§18
是C0实现契约；下一门仍是实现与跨进程故障验证，不是Slice B release。

## 源码事实与审查意见处置

- `control_store::open_control_writer`在scan前调用`open_or_create_control_log`，后者无条件
  `file.sync_all`；Project `open_writer_with_lease`与Session
  `open_session_writer_with_lease`同样先经`open_or_create_events`/
  `open_or_create_rollout`完成file sync再scan，checkpoint分支也在其后。Project/Session已有
  `EventsFileSync`/`RolloutFileSync` ordinary-open failpoint。故R2 M1引用Clean match分支却遗漏
  helper前置sync，反例不成立；C0只需补Control对称failpoint/跨进程向量，不增加重复post-scan
  sync。
- `ReadyDurableRuntime::open`会在control open后立即枚举pending redo；既有pre-scan helper门是
  Ready构造的前置条件。poisoned recovery使用另一handle，仍必须保留本轮新增的rescan后
  `sync_all`，且该第二次确认失败必须Fatal。
- 当前`audit_committed_transactions`从control Prepared逐笔open aggregate并probe，只证明
  control→aggregate，不枚举aggregate transaction index；同一aggregate有多笔Prepared时还会
  重复open/replay。裁决要求按aggregate分组、单次open/replay，并做双向集合相等与anchor字段
  相等验证。
- 当前control以单一`MAX_PENDING = 10_000`约束`prepared.len()`，Committed不会移除Prepared。
  因此接受R2 M2，改为外部10,000、internal restart 10,000、物理20,000三项分别计数的终身
  预算；checkpoint/full replay使用相同分类和限制。
- R1的restart identity、Committed anchor方向、Fatal/shutdown控制面以及payload v2决定继续
  保留；接受R2 I1的transport歧义，唯一选择为HTTP `/v2/*`、WS `/v2/ws`与
  `alda-agent.v2`，production不提供v1 transport/payload。

## 冻结不变式

1. 任一Clean log boundary在同一open handle成功`sync_all`前都不是可用于redo、probe、read
   view或anchor的权威事实；ordinary open可在单实例锁/lease保护下先sync再scan，poisoned
   rescan必须在scan后重新sync，checkpoint不能绕过。
2. prepare写入后rescan无法裁定、已见Prepared后的confirmation失败，或writer/lease/
   capability丢失，必为typed Fatal且无panic；只有明确证明没有Prepared且writer仍Ready才可
   拒绝。Recovering只表示仍完整持有能力并可重试同一Prepared的已分类瞬态失败。
3. 最终publish前，control Committed plans与aggregate中所有B4
   `global-<32hex>:{project|session}` transaction在identity、digest、sequence、checksum上双向
   一一相等；仅精确trusted验证的Session legacy `restart-v1:*`例外，其它未知transaction拒绝。
4. startup按aggregate线性处理，每个writer只open/replay一次；20,000 Prepared边界不能产生
   按transaction重复replay的二次复杂度。
5. 外部与internal restart分别最多10,000，total最多20,000；入口不能跨预算消费。一个restart
   obligation源于一个外部mutation，相同intent幂等，terminal reconciliation不产生新
   obligation。
6. production payload、HTTP path、WS path/subprotocol、CLI与PWA统一v2；不存在production
   v1 alias、降级或双栈。

## C0验收向量

- Control/Project/Session分别执行“完整line写入但sync失败→drop poisoned typestate→新进程
  ordinary open confirmation再失败/终止”的二阶crash，覆盖有/无checkpoint；不得产生下游
  append、Committed或read view，也不得panic。
- Prepared后分别注入rescan失败、confirmation失败及writer/capability丢失，断言只产生Fatal
  shutdown；保有完整能力的显式瞬态错误才可重试同一Prepared并一次发布。
- 对Project/Session transaction index分别注入control缺aggregate、aggregate多global、错误
  suffix/owner、digest/sequence/checksum错配、精确legacy和伪legacy；全部在bind前得到预期的
  fail-closed/唯一白名单结果。
- 在单aggregate承载大量Prepared以及多Session待restart时验证每个writer只open/replay一次，
  并覆盖external/internal/total预算的9,999、10,000和物理20,000边界、checkpoint/full replay、
  中途crash与重复启动不增internal计数。
- production HTTP/WS/CLI/PWA真实往返只使用v2；v1 path不路由、v1 WS subprotocol不upgrade、
  v2 transport上的v1 envelope typed拒绝。
